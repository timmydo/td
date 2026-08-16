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
//! Operands are read a BLOCK at a time, as `grep` reads its own. `$` (last line)
//! must be known before the last line is processed, which a streaming reader can
//! only answer by having LOOKED -- so `Stream` holds exactly one record ahead,
//! and that record is not counted as delivered until it is handed out, which is
//! what keeps the give-back to descriptor 0 honest. `-s` and `-i` open each
//! operand themselves, to decide whether it may be rewritten, and stream it
//! through that same reader, and so does a NAMED `R` source; `r` COPIES its
//! source to the sink a block at a time rather than holding it. `R /dev/stdin`
//! takes its line from the run's ONE reader over descriptor 0 -- the same one an
//! operand naming standard input reads -- because two readers over that
//! descriptor cannot each be right about the give-back's relative seek, and
//! because GNU shares one stream between them and so takes alternate lines.
//! SCRIPTS (`-f`) are what is left reading whole; see spec/README.
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
    errmsg, number, posixly_correct, print_line, records, Input, Out, Records, VERSION,
};

const USAGE: &str =
    "usage: sed [-nrEszu] [-i[SUFFIX]] [-l N] [-e SCRIPT] [-f FILE] [SCRIPT] [FILE]...";

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
    wfile: Option<FileTarget>,
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
    ReadLine(FileTarget),
    Subst(Subst),
    Transliterate(Box<[u8; 256]>),
    Write(FileTarget),
    WriteFirstLine(FileTarget),
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
    /// A `v` was compiled, which is GNU's `posixicity = POSIXLY_EXTENDED`
    /// (compile.c:1079). Half of what the RUN needs: the other half is the
    /// option loop's own final value, and the two are combined there rather
    /// than here because `--posix` given AFTER the last `-e` still wins --
    /// `sed -e v -e '$!d;N' --posix` drops the pattern space, measured.
    v_promoted: bool,
    regexes: Vec<Regex>,
    labels: BTreeMap<Vec<u8>, usize>,
    /// The `w` targets, ALREADY OPEN: GNU opens one while it parses the command
    /// that names it, so compiling a script is what creates and truncates them.
    /// Keyed by filename because GNU keeps one output per name, not per command.
    wfiles: BTreeMap<FileTarget, WFile>,
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
    /// Where the command being parsed STARTED, which is what picks its part and
    /// so its compile flags. Not `pos`, which has already run to the command's
    /// end by the time most of those flags are read.
    cmd_start: usize,
    /// The compile inputs that are NOT per-part; see `Invocation`.
    inv: Invocation,
    /// What the lookup answers when there are no parts to look in, which is the
    /// unit tests and nothing else: `run` always builds at least one, and
    /// `cmd_start` points at a byte of the script, so every real command lands
    /// inside a part.
    fallback: Mode,
    /// A `v` has been compiled, so everything after it is POSIXLY_EXTENDED. Not
    /// per-part: GNU keeps ONE `posixicity` across the whole compile, so a `v` in
    /// an earlier `-e` reaches every later one. Monotonic here because only
    /// `--posix` can undo it, and that is answered from the part's own `Mode`.
    v_promoted: bool,
    /// Set when a diagnostic points somewhere other than where the parse stopped:
    /// GNU SAVES the location of a `{` and reports an unmatched one there. `None`
    /// means "wherever the parser is", which is every other diagnostic.
    saved: Option<usize>,
    /// Every `-e`/`-f` fragment: where it ENDS in `src`, and what to call it in a
    /// diagnostic. The end doubles as the boundary the parse must respect, since
    /// GNU compiles each part alone: a part's closing newline is that part's end
    /// of input rather than a byte of the script, so `sed -e a -e p` is an error
    /// where the one-argument `sed 'a<newline>p'` is not. `a`/`i`/`c` text is the
    /// one thing GNU carries ACROSS that boundary, and only when a backslash asks
    /// it to (`sed -e 'a\' -e text`). See `parse_text` and `at_part_end`.
    parts: Vec<Part>,
    regexes: Vec<Regex>,
    wfiles: BTreeMap<FileTarget, WFile>,
}

impl ScriptParser<'_> {
    /// The flags the part under `pos` was SCANNED with, plus any promotion the
    /// SCRIPT has made since (see the `v` fold at the end). GNU compiles each part
    /// inside the option loop, so this is a function of position rather than one
    /// value for the run: `sed -e 1~2p --posix` compiles that part before the
    /// flag exists. Parts are in increasing `end` order, so the first whose end
    /// is past `pos` is the one holding it -- and the separator newline AT an end
    /// belongs to the part after, which is where the next command starts.
    /// A loop rather than the iterator search that reads better: this module is
    /// embedded verbatim by `recipes/src/recipes/td-txt.rs`, and the ladder's
    /// host-tool guard tokenises what it writes, so that method's NAME alone
    /// reds the gate.
    fn mode(&self) -> Mode {
        let mut mode = self.fallback;
        for p in &self.parts {
            if self.cmd_start < p.end {
                mode = p.mode;
                break;
            }
        }
        // A `v` already COMPILED promotes what follows it, across later parts:
        // GNU holds one `posixicity` for the whole compile, so this is the same
        // question the RUN asks and the same function answers it.
        mode.extended = mode.extended_with_v(self.v_promoted);
        mode
    }

    fn posix(&self) -> bool {
        self.mode().posix
    }

    /// `--posix` as the part ENDING at `pos` had it, which is not the same
    /// question `posix()` answers. GNU refuses a dangling `a`/`i`/`c` text at the
    /// end of EVERY part under that part's own posixicity (compile.c:1369), and
    /// text is the one construct whose parse crosses a boundary -- so a
    /// continuation that starts in an extended part and dangles again inside a
    /// `--posix` one is refused there, by the flag the FIRST part never saw.
    ///
    /// Reads the part's `posix` DIRECTLY rather than through `mode()`, and may:
    /// GNU's check is `posixicity == POSIXLY_BASIC` (compile.c:1369), `v`
    /// assigns EXTENDED, and `v` is refused outright under the flag -- so no
    /// compile can have a `v` move posixicity into or out of BASIC. An
    /// `extended_at_end` written by copying this WOULD need the promotion.
    fn posix_at_end(&self) -> bool {
        for p in &self.parts {
            if self.pos <= p.end {
                return p.mode.posix;
            }
        }
        self.fallback.posix
    }

    fn extended(&self) -> bool {
        self.mode().extended
    }

    fn ere(&self) -> bool {
        self.mode().ere
    }

    fn sandbox(&self) -> bool {
        self.mode().sandbox
    }

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

    /// Is the parser at a newline that ENDS an `-e`/`-f` part? Every part but
    /// the last is followed by one, and `Part::end` is where it sits. The last
    /// part's end is the source's length, where there is no byte to peek at, so
    /// it cannot answer yes here.
    fn at_part_end(&self) -> bool {
        self.peek() == Some(b'\n') && self.parts.iter().any(|p| p.end == self.pos)
    }

    fn skip_blank(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t')) {
            self.pos += 1;
        }
    }

    /// Where this parser's next diagnostic points.
    fn spot(&self) -> Spot {
        match self.saved {
            Some(at) => Spot::Saved(at),
            None => Spot::Read(self.pos),
        }
    }

    /// Put the byte just read BACK, which is what GNU's `match_slash` does
    /// before it reports an unterminated half — `savchar (ch); /* for proper
    /// line number in error report */`. The position is where the diagnostic's
    /// LINE, `char N` and `-e` part all come from, and a newline is exactly
    /// where one part ends and the next begins, so keeping it reports the error
    /// one line, or one `-e` argument, past the command that failed.
    fn savchar(&mut self) {
        self.pos = self.pos.saturating_sub(1);
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
            Some(_) => {
                // GNU's `read_end_of_cmd` READS the offending byte through
                // `in_nonblank` and only then complains, so `pZ` is `char 2`
                // rather than 1. It needs no part-boundary guard: a newline is
                // accepted above, so nothing that reaches here is the joiner.
                let _ = self.bump();
                Err("extra characters after command".to_string())
            }
        }
    }

    fn rest_of_line(&mut self) -> Vec<u8> {
        let start = self.pos;
        while !matches!(self.peek(), None | Some(b'\n')) {
            self.pos += 1;
        }
        let text = self.src.get(start..self.pos).unwrap_or_default().to_vec();
        // A part's closing newline is that part's end of input, not this line's
        // terminator: eating it leaves the parse standing in the NEXT part, so a
        // failure the SAME command raises afterwards is blamed on that one --
        // `sed -e 's/\(a/b/wg' -e p` is expression #1 in GNU. Only the `s///w`
        // flag can show it, the other callers having nothing left to fail at.
        if !self.at_part_end() {
            self.eat(b'\n');
        }
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
            ere: self.ere(),
            icase,
            strict_repeats: true,
            posix: self.posix(),
            unmatched_rparen_ordinary: !self.extended(),
            // GNU's one POSIXLY_CORRECT rule that is not a posixicity read.
            confusing_bracket_ok: self.inv.posixly,
            // sed has only glibc, which never satisfies a mid-branch `$`.
            glibc_engine: true,
            // sed lexes one regex at a time, and has no `-x`/`-w` to wrap it.
            lex_continues: false,
            reg_newline: multiline.then_some(separator_for(self.inv.null_data)),
        };
        let re = Regex::compile(&normalize_regex(raw, !self.extended())?, opts)
            .map_err(|e| e.msg)?;
        self.regexes.push(re);
        Ok(Some(self.regexes.len() - 1))
    }

    /// A `/re/` or `\cREc` address, consuming the blanks before an `I`/`M`
    /// modifier and the modifiers themselves -- except under `--posix`, which
    /// withdraws the modifiers but still crosses the blanks; see the body.
    fn parse_regex_addr(&mut self, delim: u8) -> Result<AddrKind, String> {
        let raw = self.read_delimited(delim, "unterminated address regex")?;
        let mut icase = false;
        let mut multiline = false;
        // GNU reads each modifier through `in_nonblank` and asks about
        // posixicity only afterwards, so a blank is crossed ahead of one and
        // BETWEEN two (`/a/ I M p` is one address with both) -- and under
        // `--posix`, where the letter is then met as a command instead.
        loop {
            self.skip_blank();
            if self.posix() {
                break;
            }
            match self.peek() {
                Some(b'I') => icase = true,
                Some(b'M') => multiline = true,
                _ => break,
            }
            self.pos += 1;
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
                self.savchar();
                return Err(unterminated.to_string());
            }
            if b == delim && !in_bracket {
                return Ok(out);
            }
            if b == b'\\' && !in_bracket {
                // A part's closing newline is that part's end of input, so this
                // backslash has nothing to escape and the half is unterminated
                // — GNU compiled the part alone and ran out here. INSIDE one
                // script the pair is an escaped newline, which is how a pattern
                // carries one. `a`/`i`/`c` text is the deliberate exception and
                // does cross the boundary; see `parse_text`.
                if self.at_part_end() {
                    return Err(unterminated.to_string());
                }
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
            self.savchar();
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
        self.posix() && matches!(c, b'v' | b'Q' | b'T' | b'z' | b'F' | b'W' | b'R' | b'e')
    }

    /// The commands `--posix` gives at most ONE address, GNU otherwise taking a
    /// range. Four are POSIX's own; `l` is NOT -- POSIX.1 defines it `[2addr]l`,
    /// so that one is GNU's posix-mode behaviour, taken from the oracle rather
    /// than from the standard. `q`/`Q` are restricted in both modes and check
    /// themselves; `c` is not in the list, a range being the whole point of it.
    fn posix_limits_addresses(&self, c: u8) -> bool {
        self.posix() && matches!(c, b'=' | b'a' | b'i' | b'l' | b'r')
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
                // Same rule as `s` and `y`: a part's closing newline is that
                // part's end of input, so the refusal belongs to it and not to
                // the part whose first byte would otherwise be the pattern.
                if self.at_part_end() {
                    return Err("unterminated address regex".to_string());
                }
                // GNU's wording for BOTH ways of running out here, and the two
                // have to agree: a `\` with nothing after it is the same failure
                // whether the script ended or its part did.
                let delim = self.bump().ok_or_else(|| "unterminated address regex".to_string())?;
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
            Some(b'+' | b'~') if !self.posix() => {
                self.pos += 1;
                self.skip_blank();
                match self.parse_number().unwrap_or(0) {
                    0 => Ok(Some(AddrKind::Always)),
                    _ => Err("invalid usage of +N or ~N as first address".to_string()),
                }
            }
            Some(b) if b.is_ascii_digit() => {
                let n = self.parse_number().unwrap_or(0);
                if !self.posix() {
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
                if !self.posix() && self.eat(b'+') {
                    self.skip_blank();
                    addr.a2 = Some(Addr2::Plus(self.parse_number().unwrap_or(0)));
                } else if !self.posix() && self.eat(b'~') {
                    self.skip_blank();
                    addr.a2 = Some(Addr2::Multiple(self.parse_number().unwrap_or(0)));
                } else {
                    let Some(k) = self.parse_addr_kind()? else {
                        // GNU READS the byte that turned out not to be an address
                        // before complaining (`in_nonblank`), whatever it is, so
                        // `1,p` is `char 3` and a real newline has moved its line
                        // count on. A part's closing newline is that part's end of
                        // input and is not read at all — the same rule the
                        // delimiters and `parse_filename` follow.
                        if !self.at_part_end() {
                            let _ = self.bump();
                        }
                        return Err("unexpected `,'".to_string());
                    };
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
            && (self.posix() || !(ends_in_regex || prepends))
        {
            // GNU has READ that command character before it judges the address,
            // so the refusal is reported past it: `0p` is `char 2`, `0 p` is
            // `char 3` with the blank crossed as well, and in a `-f` script a
            // newline there puts it on the next line. Not at a part's end, where
            // GNU's input has run out and it reads nothing.
            if !self.at_part_end() {
                let _ = self.bump();
            }
            return Err("invalid usage of line address 0".to_string());
        }
        // One `!` and no more: GNU refuses a second rather than toggling back,
        // having READ it -- `1!!p` is `char 3`.
        if self.eat(b'!') {
            addr.negate = true;
            self.skip_blank();
            if self.eat(b'!') {
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
            if self.posix_at_end() && (self.at_part_end() || self.peek().is_none()) {
                return Err("incomplete command".to_string());
            }
            if !self.eat(b'\n') {
                // GNU's `read_text` adds whatever follows the command's own
                // backslash to the buffer as a LEAD-IN, before escape
                // processing starts, and never asks about `--posix` first. A
                // SECOND backslash is therefore a one-character text the next
                // newline ENDS, where the loop below would read the pair as a
                // continuation and swallow the line after it.
                let Some(lead) = self.bump() else {
                    return Ok(None);
                };
                out.push(lead);
            }
        } else if self.posix() || self.peek().is_none() || self.at_part_end() {
            // The one-line `a text` form is GNU's; `--posix` leaves only `a\`.
            // GNU has READ the byte that was not a backslash, so `--posix 'a t'`
            // is `char 3` -- but a part's closing newline is that part's end of
            // input, so there is nothing to read there and the count stops.
            if !self.at_part_end() {
                let _ = self.bump();
            }
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
                    if self.posix_at_end() {
                        return Err("incomplete command".to_string());
                    }
                    self.pos += 1;
                    out.push(b'\n');
                }
                Some(b'\\') => match self.bump() {
                    None => {
                        if self.posix_at_end() {
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
        // No terminator here: the writer supplies one, and WHICH one is not the
        // same for all three. `i`/`c` end their text with the record separator,
        // so under -z it is a NUL; `a` ends its with a literal newline whatever
        // the separator is. GNU's asymmetry, recorded in spec/README, and the
        // reason parsing does not append either.
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
        // GNU's `read_label` NUL-terminates its buffer and `xstrdup`s it, so the
        // label ENDS at a NUL the script put in it while the bytes after it are
        // still CONSUMED: `b\0x` branches to the EMPTY label, which is the end of
        // the script. Keeping the byte instead made `:\0` and `b\0` agree on a
        // label neither GNU spelling has, and the script jump to itself for ever.
        crate::util::cstr(self.src.get(start..self.pos).unwrap_or_default()).to_vec()
    }

    /// r/R/w/W and the `s///w` flag all take a file name to end of line. An
    /// EMPTY one is a script error, not a no-op: silently passing the input
    /// through is exactly the fail-open this crate refuses everywhere else.
    fn parse_filename(&mut self) -> Result<Vec<u8>, String> {
        self.skip_blank();
        // No part-end guard here: at a part's closing newline `rest_of_line`
        // reads nothing and leaves that byte alone, so the empty-name refusal
        // below is already the right message at the right position. One that
        // could never fire would read as a handled case.
        // `read_filename` is a C string in GNU too, so a NUL ENDS the name --
        // `w \0f` is the EMPTY one this refuses rather than a file nothing can
        // open. Same rule as the label above and as the printer's `%s`.
        let name = crate::util::cstr(&self.rest_of_line()).to_vec();
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
        if self.sandbox() {
            return Err(Fatal {
                msg: b"e/r/w commands disabled in sandbox mode".to_vec(),
                status: 1,
                locus: None,
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
    /// The three device names are not files and GNU creates nothing for them; they
    /// are registered here anyway so the write path never has to open anything.
    fn open_wfile(&mut self, target: &FileTarget) -> Result<(), Fatal> {
        if self.wfiles.contains_key(target) {
            return Ok(());
        }
        let path = &target.path;
        let dest = match target.special {
            Some(Special::Out) => WDest::Stdout(None),
            Some(Special::Err) => WDest::Stderr,
            // Opened as a stream nobody may write to, and not as a PATH: creating
            // `/dev/stdin` truncates whatever the operator's standard input is.
            Some(Special::In) => WDest::Stdin,
            None => WDest::File(crate::util::StdioBuf::over(
                std::fs::File::create(crate::util::path_from_bytes(path))
                    .map_err(|e| Fatal::runtime_msg(cant_open(path, &e)))?,
            )),
        };
        let opened = self.wfiles.len();
        self.wfiles.insert(target.clone(), WFile { dest, owed: false, opened });
        Ok(())
    }

    /// Returns `Fatal` because the `w` FLAG opens its target here, mid-command:
    /// GNU reaches that flag before it compiles the pattern, so an unopenable
    /// target beats a bad regex or a bad backreference in the same `s`.
    fn parse_subst(&mut self) -> Result<Subst, Fatal> {
        // A backslash CAN delimit (`s\a\b\` is `s/a/b/`). A newline cannot, and
        // WHICH newline it is decides where the refusal is: one that ENDS a part
        // is that part's end of input -- GNU compiles each part alone, so there
        // is no delimiter to take -- while one inside a script is a byte GNU
        // really consumes as the delimiter, leaving the scan to fail on the next
        // line. So this returns without consuming, and the scan handles the rest.
        if self.at_part_end() {
            return Err("unterminated `s' command".to_string().into());
        }
        let delim = self.bump().ok_or_else(|| "unterminated `s' command".to_string())?;
        let pattern = self.read_delimited(delim, "unterminated `s' command")?;
        let raw_repl = self.read_replacement(delim, "unterminated `s' command")?;
        // GNU decodes the replacement BEFORE it reads the flags
        // (`setup_replacement` precedes `mark_subst_opts` in compile.c), so a bad
        // escape there outranks both an unknown flag and a bad PATTERN, and is
        // reported before the terminator the flag loop goes on to consume:
        // `s/a/\c\q/x` is `char 9` and `s/\(a/\c\q/` names the replacement.
        let replacement = compile_replacement(&raw_repl, self.posix())?;
        let mut global = false;
        let mut print = false;
        let mut icase = false;
        let mut multiline = false;
        let mut occurrence: u64 = 0;
        let mut wfile = None;
        loop {
            // A blank SEPARATES flags rather than ending them (`s/a/b/ g p` is
            // both), and GNU reads through `in_nonblank`, so a crossed blank is
            // in the character count.
            self.skip_blank();
            // The two number refusals are counted from DIFFERENT places, because
            // GNU judges a repeat before reading the number and a zero after it.
            // So `s/a/b/2p345` stops at the first digit of the second number
            // (`char 9`) while `s/a/b/000` reads all three (`char 9` as well,
            // for the other reason).
            if matches!(self.peek(), Some(d) if d.is_ascii_digit()) {
                if occurrence != 0 {
                    let _ = self.bump();
                    return Err("multiple number options to `s' command".to_string().into());
                }
                // `in_integer` puts back the byte that ended the number, so the
                // zero refusal names the last digit and not what follows it.
                let n = self.parse_number().unwrap_or(0);
                if n == 0 {
                    return Err("number option to `s' command may not be zero".to_string().into());
                }
                occurrence = n;
                continue;
            }
            match self.peek() {
                // `}` and `#` START the next command, so GNU savechars them; a
                // part's closing newline is that part's end of input and is not
                // read either. Neither is in the count.
                None | Some(b'}' | b'#') => break,
                Some(b'\n') if self.at_part_end() => break,
                _ => {}
            }
            // Everything else GNU has READ before it decides what it was, so the
            // count includes it -- `s/a/b/pp` is `char 8`, not 7.
            let Some(b) = self.bump() else { break };
            match b {
                // The terminator, which GNU consumes before compiling the
                // pattern: `s/\(a/b/;p` is `char 9` where the bare `s/\(a/b/`
                // is `char 8`, and in a `-f` script it is the NEXT line.
                b'\n' | b';' => break,
                // GNU's one concession to CRLF, and it belongs to this command
                // alone: the byte after a `\r` is read whatever it is, and the
                // pair ends the flags only if it is the newline. After any
                // OTHER command a `\r` is still `extra characters after
                // command`, so this does not make a CRLF SCRIPT work -- it
                // makes `s/a/b/<cr><nl>` work, and refuses `s/a/b/<cr>Z` past
                // the `Z` rather than at the `\r`.
                b'\r' => {
                    if !self.at_part_end() && self.bump() == Some(b'\n') {
                        break;
                    }
                    return Err("unknown option to `s'".to_string().into());
                }
                // GNU rejects a REPEAT of the three flags that carry a value, and
                // `i`/`I`/`m`/`M` are the ones it lets you say twice.
                b'g' => {
                    if global {
                        return Err("multiple `g' options to `s' command".to_string().into());
                    }
                    global = true;
                }
                b'p' => {
                    if print {
                        return Err("multiple `p' options to `s' command".to_string().into());
                    }
                    print = true;
                }
                b'i' | b'I' if !self.posix() => icase = true,
                b'm' | b'M' if !self.posix() => multiline = true,
                // Under `--posix` these are not flags at all, so the catch-all
                // below answers with GNU's `unknown option to `s''.
                b'e' if !self.posix() => {
                    return Err("the `e' flag is not supported".to_string().into())
                }
                b'w' => {
                    self.deny_in_sandbox()?;
                    let name = self.parse_filename()?;
                    let target = FileTarget::resolve(&name, self.extended());
                    self.open_wfile(&target)?;
                    wfile = Some(target);
                    break;
                }
                _ => return Err("unknown option to `s'".to_string().into()),
            }
        }
        let re = self.add_regex(&pattern, icase, multiline)?;
        // A `\N` naming a group the pattern does not have is a script error, not
        // an empty expansion -- except under EITHER posix level, where it is
        // accepted and expands to nothing (regexp.c:122 asks
        // `posixicity == POSIXLY_EXTENDED`). Checked only when
        // the regex is known here; `s//.../` reuses the LAST one and goes
        // unchecked, which is a DIVERGENCE rather than a shared rule: GNU still
        // refuses when the command carries its own address regex, so
        // `/\(a\)/s//\2/` is an error there and empty here. See spec/README.
        if let Some(groups) = re
            .filter(|_| self.extended())
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
                self.savchar();
                return Err(unterminated.to_string());
            }
            if b == delim {
                return Ok(out);
            }
            if b == b'\\' {
                // As in `read_delimited`: a part's closing newline is that
                // part's end of input, so there is nothing here to escape.
                if self.at_part_end() {
                    return Err(unterminated.to_string());
                }
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
        // As in `parse_subst`: a part's closing newline is its end of input, not
        // a delimiter to be taken from the part after it.
        if self.at_part_end() {
            return Err(UNTERM_Y.to_string());
        }
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
fn normalize_regex(raw: &[u8], at_posix_level: bool) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0usize;
    while let Some(b) = raw.get(i).copied() {
        i += 1;
        // EITHER posix level drops the decoding INSIDE a bracket expression, so
        // `[\x41]` holds four ordinary members there rather than an `A`
        // (compile.c:1473 asks `posixicity != POSIXLY_EXTENDED`, which is why
        // this takes the level and not `--posix`). GNU judges "inside" on the
        // pattern TEXT, not on what decoding produces: a `[` that is itself an
        // escape opens nothing, and `\x5b\x41]` decodes under either level
        // exactly as it does without one.
        if at_posix_level && b == b'[' {
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
    parts: Vec<Part>,
    fallback: Mode,
    inv: Invocation,
) -> Result<Script, Fatal> {
    let mut p = ScriptParser {
        src,
        pos: 0,
        cmd_start: 0,
        inv,
        fallback,
        v_promoted: false,
        saved: None,
        parts,
        regexes: Vec::new(),
        wfiles: BTreeMap::new(),
    };
    // Parsed by a function of its own so the parser SURVIVES its own failure:
    // `?` returns from THERE, leaving `p.pos` readable here. That position is the
    // only thing that can name where a compile error happened, and every `?` in
    // the body would otherwise discard it.
    let parsed = parse_commands(&mut p);
    // Only an exit-1 failure gets a locus: a `w` target that would not open, or a
    // pattern GNU reports bare, is not about a place in the script even though
    // the parser was standing in one when it happened.
    parsed.map_err(|f| match f.status {
        1 => Fatal { locus: locus_at(&p.parts, src, p.spot()), ..f },
        _ => f,
    })
}

/// The command loop itself. Split from `parse_script` for the reason given
/// there: its caller reads `p.pos` after it fails.
fn parse_commands(p: &mut ScriptParser) -> Result<Script, Fatal> {
    let mut cmds: Vec<Cmd> = Vec::new();
    let mut labels: BTreeMap<Vec<u8>, usize> = BTreeMap::new();
    // (command index, where the `{` is) — the position because an unmatched one
    // is reported where the BRACE is, not where the script ran out.
    let mut open_blocks: Vec<(usize, usize)> = Vec::new();
    loop {
        p.skip_separators();
        if p.peek().is_none() {
            break;
        }
        // Which part this command STARTS in is what decides its compile flags,
        // and `pos` cannot answer that: by the time an `s///` has been read the
        // parser stands at the command's end, which for the last command of a
        // part is the boundary itself -- so the flags of the part AFTER it, or
        // of a part that does not exist, would be the ones consulted.
        p.cmd_start = p.pos;
        let mut addr = p.parse_addr()?;
        // A part's closing newline is that part's end of input, so an address
        // left dangling on it is GNU's `missing command` -- the same message
        // this reports at the true end of a script, because to GNU, which
        // compiled the part alone, it IS the end. Reading the newline instead
        // made it the unknown command and blamed the part after the one that
        // ran out. A newline INSIDE a script is an ordinary byte, and there GNU
        // reads it and calls it unknown too.
        if p.at_part_end() {
            return Err("missing command".to_string().into());
        }
        let Some(c) = p.bump() else {
            return Err("missing command".to_string().into());
        };
        if p.posix_drops_command(c) {
            return Err(crate::util::byte_in("unknown command: `", c, "'").into());
        }
        if p.posix_limits_addresses(c) && addr.is_range() {
            return Err("command only uses one address".to_string().into());
        }
        let kind = match c {
            b'{' => {
                open_blocks.push((cmds.len(), p.pos.saturating_sub(1)));
                Kind::Block(0) // patched when the matching `}` is seen
            }
            b'}' => {
                let Some((open, _)) = open_blocks.pop() else {
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
                let w = match p.posix() {
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
                let n = match p.posix() {
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
                    // `r` is never aliased -- GNU reads its operand with a bare
                    // `read_filename` -- so only `R` resolves against the table.
                    _ => Kind::ReadLine(FileTarget::resolve(&name, p.extended())),
                }
            }
            b'w' | b'W' => {
                p.deny_in_sandbox()?;
                let name = p.parse_filename()?;
                let target = FileTarget::resolve(&name, p.extended());
                p.open_wfile(&target)?;
                match c {
                    b'w' => Kind::Write(target),
                    _ => Kind::WriteFirstLine(target),
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
                // Everything COMPILED after this is POSIXLY_EXTENDED, later `-e`
                // parts included: GNU's `case 'v'` assigns the one `posixicity`
                // the whole compile shares (compile.c:1079). Set after the version
                // check, since a `v` that refuses compiles nothing at all.
                p.v_promoted = true;
                // Never COMMITTED, which discards the ADDRESS with it: GNU counts a
                // slot only after the switch its `continue` leaves through
                // (compile.c:1081), so `sed --debug '//v;p'` shows a program of just
                // `p`. Observable through a regex a later `//` would reuse, and
                // through `$`, whose lookahead moves what `F` names.
                continue;
            }
            b'e' => return Err("the `e' command is not supported".to_string().into()),
            other => return Err(crate::util::byte_in("unknown command: `", other, "'").into()),
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
    // The INNERMOST unmatched brace, which is what GNU's block stack hands
    // `check_final_program`, and its own position rather than the end of the
    // script: `{` on line 2 of four is `line 2` there.
    if let Some((_, brace)) = open_blocks.last() {
        p.saved = Some(*brace);
        return Err("unmatched `{'".to_string().into());
    }
    Ok(Script {
        cmds,
        v_promoted: p.v_promoted,
        regexes: std::mem::take(&mut p.regexes),
        labels,
        wfiles: std::mem::take(&mut p.wfiles),
    })
}

/// Turn every branch's label name into a command index. Done after the whole
/// script is parsed because a branch may jump forward.
fn resolve_labels(cmds: &mut [Cmd], labels: &BTreeMap<Vec<u8>, usize>) -> Result<(), Vec<u8>> {
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
                None => return Err(crate::util::name_in("can't operate on label `", &name, "'")),
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
    /// The OPEN operand, read a BLOCK at a time rather than whole. `None` before
    /// the first open and once one is spent.
    src: Option<Records<Src>>,
    /// How a READ failure names the open operand, captured at open because the
    /// `Input` itself is inside the reader by the time one can happen.
    err_name: Vec<u8>,
    /// The run's ONE reader over descriptor 0, made at most once and outliving the
    /// operand that first needed it. Both roles read from it: an operand naming
    /// standard input, and `R /dev/stdin`. GNU shares one `FILE*` between them, so
    /// its `R` takes the line AFTER the one the cycle read -- `sed -n -e 'R
    /// /dev/stdin' -e 's/^/P:/' -e p -` over four lines prints `P:o1 o2 P:o3 o4`
    /// there -- and two readers cannot produce that however they are positioned.
    fd0: Option<Fd0>,
    /// How much a fill may take off the descriptor. `Self::BLOCK` normally, and
    /// ONE under `-u`, which is how GNU consumes exactly a record there: a reader
    /// cannot RECOGNISE a separator without having read past it, so leaving the
    /// rest of a pipe for the next program means asking for a byte at a time.
    block: usize,
    /// Whether the OPEN operand is descriptor 0, which is what says WHICH reader
    /// the cycle takes records from.
    on_stdin: bool,
    /// Bytes of the open operand already handed to the evaluator. With the
    /// reader's `consumed()` this is the whole of the give-back accounting.
    delivered: u64,
    /// One record read AHEAD. `$` cannot be answered without having looked, and
    /// the record that answered it is the next one the cycle gets -- so it is
    /// held here rather than re-read, and it is NOT counted as delivered until
    /// it is handed out.
    ahead: Option<(Line, bool)>,
    /// A read failure already suffered. It outranks a quit code, and because
    /// operands open lazily it can only describe a file the run actually reached.
    bad: bool,
    /// An operand the READER opened and could not read, which ends the run at 4.
    /// The diagnostic is printed where it happens, ahead of the buffered output,
    /// so it lands beside the other operand diagnostics as GNU's does.
    fatal: bool,
}

/// The run's reader over descriptor 0, and the accounting that has to travel with
/// it once it outlives an operand.
struct Fd0 {
    rec: Records<Src>,
    /// Bytes handed out of THIS reader, to a cycle or to `R`, across every operand
    /// it has served. `consumed()` counts from the reader's creation, so the
    /// give-back needs a delivered count of the same span -- where `Stream`'s own
    /// is per operand, for the torn-read test.
    delivered: u64,
    /// Whether it is the raw duplicate. A fallback through `std::io::Stdin` reads
    /// 8 KiB for a 4 KiB request, so what it over-read cannot be counted and
    /// nothing may be handed back. See `source_of`.
    raw: bool,
}

impl Fd0 {
    /// Bytes taken from standard input and never handed out as a record, which is
    /// what a `q` leaves behind. `records` partitions the buffer exactly -- every
    /// byte is in some record or is the separator its `terminated` flag counts --
    /// so subtracting what was delivered is the whole of what was over-read.
    fn unconsumed(&self) -> Option<u64> {
        if !self.raw {
            return None;
        }
        // A spent reader owes nothing: it is spent because a read returned end of
        // input, so everything it took has been handed out. That now includes what
        // `R /dev/stdin` took, which is delivered as much as a cycle's record is --
        // it was printed -- and handing it back would repeat it to the next reader.
        Some(self.rec.consumed().saturating_sub(self.delivered))
    }
}

/// What a `Stream` reads from: an operand, opened either by the stream itself or
/// by the `-s`/`-i` caller that has to decide whether the file may be rewritten.
enum Src {
    Open(Input),
    /// Descriptor 0, UNBUFFERED. `Input::Stdin` reads through `std::io::Stdin`,
    /// which is a `BufReader`: a 4 KiB request takes 8 KiB off the descriptor, so
    /// what a `q` leaves in a pipe would be std's buffer size rather than
    /// `BLOCK`. Measured before this existed -- `sed 1q` over 20000 bytes left
    /// 11808 where GNU leaves 15904, one whole block too few. Where the duplicate
    /// cannot be had, `-u`'s block of ONE is defeated the same way and for the
    /// same reason -- which is the case the give-back already declines in.
    Raw(std::fs::File),
}

impl std::io::Read for Src {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Open(input) => input.read(buf),
            Self::Raw(file) => file.read(buf),
        }
    }
}

/// The reader for an opened operand, and whether what it takes off descriptor 0
/// can be HANDED BACK. Stdin goes through a raw duplicate so `BLOCK` is what
/// reaches the kernel; a duplication that fails falls back to the buffered path
/// rather than failing the run — EMFILE must not turn a working run into an error
/// — and the give-back goes with it, because `std::io::Stdin` takes 8 KiB off the
/// descriptor for a 4 KiB request. Seeking back by what the reader counted would
/// then land a block short, and the next reader would SKIP that block: not
/// repositioning at all is the honest answer, and it is what happened before any
/// of this existed.
fn source_of(input: Input) -> (Src, bool) {
    if !input.is_stdin() {
        return (Src::Open(input), false);
    }
    match crate::util::stdin_unbuffered() {
        Some(file) => (Src::Raw(file), true),
        None => (Src::Open(input), false),
    }
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
    /// One operand the `-s`/`-i` caller has already OPENED -- that path opens each
    /// file itself to decide whether it may be rewritten -- and streams from here
    /// exactly as the single-stream path streams its own. The descriptor-0 reader
    /// comes IN because that path makes a `Stream` per operand while there is one
    /// standard input for the run: under `-s`, `R /dev/stdin` goes on where the
    /// previous operand left it rather than starting again.
    fn over(path: &[u8], input: Input, separator: u8, block: usize, fd0: Option<Fd0>) -> Self {
        let err_name = input.error_name(path);
        let mut stream = Self {
            pending: Vec::new().into_iter(),
            separator,
            name: path.to_vec(),
            src: None,
            err_name,
            block,
            fd0,
            on_stdin: false,
            delivered: 0,
            ahead: None,
            bad: false,
            fatal: false,
        };
        stream.open(input);
        stream
    }

    /// Every operand, none of them read yet. Taken BY VALUE: nothing revisits an
    /// operand, so nothing needs a copy of one.
    fn of_operands(paths: Vec<Vec<u8>>, separator: u8, block: usize) -> Self {
        Self {
            pending: paths.into_iter(),
            separator,
            name: b"-".to_vec(),
            src: None,
            err_name: b"-".to_vec(),
            block,
            fd0: None,
            on_stdin: false,
            delivered: 0,
            ahead: None,
            bad: false,
            fatal: false,
        }
    }

    /// Take the descriptor-0 reader, for the caller that makes the NEXT `Stream`.
    fn take_fd0(&mut self) -> Option<Fd0> {
        self.fd0.take()
    }

    /// Start reading an opened operand. Standard input goes through the run's one
    /// reader; anything else gets its own, per operand as before.
    fn open(&mut self, input: Input) {
        self.delivered = 0;
        if !input.is_stdin() {
            self.on_stdin = false;
            self.src = Some(Records::with_buffer(Src::Open(input), self.separator, self.block));
            return;
        }
        self.open_fd0(input);
        self.on_stdin = true;
    }

    /// The reader over descriptor 0, made once per RUN. A second operand naming it
    /// -- or an `R /dev/stdin` before one -- continues through the same buffer,
    /// which is what keeps one position between them.
    fn open_fd0(&mut self, input: Input) {
        if self.fd0.is_some() {
            return;
        }
        let (src, raw) = source_of(input);
        let rec = Records::with_buffer(src, self.separator, self.block);
        self.fd0 = Some(Fd0 { rec, delivered: 0, raw });
    }

    /// Count a record as handed out of descriptor 0's reader, which is what the
    /// give-back subtracts. A record `R` took is delivered as much as a cycle's is:
    /// it was printed, so handing it back would repeat it to the next reader.
    fn count_fd0(&mut self, line: &Line) {
        let took = (line.text.len() as u64).saturating_add(u64::from(line.terminated));
        if let Some(fd0) = self.fd0.as_mut() {
            fd0.delivered = fd0.delivered.saturating_add(took);
        }
    }

    /// The reader the CYCLE takes records from.
    fn reader(&mut self) -> Option<&mut Records<Src>> {
        match self.on_stdin {
            true => self.fd0.as_mut().map(|fd0| &mut fd0.rec),
            false => self.src.as_mut(),
        }
    }

    /// End the OPEN operand. Descriptor 0's reader is KEPT even then: `R
    /// /dev/stdin` may still ask, and what it must see is the end of input the
    /// cycle just saw rather than a fresh buffer over the same descriptor.
    fn close_current(&mut self) {
        match self.on_stdin {
            true => self.on_stdin = false,
            false => self.src = None,
        }
    }

    /// One record from descriptor 0 for `R /dev/stdin`, off the run's own reader --
    /// the same one an operand reads when it names standard input. GNU's
    /// interleaving then FOLLOWS from there being one position rather than being
    /// arranged, and so does what a later `-` operand sees.
    ///
    /// A read failure is the run's, exit 4, and names the STREAM rather than the
    /// path the script spelled -- `sed -n -e 'R /dev/stdin' -e p IN < DIR` is
    /// `read error on stdin: Is a directory` in GNU, with nothing printed, because
    /// `R` runs before the cycle's own output does.
    fn stdin_record(&mut self) -> Result<Option<Line>, Vec<u8>> {
        // Peeked BEFORE taken: `take` on a record the lookahead holds for an
        // OPERAND would drop it, and that operand's line is then never delivered.
        if matches!(self.ahead, Some((_, true))) {
            if let Some((line, _)) = self.ahead.take() {
                self.count_fd0(&line);
                return Ok(Some(line));
            }
        }
        if self.fd0.is_none() {
            self.open_fd0(Input::Stdin);
        }
        let Some(fd0) = self.fd0.as_mut() else {
            return Ok(None);
        };
        let line = match fd0.rec.next() {
            Ok(true) => Line { text: fd0.rec.line().to_vec(), terminated: fd0.rec.terminated() },
            Ok(false) => return Ok(None),
            Err(e) => return Err(read_error(Special::In.name().as_bytes(), &e)),
        };
        self.count_fd0(&line);
        Ok(Some(line))
    }

    /// GNU sed's read block, and what it buys is what a `q` LEAVES BEHIND: GNU
    /// reads one block and the rest of a pipe stays readable by the next process,
    /// where a whole-file reader consumed all of it. Measured on GNU 4.9 with
    /// `sed 1q`: 8192 bytes in leaves exactly 4096, and 20000 leaves 15904.
    /// Agreeing with GNU on that number FOLLOWS from the size rather than being a
    /// contract td-txt owes -- the same answer grep's landing reached for its
    /// 96 KiB.
    const BLOCK: usize = 4096;

    /// The next record, opening operands until one HAS A LINE -- reporting each
    /// that cannot be OPENED and stepping over each that is empty, which is GNU's
    /// `last_file_with_data_p` and also how its `$` test answers. `None` once
    /// nothing is left, and once a read failed under the reader.
    ///
    /// This does NOT count what it returns as delivered; `next_line` does, because
    /// a record pulled to answer `$` has been read and not yet used.
    fn pull(&mut self, opener: Opener) -> Option<(Line, bool)> {
        loop {
            let from_fd0 = self.on_stdin;
            if let Some(src) = self.reader() {
                match src.next() {
                    Ok(true) => {
                        let line = Line {
                            text: src.line().to_vec(),
                            terminated: src.terminated(),
                        };
                        return Some((line, from_fd0));
                    }
                    Ok(false) => self.close_current(),
                    // The bytes a failed read DID deliver are handed over first --
                    // the reader defers the failure for exactly that -- so by here
                    // there is nothing left of this operand but the error. GNU
                    // panics at it rather than processing further.
                    Err(e) => {
                        self.close_current();
                        // A LOOKAHEAD is silent only about an operand that has
                        // given nothing: to a peek, one that cannot be read at all
                        // is indistinguishable from an empty one, which is what
                        // GNU's `getc` sees and why `sed -n '$p' A DIR` is exit 0
                        // there and here. An operand that already handed over
                        // records and THEN failed is a torn read, not an empty
                        // file, and the run has processed a prefix of something it
                        // cannot finish -- so it is reported wherever it is met.
                        // Under `-i` that distinction is the operand: a buffer
                        // built from the records before a failure, written back,
                        // TRUNCATES the file the failure interrupted.
                        let torn = self.delivered > 0;
                        if torn || matches!(opener, Opener::Reader) {
                            diag(&read_error(&self.err_name, &e));
                            self.fatal = true;
                            return None;
                        }
                    }
                }
            }
            let path = self.pending.next()?;
            // GNU sets the name before it tries to open, so a failed operand is
            // what `F` would report until the next open replaces it.
            self.name = path;
            match Input::open(&self.name, true) {
                Ok(input) => {
                    self.err_name = input.error_name(&self.name);
                    // `open` resets the per-operand delivered count, as the
                    // whole-file reader's accounting did: it replaced its line
                    // vector on every open.
                    self.open(input);
                }
                Err(e) => {
                    diag(&crate::util::name_in(
                        "can't read ",
                        &self.name,
                        &format!(": {}", errmsg(&e)),
                    ));
                    self.bad = true;
                }
            }
        }
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
        let (line, from_fd0) = match self.ahead.take() {
            Some(held) => held,
            None => self.pull(opener)?,
        };
        // Counted HERE and not in `pull`, so a record read to answer `$` and never
        // used is still owed back to descriptor 0.
        let took = (line.text.len() as u64).saturating_add(u64::from(line.terminated));
        self.delivered = self.delivered.saturating_add(took);
        // The descriptor-0 count is the one the give-back subtracts, and it spans
        // every operand the reader has served rather than this one.
        if from_fd0 {
            self.count_fd0(&line);
        }
        Some(line)
    }

    /// No further input line, so the line just read was `$`. Takes `&mut` because
    /// ANSWERING opens operands: that is observable, through `F` and through which
    /// unreadable operand gets reported, so only where GNU asks may this — a `$`
    /// the evaluator reaches, and the `test_eof` inside `n` and `N`.
    fn at_last(&mut self) -> bool {
        if self.ahead.is_none() {
            self.ahead = self.pull(Opener::Lookahead);
        }
        self.ahead.is_none()
    }
}

enum Append {
    /// `a`'s text, `None` where the command carries none at all (`sed 'a\'` --
    /// ONE backslash, where two are a one-character text that appends a line).
    /// Queued even then, because the QUEUE is what pays the record's owed
    /// separator; see `flush_appends`.
    Text(Option<Vec<u8>>),
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

    /// The most one write may carry and still land on stdio's boundaries. An
    /// in-place run's replacement buffer has none, so it takes the lot -- a
    /// saturated value, safe only because its one consumer is `chunks`, which
    /// needs a non-zero length and nothing else.
    fn piece(&self) -> usize {
        match &self.dest {
            Dest::Stdout(out) => out.piece(),
            Dest::Buffer(_) => usize::MAX,
        }
    }

    fn debt(&mut self, chan: Chan) -> &mut bool {
        match chan {
            Chan::Main => &mut self.owed,
            Chan::WFile => &mut self.owed_wfile,
        }
    }

    fn put(&mut self, bytes: &[u8]) -> Result<(), Vec<u8>> {
        match &mut self.dest {
            Dest::Stdout(out) => out.write(bytes).map_err(|e| format!("write error: {}", errmsg(&e)).into_bytes()),
            // `-i` holds the whole edited file, so a source `r` dumps into it is a
            // reachable allocation failure: diagnosed, exit 4, rather than the
            // abort a bare `extend_from_slice` gives.
            Dest::Buffer(buf) => match buf.try_reserve(bytes.len()) {
                Ok(()) => {
                    buf.extend_from_slice(bytes);
                    Ok(())
                }
                Err(_) => Err(format!("write error: {}", errmsg(&crate::util::oom())).into_bytes()),
            },
        }
    }

    /// Has the READER gone away? Only a stdout sink can answer yes — `-i`'s buffer
    /// has no reader to lose — and `Out` swallows the EPIPE, so a loop that writes
    /// without returning has to ask.
    fn is_broken(&self) -> bool {
        match &self.dest {
            Dest::Stdout(out) => out.is_broken(),
            Dest::Buffer(_) => false,
        }
    }

    /// Pay a separator the channel's previous line left owed, if any.
    fn pay(&mut self, chan: Chan) -> Result<(), Vec<u8>> {
        if *self.debt(chan) {
            *self.debt(chan) = false;
            let sep = self.separator;
            return self.put(&[sep]);
        }
        Ok(())
    }

    /// Write, paying any separator the previous line left owed.
    /// One whole output operation -- `l`, `=`, `F` -- separator and all, which is
    /// the unit `-u` flushes after.
    fn write(&mut self, bytes: &[u8]) -> Result<(), Vec<u8>> {
        self.queued(bytes)?;
        self.end_line()
    }

    /// The same write from INSIDE the append queue, which does not flush: GNU
    /// dumps the whole queue and flushes once at the end of it (`dump_append`,
    /// execute.c:505), so a queue holding two items is one `write(2)` there.
    fn queued(&mut self, bytes: &[u8]) -> Result<(), Vec<u8>> {
        self.pay(Chan::Main)?;
        self.put(bytes)
    }

    /// Queued `a`/`r` text. GNU terminates it with a NEWLINE even under `-z`
    /// (the append queue dumps the text as parsed), where `i`/`c` follow the
    /// record separator.
    fn write_text(&mut self, bytes: &[u8]) -> Result<(), Vec<u8>> {
        self.queued(bytes)?;
        self.put(b"\n")
    }

    /// Write one line; an unterminated one owes its separator to the next write on
    /// the same channel.
    fn write_line_on(&mut self, chan: Chan, bytes: &[u8], terminated: bool) -> Result<(), Vec<u8>> {
        self.pay(chan)?;
        self.put(bytes)?;
        if terminated {
            let sep = self.separator;
            self.put(&[sep])?;
        } else {
            *self.debt(chan) = true;
        }
        self.end_line()
    }

    /// Where `-u`'s flush lands. An UNTERMINATED line is one too: GNU flushes at
    /// the end of `output_line` whether or not it wrote a separator.
    fn end_line(&mut self) -> Result<(), Vec<u8>> {
        match &mut self.dest {
            Dest::Stdout(out) => {
                out.end_line().map_err(|e| format!("write error: {}", errmsg(&e)).into_bytes())
            }
            Dest::Buffer(_) => Ok(()),
        }
    }

    fn write_line(&mut self, bytes: &[u8], terminated: bool) -> Result<(), Vec<u8>> {
        self.write_line_on(Chan::Main, bytes, terminated)
    }

    /// Pay a separator that an unterminated line left owed. Reaching end of input
    /// leaves it owed forever, which is how a file with no final newline keeps that
    /// shape; `q` settles the debt instead — the MAIN channel's only, since GNU's
    /// `q` leaves an unterminated `w /dev/stdout` write bare.
    fn settle(&mut self) -> Result<(), Vec<u8>> {
        self.pay(Chan::Main)
    }

    fn flush(&mut self) -> Result<(), Vec<u8>> {
        match &mut self.dest {
            Dest::Stdout(out) => out.flush().map_err(|e| format!("write error: {}", errmsg(&e)).into_bytes()),
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
    /// Where this target came in the OPEN order, which is not the order the map
    /// iterates in and is the order the end-of-run flush has to use.
    opened: usize,
}

#[derive(Debug)]
enum WDest {
    File(crate::util::StdioBuf),
    Stderr,
    /// `/dev/stdout` under `-i` only; otherwise it rides the auto-print sink. It
    /// takes stdio's buffer over a DUPLICATE of descriptor 1 rather than writing
    /// through `std::io::stdout()`, which is a `LineWriter`: GNU's is the ordinary
    /// block-buffered `stdout`, so `sed -i -n -e 'w /dev/stdout' -e 's/^/E/' -e
    /// 'w /dev/stderr'` grouped there and interleaved here, and 300 lines cost
    /// 300 `write(2)`s here against GNU's two.
    /// Empty until something actually writes here, which under anything but `-i`
    /// never happens -- `write_to_file` sends those to the sink instead. Duping
    /// at compile time would hold a descriptor for the whole run and could fail a
    /// script that never needed it, and asking `in_place` HERE as well as at the
    /// write would be two places deciding one thing.
    Stdout(Option<crate::util::StdioBuf>),
    /// `/dev/stdin`, which every write refuses.
    Stdin,
}

impl WFile {
    fn put(&mut self, bytes: &[u8]) -> Result<(), Vec<u8>> {
        use std::io::Write as _;
        let wrote = match &mut self.dest {
            WDest::File(buf) => buf.put(bytes),
            WDest::Stdout(slot) => match slot {
                Some(buf) => buf.put(bytes),
                None => crate::util::StdioBuf::over_stdout()
                    .and_then(|buf| slot.insert(buf).put(bytes)),
            },
            // Unbuffered on purpose: this arm is C's `stderr`, which is. Under
            // `--posix` the name is an ordinary path and takes the `File` arm
            // above, where GNU's `fopen` buffers it too.
            WDest::Stderr => std::io::stderr().write_all(bytes),
            WDest::Stdin => return refuse_write(bytes.len()),
        };
        wrote.map_err(|e| format!("write error: {}", errmsg(&e)).into_bytes())
    }

    /// Push what the buffer holds. Called after every line under `-u`, and for
    /// every target at the END of a run however that run ended. GNU reaches the
    /// same bytes two ways: `finish_program` (sed.c:380) walks `file_write` and
    /// closes each, which is the route a `q` takes too since `q` RETURNS rather
    /// than exiting; and a fatal read instead `panic`s into plain `exit`, whose
    /// stdio teardown writes what it can.
    fn flush(&mut self) -> Result<(), Vec<u8>> {
        let flushed = match &mut self.dest {
            WDest::File(buf) | WDest::Stdout(Some(buf)) => buf.flush(),
            // Never written to, so there is nothing to push and no dup to make.
            WDest::Stdout(None) | WDest::Stderr | WDest::Stdin => return Ok(()),
        };
        flushed.map_err(|e| format!("write error: {}", errmsg(&e)).into_bytes())
    }

    fn write_line(&mut self, bytes: &[u8], terminated: bool, separator: u8) -> Result<(), Vec<u8>> {
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

impl Sed {
    /// Every `w` target's buffer, BEFORE the auto-print sink's own flush. That is
    /// GNU's order twice over: `finish_program` closes `file_write` on the way out
    /// of `main` (sed.c:380) and only then does `ck_fclose (NULL)` close stdout,
    /// which it does last deliberately. It is what puts a `--posix`
    /// `w /dev/stdout` ahead of auto-print.
    ///
    /// LAST-OPENED FIRST, because `get_openfile` PREPENDS to `file_write`
    /// (compile.c:425) and the walk starts at the head. Two targets that both
    /// reach standard output are what see it: this map iterates by path, so
    /// `w /dev/fd/1` before `w /dev/stdout` would come out the opposite way.
    ///
    /// Only the exits that flush the SINK need call it, and they must do so
    /// first. Everywhere else -- a `?` out of anywhere in the run -- the order is
    /// structural: `out` outlives `sed`, so the drops already fall this way
    /// round. What these calls add on top is the error a drop cannot report.
    fn flush_wfiles(&mut self) -> Result<(), Vec<u8>> {
        let mut order: Vec<&mut WFile> = self.wfiles.values_mut().collect();
        order.sort_by_key(|w| std::cmp::Reverse(w.opened));
        // EVERY target, not up to the first failure: GNU's walk closes them all
        // and its panic exits through stdio's teardown, which flushes the rest.
        // Stopping here would leave a later target to `Drop` -- that is, to AFTER
        // the sink, which is the one thing this ordering exists to prevent, and a
        // full disk on one target would silently reorder another.
        let mut failed = None;
        for w in order {
            if let Err(e) = w.flush() {
                failed = failed.or(Some(e));
            }
        }
        match failed {
            Some(e) => Err(e),
            None => Ok(()),
        }
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
    /// `posixicity == POSIXLY_EXTENDED`, not `!--posix`. The only runtime read
    /// of the compile's posixicity, and the two are not the same question: the
    /// third level, which `POSIXLY_CORRECT` selects, is neither.
    extended: bool,
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
    /// `-u`, of which a `w` target owns a share: GNU flushes every output after
    /// every line, and a `w` file is an output.
    unbuffered: bool,
    terminated: bool,
    appends: Vec<Append>,
    replaced: bool,
    quit: Option<i32>,
    /// Index in the regex table of the last regex applied — what `//` reuses.
    last_regex: Option<usize>,
    wfiles: BTreeMap<FileTarget, WFile>,
    /// `R` reads ONE line per invocation, so the file is parsed once and the
    /// cursor kept; re-reading per line would be quadratic. Keyed by NAME, not
    /// per command: two `R f` commands share one cursor in GNU, and advance it
    /// twice in a cycle.
    rfiles: BTreeMap<FileTarget, RFile>,
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
) -> Result<(bool, Option<usize>), Vec<u8>> {
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
    fn addr_matches(&mut self, idx: usize, stream: &mut Stream) -> Result<bool, Vec<u8>> {
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
    ) -> Result<(bool, bool), Vec<u8>> {
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
    ) -> Result<(bool, bool), Vec<u8>> {
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

    fn match_a1(&mut self, idx: usize, stream: &mut Stream) -> Result<bool, Vec<u8>> {
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

/// One `-e`/`-f` fragment of the script, and where in `source` it ends. GNU
/// numbers `-e` parts and NAMES `-f` ones, and the counter does not advance for
/// a file -- `sed -f s.sed -e Z` is `expression #1`.
#[derive(Debug)]
enum Origin {
    /// `-e`, `--expression=`, or the bare script operand.
    Expression(usize),
    /// `-f`/`--file=`, carrying the name as it was written (`-` stays `-`).
    File(Vec<u8>),
}

#[derive(Debug)]
struct Part {
    /// Offset in `source` one past this part's last byte.
    end: usize,
    origin: Origin,
    /// The compile flags in effect when this part was SCANNED, which is what GNU
    /// compiles it under.
    mode: Mode,
}

/// Where a diagnostic points, which is not always a place the parse reached.
#[derive(Clone, Copy)]
enum Spot {
    /// A position the parser stopped at. Its distance from the part's start is
    /// GNU's `char N`.
    Read(usize),
    /// A location GNU SAVED and reported later: the `{` of an unmatched block,
    /// and the end of a compiled script for the one error deferred to run time.
    /// It picks the part the same way, but a saved location carries no character
    /// count and GNU prints `char 0`.
    Saved(usize),
}

/// How GNU names the place a script failed to compile. An `-e` part is named by
/// NUMBER and a CHARACTER COUNT within that part; an `-f` one by file and LINE,
/// the line counted within that file and no char at all -- so the position has
/// to be resolved against the part it falls in, not the whole joined script.
///
/// `pos <= end` rather than `<`: a position AT a part's end has consumed that
/// whole part and nothing of the next, which is the ordinary shape of an error
/// raised on a part's last byte. GNU compiles each part on its own, so it can
/// never blame the one after. The last part's `end` is the whole source's
/// length and the parser never steps past it, so the search reaches a part for
/// every position it is asked about; None here means an empty script.
fn locus_at(parts: &[Part], src: &[u8], spot: Spot) -> Option<Vec<u8>> {
    let (pos, saved) = match spot {
        Spot::Read(pos) => (pos, false),
        Spot::Saved(pos) => (pos, true),
    };
    // `position` rather than the iterator method whose NAME is also a retired
    // host tool's: this file is embedded verbatim in td-txt's recipe, and the
    // ladder guard scans that text for the bare token (recipes/src/ladder.rs).
    let idx = parts.iter().position(|p| pos <= p.end)?;
    let part = parts.get(idx)?;
    let start = match idx.checked_sub(1) {
        // +1 steps over the newline joining this part to the one before it.
        Some(prev) => parts.get(prev).map_or(0, |p| p.end + 1),
        None => 0,
    };
    match &part.origin {
        Origin::Expression(n) => {
            let ch = if saved { 0 } else { pos.saturating_sub(start) };
            Some(format!("-e expression #{n}, char {ch}").into_bytes())
        }
        Origin::File(name) => {
            let within = src.get(start..pos).unwrap_or_default();
            let line = 1 + within.iter().filter(|b| **b == b'\n').count();
            // The NAME goes out as GNU's `%s` writes it, raw: `show` would send a
            // non-UTF-8 one through a replacement character.
            let mut out = b"file ".to_vec();
            out.extend_from_slice(name);
            out.extend_from_slice(format!(" line {line}").as_bytes());
            Some(out)
        }
    }
}

#[derive(Debug)]
/// A fatal error and the status GNU sed exits with for it: 1 for a bad script
/// or usage, 2 for an unreadable input, 4 for a runtime failure — of which an
/// unresolvable branch label is one, because GNU resolves labels while running.
struct Fatal {
    /// BYTES rather than a `String`, because GNU builds each diagnostic with
    /// `sprintf` and prints it with `%s`: a script byte it quotes goes out as
    /// itself, and a `-f` script's name does too.
    msg: Vec<u8>,
    status: i32,
    /// Where the script went wrong, already formatted: `-e expression #2` or
    /// `file s.sed line 3`. Only a COMPILE failure has one; a runtime failure is
    /// not about a place in the script and GNU reports it bare.
    locus: Option<Vec<u8>>,
}

impl From<String> for Fatal {
    fn from(msg: String) -> Self {
        Self::from(msg.into_bytes())
    }
}

impl From<Vec<u8>> for Fatal {
    fn from(msg: Vec<u8>) -> Self {
        // GNU reports the `[:alpha:]`-for-`[[:alpha:]]` refusal bare and exits 4,
        // alone among pattern errors; classified here by text for the same reason
        // NO_PREVIOUS_REGEX is, so the raising site and this boundary cannot drift.
        let status = if msg == crate::regex::CLASS_SYNTAX.as_bytes() { 4 } else { 1 };
        Self { msg, status, locus: None }
    }
}

/// The one error raised WHILE RUNNING that GNU still classifies as a bad SCRIPT:
/// an empty `s//…/` or `//p` can only be checked once the previous regex is known,
/// but it is a mistake in the program, not a refusal by the filesystem — exit 1,
/// with the `-e expression #N' prefix. One constant, named at the sites that raise
/// it and at the boundary that classifies it, so the two cannot drift apart.
const NO_PREVIOUS_REGEX: &str = "no previous regular expression";

/// What one `s///` reports: whether it substituted, whether the `p` flag asks
/// for a print, and the file a `w` flag owes the pattern space to.
type SubstOutcome = (bool, bool, Option<FileTarget>);

impl Fatal {
    /// A failure the FILESYSTEM raised rather than the script — a `w` file that will
    /// not open, a write that fails. Exit 4, and no `-e expression #N' prefix: the
    /// script was well formed and something outside it refused. Most of these
    /// happen while running, but a `w` target opens during the PARSE, so this is
    /// reachable from there too. `From<String>` stamps 1 for the malformed-script
    /// errors `parse_script` raises, so every such `String` has to come through
    /// here or it lands in the wrong bucket wearing the wrong prefix.
    fn runtime(msg: String) -> Self {
        Self::runtime_msg(msg.into_bytes())
    }

    /// `runtime` for a message that already carries raw bytes -- a file name GNU
    /// writes through `%s`, which cannot survive a `String`.
    fn runtime_msg(msg: Vec<u8>) -> Self {
        let status = if msg == NO_PREVIOUS_REGEX.as_bytes() { 1 } else { 4 };
        Self { msg, status, locus: None }
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
    /// `-u`: read a RECORD at a time rather than a block, and flush every output
    /// after every line. Both halves are one flag because both are what "do not
    /// buffer" means from either end -- see spec/README.
    unbuffered: bool,
    /// The width a bare `l` folds at: `COLS` seeds it, `-l N` overrides it, and
    /// with neither it is 70.
    line_wrap: usize,
}

/// `couldn't open file NAME: WHY` with the NAME raw, as GNU's `%s` writes it.
fn cant_open(name: &[u8], e: &std::io::Error) -> Vec<u8> {
    crate::util::name_in("couldn't open file ", name, &format!(": {}", errmsg(e)))
}

/// One diagnostic line, written as BYTES. `eprintln!` of a `String` cannot carry
/// a script byte from 0x80 up -- it would arrive UTF-8 encoded as two -- and
/// cannot stop at a NUL.
fn diag(msg: &[u8]) {
    diag_at(None, msg);
}

/// `diag` with the place in the script GNU names first. The MESSAGE is what a
/// NUL truncates and the locus is not: GNU passes each as its own `%s`, and only
/// the message can hold one, a locus naming a `-f` file that came from argv.
fn diag_at(locus: Option<&[u8]>, msg: &[u8]) {
    let mut out = b"sed: ".to_vec();
    if let Some(locus) = locus {
        out.extend_from_slice(locus);
        out.extend_from_slice(b": ");
    }
    out.extend_from_slice(crate::util::cstr(msg));
    out.push(b'\n');
    let _ = std::io::stderr().write_all(&out);
}

pub fn main(args: &[Vec<u8>]) -> i32 {
    match run(args) {
        Ok(code) => code,
        Err(f) => {
            // A LOCUS is attached only where the script is what is wrong, so
            // having one IS the condition: a runtime failure — an unopenable
            // `-f' file, an unresolvable label (exit 4) — is not about a place
            // in the script and GNU reports it bare.
            diag_at(f.locus.as_deref(), &f.msg);
            f.status
        }
    }
}

/// The compile flags as they stand at THIS point in the option scan. Called at
/// every script part rather than once at the end, which is the whole of what
/// makes a flag govern only what follows it.
fn mode_of(conf: &Conf, posixly: bool) -> Mode {
    Mode {
        posix: conf.posix,
        // GNU's third posixicity level is the one BOTH other switches leave, and
        // it is deliberately not `!posix`: the variable and the flag differ
        // elsewhere on purpose.
        extended: !conf.posix && !posixly,
        ere: conf.ere,
        sandbox: conf.sandbox,
    }
}

/// Parse a script and resolve its branches. Split from `parse_script` so the two
/// failures keep their own exit statuses (see `Fatal`).
fn compile_script(
    src: &[u8],
    parts: Vec<Part>,
    fallback: Mode,
    inv: Invocation,
) -> Result<Script, Fatal> {
    let mut script = parse_script(src, parts, fallback, inv)?;
    let labels = std::mem::take(&mut script.labels);
    resolve_labels(&mut script.cmds, &labels)
        .map_err(|msg| Fatal { msg, status: 4, locus: None })?;
    Ok(script)
}

/// Every long option sed knows, so an unambiguous PREFIX resolves the way GNU's
/// `getopt_long` accepts one (`sed --quie`, `sed --expr=2d`). An exact name
/// always wins over being a prefix of a longer one.
///
/// A transcription of GNU's `longopts[]` (sed.c:197), in ITS order rather than
/// alphabetically: an ambiguous abbreviation lists its possibilities in the
/// order `getopt_long` walks the table, so `--s` is `'--silent' '--sandbox'
/// '--separate'` and sorting the names would report the same set in the wrong
/// order. Resolution itself does not depend on it -- an exact name returns
/// before any prefix is collected -- so this array's order is a DIAGNOSTIC, and
/// that is the only reason it is not sorted.
///
/// The flag is whether the option accepts a VALUE at all, which is all
/// `getopt_long` needs to refuse `--posix=1`. GNU's required/optional
/// distinction is not repeated here because the dispatch below already carries
/// it, and a second encoding of one fact is one that can drift from the first.
const LONG_OPTIONS: &[(&[u8], bool)] = &[
    (b"binary", false),
    (b"regexp-extended", false),
    (b"debug", false),
    (b"expression", true),
    (b"file", true),
    (b"in-place", true),
    (b"line-length", true),
    (b"null-data", false),
    (b"zero-terminated", false),
    (b"quiet", false),
    (b"posix", false),
    (b"silent", false),
    (b"sandbox", false),
    (b"separate", false),
    (b"unbuffered", false),
    (b"version", false),
    (b"help", false),
    (b"follow-symlinks", false),
];

/// `Ok((full name, takes a value))`, or `Err(msg)` where an EMPTY msg means "no
/// such option" and a non-empty one is GNU's ambiguity diagnostic.
///
/// `arg` is the whole argv element and `name` the part of it between the `--`
/// and any `=`. glibc names the ELEMENT in both diagnostics -- `--f` and `--f=1`
/// are the same ambiguity and it reports each as given -- so resolving on one
/// and reporting the other is the point of the two arguments.
fn resolve_long(name: &[u8], arg: &[u8]) -> Result<(&'static [u8], bool), Vec<u8>> {
    let mut hits: Vec<(&'static [u8], bool)> = Vec::new();
    for (cand, takes_arg) in LONG_OPTIONS {
        if *cand == name {
            return Ok((cand, *takes_arg));
        }
        if cand.starts_with(name) {
            hits.push((cand, *takes_arg));
        }
    }
    match hits.as_slice() {
        [one] => Ok(*one),
        [] => Err(Vec::new()),
        many => {
            let mut msg = crate::util::name_in("option '", arg, "' is ambiguous; possibilities:");
            for (n, _) in many {
                msg.extend_from_slice(b" '--");
                msg.extend_from_slice(n);
                msg.push(b'\'');
            }
            Err(msg)
        }
    }
}

/// A missing option ARGUMENT, reported the way GNU reports one: bare, with the
/// usage after it, at exit 1. Not through `Fatal`, whose status-1 path prefixes
/// `-e expression #1:` — true of a script that failed to compile and false of an
/// option that never got its value.
fn missing_short_argument(opt: u8) -> i32 {
    diag(&crate::util::byte_in("option requires an argument -- '", opt, "'"));
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
        // different LEVELS of one setting in GNU, and `conf.posix` is the
        // flag's. What the variable selects reaches the compile and the run
        // through `Mode::extended` -- bar GNU's one rule that reads the
        // environment rather than the level, `dfawarn` (see spec/README).
        posix: false,
        unbuffered: false,
        // Read BEFORE the options, as GNU reads it, so `-l` overrides it and not
        // the other way round.
        line_wrap: std::env::var_os("COLS")
            .and_then(|v| cols_line_wrap(v.as_bytes()))
            .unwrap_or(DEFAULT_LINE_WRAP),
    };
    // Each fragment with where it came from, so a compile error can name the
    // `-e` NUMBER or the `-f` FILE the way GNU does, and with the flags in effect
    // when it was SCANNED, which is what GNU compiles it under.
    let mut script_parts: Vec<(Option<Vec<u8>>, Vec<u8>, Mode)> = Vec::new();
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
            let (name, takes_arg) = match resolve_long(name, arg) {
                Ok(found) => found,
                Err(msg) => {
                    if msg.is_empty() {
                        // The whole argv element, as glibc's `getopt_long` prints
                        // it: `name` stops at the `=`, so `--zz=1` would name
                        // `--zz`, an option the caller never passed. grep splices
                        // `arg` here for the same reason.
                        diag(&crate::util::name_in("unrecognized option '", arg, "'"));
                    } else {
                        diag(&msg);
                    }
                    eprintln!("{USAGE}");
                    return Ok(1);
                }
            };
            // Before any arm sees it, since the check is the TABLE's and not the
            // option's: `--posix=1` is refused for the reason `--version=x` is,
            // and an arm that simply drops `inline` (every flag arm below does)
            // would accept a value GNU calls an error. The name reported is the
            // RESOLVED one, as glibc reports it, so `--po=1` names `--posix`.
            if !takes_arg && inline.is_some() {
                diag(&crate::util::name_in("option '--", name, "' doesn't allow an argument"));
                eprintln!("{USAGE}");
                return Ok(1);
            }
            match name {
                // Answered on stdout, exit 0, before any later option applies.
                // The text is td-txt's own: a GNU banner would be a lie a caller
                // could act on.
                b"help" | b"version" => {
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
                b"unbuffered" => conf.unbuffered = true,
                // Accepted and ignored for a DIFFERENT reason: GNU's `--binary`
                // chooses binary I/O, which exists only where a text mode does,
                // so on every platform td targets it already does nothing. That
                // is why ignoring it fails CLOSED where ignoring `--debug` would
                // not -- there is no behaviour left unmodelled behind it.
                b"binary" => {}
                // Accepting these silently would fail OPEN: --debug must print an
                // annotated program, and --follow-symlinks changes which file -i
                // rewrites. Refusing is the honest answer until they are built.
                b"debug" | b"follow-symlinks" => {
                    diag(&crate::util::name_in("unsupported option -- '", name, "'"));
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
                                    diag(&crate::util::name_in(
                                        "option '--",
                                        name,
                                        "' requires an argument",
                                    ));
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
                            // same wording: the script did not fail to COMPILE, it
                            // failed to arrive — at the open or at the read, which
                            // `ScriptFailure` tells apart.
                            let (text, seekable) = read_script(&value).map_err(|e| Fatal {
                                msg: e.msg(&value),
                                status: 4,
                                locus: None,
                            })?;
                            if script_parts.is_empty() {
                                hash_n_carries = seekable;
                            }
                            script_parts.push((Some(value), text, mode_of(&conf, posixly)));
                        } else {
                            script_parts.push((None, value, mode_of(&conf, posixly)));
                        }
                        script_given = true;
                    }
                }
                // Unreachable: `resolve_long` only ever returns a name from the
                // table and every one of them has an arm above. The match is over
                // `&[u8]`, so the compiler cannot see that and demands this;
                // reaching it would mean a name was added to the table alone.
                _ => {
                    diag(&crate::util::name_in("unrecognized option '", arg, "'"));
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
                b'u' => conf.unbuffered = true,
                // `--binary`'s short spelling: GNU's SHORTOPTS is
                // `"bsnrzuEe:f:l:i::V:"` (sed.c:191) and both reach the same arm.
                // Ignored for the same reason, and taking one without the other
                // would leave `sed -b` refused while `sed --binary` ran.
                b'b' => {}
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
                    script_parts.push((None, v, mode_of(&conf, posixly)));
                    script_given = true;
                }
                b'f' => {
                    let Some(v) = value_of(&mut j, &mut i) else {
                        return Ok(missing_short_argument(b'f'));
                    };
                    // An unreadable SCRIPT file is exit 4, like any other
                    // runtime failure — not 1, which means a bad script.
                    let (text, seekable) = read_script(&v).map_err(|e| Fatal {
                        msg: e.msg(&v),
                        status: 4,
                        locus: None,
                    })?;
                    if script_parts.is_empty() {
                        hash_n_carries = seekable;
                    }
                    script_parts.push((Some(v), text, mode_of(&conf, posixly)));
                    script_given = true;
                }
                _ => {
                    diag(&crate::util::byte_in("invalid option -- '", opt, "'"));
                    eprintln!("{USAGE}");
                    return Ok(1);
                }
            }
        }
    }

    let mut operands = operands.into_iter();
    if !script_given {
        match operands.next() {
            Some(s) => script_parts.push((None, s, mode_of(&conf, posixly))),
            None => {
                eprintln!("{USAGE}");
                return Ok(1);
            }
        }
    }
    let files: Vec<Vec<u8>> = operands.collect();

    let mut source: Vec<u8> = Vec::new();
    let mut parts: Vec<Part> = Vec::new();
    let mut expr_no = 0usize;
    for (n, (name, body, mode)) in script_parts.iter().enumerate() {
        if n > 0 {
            source.push(b'\n');
        }
        source.extend_from_slice(body);
        let origin = match name {
            Some(f) => Origin::File(f.clone()),
            None => {
                expr_no += 1;
                Origin::Expression(expr_no)
            }
        };
        parts.push(Part { end: source.len(), origin, mode: *mode });
    }
    // `#n` is POSIX's in-script spelling of -n, and the rule is about the first
    // two BYTES of the script, not the first line: `#nx` and `#n;p` suppress in
    // GNU as surely as `#n` does, the rest of the line being comment either way.
    // Requiring a newline after them made every such script print twice over.
    if hash_n_carries && source.starts_with(b"#n") {
        conf.suppress = true;
    }

    // GNU reports `no previous regular expression' -- the one SCRIPT error it
    // cannot see until a line RUNS -- with the position its compile left behind,
    // which is the END of the whole script and not where the `//' is: a four-line
    // `-f' script says `line 5', and `-e '//p' -e p' says expression #2. So the
    // locus is taken here, while the parts are still in hand, and attached below
    // to whatever exit-1 failure comes back without one.
    let script_end = locus_at(&parts, &source, Spot::Saved(source.len()));
    // The FINAL flags, which is what run time reads and what a bare script
    // OPERAND was already given: GNU compiles that one after the scan, so unlike
    // an `-e` it cannot be governed by a flag it precedes.
    let mode = mode_of(&conf, posixly);
    let separator = separator_for(conf.null_data);
    let inv = Invocation { null_data: conf.null_data, posixly };
    compile_and_run(conf, mode, inv, &source, parts, files, separator).map_err(|f| {
        match (f.status, &f.locus) {
            (1, None) => Fatal { locus: script_end, ..f },
            _ => f,
        }
    })
}

/// The rest of a run once the script text is assembled: compile it, then read the
/// operands through it. Split from `run` so the LOCUS a deferred script error
/// needs (see the caller) outlives every `?` in here.
fn compile_and_run(
    conf: Conf,
    mode: Mode,
    inv: Invocation,
    source: &[u8],
    parts: Vec<Part>,
    files: Vec<Vec<u8>>,
    separator: u8,
) -> Result<i32, Fatal> {
    let mut script = compile_script(source, parts, mode, inv)?;
    let seed = seed_ranges(&script.cmds);
    // GNU's `posixicity` at the moment the run starts: the option loop's FINAL
    // value plus a compiled `v`. Not a part's scan-time mode -- `--posix` after
    // the last `-e` never reaches one and still governs the run.
    let script_extended = mode.extended_with_v(script.v_promoted);
    let wfiles = std::mem::take(&mut script.wfiles);
    // BEFORE `sed`, which is what makes the `w`-before-sink order structural: a
    // local drops in reverse declaration order, so `sed`'s targets flush and then
    // this does. The explicit flushes below are for their ERROR, which a drop
    // cannot report; ordering no longer depends on anyone remembering them, and
    // an exit nobody enumerated -- a `?` from anywhere in here -- comes out right
    // anyway.
    let mut out = Out::new().map_err(|e| Fatal::runtime(format!("write error: {}", errmsg(&e))))?;
    if conf.unbuffered {
        out.unbuffer();
    }
    let mut sed = Sed {
        script,
        ranges: seed,
        suppress: conf.suppress,
        separator,
        extended: script_extended,
        line_wrap: conf.line_wrap,
        pattern: Vec::new(),
        hold: Vec::new(),
        hold_terminated: true,
        line_number: 0,
        in_place: conf.in_place.is_some(),
        unbuffered: conf.unbuffered,
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
    // `-u` is one flag over both ends: a record at a time IN, a flush after every
    // line OUT. GNU's is `unbuffered_output` plus an unbuffered input stream.
    let block = match conf.unbuffered {
        true => 1,
        false => Stream::BLOCK,
    };
    let mut status = 0;

    if conf.separate || conf.in_place.is_some() {
        // The run's descriptor-0 reader, handed from one operand's `Stream` to the
        // next: `-s` restarts a rewindable `R` source per operand, and standard
        // input is the one that cannot be restarted -- GNU reads on from where the
        // last operand left it, which needs the reader itself to survive.
        let mut fd0: Option<Fd0> = None;
        // ONE stdout sink for every operand: `-s` restarts line numbers and range
        // state per file, but stdout is still one stream, so a separator the last
        // record of one file left owed is paid by the first write of the next.
        // (`-i` writes each file's own buffer, where the debt IS per file.)
        let mut sink = Sink::stdout(&mut out, separator);
        for path in &inputs {
            // Under `-i` an operand names the file to REWRITE, so `-` is an
            // ordinary name here rather than stdin — there is no rewriting a
            // pipe. GNU reports it as the missing file it is.
            let input = match Input::open(path, conf.in_place.is_none()) {
                Ok(input) => input,
                Err(e) => {
                    diag(&crate::util::name_in(
                        "can't read ",
                        path,
                        &format!(": {}", errmsg(&e)),
                    ));
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
                    diag(&crate::util::name_in("couldn't edit ", path, &format!(": {why}")));
                    // This gives up on the whole run, so it is one of the exits
                    // that owes descriptor 0 its position back -- an earlier
                    // operand's `R /dev/stdin` may have over-read it.
                    give_back(fd0.as_mut())?;
                    sed.flush_wfiles().map_err(Fatal::runtime_msg)?;
                    sink.flush().map_err(Fatal::runtime_msg)?;
                    return Ok(4);
                }
            }
            let mut stream = Stream::over(path, input, separator, block, fd0.take());
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
                rfile.rewind();
            }
            let quit = match &conf.in_place {
                // `-i` opens its operand as a NAME (`dash_is_stdin` false above), so
                // the CYCLE never reads standard input here -- but `R /dev/stdin`
                // still can, and then this arm owes the position back like any
                // other.
                Some(suffix) => {
                    let mut buf = Sink::buffer(separator);
                    let ran = sed.run_stream(&mut stream, &mut buf).map_err(Fatal::runtime_msg);
                    // A read that failed part way leaves a buffer holding only what
                    // came BEFORE it, and writing that would rewrite the operand
                    // truncated — where GNU's own failure path unlinks its temp
                    // file and leaves the original alone.
                    let wrote = match ran.is_ok() && !stream.fatal {
                        true => write_in_place(path, suffix, &buf.into_buffer()),
                        false => Ok(()),
                    };
                    fd0 = stream.take_fd0();
                    // `-i` never READS standard input as an operand, but `R
                    // /dev/stdin` still can, so a run that dies here owes the same
                    // repositioning the other arm does.
                    if ran.is_err() || wrote.is_err() {
                        give_back(fd0.as_mut())?;
                    }
                    let quit = ran?;
                    wrote?;
                    quit
                }
                // Held rather than propagated, for the reason the other path
                // holds it: a run that DIES still owes back what it did not read.
                _ => {
                    let ran = sed.run_stream(&mut stream, &mut sink);
                    let quit = ran.map_err(Fatal::runtime_msg);
                    fd0 = stream.take_fd0();
                    // The run's, not this operand's: held rather than propagated,
                    // for the reason the other path holds it -- a run that DIES
                    // still owes back what it did not read.
                    if quit.is_err() {
                        give_back(fd0.as_mut())?;
                    }
                    quit?
                }
            };

            // A read that failed ends the run at 4 here as it does on the other
            // path, and outranks the operands not yet reached: GNU's reader panics,
            // so nothing after it happens.
            if stream.fatal {
                give_back(fd0.as_mut())?;
                // The READ failure is the report, and a flush that fails here adds
                // no second one: GNU's reader panics straight into `exit`, whose
                // stdio teardown writes what it can and says nothing about what it
                // cannot. Measured -- a `/dev/full` target under a fatal read is
                // one diagnostic there and was two here.
                let _ = sed.flush_wfiles();
                sink.flush().map_err(Fatal::runtime_msg)?;
                return Ok(4);
            }
            if let Some(code) = quit {
                give_back(fd0.as_mut())?;
                sed.flush_wfiles().map_err(Fatal::runtime_msg)?;
                sink.flush().map_err(Fatal::runtime_msg)?;
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
        // Once, after the LAST operand: whatever the run over-read of descriptor 0
        // is owed to whoever reads it next, and only here is there nothing left of
        // this run to read it first.
        give_back(fd0.as_mut())?;
    } else {
        // One logical stream: line numbers and `$` span every input. The operands
        // are opened one at a time, so a failure is only suffered where GNU
        // suffers it — reading every one up front reported files a `q` had
        // already quit before, and reported them as SUCCESS.
        let mut stream = Stream::of_operands(inputs, separator, block);
        let mut sink = Sink::stdout(&mut out, separator);
        // Held rather than propagated, because the give-back has to happen
        // whatever the run did: GNU leaves the offset after the records it
        // consumed even when it DIES, so `sed -e '1r .' < four` exits 4 having
        // read one record and leaves the other three. The run's own error still
        // outranks a failure to reposition -- that is the one that says what
        // went wrong.
        let ran = sed.run_stream(&mut stream, &mut sink);
        let back = give_back(stream.fd0.as_mut());
        let quit = ran.map_err(Fatal::runtime_msg)?;
        back?;
        // A read that failed outranks everything else, including a `bad` operand
        // opened earlier: GNU's reader panics there, so nothing after it happened.
        // The flush is for its ERROR — what was written survives either way, since
        // dropping the buffer writes it, but silently.
        if stream.fatal {
            // Same as the `-s` path: the read failure is the one report.
            let _ = sed.flush_wfiles();
            out.flush().map_err(|e| Fatal::runtime(format!("write error: {}", errmsg(&e))))?;
            return Ok(4);
        }
        if stream.bad {
            status = 2;
        }
        if let Some(code) = quit {
            sed.flush_wfiles().map_err(Fatal::runtime_msg)?;
            out.flush().map_err(|e| Fatal::runtime(format!("write error: {}", errmsg(&e))))?;
            // Same rule the `-s` path has always applied, and now sound here too:
            // a read failure ALREADY suffered outranks the quit code.
            return Ok(match status {
                0 => code,
                _ => status,
            });
        }
    }
    sed.flush_wfiles().map_err(Fatal::runtime_msg)?;
    out.flush().map_err(|e| Fatal::runtime(format!("write error: {}", errmsg(&e))))?;
    Ok(status)
}

/// One `R` source and whether the per-operand reset may REWIND it. GNU rewinds
/// with `rewind(3)`, which fails silently on a stream that cannot seek, so a pipe
/// or fifo keeps its place across an `-s` boundary while a regular file restarts.
struct RFile {
    src: RSource,
    rewindable: bool,
}

/// Where an `R` source's lines come from. GNU takes them with `ck_getdelim` off a
/// buffered stream, so a named source is read a BLOCK at a time here: `R` over an
/// endless file has to hand a line to every cycle rather than never returning.
enum RSource {
    Stream(Records<std::fs::File>),
    /// The standard-input special, still read WHOLE. It is the one source that
    /// shares its descriptor with the operand reader, and two readers giving back
    /// their own over-read cannot both be right about a RELATIVE seek; see
    /// spec/README.
    Whole { lines: Vec<Line>, pos: usize },
    /// Opened and spent, or never opened at all -- `R` over a missing file is
    /// silent, and is cached this way so the name is opened at most once.
    Spent,
}

impl RFile {
    /// The next line, or `None` once the source is spent. A read failure is the
    /// run's, exit 4, and a streaming source meets it where the reader REACHES it
    /// rather than at the first `R`.
    fn next(&mut self, name: &[u8]) -> Result<Option<Line>, Vec<u8>> {
        match &mut self.src {
            RSource::Stream(rec) => match rec.next() {
                Ok(true) => {
                    Ok(Some(Line { text: rec.line().to_vec(), terminated: rec.terminated() }))
                }
                Ok(false) => Ok(None),
                Err(e) => Err(read_error(name, &e)),
            },
            RSource::Whole { lines, pos } => match lines.get(*pos) {
                Some(line) => {
                    *pos += 1;
                    Ok(Some(Line { text: line.text.clone(), terminated: line.terminated }))
                }
                None => Ok(None),
            },
            RSource::Spent => Ok(None),
        }
    }

    /// Start the source again, for the per-operand reset under `-s`. `rewind(3)`
    /// does TWO things and only one of them needs a seekable stream: it seeks to
    /// the start, and it clears end of file EVEN WHEN THE SEEK FAILS. So a source
    /// that ended because its last writer went away is read again if a new one
    /// has appeared, while one that simply has no more bytes reads 0 and gives
    /// nothing, exactly as before.
    fn rewind(&mut self) {
        match &mut self.src {
            RSource::Stream(rec) => {
                match self.rewindable
                    && rec.source_mut().seek(std::io::SeekFrom::Start(0)).is_ok()
                {
                    true => rec.restart(),
                    false => rec.forget_eof(),
                }
            }
            RSource::Whole { pos, .. } => {
                if self.rewindable {
                    *pos = 0;
                }
            }
            RSource::Spent => {}
        }
    }
}

/// Read a `-f` script, and say whether `#n` may come from it. GNU's test for a file
/// script is `prog.file && !prog.base && 2 == ftell(prog.file)` -- an ABSOLUTE
/// offset, so the stream must both SEEK (`ftell` is -1 on a pipe, which is why
/// `printf '#n\np' | sed -f -` auto-prints there) and have STARTED at its own
/// beginning (a descriptor handed over already part-read does not carry the rule).
/// A named file is opened here, so the second half holds by construction and only
/// the seek is asked. Stdin can be neither seeked nor reopened, so it gets a PROXY
/// -- see the comment below for where the proxy and GNU part company.
fn read_script(path: &[u8]) -> Result<(Vec<u8>, bool), ScriptFailure> {
    let mut data = Vec::new();
    if path == b"-" {
        std::io::stdin().lock().read_to_end(&mut data).map_err(ScriptFailure::Stdin)?;
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
    let mut file = std::fs::File::open(crate::util::path_from_bytes(path))
        .map_err(ScriptFailure::Open)?;
    file.read_to_end(&mut data).map_err(ScriptFailure::Read)?;
    Ok((data, file.seek(std::io::SeekFrom::Start(0)).is_ok()))
}

/// Which call failed on a `-f` script. The two earn different wordings, and the
/// distinction is not cosmetic: `couldn't open file` names the call that did NOT
/// fail when a script opens and then cannot be read, which is exactly what a
/// DIRECTORY does. `read error on` is the string this program already prints for
/// an operand and for `r`/`R` on the very same errno.
#[derive(Debug)]
enum ScriptFailure {
    Open(std::io::Error),
    Read(std::io::Error),
    /// A read that failed on the STDIN script. Its own arm rather than a second
    /// `path == "-"` test beside the one `read_script` already made: the module
    /// that knows which stream it read is the one that says so.
    Stdin(std::io::Error),
}

impl ScriptFailure {
    /// `name` is the `-f` argument as spelled -- except that a READ failure on the
    /// stdin script names the STREAM. That is GNU's own machinery rather than a
    /// preference: `compile_file` binds `stdin` (compile.c:1538) for the name `-`
    /// alone, and `utils_fp_name` reports that stream as `stdin` -- so it is what
    /// GNU would print here if it inspected the failure at all.
    fn msg(&self, name: &[u8]) -> Vec<u8> {
        match self {
            Self::Open(e) => cant_open(name, e),
            Self::Read(e) => read_error(name, e),
            Self::Stdin(e) => read_error(b"stdin", e),
        }
    }
}

/// Every flag a COMPILE reads, as one value. GNU compiles each `-e`/`-f` inside
/// its own `getopt` loop, so each of these governs the parts scanned AFTER it and
/// no earlier one; a bare script operand is compiled once the scan is over and
/// takes the final values, as run time does. Threaded together because five
/// adjacent `bool`s of the same shape transpose silently, and built in exactly
/// one place so no later caller can default one.
///
/// `posix` and `extended` are deliberately not each other's negation: `posix` is
/// `--posix`, which withdraws extensions, while `extended` is GNU's
/// POSIXLY_EXTENDED, the level `--posix` and `POSIXLY_CORRECT` each leave. Two
/// things read it, and both through `extended_with_v`: the special-file table
/// while it compiles, and the run at end of input.
#[derive(Clone, Copy, Debug)]
struct Mode {
    posix: bool,
    extended: bool,
    /// `-E`/`-r`.
    ere: bool,
    /// `--sandbox`. Read at `r`/`R`/`w`/`W` and the `s///w` flag, BEFORE the
    /// target is opened, because refusing the command is GNU's answer whether or
    /// not the file could have been opened.
    sandbox: bool,
}

/// The two compile inputs that are properties of the INVOCATION rather than of
/// an `-e` part, which is what keeps them out of `Mode`: every part sees the
/// same pair, so a per-part answer could only ever be the same answer written
/// once per part. Threaded as a struct for `Mode`'s reason -- two adjacent
/// `bool` parameters of the same shape transpose silently.
///
/// `Conf` still carries `null_data`, since it is what the option loop WRITES;
/// this is what the COMPILE reads, built once beside the separator that shares
/// the bit. A handoff rather than a second source: nothing past `run` reads
/// `conf.null_data`.
#[derive(Clone, Copy, Debug)]
struct Invocation {
    /// `-z`, read by the `M` flag. GNU reads the record separator at compile
    /// time AND again in `match_regexp`, and for a pattern that is only `^` or
    /// `$` the RUN-time read is what anchors it -- so a part-scoped answer is
    /// wrong for exactly the patterns the option is most used with. See
    /// spec/README.
    null_data: bool,
    /// `POSIXLY_CORRECT` PRESENT in the environment, which is a THIRD thing
    /// that variable does and the only one that is not `posixicity`: GNU's
    /// `dfawarn` asks `getenv` directly (regexp.c:52), so the confusing-bracket
    /// lint is discarded whenever it is set -- under `--posix` too, and no `v`
    /// brings it back. Presence and not value, as the option-permutation half
    /// already is.
    posixly: bool,
}

impl Mode {
    /// GNU's `posixicity == POSIXLY_EXTENDED` once a compiled `v` is accounted
    /// for. Both are ASSIGNMENTS to one level, so this is not an OR: `v` sets
    /// EXTENDED (compile.c:1079) and `--posix` sets BASIC, and the flag wins
    /// wherever both appear. One function so the compile-time answer
    /// (`ScriptParser::mode`) and the run-time one cannot drift apart.
    fn extended_with_v(self, v_promoted: bool) -> bool {
        self.extended || (v_promoted && !self.posix)
    }
}

/// One of the three names GNU's `get_openfile` resolves to a STREAM the process
/// already holds instead of a path it opens (compile.c:81).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Special {
    In,
    Out,
    Err,
}

impl Special {
    /// What a diagnostic calls it, which is the STREAM's registered name and not
    /// the name the script spelled (GNU's `utils_fp_name`, utils.c:98).
    fn name(self) -> &'static str {
        match self {
            Self::In => "stdin",
            Self::Out => "stdout",
            Self::Err => "stderr",
        }
    }
}

/// ONE table serves both directions -- `R` on the read side, `w`/`W`/`s///w` on
/// the write side -- which is why each name carries a direction with it: the C
/// library's `stdin` is a read-only stream and its `stdout`/`stderr` are
/// write-only, so the wrong way round is EBADF decided in the library with no
/// syscall attempted. That is why `1<>f` does not make `R /dev/stdout` readable.
/// GNU consults the table only under POSIXLY_EXTENDED, so `--posix` opens all
/// three as the paths they are.
fn special(path: &[u8], extended: bool) -> Option<Special> {
    if !extended {
        return None;
    }
    match path {
        b"/dev/stdin" => Some(Special::In),
        b"/dev/stdout" => Some(Special::Out),
        b"/dev/stderr" => Some(Special::Err),
        _ => None,
    }
}

/// A `w`/`R` target as the COMPILE resolved it, which is what the command then
/// carries. Not the name alone, for two reasons that are really one. The table is
/// consulted under the posixicity of the PART naming the target, so re-asking it
/// at run time — where only the final flags survive — can answer differently for
/// the very command that asked. And two parts may name one path and mean
/// different things: GNU returns the special stream from a STATIC entry that sits
/// OUTSIDE the by-name list `get_openfile` dedups in (compile.c:398), so an
/// extended `/dev/stdout` and a `--posix` one are two targets, and this is the
/// key that keeps them apart.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct FileTarget {
    special: Option<Special>,
    path: Vec<u8>,
}

impl FileTarget {
    fn resolve(path: &[u8], extended: bool) -> Self {
        Self { special: special(path, extended), path: path.to_vec() }
    }

    /// What a diagnostic calls it: the STREAM's registered name for a special
    /// one, and the name the script spelled for everything else.
    fn name(&self) -> &[u8] {
        match self.special {
            Some(s) => s.name().as_bytes(),
            None => &self.path,
        }
    }
}

/// `EBADF`, which the C library sets ITSELF for a stream used the wrong way round.
/// Synthesised rather than obtained, because there is no operation to attempt: the
/// refusal is above the kernel, and safe Rust exposes no reader for standard
/// output to attempt it with.
const EBADF: i32 = 9;

fn ebadf() -> std::io::Error {
    std::io::Error::from_raw_os_error(EBADF)
}

/// The refusal a `w` to `/dev/stdin` earns. GNU's `ck_fwrite` (utils.c:224) counts
/// BYTES as items and pluralises. A zero-length write never reaches it: what
/// skips one is `output_line`'s own `if (length)` (execute.c:426), the `size`
/// argument being a literal 1 at every call — so an EMPTY pattern space is
/// refused by the separator that follows it instead, one item.
fn refuse_write(n: usize) -> Result<(), Vec<u8>> {
    if n == 0 {
        return Ok(());
    }
    let items = if n == 1 { "item" } else { "items" };
    Err(format!("couldn't write {n} {items} to stdin: {}", errmsg(&ebadf())).into_bytes())
}

/// `read error on NAME: WHY`, GNU's wording for a read that failed on something
/// already open (utils.c:240). The one builder for it: `-f` scripts, `r`/`R`
/// sources and the special streams all print this, and three spellings of one
/// string is three places to chase when it changes.
fn read_error(name: &[u8], e: &std::io::Error) -> Vec<u8> {
    crate::util::name_in("read error on ", name, &format!(": {}", errmsg(e)))
}

/// `R`'s source, which unlike `r`'s is resolved through the table above: GNU
/// compiles `R` with `get_openfile` and `r` with a bare `read_filename`, so only
/// `R` gets the aliasing. The non-extended arm is reached through
/// `POSIXLY_CORRECT` and not through `--posix`, which withdraws `R` itself — so
/// under the variable this opens the NAME again, hang and all, exactly as GNU
/// does there.
fn open_r_source(target: &FileTarget, separator: u8) -> Result<RFile, Vec<u8>> {
    let Some(s) = target.special else {
        return Ok(open_source(&target.path, separator));
    };
    let Special::In = s else {
        return Err(read_error(target.name(), &ebadf()));
    };
    // The descriptor we were GIVEN, rather than the name opened a second time.
    // Where standard input is a fifo whose writer has gone, fd 0 is at end of file
    // while a fresh open of the name waits for a writer that never comes.
    let mut data = Vec::new();
    match std::io::stdin().lock().read_to_end(&mut data) {
        // Never rewindable: GNU does not `rewind` a special stream under `-s`,
        // however seekable the descriptor behind it happens to be.
        Ok(_) => Ok(RFile {
            src: RSource::Whole { lines: to_lines(&data, separator), pos: 0 },
            rewindable: false,
        }),
        Err(e) => Err(read_error(s.name().as_bytes(), &e)),
    }
}

/// Open a file named by `R`, to be read a BLOCK at a time. Unlike an operand or
/// `-f`, `-` is NOT stdin here: GNU opens the name literally, so `R -` reads a file
/// called `-` and reads nothing when there is none (`w -` already wrote one).
///
/// An OPEN failure is not an error, which is what makes `R /nonexistent` silent; a
/// READ failure IS one, exit 4, and a DIRECTORY is how you get one -- on the FIRST
/// read now rather than at the open, which is where a directory fails anyway.
/// Rewindability is `rewind(3)`'s own test, asked of the handle before it is read
/// rather than after: a seek to 0 succeeds or fails on what the descriptor IS.
fn open_source(path: &[u8], separator: u8) -> RFile {
    let Ok(mut file) = std::fs::File::open(crate::util::path_from_bytes(path)) else {
        return RFile { src: RSource::Spent, rewindable: false };
    };
    let rewindable = file.seek(std::io::SeekFrom::Start(0)).is_ok();
    // A block whatever `-u` says: GNU's flag unbuffers the MAIN input stream, and
    // an `R` source it opens by name keeps its own buffering. The one `R` source
    // that shares the run's reader is `/dev/stdin`, which never reaches here.
    let src = RSource::Stream(Records::with_buffer(file, separator, Stream::BLOCK));
    RFile { src, rewindable }
}

/// Copy a file named by `r` to `sink`, a BLOCK at a time. `r` dumps the WHOLE of
/// it either way -- neither this nor GNU returns from `r /dev/zero` -- so what
/// streaming buys is not an answer but the MEMORY: GNU copies through a fixed
/// buffer and needs none for the source's size, where reading it whole first
/// needed all of it. Measured before this: 600 MB under a 700 MB address-space
/// limit was `read error on BIG: out of memory` at exit 4 here and exit 0 there.
///
/// The bytes read BEFORE a failed read are written, which is what copying as you
/// read means and what GNU does; reading whole discarded them. `put` rather than
/// `write` because the CALLER decides about the owed separator: `r` pays it before
/// the file's fate is known, and the immediate read deliberately never pays it.
///
/// An OPEN failure is not an error, which is what makes `r /nonexistent` silent; a
/// READ failure IS one, exit 4, and a DIRECTORY is how you get one.
///
/// A CLOSED reader ends the dump. `Out` swallows the EPIPE and latches instead, so
/// this is what stops `r /dev/zero | head` writing bytes nothing can receive --
/// GNU dies of SIGPIPE there, which a program the Rust runtime leaves ignoring it
/// cannot. A dump is unbounded WITHIN one cycle, which is why it asks here as well
/// as in `run_stream`; grep breaks its own loop on the same latch.
fn copy_source(path: &[u8], sink: &mut Sink) -> Result<(), Vec<u8>> {
    // Before the OPEN, because opening a FIFO blocks until a writer appears: a
    // queued `r` reached after the reader has gone would hang there rather than
    // on a read nothing wants.
    if sink.is_broken() {
        return Ok(());
    }
    let Ok(mut file) = std::fs::File::open(crate::util::path_from_bytes(path)) else {
        return Ok(());
    };
    // On the stack rather than allocated per call: `r` runs once per CYCLE.
    let mut buf = [0u8; Stream::BLOCK];
    loop {
        // And before each read, which is what ENDS an endless dump.
        if sink.is_broken() {
            return Ok(());
        }
        match file.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => sink.put(buf.get(..n).unwrap_or_default())?,
            // A signal arrived mid-call, not a read failure; `read_to_end` retried
            // it and so must this.
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(read_error(path, &e)),
        }
    }
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

/// Return what a `q` over-read from standard input, at the ONE point on each path
/// where the run is over and the stream still knows how far it got -- ahead of
/// every exit, so they cannot disagree about where descriptor 0 was left.
///
/// A failure here is fatal rather than swallowed, as every other runtime I/O
/// failure in this applet is. It says the offset was NOT restored, which is
/// otherwise invisible until some later program reads the wrong bytes; and with
/// the nothing-to-return case answered before any syscall is made, what is left
/// to fail is the descriptor itself.
fn give_back(fd0: Option<&mut Fd0>) -> Result<(), Fatal> {
    let Some(fd0) = fd0 else {
        return Ok(());
    };
    let Some(unread) = fd0.unconsumed() else {
        return Ok(());
    };
    // Through the reader's OWN duplicate of descriptor 0: it shares the file
    // description, so seeking it moves fd 0, and there is no second dup to fail
    // under descriptor pressure. `raw` is exactly the case where that duplicate
    // exists, which is why the `else` here cannot happen.
    let Src::Raw(file) = fd0.rec.source_mut() else {
        return Ok(());
    };
    crate::util::give_back_stdin(file, unread)
        .map_err(|e| Fatal::runtime(format!("can't reposition stdin: {}", errmsg(&e))))
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

    let (temp, mut file) = create_temp(&dir, path).map_err(Fatal::runtime_msg)?;
    let write = file
        .write_all(data)
        .and_then(|()| file.flush())
        .map_err(|e| crate::util::name_in("couldn't write ", path, &format!(": {}", errmsg(&e))));
    drop(file);
    let finish = write.and_then(|()| {
        if let Some(mode) = mode {
            std::fs::set_permissions(&temp, mode)
                .map_err(|e| crate::util::name_in("couldn't write ", path, &format!(": {}", errmsg(&e))))?;
        }
        let mut moved_aside = None;
        if !suffix.is_empty() {
            // The ORIGINAL is renamed aside, so the backup keeps its inode and
            // the new content lands at the original name — GNU's order.
            let backup = backup_name(&target, suffix);
            std::fs::rename(&target, &backup)
                .map_err(|e| crate::util::name_in("cannot rename ", path, &format!(": {}", errmsg(&e))))?;
            moved_aside = Some(backup);
        }
        std::fs::rename(&temp, &target).map_err(|e| {
            // The original is already at the backup name; without this the run
            // would fail with the edited path GONE, which is worse than either
            // outcome the caller asked for.
            if let Some(backup) = moved_aside {
                let _ = std::fs::rename(&backup, &target);
            }
            crate::util::name_in("cannot rename ", path, &format!(": {}", errmsg(&e)))
        })
    });
    if finish.is_err() {
        // Leaving the scratch file behind would litter the directory the caller
        // asked to edit.
        let _ = std::fs::remove_file(&temp);
    }
    finish.map_err(Fatal::runtime_msg)
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
fn create_temp(dir: &Path, path: &[u8]) -> Result<(PathBuf, std::fs::File), Vec<u8>> {
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
                return Err(crate::util::name_in(
                    "couldn't open temporary file ",
                    &crate::util::path_bytes(&candidate),
                    &format!(": {}", errmsg(&e)),
                ))
            }
        }
    }
    Err(crate::util::name_in("couldn't open temporary file for ", path, ""))
}

impl Sed {
    /// Run every cycle of one input stream. `Ok(Some(code))` means `q`/`Q` asked
    /// to exit with that status.
    fn run_stream(&mut self, stream: &mut Stream, sink: &mut Sink) -> Result<Option<i32>, Vec<u8>> {
        loop {
            // A CLOSED reader ends the run, as it already ends an `r` dump. `Out`
            // swallows the EPIPE and latches instead of failing, so without this
            // nothing stops a cycle that writes: `sed -z -n p /dev/zero | head`
            // never returned, where GNU takes SIGPIPE at once. Asked BEFORE the
            // read, so the input stops where the last written record left it and
            // `give_back` still owes the next reader an accurate position.
            //
            // Recorded as a QUIT rather than as this file's end, because those are
            // the same answer to the caller and must not be: under `-s` an end
            // means open the NEXT operand, so a broken sink went on to report a
            // missing one (exit 2) or to block on a FIFO nobody opens. `q`'s own
            // machinery already means end the whole run, and 0 is the status.
            if sink.is_broken() {
                self.quit = Some(0);
                return Ok(self.quit);
            }
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

    fn emit_pattern(&mut self, sink: &mut Sink) -> Result<(), Vec<u8>> {
        let pattern = std::mem::take(&mut self.pattern);
        let res = self.emit(sink, &pattern, self.terminated);
        self.pattern = pattern;
        res
    }

    fn emit(&self, sink: &mut Sink, bytes: &[u8], terminated: bool) -> Result<(), Vec<u8>> {
        sink.write_line(bytes, terminated)
    }

    /// Take `R`'s next line from `path` and queue the bytes. GNU reads it when the
    /// command runs, not when the queue is flushed, so `R` over a directory fails
    /// BEFORE the cycle prints while `r` over one fails after.
    fn queue_line(&mut self, target: &FileTarget, stream: &mut Stream) -> Result<(), Vec<u8>> {
        let separator = self.separator;
        // Standard input is not one of the sources this caches: it is the run's
        // own reader, held by the stream so an operand naming it and this share
        // one position. Nothing to rewind under `-s` either, which is what GNU
        // does -- the position carries across operands there.
        let line = if matches!(target.special, Some(Special::In)) {
            stream.stdin_record()?
        } else {
            let entry = match self.rfiles.get_mut(target) {
                Some(e) => e,
                None => {
                    let opened = open_r_source(target, separator)?;
                    self.rfiles.entry(target.clone()).or_insert(opened)
                }
            };
            entry.next(target.name())?
        };
        let Some(line) = line else {
            return Ok(());
        };
        // `R` writes the line as it found it and owes NOTHING of its own: over a
        // source with no final newline, `sed -n -e '1R f' -e '2p'` runs the two
        // together in GNU. It still PAYS a debt the pattern space left, which the
        // ordinary write does.
        let mut bytes = line.text;
        if line.terminated {
            bytes.push(separator);
        }
        self.appends.push(Append::Line(bytes));
        Ok(())
    }

    fn flush_appends(&mut self, sink: &mut Sink) -> Result<(), Vec<u8>> {
        let appends = std::mem::take(&mut self.appends);
        for a in appends {
            match a {
                // An `a` with NO TEXT still pays a separator the record left
                // owed, which is the whole of what queueing it does: the dump
                // terminates the pattern space without looking at whether
                // there is anything to dump, so `printf 'a\nb' | sed 'a\'`
                // ends with one where a plain unterminated last line does not.
                // Same rule as the missing `r` file below, from the other
                // direction. A text-less `i`/`c` is the OPPOSITE case and not
                // a positional one: they pay when they have text, and GNU's
                // `output_line` returns before paying when they have none.
                Append::Text(Some(text)) => sink.write_text(&text)?,
                Append::Text(None) => sink.queued(&[])?,
                // A missing file is not an error for `r`/`R`, as in GNU sed — but
                // GNU pays the owed separator BEFORE it finds out, so `printf x |
                // sed 'r /nonexistent'` still ends with one. Writing the empty
                // content pays it and adds nothing, which is what an existing but
                // empty file already did.
                Append::File(path) => {
                    // The owed separator is paid BEFORE the file's fate is known, so
                    // `printf x | sed 'r /nonexistent'` still ends with one -- and so
                    // does `r` over a directory, which then fails with exit 4.
                    sink.queued(&[])?;
                    copy_source(&path, sink)?;
                }
                // `R` resolved its line when the command RAN — see `queue_line`.
                Append::Line(bytes) => sink.queued(&bytes)?,
            }
        }
        // The queue is ONE flush, wherever `-u` left the sink: see `queued`.
        sink.end_line()
    }

    fn write_to_file(
        &mut self,
        target: &FileTarget,
        bytes: &[u8],
        terminated: bool,
        sink: &mut Sink,
    ) -> Result<(), Vec<u8>> {
        // `/dev/stdout` must share the auto-print stream's sink, or a `w` and a `p`
        // in one script interleave wrongly. Under `-i` that sink is the replacement
        // buffer, and GNU still writes to the real standard output — so there it is
        // an ordinary target with a debt of its own, like every other one.
        //
        // Asked of the target the COMPILE resolved rather than re-derived from the
        // final flags: the table is read under the posixicity of the part naming
        // it, so `-e 'w /dev/stdout' --posix` aliases and re-deriving here would
        // decide the opposite, giving one `w` two answers.
        if matches!(self.wfiles.get(target).map(|w| &w.dest), Some(WDest::Stdout(_))) && !self.in_place {
            return sink.write_line_on(Chan::WFile, bytes, terminated);
        }
        let separator = self.separator;
        // Every target was opened by the parser, so a miss is a command whose name
        // never reached `open_wfile` — not a filesystem failure, and saying so
        // would send the reader to the one place that is working.
        let unbuffered = self.unbuffered;
        let Some(w) = self.wfiles.get_mut(target) else {
            return Err(crate::util::name_in("no output was opened for ", &target.path, ""));
        };
        w.write_line(bytes, terminated, separator)?;
        // `-u`'s write half reaches here too: a `w` target is one of the outputs
        // GNU flushes after every line, which is what lets an `r` of the same file
        // see what this cycle wrote to it.
        match unbuffered {
            true => w.flush(),
            false => Ok(()),
        }
    }

    /// One pass over the script for the current pattern space.
    #[allow(clippy::too_many_lines)] // the command dispatch: one arm per sed command
    fn run_cycle(&mut self, stream: &mut Stream, sink: &mut Sink) -> Result<Flow, Vec<u8>> {
        let mut pc = 0usize;
        while pc < self.script.cmds.len() {
            // Here as well as in `run_stream`, because a script can loop WITHOUT
            // ending its cycle: `sed -n -e ':a' -e p -e 'b a'` branches inside this
            // one, so a check between cycles is a check it never reaches. Same
            // answer, same reason, and the cost is a bool per command executed.
            if sink.is_broken() {
                self.quit = Some(0);
                return Ok(Flow::Quit);
            }
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
                    return Err(crate::util::name_in("unresolved branch to `", n, "'"))
                }
                Some(Kind::Append(text)) => self.appends.push(Append::Text(text.clone())),
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
                    // Never more than the sink can BUFFER at once. GNU's
                    // `do_list` writes a BYTE at a time, so a listing never
                    // takes stdio's block path; submitted whole it would, and
                    // `l 0` -- which wraps nowhere -- would make that every
                    // listing. Any piece that cannot overflow the buffer gives
                    // the same boundaries as a byte does, at four thousandths of
                    // the calls.
                    for chunk in buf.chunks(sink.piece()) {
                        sink.write(chunk)?;
                    }
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
                    // take already flush, in that order. (`n` never had the
                    // problem.) Dropping it instead is what BOTH non-default
                    // posixicities do, so the test is `extended` and not `!posix`
                    // -- execute.c:1478 asks `== POSIXLY_EXTENDED`. `-n` needs no
                    // test here: GNU's `!no_default_output` is this crate's
                    // ordinary end-of-cycle print, which `suppress` already
                    // governs on the flow below.
                    let Some(line) = stream.next_line(Opener::Lookahead) else {
                        if !self.extended {
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
                    // previous line still owes one AFTER the prepended bytes. It
                    // does not flush under `-u` either (execute.c:1520 has no
                    // `flush_output`), so these bytes ride the next flush point.
                    copy_source(path, sink)?;
                }
                Some(Kind::ReadLine(path)) => {
                    let path = path.clone();
                    self.queue_line(&path, stream)?;
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
                        if let Some(target) = wfile {
                            let pattern = self.pattern.clone();
                            let terminated = self.terminated;
                            self.write_to_file(&target, &pattern, terminated, sink)?;
                        }
                    }
                }
            }
            pc = next;
        }
        Ok(Flow::Done)
    }

    /// Apply the `s///` at `idx` to the pattern space.
    fn substitute(&mut self, idx: usize) -> Result<SubstOutcome, Vec<u8>> {
        let (global, print, occurrence, wfile, own_re) = {
            let Some(Cmd { kind: Kind::Subst(s), .. }) = self.script.cmds.get(idx) else {
                return Ok((false, false, None));
            };
            (s.global, s.print, s.occurrence, s.wfile.clone(), s.re)
        };
        let re_idx = match own_re.or(self.last_regex) {
            Some(i) => i,
            None => return Err(NO_PREVIOUS_REGEX.as_bytes().to_vec()),
        };
        self.last_regex = Some(re_idx);
        let Some(re) = self.script.regexes.get(re_idx) else {
            return Err(NO_PREVIOUS_REGEX.as_bytes().to_vec());
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
                    return Err(e.msg.into_bytes());
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

    /// What every test here compiles under: no `--posix`, no `POSIXLY_CORRECT`,
    /// and none of the three compile flags a part can carry.
    const EXTENDED: Mode =
        Mode { posix: false, extended: true, ere: false, sandbox: false };

    /// `EXTENDED` with the one flag a test varies.
    fn mode_with(ere: bool) -> Mode {
        Mode { ere, ..EXTENDED }
    }

    /// What every test here compiles under: no `-z`, no `POSIXLY_CORRECT`.
    const PLAIN: Invocation = Invocation { null_data: false, posixly: false };

    /// `Options` carries the two posixicity rules as separate bools because GNU
    /// separates them, but they are LEVELS and not axes: BASIC is below CORRECT,
    /// so `--posix` implies the paren rule and the pair `(posix, extended) =
    /// (true, true)` must never be built. Nothing in the type says so. This
    /// covers the two `Mode` producers and NOT the `Options` construction that
    /// reads them -- a site spelling the pair by hand is the corpus's to catch,
    /// which it does.
    #[test]
    fn the_paren_rule_is_implied_by_the_extension_rule() {
        for posix in [false, true] {
            for posixly in [false, true] {
                let conf = Conf {
                    suppress: false,
                    sandbox: false,
                    ere: false,
                    separate: false,
                    null_data: false,
                    in_place: None,
                    posix,
                    unbuffered: false,
                    line_wrap: DEFAULT_LINE_WRAP,
                };
                let mode = mode_of(&conf, posixly);
                assert!(!(mode.posix && mode.extended), "mode_of({posix}, {posixly})");
                // And through the parser's own answer, which a compiled `v` moves.
                for v_promoted in [false, true] {
                    assert!(
                        !(mode.posix && mode.extended_with_v(v_promoted)),
                        "extended_with_v({posix}, {posixly}, {v_promoted})"
                    );
                }
            }
        }
    }

    /// A `Stream` over bytes. `Stream` reads a DESCRIPTOR now, so the bytes go
    /// through a pipe rather than an in-memory arm of `Src`: a test-only variant
    /// would be a second test-gated item in this file, and the confinement scan
    /// strips the test half by truncating at the first one.
    fn stream_over(data: &[u8], separator: u8) -> Stream {
        // The whole input has to FIT, since nothing drains the pipe until the
        // stream does; every caller here passes a literal, so this is a guard
        // against a future one rather than a limit anybody meets.
        assert!(data.len() < 60_000, "test input too large for one pipe");
        let (reader, mut writer) = std::io::pipe().unwrap();
        writer.write_all(data).unwrap();
        drop(writer);
        let file = std::fs::File::from(std::os::fd::OwnedFd::from(reader));
        Stream::over(b"-", Input::File(file), separator, Stream::BLOCK, None)
    }

    /// What a run LEAVES on a shared pipe is the whole of `-u`'s read half, and it
    /// is invisible to the corpus harness: seeing it needs a second reader on the
    /// same pipe, which a `## argv:` case has no way to hold. A pipe has one
    /// buffer, so a clone of the read end reads exactly what the stream did not.
    fn left_on_the_pipe(block: usize, data: &[u8]) -> usize {
        let (reader, mut writer) = std::io::pipe().unwrap();
        writer.write_all(data).unwrap();
        drop(writer);
        let rest = reader.try_clone().unwrap();
        let file = std::fs::File::from(std::os::fd::OwnedFd::from(reader));
        let mut stream = Stream::over(b"-", Input::File(file), b'\n', block, None);
        let line = stream.next_line(Opener::Reader).unwrap();
        assert_eq!(line.text, b"1234567", "one record, whatever the block");
        drop(stream);
        let mut tail = Vec::new();
        let mut rest = std::fs::File::from(std::os::fd::OwnedFd::from(rest));
        rest.read_to_end(&mut tail).unwrap();
        tail.len()
    }

    /// `-u` reads a RECORD at a time, which is what lets the next program on a
    /// shared descriptor have the rest: of 4104 bytes GNU leaves 8 without the
    /// flag and 4096 with it -- one 8-byte line DELIVERED either way, and a
    /// 4096-byte buffer taken only without.
    #[test]
    fn unbuffered_takes_one_record_off_a_shared_pipe() {
        let data = b"1234567\n".repeat(513);
        assert_eq!(data.len(), 4104);
        assert_eq!(left_on_the_pipe(1, &data), 4096, "-u leaves all but the record");
        assert_eq!(left_on_the_pipe(Stream::BLOCK, &data), 8, "buffered takes a block");
    }

    /// Run a script over `input` and return what it wrote.
    fn sed(script: &str, input: &str, opts: &[&str]) -> Vec<u8> {
        let ere = opts.contains(&"-E") || opts.contains(&"-r");
        let null_data = opts.contains(&"-z");
        let sep = separator_for(null_data);
        let mode = mode_with(ere);
        let inv = Invocation { null_data, posixly: false };
        let mut script = compile_script(script.as_bytes(), Vec::new(), mode, inv).unwrap();
        let seed = seed_ranges(&script.cmds);
        let script_extended = mode.extended_with_v(script.v_promoted);
        let wfiles = std::mem::take(&mut script.wfiles);
        let mut sed = Sed {
            script,
            ranges: seed,
            suppress: opts.contains(&"-n"),
            separator: sep,
            extended: script_extended,
            line_wrap: DEFAULT_LINE_WRAP,
            pattern: Vec::new(),
            hold: Vec::new(),
            hold_terminated: true,
            line_number: 0,
            in_place: false,
            unbuffered: false,
            terminated: true,
            appends: Vec::new(),
            replaced: false,
            quit: None,
            last_regex: None,
            wfiles,
            rfiles: BTreeMap::new(),
            };
        let mut stream = stream_over(input.as_bytes(), sep);
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
            let err = compile_script(script, Vec::new(), EXTENDED, PLAIN).err();
            assert_eq!(
                err.map(|f| f.msg),
                Some(msg.as_bytes().to_vec()),
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
            compile_script(b"s/[[....]]/X/", Vec::new(), EXTENDED, PLAIN)
                .err()
                .map(|f| f.msg),
            Some(b"Invalid collation character".to_vec())
        );
        for script in [&b"s/[[...]]/X/"[..], b"s/[[.....]]/X/", b"s/[[:::]]/X/", b"s/[[===]]/X/"] {
            let err = compile_script(script, Vec::new(), EXTENDED, PLAIN).err();
            assert_eq!(
                err.map(|f| f.msg),
                Some(b"unterminated `s' command".to_vec()),
                "{:?}",
                String::from_utf8_lossy(script)
            );
        }
        // The closer must be the character that opened it, so these run off the
        // end of the script rather than closing a set.
        for script in [&b"s/[[:alpha:]/X/"[..], b"s/[[:alpha.]]/X/", b"s/[[:]]/X/"] {
            let err = compile_script(script, Vec::new(), EXTENDED, PLAIN).err();
            assert_eq!(
                err.map(|f| f.msg),
                Some(b"unterminated `s' command".to_vec()),
                "{:?}",
                String::from_utf8_lossy(script)
            );
        }
    }

    /// GNU reports the bare-class-syntax refusal without an expression prefix and
    /// exits 4, alone among pattern errors.
    #[test]
    fn the_bare_class_syntax_refusal_is_exit_4_where_other_pattern_errors_are_1() {
        let f = compile_script(b"s@[:alpha:]@X@", Vec::new(), EXTENDED, PLAIN).err();
        assert_eq!(f.as_ref().map(|f| f.status), Some(4));
        assert_eq!(f.map(|f| f.msg), Some(crate::regex::CLASS_SYNTAX.as_bytes().to_vec()));
        let f = compile_script(b"s@[[:a:]]@X@", Vec::new(), EXTENDED, PLAIN).err();
        assert_eq!(f.map(|f| f.status), Some(1));
    }

    #[test]
    fn an_unknown_command_is_a_diagnosed_error() {
        // A bad script is status 1; an unresolvable branch is a RUNTIME error,
        // which GNU reports as 4.
        for bad in [&b"k"[..], b"s/a/b", b"{p"] {
            let err = compile_script(bad, Vec::new(), EXTENDED, PLAIN).err().map(|f| f.status);
            assert_eq!(err, Some(1), "{:?} must be a status-1 script error", bad);
        }
        assert_eq!(
            compile_script(b"bnowhere", Vec::new(), EXTENDED, PLAIN).err().map(|f| f.status),
            Some(4)
        );
    }

    /// `a`/`i`/`c` need text where the script ENDS, and an `-e` part ends there for
    /// that question only -- the same bytes as one argument are legal.
    #[test]
    fn a_text_command_needs_text_only_where_its_part_ends() {
        // Ends of the `-e` parts, as `run` builds them: each part's last byte + 1,
        // the last part's end being the whole script.
        let compile = |src: &[u8], ends: Vec<usize>| {
            let mut parts: Vec<Part> = ends
                .iter()
                .enumerate()
                .map(|(i, e)| Part { end: *e, origin: Origin::Expression(i + 1), mode: EXTENDED })
                .collect();
            parts.push(Part { end: src.len(), origin: Origin::Expression(parts.len() + 1), mode: EXTENDED });
            compile_script(src, parts, EXTENDED, PLAIN).err().map(|f| f.status)
        };
        // `sed a` and `sed -e a -e p`: nothing after the command in its own part.
        assert_eq!(compile(b"a", Vec::new()), Some(1));
        assert_eq!(compile(b"a\np", vec![1]), Some(1));
        // The same bytes as ONE argument are an empty text and a following command.
        assert_eq!(compile(b"a\np", Vec::new()), None);
        // A backslash asks for the next line, so it crosses the boundary.
        assert_eq!(compile(b"a\\\ntext", vec![2]), None);
    }

    /// EVERY position of a mixed script, not a few interesting ones: which part
    /// owns a position is a partition, and a partition tested from one side only
    /// is how an off-by-one at a boundary survives. Both boundaries here are the
    /// case that matters — a position AT a part's end has consumed that part and
    /// none of the next, so it is still the earlier part's.
    #[test]
    fn every_position_of_a_joined_script_names_the_part_it_fell_in() {
        // `sed -e p -f s.sed -e d` with `s.sed` holding `q\nZ`.
        let src = b"p\nq\nZ\nd";
        let parts = vec![
            Part { end: 1, origin: Origin::Expression(1), mode: EXTENDED },
            Part { end: 5, origin: Origin::File(b"s.sed".to_vec()), mode: EXTENDED },
            Part { end: 7, origin: Origin::Expression(2), mode: EXTENDED },
        ];
        let want = [
            "-e expression #1, char 0", // 0: `p'
            "-e expression #1, char 1", // 1: consumed `p', nothing of the file
            "file s.sed line 1",        // 2: `q'
            "file s.sed line 1",        // 3: consumed `q'
            "file s.sed line 2",        // 4: `Z'
            "file s.sed line 2",        // 5: consumed `Z', nothing of the last part
            "-e expression #2, char 0", // 6: `d'
            "-e expression #2, char 1", // 7: consumed the whole script
        ];
        for (pos, w) in want.iter().enumerate() {
            assert_eq!(locus_at(&parts, src, Spot::Read(pos)).as_deref(), Some(w.as_bytes()), "at {pos}");
        }
        // A SAVED location picks its part the same way and always reads char 0,
        // which is what GNU prints for one -- so the two spots differ only where
        // an `-e` part carries a count at all.
        assert_eq!(
            locus_at(&parts, src, Spot::Saved(1)).as_deref(),
            Some(&b"-e expression #1, char 0"[..])
        );
        assert_eq!(
            locus_at(&parts, src, Spot::Saved(7)).as_deref(),
            Some(&b"-e expression #2, char 0"[..])
        );
        assert_eq!(
            locus_at(&parts, src, Spot::Saved(4)).as_deref(),
            Some(&b"file s.sed line 2"[..])
        );
        // The NAME goes out as GNU's `%s` writes it. No case can say so: a
        // corpus file's name is read as text, so a non-UTF-8 one cannot be
        // asked for -- and a name that IS UTF-8 renders alike either way.
        let raw = vec![Part { end: 1, origin: Origin::File(b"h\xffi.sed".to_vec()), mode: EXTENDED }];
        assert_eq!(
            locus_at(&raw, b"Z", Spot::Read(0)).as_deref(),
            Some(&b"file h\xffi.sed line 1"[..])
        );
        assert_eq!(locus_at(&[], src, Spot::Read(0)), None);
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

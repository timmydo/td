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
use std::path::{Path, PathBuf};

use crate::regex::{Captures, Options, Regex};
use crate::util::{errmsg, number, print_line, read_input, records, show, Out, VERSION};

const USAGE: &str = "usage: sed [-nrEsz] [-i[SUFFIX]] [-e SCRIPT] [-f FILE] [SCRIPT] [FILE]...";

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
    List(usize),
    Next,
    NextAppend,
    Print,
    PrintFirstLine,
    Quit(i32, bool), // (exit status, auto-print first)
    ReadFile(Vec<u8>),
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
    /// `-z`. Only the `M` flag reads it: see `Options::reg_newline`.
    null_data: bool,
    /// Offsets of the newlines that JOIN `-e`/`-f` parts. GNU ends a script part
    /// like it ends the script for one question only -- whether `a`/`i`/`c` was
    /// given any text -- so `sed -e a -e p` is an error where the one-argument
    /// `sed 'a<newline>p'` is not, while `sed -e 'a\' -e text` still spans the
    /// boundary because the backslash asked to. See `parse_text`.
    part_ends: Vec<usize>,
    regexes: Vec<Regex>,
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

    fn skip_separators(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b';')) {
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
            n = n.saturating_mul(10).saturating_add(u64::from(b - b'0'));
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
            reg_newline: multiline.then_some(separator_for(self.null_data)),
        };
        let re = Regex::compile(&normalize_regex(raw)?, opts)
            .map_err(|e| e.msg)?;
        self.regexes.push(re);
        Ok(Some(self.regexes.len() - 1))
    }

    /// A `/re/` or `\cREc` address, consuming the `I`/`M` flags that follow.
    fn parse_regex_addr(&mut self, delim: u8) -> Result<AddrKind, String> {
        let raw = self.read_delimited(delim, "unterminated address regex")?;
        let mut icase = false;
        let mut multiline = false;
        loop {
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
            Some(b) if b.is_ascii_digit() => {
                let n = self.parse_number().unwrap_or(0);
                if self.eat(b'~') {
                    let step = self
                        .parse_number()
                        .ok_or_else(|| "expected a number after `~'".to_string())?;
                    return Ok(Some(AddrKind::Step { first: n, step }));
                }
                if n == 0 {
                    return Ok(Some(AddrKind::Zero));
                }
                Ok(Some(AddrKind::Line(n)))
            }
            _ => Ok(None),
        }
    }

    fn parse_addr(&mut self) -> Result<Addr, String> {
        let mut addr = Addr { a1: self.parse_addr_kind()?, ..Addr::default() };
        if addr.a1.is_some() {
            self.skip_blank();
            if self.eat(b',') {
                self.skip_blank();
                if self.eat(b'+') {
                    let n = self
                        .parse_number()
                        .ok_or_else(|| "expected a number after `+'".to_string())?;
                    addr.a2 = Some(Addr2::Plus(n));
                } else if self.eat(b'~') {
                    let n = self
                        .parse_number()
                        .ok_or_else(|| "expected a number after `~'".to_string())?;
                    addr.a2 = Some(Addr2::Multiple(n));
                } else {
                    let k = self
                        .parse_addr_kind()?
                        .ok_or_else(|| "unexpected `,'".to_string())?;
                    addr.a2 = Some(Addr2::Kind(k));
                }
            }
        }
        self.skip_blank();
        while self.eat(b'!') {
            addr.negate = !addr.negate;
            self.skip_blank();
        }
        if matches!(addr.a1, Some(AddrKind::Zero)) && !addr.is_range() {
            return Err("invalid usage of line address 0".to_string());
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
            if !self.eat(b'\n') && self.peek().is_none() {
                return Ok(None);
            }
        } else if self.peek().is_none() || self.at_part_end() {
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
                    self.pos += 1;
                    out.push(b'\n');
                }
                Some(b'\\') => match self.bump() {
                    None => {
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

    fn parse_subst(&mut self) -> Result<Subst, String> {
        let delim = self.bump().ok_or_else(|| "unterminated `s' command".to_string())?;
        // A backslash CAN delimit (`s\a\b\` is `s/a/b/`); a newline cannot, and
        // GNU says so before reading anything else.
        if delim == b'\n' {
            return Err("unterminated `s' command".to_string());
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
                        return Err("multiple `g' options to `s' command".to_string());
                    }
                    global = true;
                    self.pos += 1;
                }
                Some(b'p') => {
                    if print {
                        return Err("multiple `p' options to `s' command".to_string());
                    }
                    print = true;
                    self.pos += 1;
                }
                Some(b'i' | b'I') => {
                    icase = true;
                    self.pos += 1;
                }
                Some(b'm' | b'M') => {
                    multiline = true;
                    self.pos += 1;
                }
                Some(b'e') => return Err("the `e' flag is not supported".to_string()),
                Some(b'w') => {
                    self.pos += 1;
                    wfile = Some(self.parse_filename()?);
                    break;
                }
                Some(b) if b.is_ascii_digit() => {
                    if occurrence != 0 {
                        return Err("multiple number options to `s' command".to_string());
                    }
                    occurrence = self.parse_number().unwrap_or(0);
                    if occurrence == 0 {
                        return Err("number option to `s' command may not be zero".to_string());
                    }
                }
                // A blank SEPARATES flags rather than ending them (`s/a/b/ g p` is
                // both), and anything that is not a flag or a command terminator is
                // an error rather than the next command: `s/a/b/x` is `unknown option
                // to `s'` in GNU, not a substitution followed by an exchange.
                Some(b' ' | b'\t') => self.pos += 1,
                None | Some(b'\n' | b';' | b'}' | b'#') => break,
                Some(_) => return Err("unknown option to `s'".to_string()),
            }
        }
        let re = self.add_regex(&pattern, icase, multiline)?;
        let replacement = compile_replacement(&raw_repl)?;
        // A `\N` naming a group the pattern does not have is a script error, not
        // an empty expansion. `s//.../` reuses the LAST regex, unknowable here,
        // so it is checked at run time instead (GNU does the same).
        if let Some(groups) = re.and_then(|i| self.regexes.get(i)).map(Regex::group_count) {
            if let Some(n) = max_group(&replacement) {
                if n > groups {
                    return Err(format!("invalid reference \\{n} on `s' command's RHS"));
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
fn normalize_regex(raw: &[u8]) -> Result<Vec<u8>, String> {
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
fn compile_replacement(raw: &[u8]) -> Result<Vec<Repl>, String> {
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
                    b'U' | b'L' | b'u' | b'l' | b'E' => {
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

/// Parse a whole script: a flat command list, its regex table, and its labels.
fn parse_script(
    src: &[u8],
    ere: bool,
    null_data: bool,
    part_ends: Vec<usize>,
) -> Result<Script, String> {
    let mut p = ScriptParser { src, pos: 0, ere, null_data, part_ends, regexes: Vec::new() };
    let mut cmds: Vec<Cmd> = Vec::new();
    let mut labels: BTreeMap<Vec<u8>, usize> = BTreeMap::new();
    let mut open_blocks: Vec<usize> = Vec::new();
    loop {
        p.skip_separators();
        if p.peek().is_none() {
            break;
        }
        let addr = p.parse_addr()?;
        let Some(c) = p.bump() else {
            return Err("missing command".to_string());
        };
        let kind = match c {
            b'{' => {
                open_blocks.push(cmds.len());
                Kind::Block(0) // patched when the matching `}` is seen
            }
            b'}' => {
                let Some(open) = open_blocks.pop() else {
                    return Err("unexpected `}'".to_string());
                };
                let after = cmds.len() + 1;
                if let Some(cmd) = cmds.get_mut(open) {
                    cmd.kind = Kind::Block(after);
                }
                Kind::BlockEnd
            }
            b'#' => {
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
                // The only two address restrictions GNU enforces at compile
                // time: `:` takes none, `q`/`Q` take one.
                if addr.a1.is_some() {
                    return Err(": doesn't want any addresses".to_string());
                }
                let name = p.parse_label();
                if name.is_empty() {
                    return Err("\":\" lacks a label".to_string());
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
                Kind::List(usize::try_from(p.parse_number().unwrap_or(70)).unwrap_or(70))
            }
            b'n' => Kind::Next,
            b'N' => Kind::NextAppend,
            b'p' => Kind::Print,
            b'P' => Kind::PrintFirstLine,
            b'q' | b'Q' => {
                if addr.is_range() {
                    return Err("command only uses one address".to_string());
                }
                p.skip_blank();
                let code = i32::try_from(p.parse_number().unwrap_or(0)).unwrap_or(0);
                Kind::Quit(code, c == b'q')
            }
            b'r' => Kind::ReadFile(p.parse_filename()?),
            b'R' => Kind::ReadLine(p.parse_filename()?),
            b'w' => Kind::Write(p.parse_filename()?),
            b'W' => Kind::WriteFirstLine(p.parse_filename()?),
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
                    return Err("expected newer version of sed".to_string());
                }
                Kind::Comment
            }
            b'e' => return Err("the `e' command is not supported".to_string()),
            other => return Err(format!("unknown command: `{}'", char::from(other))),
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
            | Kind::ReadLine(_)
            | Kind::Write(_)
            | Kind::WriteFirstLine(_)
            | Kind::Subst(_) => {}
            _ => p.end_of_cmd()?,
        }
        cmds.push(Cmd { addr, kind });
    }
    if !open_blocks.is_empty() {
        return Err("unmatched `{'".to_string());
    }
    Ok(Script { cmds, regexes: p.regexes, labels })
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

struct Stream {
    /// (index of that file's first line, name). Without `-s` every operand is
    /// concatenated into ONE stream, and `F` must still name the right file.
    names: Vec<(usize, Vec<u8>)>,
    lines: Vec<Line>,
    pos: usize,
}


impl Stream {
    /// The name of the file line `idx` came from.
    fn name_at(&self, idx: usize) -> &[u8] {
        let mut name: &[u8] = b"-";
        for (start, n) in &self.names {
            if *start > idx {
                break;
            }
            name = n;
        }
        name
    }

    fn next_line(&mut self) -> Option<Line> {
        let line = self.lines.get(self.pos)?;
        let out = Line { text: line.text.clone(), terminated: line.terminated };
        self.pos += 1;
        Some(out)
    }

    /// No further input line, so the line just read was `$`.
    fn at_last(&self) -> bool {
        self.pos >= self.lines.len()
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
struct WFile {
    dest: WDest,
    owed: bool,
}

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
    file_name: Vec<u8>,
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
    start.saturating_add(n).saturating_sub(start % n)
}

fn kind_matches(
    kind: &AddrKind,
    regexes: &[Regex],
    last_regex: Option<usize>,
    pattern: &[u8],
    line_number: u64,
    at_last: bool,
) -> Result<(bool, Option<usize>), String> {
    match kind {
        AddrKind::Line(n) => Ok((line_number == *n, None)),
        AddrKind::Zero => Ok((false, None)),
        AddrKind::Last => Ok((at_last, None)),
        AddrKind::Step { first, step } => {
            if *step == 0 {
                return Ok((line_number == *first, None));
            }
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
    fn addr_matches(&mut self, idx: usize, at_last: bool) -> Result<bool, String> {
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
            let hit = self.match_a1(idx, at_last)?;
            return Ok(hit != negate);
        }
        let a1_line = match cmd.addr.a1 {
            Some(AddrKind::Line(n)) => Some(n),
            _ => None,
        };
        let state = self.ranges.get(idx).copied().unwrap_or_default();

        let hit = if let RangeState::Active(start) = state {
            let (ends, select) = self.range_step(idx, start, at_last)?;
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
                None => self.match_a1(idx, at_last)?,
            };
            if !starts {
                false
            } else {
                let start = self.line_number;
                let (ends, select) = self.range_opens(idx, start, a1_line, at_last)?;
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
        at_last: bool,
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
            // file must CLOSE on that line, or `c` never prints. Only a REGEX
            // end gets GNU's "at least two lines" rule.
            Some(Some(Addr2::Plus(_) | Addr2::Multiple(_) | Addr2::Kind(AddrKind::Last))) => {
                let (ends, _) = self.range_step(idx, start, at_last)?;
                Ok((ends, true))
            }
            _ => Ok((false, true)),
        }
    }

    /// A line INSIDE a running range: does it end here, and is it selected?
    fn range_step(&mut self, idx: usize, start: u64, at_last: bool) -> Result<(bool, bool), String> {
        let line = self.line_number;
        let kind = match self.script.cmds.get(idx).map(|c| &c.addr.a2) {
            Some(Some(Addr2::Plus(n))) => return Ok((line >= start.saturating_add(*n), true)),
            Some(Some(Addr2::Multiple(n))) => return Ok((line >= multiple_end(start, *n), true)),
            // Past the end line the range closes WITHOUT selecting: reachable
            // when `b`/`t`/`N` jumped over the end.
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
            at_last,
        )?;
        if let Some(i) = used {
            self.last_regex = Some(i);
        }
        Ok((hit, true))
    }

    fn match_a1(&mut self, idx: usize, at_last: bool) -> Result<bool, String> {
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
            at_last,
        )?;
        if let Some(i) = used {
            self.last_regex = Some(i);
        }
        Ok(hit)
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
        Self { msg, status: 1 }
    }
}

/// The one error raised WHILE RUNNING that GNU still classifies as a bad SCRIPT:
/// an empty `s//…/` or `//p` can only be checked once the previous regex is known,
/// but it is a mistake in the program, not a refusal by the filesystem — exit 1,
/// with the `-e expression #N' prefix. One constant, named at the sites that raise
/// it and at the boundary that classifies it, so the two cannot drift apart.
const NO_PREVIOUS_REGEX: &str = "no previous regular expression";

impl Fatal {
    /// A failure surfaced while RUNNING the script — a `w` file that will not open,
    /// a write that fails. Exit 4, and no `-e expression #N' prefix: the script
    /// compiled, the filesystem refused. `From<String>` stamps 1 for the COMPILE
    /// errors `parse_script` raises, so every runtime `String` has to come through
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
) -> Result<Script, Fatal> {
    let mut script = parse_script(src, ere, null_data, part_ends)?;
    if sandbox {
        check_sandbox(&script.cmds)?;
    }
    let labels = std::mem::take(&mut script.labels);
    resolve_labels(&mut script.cmds, &labels).map_err(|msg| Fatal { msg, status: 4 })?;
    Ok(script)
}

/// `--sandbox` forbids every command that reads, writes, or executes outside the
/// stream. `e` is rejected by the parser regardless, so it cannot appear here.
fn check_sandbox(cmds: &[Cmd]) -> Result<(), Fatal> {
    let banned = cmds.iter().any(|c| {
        matches!(
            c.kind,
            Kind::ReadFile(_) | Kind::ReadLine(_) | Kind::Write(_) | Kind::WriteFirstLine(_)
        ) || matches!(&c.kind, Kind::Subst(s) if s.wfile.is_some())
    });
    if banned {
        return Err(Fatal {
            msg: "e/r/w commands disabled in sandbox mode".to_string(),
            status: 1,
        });
    }
    Ok(())
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

#[allow(clippy::too_many_lines)] // one option table; splitting it would only hide it
fn run(args: &[Vec<u8>]) -> Result<i32, Fatal> {
    let mut conf = Conf {
        suppress: false,
        sandbox: false,
        ere: false,
        separate: false,
        null_data: false,
        in_place: None,
        posix: false,
    };
    let mut script_parts: Vec<Vec<u8>> = Vec::new();
    let mut script_given = false;
    let mut operands: Vec<Vec<u8>> = Vec::new();
    let mut no_more_options = false;

    let mut i = 1usize;
    while let Some(arg) = args.get(i) {
        i += 1;
        if no_more_options || arg.first() != Some(&b'-') || arg.len() == 1 {
            operands.push(arg.clone());
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
                b"expression" | b"file" => {
                    let value = match inline {
                        Some(v) => v,
                        None => {
                            let v = args.get(i).cloned();
                            i += 1;
                            v.ok_or_else(|| {
                                format!("option '--{}' requires an argument", show(name))
                            })?
                        }
                    };
                    if name == b"file" {
                        // Same failure as short `-f`, so the same status and the same
                        // wording: the script did not fail to COMPILE, the file failed
                        // to open.
                        script_parts.push(read_input(&value).map_err(|e| Fatal {
                            msg: format!("couldn't open file {}: {}", show(&value), errmsg(&e)),
                            status: 4,
                        })?);
                    } else {
                        script_parts.push(value);
                    }
                    script_given = true;
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
                b'e' => {
                    let v = value_of(&mut j, &mut i)
                        .ok_or_else(|| "option requires an argument -- 'e'".to_string())?;
                    script_parts.push(v);
                    script_given = true;
                }
                b'f' => {
                    let v = value_of(&mut j, &mut i)
                        .ok_or_else(|| "option requires an argument -- 'f'".to_string())?;
                    // An unreadable SCRIPT file is exit 4, like any other
                    // runtime failure — not 1, which means a bad script.
                    let text = read_input(&v).map_err(|e| Fatal {
                        msg: format!("couldn't open file {}: {}", show(&v), errmsg(&e)),
                        status: 4,
                    })?;
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
    // `#n` on the very first line is POSIX's in-script spelling of -n.
    if source.starts_with(b"#n") && matches!(source.get(2), None | Some(b'\n')) {
        conf.suppress = true;
    }

    let script = compile_script(&source, conf.ere, conf.sandbox, conf.null_data, part_ends)?;
    let separator = separator_for(conf.null_data);
    let seed = seed_ranges(&script.cmds);
    let mut sed = Sed {
        script,
        ranges: seed,
        suppress: conf.suppress,
        separator,
        posix: conf.posix,
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
        wfiles: BTreeMap::new(),
        rfiles: BTreeMap::new(),
        file_name: b"-".to_vec(),
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
            let read = match &conf.in_place {
                Some(_) => std::fs::read(crate::util::path_from_bytes(path)),
                None => read_input(path),
            };
            let data = match read {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("sed: can't read {}: {}", show(path), errmsg(&e));
                    status = 2;
                    continue;
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
                // second operand is ever opened. Only this path may consult `status`,
                // since it opens one operand at a time; the branch below reads every
                // operand BEFORE the first cycle to resolve `$`, so its `status` can
                // describe a file GNU would have quit before opening.
                return Ok(match status {
                    0 => code,
                    _ => status,
                });
            }
        }
    } else {
        // One logical stream: line numbers and `$` span every input.
        let mut lines: Vec<Line> = Vec::new();
        let mut names: Vec<(usize, Vec<u8>)> = Vec::new();
        for path in &inputs {
            match read_input(path) {
                Ok(d) => {
                    names.push((lines.len(), path.clone()));
                    lines.extend(to_lines(&d, separator));
                }
                Err(e) => {
                    eprintln!("sed: can't read {}: {}", show(path), errmsg(&e));
                    status = 2;
                }
            }
        }
        let mut stream = Stream { names, lines, pos: 0 };
        let mut sink = Sink::stdout(&mut out, separator);
        let quit = sed.run_stream(&mut stream, &mut sink).map_err(Fatal::runtime)?;
        if let Some(code) = quit {
            out.flush().map_err(|e| Fatal::runtime(format!("write error: {}", errmsg(&e))))?;
            return Ok(code);
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
    Stream { names: vec![(0, path.to_vec())], lines: to_lines(data, separator), pos: 0 }
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
            self.file_name = stream.name_at(stream.pos).to_vec();
            let Some(line) = stream.next_line() else {
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
        if !self.wfiles.contains_key(path) {
            let dest = if path == b"/dev/stdout" {
                WDest::Stdout
            } else if path == b"/dev/stderr" {
                WDest::Stderr
            } else {
                WDest::File(std::fs::File::create(crate::util::path_from_bytes(path))
                    .map_err(|e| format!("couldn't open file {}: {}", show(path), errmsg(&e)))?)
            };
            self.wfiles.insert(path.to_vec(), WFile { dest, owed: false });
        }
        let Some(w) = self.wfiles.get_mut(path) else {
            return Err(format!("couldn't open file {}", show(path)));
        };
        w.write_line(bytes, terminated, separator)
    }

    /// One pass over the script for the current pattern space.
    #[allow(clippy::too_many_lines)] // the command dispatch: one arm per sed command
    fn run_cycle(&mut self, stream: &mut Stream, sink: &mut Sink) -> Result<Flow, String> {
        let mut pc = 0usize;
        while pc < self.script.cmds.len() {
            let at_last = stream.at_last();
            if !self.addr_matches(pc, at_last)? {
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
                    let width = *width;
                    let mut buf = Vec::new();
                    escape_for_l(&self.pattern, width, self.separator, &mut buf);
                    sink.write(&buf)?;
                }
                Some(Kind::Next) => {
                    if !self.suppress {
                        self.emit_pattern(sink)?;
                    }
                    self.flush_appends(sink)?;
                    let Some(line) = stream.next_line() else {
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
                    let Some(line) = stream.next_line() else {
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
                    let mut buf = self.file_name.clone();
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
        let script = compile_script(script.as_bytes(), ere, false, null_data, Vec::new()).unwrap();
        let nranges = script.cmds.len();
        let seed = seed_ranges(&script.cmds);
        let mut sed = Sed {
            script,
            ranges: seed,
            suppress: opts.contains(&"-n"),
            separator: sep,
            posix: false,
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
            wfiles: BTreeMap::new(),
            rfiles: BTreeMap::new(),
            file_name: b"-".to_vec(),
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
            let err = compile_script(script, false, false, false, Vec::new()).err();
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

    #[test]
    fn an_unknown_command_is_a_diagnosed_error() {
        // A bad script is status 1; an unresolvable branch is a RUNTIME error,
        // which GNU reports as 4.
        for bad in [&b"k"[..], b"s/a/b", b"{p"] {
            let err = compile_script(bad, false, false, false, Vec::new()).err().map(|f| f.status);
            assert_eq!(err, Some(1), "{:?} must be a status-1 script error", bad);
        }
        assert_eq!(
            compile_script(b"bnowhere", false, false, false, Vec::new()).err().map(|f| f.status),
            Some(4)
        );
    }

    /// `a`/`i`/`c` need text where the script ENDS, and an `-e` part ends there for
    /// that question only -- the same bytes as one argument are legal.
    #[test]
    fn a_text_command_needs_text_only_where_its_part_ends() {
        let compile = |src: &[u8], parts: Vec<usize>| {
            compile_script(src, false, false, false, parts).err().map(|f| f.status)
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
        let rx = |p: &[u8]| normalize_regex(p).unwrap();
        assert_eq!(rx(b"\\x41\\d066\\o103\\t"), b"ABC\t");
        assert_eq!(rx(b"[a\\x2dz]"), b"[a-z]"); // a decoded dash RANGES
        assert_eq!(rx(b"\\x5cw"), b"\\w"); // ... and a decoded backslash escapes
        assert_eq!(rx(b"\\\\n"), b"\\\\n"); // while an ESCAPED one just stands
        assert_eq!(rx(b"\\w\\1\\e"), b"\\w\\1\\e"); // none of these are GNU's
        assert_eq!(rx(b"[\\x]"), b"[x]"); // ... but a digitless `\x` IS, and sheds it
        assert_eq!(rx(b"\\c\\\\"), b"\x1c");
        assert_eq!(rx(b"\\c"), b"\\"); // bare: `Trailing backslash` from the compiler
        assert!(normalize_regex(b"\\c\\q").is_err());

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

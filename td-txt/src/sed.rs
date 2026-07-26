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

use crate::regex::{Captures, Options, Regex};
use crate::util::{number, read_input, records, show, Out};

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
    Append(Vec<u8>),
    Insert(Vec<u8>),
    Change(Vec<u8>),
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
        if raw.is_empty() && !icase && !multiline {
            return Ok(None); // `//` — reuse whatever matched last
        }
        let re = Regex::compile(raw, Options { ere: self.ere, icase, escapes: true, multiline })
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

    /// Text up to the next unescaped `delim`, with `\delim` collapsed to a plain
    /// `delim` and every other escape preserved for the regex compiler. A `delim`
    /// inside a bracket expression is literal, as in GNU sed.
    fn read_delimited(&mut self, delim: u8, unterminated: &str) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        let mut in_bracket = false;
        let mut bracket_body = 0usize; // index in `out` where the set's members start
        loop {
            let Some(b) = self.bump() else {
                return Err(unterminated.to_string());
            };
            if b == b'\\' && !in_bracket {
                let Some(n) = self.bump() else {
                    return Err(unterminated.to_string());
                };
                if n == delim && delim != b'\\' && !delim.is_ascii_alphanumeric() {
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
            if b == delim {
                return Ok(out);
            }
            if b == b'\n' {
                return Err(unterminated.to_string());
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
    fn parse_text(&mut self) -> Vec<u8> {
        self.skip_blank();
        let mut out = Vec::new();
        if self.eat(b'\\') {
            self.eat(b'\n');
        }
        loop {
            match self.bump() {
                None => break,
                Some(b'\\') => match self.bump() {
                    None => break,
                    Some(c) => out.push(c), // `\<newline>` continues the text
                },
                Some(b'\n') => break,
                Some(c) => out.push(c),
            }
        }
        // No terminator here: the writer appends the RECORD separator, which
        // under -z is NUL. Parsing cannot know it — the option is read first,
        // but the text belongs to the script.
        out
    }

    fn parse_label(&mut self) -> Vec<u8> {
        self.skip_blank();
        let start = self.pos;
        while !matches!(self.peek(), None | Some(b'\n' | b';' | b'}')) {
            self.pos += 1;
        }
        let mut label = self.src.get(start..self.pos).unwrap_or_default().to_vec();
        while matches!(label.last(), Some(b' ' | b'\t')) {
            label.pop();
        }
        label
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
        if delim == b'\n' || delim == b'\\' {
            return Err("unterminated `s' command".to_string());
        }
        let pattern = self.read_delimited(delim, "unterminated `s' command")?;
        let raw_repl = self.read_replacement(delim)?;
        let mut global = false;
        let mut print = false;
        let mut icase = false;
        let mut multiline = false;
        let mut occurrence: u64 = 0;
        let mut wfile = None;
        loop {
            match self.peek() {
                Some(b'g') => {
                    global = true;
                    self.pos += 1;
                }
                Some(b'p') => {
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
                    occurrence = self.parse_number().unwrap_or(0);
                    if occurrence == 0 {
                        return Err("number option to `s' command may not be zero".to_string());
                    }
                }
                _ => break,
            }
        }
        let re = self.add_regex(&pattern, icase, multiline)?;
        let replacement = compile_replacement(&raw_repl);
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

    /// The replacement half of `s///`: raw bytes with `\delim` collapsed, every
    /// other escape left for `compile_replacement`.
    fn read_replacement(&mut self, delim: u8) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        loop {
            let Some(b) = self.bump() else {
                return Err("unterminated `s' command".to_string());
            };
            if b == b'\\' {
                let Some(n) = self.bump() else {
                    return Err("unterminated `s' command".to_string());
                };
                if n == delim && delim != b'\\' {
                    out.push(n);
                } else {
                    out.push(b'\\');
                    out.push(n);
                }
                continue;
            }
            if b == delim {
                return Ok(out);
            }
            out.push(b);
        }
    }

    fn parse_transliterate(&mut self) -> Result<Box<[u8; 256]>, String> {
        let delim = self.bump().ok_or_else(|| "unterminated `y' command".to_string())?;
        let from = unescape_y(&self.read_replacement(delim)?);
        let to = unescape_y(&self.read_replacement(delim)?);
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

fn unescape_y(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while let Some(b) = raw.get(i).copied() {
        i += 1;
        if b != b'\\' {
            out.push(b);
            continue;
        }
        match raw.get(i).copied() {
            None => out.push(b'\\'),
            Some(n) => {
                i += 1;
                out.push(match n {
                    b'n' => b'\n',
                    b't' => b'\t',
                    b'r' => b'\r',
                    other => other,
                });
            }
        }
    }
    out
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
fn compile_replacement(raw: &[u8]) -> Vec<Repl> {
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
                i += 1;
                match n {
                    b'0'..=b'9' => {
                        flush(&mut lit, &mut out);
                        out.push(Repl::Group(usize::from(n - b'0')));
                    }
                    b'U' | b'L' | b'u' | b'l' | b'E' => {
                        flush(&mut lit, &mut out);
                        out.push(Repl::Case(match n {
                            b'U' => CaseOp::Upper,
                            b'L' => CaseOp::Lower,
                            b'u' => CaseOp::UpperOne,
                            b'l' => CaseOp::LowerOne,
                            _ => CaseOp::End,
                        }));
                    }
                    b'n' => lit.push(b'\n'),
                    b't' => lit.push(b'\t'),
                    b'r' => lit.push(b'\r'),
                    b'f' => lit.push(0x0c),
                    b'v' => lit.push(0x0b),
                    b'a' => lit.push(0x07),
                    b'e' => lit.push(0x1b),
                    // The numeric/control escapes GNU sed takes on both sides of
                    // an `s///` (`\x26` for a literal `&`, which is how the
                    // corpus's amp-escape case writes one).
                    b'c' => {
                        if let Some(c) = raw.get(i).copied() {
                            i += 1;
                            lit.push(c.to_ascii_uppercase() ^ 0x40);
                        }
                    }
                    b'd' | b'o' | b'x' => {
                        let (radix, digits) = match n {
                            b'd' => (10, 3),
                            b'o' => (8, 3),
                            _ => (16, 2),
                        };
                        match radix_escape(raw, &mut i, radix, digits) {
                            Some(byte) => lit.push(byte),
                            None => lit.push(n),
                        }
                    }
                    other => lit.push(other), // includes `\&`, `\\` and `\<newline>`
                }
            }
            _ => lit.push(b),
        }
    }
    flush(&mut lit, &mut out);
    out
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

/// Parse a whole script: a flat command list, its regex table, and its labels.
fn parse_script(src: &[u8], ere: bool) -> Result<Script, String> {
    let mut p = ScriptParser { src, pos: 0, ere, regexes: Vec::new() };
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
            b'a' => Kind::Append(p.parse_text()),
            b'i' => Kind::Insert(p.parse_text()),
            b'c' => Kind::Change(p.parse_text()),
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
                p.rest_of_line();
                Kind::Comment
            }
            b'e' => return Err("the `e' command is not supported".to_string()),
            other => return Err(format!("unknown command: `{}'", char::from(other))),
        };
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
    File(Vec<u8>),
    Line(Vec<u8>),
}

/// Where a cycle's output goes: stdout, or a buffer that `-i` renames into place.
enum Dest<'a> {
    Stdout(&'a mut Out),
    Buffer(Vec<u8>),
}

/// The output stream, which OWES a separator rather than dropping one.
///
/// GNU sed omits the trailing separator only at the very END of its output: with
/// input `a\nb` (no final newline), `sed p` writes `a\na\nb\nb` — the first copy
/// of the unterminated line still gets its newline, because more output followed.
/// So an unterminated line sets `owed`, and the next write pays it.
struct Sink<'a> {
    dest: Dest<'a>,
    separator: u8,
    owed: bool,
}

impl<'a> Sink<'a> {
    fn stdout(out: &'a mut Out, separator: u8) -> Self {
        Self { dest: Dest::Stdout(out), separator, owed: false }
    }

    fn buffer(separator: u8) -> Self {
        Self { dest: Dest::Buffer(Vec::new()), separator, owed: false }
    }

    fn put(&mut self, bytes: &[u8]) -> Result<(), String> {
        match &mut self.dest {
            Dest::Stdout(out) => out.write(bytes).map_err(|e| format!("write error: {e}")),
            Dest::Buffer(buf) => {
                buf.extend_from_slice(bytes);
                Ok(())
            }
        }
    }

    /// Write, paying any separator the previous line left owed.
    fn write(&mut self, bytes: &[u8]) -> Result<(), String> {
        if self.owed {
            self.owed = false;
            let sep = self.separator;
            self.put(&[sep])?;
        }
        self.put(bytes)
    }

    /// Queued `a`/`r` text. GNU terminates it with a NEWLINE even under `-z`
    /// (the append queue dumps the text as parsed), where `i`/`c` follow the
    /// record separator.
    fn write_text(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.write(bytes)?;
        self.put(b"\n")
    }

    /// Write one line; an unterminated one owes its separator to the next write.
    fn write_line(&mut self, bytes: &[u8], terminated: bool) -> Result<(), String> {
        self.write(bytes)?;
        if terminated {
            let sep = self.separator;
            return self.put(&[sep]);
        }
        self.owed = true;
        Ok(())
    }

    fn into_buffer(self) -> Vec<u8> {
        match self.dest {
            Dest::Buffer(buf) => buf,
            Dest::Stdout(_) => Vec::new(),
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
    posix: bool,
    pattern: Vec<u8>,
    hold: Vec<u8>,
    /// GNU tracks `chomped` per BUFFER: after `g`/`G`/`x` the pattern space's
    /// terminator is the hold space's, not the input line's. Starts terminated,
    /// so `G` on an unterminated last line still emits one.
    hold_terminated: bool,
    line_number: u64,
    terminated: bool,
    appends: Vec<Append>,
    replaced: bool,
    quit: Option<i32>,
    /// Index in the regex table of the last regex applied — what `//` reuses.
    last_regex: Option<usize>,
    wfiles: BTreeMap<Vec<u8>, std::fs::File>,
    /// `R` reads ONE line per invocation, so the file is parsed once and the
    /// cursor kept; re-reading per line would be quadratic.
    rfiles: BTreeMap<Vec<u8>, (Vec<Line>, usize)>,
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
                None => last_regex.ok_or_else(|| "no previous regular expression".to_string())?,
            };
            let re = regexes.get(idx).ok_or_else(|| "no previous regular expression".to_string())?;
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
        if width > 1 && *col + chunk.len() > width - 1 {
            out.push(b'\\');
            out.push(b'\n');
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
            eprintln!("sed: -e expression #1: {}", f.msg);
            f.status
        }
    }
}

/// Parse a script and resolve its branches. Split from `parse_script` so the two
/// failures keep their own exit statuses (see `Fatal`).
fn compile_script(src: &[u8], ere: bool, sandbox: bool) -> Result<Script, Fatal> {
    let mut script = parse_script(src, ere)?;
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
    b"in-place",
    b"null-data",
    b"posix",
    b"quiet",
    b"regexp-extended",
    b"sandbox",
    b"separate",
    b"silent",
    b"unbuffered",
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
                        script_parts.push(
                            read_input(&value).map_err(|e| format!("{}: {e}", show(&value)))?,
                        );
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
                        msg: format!("couldn't open file {}: {e}", show(&v)),
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
    for (n, part) in script_parts.iter().enumerate() {
        if n > 0 {
            source.push(b'\n');
        }
        source.extend_from_slice(part);
    }
    // `#n` on the very first line is POSIX's in-script spelling of -n.
    if source.starts_with(b"#n") && matches!(source.get(2), None | Some(b'\n')) {
        conf.suppress = true;
    }

    let script = compile_script(&source, conf.ere, conf.sandbox)?;
    let separator = if conf.null_data { 0u8 } else { b'\n' };
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
        terminated: true,
        appends: Vec::new(),
        replaced: false,
        quit: None,
        last_regex: None,
        wfiles: BTreeMap::new(),
        rfiles: BTreeMap::new(),
        file_name: b"-".to_vec(),
    };

    let inputs: Vec<Vec<u8>> = if files.is_empty() { vec![b"-".to_vec()] } else { files };
    let mut out = Out::new();
    let mut status = 0;

    if conf.separate || conf.in_place.is_some() {
        for path in &inputs {
            let data = match read_input(path) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("sed: can't read {}: {e}", show(path));
                    status = 2;
                    continue;
                }
            };
            let mut stream = to_stream(path, &data, separator);
            // Line numbers and range state restart per file under -s / -i.
            sed.line_number = 0;
            sed.ranges = seed_ranges(&sed.script.cmds);
            let quit = match &conf.in_place {
                Some(suffix) if path.as_slice() != b"-" => {
                    let mut sink = Sink::buffer(separator);
                    let quit = sed.run_stream(&mut stream, &mut sink)?;
                    write_in_place(path, suffix, &sink.into_buffer())?;
                    quit
                }
                _ => {
                    let mut sink = Sink::stdout(&mut out, separator);
                    sed.run_stream(&mut stream, &mut sink)?
                }
            };
            if let Some(code) = quit {
                out.flush().map_err(|e| format!("write error: {e}"))?;
                return Ok(code);
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
                    eprintln!("sed: can't read {}: {e}", show(path));
                    status = 2;
                }
            }
        }
        let mut stream = Stream { names, lines, pos: 0 };
        let mut sink = Sink::stdout(&mut out, separator);
        let quit = sed.run_stream(&mut stream, &mut sink)?;
        if let Some(code) = quit {
            out.flush().map_err(|e| format!("write error: {e}"))?;
            return Ok(code);
        }
    }
    out.flush().map_err(|e| format!("write error: {e}"))?;
    Ok(status)
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

fn write_in_place(path: &[u8], suffix: &[u8], data: &[u8]) -> Result<(), String> {
    let target = crate::util::path_from_bytes(path);
    if !suffix.is_empty() {
        // A `*` in the suffix stands for the file name (GNU's backup form).
        let backup = if suffix.contains(&b'*') {
            let base = crate::util::path_bytes(&target);
            let mut name = Vec::new();
            for b in suffix {
                if *b == b'*' {
                    name.extend_from_slice(&base);
                } else {
                    name.push(*b);
                }
            }
            crate::util::path_from_bytes(&name)
        } else {
            let mut name = crate::util::path_bytes(&target);
            name.extend_from_slice(suffix);
            crate::util::path_from_bytes(&name)
        };
        std::fs::copy(&target, &backup)
            .map_err(|e| format!("cannot back up {}: {e}", show(path)))?;
    }
    std::fs::write(&target, data).map_err(|e| format!("couldn't write {}: {e}", show(path)))
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

    fn flush_appends(&mut self, sink: &mut Sink) -> Result<(), String> {
        let appends = std::mem::take(&mut self.appends);
        for a in appends {
            match a {
                Append::Text(text) => sink.write_text(&text)?,
                // A missing file is not an error for `r`/`R`, as in GNU sed.
                Append::File(path) => {
                    if let Ok(data) = read_input(&path) {
                        sink.write(&data)?;
                    }
                }
                Append::Line(path) => {
                    let separator = self.separator;
                    let entry = match self.rfiles.get_mut(&path) {
                        Some(e) => e,
                        None => {
                            // A missing file is not an error for `R`: cache it as
                            // exhausted so it is opened at most once.
                            let lines = read_input(&path)
                                .map(|d| to_lines(&d, separator))
                                .unwrap_or_default();
                            self.rfiles.entry(path).or_insert((lines, 0))
                        }
                    };
                    let (lines, idx) = entry;
                    if let Some(line) = lines.get(*idx) {
                        let (text, terminated) = (line.text.clone(), line.terminated);
                        *idx += 1;
                        sink.write_line(&text, terminated)?;
                    }
                }
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
        if path == b"/dev/stdout" {
            return self.emit(sink, bytes, terminated);
        }
        if path == b"/dev/stderr" {
            eprintln!("{}", show(bytes));
            return Ok(());
        }
        use std::io::Write as _;
        if !self.wfiles.contains_key(path) {
            let file = std::fs::File::create(crate::util::path_from_bytes(path))
                .map_err(|e| format!("couldn't open file {}: {e}", show(path)))?;
            self.wfiles.insert(path.to_vec(), file);
        }
        let separator = self.separator;
        let Some(file) = self.wfiles.get_mut(path) else {
            return Err(format!("couldn't open file {}", show(path)));
        };
        file.write_all(bytes).map_err(|e| format!("write error: {e}"))?;
        if !terminated {
            return Ok(()); // the input line had none either
        }
        file.write_all(&[separator]).map_err(|e| format!("write error: {e}"))
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
                    let text = text.clone();
                    self.appends.push(Append::Text(text));
                }
                Some(Kind::Insert(text)) => {
                    let text = text.clone();
                    sink.write_line(&text, true)?;
                }
                Some(Kind::Change(text)) => {
                    let text = text.clone();
                    // Over a range, `c` prints once, at the range's last line.
                    let ends = self
                        .ranges
                        .get(pc)
                        .is_none_or(|r| !matches!(r, RangeState::Active(_)));
                    if ends {
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
                    self.flush_appends(sink)?;
                    let Some(line) = stream.next_line() else {
                        // GNU prints the pattern space and exits; POSIX drops it.
                        if self.posix {
                            self.pattern.clear();
                            return Ok(Flow::Deleted);
                        }
                        return Ok(Flow::Done);
                    };
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
                    if autoprint && !self.suppress {
                        self.emit_pattern(sink)?;
                    }
                    return Ok(Flow::Quit);
                }
                Some(Kind::ReadFile(path)) => {
                    let path = path.clone();
                    self.appends.push(Append::File(path));
                }
                Some(Kind::ReadLine(path)) => {
                    let path = path.clone();
                    self.appends.push(Append::Line(path));
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
            None => return Err("no previous regular expression".to_string()),
        };
        self.last_regex = Some(re_idx);
        let Some(re) = self.script.regexes.get(re_idx) else {
            return Err("no previous regular expression".to_string());
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
            let caps = match re.search(&hay, pos) {
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
        let script = compile_script(script.as_bytes(), ere, false).unwrap();
        let nranges = script.cmds.len();
        let seed = seed_ranges(&script.cmds);
        let mut sed = Sed {
            script,
            ranges: seed,
            suppress: opts.contains(&"-n"),
            separator: b'\n',
            posix: false,
            pattern: Vec::new(),
            hold: Vec::new(),
            hold_terminated: true,
            line_number: 0,
            terminated: true,
            appends: Vec::new(),
            replaced: false,
            quit: None,
            last_regex: None,
            wfiles: BTreeMap::new(),
            rfiles: BTreeMap::new(),
            file_name: b"-".to_vec(),
        };
        let mut stream = to_stream(b"-", input.as_bytes(), b'\n');
        let mut sink = Sink::buffer(b'\n');
        sed.run_stream(&mut stream, &mut sink).unwrap();
        sink.into_buffer()
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

    #[test]
    fn an_unknown_command_is_a_diagnosed_error() {
        // A bad script is status 1; an unresolvable branch is a RUNTIME error,
        // which GNU reports as 4.
        for bad in [&b"k"[..], b"s/a/b", b"{p"] {
            let err = compile_script(bad, false, false).err().map(|f| f.status);
            assert_eq!(err, Some(1), "{:?} must be a status-1 script error", bad);
        }
        assert_eq!(compile_script(b"bnowhere", false, false).err().map(|f| f.status), Some(4));
    }
}

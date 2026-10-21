//! unifdef 2.5's line filter, as `kernel-headers` runs linux 4.14's
//! scripts/unifdef.c: `-U__KERNEL__ -D__EXPORTED_HEADERS__` and no other
//! option. It is a port, not a reimplementation: the state table, comment
//! lexer and expression evaluator follow the C, so the headers it emits are
//! byte-identical, and what the C rejects is an error here too. Input the C
//! would split at its 4096-byte line buffer or cut at a NUL is refused.

type Result<T> = std::result::Result<T, String>;

const MAXLINE: usize = 4096;
const MAXDEPTH: usize = 64;

// The C's Linetype, as indices into TABLE's rows.
const LT_TRUEI: usize = 0;
const LT_FALSEI: usize = 1;
const LT_IF: usize = 2;
const LT_TRUE: usize = 3;
const LT_FALSE: usize = 4;
const LT_ELIF: usize = 5;
const LT_ELTRUE: usize = 6;
const LT_ELFALSE: usize = 7;
const LT_ELSE: usize = 8;
const LT_ENDIF: usize = 9;
const LT_DODGY: usize = 10;
const LT_PLAIN: usize = 20;
const LT_EOF: usize = 21;
const LT_ERROR: usize = 22;

// The C's Ifstate, as TABLE's row indices.
const IS_OUTSIDE: usize = 0;
const IS_FALSE_PREFIX: usize = 1;
const IS_TRUE_PREFIX: usize = 2;
const IS_PASS_MIDDLE: usize = 3;
const IS_FALSE_MIDDLE: usize = 4;
const IS_TRUE_MIDDLE: usize = 5;
const IS_PASS_ELSE: usize = 6;
const IS_FALSE_ELSE: usize = 7;
const IS_TRUE_ELSE: usize = 8;
const IS_FALSE_TRAILER: usize = 9;

/// `-U__KERNEL__ -D__EXPORTED_HEADERS__`, in the order unifdef adds them.
const SYMBOLS: &[(&[u8], Option<i32>)] =
    &[(b"__KERNEL__", None), (b"__EXPORTED_HEADERS__", Some(1))];

#[derive(Clone, Copy)]
enum Act {
    Print,
    Drop,
    Strue,
    Sfalse,
    Selse,
    Pelif,
    Pelse,
    Pendif,
    Dfalse,
    Delif,
    Delse,
    Dendif,
    Fdrop,
    Fpass,
    Ftrue,
    Ffalse,
    Oiffy,
    Oif,
    Oelif,
    Idrop,
    Itrue,
    Ifalse,
    Mpass,
    Mtrue,
    Melif,
    Melse,
    Eelif,
    Eelse,
    Eendif,
    Eeof,
    Eioccc,
    Done,
    Abort,
}

use Act::*;

// trans_table, row by row: TRUEI FALSEI IF TRUE FALSE ELIF ELTRUE ELFALSE
// ELSE ENDIF, the same ten DODGY, then PLAIN EOF ERROR. Read by position.
const TABLE: [[Act; 23]; 10] = [
    // IS_OUTSIDE
    [
        Itrue, Ifalse, Fpass, Ftrue, Ffalse, Eelif, Eelif, Eelif, Eelse, Eendif, Oiffy, Oiffy,
        Fpass, Oif, Oif, Eelif, Eelif, Eelif, Eelse, Eendif, Print, Done, Abort,
    ],
    // IS_FALSE_PREFIX
    [
        Idrop, Idrop, Fdrop, Fdrop, Fdrop, Mpass, Strue, Sfalse, Selse, Dendif, Idrop, Idrop,
        Fdrop, Fdrop, Fdrop, Mpass, Eioccc, Eioccc, Eioccc, Eioccc, Drop, Eeof, Abort,
    ],
    // IS_TRUE_PREFIX
    [
        Itrue, Ifalse, Fpass, Ftrue, Ffalse, Dfalse, Dfalse, Dfalse, Delse, Dendif, Oiffy, Oiffy,
        Fpass, Oif, Oif, Eioccc, Eioccc, Eioccc, Eioccc, Eioccc, Print, Eeof, Abort,
    ],
    // IS_PASS_MIDDLE
    [
        Itrue, Ifalse, Fpass, Ftrue, Ffalse, Pelif, Mtrue, Delif, Pelse, Pendif, Oiffy, Oiffy,
        Fpass, Oif, Oif, Pelif, Oelif, Oelif, Pelse, Pendif, Print, Eeof, Abort,
    ],
    // IS_FALSE_MIDDLE
    [
        Idrop, Idrop, Fdrop, Fdrop, Fdrop, Pelif, Mtrue, Delif, Pelse, Pendif, Idrop, Idrop, Fdrop,
        Fdrop, Fdrop, Eioccc, Eioccc, Eioccc, Eioccc, Eioccc, Drop, Eeof, Abort,
    ],
    // IS_TRUE_MIDDLE
    [
        Itrue, Ifalse, Fpass, Ftrue, Ffalse, Melif, Melif, Melif, Melse, Pendif, Oiffy, Oiffy,
        Fpass, Oif, Oif, Eioccc, Eioccc, Eioccc, Eioccc, Pendif, Print, Eeof, Abort,
    ],
    // IS_PASS_ELSE
    [
        Itrue, Ifalse, Fpass, Ftrue, Ffalse, Eelif, Eelif, Eelif, Eelse, Pendif, Oiffy, Oiffy,
        Fpass, Oif, Oif, Eelif, Eelif, Eelif, Eelse, Pendif, Print, Eeof, Abort,
    ],
    // IS_FALSE_ELSE
    [
        Idrop, Idrop, Fdrop, Fdrop, Fdrop, Eelif, Eelif, Eelif, Eelse, Dendif, Idrop, Idrop, Fdrop,
        Fdrop, Fdrop, Eelif, Eelif, Eelif, Eelse, Eioccc, Drop, Eeof, Abort,
    ],
    // IS_TRUE_ELSE
    [
        Itrue, Ifalse, Fpass, Ftrue, Ffalse, Eelif, Eelif, Eelif, Eelse, Dendif, Oiffy, Oiffy,
        Fpass, Oif, Oif, Eelif, Eelif, Eelif, Eelse, Eioccc, Print, Eeof, Abort,
    ],
    // IS_FALSE_TRAILER
    [
        Idrop, Idrop, Fdrop, Fdrop, Fdrop, Dfalse, Dfalse, Dfalse, Delse, Dendif, Idrop, Idrop,
        Fdrop, Fdrop, Fdrop, Dfalse, Dfalse, Dfalse, Delse, Eioccc, Drop, Eeof, Abort,
    ],
];

/// The binary operators by precedence level, loosest first.
const OPS: &[&[&[u8]]] = &[
    &[b"||"],
    &[b"&&"],
    &[b"==", b"!="],
    &[b"<=", b">=", b"<", b">"],
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Comment {
    No,
    C,
    Cxx,
    Starting,
    Finishing,
    Char,
    Str,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Line {
    Start,
    Hash,
    Dirty,
}

/// Filter one header as `unifdef -U__KERNEL__ -D__EXPORTED_HEADERS__`.
pub(crate) fn filter(input: &[u8]) -> Result<Vec<u8>> {
    if input.contains(&0) {
        return Err("input holds a NUL byte".into());
    }
    let mut filter = Filter {
        input,
        pos: 0,
        out: Vec::with_capacity(input.len()),
        tline: Vec::with_capacity(MAXLINE),
        keyword: 0,
        newline: None,
        incomment: Comment::No,
        linestate: Line::Start,
        ifstate: [IS_OUTSIDE; MAXDEPTH],
        ignoring: [false; MAXDEPTH],
        stifline: [0; MAXDEPTH],
        depth: 0,
        linenum: 0,
        constexpr: false,
    };
    filter.process()
}

struct Filter<'a> {
    input: &'a [u8],
    pos: usize,
    out: Vec<u8>,
    tline: Vec<u8>,
    keyword: usize,
    newline: Option<&'static [u8]>,
    incomment: Comment,
    linestate: Line,
    ifstate: [usize; MAXDEPTH],
    ignoring: [bool; MAXDEPTH],
    stifline: [usize; MAXDEPTH],
    depth: usize,
    linenum: usize,
    constexpr: bool,
}

fn endsym(c: u8) -> bool {
    !c.is_ascii_alphanumeric() && c != b'_'
}

impl Filter<'_> {
    fn at(&self, i: usize) -> u8 {
        self.tline.get(i).copied().unwrap_or(0)
    }

    fn starts(&self, i: usize, pattern: &[u8]) -> bool {
        self.tline
            .get(i..)
            .is_some_and(|rest| rest.starts_with(pattern))
    }

    fn error(&self, message: &str) -> String {
        let line = self.linenum;
        if self.depth == 0 {
            format!("{line}: {message}")
        } else {
            let start = self.stifline.get(self.depth).copied().unwrap_or(0);
            format!("{line}: {message} (#if line {start} depth {})", self.depth)
        }
    }

    /// fgets(buf, size): through a newline or size-1 bytes; None at EOF.
    /// A line the C would split is refused rather than split.
    fn fgets(&mut self, size: usize) -> Result<Option<Vec<u8>>> {
        // fgets(buf, 1) stores only the terminator and succeeds, even at EOF.
        if size == 1 {
            return Ok(Some(Vec::new()));
        }
        let rest = self.input.get(self.pos..).unwrap_or_default();
        if rest.is_empty() {
            return Ok(None);
        }
        let room = size.saturating_sub(1);
        let take = match rest.iter().position(|&b| b == b'\n') {
            Some(at) if at < room => at + 1,
            None if rest.len() <= room => rest.len(),
            _ => return Err(self.error("line exceeds unifdef's line buffer")),
        };
        let line = rest.get(..take).unwrap_or_default().to_vec();
        self.pos += take;
        Ok(Some(line))
    }

    fn process(&mut self) -> Result<Vec<u8>> {
        loop {
            let lineval = self.parseline()?;
            let state = self.ifstate.get(self.depth).copied().unwrap_or(IS_OUTSIDE);
            let act = TABLE
                .get(state)
                .and_then(|row| row.get(lineval))
                .copied()
                .ok_or_else(|| self.error("bug: no transition"))?;
            match act {
                Print => self.flushline(true),
                Drop => self.flushline(false),
                Strue => {
                    self.flushline(false);
                    self.ignoreoff()?;
                    self.state(IS_TRUE_PREFIX);
                }
                Sfalse => {
                    self.flushline(false);
                    self.ignoreoff()?;
                    self.state(IS_FALSE_PREFIX);
                }
                Selse => {
                    self.flushline(false);
                    self.state(IS_TRUE_ELSE);
                }
                Pelif => self.pelif()?,
                Pelse => {
                    self.flushline(true);
                    self.state(IS_PASS_ELSE);
                }
                Pendif => {
                    self.flushline(true);
                    self.unnest()?;
                }
                Dfalse => self.dfalse()?,
                Delif => {
                    self.flushline(false);
                    self.ignoreoff()?;
                    self.state(IS_FALSE_MIDDLE);
                }
                Delse => {
                    self.flushline(false);
                    self.state(IS_FALSE_ELSE);
                }
                Dendif => {
                    self.flushline(false);
                    self.unnest()?;
                }
                Fdrop => {
                    self.nest()?;
                    self.dfalse()?;
                }
                Fpass => {
                    self.nest()?;
                    self.pelif()?;
                }
                Ftrue => self.ftrue()?,
                Ffalse => self.ffalse()?,
                // -e is not given: an obfuscated control line is an error.
                Oiffy | Oif | Oelif | Eioccc => {
                    return Err(self.error("Obfuscated preprocessor control line"))
                }
                Idrop => {
                    self.nest()?;
                    self.dfalse()?;
                    self.ignoreon();
                }
                Itrue => {
                    self.ftrue()?;
                    self.ignoreon();
                }
                Ifalse => {
                    self.ffalse()?;
                    self.ignoreon();
                }
                Mpass => {
                    let keyword = self.keyword;
                    let edit = self
                        .tline
                        .get_mut(keyword..keyword + 4)
                        .ok_or_else(|| "bug: #elif keyword out of range".to_string())?;
                    edit.copy_from_slice(b"if  ");
                    self.pelif()?;
                }
                Mtrue => {
                    self.keywordedit(b"else");
                    self.state(IS_TRUE_MIDDLE);
                }
                Melif => {
                    self.keywordedit(b"endif");
                    self.state(IS_FALSE_TRAILER);
                }
                Melse => {
                    self.keywordedit(b"endif");
                    self.state(IS_FALSE_ELSE);
                }
                Eelif => return Err(self.error("Inappropriate #elif")),
                Eelse => return Err(self.error("Inappropriate #else")),
                Eendif => return Err(self.error("Inappropriate #endif")),
                Eeof => return Err(self.error("Premature EOF")),
                Done => {
                    if self.incomment != Comment::No {
                        return Err(self.error("EOF in comment"));
                    }
                    return Ok(std::mem::take(&mut self.out));
                }
                Abort => return Err(self.error("bug: unevaluable line")),
            }
        }
    }

    fn pelif(&mut self) -> Result<()> {
        self.flushline(true);
        self.ignoreoff()?;
        self.state(IS_PASS_MIDDLE);
        Ok(())
    }

    fn dfalse(&mut self) -> Result<()> {
        self.flushline(false);
        self.ignoreoff()?;
        self.state(IS_FALSE_TRAILER);
        Ok(())
    }

    fn ftrue(&mut self) -> Result<()> {
        self.nest()?;
        self.flushline(false);
        self.ignoreoff()?;
        self.state(IS_TRUE_PREFIX);
        Ok(())
    }

    fn ffalse(&mut self) -> Result<()> {
        self.nest()?;
        self.flushline(false);
        self.ignoreoff()?;
        self.state(IS_FALSE_PREFIX);
        Ok(())
    }

    fn flushline(&mut self, keep: bool) {
        if keep {
            self.out.extend_from_slice(&self.tline);
        }
    }

    fn state(&mut self, state: usize) {
        if let Some(slot) = self.ifstate.get_mut(self.depth) {
            *slot = state;
        }
    }

    fn ignoreoff(&mut self) -> Result<()> {
        let outer = self
            .depth
            .checked_sub(1)
            .and_then(|outer| self.ignoring.get(outer))
            .copied()
            .ok_or_else(|| "bug: ignoreoff outside #if".to_string())?;
        if let Some(slot) = self.ignoring.get_mut(self.depth) {
            *slot = outer;
        }
        Ok(())
    }

    fn ignoreon(&mut self) {
        if let Some(slot) = self.ignoring.get_mut(self.depth) {
            *slot = true;
        }
    }

    fn keywordedit(&mut self, replacement: &[u8]) {
        let newline = self.newline.unwrap_or(b"\n");
        self.tline.truncate(self.keyword);
        self.tline.extend_from_slice(replacement);
        self.tline.extend_from_slice(newline);
        self.flushline(true);
    }

    fn nest(&mut self) -> Result<()> {
        if self.depth + 1 >= MAXDEPTH {
            return Err(self.error("Too many levels of nesting"));
        }
        self.depth += 1;
        if let Some(slot) = self.stifline.get_mut(self.depth) {
            *slot = self.linenum;
        }
        Ok(())
    }

    fn unnest(&mut self) -> Result<()> {
        self.depth = self
            .depth
            .checked_sub(1)
            .ok_or_else(|| "bug: unnest outside #if".to_string())?;
        Ok(())
    }

    fn parseline(&mut self) -> Result<usize> {
        self.linenum += 1;
        let Some(line) = self.fgets(MAXLINE)? else {
            return Ok(LT_EOF);
        };
        self.tline = line;
        if self.newline.is_none() {
            let lf = self.tline.iter().rposition(|&b| b == b'\n');
            let cr = self.tline.iter().rposition(|&b| b == b'\r');
            let crlf = lf.is_some() && cr.is_some_and(|cr| Some(cr + 1) == lf);
            self.newline = Some(if crlf { b"\r\n" } else { b"\n" });
        }
        let mut retval = LT_PLAIN;
        let wascomment = self.incomment;
        let mut cp = self.skipcomment(0)?;
        if self.linestate == Line::Start {
            if self.at(cp) == b'#' {
                self.linestate = Line::Hash;
                cp = self.skipcomment(cp + 1)?;
            } else if self.at(cp) != 0 {
                self.linestate = Line::Dirty;
            }
        }
        if self.incomment == Comment::No && self.linestate == Line::Hash {
            self.keyword = cp;
            cp = self.skipsym(cp);
            let keyword = self
                .tline
                .get(self.keyword..cp)
                .unwrap_or_default()
                .to_vec();
            // No way can a continuation inside a keyword be handled.
            if self.starts(cp, b"\\\r\n") || self.starts(cp, b"\\\n") {
                return Err(self.error("Obfuscated preprocessor control line"));
            }
            match keyword.as_slice() {
                b"ifdef" | b"ifndef" => {
                    cp = self.skipcomment(cp)?;
                    retval = match self.findsym(cp) {
                        None => LT_IF,
                        Some(value) => {
                            let ifdef = keyword.as_slice() == b"ifdef";
                            if ifdef == value.is_some() {
                                LT_TRUE
                            } else {
                                LT_FALSE
                            }
                        }
                    };
                    cp = self.skipsym(cp);
                }
                b"if" => retval = self.ifeval(&mut cp)?,
                b"elif" => retval = self.ifeval(&mut cp)? + LT_ELIF - LT_IF,
                b"else" => retval = LT_ELSE,
                b"endif" => retval = LT_ENDIF,
                _ => {
                    self.linestate = Line::Dirty;
                    retval = LT_PLAIN;
                }
            }
            cp = self.skipcomment(cp)?;
            if self.at(cp) != 0 {
                self.linestate = Line::Dirty;
                if matches!(retval, LT_TRUE | LT_FALSE | LT_TRUEI | LT_FALSEI) {
                    retval = LT_IF;
                }
                if matches!(retval, LT_ELTRUE | LT_ELFALSE) {
                    retval = LT_ELIF;
                }
            }
            if retval != LT_PLAIN && (wascomment != Comment::No || self.incomment != Comment::No) {
                retval += LT_DODGY;
                if self.incomment != Comment::No {
                    self.linestate = Line::Dirty;
                }
            }
            // Still a hash line: the last line lacked a newline, or a
            // continuation joins the next one.
            if self.linestate == Line::Hash {
                let len = cp;
                let more = self.fgets(MAXLINE.saturating_sub(len))?;
                self.tline.truncate(len);
                match more {
                    None => {
                        let newline = self.newline.unwrap_or(b"\n");
                        self.tline.extend_from_slice(newline);
                        cp += newline.len();
                        self.linestate = Line::Start;
                    }
                    Some(more) => {
                        self.tline.extend_from_slice(&more);
                        self.linestate = Line::Dirty;
                    }
                }
            }
        }
        if self.linestate == Line::Dirty {
            while self.at(cp) != 0 {
                cp = self.skipcomment(cp + 1)?;
            }
        }
        Ok(retval)
    }

    fn ifeval(&mut self, cp: &mut usize) -> Result<usize> {
        // -k is not given: a constant expression is left alone.
        self.constexpr = true;
        let mut value = 0;
        let ret = self.eval_table(0, &mut value, cp)?;
        Ok(if self.constexpr || ret == LT_ERROR {
            LT_IF
        } else {
            ret
        })
    }

    fn eval_inner(&mut self, level: usize, value: &mut i32, cp: &mut usize) -> Result<usize> {
        if level + 1 < OPS.len() {
            self.eval_table(level + 1, value, cp)
        } else {
            self.eval_unary(value, cp)
        }
    }

    fn eval_table(&mut self, level: usize, value: &mut i32, cpp: &mut usize) -> Result<usize> {
        let ops = OPS.get(level).copied().unwrap_or_default();
        let mut cp = *cpp;
        let mut lt = self.eval_inner(level, value, &mut cp)?;
        if lt == LT_ERROR {
            return Ok(LT_ERROR);
        }
        loop {
            cp = self.skipcomment(cp)?;
            let Some(op) = ops.iter().copied().find(|op| self.starts(cp, op)) else {
                break;
            };
            cp += op.len();
            let mut right = 0;
            let rt = self.eval_inner(level, &mut right, &mut cp)?;
            if rt == LT_ERROR {
                return Ok(LT_ERROR);
            }
            lt = binary(op, value, lt, rt, right);
        }
        *cpp = cp;
        Ok(lt)
    }

    fn eval_unary(&mut self, value: &mut i32, cpp: &mut usize) -> Result<usize> {
        let mut cp = self.skipcomment(*cpp)?;
        let lt;
        let c = self.at(cp);
        if c == b'!' {
            cp += 1;
            let inner = self.eval_unary(value, &mut cp)?;
            if inner == LT_ERROR {
                return Ok(LT_ERROR);
            }
            lt = if inner == LT_IF {
                inner
            } else {
                *value = i32::from(*value == 0);
                truth(*value)
            };
        } else if c == b'(' {
            cp += 1;
            let inner = self.eval_table(0, value, &mut cp)?;
            if inner == LT_ERROR {
                return Ok(LT_ERROR);
            }
            cp = self.skipcomment(cp)?;
            if self.at(cp) != b')' {
                return Ok(LT_ERROR);
            }
            cp += 1;
            lt = inner;
        } else if c.is_ascii_digit() {
            *value = self.strtol(cp);
            lt = truth(*value);
            cp = self.skipsym(cp);
        } else if self.starts(cp, b"defined") && endsym(self.at(cp + 7)) {
            cp = self.skipcomment(cp + 7)?;
            let paren = self.at(cp) == b'(';
            if paren {
                cp = self.skipcomment(cp + 1)?;
            }
            lt = match self.findsym(cp) {
                None => LT_IF,
                Some(defined) => {
                    *value = i32::from(defined.is_some());
                    truth(*value)
                }
            };
            cp = self.skipsym(cp);
            cp = self.skipcomment(cp)?;
            if paren {
                if self.at(cp) != b')' {
                    return Ok(LT_ERROR);
                }
                cp += 1;
            }
            self.constexpr = false;
        } else if !endsym(c) {
            let symbol = self.findsym(cp);
            cp = self.skipsym(cp);
            lt = match symbol {
                None => {
                    cp = self.skipargs(cp)?;
                    LT_IF
                }
                Some(None) => {
                    *value = 0;
                    LT_FALSE
                }
                Some(Some(defined)) => {
                    *value = defined;
                    cp = self.skipargs(cp)?;
                    truth(defined)
                }
            };
            self.constexpr = false;
        } else {
            return Ok(LT_ERROR);
        }
        *cpp = cp;
        Ok(lt)
    }

    /// strtol(cp, &end, 0) at a digit, narrowed to the C's int.
    fn strtol(&self, cp: usize) -> i32 {
        let hex = self.at(cp) == b'0'
            && matches!(self.at(cp + 1), b'x' | b'X')
            && self.at(cp + 2).is_ascii_hexdigit();
        let (base, mut at) = if hex {
            (16, cp + 2)
        } else if self.at(cp) == b'0' {
            (8, cp)
        } else {
            (10, cp)
        };
        let mut total: i64 = 0;
        while let Some(digit) = char::from(self.at(at)).to_digit(base) {
            total = total
                .saturating_mul(i64::from(base))
                .saturating_add(i64::from(digit));
            at += 1;
        }
        total as i32
    }

    fn skipargs(&mut self, cp: usize) -> Result<usize> {
        let ocp = cp;
        let mut level = 0i64;
        let mut cp = self.skipcomment(cp)?;
        if self.at(cp) != b'(' {
            return Ok(cp);
        }
        loop {
            match self.at(cp) {
                b'(' => level += 1,
                b')' => level -= 1,
                _ => {}
            }
            cp = self.skipcomment(cp + 1)?;
            if level == 0 || self.at(cp) == 0 {
                break;
            }
        }
        // Rewind and re-detect the syntax error later.
        Ok(if level == 0 { cp } else { ocp })
    }

    fn skipsym(&self, mut cp: usize) -> usize {
        while !endsym(self.at(cp)) {
            cp += 1;
        }
        cp
    }

    /// The symbol at `cp`: None when unknown, else its value (None when
    /// undefined).
    fn findsym(&self, cp: usize) -> Option<Option<i32>> {
        let end = self.skipsym(cp);
        let name = self.tline.get(cp..end).filter(|name| !name.is_empty())?;
        SYMBOLS
            .iter()
            .find(|(symbol, _)| *symbol == name)
            .map(|(_, value)| *value)
    }

    /// Skip comments, strings and character literals to the next character
    /// that is not whitespace, tracking the comment and line state.
    fn skipcomment(&mut self, mut cp: usize) -> Result<usize> {
        if self.ignoring.get(self.depth).copied().unwrap_or(false) {
            while matches!(
                self.at(cp),
                b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r'
            ) {
                if self.at(cp) == b'\n' {
                    self.linestate = Line::Start;
                }
                cp += 1;
            }
            return Ok(cp);
        }
        while self.at(cp) != 0 {
            // A line continuation does not reset the line state.
            if self.starts(cp, b"\\\r\n") {
                cp += 3;
                continue;
            }
            if self.starts(cp, b"\\\n") {
                cp += 2;
                continue;
            }
            let c = self.at(cp);
            match self.incomment {
                Comment::No => {
                    if self.starts(cp, b"/\\\r\n") {
                        self.incomment = Comment::Starting;
                        cp += 4;
                    } else if self.starts(cp, b"/\\\n") {
                        self.incomment = Comment::Starting;
                        cp += 3;
                    } else if self.starts(cp, b"/*") {
                        self.incomment = Comment::C;
                        cp += 2;
                    } else if self.starts(cp, b"//") {
                        self.incomment = Comment::Cxx;
                        cp += 2;
                    } else if c == b'\'' {
                        self.incomment = Comment::Char;
                        self.linestate = Line::Dirty;
                        cp += 1;
                    } else if c == b'"' {
                        self.incomment = Comment::Str;
                        self.linestate = Line::Dirty;
                        cp += 1;
                    } else if c == b'\n' {
                        self.linestate = Line::Start;
                        cp += 1;
                    } else if matches!(c, b' ' | b'\r' | b'\t') {
                        cp += 1;
                    } else {
                        return Ok(cp);
                    }
                }
                Comment::Cxx => {
                    if c == b'\n' {
                        self.incomment = Comment::No;
                        self.linestate = Line::Start;
                    }
                    cp += 1;
                }
                Comment::Char | Comment::Str => {
                    let close = if self.incomment == Comment::Char {
                        b'\''
                    } else {
                        b'"'
                    };
                    if c == close {
                        self.incomment = Comment::No;
                        cp += 1;
                    } else if c == b'\\' {
                        cp += if self.at(cp + 1) == 0 { 1 } else { 2 };
                    } else if c == b'\n' {
                        return Err(self.error(if self.incomment == Comment::Char {
                            "unterminated char literal"
                        } else {
                            "unterminated string literal"
                        }));
                    } else {
                        cp += 1;
                    }
                }
                Comment::C => {
                    if self.starts(cp, b"*\\\r\n") {
                        self.incomment = Comment::Finishing;
                        cp += 4;
                    } else if self.starts(cp, b"*\\\n") {
                        self.incomment = Comment::Finishing;
                        cp += 3;
                    } else if self.starts(cp, b"*/") {
                        self.incomment = Comment::No;
                        cp += 2;
                    } else {
                        cp += 1;
                    }
                }
                Comment::Starting => {
                    if c == b'*' {
                        self.incomment = Comment::C;
                        cp += 1;
                    } else if c == b'/' {
                        self.incomment = Comment::Cxx;
                        cp += 1;
                    } else {
                        self.incomment = Comment::No;
                        self.linestate = Line::Dirty;
                    }
                }
                Comment::Finishing => {
                    if c == b'/' {
                        self.incomment = Comment::No;
                        cp += 1;
                    } else {
                        self.incomment = Comment::C;
                    }
                }
            }
        }
        Ok(cp)
    }
}

fn truth(value: i32) -> usize {
    if value != 0 {
        LT_TRUE
    } else {
        LT_FALSE
    }
}

/// op_or, op_and and the strict comparisons; not -K, so a known true
/// operand of `||` (false of `&&`) decides an otherwise unknown expression.
fn binary(op: &[u8], value: &mut i32, at: usize, bt: usize, b: i32) -> usize {
    let a = *value;
    let result = match op {
        b"||" if at == LT_TRUE || bt == LT_TRUE => {
            *value = 1;
            return LT_TRUE;
        }
        b"&&" if at == LT_FALSE || bt == LT_FALSE => {
            *value = 0;
            return LT_FALSE;
        }
        b"||" => a != 0 || b != 0,
        b"&&" => a != 0 && b != 0,
        b"==" => a == b,
        b"!=" => a != b,
        b"<=" => a <= b,
        b">=" => a >= b,
        b"<" => a < b,
        _ => a > b,
    };
    if at == LT_IF || bt == LT_IF {
        return LT_IF;
    }
    *value = i32::from(result);
    truth(*value)
}

#[cfg(test)]
mod tests {
    use super::filter;

    fn run(input: &str) -> String {
        String::from_utf8(filter(input.as_bytes()).unwrap()).unwrap()
    }

    #[test]
    fn known_groups_go_and_unknown_ones_stay() {
        assert_eq!(
            run("a\n#ifdef __KERNEL__\nk\n#else\nu\n#endif\nb\n"),
            "a\nu\nb\n"
        );
        assert_eq!(run("#ifndef __KERNEL__\nu\n#endif\n"), "u\n");
        assert_eq!(run("#ifdef FOO\nf\n#endif\n"), "#ifdef FOO\nf\n#endif\n");
        assert_eq!(run("#ifndef __EXPORTED_HEADERS__\nx\n#endif\ny\n"), "y\n");
    }

    #[test]
    fn partial_expressions_resolve_or_stay_verbatim() {
        // A true operand of || decides it; otherwise the line is kept as is.
        assert_eq!(
            run("#if !defined(__KERNEL__) || defined(X)\nu\n#endif\n"),
            "u\n"
        );
        let kept = "#if defined(__KERNEL__) || defined(__linux__)\nl\n#endif\n";
        assert_eq!(run(kept), kept);
        assert_eq!(
            run("#if defined(__KERNEL__) && defined(X)\nk\n#endif\n"),
            ""
        );
        // A constant expression is not evaluated without -k.
        assert_eq!(run("#if 0\nz\n#endif\n"), "#if 0\nz\n#endif\n");
    }

    #[test]
    fn elif_edits_follow_the_state_table() {
        // A false #if before an unknown #elif turns it into #if, in place.
        assert_eq!(
            run("#ifdef __KERNEL__\nk\n#elif X\nx\n#endif\n"),
            "#if   X\nx\n#endif\n"
        );
        // A true #elif after an unknown #if becomes #else.
        assert_eq!(
            run("#if X\nx\n#elif !defined(__KERNEL__)\nu\n#else\ne\n#endif\n"),
            "#if X\nx\n#else\nu\n#endif\n"
        );
    }

    /// Inputs run through the pinned unifdef.c, built with gcc, and its
    /// output (None where it exits 2).
    #[test]
    fn the_c_unifdef_agrees() {
        let cases: &[(&[u8], Option<&[u8]>)] = &[
            (b"#ifdef __KERNEL__\r\nk\r\n#elif X\r\nx\r\n#else\r\ne\r\n#endif\r\n", Some(&b"#if   X\r\nx\r\n#else\r\ne\r\n#endif\r\n"[..])),
            (b"#if X\nx\n#elif defined(__EXPORTED_HEADERS__)\ny\n#elif Z\nz\n#else\nw\n#endif\n", Some(&b"#if X\nx\n#else\ny\n#endif\n"[..])),
            (b"#if X\r\nx\r\n#elif defined(__EXPORTED_HEADERS__)\r\ny\r\n#else\r\nw\r\n#endif\r\n", Some(&b"#if X\r\nx\r\n#else\r\ny\r\n#endif\r\n"[..])),
            (b"#if defined(X) && \\\n    defined(Y)\nxy\n#endif\n", Some(&b"#if defined(X) && \\\n    defined(Y)\nxy\n#endif\n"[..])),
            (b"#if !defined(__KERNEL__) || \\\n defined(Y)\nu\n#endif\n", Some(&b"#if !defined(__KERNEL__) || \\\n defined(Y)\nu\n#endif\n"[..])),
            (b"/* start\n */ #ifdef __KERNEL__\nk\n#endif\n", None),
            (b"#ifdef X /* trailing\n comment */\nx\n#endif\n", Some(&b"#ifdef X /* trailing\n comment */\nx\n#endif\n"[..])),
            (b"#ifdef __KERNEL__ /* k */\nk\n#else /* u */\nu\n#endif /* end */\n", Some(&b"u\n"[..])),
            (b"#ifdef __KERNEL__\n#if X\na\n#else\nb\n#endif\n#endif\nc\n", Some(&b"c\n"[..])),
            (b"#if defined __EXPORTED_HEADERS__\nd\n#endif\n#if defined __KERNEL__ && X\nk\n#endif\n", Some(&b"d\n"[..])),
            (b"#if FOO(a, (b)) || !defined(__KERNEL__)\nf\n#endif\n#if FOO(a || 1\nq\n#endif\n", Some(&b"f\n#if FOO(a || 1\nq\n#endif\n"[..])),
            (b"#if 0x10 > 015 && defined(__EXPORTED_HEADERS__)\nn\n#endif\n#if 99999999999999999999 && defined(__KERNEL__)\no\n#endif\n#if 1UL == 1 || defined(__KERNEL__)\np\n#endif\n", Some(&b"n\np\n"[..])),
            (b"char c = '\\'';\n#ifdef __KERNEL__\nk\n#endif\nchar *s = \"a\\\"#ifdef\";\n", Some(&b"char c = '\\'';\nchar *s = \"a\\\"#ifdef\";\n"[..])),
            (b"#\\\nifdef __KERNEL__\nk\n#endif\n", None),
            (b"#ifd\\\nef X\nx\n#endif\n", None),
            (b"a\n#ifdef X\nx\n#endif\nlast", Some(&b"a\n#ifdef X\nx\n#endif\nlast"[..])),
            (b"", Some(&b""[..])),
            (b"/\\\n* comment */\n#ifdef __KERNEL__\nk\n#endif\n", Some(&b"/\\\n* comment */\n"[..])),
            (b"#elif X\n", None),
            (b"#if 1\na\n#endif\n", Some(&b"#if 1\na\n#endif\n"[..])),
            (b"#ifndef __EXPORTED_HEADERS__\nn\n#elif X\nx\n#endif\n", Some(&b"#if   X\nx\n#endif\n"[..])),
            (b"#ifdef __KERNEL__ junk\nk\n#endif\n", Some(&b"#ifdef __KERNEL__ junk\nk\n#endif\n"[..])),
            (b"#  ifndef   __KERNEL__  \nu\n#  endif\n", Some(&b"u\n"[..])),
            (b"\"abc\n", None),
            (b"// line comment #ifdef __KERNEL__\n#ifdef __KERNEL__ // k\nk\n#endif\n", Some(&b"// line comment #ifdef __KERNEL__\n"[..])),
            (b"#if (defined(__KERNEL__) || defined(X)) && !(1 < 2)\nz\n#endif\n", Some(&b""[..])),
        ];
        for (input, expected) in cases {
            assert_eq!(
                filter(input).ok().as_deref(),
                *expected,
                "{}",
                String::from_utf8_lossy(input)
            );
        }
        // A last directive line that fills the C's buffer is re-read with
        // fgets(.., 1), which succeeds at EOF: no newline is appended.
        let mut full = b"#ifdef X\nx\n#endif".to_vec();
        full.resize(full.len() + 4089, b' ');
        assert_eq!(filter(&full).unwrap(), full);
        let mut continued = b"#ifdef X\nx\n#endif".to_vec();
        continued.resize(continued.len() + 4087, b' ');
        continued.extend_from_slice(b"\\\nnext\n");
        assert_eq!(filter(&continued).unwrap(), continued);
        // An edited keyword as far into the buffer as a line allows: the
        // C's EDITSLOP holds the longer replacement and its newline.
        let pad = |n: usize| vec![b' '; n];
        let edge = |pad: &[u8], head: &[u8], keyword: &[u8], tail: &[u8]| {
            [head, b"#", pad, keyword, tail].concat()
        };
        let head = b"#if X\nx\n#elif defined(__EXPORTED_HEADERS__)\ny\n";
        let kept = b"#if X\nx\n#else\ny\n";
        for (spaces, keyword) in [(4089, &b"else"[..]), (4087, &b"elif Z"[..])] {
            assert_eq!(
                filter(&edge(&pad(spaces), head, keyword, b"\nz\n#endif\n")).unwrap(),
                edge(&pad(spaces), kept, b"endif", b"\n")
            );
        }
        assert_eq!(
            filter(&edge(
                &pad(4059),
                b"#if X\nx\n",
                b"elif defined(__EXPORTED_HEADERS__)",
                b"\ny\n#endif\n"
            ))
            .unwrap(),
            edge(&pad(4059), b"#if X\nx\n", b"else", b"\ny\n#endif\n")
        );
    }

    #[test]
    fn directives_in_comments_are_text_and_errors_stay_errors() {
        let commented = "/*\n#ifdef __KERNEL__\n*/\nx\n";
        assert_eq!(run(commented), commented);
        assert!(filter(b"#ifdef __KERNEL__\nk\n").is_err());
        assert!(filter(b"#endif\n").is_err());
        assert!(filter(b"/* open\n").is_err());
        assert!(filter(b"a\0b\n").is_err());
        assert!(filter(&[b'x'; 5000]).is_err());
        // The last line may lack its newline; a directive there gains one.
        assert_eq!(run("#ifdef X\nx\n#endif"), "#ifdef X\nx\n#endif\n");
    }
}

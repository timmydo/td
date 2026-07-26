//! Tokenizer: source text -> operator/word tokens, with here-document bodies
//! collected as they are passed on the input.
//!
//! Words are scanned into `Seg`s here (quotes, `$name`, `${...}`, `$(...)`,
//! `` `...` ``, `$((...))`) so the parser never re-reads characters and the
//! expander never re-guesses what was quoted. Reserved words are NOT recognized
//! here: whether `in` is a keyword or an argument depends on grammar position,
//! which only the parser knows.

use crate::ast::{is_name, Param, ParamOp, Seg, Syn, Word, INCOMPLETE};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Semi,
    DSemi,
    Amp,
    AndIf,
    OrIf,
    Pipe,
    LParen,
    RParen,
    Less,
    Great,
    DGreat,
    LessAnd,
    GreatAnd,
    LessGreat,
    Clobber,
    /// `<<` / `<<-`; the payload indexes `Lexed::heredocs`.
    DLess(usize),
}

impl Op {
    pub fn is_redirect(self) -> bool {
        matches!(
            self,
            Op::Less
                | Op::Great
                | Op::DGreat
                | Op::LessAnd
                | Op::GreatAnd
                | Op::LessGreat
                | Op::Clobber
                | Op::DLess(_)
        )
    }

    pub fn text(self) -> &'static str {
        match self {
            Op::Semi => ";",
            Op::DSemi => ";;",
            Op::Amp => "&",
            Op::AndIf => "&&",
            Op::OrIf => "||",
            Op::Pipe => "|",
            Op::LParen => "(",
            Op::RParen => ")",
            Op::Less => "<",
            Op::Great => ">",
            Op::DGreat => ">>",
            Op::LessAnd => "<&",
            Op::GreatAnd => ">&",
            Op::LessGreat => "<>",
            Op::Clobber => ">|",
            Op::DLess(_) => "<<",
        }
    }
}

#[derive(Clone, Debug)]
pub enum Tok {
    Word(Word),
    Op(Op),
    /// Digits written immediately before a redirection operator (`2>file`).
    IoNumber(u32),
    Newline,
    Eof,
}

pub struct Lexed {
    pub toks: Vec<Tok>,
    /// Here-document bodies, indexed by the id carried in `Op::DLess`.
    pub heredocs: Vec<Word>,
    /// The input ended inside a `#` comment. Only alias substitution cares: a
    /// replacement that trails off in a comment must swallow the rest of the
    /// line it was written on, which is text the replacement does not contain.
    pub ended_in_comment: bool,
    /// What stopped the scan, if anything. `toks` is then the valid prefix before
    /// it, which the parser runs before reporting: a shell lexes as it parses, so
    /// the commands ahead of a bad quote have already run when it is diagnosed.
    pub error: Option<String>,
}

/// A here-document whose operator has been seen but whose body has not been
/// read yet (the body starts on the line *after* the operator).
struct Pending {
    id: usize,
    delim: String,
    quoted: bool,
    strip_tabs: bool,
}

struct Scanner {
    src: Vec<char>,
    pos: usize,
    /// Set when the last thing consumed was a `\<newline>` fold, so the lexer can
    /// tell "input ended" from "input stopped mid-line and the rest is coming".
    continued: bool,
}

impl Scanner {
    fn peek(&self) -> Option<char> {
        self.src.get(self.pos).copied()
    }

    fn peek_at(&self, off: usize) -> Option<char> {
        self.src.get(self.pos + off).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
            self.continued = false;
        }
        c
    }

    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.pos += 1;
            self.continued = false;
            true
        } else {
            false
        }
    }

    /// Consume a `\<newline>` line continuation, recording that the input is
    /// mid-line if it stops here.
    fn fold_continuation(&mut self) {
        self.bump();
        self.bump();
        self.continued = true;
    }
}

/// Characters that end a word without being part of it.
fn is_word_end(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '|' | '&' | ';' | '<' | '>' | '(' | ')')
}

fn is_special_param(c: char) -> bool {
    matches!(c, '@' | '*' | '#' | '?' | '-' | '$' | '!')
}

/// Accumulates a word, merging runs of like segments so `abc` is one `Lit`.
#[derive(Default)]
struct WordBuf {
    segs: Vec<Seg>,
}

impl WordBuf {
    fn push_lit(&mut self, c: char) {
        match self.segs.last_mut() {
            Some(Seg::Lit(s)) => s.push(c),
            _ => self.segs.push(Seg::Lit(c.to_string())),
        }
    }

    fn push_quoted(&mut self, c: char) {
        match self.segs.last_mut() {
            Some(Seg::Quoted(s)) => s.push(c),
            _ => self.segs.push(Seg::Quoted(c.to_string())),
        }
    }

    /// Make sure a quoted segment exists even when it is empty, so `""` is a
    /// real (empty) field rather than nothing at all.
    fn open_quoted(&mut self) {
        if !matches!(self.segs.last(), Some(Seg::Quoted(_))) {
            self.segs.push(Seg::Quoted(String::new()));
        }
    }

    /// Called after scanning a `"..."`/`'...'` region that began with `len`
    /// segments already present. If the region produced no new content, record
    /// an empty quoted segment so the empty quotes still form a field — but a
    /// non-empty region (notably a lone `"$@"`) is left as-is, so it keeps the
    /// zero-field behaviour when the expansion is empty.
    fn mark_empty_quote(&mut self, len: usize) {
        if self.segs.len() == len && !matches!(self.segs.last(), Some(Seg::Quoted(_))) {
            self.segs.push(Seg::Quoted(String::new()));
        }
    }

    fn push_seg(&mut self, seg: Seg) {
        self.segs.push(seg);
    }

    fn finish(self) -> Word {
        Word(self.segs)
    }
}

/// Cap on nested expansion re-lexing (`${a:-${b:-${c:-…}}}`, `$(( ${x} ))`), which
/// re-enters `word_from_str_at`/`arith_from_str_at` per level. Bounds the recursion so
/// a pathological input errors instead of overflowing the stack. Well above any real
/// script's nesting.
const MAX_EXPANSION_DEPTH: u32 = 100;

/// Lex all of `src`, failing on the first error. For text that must be whole to
/// mean anything at all -- an alias replacement, an expansion operand.
pub fn tokenize(src: &str) -> Syn<Lexed> {
    let lexed = tokenize_prefix(src);
    match lexed.error {
        Some(e) => Err(e),
        None => Ok(lexed),
    }
}

/// Lex as much of `src` as scans, reporting what stopped it in `Lexed::error`.
pub fn tokenize_prefix(src: &str) -> Lexed {
    let mut lx = Lexer {
        sc: Scanner {
            src: src.chars().collect(),
            pos: 0,
            continued: false,
        },
        ended_in_comment: false,
        heredocs: Vec::new(),
        pending: Vec::new(),
        awaiting: None,
        depth: 0,
    };
    lx.run()
}

/// Scan `text` as a single word: blanks and operator characters are ordinary
/// literal characters. Used for the operand of `${x:-...}`, which is delimited by
/// its brace, not by blanks. A nested `${…}` operand re-enters here one level
/// deeper; the depth cap is checked before any scanning so the mutual recursion
/// `scan_dollar -> parse_param -> word_from_str_at` is bounded.
fn word_from_str_at(text: &str, depth: u32) -> Syn<Word> {
    lexer_over(text, depth)?.scan_word(false)
}

/// Scan the body of `$((...))`. POSIX expands it as if it were double-quoted,
/// "except that a double-quote inside the expression is not treated specially",
/// so NEITHER quote character quotes here: both reach the arithmetic lexer, which
/// rejects them. That is why `$(( '1' + 2 ))` is an error and not 3.
fn arith_from_str_at(text: &str, depth: u32) -> Syn<Word> {
    lexer_over(text, depth)?.scan_arith_body()
}

fn lexer_over(text: &str, depth: u32) -> Syn<Lexer> {
    if depth > MAX_EXPANSION_DEPTH {
        return Err("expansion nested too deeply".into());
    }
    Ok(Lexer {
        sc: Scanner {
            src: text.chars().collect(),
            pos: 0,
            continued: false,
        },
        ended_in_comment: false,
        heredocs: Vec::new(),
        pending: Vec::new(),
        awaiting: None,
        depth,
    })
}

struct Lexer {
    sc: Scanner,
    ended_in_comment: bool,
    heredocs: Vec<Word>,
    pending: Vec<Pending>,
    /// Set when a `<<` operator is waiting for its delimiter word.
    awaiting: Option<(usize, bool)>,
    /// Expansion-nesting depth of this (re-)lexing pass; see `MAX_EXPANSION_DEPTH`.
    depth: u32,
}

impl Lexer {
    fn run(&mut self) -> Lexed {
        let mut toks = Vec::new();
        let mut error = None;
        loop {
            match self.next_tok() {
                Ok(tok) => {
                    let last = matches!(tok, Tok::Eof);
                    toks.push(tok);
                    if last {
                        break;
                    }
                }
                Err(e) => {
                    error = Some(e);
                    break;
                }
            }
        }
        Lexed {
            toks,
            heredocs: std::mem::take(&mut self.heredocs),
            ended_in_comment: self.ended_in_comment,
            error,
        }
    }

    /// The next token, or `Tok::Eof` once the input is spent.
    fn next_tok(&mut self) -> Syn<Tok> {
        loop {
            self.skip_blanks();
            let Some(c) = self.sc.peek() else {
                if self.sc.continued {
                    return Err(format!("{INCOMPLETE}: line continuation"));
                }
                self.finish_heredocs()?;
                return Ok(Tok::Eof);
            };
            if c == '\n' {
                self.sc.bump();
                self.read_heredoc_bodies()?;
                return Ok(Tok::Newline);
            }
            if c == '#' {
                while let Some(c) = self.sc.peek() {
                    if c == '\n' {
                        break;
                    }
                    self.sc.bump();
                }
                self.ended_in_comment = self.sc.peek().is_none();
                continue;
            }
            if matches!(c, '|' | '&' | ';' | '<' | '>' | '(' | ')') {
                return Ok(Tok::Op(self.scan_op()?));
            }
            let word = self.scan_word(true)?;
            if let Some((id, strip_tabs)) = self.awaiting.take() {
                let Some((delim, quoted)) = word.delimiter() else {
                    return Err("here-document delimiter may not be an expansion".into());
                };
                self.pending.push(Pending {
                    id,
                    delim,
                    quoted,
                    strip_tabs,
                });
                return Ok(Tok::Word(word));
            }
            // `2>file`: digits glued to a redirection operator name the fd.
            if let Some(text) = word.plain() {
                if !text.is_empty()
                    && text.chars().all(|c| c.is_ascii_digit())
                    && matches!(self.sc.peek(), Some('<') | Some('>'))
                {
                    if let Ok(n) = text.parse::<u32>() {
                        return Ok(Tok::IoNumber(n));
                    }
                }
            }
            return Ok(Tok::Word(word));
        }
    }

    fn skip_blanks(&mut self) {
        loop {
            match self.sc.peek() {
                Some(' ') | Some('\t') => {
                    self.sc.bump();
                }
                Some('\\') if self.sc.peek_at(1) == Some('\n') => {
                    self.sc.fold_continuation();
                }
                _ => return,
            }
        }
    }

    fn scan_op(&mut self) -> Syn<Op> {
        let Some(c) = self.sc.bump() else {
            return Err(format!("{INCOMPLETE}: expected an operator"));
        };
        let op = match c {
            ';' => {
                if self.sc.eat(';') {
                    Op::DSemi
                } else {
                    Op::Semi
                }
            }
            '&' => {
                if self.sc.eat('&') {
                    Op::AndIf
                } else {
                    Op::Amp
                }
            }
            '|' => {
                if self.sc.eat('|') {
                    Op::OrIf
                } else {
                    Op::Pipe
                }
            }
            '(' => Op::LParen,
            ')' => Op::RParen,
            '<' => {
                if self.sc.eat('<') {
                    let strip = self.sc.eat('-');
                    let id = self.heredocs.len();
                    // Reserve the slot now; the body is filled in at the next
                    // newline, once the delimiter word has been scanned.
                    self.heredocs.push(Word::default());
                    self.awaiting = Some((id, strip));
                    Op::DLess(id)
                } else if self.sc.eat('&') {
                    Op::LessAnd
                } else if self.sc.eat('>') {
                    Op::LessGreat
                } else {
                    Op::Less
                }
            }
            '>' => {
                if self.sc.eat('>') {
                    Op::DGreat
                } else if self.sc.eat('&') {
                    Op::GreatAnd
                } else if self.sc.eat('|') {
                    Op::Clobber
                } else {
                    Op::Great
                }
            }
            other => return Err(format!("unexpected character {other:?}")),
        };
        Ok(op)
    }

    fn read_raw_line(&mut self) -> Option<String> {
        self.sc.peek()?; // at end of input there is no next line
        let mut line = String::new();
        while let Some(c) = self.sc.bump() {
            if c == '\n' {
                return Some(line);
            }
            line.push(c);
        }
        Some(line)
    }

    fn read_heredoc_bodies(&mut self) -> Syn<()> {
        for p in std::mem::take(&mut self.pending) {
            let mut body = String::new();
            loop {
                let Some(raw) = self.read_raw_line() else {
                    return Err(format!(
                        "{INCOMPLETE}: here-document delimited by `{}`",
                        p.delim
                    ));
                };
                let line = if p.strip_tabs {
                    raw.trim_start_matches('\t')
                } else {
                    raw.as_str()
                };
                if line == p.delim {
                    break;
                }
                body.push_str(line);
                body.push('\n');
            }
            let word = if p.quoted {
                Word(vec![Seg::Quoted(body)])
            } else {
                heredoc_body_word(&body)?
            };
            if let Some(slot) = self.heredocs.get_mut(p.id) {
                *slot = word;
            }
        }
        Ok(())
    }

    fn finish_heredocs(&mut self) -> Syn<()> {
        if self.awaiting.is_some() {
            return Err(format!("{INCOMPLETE}: expected a here-document delimiter"));
        }
        if self.pending.is_empty() {
            return Ok(());
        }
        self.read_heredoc_bodies()
    }

    /// Scan one word. With `stop_at_delims`, blanks and operator characters end
    /// it (normal tokenizing); without, the whole remaining input is the word.
    fn scan_word(&mut self, stop_at_delims: bool) -> Syn<Word> {
        let mut buf = WordBuf::default();
        while let Some(c) = self.sc.peek() {
            if stop_at_delims && is_word_end(c) {
                break;
            }
            match c {
                '\\' => {
                    self.sc.bump();
                    match self.sc.peek() {
                        None => buf.push_quoted('\\'),
                        Some('\n') => {
                            self.sc.bump();
                            self.sc.continued = true;
                        }
                        Some(esc) => {
                            self.sc.bump();
                            buf.push_quoted(esc);
                        }
                    }
                }
                '\'' => {
                    self.sc.bump();
                    let before = buf.segs.len();
                    loop {
                        match self.sc.bump() {
                            None => return Err(format!("{INCOMPLETE}: unmatched `'`")),
                            Some('\'') => break,
                            Some(ch) => buf.push_quoted(ch),
                        }
                    }
                    buf.mark_empty_quote(before);
                }
                '"' => {
                    self.sc.bump();
                    let before = buf.segs.len();
                    self.scan_double(&mut buf)?;
                    buf.mark_empty_quote(before);
                }
                '`' => {
                    self.sc.bump();
                    let code = self.scan_backtick()?;
                    buf.push_seg(Seg::Cmd {
                        code,
                        quoted: false,
                    });
                }
                '$' => self.scan_dollar(&mut buf, false)?,
                other => {
                    self.sc.bump();
                    buf.push_lit(other);
                }
            }
        }
        Ok(buf.finish())
    }

    /// Body of `$((...))`: double-quote rules for `$`, backtick and backslash,
    /// but no quoting — every other character, quote marks included, is literal.
    fn scan_arith_body(&mut self) -> Syn<Word> {
        let mut buf = WordBuf::default();
        while let Some(c) = self.sc.peek() {
            match c {
                '\\' => {
                    self.sc.bump();
                    match self.sc.peek() {
                        Some(esc @ ('$' | '`' | '"' | '\\')) => {
                            self.sc.bump();
                            buf.push_quoted(esc);
                        }
                        Some('\n') => {
                            self.sc.bump();
                        }
                        _ => buf.push_quoted('\\'),
                    }
                }
                '`' => {
                    self.sc.bump();
                    let code = self.scan_backtick()?;
                    buf.push_seg(Seg::Cmd { code, quoted: true });
                }
                '$' => self.scan_dollar(&mut buf, true)?,
                other => {
                    self.sc.bump();
                    buf.push_quoted(other);
                }
            }
        }
        Ok(buf.finish())
    }

    /// Body of a `"..."`; the opening quote is already consumed.
    fn scan_double(&mut self, buf: &mut WordBuf) -> Syn<()> {
        loop {
            let Some(c) = self.sc.peek() else {
                return Err(format!("{INCOMPLETE}: unmatched `\"`"));
            };
            match c {
                '"' => {
                    self.sc.bump();
                    return Ok(());
                }
                '\\' => {
                    self.sc.bump();
                    match self.sc.peek() {
                        // Inside double quotes a backslash only escapes these.
                        Some(esc @ ('$' | '`' | '"' | '\\')) => {
                            self.sc.bump();
                            buf.push_quoted(esc);
                        }
                        Some('\n') => {
                            self.sc.bump();
                        }
                        _ => buf.push_quoted('\\'),
                    }
                }
                '`' => {
                    self.sc.bump();
                    let code = self.scan_backtick()?;
                    buf.push_seg(Seg::Cmd { code, quoted: true });
                }
                '$' => self.scan_dollar(buf, true)?,
                other => {
                    self.sc.bump();
                    buf.push_quoted(other);
                }
            }
        }
    }

    /// A `$`-expansion. `in_dq` marks it as appearing inside double quotes, so
    /// its result is neither field-split nor globbed.
    fn scan_dollar(&mut self, buf: &mut WordBuf, in_dq: bool) -> Syn<()> {
        self.sc.bump(); // '$'
        let push_dollar = |buf: &mut WordBuf| {
            if in_dq {
                buf.push_quoted('$')
            } else {
                buf.push_lit('$')
            }
        };
        let Some(c) = self.sc.peek() else {
            push_dollar(buf);
            return Ok(());
        };
        match c {
            '{' => {
                self.sc.bump();
                let inner = self.scan_braced()?;
                let param = parse_param(&inner, in_dq, self.depth)?;
                buf.push_seg(Seg::Param(Box::new(param)));
            }
            '(' => {
                if self.sc.peek_at(1) == Some('(') {
                    self.sc.bump();
                    self.sc.bump();
                    let text = self.scan_arith()?;
                    buf.push_seg(Seg::Arith {
                        expr: arith_from_str_at(&text, self.depth + 1)?,
                        quoted: in_dq,
                    });
                } else {
                    self.sc.bump();
                    let code = self.scan_paren_body()?;
                    buf.push_seg(Seg::Cmd {
                        code,
                        quoted: in_dq,
                    });
                }
            }
            c if is_special_param(c) => {
                self.sc.bump();
                buf.push_seg(Seg::Param(Box::new(Param {
                    name: c.to_string(),
                    op: None,
                    quoted: in_dq,
                })));
            }
            c if c.is_ascii_digit() => {
                self.sc.bump();
                buf.push_seg(Seg::Param(Box::new(Param {
                    name: c.to_string(),
                    op: None,
                    quoted: in_dq,
                })));
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                let mut name = String::new();
                while let Some(c) = self.sc.peek() {
                    if c.is_ascii_alphanumeric() || c == '_' {
                        name.push(c);
                        self.sc.bump();
                    } else {
                        break;
                    }
                }
                buf.push_seg(Seg::Param(Box::new(Param {
                    name,
                    op: None,
                    quoted: in_dq,
                })));
            }
            _ => push_dollar(buf),
        }
        Ok(())
    }

    /// Text up to the `}` matching an already-consumed `${`.
    fn scan_braced(&mut self) -> Syn<String> {
        let mut out = String::new();
        let mut depth = 1usize;
        loop {
            let Some(c) = self.sc.bump() else {
                return Err(format!("{INCOMPLETE}: unmatched `${{`"));
            };
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(out);
                    }
                }
                '\\' => {
                    out.push('\\');
                    if let Some(n) = self.sc.bump() {
                        out.push(n);
                    }
                    continue;
                }
                '\'' | '"' => {
                    out.push(c);
                    // Copy the quoted run verbatim so a `}` inside it is not
                    // mistaken for the closing brace.
                    loop {
                        let Some(q) = self.sc.bump() else {
                            return Err(format!("{INCOMPLETE}: unmatched {c:?}"));
                        };
                        out.push(q);
                        if q == '\\' && c == '"' {
                            if let Some(n) = self.sc.bump() {
                                out.push(n);
                            }
                            continue;
                        }
                        if q == c {
                            break;
                        }
                    }
                    continue;
                }
                _ => {}
            }
            out.push(c);
        }
    }

    /// Raw source up to the `)` matching an already-consumed `$(`.
    fn scan_paren_body(&mut self) -> Syn<String> {
        let mut out = String::new();
        let mut depth = 1usize;
        loop {
            let Some(c) = self.sc.bump() else {
                return Err(format!("{INCOMPLETE}: unmatched `$(`"));
            };
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(out);
                    }
                }
                '\\' => {
                    out.push('\\');
                    if let Some(n) = self.sc.bump() {
                        out.push(n);
                    }
                    continue;
                }
                '\'' | '"' | '`' => {
                    out.push(c);
                    loop {
                        let Some(q) = self.sc.bump() else {
                            return Err(format!("{INCOMPLETE}: unmatched {c:?}"));
                        };
                        out.push(q);
                        if q == '\\' && c != '\'' {
                            if let Some(n) = self.sc.bump() {
                                out.push(n);
                            }
                            continue;
                        }
                        if q == c {
                            break;
                        }
                    }
                    continue;
                }
                _ => {}
            }
            out.push(c);
        }
    }

    /// Text up to the `))` matching an already-consumed `$((`.
    fn scan_arith(&mut self) -> Syn<String> {
        let mut out = String::new();
        let mut depth = 1usize;
        loop {
            let Some(c) = self.sc.peek() else {
                return Err(format!("{INCOMPLETE}: unmatched `$((`"));
            };
            match c {
                '(' => {
                    depth += 1;
                    out.push(c);
                    self.sc.bump();
                }
                ')' => {
                    if depth == 1 {
                        if self.sc.peek_at(1) == Some(')') {
                            self.sc.bump();
                            self.sc.bump();
                            return Ok(out);
                        }
                        return Err("bad arithmetic expansion: expected `))`".into());
                    }
                    depth -= 1;
                    out.push(c);
                    self.sc.bump();
                }
                _ => {
                    out.push(c);
                    self.sc.bump();
                }
            }
        }
    }

    /// Body of a `` `...` ``; the opening backquote is already consumed. The
    /// only escapes recognised inside are `` \` ``, `\$` and `\\`.
    fn scan_backtick(&mut self) -> Syn<String> {
        let mut out = String::new();
        loop {
            let Some(c) = self.sc.bump() else {
                return Err(format!("{INCOMPLETE}: unmatched `` ` ``"));
            };
            match c {
                '`' => return Ok(out),
                '\\' => match self.sc.peek() {
                    Some(esc @ ('`' | '$' | '\\')) => {
                        self.sc.bump();
                        out.push(esc);
                    }
                    _ => out.push('\\'),
                },
                other => out.push(other),
            }
        }
    }
}

/// Lex a here-document body: expansions are live, but `"` and `'` are ordinary
/// characters and the result is never field-split, so every literal character
/// is recorded as quoted.
fn heredoc_body_word(body: &str) -> Syn<Word> {
    let mut lx = Lexer {
        sc: Scanner {
            src: body.chars().collect(),
            pos: 0,
            continued: false,
        },
        ended_in_comment: false,
        heredocs: Vec::new(),
        pending: Vec::new(),
        awaiting: None,
        depth: 0,
    };
    let mut buf = WordBuf::default();
    buf.open_quoted();
    while let Some(c) = lx.sc.peek() {
        match c {
            '\\' => {
                lx.sc.bump();
                match lx.sc.peek() {
                    Some(esc @ ('$' | '`' | '\\')) => {
                        lx.sc.bump();
                        buf.push_quoted(esc);
                    }
                    Some('\n') => {
                        lx.sc.bump();
                    }
                    _ => buf.push_quoted('\\'),
                }
            }
            '`' => {
                lx.sc.bump();
                let code = lx.scan_backtick()?;
                buf.push_seg(Seg::Cmd { code, quoted: true });
            }
            '$' => lx.scan_dollar(&mut buf, true)?,
            other => {
                lx.sc.bump();
                buf.push_quoted(other);
            }
        }
    }
    Ok(buf.finish())
}

/// Split the inside of a `${...}` into name, operator and operand word.
fn parse_param(inner: &str, quoted: bool, depth: u32) -> Syn<Param> {
    let chars: Vec<char> = inner.chars().collect();
    if chars.is_empty() {
        return Err("bad substitution: `${}`".into());
    }
    // `${#}` is the parameter `#`; `${#x}` is the length of `x`.
    if chars.first() == Some(&'#') && chars.len() > 1 {
        let rest: String = chars.iter().skip(1).collect();
        if is_name(&rest) || (rest.chars().count() == 1 && is_special_param_name(&rest)) {
            return Ok(Param {
                name: rest,
                op: Some(ParamOp::Length),
                quoted,
            });
        }
    }
    let mut i = 0usize;
    let mut name = String::new();
    match chars.first() {
        Some(c) if c.is_ascii_digit() => {
            while let Some(&c) = chars.get(i) {
                if !c.is_ascii_digit() {
                    break;
                }
                name.push(c);
                i += 1;
            }
        }
        Some(&c) if is_special_param(c) => {
            name.push(c);
            i = 1;
        }
        Some(&c) if c.is_ascii_alphabetic() || c == '_' => {
            while let Some(&c) = chars.get(i) {
                if !(c.is_ascii_alphanumeric() || c == '_') {
                    break;
                }
                name.push(c);
                i += 1;
            }
        }
        _ => return Err(format!("bad substitution: `${{{inner}}}`")),
    }
    if i >= chars.len() {
        return Ok(Param {
            name,
            op: None,
            quoted,
        });
    }
    let colon = chars.get(i) == Some(&':');
    if colon {
        i += 1;
    }
    let Some(&opc) = chars.get(i) else {
        return Err(format!("bad substitution: `${{{inner}}}`"));
    };
    i += 1;
    let doubled = chars.get(i) == Some(&opc) && matches!(opc, '%' | '#');
    if doubled {
        i += 1;
    }
    let operand: String = chars.iter().skip(i).collect();
    let word = word_from_str_at(&operand, depth + 1)?;
    let op = match opc {
        '-' => ParamOp::Default { word, colon },
        '=' => ParamOp::Assign { word, colon },
        '?' => ParamOp::Error { word, colon },
        '+' => ParamOp::Alt { word, colon },
        '%' if !colon => ParamOp::TrimSuffix {
            pat: word,
            longest: doubled,
        },
        '#' if !colon => ParamOp::TrimPrefix {
            pat: word,
            longest: doubled,
        },
        _ => return Err(format!("bad substitution: `${{{inner}}}`")),
    };
    Ok(Param {
        name,
        op: Some(op),
        quoted,
    })
}

fn is_special_param_name(s: &str) -> bool {
    s.chars().next().is_some_and(is_special_param) || s.chars().all(|c| c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(src: &str) -> Syn<Vec<Word>> {
        Ok(tokenize(src)?
            .toks
            .into_iter()
            .filter_map(|t| match t {
                Tok::Word(w) => Some(w),
                _ => None,
            })
            .collect())
    }

    fn nth(ws: &[Word], i: usize) -> Syn<&Word> {
        ws.get(i).ok_or_else(|| format!("no word at index {i}"))
    }

    fn heredoc0(src: &str) -> Syn<Word> {
        tokenize(src)?
            .heredocs
            .into_iter()
            .next()
            .ok_or_else(|| "no here-document".to_string())
    }

    #[test]
    fn splits_words_on_blanks() -> Syn<()> {
        let ws = words("echo hello world")?;
        assert_eq!(ws.len(), 3);
        assert_eq!(nth(&ws, 2)?.plain(), Some("world"));
        Ok(())
    }

    #[test]
    fn single_quotes_are_literal() -> Syn<()> {
        let ws = words("echo '$x'")?;
        assert!(matches!(nth(&ws, 1)?.0.as_slice(), [Seg::Quoted(s)] if s == "$x"));
        Ok(())
    }

    #[test]
    fn double_quotes_keep_expansions() -> Syn<()> {
        let ws = words(r#"echo "a $x b""#)?;
        let w = nth(&ws, 1)?;
        assert_eq!(w.0.len(), 3);
        assert!(matches!(w.0.first(), Some(Seg::Quoted(s)) if s == "a "));
        assert!(matches!(w.0.get(1), Some(Seg::Param(p)) if p.name == "x" && p.quoted));
        Ok(())
    }

    #[test]
    fn empty_quotes_are_a_real_field() -> Syn<()> {
        let ws = words("echo ''")?;
        assert!(matches!(nth(&ws, 1)?.0.as_slice(), [Seg::Quoted(s)] if s.is_empty()));
        Ok(())
    }

    #[test]
    fn io_number_is_recognised_only_when_glued() -> Syn<()> {
        assert!(matches!(tokenize("echo 2>x")?.toks.get(1), Some(Tok::IoNumber(2))));
        assert!(matches!(tokenize("echo 2 >x")?.toks.get(1), Some(Tok::Word(_))));
        Ok(())
    }

    #[test]
    fn heredoc_body_is_collected_after_the_line() -> Syn<()> {
        assert!(matches!(
            tokenize("cat <<EOF\nhi\nEOF\n")?.toks.get(1),
            Some(Tok::Op(Op::DLess(0)))
        ));
        assert!(matches!(heredoc0("cat <<EOF\nhi\nEOF\n")?.0.as_slice(),
            [Seg::Quoted(s)] if s == "hi\n"));
        Ok(())
    }

    #[test]
    fn quoted_heredoc_delimiter_disables_expansion() -> Syn<()> {
        assert!(matches!(heredoc0("cat <<'EOF'\n$x\nEOF\n")?.0.as_slice(),
            [Seg::Quoted(s)] if s == "$x\n"));
        let live = heredoc0("cat <<EOF\n$x\nEOF\n")?;
        assert!(live
            .0
            .iter()
            .any(|s| matches!(s, Seg::Param(p) if p.name == "x")));
        Ok(())
    }

    #[test]
    fn heredoc_dash_strips_leading_tabs() -> Syn<()> {
        assert!(matches!(heredoc0("cat <<-EOF\n\t\thi\n\tEOF\n")?.0.as_slice(),
            [Seg::Quoted(s)] if s == "hi\n"));
        Ok(())
    }

    #[test]
    fn param_ops_parse() -> Syn<()> {
        let ws = words("echo ${x:-def} ${y%%.c} ${#z}")?;
        assert!(matches!(nth(&ws, 1)?.0.first(),
            Some(Seg::Param(p)) if matches!(&p.op, Some(ParamOp::Default { colon: true, .. }))));
        assert!(matches!(nth(&ws, 2)?.0.first(),
            Some(Seg::Param(p)) if matches!(&p.op, Some(ParamOp::TrimSuffix { longest: true, .. }))));
        assert!(matches!(nth(&ws, 3)?.0.first(),
            Some(Seg::Param(p)) if matches!(&p.op, Some(ParamOp::Length))));
        Ok(())
    }

    #[test]
    fn dollar_hash_alone_is_the_count_parameter() -> Syn<()> {
        let ws = words("echo ${#}")?;
        assert!(
            matches!(nth(&ws, 1)?.0.first(), Some(Seg::Param(p)) if p.name == "#" && p.op.is_none())
        );
        Ok(())
    }

    #[test]
    fn deeply_nested_expansion_errors_instead_of_overflowing() {
        // `${x:-${x:-…}}` re-enters `word_from_str` per level; the depth cap must
        // turn a pathological nesting into an error, not a stack overflow.
        let mut s = String::new();
        for _ in 0..500 {
            s.push_str("${x:-");
        }
        s.push('y');
        for _ in 0..500 {
            s.push('}');
        }
        assert!(word_from_str_at(&s, 0).is_err());
        // The arith body shares the cap through the same constructor.
        let mut a = String::new();
        for _ in 0..500 {
            a.push_str("$((");
        }
        a.push('1');
        for _ in 0..500 {
            a.push_str("))");
        }
        assert!(arith_from_str_at(&a, 0).is_err());
    }

    #[test]
    fn an_arith_body_quotes_nothing() -> Syn<()> {
        // No corpus case covers this, so pin it here: inside `$(( … ))` neither
        // quote character quotes, and a backslash keeps its double-quote meaning
        // (literal before anything but `$`, backtick, `"`, `\`). Every one of these
        // reaches the arithmetic lexer, which rejects them — as dash does.
        for src in ["'1' + 2", "\"1\" + 2", "1 \\+ 2", "\\1"] {
            let w = arith_from_str_at(src, 0)?;
            let text: String = w
                .0
                .iter()
                .map(|s| match s {
                    Seg::Lit(t) | Seg::Quoted(t) => t.as_str(),
                    _ => "",
                })
                .collect();
            assert_eq!(text, src);
        }
        // The expansions a double-quoted body still performs are unaffected.
        let w = arith_from_str_at("$x + `echo 1`", 0)?;
        assert!(w.0.iter().any(|s| matches!(s, Seg::Param(_))));
        assert!(w.0.iter().any(|s| matches!(s, Seg::Cmd { .. })));
        Ok(())
    }

    #[test]
    fn nested_command_substitution_scans_to_the_right_paren() -> Syn<()> {
        let ws = words("echo $(echo $(echo x))y")?;
        let w = nth(&ws, 1)?;
        assert!(matches!(w.0.first(), Some(Seg::Cmd { code, .. }) if code == "echo $(echo x)"));
        assert!(matches!(w.0.get(1), Some(Seg::Lit(s)) if s == "y"));
        Ok(())
    }

    #[test]
    fn arithmetic_scans_to_the_double_paren() -> Syn<()> {
        let ws = words("echo $(( (1 + 2) * 3 ))")?;
        assert!(matches!(nth(&ws, 1)?.0.first(), Some(Seg::Arith { .. })));
        Ok(())
    }

    #[test]
    fn comments_run_to_end_of_line() -> Syn<()> {
        let ws = words("echo a # not a word\necho b")?;
        let texts: Vec<String> = ws
            .iter()
            .map(|w| w.plain().unwrap_or_default().to_string())
            .collect();
        assert_eq!(texts, vec!["echo", "a", "echo", "b"]);
        Ok(())
    }

    #[test]
    fn hash_inside_a_word_is_literal() -> Syn<()> {
        let ws = words("echo a#b")?;
        assert_eq!(nth(&ws, 1)?.plain(), Some("a#b"));
        Ok(())
    }

    #[test]
    fn line_continuation_joins_words() -> Syn<()> {
        let ws = words("echo a\\\nb")?;
        assert_eq!(nth(&ws, 1)?.plain(), Some("ab"));
        Ok(())
    }

    #[test]
    fn unterminated_quote_reports_incomplete_input() -> Syn<()> {
        match tokenize("echo 'abc") {
            Err(e) => {
                assert!(e.starts_with(INCOMPLETE), "{e}");
                Ok(())
            }
            Ok(_) => Err("an unterminated quote must not tokenize".into()),
        }
    }
}

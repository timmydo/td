//! Tokenizer: source text -> operator/word tokens, with here-document bodies
//! collected as they are passed on the input.
//!
//! Words are scanned into `Seg`s here (quotes, `$name`, `${...}`, `$(...)`,
//! `` `...` ``, `$((...))`) so the parser never re-reads characters and the
//! expander never re-guesses what was quoted. Reserved words are NOT recognized
//! here: whether `in` is a keyword or an argument depends on grammar position,
//! which only the parser knows.

use crate::ast::{is_name, Param, ParamOp, Seg, Syn, SynErr, Word};

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
    /// `&>` — both streams to one file. One token only when the `>` is GLUED to
    /// the `&`; spaced, it stays the background operator plus a redirect.
    AmpGreat,
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
                | Op::AmpGreat
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
            Op::AmpGreat => "&>",
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
    pub toks: Vec<Placed>,
    /// Here-document bodies, indexed by the id carried in `Op::DLess`.
    pub heredocs: Vec<Word>,
    /// The input ended inside a `#` comment. Only alias substitution cares: a
    /// replacement that trails off in a comment must swallow the rest of the
    /// line it was written on, which is text the replacement does not contain.
    pub ended_in_comment: bool,
    /// The delimiter of a here-document whose body ran out with the input,
    /// having ended there rather than at a delimiter line. Only alias
    /// substitution cares, and for the reason `ended_in_comment` does: its text
    /// is SPLICED into an input that continues, so a body that ran off the end
    /// of the replacement belongs to lines this scan never held.
    pub heredoc_ran_out: Option<String>,
    /// What stopped the scan, if anything. `toks` is then the valid prefix before
    /// it, which the parser runs before reporting: a shell lexes as it parses, so
    /// the commands ahead of a bad quote have already run when it is diagnosed.
    pub error: Option<SynErr>,
}

/// A here-document whose operator has been seen but whose body has not been
/// read yet (the body starts on the line *after* the operator).
struct Pending {
    id: usize,
    delim: String,
    quoted: bool,
    strip_tabs: bool,
    /// Body lines read so far. Kept HERE rather than in a local so that a scan
    /// which runs out of input part way through a body resumes after the lines
    /// it already consumed, instead of re-reading them on every refill.
    body: String,
    /// The script line the body's FIRST line is on, 0 until one is read. A
    /// `$( )` in the body counts on from it, as it does in dash.
    body_line: u32,
}

struct Scanner {
    src: Vec<char>,
    pos: usize,
    /// 1-based input line of `pos`, counted as characters are consumed rather
    /// than by scanning back over `src`: a token's line is asked for once per
    /// token, and counting from the top each time is quadratic in a long script.
    line: u32,
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
            self.line = self.line.saturating_add(u32::from(c == Some('\n')));
        }
        c
    }

    /// Deliberately `bump` rather than a second `pos += 1`: it is the ONE place
    /// input is consumed, so the line count cannot be bypassed by a caller that
    /// happens to eat a newline through here.
    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.bump();
            true
        } else {
            false
        }
    }

    /// Consume a `\<newline>` line continuation, recording that the input is
    /// mid-line if it stops here.
    fn fold_continuation(&mut self) {
        self.bump();
        self.fold_tail();
    }

    /// The same, for a caller that has already consumed the backslash -- the
    /// one scan that matches on it before knowing what follows.
    fn fold_tail(&mut self) {
        self.bump();
        self.continued = true;
    }

    /// The index of the next character at or after `i`, stepping over any
    /// `\<newline>` folds. For the READ-ONLY lookaheads, which cannot consume:
    /// ash never sees a fold there either, its own lookahead reading through
    /// `pgetc_eatbnl` and pushing back one character.
    fn past_folds(&self, mut i: usize) -> usize {
        while self.src.get(i) == Some(&'\\') && self.src.get(i + 1) == Some(&'\n') {
            i += 2;
        }
        i
    }

    /// Consume any `\<newline>` folds at the cursor. ash eats them at the READ
    /// -- `pgetc_top` (ash.c:11130) is `pgetc_eatbnl` for every syntax but the
    /// single-quoted one -- so they are invisible to the scanners rather than
    /// something each has to look for. This shell folds at the points that
    /// need it instead, and this is the shared one.
    fn skip_folds(&mut self) {
        while self.peek() == Some('\\') && self.peek_at(1) == Some('\n') {
            self.fold_continuation();
        }
    }

    /// `eat`, with folds spent first: ash's `pgetc_eatbnl`, which every second
    /// character of an operator is read through. A fold is NOT given back when
    /// the character then fails to match -- ash's `pungetc()` hands back one
    /// character, and the one it hands back is the one AFTER the fold.
    fn eat_folded(&mut self, c: char) -> bool {
        self.skip_folds();
        self.eat(c)
    }
}

/// Characters that end a word without being part of it.
fn is_word_end(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '|' | '&' | ';' | '<' | '>' | '(' | ')')
}

/// Where a `case` has got to. A `)` that ends a PATTERN closes nothing, and
/// that is the one thing a `)` token cannot say for itself: ash knows because
/// its parser reads the body, and this is that rule at the only place the scan
/// needs it.
enum CaseAt {
    /// `case` is read, the word it selects on is not.
    Subject,
    /// The subject is read, `in` is not. Until `in` arrives this is a CANDIDATE
    /// only -- `echo $(case)` is a word and a closer -- so a paren still counts.
    In,
    /// Where a pattern could START. `esac` HERE ends the case -- ash's
    /// `parsecase` breaks on it wherever a pattern may begin, so a bare `esac`
    /// is not a pattern in ash or bash, both measured.
    PatternStart,
    /// Inside a pattern list, past the optional `(` or a `|`, where `esac` is
    /// an ordinary word: `case esac in (esac) …` matches.
    PatternWords,
    /// An arm's commands, until `;;` starts another pattern or `esac` ends it.
    Body,
}

/// Whether the case walk consumed a token or left it to the paren rules.
enum Took {
    Whole,
    Fall,
}

/// Words after which a command can START, so a `case` following one opens a
/// case statement rather than being an argument. `is_word_end` cannot answer
/// this: it is about characters, and this is about position.
const OPENS_COMMAND: &[&str] = &["do", "then", "else", "elif", "{", "!", "if", "while", "until"];

/// Advance the case walk by one token. `depth` pins each open `case` to the
/// paren depth it began at, so a `)` inside a subshell in an arm's body is
/// still an ordinary closer.
fn step_case(
    cases: &mut Vec<(CaseAt, usize)>,
    tok: &Tok,
    plain: Option<&str>,
    depth: usize,
    at_cmd: bool,
) -> Took {
    // Never consumed: the caller has to see the end of input to report it.
    if matches!(tok, Tok::Eof) {
        return Took::Fall;
    }
    let mut decided = None;
    let mut pop = false;
    if let Some((state, base)) = cases.last_mut() {
        let at_base = *base == depth;
        match state {
            CaseAt::Subject => {
                decided = Some(match tok {
                    Tok::Newline => Took::Whole,
                    Tok::Word(_) => {
                        *state = CaseAt::In;
                        Took::Whole
                    }
                    _ => {
                        pop = true;
                        Took::Fall
                    }
                })
            }
            CaseAt::In => {
                decided = Some(match (tok, plain) {
                    (Tok::Newline, _) => Took::Whole,
                    (_, Some("in")) => {
                        *state = CaseAt::PatternStart;
                        Took::Whole
                    }
                    _ => {
                        pop = true;
                        Took::Fall
                    }
                })
            }
            // Everything here is the pattern's, the leading `(` included -- so
            // neither paren moves the depth this arm is pinned to.
            CaseAt::PatternStart if at_base => {
                decided = Some(match (tok, plain) {
                    (_, Some("esac")) => {
                        pop = true;
                        Took::Whole
                    }
                    (Tok::Op(Op::RParen), _) => {
                        *state = CaseAt::Body;
                        Took::Whole
                    }
                    (Tok::Newline, _) => Took::Whole,
                    _ => {
                        *state = CaseAt::PatternWords;
                        Took::Whole
                    }
                })
            }
            CaseAt::PatternWords if at_base => {
                decided = Some(match tok {
                    Tok::Op(Op::RParen) => {
                        *state = CaseAt::Body;
                        Took::Whole
                    }
                    _ => Took::Whole,
                })
            }
            CaseAt::Body if at_base => {
                if matches!(tok, Tok::Op(Op::DSemi)) {
                    *state = CaseAt::PatternStart;
                    decided = Some(Took::Whole);
                } else if at_cmd && plain == Some("esac") {
                    pop = true;
                    decided = Some(Took::Whole);
                }
            }
            _ => {}
        }
    }
    if pop {
        cases.pop();
    }
    if let Some(t) = decided {
        return t;
    }
    if at_cmd && plain == Some("case") {
        cases.push((CaseAt::Subject, depth));
        return Took::Whole;
    }
    Took::Fall
}

/// Lex forward to the `)` closing an open `$(`, returning where the BODY ends
/// -- before the blanks ahead of that `)`, which belong to neither -- and where
/// the `)` itself does.
fn close_paren(lx: &mut Lexer) -> Syn<(usize, usize)> {
    let mut depth = 1usize;
    let mut cases = Vec::new();
    let mut at_cmd = true;
    let mut open_paren = false;
    loop {
        let before = lx.sc.pos;
        let tok = lx.next_tok()?;
        let plain = match &tok {
            Tok::Word(w) => w.plain(),
            _ => None,
        };
        let took = step_case(&mut cases, &tok, plain, depth, at_cmd);
        // A `)` that closes an empty `(` is a function HEADER's, after which the
        // body -- a compound command, `case` among them -- begins with no
        // separator. Only where the case walk left the parens alone.
        let header = open_paren && matches!((&took, &tok), (Took::Fall, Tok::Op(Op::RParen)));
        open_paren = matches!((&took, &tok), (Took::Fall, Tok::Op(Op::LParen)));
        let was_cmd = at_cmd;
        at_cmd = header
            || matches!(
            tok,
            Tok::Newline
                | Tok::Op(
                    Op::Semi
                        | Op::DSemi
                        | Op::Amp
                        | Op::AndIf
                        | Op::OrIf
                        | Op::Pipe
                        | Op::LParen
                )
        ) || (was_cmd && plain.is_some_and(|w| OPENS_COMMAND.contains(&w)))
            // The only `)` the case walk consumes is a pattern's, and an arm's
            // commands begin right after it -- with no separator between, so
            // nothing else here would say a command can start.
            || matches!((&took, &tok), (Took::Whole, Tok::Op(Op::RParen)));
        if matches!(took, Took::Whole) {
            continue;
        }
        match tok {
            Tok::Op(Op::LParen) => depth = depth.saturating_add(1),
            Tok::Op(Op::RParen) => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Ok((before, lx.sc.pos));
                }
            }
            Tok::Eof => return Err(SynErr::incomplete("syntax error: unexpected end of file (expecting \")\")")),
            _ => {}
        }
    }
}

fn is_special_param(c: char) -> bool {
    matches!(c, '@' | '*' | '#' | '?' | '-' | '$' | '!')
}

/// What a numeric `$'...'` escape named. The distinction is the whole reason
/// that scan accumulates bytes: `\xNN` and `\NNN` name a BYTE, so a pair of
/// them can spell one character, while `\uNNNN` names a code point outright.
enum Esc {
    Byte(u32),
    Point(u32),
}

fn push_utf8(bytes: &mut Vec<u8>, c: char) {
    bytes.extend_from_slice(c.encode_utf8(&mut [0u8; 4]).as_bytes());
}

/// Evaluate the body of a `$'...'`, whose extent the lexer has already found.
/// Bytes rather than characters, because `\xNN` and `\NNN` name a byte: two of
/// them can spell one character, and decoding each in isolation would turn
/// `$'\xc3\xa9'` into `Ã©`.
fn decode_ansi_c(body: &[char]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(body.len());
    // A NUL ends the STRING, not the walk: the rest is still consumed so the
    // escapes in it cannot be mistaken for anything else.
    let mut done = false;
    let mut i = 0;
    while let Some(&c) = body.get(i) {
        i += 1;
        if c != '\\' {
            if !done {
                push_utf8(&mut bytes, c);
            }
            continue;
        }
        // A trailing backslash cannot occur -- the scan pairs it with the
        // character it escapes -- but it is a literal if it ever does.
        let Some(&esc) = body.get(i) else {
            if !done {
                push_utf8(&mut bytes, c);
            }
            break;
        };
        i += 1;
        let simple = match esc {
            'a' => Some('\u{7}'),
            'b' => Some('\u{8}'),
            'e' | 'E' => Some('\u{1b}'),
            'f' => Some('\u{c}'),
            'n' => Some('\n'),
            'r' => Some('\r'),
            't' => Some('\t'),
            'v' => Some('\u{b}'),
            '\\' => Some('\\'),
            '\'' => Some('\''),
            '"' => Some('"'),
            '?' => Some('?'),
            _ => None,
        };
        if let Some(ch) = simple {
            if !done {
                push_utf8(&mut bytes, ch);
            }
            continue;
        }
        // `\cX` is a control character: the operand masked to five bits, which
        // makes `$'\cA'` a 0x01 and folds case for free. NOT `^ 0x40`, which
        // agrees on letters and parts company on everything else -- `$'\c0'` is
        // 0x10, not 0x70. Handled apart from the numeric escapes because it can
        // yield more than one byte.
        if esc == 'c' {
            let Some(&ch) = body.get(i) else {
                // No operand at all: bash leaves `\c` as those two characters.
                if !done {
                    push_utf8(&mut bytes, '\\');
                    push_utf8(&mut bytes, 'c');
                }
                continue;
            };
            i += 1;
            // The operand is taken RAW -- `$'\c\a'` is Ctrl-\ then a literal
            // `a`, not Ctrl-\a. The one exception is a backslash operand, which
            // is written doubled, so a second one belongs to it.
            if ch == '\\' && body.get(i) == Some(&'\\') {
                i += 1;
            }
            // bash masks the operand's FIRST BYTE and leaves any others alone,
            // so a multi-byte operand keeps its tail: `$'\cé'` is 0x03 then a
            // stray 0xa9.
            if let Some((&first, rest)) = ch.encode_utf8(&mut [0u8; 4]).as_bytes().split_first() {
                // Ctrl-? is DEL by convention rather than the mask's 0x1f.
                let ctl = if ch == '?' { 0x7f } else { first & 0x1f };
                if ctl == 0 {
                    done = true;
                } else if !done {
                    bytes.push(ctl);
                    bytes.extend_from_slice(rest);
                }
            }
            continue;
        }
        // The numeric forms. Each stops at its own digit count, so `$'\x41B'`
        // is "AB" rather than a three-digit hex read.
        let decoded = match esc {
            // `\x{...}` is bash's ksh93-compatible brace form and takes any
            // number of digits, closing brace optional. It is a BYTE like the
            // bare form -- `\x{263a}` is masked to 0x3a, not a code point -- and
            // `\x{}` names zero, which truncates. There is no such form for
            // `\u`/`\U`, where `$'\u{41}'` stays literal.
            'x' if body.get(i) == Some(&'{') => {
                i += 1;
                let mut n: u32 = 0;
                while let Some(d) = body.get(i).and_then(|c| c.to_digit(16)) {
                    i += 1;
                    n = n.saturating_mul(16).saturating_add(d);
                }
                if body.get(i) == Some(&'}') {
                    i += 1;
                }
                Some(Esc::Byte(n))
            }
            'x' => take_digits(body, &mut i, 16, 2).map(Esc::Byte),
            'u' => take_digits(body, &mut i, 16, 4).map(Esc::Point),
            'U' => take_digits(body, &mut i, 16, 8).map(Esc::Point),
            '0'..='7' => {
                // The first digit was already taken; bash reads three in all.
                let mut n = esc.to_digit(8).unwrap_or(0);
                for _ in 0..2 {
                    let Some(d) = body.get(i).and_then(|c| c.to_digit(8)) else {
                        break;
                    };
                    i += 1;
                    n = n.saturating_mul(8).saturating_add(d);
                }
                Some(Esc::Byte(n))
            }
            _ => None,
        };
        match decoded {
            Some(Esc::Byte(n)) => {
                // Three octal digits can name 0x1ff, which is not a byte; bash
                // keeps the low eight bits, so `$'\400'` is a NUL and
                // truncates. Mask BEFORE that test, or it does not.
                let b = (n & 0xff) as u8;
                if b == 0 {
                    done = true;
                } else if !done {
                    bytes.push(b);
                }
            }
            // A `\u` names a code point. One that is not a scalar value -- a
            // surrogate, or past U+10FFFF -- has no representation here and
            // takes the same replacement an undecodable byte does.
            Some(Esc::Point(0)) => done = true,
            // Above 0x7fffffff bash emits NOTHING rather than any encoding of
            // it, so a replacement character here would be a character bash
            // does not produce at all.
            Some(Esc::Point(n)) if n > 0x7fff_ffff => {}
            Some(Esc::Point(n)) => {
                if !done {
                    push_utf8(&mut bytes, char::from_u32(n).unwrap_or(char::REPLACEMENT_CHARACTER));
                }
            }
            None => {
                // An unclaimed escape keeps BOTH characters.
                if !done {
                    push_utf8(&mut bytes, '\\');
                    push_utf8(&mut bytes, esc);
                }
            }
        }
    }
    bytes
}

/// Up to `max` digits in `radix` from `*i`, advancing it past them. `None` when
/// there is not even one, which is what leaves `$'\x'` a literal `\x`.
fn take_digits(body: &[char], i: &mut usize, radix: u32, max: usize) -> Option<u32> {
    let mut value: Option<u32> = None;
    for _ in 0..max {
        let Some(d) = body.get(*i).and_then(|c| c.to_digit(radix)) else {
            break;
        };
        *i += 1;
        value = Some(value.unwrap_or(0).saturating_mul(radix).saturating_add(d));
    }
    value
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

/// A backtick body is DE-ESCAPED before it is parsed, so dash re-scans it as a
/// string of its own and numbers it from 1 -- measured: a `` ` `` opening on
/// line 6 with its command on line 7 reports 2, where the same body in `$( )`
/// reports 7. bash numbers both absolutely; this shell follows dash.
const BACKTICK_LINE: u32 = 1;

/// Cap on nested expansion re-lexing (`${a:-${b:-${c:-…}}}`, `$(( ${x} ))`), which
/// re-enters `word_from_str_at`/`arith_from_str_at` per level. Bounds the recursion so
/// a pathological input errors instead of overflowing the stack. Well above any real
/// script's nesting.
const MAX_EXPANSION_DEPTH: u32 = 100;

/// Lex all of `src`, failing on the first error. For text that must be whole to
/// mean anything at all -- an alias replacement, an expansion operand.
/// `line` numbers the text's first line. Alias substitution is why it is a
/// parameter: dash reads a replacement from the input stream at the point the
/// NAME stood, so a `$( )` inside one reports the invocation's line and a body
/// with a newline in it reports the line after.
pub fn tokenize(src: &str, line: u32) -> Syn<Lexed> {
    let lexed = tokenize_prefix(src, line);
    match lexed.error {
        Some(e) => Err(e),
        None => Ok(lexed),
    }
}

/// Lex as much of `src` as scans, reporting what stopped it in `Lexed::error`.
pub fn tokenize_prefix(src: &str, line: u32) -> Lexed {
    let mut scan = Scan::new_at(src, line);
    scan.seal();
    let chunk = scan.pull();
    Lexed {
        toks: chunk.toks,
        heredocs: scan.take_heredocs(),
        ended_in_comment: scan.ended_in_comment(),
        heredoc_ran_out: scan.lx.heredoc_ran_out.take(),
        error: chunk.error,
    }
}

/// What one `pull` produced. `incomplete` means the scan stopped because the
/// input ran out INSIDE a construct and more of it could finish the job -- the
/// scanner has been rewound to the last token boundary, so feeding more text and
/// pulling again re-scans only the token that was open.
pub struct Chunk {
    pub toks: Vec<Placed>,
    pub incomplete: bool,
    pub error: Option<SynErr>,
}

/// A token and the input line it starts on. One value rather than two parallel
/// vectors because the parser splices alias replacements INTO this stream, and a
/// second vector to keep in step there is a divergence waiting to happen.
#[derive(Clone, Debug)]
pub struct Placed {
    pub line: u32,
    pub tok: Tok,
}

/// A lexer that can be fed more input and asked for more tokens, so a script
/// arriving a line at a time is scanned ONCE rather than from the top per line.
/// Sealing it says no more is coming, which is what turns an unfinished
/// construct from "ask for another line" into the syntax error it turns out to
/// be.
pub struct Scan {
    lx: Lexer,
    sealed: bool,
}

impl Scan {
    pub fn new(src: &str) -> Scan {
        Scan::new_at(src, 1)
    }

    /// A scan whose first line is numbered `line`. `$(...)` needs it: dash
    /// parses that body from the SAME input, so a `$LINENO` inside one is the
    /// outer script's line, not an offset into text with no line of its own.
    pub fn new_at(src: &str, line: u32) -> Scan {
        Scan {
            lx: Lexer {
                sc: Scanner {
                    src: src.chars().collect(),
                    pos: 0,
                    line,
                    continued: false,
                },
                ended_in_comment: false,
                sealed: false,
                resumable: false,
                heredoc_ran_out: None,
                owed_newline: false,
                committed: false,
                fatal: false,
                heredocs: Vec::new(),
                pending: Vec::new(),
                awaiting: None,
                depth: 0,
                cond_depth: 0,
                regex_next: false,
                cond_continues: false,
                cmd_position: true,
                regex_word: false,
                in_braces: false,
            },
            sealed: false,
        }
    }

    /// Append more input. Only meaningful before `seal`.
    pub fn feed(&mut self, text: &str) {
        self.lx.sc.src.extend(text.chars());
    }

    pub fn seal(&mut self) {
        self.sealed = true;
        self.lx.sealed = true;
    }

    /// Mark the text a SNAPSHOT of a source that can still be asked for more.
    /// Only the interactive reader needs it: its buffer is whole at every probe
    /// and grows between them, so `sealed` alone would say the input had ended.
    pub fn resumable(&mut self) {
        self.lx.resumable = true;
    }

    pub fn ended_in_comment(&self) -> bool {
        self.lx.ended_in_comment
    }

    pub fn take_heredocs(&mut self) -> Vec<Word> {
        std::mem::take(&mut self.lx.heredocs)
    }

    /// Here-document bodies collected so far, in `Op::DLess` id order.
    pub fn heredocs(&self) -> &[Word] {
        &self.lx.heredocs
    }

    /// Whether here-document `id` is still waiting for its body. A placeholder
    /// is pushed when `<<` is scanned and filled in at the end of the line, so
    /// an unfilled slot is indistinguishable from a legitimately EMPTY body --
    /// the pending list is the only thing that can tell them apart.
    pub fn heredoc_pending(&self, id: usize) -> bool {
        self.lx.awaiting.is_some_and(|(a, _)| a == id)
            || self.lx.pending.iter().any(|p| p.id == id)
    }

    /// Adopt bodies scanned elsewhere -- an alias replacement is lexed on its
    /// own, and its here-documents have to live in the SAME table as this
    /// source's or the two id spaces collide. Returns the base its ids move to.
    pub fn push_heredocs(&mut self, more: Vec<Word>) -> usize {
        let base = self.lx.heredocs.len();
        self.lx.heredocs.extend(more);
        base
    }

    /// Every token available from the input fed so far.
    pub fn pull(&mut self) -> Chunk {
        let mut toks = Vec::new();
        loop {
            // Taken BEFORE each attempt, because a `next_tok` that fails part way
            // can already have consumed input, armed `awaiting`, or pushed a body.
            let mark = self.lx.mark();
            // Cleared per attempt: it means "this token banked input a rewind
            // must not undo", and a stale one from an earlier token would skip
            // the rewind for an unrelated unfinished construct.
            self.lx.committed = false;
            match self.lx.next_tok() {
                Ok(Tok::Eof) if !self.sealed => {
                    self.lx.restore(mark);
                    return Chunk { toks, incomplete: true, error: None };
                }
                Ok(tok) => {
                    let last = matches!(tok, Tok::Eof);
                    // The line the scanner is on once the token has been
                    // READ, which is dash's: `savelinno` is taken at the top of
                    // `simplecmd` (parser.c:524) with the first token already
                    // pushed back. It differs from the line the token opens on
                    // only when the token itself spans lines, and there dash
                    // reports the later one -- `x="a\nb" y=$LINENO` is 2.
                    toks.push(Placed { line: self.lx.sc.line, tok });
                    if last {
                        return Chunk { toks, incomplete: false, error: None };
                    }
                }
                // Unsealed, an unfinished construct is a request for more input
                // rather than an error: rewind so the open token is scanned once,
                // whole, when the rest of it arrives.
                Err(e) if !self.sealed && !self.lx.fatal && e.is_incomplete() => {
                    // A rewind would throw away here-document lines already
                    // consumed, and re-reading them on every refill is what
                    // makes a large body quadratic. Progress stays; the scan is
                    // resumable from exactly where it stopped.
                    if std::mem::take(&mut self.lx.committed) {
                        return Chunk { toks, incomplete: true, error: None };
                    }
                    self.lx.restore(mark);
                    return Chunk { toks, incomplete: true, error: None };
                }
                Err(e) => return Chunk { toks, incomplete: false, error: Some(e) },
            }
        }
    }
}

/// A token boundary the scan can be wound back to. Both vectors are recorded as
/// LENGTHS rather than copies: a rewind only ever has to undo pushes, because
/// the two operations that shorten `pending` or extend a body -- consuming an
/// entry and banking a line -- each set `committed`, and `pull` does not rewind
/// once that is set. Cloning `pending` instead would copy the accumulated body
/// at every token, which is quadratic in the length of a here-document.
struct Mark {
    pos: usize,
    line: u32,
    continued: bool,
    ended_in_comment: bool,
    awaiting: Option<(usize, bool)>,
    owed_newline: bool,
    pending: usize,
    heredocs: usize,
}

/// Scan `text` as a single word: blanks and operator characters are ordinary
/// literal characters. Used for the operand of `${x:-...}`, which is delimited by
/// its brace, not by blanks. A nested `${…}` operand re-enters here one level
/// deeper; the depth cap is checked before any scanning so the mutual recursion
/// `scan_dollar -> parse_braced -> word_from_str_at` is bounded.
fn word_from_str_at(text: &str, depth: u32, line: u32) -> Syn<Word> {
    let mut lx = lexer_over(text, depth, line)?;
    lx.in_braces = true;
    lx.scan_word(false)
}

/// Scan a whole runtime string as if it were the body of a `"..."` -- dash's
/// `expandstr`, which parses `$PS4` with DQSYNTAX and expands it EXP_QUOTED.
/// There is no enclosing quote to close, so a `'` or `"` in the value is an
/// ordinary character rather than the start of a quote that never ends.
///
/// Not QUITE a double-quoted body, though: dash scans it against a fake end
/// marker, and its backslash guard (parser.c:951) spares `\"` when one is set,
/// so `a\"b` traces with the backslash still on. `\$`, `` \` `` and `\\` lose
/// theirs as they would inside real quotes.
pub fn word_from_str(text: &str) -> Syn<Word> {
    // Line 1: a runtime string (`$PS4`) is not part of the script and has no
    // line of the script's to count from.
    lexer_over(text, 0, 1)?.scan_dq_run(false, false)
}

/// The WORD of `${x-word}` when the whole expansion sits inside double quotes.
/// dash scans it as a double-quoted body, so a `'` is an ordinary character
/// while a `"` still opens a quoted run: `"${u-'c d'}"` keeps its quotes and
/// `"${u-"c d"}"` loses them. The escape set is double-quote's plus `}`, which
/// is special here and nowhere else. The PATTERN operators are not this --
/// there a `'` quotes as usual, the asymmetry `var-sub-quote` grades.
fn dq_word_from_str_at(text: &str, depth: u32, line: u32) -> Syn<Word> {
    let mut lx = lexer_over(text, depth, line)?;
    lx.in_braces = true;
    lx.scan_dq_run(true, true)
}

/// Scan the body of `$((...))`. POSIX expands it as if it were double-quoted,
/// "except that a double-quote inside the expression is not treated specially",
/// so NEITHER quote character quotes here: both reach the arithmetic lexer, which
/// rejects them. That is why `$(( '1' + 2 ))` is an error and not 3.
fn arith_from_str_at(text: &str, depth: u32, line: u32) -> Syn<Word> {
    lexer_over(text, depth, line)?.scan_dq_run(true, false)
}

/// `line` is where `text` begins in the SCRIPT, not in `text`: an operand is a
/// substring of the outer input, and a `$( )` inside one reports the script's
/// line as dash does -- which it only can if the sub-lexer starts counting
/// where the substring starts.
fn lexer_over(text: &str, depth: u32, line: u32) -> Syn<Lexer> {
    if depth > MAX_EXPANSION_DEPTH {
        return Err("expansion nested too deeply".into());
    }
    Ok(Lexer {
        sealed: true,
        resumable: false,
        heredoc_ran_out: None,
        owed_newline: false,
        committed: false,
        fatal: false,
        sc: Scanner {
            src: text.chars().collect(),
            pos: 0,
            line,
            continued: false,
        },
        ended_in_comment: false,
        heredocs: Vec::new(),
        pending: Vec::new(),
        awaiting: None,
        depth,
        cond_depth: 0,
        regex_next: false,
        cond_continues: false,
        cmd_position: true,
        regex_word: false,
        in_braces: false,
    })
}

struct Lexer {
    sc: Scanner,
    ended_in_comment: bool,
    /// Set once no more input can arrive, which is what makes an unterminated
    /// final line the last line rather than half of one still being typed.
    sealed: bool,
    /// The text is a SNAPSHOT whose source can still be asked for more, even
    /// though the snapshot itself is sealed. Only the interactive reader sets
    /// it, and only `more_can_arrive` consults it: `sealed` is a fact about the
    /// string, this is a fact about where the string came from, and they differ
    /// exactly where running out has to mean "ask" rather than "that was all".
    resumable: bool,
    /// Set when a body ended because the input did; see `Lexed::heredoc_ran_out`.
    /// Recorded rather than acted on -- ending the body is right for text that
    /// owns its own end, and only a caller knows whether this text does.
    heredoc_ran_out: Option<String>,
    /// A newline has been consumed but its here-document bodies are not all
    /// read, so the `Tok::Newline` it owes has not been emitted yet.
    owed_newline: bool,
    /// Set when a body line was consumed and recorded. Progress that a rewind
    /// must not undo -- see `Scan::pull`.
    committed: bool,
    /// Set when an error cannot be cured by more input, so `pull` reports it
    /// even though it is spelled as an end-of-input one.
    fatal: bool,
    heredocs: Vec<Word>,
    pending: Vec<Pending>,
    /// Set when a `<<` operator is waiting for its delimiter word.
    awaiting: Option<(usize, bool)>,
    /// Expansion-nesting depth of this (re-)lexing pass; see `MAX_EXPANSION_DEPTH`.
    depth: u32,
    /// Open `[[` brackets. The lexer otherwise knows nothing about the
    /// conditional command -- `[[` and `]]` are plain words to it -- and it is
    /// tracked ONLY so that `=~` is recognised where it is an operator and
    /// nowhere else: `echo =~ a|b` must keep its pipe.
    cond_depth: u32,
    /// The previous token was a `=~` inside `[[ ]]`, so the NEXT word is a
    /// regular expression and lexes by `regex_word`'s rule.
    regex_next: bool,
    /// The last token was one a `[[ ]]` may be CONTINUED past a newline by, so
    /// a newline here does not end the command. Exactly the five positions
    /// `cond_term` accepts one in.
    cond_continues: bool,
    /// A command word could start at the cursor. The lexer does not otherwise
    /// resolve reserved words -- they are positional, which is the parser's
    /// business -- and this is the crudest form of that question, kept only so
    /// that `[[` is counted where it can be the conditional and not where it
    /// is an argument. `echo [[ =~ a|cat` is the case that needs it.
    cmd_position: bool,
    /// Set for exactly the one `scan_word` call that reads a `=~` right-hand
    /// side. bash needs a mode here too, and the corpus says so in as many
    /// words (`regex.test.sh`: "different lexer mode required").
    regex_word: bool,
    /// This lexer is scanning the OPERAND of a `${...}`, so `}` is what ends
    /// the expansion and a backslash before one is consumed rather than kept.
    in_braces: bool,
}

impl Lexer {
    fn mark(&self) -> Mark {
        Mark {
            pos: self.sc.pos,
            line: self.sc.line,
            continued: self.sc.continued,
            ended_in_comment: self.ended_in_comment,
            awaiting: self.awaiting,
            owed_newline: self.owed_newline,
            pending: self.pending.len(),
            heredocs: self.heredocs.len(),
        }
    }

    /// Whether the input can still GROW. Both callers ask it where the text ran
    /// out, and both are the same question: request the rest, or take what is
    /// here.
    fn more_can_arrive(&self) -> bool {
        !self.sealed || self.resumable
    }

    /// A `\<newline>` was the last thing consumed and nothing follows it. While
    /// more input can arrive, that is a request for the rest of the line; once
    /// it cannot, the fold is simply SPENT -- ash reads it through
    /// `pgetc_eatbnl` and then gets PEOF, so `sh -c 'echo x \<newline>'` prints
    /// `x` rather than failing.
    fn fold_wants_more(&self) -> bool {
        self.sc.continued && self.sc.peek().is_none() && self.more_can_arrive()
    }

    fn restore(&mut self, m: Mark) {
        self.sc.pos = m.pos;
        self.sc.line = m.line;
        self.sc.continued = m.continued;
        self.ended_in_comment = m.ended_in_comment;
        self.awaiting = m.awaiting;
        self.owed_newline = m.owed_newline;
        self.pending.truncate(m.pending);
        self.heredocs.truncate(m.heredocs);
    }

    /// The next token, or `Tok::Eof` once the input is spent.
    fn next_tok(&mut self) -> Syn<Tok> {
        loop {
            // A newline whose here-documents are not all read: finish them
            // before anything else, so a scan resumed mid-body continues the
            // body rather than lexing its lines as ordinary tokens.
            if self.owed_newline {
                self.read_heredoc_bodies()?;
                self.owed_newline = false;
                // A newline ends the command unless the conditional was left
                // open at one of the five positions bash continues past.
                if !self.cond_continues {
                    self.end_cond_command();
                }
                self.cmd_position = true;
                return Ok(Tok::Newline);
            }
            self.skip_blanks();
            let Some(c) = self.sc.peek() else {
                if self.fold_wants_more() {
                    return Err(SynErr::incomplete("syntax error: unexpected end of file"));
                }
                self.finish_heredocs()?;
                return Ok(Tok::Eof);
            };
            if c == '\n' {
                self.sc.bump();
                self.owed_newline = true;
                continue;
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
            // The `=~` right-hand side is read as ONE word before `|` and `(`
            // can become operators, which is the only place the shell's own
            // punctuation and a regex's overlap. Everything else in `[[ ]]`
            // still lexes exactly as a command line does.
            // ...but only where an operand can actually begin. `[[ =~ && x ]]`
            // is a conditional whose first operand is the literal word `=~`,
            // and arming the mode there scanned an EMPTY word and failed the
            // parse where bash answers 0.
            if self.regex_next && !matches!(c, '&' | ';' | '<' | '>' | ')') {
                self.regex_next = false;
                self.regex_word = true;
                let word = self.scan_word(true);
                self.regex_word = false;
                return Ok(Tok::Word(word?));
            }
            self.regex_next = false;
            if matches!(c, '|' | '&' | ';' | '<' | '>' | '(' | ')') {
                let op = self.scan_op()?;
                match op {
                    // `&&`, `||` and `(` are the conditional's own connectives
                    // and grouping, so they continue it rather than ending it.
                    Op::AndIf | Op::OrIf | Op::LParen => self.cond_continues = true,
                    Op::Semi | Op::Amp | Op::Pipe => self.end_cond_command(),
                    _ => self.cond_continues = false,
                }
                // A redirection is followed by its target, which is not a
                // command; everything else here separates or groups commands.
                self.cmd_position = !op.is_redirect();
                return Ok(Tok::Op(op));
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
                    body: String::new(),
                    body_line: 0,
                });
                return Ok(Tok::Word(word));
            }
            // `2>file`: digits glued to a redirection operator name the fd.
            // `&` counts only with a `>` glued to IT, since that pair is one
            // operator -- ash's `isdigit_str(out)` test, whose `c` may be the
            // flagged `&>` (ash.c:12707). `2&` alone is still a word and a
            // background operator, which is why the second character is peeked
            // rather than assumed.
            if let Some(text) = word.plain() {
                let starts_redirect = match self.sc.peek() {
                    Some('<') | Some('>') => true,
                    Some('&') => self.sc.peek_at(1) == Some('>'),
                    _ => false,
                };
                if !text.is_empty()
                    && text.chars().all(|c| c.is_ascii_digit())
                    && starts_redirect
                {
                    if let Ok(n) = text.parse::<u32>() {
                        return Ok(Tok::IoNumber(n));
                    }
                }
            }
            self.track_cond(&word);
            return Ok(Tok::Word(word));
        }
    }

    /// The only conditional-command state the lexer keeps, and the whole of it:
    /// where the brackets are open, so that `=~` arms `regex_next` there and a
    /// bare `=~` on a command line does not.
    fn track_cond(&mut self, word: &Word) {
        match word.plain() {
            // Deliberately NOT `cond_continues`: `[[` is the one word that
            // RAISES the count, so letting it also protect that count across a
            // newline is what keeps a stray `echo [[` alive into the next line.
            // The cost is a newline written directly after `[[`, which stops
            // arming the regex mode -- and that costs a glued `|` in a regex in
            // that one shape, where the alternative silently ate a pipe.
            Some("[[") if self.cmd_position => {
                self.cond_depth = self.cond_depth.saturating_add(1)
            }
            Some("]]") => {
                self.cond_depth = self.cond_depth.saturating_sub(1);
                self.cond_continues = false;
            }
            Some("=~") if self.cond_depth > 0 => {
                self.regex_next = true;
                self.cond_continues = false;
            }
            Some("!") => self.cond_continues = true,
            _ => self.cond_continues = false,
        }
        // Only the words a command can FOLLOW keep the position open. The list
        // is the reserved words that introduce one; anything else is a command
        // name or an argument, and what comes after it is an argument.
        self.cmd_position = matches!(
            word.plain(),
            Some("if" | "then" | "else" | "elif" | "do" | "while" | "until" | "{" | "!")
        );
    }

    /// Forget any open bracket, because the command it would have belonged to
    /// has ended. The lexer cannot tell `[[` in COMMAND POSITION from `[[` as
    /// an argument -- that is the parser's job -- so `echo [[` counts one that
    /// no `]]` will ever close, and a later `=~` would then lex its operand as
    /// a regex and swallow a pipe. Bounding the count to a single command is
    /// what makes that a non-event: measured, `echo [[; echo a =~ b|tr a-z A-Z`
    /// piped correctly again. The tokens below cannot occur inside `[[ ]]` at
    /// all; `<` and `>` deliberately are NOT among them, being its string
    /// comparisons.
    fn end_cond_command(&mut self) {
        self.cond_depth = 0;
        self.regex_next = false;
        self.cond_continues = false;
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
            return Err(SynErr::incomplete("syntax error: unexpected end of file"));
        };
        let op = match c {
            ';' => {
                if self.sc.eat_folded(';') {
                    Op::DSemi
                } else {
                    Op::Semi
                }
            }
            '&' => {
                if self.sc.eat_folded('&') {
                    Op::AndIf
                } else if self.sc.eat_folded('>') {
                    Op::AmpGreat
                } else {
                    Op::Amp
                }
            }
            '|' => {
                if self.sc.eat_folded('|') {
                    Op::OrIf
                } else {
                    Op::Pipe
                }
            }
            '(' => Op::LParen,
            ')' => Op::RParen,
            '<' => {
                if self.sc.eat_folded('<') {
                    let strip = self.sc.eat_folded('-');
                    let id = self.heredocs.len();
                    // Reserve the slot now; the body is filled in at the next
                    // newline, once the delimiter word has been scanned.
                    self.heredocs.push(Word::default());
                    self.awaiting = Some((id, strip));
                    Op::DLess(id)
                } else if self.sc.eat_folded('&') {
                    Op::LessAnd
                } else if self.sc.eat_folded('>') {
                    Op::LessGreat
                } else {
                    Op::Less
                }
            }
            '>' => {
                if self.sc.eat_folded('>') {
                    Op::DGreat
                } else if self.sc.eat_folded('&') {
                    Op::GreatAnd
                } else if self.sc.eat_folded('|') {
                    Op::Clobber
                } else {
                    Op::Great
                }
            }
            other => return Err(format!("unexpected character {other:?}").into()),
        };
        // A fold with nothing after it leaves the operator UNFINISHED -- `&` can
        // still become `&&` -- so this asks for more input rather than banking
        // the short one, which `pull` would take for a token boundary and never
        // wind back past.
        if self.fold_wants_more() {
            return Err(SynErr::incomplete("syntax error: unexpected end of file"));
        }
        Ok(op)
    }

    /// One raw line, and whether a NEWLINE ended it. A here-document body is the
    /// characters up to its delimiter line, so a last line that ran out of input
    /// contributes none -- `cat <<E` over `body` feeds four bytes where a
    /// delimited body would feed five.
    fn read_raw_line(&mut self) -> Option<(String, bool)> {
        self.sc.peek()?; // at end of input there is no next line
        let start = self.sc.pos;
        let start_line = self.sc.line;
        let mut line = String::new();
        while let Some(c) = self.sc.bump() {
            if c == '\n' {
                return Some((line, true));
            }
            line.push(c);
        }
        // No terminator. Once nothing more can arrive that IS the last line;
        // while it can, the rest is still coming, so give the characters back
        // rather than treat half a line as whole -- it might be the delimiter
        // with its tail unread.
        if !self.more_can_arrive() {
            Some((line, false))
        } else {
            self.sc.pos = start;
            self.sc.line = start_line;
            None
        }
    }

    fn read_heredoc_bodies(&mut self) -> Syn<()> {
        while !self.pending.is_empty() {
            let Some((id, delim, quoted, strip_tabs)) = self
                .pending
                .first()
                .map(|p| (p.id, p.delim.clone(), p.quoted, p.strip_tabs))
            else {
                break;
            };
            loop {
                let start = self.sc.line;
                let Some((raw, terminated)) = self.read_raw_line() else {
                    // Input that has ENDED ends the body with it. ash leaves the
                    // here-document read on PEOF (`case CENDFILE`, ash.c:12664)
                    // and every syntax check under `endword` is guarded by
                    // `eofmark == NULL`, so `cat <<E` runs what it collected and
                    // says nothing. While more can still arrive this is the
                    // reader's PS2 instead.
                    if !self.more_can_arrive() {
                        self.heredoc_ran_out.get_or_insert_with(|| delim.clone());
                        break;
                    }
                    return Err(SynErr::incomplete(format!("syntax error: unexpected end of file (expecting {delim:?})")));
                };
                let line = if strip_tabs {
                    raw.trim_start_matches('\t')
                } else {
                    raw.as_str()
                };
                if line == delim {
                    break;
                }
                if let Some(p) = self.pending.first_mut() {
                    if p.body_line == 0 {
                        // `read_raw_line` consumed the newline, so the line it
                        // read is the one before where the scanner now sits.
                        p.body_line = start;
                    }
                    p.body.push_str(line);
                    if terminated {
                        p.body.push('\n');
                    }
                }
                self.committed = true;
            }
            let (body, body_line) = match self.pending.first_mut() {
                Some(p) => (std::mem::take(&mut p.body), p.body_line.max(1)),
                None => (String::new(), 1),
            };
            let word = if quoted {
                Word(vec![Seg::Quoted(body)])
            } else {
                // The body is bounded by its delimiter, so an unfinished
                // construct inside it is a syntax error and NOT a request for
                // another line -- `fatal` says so without changing the text,
                // which is the same diagnostic the whole-of-input path gives.
                match heredoc_body_word(&body, body_line) {
                    Ok(w) => w,
                    Err(e) => {
                        self.fatal = true;
                        return Err(e);
                    }
                }
            };
            if let Some(slot) = self.heredocs.get_mut(id) {
                *slot = word;
            }
            // Only now: the entry is what a resumed scan would need if storing
            // the body had failed.
            if !self.pending.is_empty() {
                self.pending.remove(0);
            }
            self.committed = true;
        }
        Ok(())
    }

    fn finish_heredocs(&mut self) -> Syn<()> {
        if self.awaiting.is_some() {
            return Err(SynErr::incomplete("syntax error: unexpected end of file"));
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
        // Distinguishes a word ENDED by a delimiter from one that merely ran out
        // of fed text, which unsealed means the rest is still coming.
        let mut ran_out = true;
        // Open `(` groups, in a `=~` right-hand side only. bash absorbs a
        // BALANCED group whole -- `[[ "a b" =~ (a b) ]]` holds, space and all --
        // and refuses an unbalanced one, both measured.
        let mut group = 0u32;
        while let Some(c) = self.sc.peek() {
            if self.regex_word {
                // `|` is alternation here, never a pipe. `(`/`)` nest, and
                // inside them a blank is part of the expression rather than the
                // end of the word.
                if c == '|' || c == '(' || (c == ')' && group > 0) {
                    if c == '(' {
                        group = group.saturating_add(1);
                    } else if c == ')' {
                        group -= 1;
                    }
                    self.sc.bump();
                    buf.push_lit(c);
                    continue;
                }
                // INSIDE a group every shell operator loses its meaning, which
                // is what the corpus case of that name asks for: bash reads
                // `[[ '< >' =~ (< >) ]]` as one regex. Only the characters that
                // would otherwise END the word are listed -- quoting and
                // expansion still go through the ordinary path below, so
                // `( $v )` expands.
                if group > 0 && matches!(c, ' ' | '\t' | ';' | '&' | '<' | '>') {
                    self.sc.bump();
                    buf.push_lit(c);
                    continue;
                }
                // A newline ends the word even mid-group, so an unbalanced `(`
                // reports an unmatched paren rather than swallowing the script.
            }
            if stop_at_delims && is_word_end(c) {
                ran_out = false;
                break;
            }
            match c {
                '\\' => {
                    self.sc.bump();
                    match self.sc.peek() {
                        // Sealed, a trailing backslash is a literal one; unsealed
                        // it is half of a fold whose newline has not arrived.
                        None if !self.sealed => {
                            return Err(SynErr::incomplete("syntax error: unexpected end of file"))
                        }
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
                            None => return Err(SynErr::incomplete("syntax error: unterminated quoted string")),
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
                        line: BACKTICK_LINE,
                    });
                }
                '$' => self.scan_dollar(&mut buf, false)?,
                other => {
                    self.sc.bump();
                    buf.push_lit(other);
                }
            }
        }
        // A word that ran to the end of the fed text is only finished if nothing
        // more can arrive. Unsealed it is not: a `\<newline>` fold consumed this
        // line's terminator, so the next line CONTINUES this word rather than
        // starting a new one, and ending it here would split `foo\<nl>bar` into
        // two words.
        if ran_out && !self.sealed {
            return Err(SynErr::incomplete("syntax error: unexpected end of file"));
        }
        Ok(buf.finish())
    }

    /// A run of text under double-quote rules with no closing quote to look for:
    /// `$`, backtick and backslash keep their meaning, the whole run stays
    /// quoted, so it is neither field-split nor globbed. Serves THREE callers,
    /// and the two flags are where they part company. `$((...))` bodies and the
    /// runtime strings expanded as if double-quoted (`$PS4`) take neither, so
    /// for them a quote mark is literal too; `escapes_dquote` decides whether
    /// `\"` keeps its backslash there (see `word_from_str`). `subst_word` is the
    /// word of a `${x-word}` inside real double quotes: a `"` opens a nested
    /// quoted run rather than being literal, and `}` joins the escape set,
    /// because it is what ENDS the expansion.
    fn scan_dq_run(&mut self, escapes_dquote: bool, subst_word: bool) -> Syn<Word> {
        let mut buf = WordBuf::default();
        while let Some(c) = self.sc.peek() {
            match c {
                '"' if subst_word => {
                    self.sc.bump();
                    let before = buf.segs.len();
                    self.scan_double(&mut buf)?;
                    buf.mark_empty_quote(before);
                }
                '\\' => {
                    self.sc.bump();
                    match self.sc.peek() {
                        Some(esc @ ('$' | '`' | '\\')) => {
                            self.sc.bump();
                            buf.push_quoted(esc);
                        }
                        // `}` ends the expansion, so the backslash protecting
                        // one is consumed -- unlike in a plain `"..."`, where
                        // `"a\}b"` keeps it.
                        Some('}') if subst_word => {
                            self.sc.bump();
                            buf.push_quoted('}');
                        }
                        Some('"') if escapes_dquote => {
                            self.sc.bump();
                            buf.push_quoted('"');
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
                    buf.push_seg(Seg::Cmd {
                        code,
                        quoted: true,
                        line: BACKTICK_LINE,
                    });
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
                return Err(SynErr::incomplete("syntax error: unterminated quoted string"));
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
                        // ... and `}` when this run is inside a `${...}`, where
                        // it is the brace that would END the expansion.
                        Some('}') if self.in_braces => {
                            self.sc.bump();
                            buf.push_quoted('}');
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
                    buf.push_seg(Seg::Cmd {
                        code,
                        quoted: true,
                        line: BACKTICK_LINE,
                    });
                }
                '$' => self.scan_dollar(buf, true)?,
                other => {
                    self.sc.bump();
                    buf.push_quoted(other);
                }
            }
        }
    }

    /// The body of a `$'...'`, decoded. Escapes are accumulated as BYTES and
    /// the whole body is decoded once at the end, because `\xNN` and `\NNN`
    /// name a byte rather than a character: `$'\xc3\xa9'` is the two bytes
    /// that spell `é`, and decoding each in isolation would give `Ã©`. A
    /// sequence that is not valid UTF-8 becomes U+FFFD, which is already what
    /// this shell does with one out of a command substitution -- its words are
    /// Unicode scalar values and there is no byte string to put a raw `\xff`
    /// in.
    ///
    /// Every character is pushed QUOTED, because the result of the construct
    /// is a quoted string: `set -- $'a b'` is ONE argument and `echo $'*'`
    /// prints an asterisk, both measured. An escape the roster does not claim
    /// keeps its backslash, as bash's does -- `$'\q'` is two characters.
    fn scan_ansi_c(&mut self, buf: &mut WordBuf) -> Syn<()> {
        // Phase one finds the BODY. bash locates the closing quote before
        // evaluating anything in it, honouring only a backslash-escape, so
        // `$'\c'` is a literal `\c` rather than a `\c` that takes the quote as
        // its operand and runs on to the next one.
        let mut body: Vec<char> = Vec::new();
        loop {
            let Some(c) = self.sc.bump() else {
                return Err(SynErr::incomplete("syntax error: unterminated quoted string"));
            };
            if c == '\'' {
                break;
            }
            body.push(c);
            if c == '\\' {
                let Some(esc) = self.sc.bump() else {
                    return Err(SynErr::incomplete("syntax error: unterminated quoted string"));
                };
                body.push(esc);
            }
        }
        // Even an empty body is a real (empty) field, exactly as `''` is: `set
        // -- a $'' b` passes THREE arguments, and a `$'\0'` that truncated to
        // nothing is the same case.
        buf.open_quoted();
        for ch in String::from_utf8_lossy(&decode_ansi_c(&body)).chars() {
            buf.push_quoted(ch);
        }
        Ok(())
    }


    /// A `$`-expansion. `in_dq` marks it as appearing inside double quotes, so
    /// its result is neither field-split nor globbed.
    fn scan_dollar(&mut self, buf: &mut WordBuf, in_dq: bool) -> Syn<()> {
        self.sc.bump(); // '$'
        // The opener is read through any fold, so `$\<newline>{a}` is `${a}`
        // and `$\<newline>(cmd)` a substitution. At end of input the fold is
        // spent and a bare `$` is left, which is a literal.
        self.sc.skip_folds();
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
            // `$'...'` -- ANSI-C quoting. NOT a construct inside double quotes,
            // where bash leaves `$'x'` as those four characters, so the `$` has
            // already been pushed by then and this arm is unquoted-only.
            '\'' if !in_dq => {
                self.sc.bump();
                self.scan_ansi_c(buf)?;
            }
            '{' => {
                self.sc.bump();
                // After the `{`, so this is where the braced text begins.
                let line = self.sc.line;
                let inner = self.scan_braced(in_dq)?;
                buf.push_seg(parse_braced(&inner, in_dq, self.depth, line)?);
            }
            '(' => {
                self.sc.bump();
                // A fold between the two parens is invisible too, so
                // `$(\<newline>(1+2))` is arithmetic rather than a
                // substitution whose body opens with a subshell.
                if self.sc.eat_folded('(') {
                    let line = self.sc.line;
                    let text = self.scan_arith()?;
                    buf.push_seg(Seg::Arith {
                        expr: arith_from_str_at(&text, self.depth + 1, line)?,
                        quoted: in_dq,
                    });
                } else {
                    // After the `(`, so this is the line the BODY opens on.
                    let line = self.sc.line;
                    let code = self.scan_paren_body()?;
                    buf.push_seg(Seg::Cmd {
                        code,
                        quoted: in_dq,
                        line,
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
                loop {
                    // Through folds, as every other character of the construct
                    // is: `$a\<newline>b` names `ab`, not `a` then a literal.
                    self.sc.skip_folds();
                    let Some(c) = self.sc.peek() else { break };
                    if !(c.is_ascii_alphanumeric() || c == '_') {
                        break;
                    }
                    name.push(c);
                    self.sc.bump();
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
    /// Whether the body after the name is scanned in BASE syntax, where a `'`
    /// is real and so protects a `}`. Read-only lookahead, because the body is
    /// scanned before it is parsed.
    ///
    /// ash's `newsyn` starts at the ENCLOSING syntax and only the three
    /// PATTERN operators set `BASESYNTAX` (`parsesub`); a substring offset and
    /// a body ash cannot read reach their arms without touching it. Reading
    /// this wrong is no diagnosable error but a different program, silently.
    fn braced_body_is_base_syntax(&self) -> bool {
        // Every step is taken through `past_folds`, so this reads the text the
        // SCAN will read rather than the source it is written in.
        let mut i = self.sc.past_folds(self.sc.pos);
        match self.sc.src.get(i) {
            Some(c) if c.is_ascii_digit() => {
                while matches!(self.sc.src.get(i), Some(c) if c.is_ascii_digit()) {
                    i = self.sc.past_folds(i + 1);
                }
            }
            Some(&c) if is_special_param(c) => i = self.sc.past_folds(i + 1),
            Some(c) if c.is_ascii_alphabetic() || *c == '_' => {
                while matches!(self.sc.src.get(i), Some(c) if c.is_ascii_alphanumeric() || *c == '_')
                {
                    i = self.sc.past_folds(i + 1);
                }
            }
            // No name ash would read: its `badsub:`, which changes nothing.
            _ => return false,
        }
        // The `:` of a substring is NOT skipped: `${v:#x}` is an offset
        // starting `#`, which ash reads as `VSSUBSTR` and not as a pattern.
        matches!(self.sc.src.get(i), Some('#' | '%' | '/'))
    }

    /// Raw source up to the `}` matching an already-consumed `${`.
    ///
    /// `in_dq` is whether the `${` itself sits inside double quotes, where a
    /// `'` is an ordinary character: `"${u-'x}y'}"` ends at the FIRST `}` and
    /// is `'xy'}` -- word `'x`, then literal outer text. A `"` still protects
    /// one, and `$'...'` is not a construct there at all.
    fn scan_braced(&mut self, in_dq: bool) -> Syn<String> {
        // Only a PATTERN operator's body leaves base syntax on, where quotes
        // are real and still protect a `}`; every other body keeps what
        // encloses it.
        let quotes_off = in_dq && !self.braced_body_is_base_syntax();
        let mut out = String::new();
        let mut depth = 1usize;
        loop {
            let Some(c) = self.sc.bump() else {
                return Err(SynErr::incomplete("syntax error: missing '}'"));
            };
            match c {
                '{' if !quotes_off => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(out);
                    }
                }
                '\\' => {
                    // A fold is eaten before the substitution's own syntax ever
                    // sees it, so `${a\<newline>-x}` is `${a-x}` rather than a
                    // name with a backslash in it. Quoted runs are copied
                    // verbatim below and keep theirs, which is right for a
                    // single-quoted one and harmless for the rest: that text is
                    // lexed again, and folds there.
                    if self.sc.peek() == Some('\n') {
                        self.sc.fold_tail();
                        continue;
                    }
                    out.push('\\');
                    if let Some(n) = self.sc.bump() {
                        out.push(n);
                    }
                    continue;
                }
                // With quotes demoted nothing else shields a `}` inside a
                // nested construct, so each is copied WHOLE. dash pushes a
                // fresh syntax for these, where a `'` quotes again -- which is
                // what the recursive call reproduces.
                // The opener is found through a fold, as it is after any other
                // `$`; without that the nested construct is not copied WHOLE
                // and a `}` inside it ends the outer expansion.
                '$' if quotes_off && {
                    self.sc.skip_folds();
                    matches!(self.sc.peek(), Some('(' | '{'))
                } =>
                {
                    let opener = self.sc.bump();
                    if opener == Some('(') {
                        out.push_str("$(");
                        out.push_str(&self.scan_paren_body()?);
                        out.push(')');
                    } else {
                        out.push_str("${");
                        out.push_str(&self.scan_braced(in_dq)?);
                        out.push('}');
                    }
                    continue;
                }
                // Copied verbatim rather than through `scan_backtick`, which
                // DECODES its escapes: this text is lexed again later.
                '`' if quotes_off => {
                    out.push('`');
                    loop {
                        let Some(q) = self.sc.bump() else {
                            return Err(SynErr::incomplete("syntax error: EOF in backquote substitution"));
                        };
                        out.push(q);
                        if q == '\\' {
                            if let Some(n) = self.sc.bump() {
                                out.push(n);
                            }
                            continue;
                        }
                        if q == '`' {
                            break;
                        }
                    }
                    continue;
                }
                // As in `count_paren_body`: only a `$'...'` escapes its own
                // closing quote, so `${v#$'a\''}` ends at the wrong one under a
                // plain single quote's rule and the brace never closes.
                '$' if !quotes_off && {
                    self.sc.skip_folds();
                    self.sc.peek() == Some('\'')
                } =>
                {
                    out.push('$');
                    out.push('\'');
                    self.sc.bump();
                    loop {
                        let Some(q) = self.sc.bump() else {
                            return Err(SynErr::incomplete("syntax error: unterminated quoted string"));
                        };
                        out.push(q);
                        if q == '\\' {
                            if let Some(n) = self.sc.bump() {
                                out.push(n);
                            }
                            continue;
                        }
                        if q == '\'' {
                            break;
                        }
                    }
                    continue;
                }
                // Inside double quotes a `'` quotes nothing, so it neither
                // opens a run nor protects a brace; it falls through below.
                '\'' if quotes_off => {}
                '\'' | '"' => {
                    out.push(c);
                    // Copy the quoted run verbatim so a `}` inside it is not
                    // mistaken for the closing brace.
                    loop {
                        let Some(q) = self.sc.bump() else {
                            return Err(SynErr::incomplete("syntax error: unterminated quoted string"));
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
    ///
    /// Found by LEXING rather than by counting parens. ash reads a `$( )` body
    /// with the parser itself (`PARSEBACKQNEW`, ash.c:12898), so a `)` inside a
    /// comment, a here-document body, or an arithmetic `<<` never reaches a
    /// paren count at all. A counting scan has to be taught each of those
    /// separately, and a rule copied out of the lexer is a rule that can come
    /// to differ from it -- which is exactly what happened. So the body is
    /// tokenised with this same lexer and the closer is the `)` its tokens
    /// arrive at.
    ///
    /// The one `)` that is not the closer despite being the same token is a
    /// case PATTERN's; `step_case` carries the little grammar that tells them
    /// apart. The COUNT below has none of it, which is why the two paths can
    /// pick different parens for a body that does not lex.
    fn scan_paren_body(&mut self) -> Syn<String> {
        // The scan RECURSES once per nested `$(`, so it is charged and capped
        // like every other re-lexing pass. Past the cap the count takes over
        // rather than the stack running out: it does not recurse, and a script
        // that deep has its own depth error to report when the body is lexed.
        if self.depth >= MAX_EXPANSION_DEPTH {
            return self.count_paren_body();
        }
        let mut lx = self.nested_at_cursor(self.depth.saturating_add(1));
        let found = close_paren(&mut lx);
        // Back before `?`: the source is MOVED rather than copied -- the input
        // left to scan can be the whole script, and copying it at every `$(`
        // would be quadratic in a script that uses many -- so an error must not
        // leave this lexer holding none.
        self.sc.src = std::mem::take(&mut lx.sc.src);
        let (body_end, close_end) = match found {
            Ok(pair) => pair,
            // A body the lexer cannot read cannot be SPLIT by lexing either,
            // so the parens are counted instead -- the split this code had
            // before. Only a body that ran OUT is decided here: a `)` that has
            // not arrived is not one the count reaches either.
            Err(e) if e.is_incomplete() => return Err(e),
            Err(_) => return self.count_paren_body(),
        };
        let mut out = String::new();
        while self.sc.pos < close_end {
            let Some(c) = self.sc.bump() else { break };
            if self.sc.pos <= body_end {
                out.push(c);
            }
        }
        Ok(out)
    }

    /// The closer by paren COUNT, for a body `close_paren` could not lex. Blind
    /// to comments and here-document bodies, which is why it is the fallback
    /// and not the rule -- but a body that does not lex has an error to report
    /// whichever `)` is picked, and this is the split it had before.
    fn count_paren_body(&mut self) -> Syn<String> {
        let mut out = String::new();
        let mut depth = 1usize;
        loop {
            let Some(c) = self.sc.bump() else {
                return Err(SynErr::incomplete("syntax error: unexpected end of file (expecting \")\")"));
            };
            match c {
                '(' => depth = depth.saturating_add(1),
                ')' => {
                    depth = depth.saturating_sub(1);
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
                // A `$'...'` escapes its own closing quote, where a plain
                // `'...'` has no escapes at all -- so this scan has to tell them
                // apart or `$(printf %s $'a\'b')` ends at the WRONG quote and
                // the substitution never closes.
                '$' if self.sc.peek() == Some('\'') => {
                    out.push('$');
                    out.push('\'');
                    self.sc.bump();
                    loop {
                        let Some(q) = self.sc.bump() else {
                            return Err(SynErr::incomplete("syntax error: unterminated quoted string"));
                        };
                        out.push(q);
                        if q == '\\' {
                            if let Some(n) = self.sc.bump() {
                                out.push(n);
                            }
                            continue;
                        }
                        if q == '\'' {
                            break;
                        }
                    }
                    continue;
                }
                '\'' | '"' | '`' => {
                    out.push(c);
                    loop {
                        let Some(q) = self.sc.bump() else {
                            // A backtick body is its own wording in ash; the two
                            // quotes are not.
                            return Err(SynErr::incomplete(if c == '`' {
                                "syntax error: EOF in backquote substitution"
                            } else {
                                "syntax error: unterminated quoted string"
                            }));
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

    /// A lexer over the SAME source, positioned at the cursor. `sealed` carries
    /// over so text that has merely not arrived yet still reads as unfinished.
    /// Nothing else does, deliberately: this pass reports WHERE the body ends
    /// and the body is lexed again for real, so `fatal`, `heredoc_ran_out`,
    /// `committed` and `ended_in_comment` are that second pass's to set.
    fn nested_at_cursor(&mut self, depth: u32) -> Lexer {
        Lexer {
            sealed: self.sealed,
            resumable: false,
            heredoc_ran_out: None,
            owed_newline: false,
            committed: false,
            fatal: false,
            sc: Scanner {
                src: std::mem::take(&mut self.sc.src),
                pos: self.sc.pos,
                line: self.sc.line,
                continued: false,
            },
            ended_in_comment: false,
            heredocs: Vec::new(),
            pending: Vec::new(),
            awaiting: None,
            depth,
            cond_depth: 0,
            regex_next: false,
            cond_continues: false,
            cmd_position: true,
            regex_word: false,
            in_braces: false,
        }
    }

    /// Text up to the `))` matching an already-consumed `$((`.
    fn scan_arith(&mut self) -> Syn<String> {
        let mut out = String::new();
        let mut depth = 1usize;
        loop {
            let Some(c) = self.sc.peek() else {
                return Err(SynErr::incomplete("syntax error: missing '))'"));
            };
            match c {
                '(' => {
                    depth += 1;
                    out.push(c);
                    self.sc.bump();
                }
                ')' => {
                    if depth == 1 {
                        self.sc.bump();
                        if self.sc.eat_folded(')') {
                            return Ok(out);
                        }
                        // Running OUT of input here is not a malformed
                        // expansion: the second `)` may be on the next line,
                        // and a fatal error refuses it where every other
                        // construct would have asked. `scan_op`'s argument.
                        if self.sc.peek().is_none() {
                            return Err(SynErr::incomplete("syntax error: missing '))'"));
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
                return Err(SynErr::incomplete("syntax error: EOF in backquote substitution"));
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
fn heredoc_body_word(body: &str, line: u32) -> Syn<Word> {
    let mut lx = Lexer {
        sealed: true,
        resumable: false,
        heredoc_ran_out: None,
        owed_newline: false,
        committed: false,
        fatal: false,
        sc: Scanner {
            src: body.chars().collect(),
            pos: 0,
            line,
            continued: false,
        },
        ended_in_comment: false,
        heredocs: Vec::new(),
        pending: Vec::new(),
        awaiting: None,
        depth: 0,
        cond_depth: 0,
        regex_next: false,
        cond_continues: false,
        cmd_position: true,
        regex_word: false,
        in_braces: false,
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
                buf.push_seg(Seg::Cmd {
                    code,
                    quoted: true,
                    line: BACKTICK_LINE,
                });
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
/// `line` is where `inner` begins in the script. Every operand but the patsub
/// replacement begins on that same line: what precedes one is the NAME, an
/// optional `:` and the operator, and a name cannot hold a newline. The
/// replacement is the exception, since the PATTERN before it can.
///
/// A body that is none of those becomes a `Seg::BadSub`, which raises when the
/// word is expanded; only one that could not be READ stays a `SynErr`.
fn parse_braced(inner: &str, quoted: bool, depth: u32, line: u32) -> Syn<Seg> {
    let chars: Vec<char> = inner.chars().collect();
    if chars.is_empty() {
        return Ok(Seg::BadSub("${}".to_string()));
    }
    // `${#}` is the parameter `#`; `${#x}` is the length of `x`.
    if chars.first() == Some(&'#') && chars.len() > 1 {
        let rest: String = chars.iter().skip(1).collect();
        if is_name(&rest) || (rest.chars().count() == 1 && is_special_param_name(&rest)) {
            return Ok(Seg::Param(Box::new(Param {
                name: rest,
                op: Some(ParamOp::Length),
                quoted,
            })));
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
        _ => return Ok(Seg::BadSub(format!("${{{inner}}}"))),
    }
    if i >= chars.len() {
        return Ok(Seg::Param(Box::new(Param {
            name,
            op: None,
            quoted,
        })));
    }
    let colon = chars.get(i) == Some(&':');
    if colon {
        i += 1;
    }
    let Some(&opc) = chars.get(i) else {
        // A `:` with no operator is the one body ash does not defer: it cannot
        // read this far, reporting the enclosing quote unterminated instead.
        return Err(format!("bad substitution: `${{{inner}}}`").into());
    };
    i += 1;
    // `${v:off}` and `${v:off:len}`. A colon followed by anything that is not
    // one of the four word operators starts an arithmetic offset, which is what
    // separates `${v:-1}` (the default operator, so `abcdef`) from `${v: -1}`
    // and `${v:(-1)}` (the last character). `opc` is part of the expression, so
    // this reads from BEFORE it.
    if colon && !matches!(opc, '-' | '=' | '?' | '+') {
        let rest = chars.get(i - 1..).unwrap_or_default();
        let (offset, length) = split_slice(rest);
        return Ok(Seg::Param(Box::new(Param {
            name,
            op: Some(ParamOp::Substring {
                offset: arith_from_str_at(&offset, depth + 1, line)?,
                length: match length {
                    Some(l) => Some(arith_from_str_at(&l, depth + 1, line)?),
                    None => None,
                },
            }),
            quoted,
        })));
    }
    let doubled = chars.get(i) == Some(&opc) && matches!(opc, '%' | '#' | '/');
    if doubled {
        i += 1;
    }
    // patsub splits before expanding, so it needs the RAW operand; every other
    // operator takes the whole remainder as one word.
    if opc == '/' && !colon {
        let rest = chars.get(i..).unwrap_or_default();
        let (pat, repl) = split_patsub(rest);
        return Ok(Seg::Param(Box::new(Param {
            name,
            op: Some(ParamOp::Replace {
                pat: word_from_str_at(&pat, depth + 1, line)?,
                repl: word_from_str_at(
                    &repl,
                    depth + 1,
                    line.saturating_add(u32::try_from(pat.matches('\n').count()).unwrap_or(u32::MAX)),
                )?,
                all: doubled,
            }),
            quoted,
        })));
    }
    // Decided BEFORE the operand is lexed. The rest of a body ash never reads
    // is not a word, and lexing it makes an unclosed quote there a PARSE error
    // -- which is the whole of what this defers: `"${v['}'"` would stop the
    // script where ash raises at expansion and carries on.
    if !matches!(opc, '-' | '=' | '?' | '+' | '%' | '#') {
        return Ok(Seg::BadSub(format!("${{{inner}}}")));
    }
    let operand: String = chars.iter().skip(i).collect();
    let word = if quoted && matches!(opc, '-' | '=' | '?' | '+') {
        dq_word_from_str_at(&operand, depth + 1, line)?
    } else {
        word_from_str_at(&operand, depth + 1, line)?
    };
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
        // The guard above returned for every other operator, so this backs up
        // the two `!colon` arms rather than deciding anything itself.
        _ => return Ok(Seg::BadSub(format!("${{{inner}}}"))),
    };
    Ok(Seg::Param(Box::new(Param {
        name,
        op: Some(op),
        quoted,
    })))
}

/// Split a slice operand into offset and optional length at the colon between
/// them. The colon has to be found at the TOP level: `${v:$(f:x):2}` carries one
/// inside a substitution, and `${v:a?1:2}` carries one inside a conditional,
/// where the bracket depth is what tells them apart. bash reads the length as
/// starting after the LAST such colon in the ternary case, which is why the
/// scan tracks `?` depth rather than stopping at the first colon it sees.
fn split_slice(operand: &[char]) -> (String, Option<String>) {
    let text = |r: Option<&[char]>| -> String { r.unwrap_or_default().iter().collect() };
    let mut i = 0usize;
    let mut depth = 0u32;
    let mut ternary = 0u32;
    while let Some(&c) = operand.get(i) {
        match c {
            '\\' => i += 1,
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            '?' if depth == 0 => ternary += 1,
            '\'' | '"' => {
                let q = c;
                i += 1;
                while let Some(&n) = operand.get(i) {
                    // A backslash escapes inside double quotes but not inside
                    // single ones, so `"\""` is data and does not end the run.
                    if n == '\\' && q == '"' {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    if n == q {
                        break;
                    }
                }
                continue;
            }
            '$' => i += skip_substitution(operand, i).saturating_sub(1),
            '`' => {
                i += 1;
                while let Some(&n) = operand.get(i) {
                    if n == '\\' {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    if n == '`' {
                        break;
                    }
                }
                continue;
            }
            ':' if depth == 0 && ternary == 0 => {
                return (text(operand.get(..i)), Some(text(operand.get(i + 1..))));
            }
            ':' if depth == 0 => ternary -= 1,
            _ => {}
        }
        i += 1;
    }
    (text(Some(operand)), None)
}

/// Split a patsub operand into pattern and replacement at the first delimiting
/// `/`. ash finds it in the RAW text, before the pattern is expanded, so a `/`
/// arriving from a value is data: `a=a/; ${v/$a/r}` has the whole of `$a` as its
/// pattern. A LEADING `/` is data too, since the pattern cannot be empty --
/// that is what makes `${v////-}` replace `/` rather than nothing. No delimiter
/// at all means an empty replacement, so `${v/a}` deletes.
fn split_patsub(operand: &[char]) -> (String, String) {
    let text = |r: Option<&[char]>| -> String { r.unwrap_or_default().iter().collect() };
    let mut i = usize::from(operand.first() == Some(&'/'));
    while let Some(&c) = operand.get(i) {
        match c {
            '\\' => i += 2,
            '\'' | '"' => {
                i += 1;
                while let Some(&q) = operand.get(i) {
                    // Inside DOUBLE quotes a backslash escapes what follows, so
                    // `\"` is data and does not end the region. Single quotes
                    // have no escapes, which is why this turns on `c`.
                    if c == '"' && q == '\\' {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    if q == c {
                        break;
                    }
                }
            }
            // A `/` inside a substitution is data. ash agrees for `$( )` and
            // backticks (by then control bytes, not text) but mis-splits a
            // nested `${ }`; treating all three as data is the reading that
            // does not corrupt.
            '$' => i += skip_substitution(operand, i),
            '`' => {
                i += 1;
                while let Some(&q) = operand.get(i) {
                    i += 1;
                    if q == '`' {
                        break;
                    }
                }
            }
            '/' => return (text(operand.get(..i)), text(operand.get(i + 1..))),
            _ => i += 1,
        }
    }
    (text(Some(operand)), String::new())
}

/// How far to step over a `$`-substitution starting at `at`, counting nesting so
/// the delimiter scan cannot stop inside one. A lone `$` advances by 1.
fn skip_substitution(operand: &[char], at: usize) -> usize {
    if !matches!(operand.get(at + 1), Some('{') | Some('(')) {
        return 1;
    }
    // Both bracket kinds are counted together: `$((n))` and `${a-$(f)}` nest
    // either way, and the scan only needs to know where the outermost one ends.
    // Quoted and escaped brackets do not count, since the `)` in
    // `$(printf ")")` closes nothing.
    let mut depth = 0u32;
    let mut i = at + 1;
    while let Some(&c) = operand.get(i) {
        match c {
            '\\' => i += 1,
            '\'' | '"' => {
                i += 1;
                while let Some(&q) = operand.get(i) {
                    if q == c {
                        break;
                    }
                    i += 1;
                }
            }
            '{' | '(' => depth += 1,
            '}' | ')' => {
                depth -= u32::from(depth > 0);
                if depth == 0 {
                    return i + 1 - at;
                }
            }
            _ => {}
        }
        i += 1;
    }
    // Unbalanced: the rest is one blob, so no `/` in it can be a delimiter.
    operand.len() - at
}

fn is_special_param_name(s: &str) -> bool {
    s.chars().next().is_some_and(is_special_param) || s.chars().all(|c| c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(src: &str) -> Syn<Vec<Word>> {
        Ok(tokenize(src, 1)?
            .toks
            .into_iter()
            .filter_map(|p| match p.tok {
                Tok::Word(w) => Some(w),
                _ => None,
            })
            .collect())
    }

    /// The colon between a slice's offset and its length is the one at the TOP
    /// level, so one inside brackets, a ternary, a substitution or a quoted run
    /// is not it -- and a backslash inside a double-quoted run or backticks
    /// escapes, so a quote it protects does not end that run early.
    #[test]
    fn a_slice_operand_splits_at_its_own_colon() {
        let split = |s: &str| {
            let cs: Vec<char> = s.chars().collect();
            split_slice(&cs)
        };
        assert_eq!(split("2"), ("2".into(), None));
        assert_eq!(split("1:2"), ("1".into(), Some("2".into())));
        assert_eq!(split(""), (String::new(), None));
        assert_eq!(split(":"), (String::new(), Some(String::new())));
        // A ternary's colon belongs to the ternary.
        assert_eq!(split("1>0?1:2"), ("1>0?1:2".into(), None));
        assert_eq!(split("1>0?1:2:3"), ("1>0?1:2".into(), Some("3".into())));
        // Brackets and substitutions carry their own.
        assert_eq!(split("(1:2)"), ("(1:2)".into(), None));
        assert_eq!(split("$(f:x):2"), ("$(f:x)".into(), Some("2".into())));
        assert_eq!(split("${a:-x}:2"), ("${a:-x}".into(), Some("2".into())));
        // Quoted runs do too, and an ESCAPED quote does not end one early.
        assert_eq!(split("'a:b':2"), ("'a:b'".into(), Some("2".into())));
        assert_eq!(split("\"a\\\":b\":2"), ("\"a\\\":b\"".into(), Some("2".into())));
        assert_eq!(split("`a\\`:b`:2"), ("`a\\`:b`".into(), Some("2".into())));
    }

    #[test]
    fn patsub_delimiter_is_found_in_the_raw_text() {
        let split = |s: &str| {
            let cs: Vec<char> = s.chars().collect();
            split_patsub(&cs)
        };
        assert_eq!(split("b/X"), ("b".into(), "X".into()));
        // No delimiter: the replacement is empty, so `${v/a}` deletes.
        assert_eq!(split("b"), ("b".into(), String::new()));
        assert_eq!(split("b/"), ("b".into(), String::new()));
        // A LEADING slash is pattern data -- the pattern cannot be empty. These
        // two operands are what `${v////-}` and `${v///r/-}` leave after the
        // `//` that selected replace-all, and they replace `/` and `/r`.
        assert_eq!(split("//-"), ("/".into(), "-".into()));
        assert_eq!(split("/r/-"), ("/r".into(), "-".into()));
        // Quoted and escaped slashes are data, not the delimiter.
        assert_eq!(split("'/'/X"), ("'/'".into(), "X".into()));
        assert_eq!(split("\"/\"/X"), ("\"/\"".into(), "X".into()));
        assert_eq!(split("\\//X"), ("\\/".into(), "X".into()));
        // And so is a slash inside a substitution: it is the value that carries
        // it, not the operand, so the delimiter is the one AFTER the closer.
        assert_eq!(split("$(echo a/b)/Y"), ("$(echo a/b)".into(), "Y".into()));
        assert_eq!(split("${x-a/b}/Y"), ("${x-a/b}".into(), "Y".into()));
        // A bracket inside quotes closes nothing, so the `)` of `printf ")"` must
        // not end the substitution -- otherwise the delimiter is taken from
        // inside it and the pattern is cut in half.
        assert_eq!(split("$(printf \")\")/X"), ("$(printf \")\")".into(), "X".into()));
        assert_eq!(split("$(printf ')')/X"), ("$(printf ')')".into(), "X".into()));
        // An escaped `"` does not end a double-quoted region; a single-quoted one
        // has no escapes, so there the backslash is just data.
        assert_eq!(split("\"a\\\"b\"/X"), ("\"a\\\"b\"".into(), "X".into()));
        assert_eq!(split("'a\\'/X"), ("'a\\'".into(), "X".into()));
        // Backticks are a substitution too.
        assert_eq!(split("`echo a/b`/X"), ("`echo a/b`".into(), "X".into()));
        // `$(( ))` is skipped here, which is bash's answer and NOT ash's: ash
        // treats the `/` inside as the delimiter, a divergence its own source
        // marks as a bug. See the commit message.
        assert_eq!(split("$((1/1))/Y"), ("$((1/1))".into(), "Y".into()));
        // An unbalanced opener leaves no delimiter after it at all.
        assert_eq!(split("$(echo a/b"), ("$(echo a/b".into(), String::new()));
        assert_eq!(split("`echo a/b"), ("`echo a/b".into(), String::new()));
    }

    fn nth(ws: &[Word], i: usize) -> Syn<&Word> {
        ws.get(i).ok_or_else(|| format!("no word at index {i}").into())
    }

    fn heredoc0(src: &str) -> Syn<Word> {
        tokenize(src, 1)?
            .heredocs
            .into_iter()
            .next()
            .ok_or_else(|| "no here-document".into())
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
        assert!(matches!(tokenize("echo 2>x", 1)?.toks.get(1).map(|p| &p.tok), Some(Tok::IoNumber(2))));
        assert!(matches!(tokenize("echo 2 >x", 1)?.toks.get(1).map(|p| &p.tok), Some(Tok::Word(_))));
        Ok(())
    }

    #[test]
    fn heredoc_body_is_collected_after_the_line() -> Syn<()> {
        assert!(matches!(
            tokenize("cat <<EOF\nhi\nEOF\n", 1)?.toks.get(1).map(|p| &p.tok),
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
        assert!(word_from_str_at(&s, 0, 1).is_err());
        // patsub re-enters TWICE per level, once for the pattern and once for the
        // replacement, so it needs the same cap -- and it is charged separately
        // from the operator above, since nothing else reaches these two words.
        let mut s = String::new();
        for _ in 0..500 {
            s.push_str("${x/");
        }
        s.push('y');
        for _ in 0..500 {
            s.push('}');
        }
        assert!(word_from_str_at(&s, 0, 1).is_err());
        // The arith body shares the cap through the same constructor.
        let mut a = String::new();
        for _ in 0..500 {
            a.push_str("$((");
        }
        a.push('1');
        for _ in 0..500 {
            a.push_str("))");
        }
        assert!(arith_from_str_at(&a, 0, 1).is_err());
    }

    #[test]
    fn an_arith_body_quotes_nothing() -> Syn<()> {
        // No corpus case covers this, so pin it here: inside `$(( … ))` neither
        // quote character quotes, and a backslash keeps its double-quote meaning
        // (literal before anything but `$`, backtick, `"`, `\`). Every one of these
        // reaches the arithmetic lexer, which rejects them — as dash does.
        for src in ["'1' + 2", "\"1\" + 2", "1 \\+ 2", "\\1"] {
            let w = arith_from_str_at(src, 0, 1)?;
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
        let w = arith_from_str_at("$x + `echo 1`", 0, 1)?;
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

    /// Every token of a script rendered flat, so a test can pin what a source
    /// lexed to rather than only the words in it.
    fn shape(src: &str) -> Syn<String> {
        let mut out = String::new();
        for p in tokenize(src, 1)?.toks {
            if !out.is_empty() {
                out.push(' ');
            }
            match p.tok {
                Tok::Word(w) => out.push_str(w.plain().unwrap_or("<expansion>")),
                Tok::Op(op) => out.push_str(&format!("{op:?}")),
                Tok::IoNumber(n) => out.push_str(&format!("Io{n}")),
                Tok::Newline => out.push_str("NL"),
                Tok::Eof => out.push_str("EOF"),
            }
        }
        Ok(out)
    }

    /// ash reads the SECOND character of an operator through `pgetc_eatbnl`
    /// (ash.c:13256 for the connectives, 12802/12824/12834 for redirections),
    /// so a `\<newline>` may sit inside one. Measured against busybox ash
    /// 1.37.0, which runs every line below.
    #[test]
    fn a_line_continuation_folds_inside_an_operator() -> Syn<()> {
        let split = |a: &str, b: &str| format!("x {a}\\\n{b} y");
        assert_eq!(shape(&split("&", "&"))?, "x AndIf y EOF");
        assert_eq!(shape(&split("|", "|"))?, "x OrIf y EOF");
        assert_eq!(shape(&split(">", ">"))?, "x DGreat y EOF");
        assert_eq!(shape(&split(">", "|"))?, "x Clobber y EOF");
        assert_eq!(shape(&split(">", "&"))?, "x GreatAnd y EOF");
        assert_eq!(shape(&split("<", "&"))?, "x LessAnd y EOF");
        assert_eq!(shape(&split("<", ">"))?, "x LessGreat y EOF");
        assert_eq!(shape("case x in x) y ;\\\n; esac")?, "case x in x RParen y DSemi esac EOF");
        // The heredoc pair and the `-` that strips its tabs are two more. The
        // fold spends a newline, so the delimiter and `y` are on ONE line and
        // the body begins after the next.
        assert_eq!(shape("x <\\\n<E y\nE\n")?, "x DLess(0) E y NL EOF");
        assert_eq!(shape("x <<\\\n-E y\nE\n")?, "x DLess(0) E y NL EOF");
        // Consecutive folds, since ash's is a `while` loop.
        assert_eq!(shape("x &\\\n\\\n& y")?, "x AndIf y EOF");
        // A fold that completes no operator is still SPENT: ash's `pungetc()`
        // gives back one character and gives back the one after the fold.
        assert_eq!(shape("x >\\\ny")?, "x Great y EOF");
        assert_eq!(shape("x &\\\ny")?, "x Amp y EOF");
        // `&>` glued to a word is the one ash reads with a plain `pgetc`
        // (ash.c:12670 says why), so THAT pair does not fold: `2&>f` names fd
        // 2, and split it is the word `2` and a bare `&>`.
        assert_eq!(shape("x 2&>f")?, "x Io2 AmpGreat f EOF");
        assert_eq!(shape("x 2&\\\n>f")?, "x 2 AmpGreat f EOF");
        assert_eq!(shape("x &\\\n>f")?, "x AmpGreat f EOF");
        Ok(())
    }

    /// A LINE-AT-A-TIME source sees the fold at the end of what it has read, and
    /// there the operator is not finished -- `&` can still become `&&`. So the
    /// pull must ask for more rather than bank the short one: a completed token
    /// is a boundary `pull` rewinds no further than, so emitting `Amp` here made
    /// a stdin-fed `true &\<newline>& echo yes` run `true &` and then report a
    /// syntax error on the rest, while `sh -c` on the same text ran it.
    #[test]
    fn an_operator_left_open_by_a_fold_asks_for_the_rest_of_it() -> Syn<()> {
        let mut scan = Scan::new("true &\\\n");
        let first = scan.pull();
        assert!(first.incomplete, "the operator is unfinished");
        assert_eq!(first.toks.len(), 1, "only the word is banked");
        scan.feed("& echo yes\n");
        scan.seal();
        let rest = scan.pull();
        assert!(rest.error.is_none(), "{:?}", rest.error);
        let ops: Vec<Op> = rest
            .toks
            .iter()
            .filter_map(|p| match p.tok {
                Tok::Op(op) => Some(op),
                _ => None,
            })
            .collect();
        assert_eq!(ops, vec![Op::AndIf]);
        Ok(())
    }

    #[test]
    fn unterminated_quote_reports_incomplete_input() -> Syn<()> {
        match tokenize("echo 'abc", 1) {
            Err(e) => {
                assert!(e.is_incomplete(), "{e}");
                Ok(())
            }
            Ok(_) => Err("an unterminated quote must not tokenize".into()),
        }
    }

    /// The text of the single word `$'...'` produces. Every escape below was
    /// measured against bash 5.2 rather than read off the manual.
    fn ansi_c(body: &str) -> Syn<String> {
        let ws = words(&format!("x $'{body}'"))?;
        let w = nth(&ws, 1)?;
        // The construct is QUOTED throughout: one `Seg`, and a `Quoted` one,
        // so `$'a b'` cannot field-split and `$'*'` cannot glob.
        // Exactly ONE quoted segment, never zero: a segment-less word is not an
        // empty field but no field at all, and accepting one here is what let
        // `set -- a $'' b` pass two arguments unnoticed.
        match w.0.as_slice() {
            [Seg::Quoted(s)] => Ok(s.clone()),
            other => Err(format!("expected one quoted segment, got {other:?}").into()),
        }
    }

    #[test]
    fn ansi_c_quoting_decodes_the_named_escapes() -> Syn<()> {
        assert_eq!(ansi_c(r"\a\b\e\E\f\n\r\t\v")?, "\u{7}\u{8}\u{1b}\u{1b}\u{c}\n\r\t\u{b}");
        assert_eq!(ansi_c(r"\\")?, "\\");
        assert_eq!(ansi_c(r"\'")?, "'");
        assert_eq!(ansi_c(r#"\""#)?, "\"");
        assert_eq!(ansi_c(r"\?")?, "?");
        // An escape the roster does not claim keeps BOTH characters.
        assert_eq!(ansi_c(r"\q")?, "\\q");
        assert_eq!(ansi_c(r"\uZ")?, "\\uZ");
        // bash has no `\u{...}` brace form, so that is the same case.
        assert_eq!(ansi_c(r"\u{03bc}")?, "\\u{03bc}");
        assert_eq!(ansi_c("plain")?, "plain");
        Ok(())
    }

    #[test]
    fn ansi_c_numeric_escapes_stop_at_their_own_digit_count() -> Syn<()> {
        // Two hex digits, so the `B` is a literal rather than a third digit.
        assert_eq!(ansi_c(r"\x41B")?, "AB");
        assert_eq!(ansi_c(r"\x41")?, "A");
        // Three octal, so `\0101` is a NUL-free `\010` then `1`.
        assert_eq!(ansi_c(r"\111")?, "I");
        assert_eq!(ansi_c(r"\1ff")?, "\u{1}ff");
        assert_eq!(ansi_c(r"A")?, "A");
        assert_eq!(ansi_c(r"\U0001F600")?, "\u{1F600}");
        // Not even one digit: the escape is unclaimed and keeps its backslash.
        assert_eq!(ansi_c(r"\x")?, "\\x");
        Ok(())
    }

    /// `\x{...}` is bash's ksh93-compatible brace form: any number of digits,
    /// closing brace optional, and a BYTE rather than a code point -- so it is
    /// masked, unlike `\u`. There is no such form for `\u`/`\U`, which is why
    /// generalising from one to the other would be wrong.
    #[test]
    fn a_braced_hex_escape_is_a_byte_and_the_brace_is_optional() -> Syn<()> {
        assert_eq!(ansi_c(r"\x{41}")?, "A");
        assert_eq!(ansi_c(r"\x{7}")?, "\u{7}");
        // Unterminated still takes the digits it read.
        assert_eq!(ansi_c(r"\x{41")?, "A");
        // Masked to a byte: 0x263a becomes 0x3a, NOT the code point.
        assert_eq!(ansi_c(r"\x{263a}")?, ":");
        assert_eq!(ansi_c(r"\x{1ff}")?, "\u{fffd}");
        // No digits names zero, which truncates as any other NUL does.
        assert_eq!(ansi_c(r"\x{}")?, "");
        assert_eq!(ansi_c(r"\x{0}b")?, "");
        // `\u`/`\U` have NO brace form -- both stay literal in bash.
        assert_eq!(ansi_c(r"\u{41}")?, "\\u{41}");
        assert_eq!(ansi_c(r"\U{41}")?, "\\U{41}");
        Ok(())
    }

    /// Above 0x7fffffff bash emits NOTHING rather than any encoding, so a
    /// replacement character would be a character bash never produces. Between
    /// U+10FFFF and there it emits an extended UTF-8 form this shell cannot
    /// represent, which is the documented divergence instead.
    #[test]
    fn a_code_point_past_the_encodable_range_emits_nothing() -> Syn<()> {
        assert_eq!(ansi_c(r"a\U80000000!b")?, "a!b");
        assert_eq!(ansi_c(r"\Uffffffff")?, "");
        assert_eq!(ansi_c(r"a\U7fffffffb")?, "a\u{fffd}b");
        assert_eq!(ansi_c(r"a\U00110000b")?, "a\u{fffd}b");
        Ok(())
    }

    /// `\xNN` and `\NNN` name a BYTE, so two of them can spell one character.
    /// Decoding each in isolation would give `Ã©` for the first of these.
    #[test]
    fn ansi_c_byte_escapes_compose_into_one_character() -> Syn<()> {
        assert_eq!(ansi_c(r"\xc3\xa9")?, "é");
        assert_eq!(ansi_c(r"\xe6\x97\xa5")?, "日");
        assert_eq!(ansi_c(r"\303\251")?, "é");
        // A lone high byte is not valid UTF-8 and has no representation in a
        // shell whose words are Unicode scalar values, so it takes the same
        // replacement a command substitution's would. bash emits the raw byte.
        assert_eq!(ansi_c(r"\xff")?, "\u{fffd}");
        // As does a code point that is not a scalar value: a surrogate, or one
        // past the Unicode maximum. bash emits WTF-8 for both.
        assert_eq!(ansi_c(r"\uD800")?, "\u{fffd}");
        assert_eq!(ansi_c(r"\U00110000")?, "\u{fffd}");
        Ok(())
    }

    /// Ctrl-X is its letter masked to five bits, NOT xor 0x40: the two agree on
    /// letters and part company on everything else.
    #[test]
    fn ansi_c_control_escapes_mask_to_five_bits() -> Syn<()> {
        assert_eq!(ansi_c(r"\cA")?, "\u{1}");
        assert_eq!(ansi_c(r"\ca")?, "\u{1}");
        assert_eq!(ansi_c(r"\cz")?, "\u{1a}");
        // The arms that `^ 0x40` would get wrong.
        assert_eq!(ansi_c(r"\c0")?, "\u{10}");
        assert_eq!(ansi_c(r"\c9")?, "\u{19}");
        assert_eq!(ansi_c(r"\c-")?, "\r");
        assert_eq!(ansi_c(r"\c+")?, "\u{b}");
        assert_eq!(ansi_c(r#"\c""#)?, "\u{2}");
        // The operand is taken RAW -- Ctrl-\ then a literal `a`, not Ctrl-\a --
        // except that a backslash operand is written doubled.
        assert_eq!(ansi_c(r"\c\a")?, "\u{1c}a");
        assert_eq!(ansi_c(r"\c\\")?, "\u{1c}");
        assert_eq!(ansi_c(r"\c\\x")?, "\u{1c}x");
        // Ctrl-? is DEL by convention rather than the mask's 0x1f.
        assert_eq!(ansi_c(r"\c?")?, "\u{7f}");
        // An operand that masks to NUL truncates like any other NUL.
        assert_eq!(ansi_c(r"\c@x")?, "");
        assert_eq!(ansi_c(r"a\c@b")?, "a");
        assert_eq!(ansi_c(r"\c x")?, "");
        // Only the FIRST byte is masked, so a multi-byte operand keeps its
        // tail: bash gives 0x03 then a stray 0xa9, which is not valid UTF-8 and
        // so takes the replacement the byte escapes above do.
        assert_eq!(ansi_c(r"\cé!")?, "\u{3}\u{fffd}!");
        Ok(())
    }

    /// A NUL cannot travel in an argument and bash TRUNCATES there. The rest of
    /// the body is still LEXED, or a later `\'` would be read as the closing
    /// quote and the construct would end in the wrong place.
    #[test]
    fn ansi_c_truncates_at_a_nul_but_keeps_lexing() -> Syn<()> {
        assert_eq!(ansi_c(r"a\0b")?, "a");
        assert_eq!(ansi_c(r"\0")?, "");
        assert_eq!(ansi_c(r"\x00b")?, "");
        // `\400` masks to 0x00, so the mask has to come BEFORE the NUL test.
        assert_eq!(ansi_c(r"\400x")?, "");
        // The escaped quote is an escape, not the closer: one word, and the
        // `c` after it belongs to this construct rather than to the next word.
        assert_eq!(ansi_c(r"a\0b\'c")?, "a");
        assert_eq!(words(r"echo $'a\0b\'c' second")?.len(), 3);
        Ok(())
    }

    /// The closing quote is found BEFORE anything is evaluated, so an escape
    /// cannot reach past it. `\c` is the one that could: it consumes an
    /// operand, and consuming the quote would run the construct on to the next
    /// one -- turning a program bash rejects into a BEL, and `$'\c'` into an
    /// error where bash has two literal characters.
    #[test]
    fn the_closing_quote_is_found_before_escapes_are_evaluated() -> Syn<()> {
        assert_eq!(ansi_c(r"\c")?, "\\c");
        assert_eq!(ansi_c(r"a\cb")?, "a\u{2}");
        // `\c` takes the backslash and the quote is left a literal, so this is
        // Ctrl-\ followed by `'` -- not a closed string followed by junk.
        assert_eq!(ansi_c(r"\c\'")?, "\u{1c}'");
        // An escaped quote is body, not the closer.
        assert_eq!(ansi_c(r"a\'b")?, "a'b");
        assert_eq!(ansi_c(r"a\\")?, "a\\");
        // And a body that never closes is incomplete rather than silently
        // ending at whatever `\c` swallowed.
        for src in [r"echo $'\c", r"echo $'\c\", r"echo $'a\'"] {
            match tokenize(src, 1) {
                Err(e) => assert!(e.is_incomplete(), "{src}: {e}"),
                Ok(_) => return Err(format!("`{src}` must not tokenize").into()),
            }
        }
        Ok(())
    }

    /// The body of a `$'...'` is text the SCRIPT supplies, and every escape in
    /// it walks an index over a slice. A linear congruential generator rather
    /// than a dependency, seeded fixed so a failure reproduces.
    #[test]
    fn no_ansi_c_body_panics_the_decoder() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut rand = move || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) as u32
        };
        // Oversample what the walk turns on: backslashes, the escapes that
        // consume a following operand, and digits in both radices.
        let pick = |r: u32| -> char {
            match r % 10 {
                0..=2 => '\\',
                3 => "cxuU01234567".chars().nth((r >> 8) as usize % 12).unwrap_or('c'),
                4 => "89abcdefABCDEF".chars().nth((r >> 8) as usize % 14).unwrap_or('a'),
                5 => '\'',
                6 => ['é', '日', '\u{ff}', '\u{10fffd}'][(r >> 8) as usize % 4],
                _ => char::from_u32(0x20 + (r >> 8) % 0x60).unwrap_or('a'),
            }
        };
        for _ in 0..20_000 {
            let n = rand() % 24;
            let body: Vec<char> = (0..n).map(|_| pick(rand())).collect();
            // The property is only that it returns: no panic, no overflow in a
            // debug build, and no walk that fails to advance.
            let out = decode_ansi_c(&body);
            // A NUL never reaches the result -- it truncates instead.
            assert!(!out.contains(&0), "NUL in output for {body:?}");
            // And the same text through the WHOLE lexer, which is where the
            // body-finding phase meets the escapes: a syntax error is a fine
            // answer, hanging or panicking is not.
            let src: String = body.iter().collect();
            let _ = tokenize(&format!("echo $'{src}'"), 1);
            let _ = tokenize(&format!("echo $({src})"), 1);
        }
    }

    /// `$( )`'s body is scanned as RAW SOURCE to locate its closing paren, and
    /// that scan has to tell `$'...'` from `'...'`: only the first escapes its
    /// own closing quote, so reading `$'a\'b'` by the second's rule ends the
    /// string at the wrong quote and the substitution never closes.
    #[test]
    fn a_substitution_body_knows_an_ansi_c_quote_from_a_plain_one() -> Syn<()> {
        let body = |src: &str| -> Syn<String> {
            let ws = words(src)?;
            match nth(&ws, 1)?.0.as_slice() {
                [Seg::Cmd { code, .. }] => Ok(code.clone()),
                other => Err(format!("expected one command segment, got {other:?}").into()),
            }
        };
        assert_eq!(body(r"echo $(f $'a\'b')")?, r"f $'a\'b'");
        assert_eq!(body(r"echo $(f $'a\\')")?, r"f $'a\\'");
        // A PLAIN single-quoted region still has no escapes, so the backslash
        // before the quote is data and the region ends at that quote.
        assert_eq!(body(r"echo $(f 'a\' b)")?, r"f 'a\' b");
        // `${ }` scans its operand the same way, and for the same reason: the
        // brace must not be found inside the string.
        let param = |src: &str| -> Syn<String> {
            let ws = words(src)?;
            match nth(&ws, 1)?.0.as_slice() {
                [Seg::Param(p)] => Ok(format!("{:?}", p.op)),
                other => Err(format!("expected one param segment, got {other:?}").into()),
            }
        };
        assert!(param(r"echo ${v#$'a\''}")?.contains("Trim"));
        assert!(param(r"echo ${v#'a}b'}")?.contains("Trim"));
        Ok(())
    }

    /// Which braced bodies a `'` is REAL in, inside `"..."`. ash starts
    /// `newsyn` at the enclosing syntax and only the three PATTERN operators
    /// set `BASESYNTAX` (`parsesub`), so a quote protects a `}` for those and
    /// for nothing else -- not a substitution operator, not a substring
    /// offset, and not a body ash cannot read, which reaches `badsub:` without
    /// touching it. Each probe puts the quoted `}` LAST, which is what makes
    /// the two rules different programs rather than the same error by luck.
    #[test]
    fn only_a_pattern_operator_makes_a_quote_real_inside_a_braced_body() -> Syn<()> {
        // Quotes off: the body ends at the `}` between them, so this parses.
        for src in [
            r#"echo "${v-'}'""#,
            r#"echo "${v:-'}'""#,
            r#"echo "${v+'}'""#,
            r#"echo "${v:?'}'""#,
            // A substring OFFSET that starts with a pattern character is still
            // an offset -- ash's `VSSUBSTR` -- so the `:` is not skipped when
            // looking for one. Skipping it reads these as trims and scans the
            // body under the other rule, where the quote protects the `}`.
            r#"echo "${v:#'}'""#,
            r#"echo "${v:%'}'""#,
            r#"echo "${v:/'}'""#,
        ] {
            assert!(words(src).is_ok(), "{src:?} must parse");
        }
        // Quotes real: the `'` protects that `}` and the scan runs out.
        for src in [
            r#"echo "${v#'}'""#,
            r#"echo "${v##'}'""#,
            r#"echo "${v%'}'""#,
            r#"echo "${v%%'}'""#,
            r#"echo "${v/'}'""#,
            r#"echo "${v//'}'""#,
        ] {
            assert!(words(src).is_err(), "{src:?} must not parse");
        }
        // A body this shell cannot read is quotes-off too, and the operator is
        // judged BEFORE the operand is lexed -- otherwise the lone `'` left in
        // the body is an unterminated quote and a PARSE error, which is the
        // thing `Seg::BadSub` exists to avoid.
        for (src, text) in [
            (r#"echo "${v['}'""#, r"${v['}"),
            (r#"echo "${v@'}'""#, r"${v@'}"),
            (r#"echo "${!v'}'""#, r"${!v'}"),
            (r#"echo "${#v'}'""#, r"${#v'}"),
            (r#"echo "${%'}'""#, r"${%'}"),
        ] {
            let ws = words(src)?;
            let seg = nth(&ws, 1)?.0.first().cloned();
            assert!(
                matches!(&seg, Some(Seg::BadSub(s)) if s == text),
                "{src:?} wanted BadSub({text:?}), got {seg:?}"
            );
        }
        Ok(())
    }

    /// An empty result is an empty FIELD, not the absence of one -- `''`'s
    /// rule. Without it `set -- a $'' b` passes two arguments and `test -n $''`
    /// answers about the string `-n`.
    #[test]
    fn an_empty_ansi_c_quote_is_still_a_field() -> Syn<()> {
        assert_eq!(ansi_c("")?, "");
        assert_eq!(ansi_c(r"\0")?, "");
        // Three words after `echo`, the middle one empty.
        let ws = words(r"echo a $'' b")?;
        assert_eq!(ws.len(), 4);
        assert!(matches!(nth(&ws, 2)?.0.as_slice(), [Seg::Quoted(s)] if s.is_empty()));
        Ok(())
    }

    #[test]
    fn ansi_c_quoting_is_not_a_construct_inside_double_quotes() -> Syn<()> {
        // bash leaves `$'x'` as those four characters there, measured.
        let ws = words("echo \"$'a\\tb'\"")?;
        assert_eq!(nth(&ws, 1)?.delimiter().map(|(t, _)| t), Some("$'a\\tb'".into()));
        Ok(())
    }

    #[test]
    fn an_unterminated_ansi_c_quote_reports_incomplete_input() -> Syn<()> {
        for src in ["echo $'abc", "echo $'a\\", "echo $'a\\c"] {
            match tokenize(src, 1) {
                Err(e) => assert!(e.is_incomplete(), "{src}: {e}"),
                Ok(_) => return Err(format!("`{src}` must not tokenize").into()),
            }
        }
        Ok(())
    }
}

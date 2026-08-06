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
    /// Body lines read so far. Kept HERE rather than in a local so that a scan
    /// which runs out of input part way through a body resumes after the lines
    /// it already consumed, instead of re-reading them on every refill.
    body: String,
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
    let mut scan = Scan::new(src);
    scan.seal();
    let chunk = scan.pull();
    Lexed {
        toks: chunk.toks,
        heredocs: scan.take_heredocs(),
        ended_in_comment: scan.ended_in_comment(),
        error: chunk.error,
    }
}

/// What one `pull` produced. `incomplete` means the scan stopped because the
/// input ran out INSIDE a construct and more of it could finish the job -- the
/// scanner has been rewound to the last token boundary, so feeding more text and
/// pulling again re-scans only the token that was open.
pub struct Chunk {
    pub toks: Vec<Tok>,
    pub incomplete: bool,
    pub error: Option<String>,
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
        Scan {
            lx: Lexer {
                sc: Scanner {
                    src: src.chars().collect(),
                    pos: 0,
                    continued: false,
                },
                ended_in_comment: false,
                sealed: false,
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
                    toks.push(tok);
                    if last {
                        return Chunk { toks, incomplete: false, error: None };
                    }
                }
                // Unsealed, an unfinished construct is a request for more input
                // rather than an error: rewind so the open token is scanned once,
                // whole, when the rest of it arrives.
                Err(e) if !self.sealed && !self.lx.fatal && e.starts_with(INCOMPLETE) => {
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
/// `scan_dollar -> parse_param -> word_from_str_at` is bounded.
fn word_from_str_at(text: &str, depth: u32) -> Syn<Word> {
    lexer_over(text, depth)?.scan_word(false)
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
    lexer_over(text, 0)?.scan_dq_run(false)
}

/// Scan the body of `$((...))`. POSIX expands it as if it were double-quoted,
/// "except that a double-quote inside the expression is not treated specially",
/// so NEITHER quote character quotes here: both reach the arithmetic lexer, which
/// rejects them. That is why `$(( '1' + 2 ))` is an error and not 3.
fn arith_from_str_at(text: &str, depth: u32) -> Syn<Word> {
    lexer_over(text, depth)?.scan_dq_run(true)
}

fn lexer_over(text: &str, depth: u32) -> Syn<Lexer> {
    if depth > MAX_EXPANSION_DEPTH {
        return Err("expansion nested too deeply".into());
    }
    Ok(Lexer {
        sealed: true,
        owed_newline: false,
        committed: false,
        fatal: false,
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
        cond_depth: 0,
        regex_next: false,
        cond_continues: false,
        cmd_position: true,
        regex_word: false,
    })
}

struct Lexer {
    sc: Scanner,
    ended_in_comment: bool,
    /// Set once no more input can arrive, which is what makes an unterminated
    /// final line the last line rather than half of one still being typed.
    sealed: bool,
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
}

impl Lexer {
    fn mark(&self) -> Mark {
        Mark {
            pos: self.sc.pos,
            continued: self.sc.continued,
            ended_in_comment: self.ended_in_comment,
            awaiting: self.awaiting,
            owed_newline: self.owed_newline,
            pending: self.pending.len(),
            heredocs: self.heredocs.len(),
        }
    }

    fn restore(&mut self, m: Mark) {
        self.sc.pos = m.pos;
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
                if self.sc.continued {
                    return Err(format!("{INCOMPLETE}: line continuation"));
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
        let start = self.sc.pos;
        let mut line = String::new();
        while let Some(c) = self.sc.bump() {
            if c == '\n' {
                return Some(line);
            }
            line.push(c);
        }
        // No terminator. Sealed, that IS the last line; unsealed, the rest of it
        // is still coming, so give the characters back rather than treat half a
        // line as whole -- it might be the delimiter with its tail unread.
        if self.sealed {
            Some(line)
        } else {
            self.sc.pos = start;
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
                let Some(raw) = self.read_raw_line() else {
                    return Err(format!("{INCOMPLETE}: here-document delimited by `{delim}`"));
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
                    p.body.push_str(line);
                    p.body.push('\n');
                }
                self.committed = true;
            }
            let body = match self.pending.first_mut() {
                Some(p) => std::mem::take(&mut p.body),
                None => String::new(),
            };
            let word = if quoted {
                Word(vec![Seg::Quoted(body)])
            } else {
                // The body is bounded by its delimiter, so an unfinished
                // construct inside it is a syntax error and NOT a request for
                // another line -- `fatal` says so without changing the text,
                // which is the same diagnostic the whole-of-input path gives.
                match heredoc_body_word(&body) {
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
                            return Err(format!("{INCOMPLETE}: line continuation"))
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
        // A word that ran to the end of the fed text is only finished if nothing
        // more can arrive. Unsealed it is not: a `\<newline>` fold consumed this
        // line's terminator, so the next line CONTINUES this word rather than
        // starting a new one, and ending it here would split `foo\<nl>bar` into
        // two words.
        if ran_out && !self.sealed {
            return Err(format!("{INCOMPLETE}: line continuation"));
        }
        Ok(buf.finish())
    }

    /// A run of text under double-quote rules with no closing quote to look for:
    /// `$`, backtick and backslash keep their meaning, every other character --
    /// quote marks included -- is literal, and the whole run stays quoted, so it
    /// is neither field-split nor globbed. Serves both `$((...))` bodies and the
    /// runtime strings expanded as if double-quoted (`$PS4`).
    ///
    /// `escapes_dquote` is the one place the two part company; see `word_from_str`.
    fn scan_dq_run(&mut self, escapes_dquote: bool) -> Syn<Word> {
        let mut buf = WordBuf::default();
        while let Some(c) = self.sc.peek() {
            match c {
                '\\' => {
                    self.sc.bump();
                    match self.sc.peek() {
                        Some(esc @ ('$' | '`' | '\\')) => {
                            self.sc.bump();
                            buf.push_quoted(esc);
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
        sealed: true,
        owed_newline: false,
        committed: false,
        fatal: false,
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
        cond_depth: 0,
        regex_next: false,
        cond_continues: false,
        cmd_position: true,
        regex_word: false,
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
    let doubled = chars.get(i) == Some(&opc) && matches!(opc, '%' | '#' | '/');
    if doubled {
        i += 1;
    }
    // patsub splits before expanding, so it needs the RAW operand; every other
    // operator takes the whole remainder as one word.
    if opc == '/' && !colon {
        let rest = chars.get(i..).unwrap_or_default();
        let (pat, repl) = split_patsub(rest);
        return Ok(Param {
            name,
            op: Some(ParamOp::Replace {
                pat: word_from_str_at(&pat, depth + 1)?,
                repl: word_from_str_at(&repl, depth + 1)?,
                all: doubled,
            }),
            quoted,
        });
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
        Ok(tokenize(src)?
            .toks
            .into_iter()
            .filter_map(|t| match t {
                Tok::Word(w) => Some(w),
                _ => None,
            })
            .collect())
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

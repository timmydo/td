//! Recursive-descent parser for the POSIX shell grammar over the lexer's token
//! stream.
//!
//! Reserved words are resolved here rather than in the lexer because they are
//! positional: `done` ends a loop only where a command could start, and is an
//! ordinary argument in `echo done`. Alias substitution is here for the same
//! reason -- only the grammar knows which positions can start a command word --
//! and happens at the token fetch, as dash's does.

use std::sync::Arc;

use crate::ast::{
    is_name, AndOr, ArithCmp, Assign, CaseItem, Cmd, CondExpr, CondOp, Conn, IfArm, List, Pipeline, Redir,
    RedirKind, Seg, Sep, Stage, Syn, SynErr, Word,
};
use crate::lexer::{tokenize, Op, Placed, Scan, Tok};

/// Alias name -> replacement text. Ordered so `alias` lists deterministically.
pub type Aliases = std::collections::BTreeMap<String, String>;

/// Parse with no aliases in force. Every runtime path carries a table, so this
/// is the grammar tests' entry point.
#[cfg(test)]
fn parse(src: &str) -> Syn<List> {
    parse_aliased_at(src, &Aliases::new(), 1)
}

/// Parse all of `src` under one alias table, for the callers that run nothing
/// while parsing: the interactive line and command substitution. `line` numbers
/// the text's first line, which only `$(...)` needs anything but 1 for -- dash
/// reads that body from the outer input rather than re-scanning it as a string.
pub fn parse_aliased_at(src: &str, aliases: &Aliases, line: u32) -> Syn<List> {
    drain(Units::at(src, line), aliases)
}

/// The same for a command-substitution BODY, where a word that only closes a
/// construct is NOT refused. ash reads an old-style `` `…` `` body with
/// `list(2)`, which ends the list at one of them and never checks what is
/// left, so `` echo `fi` `` is empty output and status 0 there; refusing would
/// stop a script ash runs. `$( )` does refuse in ash and does not here, which
/// is the half of this still owed.
pub fn parse_subst_body(src: &str, aliases: &Aliases, line: u32) -> Syn<List> {
    let mut units = Units::at(src, line);
    units.refuse_closers = false;
    drain(units, aliases)
}

/// The interactive reader's probe: does what it has read so far parse, and if
/// not, could another line finish it? Everything else about `parse_aliased_at`,
/// but the text is a snapshot of a source that can still be asked -- so a
/// trailing `\<newline>` is a request for the next line rather than a fold the
/// input ended on. The same buffer is parsed for REAL through `parse_aliased_at`
/// once the reader has stopped asking.
pub fn parse_probe(src: &str, aliases: &Aliases) -> Syn<List> {
    drain(Units::probe(src), aliases)
}

fn drain(mut units: Units, aliases: &Aliases) -> Syn<List> {
    let mut items = Vec::new();
    while let Some(unit) = units.next_unit(aliases) {
        items.extend(unit?.items);
    }
    Ok(List { items })
}

/// Bound on command-nesting depth. Every nested compound (`( … )`, `{ … }`, `if`,
/// loops, `case`, a function body) re-enters `parse_command`, so guarding that one
/// node bounds the recursive-descent stack: pathological input like `((((…))))`
/// errors as a syntax error instead of overflowing the stack and aborting. Real
/// scripts nest a handful deep; this only fires far past any legitimate program.
const MAX_PARSE_DEPTH: u32 = 256;

/// Bound on substitutions per parse unit -- a unit being a whole compound command,
/// however long. The in-use guard already stops an alias re-entering itself, but a
/// chain of aliases that each name several others still expands exponentially.
const MAX_ALIAS_EXPANSIONS: u32 = 4096;

/// The `-X` operators `[[ ]]` takes one operand for: `test`'s roster, plus the
/// three it does not serve. `v` (is this NAME set) has no `test` spelling; `o`
/// (is this shell option on) reads what `set -o` writes; and `a` is file-exists
/// here where in `test` it is the binary AND operator -- inside `[[ ]]` the
/// connective is `&&`, so the letter is free and bash gives it `-e`'s meaning.
/// `G`/`N`/`O` are deliberately absent: they are missing from this shell's
/// `test` too, and adding them belongs there, where both constructs get them.
fn is_cond_unary(w: &str) -> bool {
    matches!(w.strip_prefix('-'), Some(u) if u.len() == 1 && "znefdrwxshLtbcpSugkvoa".contains(u))
}

/// Reserved words that can only CLOSE or continue a construct. Reaching a
/// command position means the construct they belong to is not open, which ash
/// refuses rather than running: `while :; do fi; done` is a syntax error there
/// and an ENDLESS `fi: not found` without this. `case`/`if`/`for`/`while`/
/// `until`/`{` are absent because each does start one.
const CANNOT_START_COMMAND: &[&str] =
    &["then", "else", "elif", "fi", "do", "done", "esac", "in", "}"];

/// What ash takes as the body of a `function NAME` written WITHOUT parentheses
/// (ash.c:12132). `[[` is on it because this build has BASH_TEST2; a word, a
/// `!`, a redirection or a nested definition is a syntax error there rather
/// than a body.
const FUNCTION_BODY_OPENS: &[&str] = &["{", "if", "case", "until", "while", "for", "[["];

/// Reserved words. dash resolves these BEFORE aliases, so `alias if=…` never fires
/// where a keyword is recognized.
pub fn is_reserved(w: &str) -> bool {
    matches!(
        w,
        "if" | "then"
            | "else"
            | "elif"
            | "fi"
            | "do"
            | "done"
            | "case"
            | "esac"
            | "while"
            | "until"
            | "for"
            | "in"
            | "{"
            | "}"
            | "!"
            | "function"
    )
}

/// What the grammar says about the next token, in dash's terms: whether an alias
/// is looked up there at all (CHKALIAS), and whether a reserved word wins (CHKKWD).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Chk {
    /// A command word may start here.
    Command,
    /// The assignment prefix of a simple command, where an alias still expands but
    /// a reserved word is an ordinary word.
    Word,
    /// Nowhere a command starts. Only a blank-terminated replacement reaches here.
    None,
}

/// A replacement spliced into the token stream, live until the parse passes its
/// end: dash's ALIASINUSE, plus the trailing blank it checks when popping one.
struct Region {
    name: String,
    end: usize,
    blank: bool,
}

/// One source's tokens, handed out a parse unit at a time -- dash's `list(1)`:
/// and-or lists up to the first newline the grammar does not consume. Running each
/// unit before the next is parsed is what makes an `alias` visible to the next line
/// but not to the rest of its own, and it costs one lexing pass, not one per line.
pub struct Units {
    toks: Vec<Placed>,
    /// The scan this source's tokens come from, kept alive because it OWNS the
    /// here-document bodies: an id is minted (and a placeholder pushed) when
    /// `<<` is scanned, and the body is written into that slot later.
    scan: Scan,
    /// What stopped the lexer, raised only once a parse needs the text there.
    pending: Option<SynErr>,
    /// Set when a fetch ran off the end of the tokens, which is how `pending`
    /// becomes this unit's error rather than a bare end-of-file one.
    hit_end: bool,
    pos: usize,
    depth: u32,
    budget: u32,
    /// The table this unit is parsed under, copied because the caller runs each
    /// unit -- with `&mut Shell` -- before asking for the next.
    aliases: Aliases,
    active: Vec<Region>,
    /// Index of the token a blank-terminated replacement made a candidate.
    force_at: Option<usize>,
    /// Where more input comes from, for a script arriving a line at a time.
    /// `None` is a whole-string source, which is sealed from the start.
    source: Option<fn() -> More>,
    /// A source that FAILED rather than ended. Kept rather than raised, because
    /// the units already parsed have run and the driver reports it after them.
    source_error: Option<String>,
    /// Set once the scan has yielded its `Eof` or its error. Without it a fetch
    /// past the end would pull again and append a SECOND `Eof` every time.
    spent: bool,
    /// Whether a word that only CLOSES a construct is a syntax error here. Off
    /// for a substitution body; see `parse_subst_body`.
    refuse_closers: bool,
}

/// What a streaming source hands back. `Eof` and `Failed` are distinct because a
/// script whose input broke did not end, and must not be run as though it had.
pub enum More {
    Line(String),
    Eof,
    Failed(String),
}

impl Units {
    pub fn new(src: &str) -> Units {
        Units::at(src, 1)
    }

    /// A whole-string source whose first line is numbered `line`.
    pub fn at(src: &str, line: u32) -> Units {
        let mut scan = Scan::new_at(src, line);
        scan.seal();
        Units::over(scan, None)
    }

    /// The same over a snapshot the reader can still add to; see `parse_probe`.
    /// Sealed as well, because an unsealed scan reports an unfinished construct
    /// by rewinding rather than by erroring, and with no source to ask that
    /// becomes a silent partial parse -- half a line RUN instead of prompted for.
    pub fn probe(src: &str) -> Units {
        let mut scan = Scan::new_at(src, 1);
        scan.seal();
        scan.resumable();
        Units::over(scan, None)
    }

    /// A source read a line at a time, parsed ONCE however many lines a unit
    /// spans: running off the end of the tokens asks `source` for another line
    /// and resumes, rather than re-lexing and re-parsing everything before it.
    pub fn streaming(source: fn() -> More) -> Units {
        Units::over(Scan::new(""), Some(source))
    }

    /// Nothing is pulled here. Every token this parse sees arrives through
    /// `ensure`, so that the scan is asked exactly once for its end: a second
    /// ask after an error returns a fresh `Eof` -- the scanner having consumed
    /// the input the error was found in -- and that `Eof` would mask `hit_end`,
    /// leaving the lexer's error unreported and its half-read command run.
    fn over(scan: Scan, source: Option<fn() -> More>) -> Units {
        Units {
            toks: Vec::new(),
            scan,
            pending: None,
            hit_end: false,
            refuse_closers: true,
            pos: 0,
            depth: 0,
            budget: 0,
            aliases: Aliases::new(),
            active: Vec::new(),
            force_at: None,
            source,
            source_error: None,
            spent: false,
        }
    }

    /// The input failure that ended the source, if it was a failure.
    pub fn source_error(&mut self) -> Option<String> {
        self.source_error.take()
    }

    /// Buffer tokens through index `upto`, reading more input while the scan
    /// says the text so far stops inside a construct. The parser looks ahead at
    /// most one token, so holding this through `pos + 1` is what lets every
    /// `&self` peek keep handing out a borrow into `toks`.
    fn ensure(&mut self, upto: usize) {
        while !self.spent && self.toks.len() <= upto {
            let chunk = self.scan.pull();
            self.toks.extend(chunk.toks);
            if let Some(e) = chunk.error {
                self.pending = Some(e);
                self.spent = true;
                return;
            }
            if !chunk.incomplete {
                self.spent = true;
                return;
            }
            // The pull may already have answered the fetch; asking the source
            // anyway would take a line THIS unit does not need, which on a
            // shared stdin is a line the commands in it were owed.
            if self.toks.len() > upto {
                return;
            }
            let Some(source) = self.source else {
                self.spent = true;
                return;
            };
            match source() {
                More::Line(text) => self.scan.feed(&text),
                More::Eof => self.scan.seal(),
                More::Failed(e) => {
                    self.source_error = Some(e);
                    self.scan.seal();
                }
            }
        }
    }

    /// Read on until here-document `id` has its body. The parser copies the
    /// body into the redirection as it parses, but the lexer only fills it at
    /// the END of the operator's line -- which under a streaming source is a
    /// later pull than the one that produced the `<<` token.
    fn ensure_heredoc(&mut self, id: usize) {
        while !self.spent && self.scan.heredoc_pending(id) {
            let chunk = self.scan.pull();
            self.toks.extend(chunk.toks);
            if let Some(e) = chunk.error {
                self.pending = Some(e);
                self.spent = true;
                return;
            }
            if !chunk.incomplete {
                self.spent = true;
                return;
            }
            if !self.scan.heredoc_pending(id) {
                return;
            }
            let Some(source) = self.source else {
                self.spent = true;
                return;
            };
            match source() {
                More::Line(text) => self.scan.feed(&text),
                More::Eof => self.scan.seal(),
                More::Failed(e) => {
                    self.source_error = Some(e);
                    self.scan.seal();
                }
            }
        }
    }

    /// Buffer through the end of the input LINE starting at `from`, so
    /// `line_end` finds the real newline rather than the end of what has been
    /// read. Only an alias replacement that trails off in a comment needs it.
    fn ensure_line(&mut self, from: usize) {
        let mut i = from;
        loop {
            self.ensure(i);
            match self.toks.get(i).map(|p| &p.tok) {
                None | Some(Tok::Newline) | Some(Tok::Eof) => return,
                _ => i += 1,
            }
        }
    }

    /// The next unit, or `None` at the end of the source.
    pub fn next_unit(&mut self, aliases: &Aliases) -> Option<Syn<List>> {
        self.aliases.clone_from(aliases);
        // `active`/`force_at` deliberately survive: a replacement containing a
        // newline spans the boundary, and dash keeps ALIASINUSE across its units.
        self.depth = 0;
        self.budget = MAX_ALIAS_EXPANSIONS;
        let parsed = self.parse_unit();
        if self.hit_end {
            // The parse reached where lexing stopped, so what stopped it is the
            // real error -- and everything before here has already run.
            if let Some(e) = self.pending.take() {
                return Some(Err(e));
            }
        }
        match parsed {
            Ok(list) if list.items.is_empty() => None,
            other => Some(other),
        }
    }

    fn parse_unit(&mut self) -> Syn<List> {
        let mut items = Vec::new();
        loop {
            self.open_command()?;
            if self.at_eof() {
                return Ok(List { items });
            }
            let and_or = self.parse_and_or()?;
            let (sep, explicit) = match self.peek_op() {
                Some(Op::Semi) => {
                    self.bump();
                    (Sep::Seq, true)
                }
                Some(Op::Amp) => {
                    self.bump();
                    (Sep::Bg, true)
                }
                _ => (Sep::Seq, false),
            };
            items.push((and_or, sep));
            if matches!(self.peek(), Some(Tok::Newline)) {
                self.bump();
                return Ok(List { items });
            }
            if !explicit {
                break;
            }
        }
        if self.at_eof() {
            return Ok(List { items });
        }
        Err(self.unexpected(None))
    }

    /// A CHKNL|CHKKWD|CHKALIAS point: a newline is not a token where a command may
    /// start, and a replacement can leave more of them behind -- one ending in a
    /// comment eats the rest of its line -- so the two go round together.
    fn open_command(&mut self) -> Syn<()> {
        loop {
            self.skip_newlines();
            if !self.check_alias(Chk::Command)? {
                return Ok(());
            }
        }
    }

    /// dash's CHKALIAS point: substitute the alias named by the token at `pos`,
    /// rescanning the replacement in place so it can name further aliases. Reports
    /// whether anything was substituted.
    fn check_alias(&mut self, chk: Chk) -> Syn<bool> {
        let mut any = false;
        loop {
            self.settle();
            let Some((name, value)) = self.alias_here(chk) else {
                return Ok(any);
            };
            self.expand(&name, &value)?;
            any = true;
        }
    }

    /// The alias the token at `pos` names, if this position may take one: where the
    /// grammar allows one, or on the token after a blank-terminated replacement --
    /// dash sets its check when popping one, wherever that lands.
    fn alias_here(&mut self, chk: Chk) -> Option<(String, String)> {
        if chk == Chk::None && self.force_at != Some(self.pos) {
            return None;
        }
        // Through the fetch, not `toks` directly: under a streaming source the
        // candidate may not be buffered yet, and reading the vector would miss
        // it. `A \<newline> B` is exactly that -- the fold ends the fed line, so
        // B arrives only when asked for.
        self.ensure(self.pos);
        // Only an unquoted literal word is a candidate: `'hi'` and `$cmd` are not.
        let Some(Tok::Word(w)) = self.toks.get(self.pos).map(|p| &p.tok) else {
            return None;
        };
        let name = w.plain()?;
        if (chk == Chk::Command && is_reserved(name)) || self.active.iter().any(|r| r.name == name) {
            return None;
        }
        let value = self.aliases.get(name)?.clone();
        Some((name.to_string(), value))
    }

    /// Splice a replacement in place of the alias word, so the grammar sees it: an
    /// alias may supply syntax (`alias L='{'`), which is why substitution has to
    /// happen before the parser reads the token rather than after.
    fn expand(&mut self, name: &str, value: &str) -> Syn<()> {
        self.budget = self
            .budget
            .checked_sub(1)
            .ok_or_else(|| "alias expansion too deep".to_string())?;
        // The replacement stands where the NAME stood, so it is LEXED from
        // there: dash reads an alias body off the same input, so `$LINENO`
        // inside one -- and inside a `$( )` inside one -- is the line the alias
        // was invoked on, and a body with a newline in it reports the next.
        // Where dash goes further and this shell does not is the REST of the
        // script: dash shifts it by the body's newlines, so a two-line body
        // makes the file's line 4 report 5.
        let line = self.toks.get(self.pos).map_or(1, |p| p.line);
        // Neither this nor the unclosed here-document below is incomplete input
        // however it failed: no amount of further input can complete a
        // replacement, so the unit loop must stop rather than read on. Both go
        // through `From<String>`, which clears the flag whatever the wrapped
        // message says.
        let lexed = tokenize(value, line).map_err(|e| format!("alias `{name}': {e}"))?;
        // A here-document the replacement OPENED but did not close is refused
        // rather than run. ash reads that body from the enclosing input, this
        // splices TOKENS and cannot; ending it here would give the alias an
        // empty body and hand the script's here-document DATA back to the
        // parser as commands.
        if let Some(delim) = lexed.heredoc_ran_out {
            return Err(SynErr::from(format!(
                "alias `{name}': syntax error: unexpected end of file (expecting {delim:?})"
            )));
        }
        let base = self.scan.push_heredocs(lexed.heredocs);
        let sub: Vec<Placed> = lexed
            .toks
            .into_iter()
            .filter(|p| !matches!(p.tok, Tok::Eof))
            // Renumber onto the end of this source's table so a here-document
            // written inside an alias still resolves.
            .map(|p| match p.tok {
                Tok::Op(Op::DLess(id)) => Placed { line: p.line, tok: Tok::Op(Op::DLess(id + base)) },
                tok => Placed { line: p.line, tok },
            })
            .collect();
        let end = self.pos + sub.len();
        // A replacement that trails off inside a comment comments out the rest of
        // the line it was written on: dash reads the replacement from the same
        // input stream, so the comment runs to the next newline of the INPUT.
        let over = if lexed.ended_in_comment {
            self.ensure_line(self.pos + 1);
            self.line_end(self.pos + 1)
        } else {
            self.pos + 1
        };
        self.splice(self.pos, over, sub);
        self.active.push(Region {
            name: name.to_string(),
            end,
            blank: value.ends_with([' ', '\t']),
        });
        Ok(())
    }

    /// Replace `toks[at..over]` with `sub`, carrying the recorded replacement ends
    /// across the shift.
    fn splice(&mut self, at: usize, over: usize, sub: Vec<Placed>) {
        let at = at.min(self.toks.len());
        let over = over.max(at).min(self.toks.len());
        let added = sub.len();
        self.toks.splice(at..over, sub);
        for r in &mut self.active {
            r.end = reindex(r.end, at, over, added);
        }
    }

    /// The newline ending the line token `from` sits on, or the end of the stream.
    fn line_end(&self, from: usize) -> usize {
        let mut i = from;
        while !matches!(
            self.toks.get(i).map(|p| &p.tok),
            None | Some(Tok::Newline) | Some(Tok::Eof)
        ) {
            i += 1;
        }
        i
    }

    /// dash's popstring: drop the replacements the parse has passed the end of.
    /// Their names become expandable again, and one that ended in a blank makes the
    /// token landed on a candidate wherever it sits.
    fn settle(&mut self) {
        while self.active.last().is_some_and(|r| r.end <= self.pos) {
            if self.active.pop().is_some_and(|r| r.blank) {
                self.force_at = Some(self.pos);
            }
        }
    }

    /// The input line of the token at `pos`. Off the end it is the last token's,
    /// so a line is never invented: the caller is about to fail the parse there.
    fn line(&mut self) -> u32 {
        self.ensure(self.pos);
        self.toks
            .get(self.pos)
            .or_else(|| self.toks.last())
            .map_or(1, |p| p.line)
    }

    /// The line a failed parse stopped on. ash sets `errlinno` from the
    /// parser's own position (`raise_error_syntax`, ash.c:1477) rather than
    /// from the last command's, which is what `$LINENO` still holds -- so
    /// without this a syntax error carries the line of whatever ran BEFORE it,
    /// which can be anywhere.
    pub fn error_line(&mut self) -> u32 {
        self.line()
    }

    fn tok_at(&mut self, i: usize) -> Option<&Tok> {
        // The refill belongs HERE and not one fetch earlier: filling ahead of
        // the fetch reads a line the current unit does not need, which for a
        // shared stdin is a line the commands in it were owed.
        self.ensure(i);
        let tok = self.toks.get(i).map(|p| &p.tok);
        if tok.is_none() {
            self.hit_end = true;
        }
        tok
    }

    fn peek(&mut self) -> Option<&Tok> {
        self.tok_at(self.pos)
    }

    fn bump(&mut self) {
        if self.pos < self.toks.len() {
            self.pos += 1;
        }
        self.settle();
    }

    fn at_eof(&mut self) -> bool {
        matches!(self.peek(), None | Some(Tok::Eof))
    }

    fn peek_op(&mut self) -> Option<Op> {
        match self.peek() {
            Some(Tok::Op(op)) => Some(*op),
            _ => None,
        }
    }

    /// The next token's text when it is an unquoted literal word — the only
    /// shape that can be a reserved word.
    fn peek_reserved(&mut self) -> Option<&str> {
        match self.peek() {
            Some(Tok::Word(w)) => w.plain(),
            _ => None,
        }
    }

    /// How ash NAMES the token a parse stopped on (`tokname`, ash.c:11895):
    /// the four classes -- `word`, `newline`, `redirection`, `end of file` --
    /// bare, and anything with a spelling of its own in quotes. `kw` is whether
    /// this position recognises keywords, which is what makes `do` `"do"` after
    /// a list and `word` in a `case` pattern: ash names what was LEXED, and its
    /// lexer is told per position.
    fn tokname(&mut self, kw: bool) -> String {
        match self.peek() {
            None | Some(Tok::Eof) => "end of file".to_string(),
            Some(Tok::Newline) => "newline".to_string(),
            Some(Tok::IoNumber(_)) => "redirection".to_string(),
            Some(Tok::Op(op)) if op.is_redirect() => "redirection".to_string(),
            Some(Tok::Op(op)) => format!("\"{}\"", op.text()),
            Some(Tok::Word(w)) => match w.plain() {
                Some(t) if kw && is_reserved(t) => format!("\"{t}\""),
                _ => "word".to_string(),
            },
        }
    }

    /// ash's `raise_error_unexpected_syntax`: the token that stopped the parse,
    /// and the one the grammar was owed where there is one to name.
    fn unexpected(&mut self, expecting: Option<&str>) -> SynErr {
        self.unexpected_named(expecting, true)
    }

    /// The same from a WORD position, where a reserved word is just a word.
    fn unexpected_in_word(&mut self, expecting: Option<&str>) -> SynErr {
        self.unexpected_named(expecting, false)
    }

    fn unexpected_named(&mut self, expecting: Option<&str>, kw: bool) -> SynErr {
        let tok = self.tokname(kw);
        let msg = match expecting {
            Some(want) => format!("syntax error: unexpected {tok} (expecting {want})"),
            None => format!("syntax error: unexpected {tok}"),
        };
        // `end of file` is the input running out, which another line completes;
        // every other name is a token that arrived, which none does.
        if self.at_eof() {
            SynErr::incomplete(msg)
        } else {
            msg.into()
        }
    }

    fn describe(&mut self) -> String {
        match self.peek() {
            None | Some(Tok::Eof) => "end of input".to_string(),
            Some(Tok::Newline) => "newline".to_string(),
            Some(Tok::Op(op)) => format!("`{}`", op.text()),
            Some(Tok::IoNumber(n)) => format!("`{n}`"),
            Some(Tok::Word(w)) => match w.plain() {
                Some(t) => format!("`{t}`"),
                None => "a word".to_string(),
            },
        }
    }

    fn skip_newlines(&mut self) {
        while matches!(self.peek(), Some(Tok::Newline)) {
            self.bump();
        }
    }

    fn expect_op(&mut self, want: Op) -> Syn<()> {
        self.expect_op_named(want, true)
    }

    /// `expect_op` from a WORD position -- a `case` pattern's `)`, where ash
    /// names a reserved word `word` because its lexer was not told to look.
    fn expect_op_in_word(&mut self, want: Op) -> Syn<()> {
        self.expect_op_named(want, false)
    }

    fn expect_op_named(&mut self, want: Op, kw: bool) -> Syn<()> {
        if self.peek_op() == Some(want) {
            self.bump();
            return Ok(());
        }
        let want = format!("\"{}\"", want.text());
        Err(self.unexpected_named(Some(&want), kw))
    }

    fn expect_reserved(&mut self, want: &str) -> Syn<()> {
        if self.peek_reserved() == Some(want) {
            self.bump();
            return Ok(());
        }
        let want = format!("\"{want}\"");
        Err(self.unexpected(Some(&want)))
    }

    fn take_word(&mut self, expecting: Option<&str>) -> Syn<Word> {
        // No command starts here (a redirection target, a `case` pattern, a `for`
        // list word), but a blank-terminated replacement still reaches this far.
        self.check_alias(Chk::None)?;
        match self.peek() {
            Some(Tok::Word(w)) => {
                let w = w.clone();
                self.bump();
                Ok(w)
            }
            // A word POSITION, so a reserved word would be named `word` --
            // which nothing reaching here can be, the `Word` arm above having
            // matched first. End of file names itself and needs no arm.
            _ => Err(self.unexpected_in_word(expecting)),
        }
    }

    /// True where a list ends: at EOF, at `)`, at `;;`, or at one of the
    /// caller's closing reserved words.
    fn at_list_end(&mut self, terms: &[&str]) -> bool {
        match self.peek() {
            None | Some(Tok::Eof) => true,
            Some(Tok::Op(Op::RParen)) | Some(Tok::Op(Op::DSemi)) => true,
            Some(Tok::Word(w)) => w.plain().is_some_and(|s| terms.contains(&s)),
            _ => false,
        }
    }

    fn parse_list(&mut self, terms: &[&str]) -> Syn<List> {
        let mut items = Vec::new();
        loop {
            // Before the terminator test, not after: `alias t=then` has to become
            // `then` for `if …; t …; fi` to see the arm end.
            self.open_command()?;
            if self.at_list_end(terms) {
                break;
            }
            let and_or = self.parse_and_or()?;
            let (sep, explicit) = match self.peek_op() {
                Some(Op::Semi) => {
                    self.bump();
                    (Sep::Seq, true)
                }
                Some(Op::Amp) => {
                    self.bump();
                    (Sep::Bg, true)
                }
                _ => (Sep::Seq, false),
            };
            items.push((and_or, sep));
            if !explicit && !matches!(self.peek(), Some(Tok::Newline)) {
                break;
            }
        }
        Ok(List { items })
    }

    fn parse_and_or(&mut self) -> Syn<AndOr> {
        let first = self.parse_pipeline()?;
        let mut rest = Vec::new();
        loop {
            let conn = match self.peek_op() {
                Some(Op::AndIf) => Conn::And,
                Some(Op::OrIf) => Conn::Or,
                _ => break,
            };
            self.bump();
            self.open_command()?;
            rest.push((conn, self.parse_pipeline()?));
        }
        Ok(AndOr { first, rest })
    }

    fn parse_pipeline(&mut self) -> Syn<Pipeline> {
        // Every command in the grammar is reached through here, so this is the one
        // place a command word's alias check has to be made.
        //
        // ONE `!`, because ash's `pipeline()` reads it with an `if` and not a
        // loop (ash.c:11957). A second reaches a command position, which
        // `parse_command_inner` refuses -- the word after the `!` is a command
        // word, which is why the alias check runs on both sides of it.
        self.check_alias(Chk::Command)?;
        let bang = self.peek_reserved() == Some("!");
        if bang {
            self.bump();
            self.check_alias(Chk::Command)?;
        }
        let mut cmds = vec![self.staged()?];
        while self.peek_op() == Some(Op::Pipe) {
            self.bump();
            self.open_command()?;
            cmds.push(self.staged()?);
        }
        Ok(Pipeline { bang, cmds })
    }

    /// One command with the line it opens on, read BEFORE the command is
    /// parsed -- afterwards the position is past it, and a command that spans
    /// lines would be stamped with the line it ended on.
    fn staged(&mut self) -> Syn<Stage> {
        let line = self.line();
        let cmd = self.parse_command()?;
        Ok(Stage { line, cmd })
    }

    /// Depth-guarded entry to command parsing. Every recursive descent through
    /// the COMMAND grammar bottoms out here, so this counter bounds its stack
    /// use. `cond_term` shares the same counter for the same reason: its
    /// parentheses recurse without passing through here.
    fn parse_command(&mut self) -> Syn<Cmd> {
        self.depth += 1;
        if self.depth > MAX_PARSE_DEPTH {
            self.depth -= 1;
            return Err("syntax error: command nesting too deep".into());
        }
        let result = self.parse_command_inner();
        self.depth -= 1;
        result
    }

    fn parse_command_inner(&mut self) -> Syn<Cmd> {
        if self.peek_op() == Some(Op::LParen) {
            self.bump();
            let body = self.parse_list(&[])?;
            self.expect_op(Op::RParen)?;
            let redirs = self.parse_redirs()?;
            return Ok(Cmd::Subshell { body, redirs });
        }
        match self.peek_reserved() {
            Some("if") => return self.parse_if(),
            Some("while") => return self.parse_loop(false),
            Some("until") => return self.parse_loop(true),
            Some("for") => return self.parse_for(),
            Some("case") => return self.parse_case(),
            Some("function") => return self.parse_function_word_def(),
            Some("{") => {
                self.bump();
                let body = self.parse_list(&["}"])?;
                self.expect_reserved("}")?;
                let redirs = self.parse_redirs()?;
                return Ok(Cmd::Group { body, redirs });
            }
            _ => {}
        }
        // `[[` is dispatched here rather than through `is_reserved`, which would
        // also stop it being a command NAME -- and `[[` reaching a PATH lookup
        // is what any shell without this does, so the word keeps that meaning
        // everywhere except the one position the grammar claims it.
        if self.peek_reserved() == Some("[[") {
            return self.parse_cond();
        }
        // A second `!`, or one opening a stage that is not the head. Ungated
        // unlike the closers below: `TNOT` carries no `tokendlist` bit
        // (ash.c:8657), so it does not end a backtick body the way they do.
        if self.peek_reserved() == Some("!") {
            return Err("syntax error: unexpected \"!\"".into());
        }
        // Before the function-definition test, or `fi() { … }` defines one where
        // ash refuses it. The text is ash's spelling exactly.
        let refuse = self.refuse_closers;
        if let Some(w) = self.peek_reserved() {
            if refuse && CANNOT_START_COMMAND.contains(&w) {
                return Err(format!("syntax error: unexpected \"{w}\"").into());
            }
        }
        if self.at_func_def() {
            return self.parse_func_def();
        }
        self.parse_simple()
    }

    /// `[[ expr ]]`.
    ///
    /// Parsed off the ORDINARY token stream rather than through a second lexer
    /// mode: `[[` and `]]` arrive as plain words, and the operators the shell
    /// spells with punctuation (`<`, `>`, `&&`, `||`, `(`, `)`) arrive as the
    /// same `Op`s a command line uses. Reading them back as comparisons here is
    /// what makes `[[ a < b ]]` a string test where `a < b` is a redirection,
    /// with no change to how anything outside the brackets lexes.
    fn parse_cond(&mut self) -> Syn<Cmd> {
        self.expect_reserved("[[")?;
        let expr = self.cond_or()?;
        // A newline is skipped where a TERM is expected (see `cond_term_inner`)
        // and refused everywhere else, which is bash's rule. Here the
        // expression is COMPLETE, so what may follow is `]]`; bash calls a
        // newline in this position an error too, and saying which beats
        // "expected `]]`, found newline".
        if matches!(self.peek(), Some(Tok::Newline)) {
            return Err("syntax error: `[[` expression ended before `]]`".into());
        }
        self.expect_reserved("]]")?;
        let redirs = self.parse_redirs()?;
        Ok(Cmd::Cond { expr, redirs })
    }

    /// The chains are bounded as well as the nesting, and by the same constant.
    /// Parsing `a && b && c` iterates rather than recursing, but the TREE it
    /// builds is left-deep -- one level per term -- and both the evaluator and
    /// the tree's own `Drop` walk that spine recursively. A 100k-term chain
    /// therefore aborted the shell exactly as nested parentheses did, with
    /// nothing in the parser's stack to show for it. Bounding the tree at parse
    /// time is what makes the depth `eval_cond` and `Drop` see finite.
    fn cond_or(&mut self) -> Syn<CondExpr> {
        let mut left = self.cond_and()?;
        let mut chain = 0u32;
        while self.peek_op() == Some(Op::OrIf) {
            chain += 1;
            if chain > MAX_PARSE_DEPTH {
                return Err("syntax error: `[[` expression too long".into());
            }
            self.bump();
            let right = self.cond_and()?;
            left = CondExpr::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn cond_and(&mut self) -> Syn<CondExpr> {
        let mut left = self.cond_term()?;
        let mut chain = 0u32;
        while self.peek_op() == Some(Op::AndIf) {
            chain += 1;
            if chain > MAX_PARSE_DEPTH {
                return Err("syntax error: `[[` expression too long".into());
            }
            self.bump();
            let right = self.cond_term()?;
            left = CondExpr::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    /// Depth-guarded, sharing `parse_command`'s counter. `( … )` and `!` recurse
    /// here WITHOUT going through `parse_command`, so without this the bound
    /// that stops `((((…))))` overflowing the stack would not cover the same
    /// shape inside `[[ ]]` -- and a stack overflow is the shell dying rather
    /// than a diagnostic, whatever the panic strategy. Verified: 100k
    /// nested parens abort without it and are a syntax error with it.
    fn cond_term(&mut self) -> Syn<CondExpr> {
        self.depth += 1;
        if self.depth > MAX_PARSE_DEPTH {
            self.depth -= 1;
            return Err("syntax error: `[[` nesting too deep".into());
        }
        let result = self.cond_term_inner();
        self.depth -= 1;
        result
    }

    fn cond_term_inner(&mut self) -> Syn<CondExpr> {
        // A newline is skipped exactly where a TERM is expected, which is what
        // reaching this function means: after `[[`, after `&&`, after `||`,
        // after `(` and after `!`. It is NOT skipped where an OPERAND or the
        // closer is expected -- after a binary or unary operator, before `)`,
        // before `]]` -- and those are in `cond_operand` and the callers.
        // Measured against bash 5.2 in all ten positions rather than assumed:
        // it accepts the first five and refuses the rest.
        while matches!(self.peek(), Some(Tok::Newline)) {
            self.bump();
        }
        if self.peek_reserved() == Some("!") {
            self.bump();
            return Ok(CondExpr::Not(Box::new(self.cond_term()?)));
        }
        if self.peek_op() == Some(Op::LParen) {
            self.bump();
            let inner = self.cond_or()?;
            self.expect_op(Op::RParen)?;
            return Ok(inner);
        }
        // A unary operator claims the word after it. Checked before the binary
        // shape because `-n` and `-z` take one operand where `-eq` takes two,
        // and both start with `-`.
        if let Some(op) = self.peek_reserved().filter(|w| is_cond_unary(w)) {
            let op = op.to_string();
            self.bump();
            let arg = self.cond_operand()?;
            return Ok(CondExpr::Unary { op, arg });
        }
        let lhs = self.cond_operand()?;
        let Some(op) = self.peek_cond_op() else {
            return Ok(CondExpr::Word(lhs));
        };
        self.bump();
        let rhs = self.cond_operand()?;
        Ok(CondExpr::Binary { op, lhs, rhs })
    }

    /// The binary operator at the cursor, whether it arrived as a word (`==`,
    /// `-eq`) or as punctuation the lexer already claimed (`<`, `>`).
    fn peek_cond_op(&mut self) -> Option<CondOp> {
        match self.peek_op() {
            Some(Op::Less) => return Some(CondOp::Before),
            Some(Op::Great) => return Some(CondOp::After),
            _ => {}
        }
        let w = self.peek_reserved()?;
        Some(match w {
            "==" | "=" => CondOp::Match,
            "!=" => CondOp::NoMatch,
            "=~" => CondOp::Regex,
            "-eq" => CondOp::Arith(ArithCmp::Eq),
            "-ne" => CondOp::Arith(ArithCmp::Ne),
            "-lt" => CondOp::Arith(ArithCmp::Lt),
            "-le" => CondOp::Arith(ArithCmp::Le),
            "-gt" => CondOp::Arith(ArithCmp::Gt),
            "-ge" => CondOp::Arith(ArithCmp::Ge),
            "-ef" => CondOp::File("-ef"),
            "-nt" => CondOp::File("-nt"),
            "-ot" => CondOp::File("-ot"),
            _ => return None,
        })
    }

    /// One operand word. `]]` is refused here so a missing operand is reported
    /// against the operator that wanted it rather than swallowing the closer and
    /// failing at end of input.
    fn cond_operand(&mut self) -> Syn<Word> {
        match self.peek() {
            Some(Tok::Word(w)) if w.plain() != Some("]]") => {
                let w = w.clone();
                self.bump();
                Ok(w)
            }
            // An `IoNumber` is digits written HARD AGAINST a redirection
            // operator, so inside `[[ ]]` it can only come from `2>1` -- and
            // bash calls that a conditional syntax error rather than a
            // comparison of "2" with "1". Turning it back into a word would
            // silently answer a question the user did not ask. (`[[ 2 -gt 1 ]]`
            // never reaches here: the space makes `2` an ordinary word.)
            Some(Tok::IoNumber(_)) => {
                Err("syntax error: unexpected redirection inside `[[`".into())
            }
            // NOT ash's `missing ]]`: there that is the test builtin at RUN
            // time and the shell survives it, so borrowing the words would
            // describe a parse error as something it is not.
            None | Some(Tok::Eof) => Err(self.unexpected(Some("\"]]\""))),
            _ => Err(format!(
                "syntax error: expected an operand inside `[[`, found {}",
                self.describe()
            ).into()),
        }
    }

    fn at_func_def(&mut self) -> bool {
        let Some(Tok::Word(w)) = self.peek() else {
            return false;
        };
        if !w.plain().is_some_and(is_name) {
            return false;
        }
        matches!(self.tok_at(self.pos + 1), Some(Tok::Op(Op::LParen)))
    }

    fn parse_func_def(&mut self) -> Syn<Cmd> {
        let name = match self.peek() {
            Some(Tok::Word(w)) => w.plain().unwrap_or_default().to_string(),
            _ => return Err("syntax error: expected a function name".into()),
        };
        self.bump();
        self.expect_op(Op::LParen)?;
        // The `)` TOKEN's line, which is where dash takes it (`ndefun.linno =
        // plinno` at parser.c:569, with `)` just read). Not the NAME's, so a
        // definition folded across a `\` counts from the parentheses; and not
        // the BODY's, so `f()` with its `{` on the next line still counts from
        // the `f()`. Both spellings measured.
        let line = self.line();
        self.expect_op(Op::RParen)?;
        // The body is a command position like any other, so an alias may supply it:
        // `alias B='{ echo yes; }'` then `f()` / `B`.
        self.open_command()?;
        self.func_body(Some(name), line)
    }

    /// The body both spellings share, its position already opened by the
    /// caller. They share it because ash does: `()` clears the `function` flag
    /// and reaches the same `do_func` the bare form does (ash.c:12163).
    fn func_body(&mut self, name: Option<String>, line: u32) -> Syn<Cmd> {
        let body = self.staged()?;
        if matches!(body.cmd, Cmd::Simple { .. }) {
            return Err(match &name {
                Some(n) => {
                    format!("syntax error: the body of function `{n}` must be a compound command")
                }
                None => "syntax error: a function body must be a compound command".to_string(),
            }
            .into());
        }
        Ok(Cmd::FuncDef {
            name,
            body: Arc::new(body),
            line,
        })
    }

    /// `function NAME [()] <compound>` — bash's spelling, which ash takes when
    /// built with BASH_FUNCTION and the reference build is. WITH the
    /// parentheses ash clears its `function_flag` (ash.c:12144) and rejoins the
    /// ordinary `NAME()` path; without them only `FUNCTION_BODY_OPENS` follows,
    /// and the NAME is any word rather than an `is_name` one either way.
    fn parse_function_word_def(&mut self) -> Syn<Cmd> {
        self.bump();
        // ash reads the NAME with `savecheckkwd` (ash.c:12085), which is
        // CHKALIAS and not CHKKWD: an alias fires here, a keyword does not.
        self.check_alias(Chk::Word)?;
        // The `)`'s line for the bare spelling, so the NAME's here: taking it
        // anywhere later makes the two disagree about `$LINENO` in a body.
        let mut line = self.line();
        // `plain` is None for a quoted or expanded name, which ash takes as
        // readily -- see `FuncDef::name`. The assignment test is asked of the
        // WORD, since `a="$x"` is that shape too.
        let taken = match self.peek() {
            Some(Tok::Word(w)) => Some((w.plain().map(str::to_string), as_assignment(w).is_some())),
            _ => None,
        };
        let Some((name, assignment)) = taken else {
            // ash passes TWORD whatever stopped it (ash.c:12095), so the
            // expectation stands at end of file as well.
            return Err(self.unexpected(Some("word")));
        };
        // ash files an assignment-shaped name as a VARIABLE, leaving `do_func`
        // (ash.c:12163) without the one argument it tests for.
        if assignment {
            // ash files the word as a VARIABLE and stops on what follows, so
            // the token it names is the body's, not the name's.
            self.bump();
            return Err(self.unexpected(None));
        }
        // A command word with a `/` in it is never a function LOOKUP, so a
        // definition under one is as unreachable as an unspellable name.
        let name = name.filter(|n| !n.contains('/'));
        self.bump();
        // CHKNL|CHKKWD (ash.c:12133): newlines are skipped and an alias does
        // NOT fire. The `()` spelling rejoins the ordinary path below, where
        // one does.
        self.skip_newlines();
        if self.peek_op() == Some(Op::LParen) {
            self.bump();
            line = self.line();
            self.expect_op(Op::RParen)?;
            self.open_command()?;
        } else if !self
            .peek_reserved()
            .is_some_and(|w| FUNCTION_BODY_OPENS.contains(&w))
        {
            return Err(self.unexpected(None));
        }
        self.func_body(name, line)
    }

    fn parse_if(&mut self) -> Syn<Cmd> {
        self.expect_reserved("if")?;
        let mut arms = Vec::new();
        let cond = self.parse_list(&["then"])?;
        self.expect_reserved("then")?;
        let body = self.parse_list(&["elif", "else", "fi"])?;
        arms.push(IfArm { cond, body });
        let mut otherwise = None;
        loop {
            match self.peek_reserved() {
                Some("elif") => {
                    self.bump();
                    let cond = self.parse_list(&["then"])?;
                    self.expect_reserved("then")?;
                    let body = self.parse_list(&["elif", "else", "fi"])?;
                    arms.push(IfArm { cond, body });
                }
                Some("else") => {
                    self.bump();
                    otherwise = Some(self.parse_list(&["fi"])?);
                    break;
                }
                _ => break,
            }
        }
        self.expect_reserved("fi")?;
        let redirs = self.parse_redirs()?;
        Ok(Cmd::If {
            arms,
            otherwise,
            redirs,
        })
    }

    fn parse_loop(&mut self, until: bool) -> Syn<Cmd> {
        self.expect_reserved(if until { "until" } else { "while" })?;
        let cond = self.parse_list(&["do"])?;
        self.expect_reserved("do")?;
        let body = self.parse_list(&["done"])?;
        self.expect_reserved("done")?;
        let redirs = self.parse_redirs()?;
        Ok(Cmd::Loop {
            until,
            cond,
            body,
            redirs,
        })
    }

    fn parse_for(&mut self) -> Syn<Cmd> {
        self.expect_reserved("for")?;
        // dash reads the loop variable with its keyword and alias checks off, but a
        // blank-terminated replacement still reaches it: `alias F='for '` then
        // `F eye ...` expands `eye`.
        self.check_alias(Chk::None)?;
        let var = match self.peek() {
            Some(Tok::Word(w)) => match w.plain() {
                Some(n) if is_name(n) => n.to_string(),
                _ => return Err("syntax error: bad for loop variable".into()),
            },
            _ => return Err("syntax error: bad for loop variable".into()),
        };
        self.bump();
        self.open_command()?;
        let words = if self.peek_reserved() == Some("in") {
            self.bump();
            let mut ws = Vec::new();
            loop {
                // Before the test, not inside `take_word`: a replacement can be the
                // operator that ENDS the list (`alias S=';'`).
                self.check_alias(Chk::None)?;
                if !matches!(self.peek(), Some(Tok::Word(_))) {
                    break;
                }
                ws.push(self.take_word(Some("word"))?);
            }
            // A list that stopped on something that is neither a separator
            // nor a newline is the error ITSELF here, before any `do` is owed.
            let stopped = !matches!(self.peek(), Some(Tok::Newline))
                && self.peek_op() != Some(Op::Semi)
                && !self.at_eof();
            if stopped {
                return Err(self.unexpected(None));
            }
            Some(ws)
        } else {
            None
        };
        if self.peek_op() == Some(Op::Semi) {
            self.bump();
        }
        self.open_command()?;
        self.expect_reserved("do")?;
        let body = self.parse_list(&["done"])?;
        self.expect_reserved("done")?;
        let redirs = self.parse_redirs()?;
        Ok(Cmd::For {
            var,
            words,
            body,
            redirs,
        })
    }

    fn parse_case(&mut self) -> Syn<Cmd> {
        self.expect_reserved("case")?;
        let word = self.take_word(Some("word"))?;
        self.open_command()?;
        self.expect_reserved("in")?;
        let mut items = Vec::new();
        loop {
            self.skip_newlines();
            self.check_alias(Chk::None)?;
            if self.peek_reserved() == Some("esac") {
                break;
            }
            if self.at_eof() {
                return Err(self.unexpected(Some("\"esac\"")));
            }
            if self.peek_op() == Some(Op::LParen) {
                self.bump();
            }
            let mut patterns = vec![self.take_word(Some("word"))?];
            while self.peek_op() == Some(Op::Pipe) {
                self.bump();
                patterns.push(self.take_word(Some("word"))?);
            }
            self.expect_op_in_word(Op::RParen)?;
            let body = self.parse_list(&["esac"])?;
            items.push(CaseItem { patterns, body });
            if self.peek_op() == Some(Op::DSemi) {
                self.bump();
            } else {
                break;
            }
        }
        self.skip_newlines();
        self.expect_reserved("esac")?;
        let redirs = self.parse_redirs()?;
        Ok(Cmd::Case {
            word,
            items,
            redirs,
        })
    }

    fn parse_simple(&mut self) -> Syn<Cmd> {
        let mut assigns: Vec<Assign> = Vec::new();
        let mut words: Vec<Word> = Vec::new();
        let mut redirs: Vec<Redir> = Vec::new();
        loop {
            // dash holds the alias check open across the whole assignment prefix
            // and its redirections, so `FOO=1 al` and `>f al` expand -- but with
            // keywords off past the first word, so `FOO=1 if` is a command named
            // `if`. Past the command word only a blank-terminated replacement
            // still reaches an argument.
            self.check_alias(if !words.is_empty() {
                Chk::None
            } else if assigns.is_empty() && redirs.is_empty() {
                Chk::Command
            } else {
                Chk::Word
            })?;
            match self.peek() {
                Some(Tok::IoNumber(n)) => {
                    let n = *n;
                    self.bump();
                    redirs.push(self.parse_redir(Some(n))?);
                }
                Some(Tok::Op(op)) if op.is_redirect() => {
                    redirs.push(self.parse_redir(None)?);
                }
                Some(Tok::Word(w)) => {
                    let w = w.clone();
                    // Only a leading run of `name=value` words are assignments;
                    // after the command word they are ordinary arguments.
                    if words.is_empty() {
                        if let Some(a) = as_assignment(&w) {
                            assigns.push(a);
                            self.bump();
                            continue;
                        }
                    }
                    words.push(w);
                    self.bump();
                }
                _ => break,
            }
        }
        if assigns.is_empty() && words.is_empty() && redirs.is_empty() {
            // Running out of input where a command was due is incomplete, not
            // wrong -- `f ()` on its own line still has its body coming -- and
            // `unexpected` reads that off the token it names.
            return Err(self.unexpected(None));
        }
        Ok(Cmd::Simple {
            assigns,
            words,
            redirs,
        })
    }

    fn parse_redirs(&mut self) -> Syn<Vec<Redir>> {
        let mut redirs = Vec::new();
        // Only the FIRST token after a compound command is checked, which is what
        // dash's one `checkkwd` before its redirection loop amounts to: an alias
        // may carry the redirection itself (`alias R='>/dev/null'`).
        self.check_alias(Chk::Command)?;
        loop {
            match self.peek() {
                Some(Tok::IoNumber(n)) => {
                    let n = *n;
                    self.bump();
                    redirs.push(self.parse_redir(Some(n))?);
                }
                Some(Tok::Op(op)) if op.is_redirect() => {
                    redirs.push(self.parse_redir(None)?);
                }
                _ => return Ok(redirs),
            }
        }
    }

    fn parse_redir(&mut self, fd: Option<u32>) -> Syn<Redir> {
        let Some(op) = self.peek_op() else {
            return Err(self.unexpected(None));
        };
        self.bump();
        if let Op::DLess(id) = op {
            // The delimiter word follows the operator; its body was collected
            // by the lexer at the end of the line.
            let _delim = self.take_word(None)?;
            self.ensure_heredoc(id);
            let body = self.scan.heredocs().get(id).cloned().unwrap_or_default();
            return Ok(Redir {
                fd,
                kind: RedirKind::Here(body),
                word: Word::default(),
            });
        }
        let kind = match op {
            Op::Less => RedirKind::In,
            Op::Great => RedirKind::Out,
            Op::DGreat => RedirKind::Append,
            Op::LessGreat => RedirKind::ReadWrite,
            Op::Clobber => RedirKind::Clobber,
            Op::LessAnd => RedirKind::DupIn,
            Op::GreatAnd => RedirKind::DupOut,
            Op::AmpGreat => RedirKind::OutBoth,
            other => return Err(format!("syntax error near `{}`", other.text()).into()),
        };
        let word = self.take_word(None)?;
        Ok(Redir { fd, kind, word })
    }
}

/// Where an index recorded in the old token stream lands once `toks[a..b]` has
/// become `added` tokens.
fn reindex(e: usize, a: usize, b: usize, added: usize) -> usize {
    if e <= a {
        e
    } else if e >= b {
        (e + added).saturating_sub(b - a)
    } else {
        // Inside the replaced span: what it pointed past is gone.
        a + added
    }
}

/// Recognise `name=value` where `name=` is unquoted literal text.
fn as_assignment(w: &Word) -> Option<Assign> {
    let Some(Seg::Lit(s)) = w.0.first() else {
        return None;
    };
    // Locate '=' with `bytes().position` rather than a str search combinator: this
    // source is embedded into the td-sh recipe as a WriteFile body scanned by the
    // ladder guard that bars GNU findutils from the tool tier, which rejects that
    // tool's bare name as a token (see td-sh.rs recipe). '=' is ASCII, so the byte
    // offset is a valid str boundary for the `get(..eq)`/`get(eq+1..)` splits.
    let eq = s.bytes().position(|b| b == b'=')?;
    let name = s.get(..eq)?;
    if !is_name(name) {
        return None;
    }
    let rest = s.get(eq + 1..)?;
    let mut segs = Vec::new();
    if !rest.is_empty() {
        segs.push(Seg::Lit(rest.to_string()));
    }
    segs.extend(w.0.iter().skip(1).cloned());
    Some(Assign {
        name: name.to_string(),
        value: Word(segs),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one_cmd(src: &str) -> Syn<Cmd> {
        let list = parse(src)?;
        let (and_or, _) = list
            .items
            .into_iter()
            .next()
            .ok_or_else(|| SynErr::from(format!("{src}: parsed to no commands")))?;
        and_or
            .first
            .cmds
            .into_iter()
            .next()
            .map(|s| s.cmd)
            .ok_or_else(|| SynErr::from(format!("{src}: pipeline has no command")))
    }

    fn wrong(src: &str, got: &Cmd) -> SynErr {
        format!("{src}: unexpected command shape {got:?}").into()
    }

    #[test]
    fn simple_command_splits_assignments_from_words() -> Syn<()> {
        let src = "a=1 b=2 echo hi c=3";
        let cmd = one_cmd(src)?;
        let Cmd::Simple {
            assigns,
            words,
            redirs,
        } = &cmd
        else {
            return Err(wrong(src, &cmd));
        };
        assert_eq!(assigns.len(), 2);
        assert_eq!(assigns.first().map(|a| a.name.as_str()), Some("a"));
        assert_eq!(words.len(), 3);
        // `c=3` after the command word is an argument, not an assignment.
        assert_eq!(words.get(2).and_then(|w| w.plain()), Some("c=3"));
        assert!(redirs.is_empty());
        Ok(())
    }

    #[test]
    fn semicolons_and_newlines_both_separate() -> Syn<()> {
        assert_eq!(parse("echo a; echo b")?.items.len(), 2);
        assert_eq!(parse("echo a\necho b\n")?.items.len(), 2);
        assert_eq!(parse("echo a &\n")?.items.len(), 1);
        assert_eq!(parse("echo a &")?.items.first().map(|i| i.1), Some(Sep::Bg));
        Ok(())
    }

    #[test]
    fn and_or_chains_keep_their_connectors() -> Syn<()> {
        let list = parse("true && echo a || echo b")?;
        let (and_or, _) = list
            .items
            .into_iter()
            .next()
            .ok_or_else(|| "no commands".to_string())?;
        assert_eq!(and_or.rest.len(), 2);
        assert_eq!(and_or.rest.first().map(|r| r.0), Some(Conn::And));
        assert_eq!(and_or.rest.get(1).map(|r| r.0), Some(Conn::Or));
        Ok(())
    }

    #[test]
    fn pipeline_collects_every_stage() -> Syn<()> {
        let list = parse("! a | b | c")?;
        let (and_or, _) = list
            .items
            .into_iter()
            .next()
            .ok_or_else(|| "no commands".to_string())?;
        assert!(and_or.first.bang);
        assert_eq!(and_or.first.cmds.len(), 3);
        Ok(())
    }

    #[test]
    fn if_elif_else_parses() -> Syn<()> {
        let src = "if true; then echo a; elif false; then echo b; else echo c; fi";
        let cmd = one_cmd(src)?;
        let Cmd::If {
            arms, otherwise, ..
        } = &cmd
        else {
            return Err(wrong(src, &cmd));
        };
        assert_eq!(arms.len(), 2);
        assert!(otherwise.is_some());
        Ok(())
    }

    #[test]
    fn for_with_and_without_a_word_list() -> Syn<()> {
        let src = "for x in a b c; do echo $x; done";
        let cmd = one_cmd(src)?;
        let Cmd::For { var, words, .. } = &cmd else {
            return Err(wrong(src, &cmd));
        };
        assert_eq!(var, "x");
        assert_eq!(words.as_ref().map(|w| w.len()), Some(3));

        let src = "for x do echo $x; done";
        let cmd = one_cmd(src)?;
        let Cmd::For { words, .. } = &cmd else {
            return Err(wrong(src, &cmd));
        };
        assert!(words.is_none());
        Ok(())
    }

    #[test]
    fn case_collects_alternating_patterns() -> Syn<()> {
        let src = "case $x in a|b) echo ab ;; (c) echo c ;; *) echo other ;; esac";
        let cmd = one_cmd(src)?;
        let Cmd::Case { items, .. } = &cmd else {
            return Err(wrong(src, &cmd));
        };
        assert_eq!(items.len(), 3);
        assert_eq!(items.first().map(|i| i.patterns.len()), Some(2));
        Ok(())
    }

    /// A word only closes a construct if it is RESERVED at all. Nothing else
    /// ties the two lists together, and the guard is `peek_reserved`, which
    /// returns any plain literal -- so a typo here would refuse a command name.
    #[test]
    fn every_word_that_cannot_start_a_command_is_reserved() {
        for w in CANNOT_START_COMMAND {
            assert!(is_reserved(w), "{w} is not a reserved word");
        }
    }

    /// The flag and the message are separate things, and the alias splice is
    /// where they disagree: the same unclosed here-document is more input
    /// where it is typed and a refusal where it is a replacement, since no
    /// line completes an alias. Both messages carry the same phrase, which is
    /// why matching that text got this right only by accident of the prefix.
    #[test]
    fn an_alias_that_opens_a_here_document_is_refused_rather_than_continued() {
        let typed = parse_probe("cat <<EOF\n", &Aliases::new()).unwrap_err();
        assert!(typed.is_incomplete(), "{typed}");
        let mut aliases = Aliases::new();
        aliases.insert("q".to_string(), "cat <<EOF".to_string());
        let spliced = parse_probe("q\n", &aliases).unwrap_err();
        assert!(!spliced.is_incomplete(), "{spliced}");
        // Named, so this is the here-document refusal and not some other one,
        // and carrying the ENDED wording without being one -- a message and a
        // flag saying different things, which is the whole point of the two
        // being separate.
        assert!(spliced.msg.starts_with("alias `q':"), "{spliced}");
        assert!(spliced.msg.contains(r#"(expecting "EOF")"#), "{spliced}");
        // A delimiter is an arbitrary WORD: one carrying the quote it is put
        // inside has to come back escaped rather than closing it early.
        let mut odd = Aliases::new();
        odd.insert("q".to_string(), "cat <<'E\"F'".to_string());
        let e = parse_probe("q\n", &odd).unwrap_err();
        assert!(e.msg.contains(r#"(expecting "E\"F")"#), "{e}");
        assert!(spliced.msg.contains("unexpected end of file"), "{spliced}");
    }

    /// An assignment-shaped name is the shape where "ended" and "cannot be
    /// repaired" come apart: at EOF the flag is set, because the token named
    /// IS end of file and ash prompts there too. What the reader sees is the
    /// probe of a line it has already ended, which stops on the NEWLINE and is
    /// hard -- so no prompt waits for input that cannot arrive.
    #[test]
    fn a_name_that_no_line_can_repair_never_holds_the_prompt() {
        let a = Aliases::new();
        assert!(!parse_probe("function a=1\n", &a).unwrap_err().is_incomplete());
        assert!(!parse_probe("function ;\n", &a).unwrap_err().is_incomplete());
        // A construct that another line really does finish still asks, so this
        // is about THIS shape rather than about newline-ended probes at large.
        assert!(parse_probe("if :\n", &a).unwrap_err().is_incomplete());
    }

    /// The flag is the interactive reader's request for another line, so a
    /// token that is merely WRONG must not carry it -- `parse_probe` would go
    /// to PS2 and never come back.
    #[test]
    fn only_a_name_that_never_arrived_is_incomplete() {
        let ended = parse("function").unwrap_err();
        assert!(ended.is_incomplete(), "{ended}");
        for src in ["function ;", "function |", "function (", "function <f", "function\n"] {
            let e = parse(src).unwrap_err();
            assert!(!e.is_incomplete(), "{src:?}: {e}");
        }
    }

    #[test]
    fn function_definition_requires_a_compound_body() -> Syn<()> {
        let src = "f() { echo hi; }";
        let cmd = one_cmd(src)?;
        let Cmd::FuncDef { name, .. } = &cmd else {
            return Err(wrong(src, &cmd));
        };
        assert_eq!(name.as_deref(), Some("f"));
        assert!(parse("f() echo hi").is_err());
        Ok(())
    }

    #[test]
    fn redirections_attach_with_and_without_an_fd() -> Syn<()> {
        let src = "echo hi >out 2>&1 <in";
        let cmd = one_cmd(src)?;
        let Cmd::Simple { redirs, .. } = &cmd else {
            return Err(wrong(src, &cmd));
        };
        assert_eq!(redirs.len(), 3);
        assert_eq!(redirs.first().and_then(|r| r.fd), None);
        assert_eq!(redirs.get(1).and_then(|r| r.fd), Some(2));
        Ok(())
    }

    #[test]
    fn compound_commands_take_trailing_redirections() -> Syn<()> {
        let src = "while read a; do echo $a; done <in";
        let cmd = one_cmd(src)?;
        let Cmd::Loop { redirs, .. } = &cmd else {
            return Err(wrong(src, &cmd));
        };
        assert_eq!(redirs.len(), 1);
        Ok(())
    }

    #[test]
    fn incomplete_input_is_reported_as_such() -> Syn<()> {
        for src in ["if true; then", "while true; do", "case x in", "f() {"] {
            match parse(src) {
                Err(e) => assert!(e.is_incomplete(), "{src}: {e}"),
                Ok(_) => return Err(format!("{src}: expected an incomplete-input error").into()),
            }
        }
        Ok(())
    }

    /// Two constructs answer the two parses of one buffer differently, and by
    /// one rule: text running out means "ask" to the reader's PROBE, which has a
    /// source it can still ask, and "that was all" to the parse that finally
    /// RUNS it. So a trailing `\<newline>` is spent and `echo x \` runs `echo
    /// x`, and an unterminated here-document ends its body and the command runs
    /// -- both what ash does at `-c`, at end of a script, and at end of an
    /// interactive session alike.
    ///
    /// Without it the probe would report the line COMPLETE, and an operator
    /// typing a continuation would watch the shell run half of what they meant.
    /// This is the level it can be tested at: `repl` needs a terminal, since a
    /// shell whose stdin is not one reads it as a script however `-i` is set.
    #[test]
    fn the_probe_asks_for_more_where_the_real_parse_takes_what_it_has() -> Syn<()> {
        let aliases = Aliases::new();
        for src in [
            "echo x \\\n",
            "echo ab\\\n",
            "true &\\\n",
            "\\\n",
            "cat <<E\n",
            "cat <<E\nbody\n",
            // A delimiter line no newline ended is the one a SNAPSHOT must not
            // take: another keystroke could make it `EX`, where the finished
            // text really does close the body there.
            "cat <<E\nE",
            "cat <<A\none\nA\ncat <<B\ntwo\n",
        ] {
            match parse_probe(src, &aliases) {
                Err(e) => assert!(e.is_incomplete(), "{src:?}: {e}"),
                Ok(_) => return Err(format!("{src:?}: the probe must ask for more").into()),
            }
            parse_aliased_at(src, &aliases, 1)
                .map_err(|e| format!("{src:?}: the real parse must take it: {e}"))?;
        }
        // Everything else answers the same to both, because more input really
        // could finish it and the text really is unfinished.
        // A `<<` whose DELIMITER never arrived is still unfinished to both: the
        // body was never opened, and ash refuses `cat <<` at end of input too.
        for src in ["if true; then", "echo 'abc", "echo x |", "cat <<"] {
            for got in [parse_probe(src, &aliases), parse_aliased_at(src, &aliases, 1)] {
                match got {
                    Err(e) => assert!(e.is_incomplete(), "{src:?}: {e}"),
                    Ok(_) => return Err(format!("{src:?}: expected incomplete input").into()),
                }
            }
        }
        Ok(())
    }

    #[test]
    fn reserved_words_are_positional() -> Syn<()> {
        // `done` here is an argument, not the end of a loop.
        let src = "echo done";
        let cmd = one_cmd(src)?;
        let Cmd::Simple { words, .. } = &cmd else {
            return Err(wrong(src, &cmd));
        };
        assert_eq!(words.len(), 2);
        Ok(())
    }

    #[test]
    fn deeply_nested_input_errors_instead_of_overflowing() {
        // Past MAX_PARSE_DEPTH the parser returns a syntax error rather than
        // recursing into a stack overflow. It must NOT look like incomplete input
        // (which would make the REPL wait for more).
        let src = "(".repeat(2000) + "true" + &")".repeat(2000);
        match parse(&src) {
            Err(e) => assert!(!e.is_incomplete(), "should be a hard error: {e}"),
            Ok(_) => panic!("expected a nesting-depth error"),
        }
    }
}

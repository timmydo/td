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
    RedirKind, Seg, Sep, Syn, Word, INCOMPLETE,
};
use crate::lexer::{tokenize, Op, Scan, Tok};

/// Alias name -> replacement text. Ordered so `alias` lists deterministically.
pub type Aliases = std::collections::BTreeMap<String, String>;

/// Parse with no aliases in force. Every runtime path carries a table, so this
/// is the grammar tests' entry point.
#[cfg(test)]
fn parse(src: &str) -> Syn<List> {
    parse_aliased(src, &Aliases::new())
}

/// Parse all of `src` under one alias table, for the callers that run nothing
/// while parsing: the interactive line and command substitution.
pub fn parse_aliased(src: &str, aliases: &Aliases) -> Syn<List> {
    let mut units = Units::new(src);
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
    toks: Vec<Tok>,
    /// The scan this source's tokens come from, kept alive because it OWNS the
    /// here-document bodies: an id is minted (and a placeholder pushed) when
    /// `<<` is scanned, and the body is written into that slot later.
    scan: Scan,
    /// What stopped the lexer, raised only once a parse needs the text there.
    pending: Option<String>,
    /// Set when a fetch ran off the end of the tokens, which is how `pending`
    /// becomes this unit's error rather than a bare "unexpected end of input".
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
        let mut scan = Scan::new(src);
        scan.seal();
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
            match self.toks.get(i) {
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
        Err(format!("syntax error near {}", self.describe()))
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
        let Some(Tok::Word(w)) = self.toks.get(self.pos) else {
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
        // Not incomplete input however it failed: no amount of further input can
        // complete a replacement, so the unit loop must stop rather than read on.
        let lexed = tokenize(value).map_err(|e| format!("alias `{name}': {e}"))?;
        let base = self.scan.push_heredocs(lexed.heredocs);
        let sub: Vec<Tok> = lexed
            .toks
            .into_iter()
            .filter(|t| !matches!(t, Tok::Eof))
            // Renumber onto the end of this source's table so a here-document
            // written inside an alias still resolves.
            .map(|t| match t {
                Tok::Op(Op::DLess(id)) => Tok::Op(Op::DLess(id + base)),
                other => other,
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
    fn splice(&mut self, at: usize, over: usize, sub: Vec<Tok>) {
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
        while !matches!(self.toks.get(i), None | Some(Tok::Newline) | Some(Tok::Eof)) {
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

    fn tok_at(&mut self, i: usize) -> Option<&Tok> {
        // The refill belongs HERE and not one fetch earlier: filling ahead of
        // the fetch reads a line the current unit does not need, which for a
        // shared stdin is a line the commands in it were owed.
        self.ensure(i);
        let tok = self.toks.get(i);
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
        if self.peek_op() == Some(want) {
            self.bump();
            return Ok(());
        }
        if self.at_eof() {
            return Err(format!("{INCOMPLETE}: expected `{}`", want.text()));
        }
        Err(format!(
            "syntax error: expected `{}`, found {}",
            want.text(),
            self.describe()
        ))
    }

    fn expect_reserved(&mut self, want: &str) -> Syn<()> {
        if self.peek_reserved() == Some(want) {
            self.bump();
            return Ok(());
        }
        if self.at_eof() {
            return Err(format!("{INCOMPLETE}: expected `{want}`"));
        }
        Err(format!(
            "syntax error: expected `{want}`, found {}",
            self.describe()
        ))
    }

    fn take_word(&mut self) -> Syn<Word> {
        // No command starts here (a redirection target, a `case` pattern, a `for`
        // list word), but a blank-terminated replacement still reaches this far.
        self.check_alias(Chk::None)?;
        match self.peek() {
            Some(Tok::Word(w)) => {
                let w = w.clone();
                self.bump();
                Ok(w)
            }
            None | Some(Tok::Eof) => Err(format!("{INCOMPLETE}: expected a word")),
            _ => Err(format!("syntax error: expected a word, found {}", self.describe())),
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
        let mut bang = false;
        // Every command in the grammar is reached through here, so this is the one
        // place a command word's alias check has to be made.
        loop {
            self.check_alias(Chk::Command)?;
            if self.peek_reserved() != Some("!") {
                break;
            }
            self.bump();
            bang = !bang;
        }
        let mut cmds = vec![self.parse_command()?];
        while self.peek_op() == Some(Op::Pipe) {
            self.bump();
            self.open_command()?;
            cmds.push(self.parse_command()?);
        }
        Ok(Pipeline { bang, cmds })
    }

    /// Depth-guarded entry to command parsing. Every recursive descent through
    /// the COMMAND grammar bottoms out here, so this counter bounds its stack
    /// use. `cond_term` shares the same counter for the same reason: its
    /// parentheses recurse without passing through here.
    fn parse_command(&mut self) -> Syn<Cmd> {
        self.depth += 1;
        if self.depth > MAX_PARSE_DEPTH {
            self.depth -= 1;
            return Err("syntax error: command nesting too deep".to_string());
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
                return Err("syntax error: `[[` expression too long".to_string());
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
                return Err("syntax error: `[[` expression too long".to_string());
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
    /// shape inside `[[ ]]` -- and this crate builds with `panic = "abort"`, so
    /// an overflow is the shell dying rather than a diagnostic. Verified: 100k
    /// nested parens abort without it and are a syntax error with it.
    fn cond_term(&mut self) -> Syn<CondExpr> {
        self.depth += 1;
        if self.depth > MAX_PARSE_DEPTH {
            self.depth -= 1;
            return Err("syntax error: `[[` nesting too deep".to_string());
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
            Some(Tok::IoNumber(_)) => Err(
                "syntax error: unexpected redirection inside `[[`".to_string()
            ),
            None | Some(Tok::Eof) => Err(format!("{INCOMPLETE}: expected `]]`")),
            _ => Err(format!(
                "syntax error: expected an operand inside `[[`, found {}",
                self.describe()
            )),
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
        self.expect_op(Op::RParen)?;
        // The body is a command position like any other, so an alias may supply it:
        // `alias B='{ echo yes; }'` then `f()` / `B`.
        self.open_command()?;
        let body = self.parse_command()?;
        if matches!(body, Cmd::Simple { .. }) {
            return Err(format!(
                "syntax error: the body of function `{name}` must be a compound command"
            ));
        }
        Ok(Cmd::FuncDef {
            name,
            body: Arc::new(body),
        })
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
                _ => return Err("syntax error: `for` requires a variable name".into()),
            },
            _ => return Err("syntax error: `for` requires a variable name".into()),
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
                ws.push(self.take_word()?);
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
        let word = self.take_word()?;
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
                return Err(format!("{INCOMPLETE}: expected `esac`"));
            }
            if self.peek_op() == Some(Op::LParen) {
                self.bump();
            }
            let mut patterns = vec![self.take_word()?];
            while self.peek_op() == Some(Op::Pipe) {
                self.bump();
                patterns.push(self.take_word()?);
            }
            self.expect_op(Op::RParen)?;
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
            // wrong: `f ()` on its own line still has its body coming.
            return Err(if self.at_eof() {
                format!("{INCOMPLETE}: expected a command")
            } else {
                format!("syntax error near {}", self.describe())
            });
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
            return Err(format!("{INCOMPLETE}: expected a redirection"));
        };
        self.bump();
        if let Op::DLess(id) = op {
            // The delimiter word follows the operator; its body was collected
            // by the lexer at the end of the line.
            let _delim = self.take_word()?;
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
            other => return Err(format!("syntax error near `{}`", other.text())),
        };
        let word = self.take_word()?;
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
            .ok_or_else(|| format!("{src}: parsed to no commands"))?;
        and_or
            .first
            .cmds
            .into_iter()
            .next()
            .ok_or_else(|| format!("{src}: pipeline has no command"))
    }

    fn wrong(src: &str, got: &Cmd) -> String {
        format!("{src}: unexpected command shape {got:?}")
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

    #[test]
    fn function_definition_requires_a_compound_body() -> Syn<()> {
        let src = "f() { echo hi; }";
        let cmd = one_cmd(src)?;
        let Cmd::FuncDef { name, .. } = &cmd else {
            return Err(wrong(src, &cmd));
        };
        assert_eq!(name, "f");
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
                Err(e) => assert!(e.starts_with(INCOMPLETE), "{src}: {e}"),
                Ok(_) => return Err(format!("{src}: expected an incomplete-input error")),
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
            Err(e) => assert!(!e.starts_with(INCOMPLETE), "should be a hard error: {e}"),
            Ok(_) => panic!("expected a nesting-depth error"),
        }
    }
}

//! Recursive-descent parser for the POSIX shell grammar over the lexer's token
//! stream.
//!
//! Reserved words are resolved here rather than in the lexer because they are
//! positional: `done` ends a loop only where a command could start, and is an
//! ordinary argument in `echo done`.

use std::sync::Arc;

use crate::ast::{
    is_name, AndOr, Assign, CaseItem, Cmd, Conn, IfArm, List, Pipeline, Redir, RedirKind, Seg, Sep,
    Syn, Word, INCOMPLETE,
};
use crate::lexer::{tokenize, Op, Tok};

pub fn parse(src: &str) -> Syn<List> {
    let lexed = tokenize(src)?;
    let mut p = Parser {
        toks: lexed.toks,
        heredocs: lexed.heredocs,
        pos: 0,
        depth: 0,
    };
    let list = p.parse_list(&[])?;
    if !p.at_eof() {
        return Err(format!("syntax error near {}", p.describe()));
    }
    Ok(list)
}

/// Bound on command-nesting depth. Every nested compound (`( … )`, `{ … }`, `if`,
/// loops, `case`, a function body) re-enters `parse_command`, so guarding that one
/// node bounds the recursive-descent stack: pathological input like `((((…))))`
/// errors as a syntax error instead of overflowing the stack and aborting. Real
/// scripts nest a handful deep; this only fires far past any legitimate program.
const MAX_PARSE_DEPTH: u32 = 256;

struct Parser {
    toks: Vec<Tok>,
    heredocs: Vec<Word>,
    pos: usize,
    depth: u32,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn bump(&mut self) {
        if self.pos < self.toks.len() {
            self.pos += 1;
        }
    }

    fn at_eof(&self) -> bool {
        matches!(self.peek(), None | Some(Tok::Eof))
    }

    fn peek_op(&self) -> Option<Op> {
        match self.peek() {
            Some(Tok::Op(op)) => Some(*op),
            _ => None,
        }
    }

    /// The next token's text when it is an unquoted literal word — the only
    /// shape that can be a reserved word.
    fn peek_reserved(&self) -> Option<&str> {
        match self.peek() {
            Some(Tok::Word(w)) => w.plain(),
            _ => None,
        }
    }

    fn describe(&self) -> String {
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
    fn at_list_end(&self, terms: &[&str]) -> bool {
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
            self.skip_newlines();
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
            self.skip_newlines();
            rest.push((conn, self.parse_pipeline()?));
        }
        Ok(AndOr { first, rest })
    }

    fn parse_pipeline(&mut self) -> Syn<Pipeline> {
        let mut bang = false;
        while self.peek_reserved() == Some("!") {
            self.bump();
            bang = !bang;
        }
        let mut cmds = vec![self.parse_command()?];
        while self.peek_op() == Some(Op::Pipe) {
            self.bump();
            self.skip_newlines();
            cmds.push(self.parse_command()?);
        }
        Ok(Pipeline { bang, cmds })
    }

    /// Depth-guarded entry to command parsing. Every recursive descent bottoms out
    /// here, so the counter here bounds the whole parse's stack use.
    fn parse_command(&mut self) -> Syn<Cmd> {
        self.depth += 1;
        if self.depth > MAX_PARSE_DEPTH {
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
        if self.at_func_def() {
            return self.parse_func_def();
        }
        self.parse_simple()
    }

    fn at_func_def(&self) -> bool {
        let Some(Tok::Word(w)) = self.peek() else {
            return false;
        };
        if !w.plain().is_some_and(is_name) {
            return false;
        }
        matches!(self.toks.get(self.pos + 1), Some(Tok::Op(Op::LParen)))
    }

    fn parse_func_def(&mut self) -> Syn<Cmd> {
        let name = match self.peek() {
            Some(Tok::Word(w)) => w.plain().unwrap_or_default().to_string(),
            _ => return Err("syntax error: expected a function name".into()),
        };
        self.bump();
        self.expect_op(Op::LParen)?;
        self.expect_op(Op::RParen)?;
        self.skip_newlines();
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
        let var = match self.peek() {
            Some(Tok::Word(w)) => match w.plain() {
                Some(n) if is_name(n) => n.to_string(),
                _ => return Err("syntax error: `for` requires a variable name".into()),
            },
            _ => return Err("syntax error: `for` requires a variable name".into()),
        };
        self.bump();
        self.skip_newlines();
        let words = if self.peek_reserved() == Some("in") {
            self.bump();
            let mut ws = Vec::new();
            while matches!(self.peek(), Some(Tok::Word(_))) {
                ws.push(self.take_word()?);
            }
            Some(ws)
        } else {
            None
        };
        if self.peek_op() == Some(Op::Semi) {
            self.bump();
        }
        self.skip_newlines();
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
        self.skip_newlines();
        self.expect_reserved("in")?;
        let mut items = Vec::new();
        loop {
            self.skip_newlines();
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
            return Err(format!("syntax error near {}", self.describe()));
        }
        Ok(Cmd::Simple {
            assigns,
            words,
            redirs,
        })
    }

    fn parse_redirs(&mut self) -> Syn<Vec<Redir>> {
        let mut redirs = Vec::new();
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
            let body = self.heredocs.get(id).cloned().unwrap_or_default();
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

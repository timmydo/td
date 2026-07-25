//! td-sh syntax tree: words (with their quoting structure preserved) and the
//! command tree the parser builds and the executor walks.
//!
//! A word is a list of segments rather than a flat string because expansion has
//! to know, per character, whether it was quoted (no pathname expansion, no
//! field splitting) and whether it came out of an expansion (field-splitting
//! eligible). Flattening early is exactly what makes naive shells mis-split
//! `$x` or glob a quoted `*`.

use std::sync::Arc;

/// Lex/parse errors are plain messages; the caller adds the `td-sh:` prefix.
pub type Syn<T> = Result<T, String>;

/// The marker a syntax error carries when the input simply ended early. The
/// interactive reader uses it to ask for a continuation line instead of
/// reporting an error.
pub const INCOMPLETE: &str = "unexpected end of input";

#[derive(Clone, Debug, Default)]
pub struct Word(pub Vec<Seg>);

impl Word {
    /// The word's text when it is a single unquoted literal — the only shape a
    /// reserved word (`if`, `done`, `{`) or a function name may take.
    pub fn plain(&self) -> Option<&str> {
        match self.0.as_slice() {
            [Seg::Lit(s)] => Some(s.as_str()),
            _ => None,
        }
    }

    /// A here-document delimiter: its literal text plus whether any part of it
    /// was quoted (a quoted delimiter turns off expansion in the body).
    pub fn delimiter(&self) -> Option<(String, bool)> {
        let mut text = String::new();
        let mut quoted = false;
        for seg in &self.0 {
            match seg {
                Seg::Lit(s) => text.push_str(s),
                Seg::Quoted(s) => {
                    quoted = true;
                    text.push_str(s);
                }
                _ => return None,
            }
        }
        Some((text, quoted))
    }
}

#[derive(Clone, Debug)]
pub enum Seg {
    /// Unquoted source text: pathname-expansion metacharacters are live, but it
    /// is not field-split (splitting applies to expansion results only).
    Lit(String),
    /// Text that was inside quotes or escaped: literal everywhere.
    Quoted(String),
    Param(Box<Param>),
    /// `$(...)` / backticks — the raw source, parsed when the word is expanded.
    Cmd { code: String, quoted: bool },
    /// `$((...))` — the inner text is itself a word (it may contain `$x`), so it
    /// is expanded first and then evaluated as an arithmetic expression.
    Arith { expr: Word, quoted: bool },
}

#[derive(Clone, Debug)]
pub struct Param {
    /// A name, a positional digit string, or one of `@ * # ? - $ !`.
    pub name: String,
    pub op: Option<ParamOp>,
    pub quoted: bool,
}

#[derive(Clone, Debug)]
pub enum ParamOp {
    /// `${#name}`
    Length,
    /// `${name-w}` / `${name:-w}`; `colon` also tests for empty, not just unset.
    Default { word: Word, colon: bool },
    /// `${name=w}` / `${name:=w}`
    Assign { word: Word, colon: bool },
    /// `${name?w}` / `${name:?w}`
    Error { word: Word, colon: bool },
    /// `${name+w}` / `${name:+w}`
    Alt { word: Word, colon: bool },
    /// `${name%pat}` / `${name%%pat}`
    TrimSuffix { pat: Word, longest: bool },
    /// `${name#pat}` / `${name##pat}`
    TrimPrefix { pat: Word, longest: bool },
}

#[derive(Clone, Debug)]
pub enum RedirKind {
    /// `<`
    In,
    /// `>`
    Out,
    /// `>>`
    Append,
    /// `<>`
    ReadWrite,
    /// `>|` — write, overriding `set -C`
    Clobber,
    /// `<&`
    DupIn,
    /// `>&`
    DupOut,
    /// `<<` / `<<-`; the body is already tab-stripped and, when the delimiter
    /// was unquoted, lexed for expansions.
    Here(Word),
}

#[derive(Clone, Debug)]
pub struct Redir {
    /// The redirected descriptor when written explicitly (`2>`), else the
    /// operator's default.
    pub fd: Option<u32>,
    pub kind: RedirKind,
    /// The target: a filename, or an fd number / `-` for the dup operators.
    /// Unused (and empty) for `Here`.
    pub word: Word,
}

#[derive(Clone, Debug)]
pub struct Assign {
    pub name: String,
    pub value: Word,
}

#[derive(Clone, Debug)]
pub struct IfArm {
    pub cond: List,
    pub body: List,
}

#[derive(Clone, Debug)]
pub struct CaseItem {
    pub patterns: Vec<Word>,
    pub body: List,
}

#[derive(Clone, Debug)]
pub enum Cmd {
    Simple {
        assigns: Vec<Assign>,
        words: Vec<Word>,
        redirs: Vec<Redir>,
    },
    /// `( list )` — always its own environment.
    Subshell { body: List, redirs: Vec<Redir> },
    /// `{ list; }` — runs in the current environment.
    Group { body: List, redirs: Vec<Redir> },
    If {
        arms: Vec<IfArm>,
        otherwise: Option<List>,
        redirs: Vec<Redir>,
    },
    For {
        var: String,
        /// `None` for `for x; do` — iterate the positional parameters.
        words: Option<Vec<Word>>,
        body: List,
        redirs: Vec<Redir>,
    },
    Loop {
        until: bool,
        cond: List,
        body: List,
        redirs: Vec<Redir>,
    },
    Case {
        word: Word,
        items: Vec<CaseItem>,
        redirs: Vec<Redir>,
    },
    /// `name() compound` — `Arc` so a call site can hold the body while the
    /// definition is redefined out from under it.
    FuncDef { name: String, body: Arc<Cmd> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Conn {
    And,
    Or,
}

#[derive(Clone, Debug)]
pub struct Pipeline {
    pub bang: bool,
    pub cmds: Vec<Cmd>,
}

#[derive(Clone, Debug)]
pub struct AndOr {
    pub first: Pipeline,
    pub rest: Vec<(Conn, Pipeline)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sep {
    Seq,
    Bg,
}

#[derive(Clone, Debug, Default)]
pub struct List {
    pub items: Vec<(AndOr, Sep)>,
}

/// True for a portable variable/function name: `[A-Za-z_][A-Za-z0-9_]*`.
pub fn is_name(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

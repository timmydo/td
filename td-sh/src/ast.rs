//! td-sh syntax tree: words (with their quoting structure preserved) and the
//! command tree the parser builds and the executor walks.
//!
//! A word is a list of segments rather than a flat string because expansion has
//! to know, per character, whether it was quoted (no pathname expansion, no
//! field splitting) and whether it came out of an expansion (field-splitting
//! eligible). Flattening early is exactly what makes naive shells mis-split
//! `$x` or glob a quoted `*`.

use std::sync::Arc;

/// Lex/parse errors are plain messages; the caller adds the `$0: ` prefix.
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
    /// `line` is where that source starts, which `$LINENO` inside it counts on
    /// from; a backtick body is renumbered from 1, as dash renumbers it.
    Cmd {
        code: String,
        quoted: bool,
        line: u32,
    },
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
    /// `${name/pat/repl}` / `${name//pat/repl}`. ash has no bash `#`/`%`
    /// anchors, so a leading `#` or `%` is an ordinary pattern character.
    Replace { pat: Word, repl: Word, all: bool },
    /// `${name:off}` / `${name:off:len}`. Both operands are ARITHMETIC, not
    /// words, so `${v:n+1}` reads `n` as a variable the way `$((n+1))` does.
    Substring { offset: Word, length: Option<Word> },
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
    /// `&>` — the target is a FILE, and on fd 1 stderr follows stdout to it.
    /// NOT a spelling of `>&`: see `plan_redirs`.
    OutBoth,
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
    /// `[[ expr ]]` — the conditional command. Not a builtin: its operands are
    /// neither field-split nor pathname-expanded, and `<`/`>`/`&&` inside it are
    /// comparisons rather than redirections and list operators, so it has to be
    /// syntax. busybox ash provides it under `ASH_BASH_COMPAT`, which td's
    /// defconfig enables, so it is in this shell's model.
    Cond { expr: CondExpr, redirs: Vec<Redir> },
    /// `name() compound` — `Arc` so a call site can hold the body while the
    /// definition is redefined out from under it. `line` is the definition's
    /// own, kept because it is not the body's: dash reports `$LINENO` inside a
    /// function RELATIVE to it (`funcline`, eval.c:996).
    FuncDef {
        name: String,
        /// A `Stage` and not a bare `Cmd` because the body is a command node
        /// too, and a compound's OWN line is what its header expands under:
        /// `f() for x in "$LINENO"; do …` is the definition's line in dash,
        /// not the caller's.
        body: Arc<Stage>,
        line: u32,
    },
}

/// The expression inside `[[ ]]`. A tree rather than an argument vector — which
/// is what `test` gets — because the operators bind: `!` tighter than `&&`,
/// `&&` tighter than `||`, and parentheses group. `test` recovers that from a
/// flat argv at run time and has to guess at ambiguities; here the parser knows.
#[derive(Clone, Debug)]
pub enum CondExpr {
    /// A bare word: true when it expands to a non-empty string.
    Word(Word),
    /// `-X word`, the unary operators `test` already serves, plus `-v`.
    Unary { op: String, arg: Word },
    /// `lhs OP rhs`. The RHS of `==`/`!=`/`=` is a PATTERN, which is why it
    /// stays a `Word` here: its quoting decides, per character, whether a `*`
    /// matches anything or itself.
    Binary { op: CondOp, lhs: Word, rhs: Word },
    Not(Box<CondExpr>),
    And(Box<CondExpr>, Box<CondExpr>),
    Or(Box<CondExpr>, Box<CondExpr>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArithCmp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CondOp {
    /// `==` and `=`: the right side is a pattern.
    Match,
    /// `!=`: the right side is a pattern.
    NoMatch,
    /// `<` / `>`: string order. NOT numeric -- `[[ 10 < 9 ]]` is true, and the
    /// numeric spellings are the `-lt` family.
    Before,
    After,
    /// `-eq`, `-ne`, `-lt`, `-le`, `-gt`, `-ge`. Both operands are ARITHMETIC
    /// EXPRESSIONS, not integers -- `[[ 1+1 -eq 2 ]]` and, with `x=5`,
    /// `[[ x -eq 5 ]]` are both true, where `test x -eq 5` is an error. That is
    /// why these do not defer to `test` the way the file operators below do.
    Arith(ArithCmp),
    /// `=~`: the right side is a POSIX extended regular expression, SEARCHED
    /// for rather than matched whole, and quoting makes a part of it literal
    /// exactly as it does for `Match` above -- which is why this too keeps a
    /// `Word` rather than a string.
    Regex,
    /// `-ef`, `-nt`, `-ot`: file comparisons, which ARE `test`'s, spelled here
    /// rather than duplicated.
    File(&'static str),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Conn {
    And,
    Or,
}

/// One command of a pipeline, with the input line it starts on. Per COMMAND
/// and not per pipeline because that is where dash keeps it (eval.c:751), and
/// the difference shows the moment a pipeline spans lines: in `true |\n  echo
/// $LINENO` the second stage reports its own line, not the first's.
#[derive(Clone, Debug)]
pub struct Stage {
    pub line: u32,
    pub cmd: Cmd,
}

#[derive(Clone, Debug)]
pub struct Pipeline {
    pub bang: bool,
    pub cmds: Vec<Stage>,
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

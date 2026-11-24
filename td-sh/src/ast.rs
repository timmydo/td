//! td-sh syntax tree: words (with their quoting structure preserved) and the
//! command tree the parser builds and the executor walks.
//!
//! A word is a list of segments rather than a flat string because expansion has
//! to know, per character, whether it was quoted (no pathname expansion, no
//! field splitting) and whether it came out of an expansion (field-splitting
//! eligible). Flattening early is exactly what makes naive shells mis-split
//! `$x` or glob a quoted `*`.

use std::sync::Arc;

/// A lex/parse error: the message the caller prefixes with `$0: `, and whether
/// the input simply ENDED early -- which the interactive reader answers with a
/// continuation line rather than a diagnostic.
#[derive(Clone, Debug)]
pub struct SynErr {
    pub msg: String,
    /// Private, so the constructors below are the only way to set it.
    incomplete: bool,
    /// A reservation failed while reading. Carried as a flag rather than as
    /// message text so that reporting it allocates nothing; `text` answers
    /// with the static spelling.
    oom: bool,
}

impl SynErr {
    /// The input ran out where more of it was owed. The WHOLE message, since
    /// ash spells this case several ways with no common opening, and a
    /// `SynErr` cannot be passed in: that would keep the flag through a wrap.
    pub fn incomplete(msg: impl Into<String>) -> Self {
        Self {
            msg: msg.into(),
            incomplete: true,
            oom: false,
        }
    }

    /// Storage for what was being read could not be reserved. `String::new`
    /// does not allocate, so building this one cannot fail the same way.
    pub fn out_of_memory() -> Self {
        Self {
            msg: String::new(),
            incomplete: false,
            oom: true,
        }
    }

    pub fn is_incomplete(&self) -> bool {
        self.incomplete
    }

    /// The diagnostic to print: the message, or the static out-of-memory text.
    pub fn text(&self) -> &str {
        if self.oom {
            "out of memory"
        } else {
            &self.msg
        }
    }
}

/// A message alone is a hard error. Wrapping one -- an alias splicing the
/// failure of its own replacement into a diagnostic -- therefore clears the
/// flag, which is what that caller wants: no further input completes an alias.
impl From<String> for SynErr {
    fn from(msg: String) -> Self {
        Self {
            msg,
            incomplete: false,
            oom: false,
        }
    }
}

impl From<&str> for SynErr {
    fn from(msg: &str) -> Self {
        Self {
            msg: msg.to_string(),
            incomplete: false,
            oom: false,
        }
    }
}

impl std::fmt::Display for SynErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.text())
    }
}

pub type Syn<T> = Result<T, SynErr>;

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
    /// `backtick` is which spelling wrote it, which the parser needs because
    /// the two disagree about a word that can only close a construct; see
    /// `parser::parse_subst_body`.
    Cmd {
        code: String,
        quoted: bool,
        line: u32,
        backtick: bool,
    },
    /// `$((...))` — the inner text is itself a word (it may contain `$x`), so it
    /// is expanded first and then evaluated as an arithmetic expression.
    Arith {
        expr: Word,
        quoted: bool,
    },
    /// `<(...)` / `>(...)` — the raw source. Expanding the word runs it
    /// CONCURRENTLY behind a pipe and leaves the path of this shell's end of
    /// that pipe in its place (`process::dev_fd_path`). `write` is the `>(`
    /// direction, whose command READS what the outer one writes.
    ///
    /// No `quoted` field, unlike `Cmd`: ash reads `<(` in its word scanner and
    /// only outside quotes and outside `${...}` (ash.c:12676), so there is no
    /// quoted spelling of one -- `"<(true)"` and `${v:-<(true)}` are both the
    /// literal text, measured.
    ProcSub {
        code: String,
        write: bool,
        line: u32,
    },
    /// A `${...}` whose body this shell does not serve, as its source text: ash
    /// raises `bad substitution` from its expander, so this fails there too.
    /// Not a `Param` -- `expand_param` would look the name up first.
    BadSub(String),
    /// bash's compound assignment, `name=(w1 w2 …)`: the elements, each still
    /// to be expanded. The lexer makes one only directly after a word that is
    /// exactly `name=` or `name+=`, in a position where bash would assign (see
    /// `lexer::AssignPos`), so it is always a whole assignment value. ash has
    /// no arrays; td-sh carries bash's indexed ones because scripts written
    /// for bash -- an agent harness's shell snapshot among them -- use them.
    Array(Vec<Elem>),
}

/// One element of a compound assignment.
#[derive(Clone, Debug)]
pub enum Elem {
    /// A word, expanded as a command's argument is: split and globbed, each
    /// field its own element at the index after the one before.
    Plain(Word),
    /// `[sub]=value`: the value, expanded as an assignment's is, at the index
    /// the arithmetic `sub` names.
    Keyed { index: Word, value: Word },
}

#[derive(Clone, Debug)]
pub struct Param {
    /// A name, a positional digit string, or one of `@ * # ? - $ !`.
    pub name: String,
    pub op: Option<ParamOp>,
    pub quoted: bool,
    /// `${name[...]}`: which element(s) of an indexed array. `None` is the
    /// plain parameter, which for an array is its element 0, as in bash.
    pub index: Option<Subscript>,
}

/// The subscript of a `${name[...]}`.
#[derive(Clone, Debug)]
pub enum Subscript {
    /// `[@]` or `[*]`: every element, in index order. `star` is `[*]`, which
    /// joins quoted where `[@]` keeps a field per element, as `$*` and `$@` do.
    All { star: bool },
    /// `${!name[@]}` / `${!name[*]}`: the indices that are set, in order.
    Keys { star: bool },
    /// `[expr]`: one element. The expression is arithmetic, like a slice
    /// operand, and a negative value counts back from one past the highest
    /// index that is set.
    At(Word),
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
    /// `name[expr]=value`: the element's subscript, an arithmetic word.
    pub index: Option<Word>,
    /// `name+=value`: append to the value (or, for `name+=(…)`, after the
    /// highest element) rather than replace it.
    pub append: bool,
    /// The value word. A compound assignment's is exactly one `Seg::Array`.
    pub value: Word,
}

impl Assign {
    /// The elements of a compound assignment, `name=(…)`.
    pub fn elements(&self) -> Option<&[Elem]> {
        match self.value.0.as_slice() {
            [Seg::Array(elems)] => Some(elems),
            _ => None,
        }
    }
}

/// Where the parts of an assignment word lie, found without allocating so the
/// lexer can ask it of every word it scans. The NAME and the operator must be
/// unquoted literal text, as ash's `isassignment` requires of `name=`; a
/// subscript may hold anything, including quotes and expansions, up to the
/// `]` that closes it at bracket depth zero in literal text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AssignShape<'a> {
    pub name: &'a str,
    /// The subscript's closing `]`: its segment, and its byte offset in that
    /// segment's literal text. The subscript starts just past the `[` that
    /// follows the name in segment 0.
    pub index_end: Option<(usize, usize)>,
    pub append: bool,
    /// Where the value starts: a segment, and a byte offset into it when that
    /// segment is the literal holding the `=`.
    pub value_seg: usize,
    pub value_byte: usize,
}

/// `name=`, `name+=`, `name[sub]=` and `name[sub]+=` at the start of a word's
/// segments. A slice rather than a `Word` so the lexer can ask it of a word it
/// has not finished building.
pub fn assignment_shape(segs: &[Seg]) -> Option<AssignShape<'_>> {
    let Some(Seg::Lit(first)) = segs.first() else {
        return None;
    };
    let end = first
        .bytes()
        .position(|b| !(b == b'_' || b.is_ascii_alphanumeric()))
        .unwrap_or(first.len());
    let name = first.get(..end)?;
    if !is_name(name) {
        return None;
    }
    let operator = |seg: usize, text: &str, at: usize| -> Option<(bool, usize, usize)> {
        let rest = text.get(at..)?;
        if rest.starts_with('=') {
            Some((false, seg, at + 1))
        } else if rest.starts_with("+=") {
            Some((true, seg, at + 2))
        } else {
            None
        }
    };
    if first.as_bytes().get(end) != Some(&b'[') {
        let (append, value_seg, value_byte) = operator(0, first, end)?;
        return Some(AssignShape {
            name,
            index_end: None,
            append,
            value_seg,
            value_byte,
        });
    }
    let (seg_no, at) = close_subscript(segs, end)?;
    let Some(Seg::Lit(text)) = segs.get(seg_no) else {
        return None;
    };
    let (append, value_seg, value_byte) = operator(seg_no, text, at + 1)?;
    Some(AssignShape {
        name,
        index_end: Some((seg_no, at)),
        append,
        value_seg,
        value_byte,
    })
}

/// The `]` closing the `[` at byte `open` of segment 0, as (segment, byte).
/// Brackets count only in literal text: a quoted or expanded `]` is part of
/// what the brackets hold.
fn close_subscript(segs: &[Seg], open: usize) -> Option<(usize, usize)> {
    let mut depth = 0usize;
    for (seg_no, seg) in segs.iter().enumerate() {
        let Seg::Lit(text) = seg else {
            continue;
        };
        let start = if seg_no == 0 { open } else { 0 };
        for (at, b) in text.bytes().enumerate().skip(start) {
            match b {
                b'[' => depth += 1,
                b']' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return Some((seg_no, at));
                    }
                }
                _ => {}
            }
        }
    }
    None
}

/// Split an owned word at an assignment's boundaries: the subscript runs from
/// byte `index_from` of segment 0 to `index_end`, and the value from
/// `value_at` to the end. A literal can hold the end of one part and the start
/// of the next -- `a[1]=x` is one segment carrying all three -- and any other
/// segment lies wholly inside one part, so it MOVES there: copying an
/// expansion segment would be an allocation with no fallible form.
pub fn split_word(
    w: Word,
    index_from: usize,
    index_end: Option<(usize, usize)>,
    value_at: (usize, usize),
) -> Result<(Option<Word>, Word), crate::allocation::Error> {
    let (value_seg, value_byte) = value_at;
    let mut index: Vec<Seg> = Vec::new();
    let mut value: Vec<Seg> = Vec::new();
    for (n, seg) in w.0.into_iter().enumerate() {
        match seg {
            Seg::Lit(text) => {
                if let Some((end_seg, end_byte)) = index_end {
                    if n <= end_seg {
                        let from = if n == 0 { index_from } else { 0 };
                        let to = if n == end_seg { end_byte } else { text.len() };
                        push_lit(&mut index, text.get(from..to))?;
                    }
                }
                if n >= value_seg {
                    let from = if n == value_seg { value_byte } else { 0 };
                    push_lit(&mut value, text.get(from..))?;
                }
            }
            other => {
                let into = match index_end {
                    Some((end_seg, _)) if n < end_seg => &mut index,
                    _ => &mut value,
                };
                crate::allocation::push(into, other)?;
            }
        }
    }
    Ok((index_end.map(|_| Word(index)), Word(value)))
}

/// Append the non-empty part of a split literal.
fn push_lit(out: &mut Vec<Seg>, text: Option<&str>) -> Result<(), crate::allocation::Error> {
    match text {
        Some(t) if !t.is_empty() => {
            let lit = crate::allocation::string(t)?;
            crate::allocation::push(out, Seg::Lit(lit))
        }
        _ => Ok(()),
    }
}

/// Classify one word of a compound assignment: `[sub]=value`, with the
/// brackets and the `=` unquoted, or a plain word.
pub fn element(w: Word) -> Result<Elem, crate::allocation::Error> {
    let keyed = match w.0.first() {
        Some(Seg::Lit(first)) if first.starts_with('[') => {
            close_subscript(&w.0, 0).filter(|&(seg, at)| match w.0.get(seg) {
                Some(Seg::Lit(text)) => text.as_bytes().get(at + 1) == Some(&b'='),
                _ => false,
            })
        }
        _ => None,
    };
    let Some((seg, at)) = keyed else {
        return Ok(Elem::Plain(w));
    };
    let (index, value) = split_word(w, 1, Some((seg, at)), (seg, at + 2))?;
    Ok(Elem::Keyed {
        index: index.unwrap_or_default(),
        value,
    })
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
        /// `None` for a definition nothing can ever name -- a quoted or an
        /// expanded one, which ash files under the word's own text. Parsed
        /// and not registered, which is ash's own unreachable definition
        /// observably. A `/` name is NOT one of these: it is registered, and
        /// `Funcs::get` is what refuses it at the lookup.
        name: Option<String>,
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
    Unary {
        op: String,
        arg: Word,
    },
    /// `lhs OP rhs`. The RHS of `==`/`!=`/`=` is a PATTERN, which is why it
    /// stays a `Word` here: its quoting decides, per character, whether a `*`
    /// matches anything or itself.
    Binary {
        op: CondOp,
        lhs: Word,
        rhs: Word,
    },
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

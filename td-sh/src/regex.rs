//! POSIX extended regular expressions, for `[[ str =~ re ]]`.
//!
//! Simulated as a Thompson NFA -- a set of live states advanced one character
//! at a time -- rather than by backtracking. That is not a performance
//! preference: a backtracking matcher runs in exponential time on patterns as
//! ordinary as `(a+)+b`, and this engine's input is a REGEX A SCRIPT SUPPLIED,
//! usually from a variable. A shell that can be hung by `[[ $x =~ $re ]]` has
//! handed the caller of a script control over whether it terminates. The state
//! set makes the cost O(text * program) with no input able to change that.
//!
//! Only the boolean is answered here. bash records the match and its groups in
//! `BASH_REMATCH`, which is an ARRAY, and td-sh has none -- so submatch capture
//! is deliberately absent rather than half-built (see the `=~` paragraph in the
//! landing commit).

use crate::expand::QChar;
use crate::pattern::ClassItem;

/// The parser's nesting bound, shared in spirit with the shell parser's own:
/// `((((…))))` must be an error rather than a stack overflow, which is the
/// shell DYING -- Rust aborts on one whatever the panic strategy.
const MAX_DEPTH: u32 = 256;

/// The compiler's budget, spent one unit per `emit` call. It bounds the
/// program's SIZE, because almost every call appends -- but it is spent per
/// CALL rather than per instruction on purpose: a repetition whose body emits
/// NOTHING (`(){4000000000}`) appends no instruction however many times it
/// runs, so a size-only bound never fires and the compile spins instead. That
/// is the same denial the NFA exists to prevent, arriving one stage earlier;
/// measured before the fix, `[[ x =~ (){4000000000} ]]` did not return.
const MAX_EMIT: usize = 100_000;

/// The zero-width assertions glibc adds to ERE and bash inherits. Each is a
/// question about the two characters STRADDLING a position, which is why they
/// cannot be classes: `\b` matches where a word begins or ends and consumes
/// nothing.
#[derive(Clone, Copy, Debug)]
enum Boundary {
    /// `\b`
    Word,
    /// `\B`
    NotWord,
    /// `\<`
    WordStart,
    /// `\>`
    WordEnd,
}

/// A word character for the boundary assertions. Derived from `\w`'s OWN
/// members rather than spelled out again: the two agreed only because both
/// said `is_alphanumeric()`, so changing what `[[:alnum:]]` covers would have
/// moved `\w` and left `\b` behind, silently and with no test to catch it.
fn is_word_char(c: char) -> bool {
    crate::pattern::class_matches(false, &word_class(), c)
}

#[derive(Clone, Debug)]
enum Node {
    Empty,
    Lit(char),
    /// `.` -- any character, NEWLINE INCLUDED. bash does not set `REG_NEWLINE`,
    /// so `[[ $'a\nb' =~ a.b ]]` is true; measured rather than assumed.
    Any,
    /// An index into the parser's class table. The members are held ONCE per
    /// bracket expression in the pattern rather than in the node, so a counted
    /// repetition -- which copies its body -- cannot multiply them.
    Class(usize),
    Cat(Vec<Node>),
    Alt(Vec<Node>),
    Rep {
        node: Box<Node>,
        min: u32,
        /// `None` is unbounded (`*`, `+`, `{n,}`).
        max: Option<u32>,
    },
    /// `^`
    Start,
    /// `$`
    End,
    /// `\b`, `\B`, `\<`, `\>`.
    Assert(Boundary),
    /// `( … )`. Kept in the tree rather than flattened away because a group is
    /// not its content for one purpose: `^*` has no preceding expression to
    /// repeat and is refused, while `(^)*` repeats a GROUP and bash accepts it.
    /// Flattening made the two indistinguishable and refused both.
    Group(Box<Node>),
}

#[derive(Clone, Debug)]
enum Inst {
    Char(char),
    Any,
    /// An index into `Regex::classes`. INTERNED rather than inlined because a
    /// counted repetition COPIES its body: inlining meant `[<50k members>]{100k}`
    /// cloned the member list per copy, which is hundreds of gigabytes and
    /// aborted the process on the allocation. An index is one word however
    /// often it is repeated.
    Class(usize),
    /// Try `a` first, then `b`. Both are explored -- there is no preference,
    /// because only "does it match" is asked.
    Split(usize, usize),
    Jmp(usize),
    AssertStart,
    AssertEnd,
    AssertBoundary(Boundary),
    Match,
}

pub struct Regex {
    prog: Vec<Inst>,
    /// Bracket expressions, referenced by index from `Inst::Class`.
    classes: Vec<(bool, Vec<ClassItem>)>,
}

/// Compile an ERE. The pattern arrives as expanded CHARACTERS rather than text
/// because quoting decides, per character, whether a metacharacter is one:
/// `[[ $x =~ "a.c" ]]` matches a literal dot, and `[[ $x =~ $re ]]` -- the
/// documented idiom -- treats the variable's contents as a regex. A flattened
/// string cannot carry that distinction, which is the same reason `case`'s
/// patterns are `QChar`s.
pub fn compile(pat: &[QChar]) -> Result<Regex, String> {
    let mut p = Parser { pat, i: 0, depth: 0, classes: Vec::new() };
    let node = p.alt()?;
    // A backstop rather than a reachable path: every character is consumable
    // somewhere at the top level, `)` included, so `alt` should always arrive
    // at the end. Kept because "the parser stopped early" must never be
    // silently read as a successful compile.
    if p.i < pat.len() {
        return Err("unmatched `)`".to_string());
    }
    let mut prog = Vec::new();
    let mut fuel = MAX_EMIT;
    emit(&node, &mut prog, &mut fuel)?;
    prog.push(Inst::Match);
    Ok(Regex { prog, classes: p.classes })
}

struct Parser<'a> {
    pat: &'a [QChar],
    i: usize,
    depth: u32,
    /// Every bracket expression in the pattern, in the order met. `Node::Class`
    /// indexes this, and `Regex` takes it whole.
    classes: Vec<(bool, Vec<ClassItem>)>,
}

/// `\w`'s members: `[[:alnum:]_]`.
fn word_class() -> Vec<ClassItem> {
    vec![ClassItem::Named("alnum".into()), ClassItem::Ch('_')]
}

impl Parser<'_> {
    /// Record a bracket expression and return the node that indexes it. The
    /// `\w`-family escapes are classes with no brackets written round them, so
    /// they arrive here rather than through `bracket`.
    fn intern_class(&mut self, negated: bool, items: Vec<ClassItem>) -> Node {
        let at = self.classes.len();
        self.classes.push((negated, items));
        Node::Class(at)
    }

    /// The character at the cursor when it is an unquoted `c` -- i.e. when it is
    /// acting as a metacharacter. A quoted one is a literal and answers `false`.
    fn at_meta(&self, c: char) -> bool {
        matches!(self.pat.get(self.i), Some(q) if q.c == c && !q.quoted)
    }

    fn alt(&mut self) -> Result<Node, String> {
        let mut branches = vec![self.cat()?];
        while self.at_meta('|') {
            self.i += 1;
            branches.push(self.cat()?);
        }
        if branches.len() == 1 {
            return branches.pop().ok_or_else(|| "empty alternation".to_string());
        }
        Ok(Node::Alt(branches))
    }

    fn cat(&mut self) -> Result<Node, String> {
        let mut items = Vec::new();
        // A `)` closes a group only when one is OPEN. At the top level glibc
        // reads it as an ordinary character: `re="a)"; [[ abc =~ $re ]]` is
        // FALSE in bash rather than an error, so stopping here unconditionally
        // would turn a legal pattern into a diagnostic. (Written literally,
        // `[[ abc =~ a) ]]` is a syntax error in both shells -- the shell's
        // parser refuses it before any regex is compiled.)
        while self.i < self.pat.len()
            && !self.at_meta('|')
            && !(self.at_meta(')') && self.depth > 0)
        {
            items.push(self.rep()?);
        }
        match items.len() {
            0 => Ok(Node::Empty),
            1 => items.pop().ok_or_else(|| "empty sequence".to_string()),
            _ => Ok(Node::Cat(items)),
        }
    }

    /// An atom with any run of postfix repetitions. A run rather than one,
    /// because glibc accepts `a**` and so must this: measured, `[[ abc =~ a** ]]`
    /// is true in bash where a stricter reading would call it an error.
    fn rep(&mut self) -> Result<Node, String> {
        let mut node = self.atom()?;
        // Each postfix operator wraps the last in another `Rep`, so a RUN of
        // them nests as deeply as it is long -- and both `emit` and the tree's
        // own `Drop` walk that spine. `a` followed by 200k `*` aborted with
        // SIGABRT before this bound, which is the shell dying rather than
        // reporting a bad pattern.
        let mut chain = 0u32;
        loop {
            // glibc refuses a repeated ANCHOR (`^*`): there is no preceding
            // expression to repeat. Measured -- bash answers 2 where this
            // compiled and matched the empty prefix.
            if matches!(node, Node::Start | Node::End | Node::Assert(_))
                && (self.at_meta('*') || self.at_meta('+') || self.at_meta('?') || self.at_meta('{'))
            {
                return Err("nothing to repeat before a repetition".to_string());
            }
            let (min, max) = if self.at_meta('*') {
                self.i += 1;
                (0, None)
            } else if self.at_meta('+') {
                self.i += 1;
                (1, None)
            } else if self.at_meta('?') {
                self.i += 1;
                (0, Some(1))
            } else if self.at_meta('{') {
                self.interval()?
            } else {
                break;
            };
            chain += 1;
            if chain > MAX_DEPTH {
                return Err("too many repetitions in a row".to_string());
            }
            node = Node::Rep { node: Box::new(node), min, max };
        }
        Ok(node)
    }

    /// `{n}`, `{n,}` or `{n,m}` at the cursor. A `{` that does not open one is
    /// an error rather than a literal brace: bash refuses `a{` with status 2,
    /// which is measurable and is what this reproduces.
    fn interval(&mut self) -> Result<(u32, Option<u32>), String> {
        self.i += 1;
        let Some(min) = self.number() else {
            return Err("`{` is not a valid repetition".to_string());
        };
        let max = if self.at_meta(',') {
            self.i += 1;
            self.number()
        } else {
            Some(min)
        };
        if !self.at_meta('}') {
            return Err("unmatched `{`".to_string());
        }
        self.i += 1;
        if let Some(m) = max {
            if m < min {
                return Err("repetition range is inverted".to_string());
            }
        }
        Ok((min, max))
    }

    /// A run of unquoted digits, or `None` when there are none.
    fn number(&mut self) -> Option<u32> {
        let mut value: Option<u32> = None;
        while let Some(q) = self.pat.get(self.i) {
            if q.quoted || !q.c.is_ascii_digit() {
                break;
            }
            let digit = u32::from(q.c).wrapping_sub(u32::from('0'));
            // Saturating rather than wrapping: an absurd count is refused by
            // the program-size bound below, not silently turned into a small one.
            value = Some(value.unwrap_or(0).saturating_mul(10).saturating_add(digit));
            self.i += 1;
        }
        value
    }

    fn atom(&mut self) -> Result<Node, String> {
        let Some(q) = self.pat.get(self.i) else {
            return Ok(Node::Empty);
        };
        if q.quoted {
            self.i += 1;
            return Ok(Node::Lit(q.c));
        }
        match q.c {
            '(' => {
                self.depth += 1;
                if self.depth > MAX_DEPTH {
                    self.depth -= 1;
                    return Err("regular expression nests too deeply".to_string());
                }
                self.i += 1;
                let inner = self.alt();
                self.depth -= 1;
                let inner = inner?;
                if !self.at_meta(')') {
                    return Err("unmatched `(`".to_string());
                }
                self.i += 1;
                Ok(Node::Group(Box::new(inner)))
            }
            '[' => self.bracket(),
            '.' => {
                self.i += 1;
                Ok(Node::Any)
            }
            '^' => {
                self.i += 1;
                Ok(Node::Start)
            }
            '$' => {
                self.i += 1;
                Ok(Node::End)
            }
            '\\' => {
                self.i += 1;
                let Some(next) = self.pat.get(self.i) else {
                    return Err("trailing backslash".to_string());
                };
                self.i += 1;
                match next.c {
                    // glibc's ERE extensions, which bash inherits. Serving them
                    // as literals -- which is what an escape otherwise means --
                    // silently answered the wrong question: `\bfoo\b` matched
                    // the text "bfoob".
                    'w' => Ok(self.intern_class(false, word_class())),
                    'W' => Ok(self.intern_class(true, word_class())),
                    's' => Ok(self.intern_class(false, vec![ClassItem::Named("space".into())])),
                    'S' => Ok(self.intern_class(true, vec![ClassItem::Named("space".into())])),
                    // `\\`` and `\\'` are the buffer anchors. bash does not set
                    // `REG_NEWLINE`, so they are exactly `^` and `$` -- which
                    // also gets their repetition refused for free, since
                    // `\\`*` has no more of a preceding expression than `^*`.
                    '`' => Ok(Node::Start),
                    '\'' => Ok(Node::End),
                    'b' => Ok(Node::Assert(Boundary::Word)),
                    'B' => Ok(Node::Assert(Boundary::NotWord)),
                    '<' => Ok(Node::Assert(Boundary::WordStart)),
                    '>' => Ok(Node::Assert(Boundary::WordEnd)),
                    // A BACKREFERENCE cannot be served by a state machine: it
                    // asks what an earlier group captured, which is why every
                    // engine that has them backtracks -- and backtracking is
                    // the exponential behaviour this one exists to avoid.
                    // Refused rather than read as a literal digit, because a
                    // silent wrong answer is what this whole arm is fixing.
                    '1'..='9' => Err(format!(
                        "backreference `\\{}` needs a backtracking matcher",
                        next.c
                    )),
                    other => Ok(Node::Lit(other)),
                }
            }
            '*' | '+' | '?' => Err(format!("nothing to repeat before `{}`", q.c)),
            // A `{` reaches here only with no atom before it, so it cannot be
            // opening an interval. glibc refuses that rather than reading a
            // literal brace -- `[[ { =~ { ]]` is an error in bash, measured --
            // and the corpus grades the refusal.
            '{' => Err("`{` is not a valid repetition".to_string()),
            // A stray `)` reaches here only from the top level, where glibc
            // reads it as a literal: `re="a)"; [[ abc =~ $re ]]` is FALSE in
            // bash, not an error. Measured; a stricter reading would diverge.
            _ => {
                self.i += 1;
                Ok(Node::Lit(q.c))
            }
        }
    }

    /// `[...]` -- a bracket expression, sharing `case`'s member syntax so
    /// `[[:alpha:]]`, ranges and negation mean one thing across the shell.
    fn bracket(&mut self) -> Result<Node, String> {
        self.i += 1;
        let negated = self.at_meta('^');
        if negated {
            self.i += 1;
        }
        let mut items = Vec::new();
        // A `]` FIRST is a literal member, which is how POSIX spells a class
        // containing it -- there is no escape inside a bracket expression.
        let mut first = true;
        loop {
            let Some(q) = self.pat.get(self.i) else {
                return Err("unmatched `[`".to_string());
            };
            if q.c == ']' && !first {
                self.i += 1;
                return Ok(self.intern_class(negated, items));
            }
            first = false;
            if q.c == '[' && matches!(self.pat.get(self.i + 1), Some(n) if n.c == ':') {
                self.i += 2;
                let mut name = String::new();
                loop {
                    match self.pat.get(self.i) {
                        Some(n) if n.c == ':' => break,
                        Some(n) => {
                            name.push(n.c);
                            self.i += 1;
                        }
                        None => return Err("unmatched `[:`".to_string()),
                    }
                }
                if !matches!(self.pat.get(self.i + 1), Some(n) if n.c == ']') {
                    return Err("unmatched `[:`".to_string());
                }
                self.i += 2;
                // Refused rather than left to never match. The negated form is
                // why: `[^[:bogus:]]` as an empty class matches EVERY
                // character, so a typo silently takes the branch it was written
                // to exclude. glibc reports it, measured, and so does this.
                if !crate::pattern::is_class_name(&name) {
                    return Err(format!("unknown character class `[:{name}:]`"));
                }
                items.push(ClassItem::Named(name));
                continue;
            }
            let lo = q.c;
            // A `-` is a range only BETWEEN two members: `[a-]` ends with a
            // literal hyphen, as `case`'s does.
            let dash = matches!(self.pat.get(self.i + 1), Some(d) if d.c == '-');
            let after = self.pat.get(self.i + 2);
            match (dash, after) {
                (true, Some(hi)) if hi.c != ']' => {
                    // A REVERSED range is a mistake, not an empty set: glibc
                    // reports `[z-a]` and answering false would hide it.
                    if hi.c < lo {
                        return Err("range is out of order in a bracket expression".to_string());
                    }
                    items.push(ClassItem::Range(lo, hi.c));
                    self.i += 3;
                }
                _ => {
                    items.push(ClassItem::Ch(lo));
                    self.i += 1;
                }
            }
        }
    }
}

/// Append `node`'s program. Counted repetition is expanded by COPYING the body,
/// so the budget is spent HERE, one unit per call -- `{100}{100}{100}` is
/// twelve characters and a million copies, and a body that appends nothing
/// would never be caught by a bound on the program's length.
fn emit(node: &Node, prog: &mut Vec<Inst>, fuel: &mut usize) -> Result<(), String> {
    match fuel.checked_sub(1) {
        Some(left) => *fuel = left,
        None => return Err("regular expression is too large".to_string()),
    }
    match node {
        Node::Empty => Ok(()),
        Node::Lit(c) => {
            prog.push(Inst::Char(*c));
            Ok(())
        }
        Node::Any => {
            prog.push(Inst::Any);
            Ok(())
        }
        Node::Class(at) => {
            prog.push(Inst::Class(*at));
            Ok(())
        }
        Node::Start => {
            prog.push(Inst::AssertStart);
            Ok(())
        }
        Node::End => {
            prog.push(Inst::AssertEnd);
            Ok(())
        }
        Node::Assert(b) => {
            prog.push(Inst::AssertBoundary(*b));
            Ok(())
        }
        Node::Group(inner) => emit(inner, prog, fuel),
        Node::Cat(items) => {
            for item in items {
                emit(item, prog, fuel)?;
            }
            Ok(())
        }
        Node::Alt(branches) => emit_alt(branches, prog, fuel),
        Node::Rep { node, min, max } => emit_rep(node, *min, *max, prog, fuel),
    }
}

/// ITERATIVE over the branches, deliberately. Recursing once per branch made a
/// 200k-term `a|a|a|…` overflow the stack and abort -- a flat expression, so no
/// nesting bound would have caught it. Each branch is emitted with a `Split`
/// before it and a `Jmp` after, and both are patched once the end is known.
fn emit_alt(branches: &[Node], prog: &mut Vec<Inst>, fuel: &mut usize) -> Result<(), String> {
    let Some((last, leading)) = branches.split_last() else {
        return Ok(());
    };
    let mut jumps: Vec<usize> = Vec::with_capacity(leading.len());
    for branch in leading {
        let split_at = prog.len();
        prog.push(Inst::Jmp(0)); // patched to Split once this branch's length is known
        emit(branch, prog, fuel)?;
        jumps.push(prog.len());
        prog.push(Inst::Jmp(0)); // patched to the end below
        let next_at = prog.len();
        if let Some(slot) = prog.get_mut(split_at) {
            *slot = Inst::Split(split_at + 1, next_at);
        }
    }
    emit(last, prog, fuel)?;
    let end = prog.len();
    for jump_at in jumps {
        if let Some(slot) = prog.get_mut(jump_at) {
            *slot = Inst::Jmp(end);
        }
    }
    Ok(())
}

fn emit_rep(
    node: &Node,
    min: u32,
    max: Option<u32>,
    prog: &mut Vec<Inst>,
    fuel: &mut usize,
) -> Result<(), String> {
    // The mandatory copies first. Each costs fuel whether or not it appends,
    // which is what bounds a body that compiles to nothing.
    for _ in 0..min {
        emit(node, prog, fuel)?;
    }
    match max {
        None => {
            // `x*` as a loop: split into the body or past it, and jump back.
            let split_at = prog.len();
            prog.push(Inst::Jmp(0));
            emit(node, prog, fuel)?;
            prog.push(Inst::Jmp(split_at));
            let end = prog.len();
            if let Some(slot) = prog.get_mut(split_at) {
                *slot = Inst::Split(split_at + 1, end);
            }
            Ok(())
        }
        Some(m) => {
            // The optional copies, each guarded by its own split to the end.
            let optional = m.saturating_sub(min);
            let mut splits = Vec::new();
            for _ in 0..optional {
                let split_at = prog.len();
                prog.push(Inst::Jmp(0));
                splits.push(split_at);
                emit(node, prog, fuel)?;
            }
            let end = prog.len();
            for split_at in splits {
                if let Some(slot) = prog.get_mut(split_at) {
                    *slot = Inst::Split(split_at + 1, end);
                }
            }
            Ok(())
        }
    }
}

impl Regex {
    /// True when the pattern matches ANYWHERE in `text` -- `=~` is a search,
    /// not a whole-string test, which is why `[[ abc =~ b ]]` holds.
    pub fn is_match(&self, text: &str) -> bool {
        let chars: Vec<char> = text.chars().collect();
        let len = chars.len();
        let mut current: Vec<usize> = Vec::with_capacity(self.prog.len());
        let mut next: Vec<usize> = Vec::with_capacity(self.prog.len());
        let mut on_current = vec![false; self.prog.len()];
        let mut on_next = vec![false; self.prog.len()];
        let mut stack: Vec<usize> = Vec::with_capacity(self.prog.len());
        for pos in 0..=len {
            // The search start is injected at every position, which is what
            // makes this unanchored without a `.*` prefix that `^` would then
            // have to see through.
            self.add(&mut current, &mut on_current, &mut stack, 0, pos, &chars);
            for pc in &current {
                if matches!(self.prog.get(*pc), Some(Inst::Match)) {
                    return true;
                }
            }
            let Some(c) = chars.get(pos) else { break };
            next.clear();
            for slot in on_next.iter_mut() {
                *slot = false;
            }
            for pc in &current {
                let consumes = match self.prog.get(*pc) {
                    Some(Inst::Char(want)) => want == c,
                    Some(Inst::Any) => true,
                    Some(Inst::Class(at)) => match self.classes.get(*at) {
                        Some((negated, items)) => {
                            crate::pattern::class_matches(*negated, items, *c)
                        }
                        None => false,
                    },
                    _ => false,
                };
                if consumes {
                    self.add(&mut next, &mut on_next, &mut stack, pc + 1, pos + 1, &chars);
                }
            }
            std::mem::swap(&mut current, &mut next);
            std::mem::swap(&mut on_current, &mut on_next);
        }
        false
    }

    /// Add `pc` and everything reachable from it without consuming a character.
    /// Iterative with an explicit stack: the epsilon closure of a program with
    /// 100k instructions is 100k deep, and recursion there is the overflow this
    /// engine exists to avoid.
    fn add(
        &self,
        list: &mut Vec<usize>,
        seen: &mut [bool],
        stack: &mut Vec<usize>,
        pc: usize,
        pos: usize,
        chars: &[char],
    ) {
        let len = chars.len();
        stack.clear();
        stack.push(pc);
        while let Some(pc) = stack.pop() {
            match seen.get(pc) {
                Some(true) | None => continue,
                Some(false) => {}
            }
            if let Some(slot) = seen.get_mut(pc) {
                *slot = true;
            }
            match self.prog.get(pc) {
                Some(Inst::Jmp(t)) => stack.push(*t),
                Some(Inst::Split(a, b)) => {
                    stack.push(*b);
                    stack.push(*a);
                }
                Some(Inst::AssertStart) => {
                    if pos == 0 {
                        stack.push(pc + 1);
                    }
                }
                Some(Inst::AssertEnd) => {
                    if pos == len {
                        stack.push(pc + 1);
                    }
                }
                Some(Inst::AssertBoundary(kind)) => {
                    // Both sides, because every one of these is a question
                    // about the PAIR: off either end counts as a non-word.
                    let before = pos
                        .checked_sub(1)
                        .and_then(|i| chars.get(i))
                        .is_some_and(|c| is_word_char(*c));
                    let after = chars.get(pos).is_some_and(|c| is_word_char(*c));
                    let ok = match kind {
                        Boundary::Word => before != after,
                        Boundary::NotWord => before == after,
                        Boundary::WordStart => !before && after,
                        Boundary::WordEnd => before && !after,
                    };
                    if ok {
                        stack.push(pc + 1);
                    }
                }
                Some(_) => list.push(pc),
                None => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(s: &str) -> Vec<QChar> {
        s.chars().map(|c| QChar { c, quoted: false, expanded: false }).collect()
    }

    fn m(pat: &str, text: &str) -> bool {
        compile(&q(pat)).unwrap().is_match(text)
    }

    #[test]
    fn literals_and_search() {
        assert!(m("b", "abc"));
        assert!(m("abc", "abc"));
        assert!(!m("z", "abc"));
        assert!(m("", "abc"));
        assert!(m("", ""));
    }

    #[test]
    fn anchors_bind_to_the_ends() {
        assert!(m("^a", "abc"));
        assert!(!m("^b", "abc"));
        assert!(m("c$", "abc"));
        assert!(!m("b$", "abc"));
        assert!(m("^abc$", "abc"));
        assert!(!m("^ab$", "abc"));
    }

    /// bash does not set `REG_NEWLINE`, so `.` is genuinely any character.
    #[test]
    fn dot_matches_a_newline() {
        assert!(m("a.b", "a\nb"));
        assert!(m("a.c", "abc"));
        assert!(!m("a.c", "ac"));
    }

    #[test]
    fn repetition() {
        assert!(m("^ab*c$", "ac"));
        assert!(m("^ab*c$", "abbbc"));
        assert!(!m("^ab+c$", "ac"));
        assert!(m("^ab+c$", "abc"));
        assert!(m("^ab?c$", "ac"));
        assert!(m("^ab?c$", "abc"));
        assert!(!m("^ab?c$", "abbc"));
        assert!(m("^a{2}$", "aa"));
        assert!(!m("^a{2}$", "aaa"));
        assert!(m("^a{2,}$", "aaaa"));
        assert!(m("^a{2,3}$", "aaa"));
        assert!(!m("^a{2,3}$", "aaaa"));
        // glibc accepts a repeated repetition; measured in bash.
        assert!(m("a**", "abc"));
    }

    #[test]
    fn alternation_and_grouping() {
        assert!(m("^(a|b)$", "a"));
        assert!(m("^(a|b)$", "b"));
        assert!(!m("^(a|b)$", "c"));
        assert!(m("^(ab|cd)+$", "abcdab"));
        assert!(m("a|z", "abc"));
        assert!(m("^(a|b|c)$", "c"));
        // An empty branch matches the empty string.
        assert!(m("^(a|)$", ""));
    }

    #[test]
    fn bracket_expressions() {
        assert!(m("^[abc]$", "b"));
        assert!(!m("^[abc]$", "d"));
        assert!(m("^[a-z]+$", "hello"));
        assert!(!m("^[a-z]+$", "Hello"));
        assert!(m("^[^a-z]$", "X"));
        assert!(m("[[:alpha:]]+", "abc"));
        assert!(m("^[[:digit:]]{3}$", "123"));
        assert!(!m("^[[:digit:]]{3}$", "12a"));
        // A `]` first is a member, and a trailing `-` is literal.
        assert!(m("^[]]$", "]"));
        assert!(m("^[a-]$", "-"));
    }

    #[test]
    fn escapes_make_a_metacharacter_literal() {
        assert!(m("^a\\.c$", "a.c"));
        assert!(!m("^a\\.c$", "abc"));
        assert!(m("^a\\*$", "a*"));
    }

    /// Quoting in the SOURCE is per-character, which is the whole reason the
    /// pattern is `QChar`s: `"a.c"` is a literal dot while `$re` is a regex.
    #[test]
    fn a_quoted_metacharacter_is_a_literal() {
        let mut pat = q("a.c");
        if let Some(dot) = pat.get_mut(1) {
            dot.quoted = true;
        }
        let re = compile(&pat).unwrap();
        assert!(re.is_match("a.c"));
        assert!(!re.is_match("abc"));
    }

    #[test]
    fn malformed_patterns_are_refused() {
        for bad in ["a[", "*", "+", "?", "a{", "a{2", "a{3,2}", "(a", "a\\", "{", "{2}"] {
            assert!(compile(&q(bad)).is_err(), "should have been refused: {bad:?}");
        }
        // ...but a stray `)` is a literal, as glibc reads it.
        assert!(!m("a)", "abc"));
        assert!(m("a)", "a)"));
    }

    /// The property the NFA exists for. A backtracking matcher takes
    /// exponential time on this shape; here it is linear, so the assertion is
    /// simply that it RETURNS.
    #[test]
    fn a_pathological_pattern_does_not_blow_up() {
        assert!(!m("^(a+)+b$", &"a".repeat(60)));
        assert!(!m("^(a|a)*b$", &"a".repeat(60)));
        assert!(m("^(a+)+b$", &format!("{}b", "a".repeat(60))));
    }

    #[test]
    fn deep_nesting_errors_instead_of_overflowing() {
        let deep = format!("{}a{}", "(".repeat(100_000), ")".repeat(100_000));
        assert!(compile(&q(&deep)).is_err());
    }

    #[test]
    fn an_enormous_expansion_is_refused() {
        assert!(compile(&q("^(a{1000}){1000}$")).is_err());
    }

    /// Two shapes that are FLAT rather than nested, so no nesting bound sees
    /// them: a run of postfix operators (each wrapping the last in another
    /// `Rep`) and a long alternation (which recursed once per branch). Both
    /// aborted the process with SIGABRT -- the shell dying -- before the chain
    /// bound and the iterative `emit_alt`.
    #[test]
    fn flat_pathological_shapes_error_instead_of_overflowing() {
        let deep_rep = format!("a{}", "*".repeat(200_000));
        assert!(compile(&q(&deep_rep)).is_err());
        let wide_alt = vec!["a"; 200_000].join("|");
        assert!(compile(&q(&wide_alt)).is_err());
        // ...and alternation of an ordinary width still compiles and matches.
        let ok_alt = vec!["a"; 100].join("|");
        assert!(compile(&q(&ok_alt)).unwrap().is_match("a"));
        assert!(!compile(&q(&ok_alt)).unwrap().is_match("z"));
    }

    /// An invalid bracket expression is REFUSED rather than left to match
    /// nothing. The negated form is the argument: an unknown class read as the
    /// empty set makes `[^[:bogus:]]` match every character, so a typo silently
    /// takes the branch it was written to exclude. glibc reports all of these.
    #[test]
    fn invalid_bracket_expressions_are_refused() {
        for bad in ["[[:bogus:]]", "[^[:bogus:]]", "[z-a]", "[[:Alpha:]]"] {
            assert!(compile(&q(bad)).is_err(), "should have been refused: {bad:?}");
        }
        // A single-character range is legal and is not "reversed".
        assert!(compile(&q("[a-a]")).unwrap().is_match("a"));
    }

    /// Repeating a zero-width assertion has no preceding expression to repeat,
    /// which glibc refuses; this compiled and matched the empty prefix.
    #[test]
    fn a_repeated_anchor_is_refused() {
        for bad in ["^*", "^+", "^?", "$*", "^{2}"] {
            assert!(compile(&q(bad)).is_err(), "should have been refused: {bad:?}");
        }
        assert!(compile(&q("^a")).unwrap().is_match("ab"));
        // A PARENTHESISED anchor is a group, and repeating a group is legal --
        // bash accepts every one of these. Flattening `( … )` out of the tree
        // made them indistinguishable from the bare form above and refused
        // them all, which is why a group is its own node.
        for ok in ["(^)*", "(^)+", "(^)?", "(^){2}", "((^))*", "($)*", "($)?"] {
            assert!(compile(&q(ok)).is_ok(), "should have compiled: {ok:?}");
        }
    }

    /// A bracket expression's members are held ONCE per pattern. Inlining them
    /// into the instruction meant a counted repetition -- which copies its
    /// body -- cloned the member list per copy: a 10k-member class repeated
    /// 10k times aborted the process on a failed allocation, and 6k x 6k took
    /// 864 MB. Both return in milliseconds interned.
    #[test]
    fn a_repeated_class_does_not_multiply_its_members() {
        let big: String = std::iter::repeat_n('b', 10_000).collect();
        let re = compile(&q(&format!("[{big}]{{10000}}"))).unwrap();
        assert!(!re.is_match("x"));
        assert!(compile(&q(&format!("[{big}]{{100000}}"))).is_err(), "budget still bounds it");
    }

    /// glibc's ERE extensions, which bash inherits. Served as ordinary escaped
    /// literals before, which is a SILENT wrong answer: `\bfoo\b` asked
    /// whether the text contained "bfoob".
    #[test]
    fn the_gnu_class_escapes_are_classes() {
        assert!(m(r"^\w+$", "abc"));
        assert!(m(r"^\w+$", "a_1"));
        assert!(!m(r"^\w+$", "a b"));
        assert!(m(r"^\W$", " "));
        assert!(!m(r"^\W$", "_"), "underscore is a word character");
        assert!(m(r"a\sb", "a b"));
        assert!(m(r"a\sb", "a\tb"));
        assert!(!m(r"a\sb", "ab"));
        assert!(m(r"^\S$", "x"));
        assert!(!m(r"^\S$", " "));
        // Non-ASCII is a word character, as `[[:alnum:]]` already had it.
        assert!(m(r"^\w$", "é"));
    }

    /// The boundary assertions are questions about the PAIR of characters
    /// straddling a position, and consume nothing. Off either end counts as a
    /// non-word, which is what makes `\bfoo\b` match a word at the very start.
    #[test]
    fn the_boundary_assertions_straddle_a_position() {
        assert!(m(r"\bfoo\b", "a foo b"));
        assert!(m(r"\bfoo\b", "foo"), "both ends of the subject are boundaries");
        assert!(!m(r"\bfoo\b", "afoob"));
        assert!(m(r"\Bbc", "abc"));
        assert!(!m(r"\Bbc", "a bc"));
        assert!(m(r"\<abc\>", "x abc y"));
        assert!(!m(r"\<abc\>", "xabcy"));
        assert!(!m(r"\<abc", "xabc"));
        assert!(m(r"\<abc", "abc"), "the start of the subject begins a word");
        assert!(m(r"abc\>", "xabc"));
        assert!(!m(r"abc\>", "abcd"), "`\\>` needs a NON-word after it");
        // `\B` holds where NEITHER side is a word character as well as where
        // both are. Without this, reading it as "both" passes every other
        // assertion here.
        assert!(m(r"\B", "  "));
        assert!(m(r"\B", "ab"));
        // Repeating one is refused exactly as repeating an anchor is, and
        // parenthesising it is allowed exactly as `(^)*` is.
        assert!(compile(&q(r"\b*")).is_err());
        assert!(compile(&q(r"(\b)*")).is_ok());
    }

    /// The other two GNU operators, and the reason they are not assertions of
    /// their own: bash does not set `REG_NEWLINE`, so `\`` and `\'` are the
    /// buffer anchors and mean exactly what `^` and `$` mean. Served as
    /// literals they were the same silent wrong answer as the rest -- `\`abc`
    /// asked whether the subject contained a backtick.
    #[test]
    fn the_buffer_anchors_are_start_and_end() {
        assert!(m(r"\`abc", "abc"));
        assert!(!m(r"\`bc", "abc"));
        assert!(m(r"abc\'", "abc"));
        assert!(!m(r"ab\'", "abc"));
        assert!(m(r"\`a\'", "a"));
        assert!(!m(r"\`a\'", "ab"));
        // ...which also gets their repetition refused, since they are the same
        // node `^*` is refused for.
        assert!(compile(&q(r"\`*a")).is_err());
        assert!(compile(&q(r"\'*")).is_err());
        assert!(compile(&q(r"(\`)*a")).is_ok());
    }

    /// `\b`'s notion of a word character IS `\w`'s, rather than a second
    /// spelling of it that happens to agree. Pinned because the two would
    /// otherwise part company the moment `[[:alnum:]]` changed, and nothing
    /// else here would notice.
    #[test]
    fn the_boundary_and_the_class_share_one_word_character() {
        for c in ['a', 'Z', '0', '_', ' ', '-', '.', 'é', '²'] {
            let subject = format!("{c}");
            assert_eq!(
                is_word_char(c),
                compile(&q(r"^\w$")).unwrap().is_match(&subject),
                "`\\b` and `\\w` disagree about {c:?}"
            );
        }
    }

    /// A backreference asks what an earlier group CAPTURED, which no state
    /// machine can answer -- every engine that serves them backtracks, and
    /// backtracking is the exponential behaviour this one exists to avoid. So
    /// it is refused rather than read as a literal digit: bash matches
    /// `(a)\1` against "aa" and this cannot, and saying so beats answering a
    /// different question.
    #[test]
    fn a_backreference_is_refused_rather_than_misread() {
        for bad in [r"(a)\1", r"\1", r"(a)(b)\2"] {
            assert!(compile(&q(bad)).is_err(), "should have been refused: {bad:?}");
        }
        // `\0` is not a backreference and stays a literal, as does any other
        // escaped character the roster does not claim.
        assert!(m(r"\0", "0"));
        assert!(m(r"a\.c", "a.c"));
        assert!(!m(r"a\.c", "abc"));
    }

    /// A repetition whose BODY emits nothing appends no instruction however
    /// many times it runs, so a bound on the program's size never fires while
    /// the compile spins -- the same denial the NFA prevents, one stage
    /// earlier. `(){4000000000}` did not return before the budget was spent
    /// per `emit` CALL instead. bash hangs on the third of these.
    #[test]
    fn a_repetition_of_nothing_cannot_spin_the_compiler() {
        for bad in ["(){4000000000}", "a{4000000000}", "(){1000}{1000}{1000}", "(){100000}"] {
            assert!(compile(&q(bad)).is_err(), "should have been refused: {bad:?}");
        }
    }

    #[test]
    fn multibyte_text_is_matched_by_character() {
        assert!(m("^.$", "é"));
        assert!(m("é", "café"));
        assert!(!m("^.$", "ab"));
    }
}

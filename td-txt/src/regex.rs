//! POSIX regular expressions (BRE and ERE) over BYTES, in the C locale.
//!
//! td-txt replaces busybox `grep`/`sed`, which are byte-oriented under `LC_ALL=C`
//! — the only locale td's image sets. So there is no character decoding here: a
//! pattern and a subject are both byte strings, `.` matches one byte, and
//! `[[:alpha:]]` is the ASCII class. A multibyte-aware engine is a separate,
//! reviewed step, not a silent half-measure.
//!
//! Both dialects compile to one AST. Backreferences (`\1`) are part of POSIX BRE
//! and GNU ERE, which rules out a Thompson/Pike VM, so matching is backtracking
//! with a step budget: a pathological pattern reports `too complex` instead of
//! wedging the caller. Repetition is greedy (POSIX has no lazy quantifier), and the
//! matcher explores the whole space at each start and keeps the LONGEST match —
//! POSIX leftmost-longest, which is where a Perl-style first-match engine would
//! differ (`x\|xy` on `xy`). Exploring was once limited to patterns containing an
//! alternation; a bounded repeat can also need an earlier greedy one to give ground,
//! so that shortcut returned a short match (see `match_from`).

/// A compile error. The message follows GNU's wording where the corpus asserts a
/// diagnostic; callers prefix it with the program name.
#[derive(Clone, Debug)]
pub struct Error {
    pub msg: String,
}

impl Error {
    fn new(msg: impl Into<String>) -> Self {
        Self { msg: msg.into() }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for Error {}

/// Dialect and matching knobs. `escapes` enables the GNU *sed* escape vocabulary
/// (`\n`, `\t`, `\o101`, `\x41`, …) inside the pattern; grep does not take it.
#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    pub ere: bool,
    pub icase: bool,
    pub escapes: bool,
    /// POSIX REG_NEWLINE (sed's `M` flag), carrying the RECORD SEPARATOR it is
    /// relative to: `\n` normally, NUL under `-z`. GNU implements it as TWO
    /// mechanisms, and a pattern can tell them apart:
    ///
    /// - libc's REG_NEWLINE, bound to `\n` whatever the separator is. `^`/`$` gain
    ///   the embedded newlines and `.` / a non-matching bracket list lose them —
    ///   but nothing else does, so `\W` and an explicit `[\n]` still match one.
    ///   Compiled into the pattern; see `parse_atom` and `parse_bracket`.
    /// - Segments, when the separator is NOT a newline (libc cannot express it).
    ///   The anchors move to that byte — `^`/`$` AND `\``/`\'` — for every caller.
    ///   Whether the separator can be CONSUMED depends on the caller: a
    ///   substitution works within one segment, an address over the whole pattern
    ///   space. See `search_subst`, which is the only way to ask for the first.
    ///
    /// So under `-z` a `\n` inside a record stays ordinary to `\W` while the NUL is
    /// unmatchable by `s///` and ordinary to an address, and `s/^d/X/M` does not
    /// match after a `\n`.
    pub reg_newline: Option<u8>,
}

/// A 256-bit byte set.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct ByteSet {
    bits: [u64; 4],
}

impl ByteSet {
    fn empty() -> Self {
        Self { bits: [0; 4] }
    }

    fn insert(&mut self, b: u8) {
        let word = usize::from(b >> 6);
        if let Some(w) = self.bits.get_mut(word) {
            *w |= 1u64 << (b & 63);
        }
    }

    fn remove(&mut self, b: u8) {
        let word = usize::from(b >> 6);
        if let Some(w) = self.bits.get_mut(word) {
            *w &= !(1u64 << (b & 63));
        }
    }

    fn contains(&self, b: u8) -> bool {
        let word = usize::from(b >> 6);
        self.bits.get(word).is_some_and(|w| w & (1u64 << (b & 63)) != 0)
    }

    fn insert_range(&mut self, lo: u8, hi: u8) {
        let mut b = lo;
        loop {
            self.insert(b);
            if b == hi {
                break;
            }
            b = b.saturating_add(1);
        }
    }

    fn negate(&mut self) {
        for w in self.bits.iter_mut() {
            *w = !*w;
        }
    }

    /// Case-fold the set so either case of a member matches.
    fn fold_case(&mut self) {
        for b in b'a'..=b'z' {
            if self.contains(b) {
                self.insert(b - 32);
            }
        }
        for b in b'A'..=b'Z' {
            if self.contains(b) {
                self.insert(b + 32);
            }
        }
    }
}

#[derive(Clone, Debug)]
enum Node {
    Empty,
    Byte(u8),
    /// Any byte. POSIX excludes NUL only in a text file; GNU sed's `.` matches a
    /// newline in the pattern space, so nothing is excluded here.
    Any,
    Class(ByteSet),
    Bol,
    Eol,
    /// `\\``/`\\'` — the BUFFER ends, which `M` does not move (that is what
    /// distinguishes them from `^`/`$`). Separator confinement does move them,
    /// because GNU then matches within a segment and its ends ARE the buffer's.
    BufStart,
    BufEnd,
    /// `\b` (true) / `\B` (false).
    WordBoundary(bool),
    /// `\<` (true) / `\>` (false).
    WordEdge(bool),
    Group(usize, Box<Node>),
    Backref(usize),
    Concat(Vec<Node>),
    Alt(Vec<Node>),
    Repeat {
        node: Box<Node>,
        min: u32,
        max: Option<u32>,
    },
}

/// A compiled pattern.
#[derive(Clone, Debug)]
pub struct Regex {
    root: Node,
    ngroups: usize,
    icase: bool,
    reg_newline: Option<u8>,
    /// The record separator when it is not a newline, i.e. when GNU works in
    /// SEGMENTS. It moves `\`` and `\'` (both match paths) and is what nothing may
    /// consume in a substitution. See `Options::reg_newline`.
    segment: Option<u8>,
    /// Whether the pattern contains a BACKREFERENCE. Not a matching rule of its own,
    /// but GNU is confined to a segment in an ADDRESS too when the pattern has one --
    /// even `\1*`, which need consume nothing, so it is the presence of a backref in
    /// the compiled pattern and not the backref's own match that does it. That is
    /// glibc's backref matcher rather than anything sed documents: `sed -z -n -e
    /// 'N;N' -e '/\(.\)\1/Mp'` over `a\0\0b\0` finds nothing while `/../Mp` and
    /// `/\(a\)\x00/Mp` over the same space both match.
    has_backref: bool,
    /// Bytes that can begin a match, when that is knowable — a cheap skip for the
    /// scan loop. `None` means "anything".
    first: Option<ByteSet>,
    /// Whether the pattern contains an alternation. Not a matching rule — it once
    /// wrongly decided whether to search for the longest match — but the condition
    /// under which the FIRST end found can be catastrophically shorter than the
    /// longest, which is what makes it unsafe as a budget fallback. See `scan`.
    has_alt: bool,
}

/// What an exhausted step budget means for the caller.
///
/// Exploring every end is what leftmost-longest costs, and on a pathological pattern
/// the budget can run out with a match found but not yet proven longest. Whether that
/// match may be reported depends entirely on what the caller does with it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OnBudget {
    /// Fail closed. The caller consumes the SPAN — sed's substitution, `grep -o`,
    /// `grep -x` — where a span that is merely SOME match is wrong output: for
    /// `x\|x\(a\|aa\)*b` the first branch matches one byte and the second the whole
    /// line, so answering with the short one silently rewrites the wrong text. A
    /// diagnosed refusal is the honest answer.
    Fail,
    /// Answer with the match already found. The caller only asks WHETHER the line
    /// matches — `grep` selection, `-c`, `-v`, `-q` — and any match settles that, so
    /// refusing a line the previous release answered would be the worse trade.
    Existence,
}

/// Which spans `search_filtered` may return. `span` tests a candidate `(start, end)`
/// as the matcher reports it; `start` answers the cheaper question "could ANY end at
/// this start pass?", which lets the scan skip a start without exploring it. `start`
/// must not reject a start that `span` would accept some end at, or a match is lost.
pub struct Filter<'a> {
    pub span: &'a dyn Fn(usize, usize) -> bool,
    pub start: &'a dyn Fn(usize) -> bool,
}

/// Byte spans of a match and its groups; `None` where a group did not
/// participate.
type Spans = Vec<Option<(usize, usize)>>;

/// Where a match landed: `spans[0]` is the whole match, `spans[n]` group `n`.
#[derive(Clone, Debug)]
pub struct Captures {
    spans: Spans,
}

impl Captures {
    pub fn start(&self) -> usize {
        self.spans.first().copied().flatten().map_or(0, |(s, _)| s)
    }

    pub fn end(&self) -> usize {
        self.spans.first().copied().flatten().map_or(0, |(_, e)| e)
    }

    /// Byte span of group `n` (`0` = whole match), if it participated.
    pub fn group(&self, n: usize) -> Option<(usize, usize)> {
        self.spans.get(n).copied().flatten()
    }
}

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn lower(b: u8) -> u8 {
    b.to_ascii_lowercase()
}

// ---- parsing -------------------------------------------------------------

struct Parser<'a> {
    pat: &'a [u8],
    pos: usize,
    opts: Options,
    ngroups: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<u8> {
        self.pat.get(self.pos).copied()
    }

    fn peek_at(&self, off: usize) -> Option<u8> {
        self.pat.get(self.pos + off).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let b = self.peek();
        if b.is_some() {
            self.pos += 1;
        }
        b
    }

    fn eat(&mut self, b: u8) -> bool {
        if self.peek() == Some(b) {
            self.pos += 1;
            return true;
        }
        false
    }

    /// BRE spells the group/alternation/interval punctuation with a backslash;
    /// ERE spells it bare. `is_op` answers "does the input at `pos` open <op>",
    /// consuming it when it does.
    fn eat_op(&mut self, ch: u8) -> bool {
        if self.opts.ere {
            return self.eat(ch);
        }
        if self.peek() == Some(b'\\') && self.peek_at(1) == Some(ch) {
            self.pos += 2;
            return true;
        }
        false
    }

    fn at_op(&self, ch: u8) -> bool {
        if self.opts.ere {
            return self.peek() == Some(ch);
        }
        self.peek() == Some(b'\\') && self.peek_at(1) == Some(ch)
    }

    fn parse_alt(&mut self, depth: usize) -> Result<Node, Error> {
        // Groups nest by recursion here and in every AST walk below, so a pattern
        // of 20k `(` would exhaust the stack — which aborts, and an abort is not
        // an error a caller can report. Cap it far above any real pattern.
        if depth > MAX_NESTING {
            return Err(Error::new("regular expression is too complex"));
        }
        let mut branches = vec![self.parse_concat(depth)?];
        while self.eat_op(b'|') {
            branches.push(self.parse_concat(depth)?);
        }
        if branches.len() == 1 {
            return branches.pop().ok_or_else(|| Error::new("empty alternation"));
        }
        Ok(Node::Alt(branches))
    }

    fn parse_concat(&mut self, depth: usize) -> Result<Node, Error> {
        let mut items: Vec<Node> = Vec::new();
        // In BRE a repetition operator has nothing to repeat at the start of a
        // branch, so it is a LITERAL there — and a leading `^` anchor does not
        // give it one: `^*` matches a line starting with `*`, not every line.
        let mut first = true;
        // Whether a `^` HERE is still the anchor. Separate from `first`, which outlives
        // it: after a leading `^` a repetition operator is still literal, but a SECOND
        // `^` is not a second anchor — BRE `^^` selects lines starting with `^`, and
        // conflating the two made it match every line.
        let mut bol_ok = true;
        loop {
            if self.peek().is_none() || self.at_op(b'|') || (depth > 0 && self.at_op(b')')) {
                break;
            }
            // An ERE `)` with no open group is a literal in GNU's grammar only at
            // depth 0; POSIX leaves it undefined. Treat it as literal, matching GNU.
            if items.len() >= MAX_CONCAT {
                return Err(Error::new("regular expression is too complex"));
            }
            let atom = self.parse_atom(depth, first, bol_ok)?;
            let anchor = !self.opts.ere && first && matches!(atom, Node::Bol);
            let atom = if anchor { atom } else { self.parse_repeats(atom)? };
            first = anchor;
            bol_ok = false;
            items.push(atom);
        }
        match items.len() {
            0 => Ok(Node::Empty),
            1 => items.pop().ok_or_else(|| Error::new("empty branch")),
            _ => Ok(Node::Concat(items)),
        }
    }

    /// Apply every postfix repetition operator that follows an atom (`a**` and
    /// `a*\{2\}` are legal stacked operators in GNU's grammar).
    fn parse_repeats(&mut self, mut atom: Node) -> Result<Node, Error> {
        loop {
            if self.eat(b'*') {
                atom = Node::Repeat { node: Box::new(atom), min: 0, max: None };
                continue;
            }
            if self.opts.ere && self.peek() == Some(b'+') {
                self.pos += 1;
                atom = Node::Repeat { node: Box::new(atom), min: 1, max: None };
                continue;
            }
            if self.opts.ere && self.peek() == Some(b'?') {
                self.pos += 1;
                atom = Node::Repeat { node: Box::new(atom), min: 0, max: Some(1) };
                continue;
            }
            // GNU BRE: `\+` and `\?` are the one-or-more / optional operators.
            if !self.opts.ere && self.peek() == Some(b'\\') {
                match self.peek_at(1) {
                    Some(b'+') => {
                        self.pos += 2;
                        atom = Node::Repeat { node: Box::new(atom), min: 1, max: None };
                        continue;
                    }
                    Some(b'?') => {
                        self.pos += 2;
                        atom = Node::Repeat { node: Box::new(atom), min: 0, max: Some(1) };
                        continue;
                    }
                    _ => {}
                }
            }
            if let Some(node) = self.parse_interval(&atom)? {
                atom = node;
                continue;
            }
            return Ok(atom);
        }
    }

    /// `{n}`, `{n,}`, `{n,m}` (ERE) / `\{…\}` (BRE). Returns `None` when what
    /// follows is not an interval at all, which in ERE means a literal `{`.
    fn parse_interval(&mut self, atom: &Node) -> Result<Option<Node>, Error> {
        let save = self.pos;
        if !self.eat_op(b'{') {
            return Ok(None);
        }
        // `{}` has no interval reading in either dialect: GNU rejects it (the
        // grep corpus asserts status 2 for ERE `a{}`), unlike `{1a}` or `a{b`,
        // which fall back to a literal brace.
        if self.at_op(b'}') {
            return Err(Error::new("Invalid content of \\{\\}"));
        }
        // GNU reads an omitted lower bound as 0 (`a\{,2\}` is `a\{0,2\}`); the
        // grep corpus asserts that BRE spelling matches rather than erroring.
        let min = match self.parse_number() {
            Some(n) => n,
            None if self.peek() == Some(b',') => 0,
            None => {
                // A `{` that opens no valid interval is a literal in ERE, but in
                // BRE `\{` must be one.
                if self.opts.ere {
                    self.pos = save;
                    return Ok(None);
                }
                return Err(Error::new("Invalid content of \\{\\}"));
            }
        };
        let max = if self.eat(b',') {
            self.parse_number()
        } else {
            Some(min)
        };
        if !self.eat_op(b'}') {
            if self.opts.ere {
                self.pos = save;
                return Ok(None);
            }
            return Err(Error::new("Unmatched \\{"));
        }
        if let Some(m) = max {
            if m < min {
                return Err(Error::new("Invalid content of \\{\\}"));
            }
        }
        if min > RE_DUP_MAX || max.is_some_and(|m| m > RE_DUP_MAX) {
            return Err(Error::new("Regular expression too big"));
        }
        if matches!(atom, Node::Bol | Node::Eol | Node::BufStart | Node::BufEnd) {
            // `^\{1\}` — GNU rejects a repeated anchor in BRE.
            return Err(Error::new("Invalid preceding regular expression"));
        }
        Ok(Some(Node::Repeat { node: Box::new(atom.clone()), min, max }))
    }

    fn parse_number(&mut self) -> Option<u32> {
        let start = self.pos;
        let mut n: u32 = 0;
        while let Some(b) = self.peek() {
            if !b.is_ascii_digit() {
                break;
            }
            n = n.saturating_mul(10).saturating_add(u32::from(b - b'0'));
            self.pos += 1;
        }
        if self.pos == start {
            return None;
        }
        Some(n)
    }

    /// One atom. `first` says nothing precedes the atom in its branch, which is what
    /// makes a leading `*` a literal; `bol_ok` says a `^` here is still that branch's
    /// leading anchor. They differ after a `^`: it consumes the anchor but still leaves
    /// a following repetition operator nothing to repeat.
    fn parse_atom(&mut self, depth: usize, first: bool, bol_ok: bool) -> Result<Node, Error> {
        let b = self.bump().ok_or_else(|| Error::new("unexpected end of pattern"))?;
        match b {
            // sed's `M` flag is POSIX REG_NEWLINE, which is two rules, not one: `^`/`$`
            // gain the embedded separators (see `State::at_bol`) AND `.` loses them.
            // GNU reads `s/c.d/Z/M` over `abc\ndef` as no match at all.
            b'.' => Ok(match self.opts.reg_newline.is_some() {
                true => Node::Class(all_but_newline()),
                false => Node::Any,
            }),
            b'[' => self.parse_bracket(),
            b'^' => {
                if self.opts.ere || bol_ok {
                    Ok(Node::Bol)
                } else {
                    Ok(self.literal(b'^'))
                }
            }
            b'$' => {
                // BRE: `$` anchors only at the end of the pattern or of a branch.
                let anchors = self.opts.ere
                    || self.peek().is_none()
                    || self.at_op(b'|')
                    || (depth > 0 && self.at_op(b')'));
                if anchors {
                    Ok(Node::Eol)
                } else {
                    Ok(self.literal(b'$'))
                }
            }
            b'*' if first => Ok(self.literal(b'*')), // leading `*` is literal
            b'(' if self.opts.ere => {
                self.ngroups += 1;
                let idx = self.ngroups;
                let inner = self.parse_alt(depth + 1)?;
                if !self.eat(b')') {
                    return Err(Error::new("Unmatched ( or \\("));
                }
                Ok(Node::Group(idx, Box::new(inner)))
            }
            b')' if self.opts.ere && depth == 0 => Ok(self.literal(b')')),
            b'+' | b'?' if self.opts.ere => Err(Error::new("Invalid preceding regular expression")),
            b'\\' => self.parse_escape(depth),
            _ => Ok(self.literal(b)),
        }
    }

    fn literal(&self, b: u8) -> Node {
        if self.opts.icase && b.is_ascii_alphabetic() {
            let mut set = ByteSet::empty();
            set.insert(b);
            set.fold_case();
            return Node::Class(set);
        }
        Node::Byte(b)
    }

    fn parse_escape(&mut self, depth: usize) -> Result<Node, Error> {
        let b = self
            .bump()
            .ok_or_else(|| Error::new("Trailing backslash"))?;
        match b {
            b'(' if !self.opts.ere => {
                self.ngroups += 1;
                let idx = self.ngroups;
                let inner = self.parse_alt(depth + 1)?;
                if !self.eat_op(b')') {
                    return Err(Error::new("Unmatched ( or \\("));
                }
                Ok(Node::Group(idx, Box::new(inner)))
            }
            b')' if !self.opts.ere => Err(Error::new("Unmatched ) or \\)")),
            b'{' if !self.opts.ere => Err(Error::new("Invalid preceding regular expression")),
            b'}' if !self.opts.ere => Err(Error::new("Unmatched \\{")),
            b'1'..=b'9' => {
                let n = usize::from(b - b'0');
                if n > self.ngroups {
                    return Err(Error::new("Invalid back reference"));
                }
                Ok(Node::Backref(n))
            }
            b'w' | b'W' => {
                let mut set = ByteSet::empty();
                for c in 0..=255u8 {
                    if is_word(c) {
                        set.insert(c);
                    }
                }
                if b == b'W' {
                    set.negate();
                }
                Ok(Node::Class(set))
            }
            b's' | b'S' => {
                let mut set = ByteSet::empty();
                for c in [b' ', b'\t', b'\n', b'\r', 0x0b, 0x0c] {
                    set.insert(c);
                }
                if b == b'S' {
                    set.negate();
                }
                Ok(Node::Class(set))
            }
            b'b' => Ok(Node::WordBoundary(true)),
            b'B' => Ok(Node::WordBoundary(false)),
            b'<' => Ok(Node::WordEdge(true)),
            b'>' => Ok(Node::WordEdge(false)),
            b'`' => Ok(Node::BufStart),
            b'\'' => Ok(Node::BufEnd),
            _ => {
                if self.opts.escapes {
                    if let Some(c) = self.escape_byte(b) {
                        return Ok(self.literal(c));
                    }
                }
                Ok(self.literal(b))
            }
        }
    }

    /// The GNU sed escape vocabulary for a single byte, consuming any operand.
    fn escape_byte(&mut self, b: u8) -> Option<u8> {
        match b {
            b'n' => Some(b'\n'),
            b't' => Some(b'\t'),
            b'r' => Some(b'\r'),
            b'f' => Some(0x0c),
            b'v' => Some(0x0b),
            b'a' => Some(0x07),
            b'e' => Some(0x1b),
            b'c' => {
                let c = self.bump()?;
                Some(c.to_ascii_uppercase() ^ 0x40)
            }
            b'd' => self.radix_operand(10, 3),
            b'o' => self.radix_operand(8, 3),
            b'x' => self.radix_operand(16, 2),
            _ => None,
        }
    }

    fn radix_operand(&mut self, radix: u32, max_digits: usize) -> Option<u8> {
        let mut value: u32 = 0;
        let mut digits = 0;
        while digits < max_digits {
            let Some(d) = self.peek().and_then(|c| char::from(c).to_digit(radix)) else {
                break;
            };
            value = value * radix + d;
            self.pos += 1;
            digits += 1;
        }
        if digits == 0 {
            return None;
        }
        u8::try_from(value & 0xff).ok()
    }

    /// A bracket expression: ranges, negation, `[:class:]`, `[.c.]`, `[=c=]`.
    fn parse_bracket(&mut self) -> Result<Node, Error> {
        let mut set = ByteSet::empty();
        let negated = self.eat(b'^');
        let mut first = true;
        loop {
            let Some(b) = self.bump() else {
                return Err(Error::new("Unmatched [, [^, [:, [., or [="));
            };
            if b == b']' && !first {
                break;
            }
            first = false;
            if b == b'[' {
                if let Some(kind @ (b':' | b'.' | b'=')) = self.peek() {
                    self.pos += 1;
                    let name = self.bracket_name(kind)?;
                    match kind {
                        b':' => self.add_named_class(&mut set, &name)?,
                        // A collating element / equivalence class of one byte is
                        // that byte in the C locale; anything longer has no
                        // C-locale meaning.
                        _ => match name.as_slice() {
                            [one] => set.insert(*one),
                            _ => return Err(Error::new("Invalid collation character")),
                        },
                    }
                    continue;
                }
            }
            // A range, unless `-` is the last character before `]`.
            if self.peek() == Some(b'-') && self.peek_at(1).is_some_and(|c| c != b']') {
                self.pos += 1;
                let hi = self.bump().ok_or_else(|| Error::new("Unmatched [, [^, [:, [., or [="))?;
                if hi < b {
                    return Err(Error::new("Invalid range end"));
                }
                set.insert_range(b, hi);
                continue;
            }
            set.insert(b);
        }
        // Fold BEFORE negating: `[^a]` under -i must reject `a` AND `A`. Folding
        // the negated set would re-add both cases and make it match everything.
        if self.opts.icase {
            set.fold_case();
        }
        if negated {
            set.negate();
            // The other half of REG_NEWLINE: a NON-MATCHING list does not match a
            // newline either, so `s/[^abc]/Y/M` over `abc\ndef` reaches the `d`.
            // Only `.` and this form lose it -- `\W` and an explicit `[\n]` still
            // match one, which is why the rule lives here and not in `negate`.
            if self.opts.reg_newline.is_some() {
                set.remove(b'\n');
            }
        }
        Ok(Node::Class(set))
    }

    /// Contents of `[:name:]` / `[.name.]` / `[=name=]`, positioned after the
    /// opener and leaving `pos` after the closer.
    fn bracket_name(&mut self, kind: u8) -> Result<Vec<u8>, Error> {
        let mut name = Vec::new();
        loop {
            let Some(b) = self.bump() else {
                return Err(Error::new("Unmatched [, [^, [:, [., or [="));
            };
            if b == kind && self.peek() == Some(b']') {
                self.pos += 1;
                return Ok(name);
            }
            name.push(b);
        }
    }

    fn add_named_class(&mut self, set: &mut ByteSet, name: &[u8]) -> Result<(), Error> {
        let mut add = |f: fn(u8) -> bool| {
            for c in 0..=255u8 {
                if f(c) {
                    set.insert(c);
                }
            }
        };
        match name {
            b"alpha" => add(|c| c.is_ascii_alphabetic()),
            b"digit" => add(|c| c.is_ascii_digit()),
            b"alnum" => add(|c| c.is_ascii_alphanumeric()),
            b"upper" => add(|c| c.is_ascii_uppercase()),
            b"lower" => add(|c| c.is_ascii_lowercase()),
            b"space" => add(|c| c == b' ' || (0x09..=0x0d).contains(&c)),
            b"blank" => add(|c| c == b' ' || c == b'\t'),
            b"punct" => add(|c| c.is_ascii_punctuation()),
            b"print" => add(|c| (0x20..=0x7e).contains(&c)),
            b"graph" => add(|c| (0x21..=0x7e).contains(&c)),
            b"cntrl" => add(|c| c < 0x20 || c == 0x7f),
            b"xdigit" => add(|c| c.is_ascii_hexdigit()),
            _ => return Err(Error::new("Invalid character class")),
        }
        Ok(())
    }
}

/// POSIX's interval ceiling; GNU rejects a bound above it.
const RE_DUP_MAX: u32 = 32767;

/// How deeply groups may nest. Bounds the parser's recursion and every AST walk
/// that follows it. Sized against GNU grep 3.11, which handles 5k nested groups
/// and reports its own `stack overflow` past that; td-txt refuses one step
/// earlier with a diagnosed error instead of aborting.
const MAX_NESTING: usize = 5_000;

/// How many iterations a repetition whose body is NOT a single byte may take.
/// Such a body recurses once per iteration; the cap turns what would be a stack
/// overflow (an abort, which no caller can report) into the same `too complex`
/// error a step-budget overrun gives. A single-byte body — `a*`, `.*`, `[0-9]\+`,
/// which is nearly every real pattern — runs iteratively and is NOT capped.
pub const MAX_REPEAT_DEPTH: u32 = 20_000;

/// Stack to reserve per permitted iteration. Frame cost depends on CODEGEN, not
/// just on the code: measured ~0.5 KiB/iteration optimized but ~4 KiB
/// unoptimized (a 64 MiB stack aborts near 16k in a debug build). 8 KiB is 2x
/// the worst measured. `main.rs` asserts the applet stack covers
/// `MAX_REPEAT_DEPTH * this` at COMPILE time, and `tests/conformance.rs` runs a
/// repetition one short of the cap through the built binary, so neither the
/// arithmetic nor the reality can drift.
pub const REPEAT_FRAME_BUDGET: usize = 8 << 10;

/// How many atoms one concatenation may hold. `m_seq` recurses per element, so
/// pattern LENGTH is a stack axis too — a pattern of 400k `.` aborted before
/// this cap (measured ~875 B/frame unoptimized, so ~300k fits the applet
/// stack). 100k leaves a 3x margin and is far past any real pattern.
const MAX_CONCAT: usize = 100_000;

/// Steps one SEARCH may take before the pattern is declared too complex.
/// Backtracking is exponential in the worst case, and a shared `grep` must not
/// be wedgeable by one line of input. The budget spans every start position the
/// scan tries, not each one: a per-position budget would still let a long line
/// multiply the worst case by its length.
const STEP_BUDGET: u64 = 40_000_000;

impl Regex {
    pub fn compile(pattern: &[u8], opts: Options) -> Result<Self, Error> {
        let mut p = Parser { pat: pattern, pos: 0, opts, ngroups: 0 };
        let root = p.parse_alt(0)?;
        if p.pos < pattern.len() {
            // Only an unbalanced `)` can stop the top-level parse early.
            return Err(Error::new("Unmatched ) or \\)"));
        }
        let first = first_bytes(&root);
        let has_alt = has_alt(&root);
        let has_backref = has_backref(&root);
        Ok(Self {
            root,
            ngroups: p.ngroups,
            icase: opts.icase,
            reg_newline: opts.reg_newline,
            segment: opts.reg_newline.filter(|sep| *sep != b'\n'),
            first,
            has_alt,
            has_backref,
        })
    }

    /// Leftmost match at or after `from`, or `None`. `Err` means the step budget
    /// was exhausted.
    pub fn search(&self, hay: &[u8], from: usize) -> Result<Option<Captures>, Error> {
        self.scan(hay, from, None, OnBudget::Fail, false)
    }

    /// As `search`, but for a SUBSTITUTION, which GNU matches differently from an
    /// address under `M` in two ways.
    ///
    /// It works within the segments between record separators, where an address
    /// matches the whole pattern space: `printf 'a\0b\0' | sed -z -n -e N -e
    /// '/a.b/Mp'` matches and `s/a.b/X/Mp` over the same space does not. And the
    /// buffer anchors ``\` ``/`\'` follow `M`'s separator in an ADDRESS, where they
    /// are indistinguishable from `^`/`$`, but in a substitution only a segment moves
    /// them -- `s/\`a/X/Mg` over `a\na` rewrites only the first `a` while
    /// `/\`a/M` matches at both. So confinement and the anchor policy are properties
    /// of the CALL, not of the compiled pattern.
    pub fn search_subst(&self, hay: &[u8], from: usize) -> Result<Option<Captures>, Error> {
        self.scan(hay, from, None, OnBudget::Fail, true)
    }

    /// As `search`, but a budget exhausted with a match in hand answers with that
    /// match instead of failing. Only for callers that ask WHETHER `hay` matches —
    /// the span may not be the longest. See `OnBudget`.
    pub fn search_existence(&self, hay: &[u8], from: usize) -> Result<Option<Captures>, Error> {
        self.scan(hay, from, None, OnBudget::Existence, false)
    }

    /// Leftmost-then-longest match at or after `from` whose span satisfies `filter`.
    ///
    /// `grep -w` needs the test INSIDE the scan. GNU retries a SHORTER match at the
    /// SAME start before advancing, so filtering `search`'s result cannot express it:
    /// for `\.*` the greedy span at the start of `..a` is `..`, which is not
    /// word-bounded, and the span GNU selects is the shorter `.`. On `.a` it is the
    /// EMPTY span. `match_from` explores every end anyway, so `filter.span` only has
    /// to narrow which of them may win.
    pub fn search_filtered(
        &self,
        hay: &[u8],
        from: usize,
        filter: &Filter<'_>,
        on_budget: OnBudget,
    ) -> Result<Option<Captures>, Error> {
        self.scan(hay, from, Some(filter), on_budget, false)
    }

    /// `scan_once`, with the PREVIOUS release's algorithm as a floor beneath it.
    ///
    /// Searching every end is what leftmost-longest costs, and on a long line with
    /// three or more sliding repeats it can exhaust the budget where stopping at the
    /// first end would have answered — `sed 's/\(.*\)=\(.*\);.*END/[\2]/'` over a
    /// 1200-byte line did. Refusing that is a REGRESSION, so a `Fail` caller retries
    /// with first-end semantics rather than failing.
    ///
    /// Except when the pattern has an alternation, where the first end can be
    /// arbitrarily shorter than the longest — `x\|x\(a\|aa\)*b` matches one byte on
    /// its first branch and the whole line on its second, so first-end would hand sed
    /// a one-byte span to rewrite. That is exactly the case the previous release also
    /// refused, because its own shortcut was conditional on this same flag. So the
    /// floor is "whatever the previous release answered", never a new wrong span.
    fn scan(
        &self,
        hay: &[u8],
        from: usize,
        filter: Option<&Filter<'_>>,
        on_budget: OnBudget,
        subst: bool,
    ) -> Result<Option<Captures>, Error> {
        match self.scan_once(hay, from, filter, on_budget, subst) {
            Err(e) => match on_budget == OnBudget::Fail && !self.has_alt {
                // One retry, so the worst case stays a bounded multiple of the budget.
                true => self.scan_once(hay, from, filter, OnBudget::Existence, subst),
                false => Err(e),
            },
            found => found,
        }
    }

    /// Advance the start position until `match_from` reports a match. One step
    /// budget spans the whole scan, so a long line cannot multiply the worst case
    /// by its length.
    fn scan_once(
        &self,
        hay: &[u8],
        from: usize,
        filter: Option<&Filter<'_>>,
        on_budget: OnBudget,
        subst: bool,
    ) -> Result<Option<Captures>, Error> {
        let mut steps = 0u64;
        let mut at = from;
        loop {
            if at > hay.len() {
                return Ok(None);
            }
            // The first-byte skip is sound even under a filter: `first_bytes` yields
            // None whenever the pattern can match empty, so a start whose empty span
            // might be acceptable is never skipped.
            let mut skippable = self
                .first
                .as_ref()
                .is_some_and(|set| hay.get(at).is_some_and(|b| !set.contains(*b)));
            // A start no end can satisfy is skipped WITHOUT matching. This is what
            // keeps `-w` affordable now that it explores every end: on a line of
            // words, most starts sit inside one, and exploring them can only ever
            // rediscover spans the word test then throws away.
            skippable = skippable || filter.is_some_and(|f| !(f.start)(at));
            if !skippable {
                if let Some(caps) = self.match_from(hay, at, &mut steps, filter, on_budget, subst)? {
                    return Ok(Some(caps));
                }
            }
            at += 1;
        }
    }

    /// How many capturing groups the pattern has — what a `\N` backreference in
    /// a sed replacement may name.
    pub fn group_count(&self) -> usize {
        self.ngroups
    }

    pub fn is_match(&self, hay: &[u8]) -> Result<bool, Error> {
        Ok(self.search_existence(hay, 0)?.is_some())
    }

    /// Does some match cover `hay` exactly? Used by `grep -x`, which cannot be
    /// expressed by wrapping the pattern in `^\(…\)$` without renumbering the
    /// backreferences. Explores the whole space, so a match reaching the end is
    /// seen even when a greedier one does not.
    pub fn matches_whole(&self, hay: &[u8]) -> Result<bool, Error> {
        let mut steps = 0u64;
        match self.match_from(hay, 0, &mut steps, None, OnBudget::Fail, false)? {
            Some(caps) => Ok(caps.end() == hay.len()),
            None => Ok(false),
        }
    }

    /// The LONGEST match anchored at exactly `at`. Every end is explored, because
    /// POSIX is leftmost-longest and the greedy path is not always the longest one:
    /// a bounded repeat reaches its higher count only if an EARLIER greedy repeat
    /// gives ground, so `.A*\(\^\?.\W\)\{1,3\}` covers two more bytes of `AA[  `
    /// when `A*` takes none. This was once conditional on the pattern containing an
    /// alternation, which is why that case came back short.
    ///
    /// Exploring every end is what a backtracker pays for leftmost-longest, so three
    /// things bound it: `k` stops at an end reaching the last byte, `OnBudget::
    /// Existence` stops at the FIRST acceptable end because its caller needs a boolean
    /// and not a span, and an exhausted budget yields the best end so far to that same
    /// caller rather than an error. Running out with nothing found is always fatal.
    ///
    /// `steps` is the caller's running budget, so a whole scan is bounded rather
    /// than each start position separately.
    /// `filter`, when given, narrows candidate ends as they are reported — see
    /// `search_filtered` for why the span test cannot be applied afterwards.
    fn match_from(
        &self,
        hay: &[u8],
        at: usize,
        steps: &mut u64,
        filter: Option<&Filter<'_>>,
        on_budget: OnBudget,
        subst: bool,
    ) -> Result<Option<Captures>, Error> {
        let mut st = State {
            hay,
            icase: self.icase,
            reg_newline: self.reg_newline,
            segment: self.segment,
            subst,
            has_backref: self.has_backref,
            caps: vec![None; self.ngroups + 1],
            best: None,
            steps: *steps,
        };
        let root = &self.root;
        let matched = m(&mut st, root, at, &mut |st, end| {
            let allowed = filter.is_none_or(|f| (f.span)(at, end));
            let better = allowed && st.best.as_ref().is_none_or(|(_, e)| end > *e);
            if better {
                let mut spans = st.caps.clone();
                if let Some(slot) = spans.first_mut() {
                    *slot = Some((at, end));
                }
                st.best = Some((spans, end));
            }
            // POSIX wants the LONGEST end, so keep exploring — with two exceptions.
            // An end at the last byte cannot be beaten, which is what keeps a trailing
            // greedy repeat linear: `.*=.*=.*` on a 4000-byte line reaches the end on
            // its first success, and without the stop it re-partitions the line and
            // exhausts the budget. And a caller asking only WHETHER `hay` matches has
            // its answer from any end at all, so it pays for no exploration — the
            // filtered scan still reaches a SHORTER acceptable end first, because
            // `allowed` is tested as each end is reported, not afterwards.
            allowed && (on_budget == OnBudget::Existence || end == st.hay.len())
        });
        *steps = st.steps;
        if st.steps >= STEP_BUDGET && (st.best.is_none() || on_budget == OnBudget::Fail) {
            // Out of budget with nothing found: "no match" cannot be claimed without
            // having looked, so fail closed. With a match in hand it depends on what
            // the caller does with the span — see `OnBudget`.
            return Err(Error::new("regular expression is too complex"));
        }
        if !matched && st.best.is_none() {
            return Ok(None);
        }
        Ok(st.best.take().map(|(spans, _)| Captures { spans }))
    }
}

/// Every byte but the newline — what `.` means under REG_NEWLINE.
fn all_but_newline() -> ByteSet {
    let mut set = ByteSet::empty();
    set.negate();
    set.remove(b'\n');
    set
}

/// Does the pattern contain an alternation anywhere?
fn has_alt(node: &Node) -> bool {
    match node {
        Node::Alt(_) => true,
        Node::Group(_, inner) | Node::Repeat { node: inner, .. } => has_alt(inner),
        Node::Concat(items) => items.iter().any(has_alt),
        _ => false,
    }
}

/// Does the pattern contain a backreference anywhere? See `Regex::has_backref`.
fn has_backref(node: &Node) -> bool {
    match node {
        Node::Backref(_) => true,
        Node::Group(_, inner) | Node::Repeat { node: inner, .. } => has_backref(inner),
        Node::Concat(items) => items.iter().any(has_backref),
        Node::Alt(branches) => branches.iter().any(has_backref),
        _ => false,
    }
}

/// The set of bytes a match can start with, when every path is known to consume
/// one. `None` when the pattern can match empty or starts with an assertion.
fn first_bytes(node: &Node) -> Option<ByteSet> {
    match node {
        Node::Byte(b) => {
            let mut set = ByteSet::empty();
            set.insert(*b);
            Some(set)
        }
        Node::Class(set) => Some(*set),
        Node::Group(_, inner) => first_bytes(inner),
        Node::Concat(items) => {
            let head = items.first()?;
            first_bytes(head)
        }
        Node::Alt(branches) => {
            let mut acc = ByteSet::empty();
            for b in branches {
                let set = first_bytes(b)?;
                for w in 0..4 {
                    if let (Some(dst), Some(src)) = (acc.bits.get_mut(w), set.bits.get(w)) {
                        *dst |= *src;
                    }
                }
            }
            Some(acc)
        }
        Node::Repeat { node, min, .. } if *min >= 1 => first_bytes(node),
        _ => None,
    }
}

// ---- matching ------------------------------------------------------------

struct State<'a> {
    hay: &'a [u8],
    icase: bool,
    reg_newline: Option<u8>,
    /// Compile-time: the record separator, when it is not a newline. This is the
    /// half of `M` libc cannot express, so GNU splits on it itself.
    segment: Option<u8>,
    /// Per CALL: is this a SUBSTITUTION? It is confined to one segment, and its
    /// buffer anchors follow only a segment; an address sees the whole pattern space
    /// and its buffer anchors follow the separator. See `Regex::search_subst`.
    subst: bool,
    /// See `Regex::has_backref`: it confines an address as a substitution already is.
    has_backref: bool,
    caps: Spans,
    /// The longest match seen so far and where it ended.
    best: Option<(Spans, usize)>,
    steps: u64,
}

impl State<'_> {
    fn byte(&self, pos: usize) -> Option<u8> {
        self.hay.get(pos).copied()
    }

    /// The byte at `pos`, if THIS CALL may consume it. Every byte-consuming step goes
    /// through here, so nothing in a substitution crosses a separator — including the
    /// greedy-repeat fast path and a backreference, which is where it leaked twice.
    fn consumable(&self, pos: usize) -> Option<u8> {
        match self.byte(pos) {
            Some(b) if (self.subst || self.has_backref) && Some(b) == self.segment => None,
            got => got,
        }
    }

    fn at_bol(&self, pos: usize) -> bool {
        pos == 0
            || self
                .reg_newline
                .is_some_and(|sep| self.byte(pos.wrapping_sub(1)) == Some(sep))
    }

    fn at_eol(&self, pos: usize) -> bool {
        pos == self.hay.len() || self.reg_newline.is_some_and(|sep| self.byte(pos) == Some(sep))
    }

    /// Which separator moves ``\` `` and `\'`, which is not the same for both callers.
    /// In an ADDRESS they are indistinguishable from `^`/`$` and move to the record
    /// separator with them. In a SUBSTITUTION only a segment moves them: `sed -n
    /// 'N;s/\`a/X/Mg'` over `a\na` rewrites only the first `a`, where `/\`a/M`
    /// matches at both, so REG_NEWLINE alone leaves them at the buffer's real ends.
    fn buf_anchor(&self) -> Option<u8> {
        match self.subst {
            true => self.segment,
            false => self.reg_newline,
        }
    }

    fn at_buf_start(&self, pos: usize) -> bool {
        pos == 0
            || self.buf_anchor().is_some_and(|sep| self.byte(pos.wrapping_sub(1)) == Some(sep))
    }

    fn at_buf_end(&self, pos: usize) -> bool {
        pos == self.hay.len() || self.buf_anchor().is_some_and(|sep| self.byte(pos) == Some(sep))
    }

    fn word_at(&self, pos: usize) -> bool {
        self.byte(pos).is_some_and(is_word)
    }

    fn word_before(&self, pos: usize) -> bool {
        pos > 0 && self.byte(pos - 1).is_some_and(is_word)
    }

    fn eq(&self, a: u8, b: u8) -> bool {
        if self.icase {
            return lower(a) == lower(b);
        }
        a == b
    }
}

/// Continuation-passing backtracker. `k` reports a candidate end position and returns
/// `true` to stop the search or `false` to keep exploring; the return value propagates
/// that "stop" signal. `match_from`'s `k` keeps exploring until an end reaches the last
/// byte, which no later end can beat.
fn m(st: &mut State, node: &Node, pos: usize, k: &mut dyn FnMut(&mut State, usize) -> bool) -> bool {
    st.steps += 1;
    if st.steps >= STEP_BUDGET {
        return true; // unwind; the caller turns an exhausted budget into an error
    }
    match node {
        Node::Empty => k(st, pos),
        Node::Byte(b) => match st.consumable(pos) {
            Some(got) if st.eq(got, *b) => k(st, pos + 1),
            _ => false,
        },
        Node::Any => match st.consumable(pos) {
            Some(_) => k(st, pos + 1),
            None => false,
        },
        Node::Class(set) => match st.consumable(pos) {
            Some(got) if set.contains(got) => k(st, pos + 1),
            _ => false,
        },
        Node::Bol => {
            if st.at_bol(pos) {
                k(st, pos)
            } else {
                false
            }
        }
        Node::Eol => {
            if st.at_eol(pos) {
                k(st, pos)
            } else {
                false
            }
        }
        Node::BufStart => {
            if st.at_buf_start(pos) {
                k(st, pos)
            } else {
                false
            }
        }
        Node::BufEnd => {
            if st.at_buf_end(pos) {
                k(st, pos)
            } else {
                false
            }
        }
        Node::WordBoundary(want) => {
            let boundary = st.word_before(pos) != st.word_at(pos);
            if boundary == *want {
                k(st, pos)
            } else {
                false
            }
        }
        Node::WordEdge(start) => {
            let ok = if *start {
                !st.word_before(pos) && st.word_at(pos)
            } else {
                st.word_before(pos) && !st.word_at(pos)
            };
            if ok {
                k(st, pos)
            } else {
                false
            }
        }
        Node::Group(idx, inner) => {
            let saved = st.caps.get(*idx).copied().flatten();
            let idx = *idx;
            let stop = m(st, inner, pos, &mut |st, end| {
                let prev = st.caps.get(idx).copied().flatten();
                if let Some(slot) = st.caps.get_mut(idx) {
                    *slot = Some((pos, end));
                }
                let stop = k(st, end);
                if !stop {
                    if let Some(slot) = st.caps.get_mut(idx) {
                        *slot = prev;
                    }
                }
                stop
            });
            if !stop {
                if let Some(slot) = st.caps.get_mut(idx) {
                    *slot = saved;
                }
            }
            stop
        }
        Node::Backref(n) => {
            let Some((s, e)) = st.caps.get(*n).copied().flatten() else {
                // An unset group matches the empty string, as glibc does.
                return k(st, pos);
            };
            let len = e.saturating_sub(s);
            if pos + len > st.hay.len() {
                return false;
            }
            // Charge each byte ACTUALLY compared. A backreference to a long
            // group costs O(len) per attempt, so without accounting a pattern
            // like `^\(.*\)\1$` spends quadratic time while the step budget
            // barely moves. Charging inside the loop leaves the common case —
            // a mismatch on the first byte — as cheap as it was.
            for i in 0..len {
                st.steps = st.steps.saturating_add(1);
                if st.steps >= STEP_BUDGET {
                    return false;
                }
                let (Some(a), Some(b)) = (st.byte(s + i), st.consumable(pos + i)) else {
                    return false;
                };
                if !st.eq(a, b) {
                    return false;
                }
            }
            k(st, pos + len)
        }
        Node::Concat(items) => m_seq(st, items, pos, k),
        Node::Alt(branches) => {
            for b in branches {
                if m(st, b, pos, k) {
                    return true;
                }
            }
            false
        }
        Node::Repeat { node, min, max } => m_repeat(st, node, *min, *max, 0, pos, k),
    }
}

fn m_seq(
    st: &mut State,
    items: &[Node],
    pos: usize,
    k: &mut dyn FnMut(&mut State, usize) -> bool,
) -> bool {
    match items.split_first() {
        None => k(st, pos),
        Some((head, rest)) => m(st, head, pos, &mut |st, next| m_seq(st, rest, next, k)),
    }
}

/// A repetition body that consumes exactly one byte and needs no backtracking of
/// its own. `a*`, `.*` and `[0-9]\+` are all of this shape, and recognizing it is
/// what lets the common case run as a LOOP instead of one stack frame per
/// matched byte — `grep 'a*'` over a 200 KB line would otherwise overflow.
fn single_byte_body(node: &Node) -> Option<&Node> {
    match node {
        Node::Byte(_) | Node::Any | Node::Class(_) => Some(node),
        _ => None,
    }
}

fn single_byte_matches(st: &State, node: &Node, b: u8) -> bool {
    match node {
        Node::Byte(want) => st.eq(b, *want),
        Node::Any => true,
        Node::Class(set) => set.contains(b),
        _ => false,
    }
}

/// Greedy repetition of a single-byte body: consume as many as the input and
/// `max` allow, then hand the continuation ever-shorter ends down to `min`.
fn m_repeat_flat(
    st: &mut State,
    node: &Node,
    min: u32,
    max: Option<u32>,
    pos: usize,
    k: &mut dyn FnMut(&mut State, usize) -> bool,
) -> bool {
    let mut taken: u32 = 0;
    let mut end = pos;
    while max.is_none_or(|m| taken < m) {
        let Some(b) = st.consumable(end) else { break };
        if !single_byte_matches(st, node, b) {
            break;
        }
        end += 1;
        taken = taken.saturating_add(1);
        st.steps += 1;
        if st.steps >= STEP_BUDGET {
            return true;
        }
    }
    loop {
        if taken < min {
            return false;
        }
        if k(st, end) {
            return true;
        }
        if taken == min || end == pos {
            return false;
        }
        taken -= 1;
        end -= 1;
        st.steps += 1;
        if st.steps >= STEP_BUDGET {
            return true;
        }
    }
}

/// Greedy repetition: take one more iteration before trying the continuation.
/// `count` is how many iterations already matched. An iteration that consumes
/// nothing is not retried, which is what keeps `\(a*\)*` from looping.
fn m_repeat(
    st: &mut State,
    node: &Node,
    min: u32,
    max: Option<u32>,
    count: u32,
    pos: usize,
    k: &mut dyn FnMut(&mut State, usize) -> bool,
) -> bool {
    if count == 0 {
        if let Some(simple) = single_byte_body(node) {
            return m_repeat_flat(st, simple, min, max, pos, k);
        }
    }
    // A body that can consume nothing recurses per iteration; cap the depth so a
    // long subject reports `too complex` instead of overflowing the stack.
    if count >= MAX_REPEAT_DEPTH {
        st.steps = STEP_BUDGET;
        return true;
    }
    let may_more = max.is_none_or(|m| count < m);
    if may_more
        && m(st, node, pos, &mut |st, next| {
            if next == pos {
                return false; // empty iteration: no progress, so stop unrolling
            }
            m_repeat(st, node, min, max, count.saturating_add(1), next, k)
        })
    {
        return true;
    }
    if count >= min {
        return k(st, pos);
    }
    // The minimum is not met yet, but an EMPTY-matching body still satisfies it:
    // `^\(a*\)\{2\}$` matches an empty line because the group matches empty
    // twice. Keep folding empty iterations in until `min` is reached.
    if may_more {
        return m(st, node, pos, &mut |st, next| {
            if next != pos {
                return false;
            }
            m_repeat(st, node, min, max, count.saturating_add(1), next, k)
        });
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bre(pat: &str) -> Regex {
        Regex::compile(pat.as_bytes(), Options::default()).unwrap()
    }

    fn ere(pat: &str) -> Regex {
        Regex::compile(pat.as_bytes(), Options { ere: true, ..Options::default() }).unwrap()
    }

    fn matched(re: &Regex, s: &str) -> bool {
        re.is_match(s.as_bytes()).unwrap()
    }

    #[test]
    fn bre_specials_are_literal_without_backslash() {
        assert!(matched(&bre("a(b"), "a(b"));
        assert!(matched(&bre("a+b"), "a+b"));
        assert!(!matched(&bre("a\\(b\\)c"), "abbc"));
        assert!(matched(&bre("a\\(b\\)c"), "abc"));
    }

    #[test]
    fn ere_alternation_takes_the_longest_match() {
        let re = ere("x|xy");
        let caps = re.search(b"xy", 0).unwrap().unwrap();
        assert_eq!((caps.start(), caps.end()), (0, 2));
    }

    #[test]
    fn backreference_matches_the_captured_text() {
        assert!(matched(&bre("a\\(b*\\)c\\1d"), "abbcbbd"));
        assert!(!matched(&bre("a\\(b*\\)c\\1d"), "abbcbd"));
    }

    #[test]
    fn intervals_bound_repetition() {
        assert!(matched(&bre("a\\{2,3\\}$"), "aaa"));
        assert!(!matched(&ere("^a{2,3}$"), "aaaa"));
        assert!(matched(&ere("^a{2,3}$"), "aa"));
    }

    #[test]
    fn bracket_expressions_handle_classes_and_edge_placement() {
        assert!(matched(&bre("[[:digit:]]"), "x7"));
        assert!(matched(&bre("[]a]"), "]"));
        assert!(matched(&bre("[a-]"), "-"));
        assert!(!matched(&bre("[^a]"), "a"));
    }

    #[test]
    fn anchors_bind_to_the_whole_subject() {
        assert!(matched(&bre("^ab$"), "ab"));
        assert!(!matched(&bre("^ab$"), "xab"));
        // A `$` in the middle of a BRE is a literal.
        assert!(matched(&bre("a$b"), "a$b"));
    }

    /// Only the FIRST `^` of a BRE branch anchors; a second one is a literal caret.
    /// The leading-`*`-is-literal rule outlives the anchor, so `^*` still matches a
    /// star — one flag cannot carry both, and sharing one made `^^` match every line.
    /// POSIX is leftmost-LONGEST, and the greedy path is not always the longest: the
    /// group below reaches its SECOND iteration only if `A*` takes nothing, and the
    /// traversal reports the greedy 4-byte end first. Exploring every end was once
    /// conditional on the pattern alternating, so this came back as (0, 4).
    #[test]
    fn a_bounded_repeat_can_beat_an_earlier_greedy_one() {
        let re = bre(r".A*\(.\W\)\{1,3\}");
        let caps = re.search(b"AA[  ", 0).unwrap().unwrap();
        assert_eq!((caps.start(), caps.end()), (0, 5));
        // Still longest when the greedy path already was.
        let re2 = bre("a*b");
        let c2 = re2.search(b"aaab", 0).unwrap().unwrap();
        assert_eq!((c2.start(), c2.end()), (0, 4));
        // An UNBOUNDED repeat needs the same exploration, which is why the old fast
        // path cannot be recovered by asking whether a repeat is counted: greedy `a*`
        // takes both a's and leaves `\(ab\)*` nothing, so the longest match needs it
        // to give one back.
        let re3 = bre(r"a*\(ab\)*");
        let c3 = re3.search(b"aab", 0).unwrap().unwrap();
        assert_eq!((c3.start(), c3.end()), (0, 3));
    }

    /// Exploring every end is bounded two ways. An end at the last byte cannot be
    /// beaten, so the scan stops there rather than re-partitioning the line — without
    /// which these blew the step budget and reported `too complex`.
    #[test]
    fn a_match_reaching_the_last_byte_stops_the_search() {
        let hay = "key=val ".repeat(80);
        for pat in [r"^.*key.*=.*val.*$", r".*=.*=.*", r".*.*.*"] {
            let re = bre(pat);
            let caps = re
                .search(hay.as_bytes(), 0)
                .unwrap_or_else(|e| panic!("{pat}: {}", e.msg))
                .unwrap_or_else(|| panic!("{pat}: no match"));
            assert_eq!(caps.end(), hay.len(), "{pat} should reach the line end");
        }
    }

    /// The other bound, and it depends on the CALLER. No end reaches the last byte
    /// here, so the search explores until the budget runs out.
    #[test]
    fn an_exhausted_budget_falls_back_rather_than_refusing() {
        let hay = "x:".repeat(2000) + "E" + &"y".repeat(2000);
        let re = bre(r".*:.*:.*E");
        // Asked whether the line matches, that is settled by any end at all.
        let seen = re.search_existence(hay.as_bytes(), 0).unwrap().unwrap();
        assert_eq!(seen.start(), 0);
        // Asked for the SPAN, the longest is unaffordable — but refusing would regress
        // a line the previous release answered, so first-end semantics answer instead.
        // No alternation, so that end cannot be arbitrarily short.
        let span = re.search(hay.as_bytes(), 0).unwrap().unwrap();
        assert_eq!(span.start(), 0);
        assert!(span.end() <= hay.len());
        // A pattern the budget DOES cover answers both ways, identically.
        let easy = bre(r"x:*");
        assert_eq!(
            easy.search(hay.as_bytes(), 0).unwrap().unwrap().end(),
            easy.search_existence(hay.as_bytes(), 0).unwrap().unwrap().end(),
        );
    }

    /// sed's `M` is POSIX REG_NEWLINE, and this is the half td-txt was missing: `.`
    /// and a NON-MATCHING bracket list stop at a separator.
    #[test]
    fn reg_newline_keeps_dot_and_negated_lists_off_the_newline() {
        let opts = Options { reg_newline: Some(b'\n'), ..Options::default() };
        let dot = Regex::compile(b"c.d", opts).unwrap();
        assert!(dot.search(b"abc\ndef", 0).unwrap().is_none());
        let neg = Regex::compile(b"[^abc]", opts).unwrap();
        // Reaches the `d`, stepping over the newline rather than matching it.
        assert_eq!(neg.search(b"abc\ndef", 0).unwrap().unwrap().start(), 4);
        let star = Regex::compile(b".*", opts).unwrap();
        assert_eq!(star.search(b"abc\ndef", 0).unwrap().unwrap().end(), 3);
        // Without the flag, all three cross it.
        let plain = Options::default();
        assert!(Regex::compile(b"c.d", plain).unwrap().search(b"abc\ndef", 0).unwrap().is_some());
        assert_eq!(
            Regex::compile(b"[^abc]", plain).unwrap().search(b"abc\ndef", 0).unwrap().unwrap().start(),
            3
        );
        // A newline named EXPLICITLY in a positive list still matches under the flag.
        let lit = Regex::compile(b"[\n]", opts).unwrap();
        assert!(lit.search(b"abc\ndef", 0).unwrap().is_some());
    }

    /// The two halves of REG_NEWLINE do not use the same byte when the record
    /// separator is not a newline (sed `-z`, where `N` joins records with a NUL):
    /// the anchor half takes the separator ALONE, the exclusion half takes both.
    #[test]
    fn reg_newline_anchors_on_the_separator_but_excludes_the_newline_too() {
        let nul = Options { reg_newline: Some(0), ..Options::default() };
        // Anchors at the NUL, for EITHER caller...
        assert!(Regex::compile(b"^b", nul).unwrap().search(b"a\0b", 0).unwrap().is_some());
        assert!(Regex::compile(b"^b", nul).unwrap().search_subst(b"a\0b", 0).unwrap().is_some());
        assert!(Regex::compile(b"a$", nul).unwrap().search(b"a\0b", 0).unwrap().is_some());
        // ...but not at a newline, which is not the separator here.
        assert!(Regex::compile(b"^d", nul).unwrap().search(b"abc\ndef", 0).unwrap().is_none());
        assert!(Regex::compile(b"c$", nul).unwrap().search(b"abc\ndef", 0).unwrap().is_none());
        // REG_NEWLINE drops the NEWLINE from `.` and a non-matching list in either
        // path, while the SEPARATOR is only out of reach in a substitution.
        assert!(Regex::compile(b"c.d", nul).unwrap().search(b"abc\ndef", 0).unwrap().is_none());
        assert!(Regex::compile(b"a.b", nul).unwrap().search_subst(b"a\0b", 0).unwrap().is_none());
        assert!(Regex::compile(b"a.b", nul).unwrap().search(b"a\0b", 0).unwrap().is_some());
        assert!(Regex::compile(b"[^a]", nul).unwrap().search_subst(b"a\0b", 0).unwrap()
            .is_some_and(|c| c.start() == 2));
        // Without the flag both bytes are ordinary.
        let plain = Options::default();
        assert!(Regex::compile(b"a.b", plain).unwrap().search(b"a\0b", 0).unwrap().is_some());
        assert!(Regex::compile(b"^b", plain).unwrap().search(b"a\0b", 0).unwrap().is_none());
    }

    /// A separator that is not a newline confines a SUBSTITUTION to the segments
    /// between them, so nothing consumes one there — while the same pattern used as
    /// an ADDRESS matches the whole space, and the newline half of the flag (libc's
    /// REG_NEWLINE, touching only `.` and a non-matching list) applies to both.
    #[test]
    fn a_non_newline_separator_is_consumed_by_nothing_in_a_substitution() {
        let nul = Options { reg_newline: Some(0), escapes: true, ..Options::default() };
        // Nothing may cover the separator at index 1 in a substitution. `[\x00]` is
        // here for the fast paths' sake even though an escape inside a bracket is a
        // recorded gap, and `\s` because a NUL is not whitespace either way.
        for pat in [&b"\\x00"[..], b"[\\x00]", b"\\W", b"\\s", b"[^a]", b".", b"\\(.\\)\\1"] {
            let re = Regex::compile(pat, nul).unwrap();
            assert!(
                re.search_subst(b"a\0a", 0).unwrap().is_none_or(|c| c.end() <= 1 || c.start() >= 2),
                "{:?} consumed the separator",
                String::from_utf8_lossy(pat)
            );
        }
        // The ADDRESS path reaches it, which is what makes confinement a property of
        // the call and not of the compiled pattern. Only patterns that can match a
        // NUL at all show it — not `\s`, and not `[\x00]` while the escape gap stands.
        for pat in [&b"\\x00"[..], b"\\W", b"[^a]", b"."] {
            let re = Regex::compile(pat, nul).unwrap();
            let hit = re.search(b"a\0a", 0).unwrap();
            assert!(hit.is_some(), "{:?} found nothing", String::from_utf8_lossy(pat));
        }
        assert!(Regex::compile(b"a.a", nul).unwrap().search(b"a\0a", 0).unwrap().is_some());
        assert!(Regex::compile(b"a.a", nul).unwrap().search_subst(b"a\0a", 0).unwrap().is_none());
        // The newline is NOT confined either way: `\W` matches one under the flag,
        // which is what makes the two mechanisms distinguishable.
        let nl = Regex::compile(b"\\W", nul).unwrap();
        assert_eq!(nl.search_subst(b"a\nb", 0).unwrap().unwrap().start(), 1);
        assert_eq!(nl.search(b"a\nb", 0).unwrap().unwrap().start(), 1);
        // ...while `.` and a non-matching list still lose it, in EITHER path.
        assert!(Regex::compile(b"a.b", nul).unwrap().search(b"a\nb", 0).unwrap().is_none());
        assert!(Regex::compile(b"a.b", nul).unwrap().search_subst(b"a\nb", 0).unwrap().is_none());
        // With a NEWLINE separator nothing is confined; REG_NEWLINE alone applies.
        let opts = Options { reg_newline: Some(b'\n'), ..Options::default() };
        let w = Regex::compile(b"\\W", opts).unwrap();
        assert_eq!(w.search(b"a\nb", 0).unwrap().unwrap().start(), 1);
        assert_eq!(w.search_subst(b"a\nb", 0).unwrap().unwrap().start(), 1);
    }

    /// The buffer anchors are `^`/`$` to an ADDRESS and true buffer anchors to a
    /// SUBSTITUTION, which is the third thing about `M` this engine had wrong.
    #[test]
    fn the_buffer_anchors_follow_the_separator_only_where_gnu_moves_them() {
        for (sep, hay) in [(b'\n', &b"a\nb"[..]), (0, b"a\0b")] {
            let opts = Options { reg_newline: Some(sep), ..Options::default() };
            let (open, close) = (Regex::compile(b"\\`b", opts).unwrap(), Regex::compile(b"a\\'", opts).unwrap());
            // An address moves them to the record separator, whatever it is.
            assert!(open.search(hay, 0).unwrap().is_some(), "sep {sep}: address lost \\`");
            assert!(close.search(hay, 0).unwrap().is_some(), "sep {sep}: address lost \\'");
            // A substitution moves them only for a SEGMENT, which a newline is not.
            let moved = sep != b'\n';
            assert_eq!(open.search_subst(hay, 0).unwrap().is_some(), moved);
            assert_eq!(close.search_subst(hay, 0).unwrap().is_some(), moved);
        }
        // Without the flag neither caller moves them, and `^` is the contrast that
        // does move: `s/\`a/X/Mg` over `a\na` rewrites one `a`, `s/^a/X/Mg` both.
        let plain = Options::default();
        assert!(Regex::compile(b"\\`b", plain).unwrap().search(b"a\nb", 0).unwrap().is_none());
        let m = Options { reg_newline: Some(b'\n'), ..Options::default() };
        assert!(Regex::compile(b"^b", m).unwrap().search_subst(b"a\nb", 0).unwrap().is_some());
    }

    /// A backreference confines an ADDRESS too, which no rule about `M` predicts —
    /// see `Regex::has_backref`.
    #[test]
    fn a_backreference_confines_the_address_path_as_well() {
        let nul = Options { reg_newline: Some(0), escapes: true, ..Options::default() };
        let hay = &b"a\0\0b"[..];
        // Nothing crosses the separator once a backref is in the pattern, whether the
        // backref does the crossing or a literal does.
        for pat in [&b"\\(.\\)\\1"[..], b"\\(a\\)\\x00\\1*"] {
            let re = Regex::compile(pat, nul).unwrap();
            assert!(re.search(hay, 0).unwrap().is_none(), "{:?} crossed", String::from_utf8_lossy(pat));
        }
        // Without one, an address crosses; and a match inside a segment is unaffected.
        assert!(Regex::compile(b"..", nul).unwrap().search(hay, 0).unwrap().is_some());
        assert!(Regex::compile(b"\\(a\\)\\x00", nul).unwrap().search(hay, 0).unwrap().is_some());
        assert!(Regex::compile(b"\\(b\\)\\1*", nul).unwrap().search(hay, 0).unwrap().is_some());
        // A newline separator has no segment, so a doubled NUL is an ordinary pair.
        let nl = Options { reg_newline: Some(b'\n'), ..Options::default() };
        assert!(Regex::compile(b"\\(.\\)\\1", nl).unwrap().search(hay, 0).unwrap().is_some());
    }

    #[test]
    fn only_the_first_caret_of_a_bre_branch_anchors() {
        assert!(matched(&bre("^^"), "^x"));
        assert!(!matched(&bre("^^"), "x"));
        // …while the repeat rule still sees a branch start.
        assert!(matched(&bre("^*"), "*x"));
        assert!(!matched(&bre("^*"), "x"));
        // `^` then zero-or-more literal carets.
        assert!(matched(&bre("^^*"), "^^x"));
        assert!(matched(&bre("^^*"), "x"));
        // A new branch restores the anchor.
        assert!(matched(&bre(r"\(^^\)b"), "^b"));
        assert!(!matched(&bre(r"\(^^\)b"), "xb"));
        // ERE anchors anywhere, so there the same pattern matches everything.
        let ere = Regex::compile(b"^^", Options { ere: true, ..Options::default() }).unwrap();
        assert!(matched(&ere, "x"));
    }

    #[test]
    fn case_folding_covers_literals_and_classes() {
        let re = Regex::compile(b"[a-z]bc", Options { icase: true, ..Options::default() }).unwrap();
        assert!(matched(&re, "ABC"));
    }

    #[test]
    fn invalid_patterns_report_a_diagnostic() {
        assert!(Regex::compile(b"a\\(", Options::default()).is_err());
        assert!(Regex::compile(b"a\\1", Options::default()).is_err());
        assert!(Regex::compile(b"[a", Options::default()).is_err());
    }

    #[test]
    fn word_operators_bind_to_word_edges() {
        assert!(matched(&bre("\\<cat\\>"), "a cat here"));
        assert!(!matched(&bre("\\<cat\\>"), "concatenate"));
        assert!(matched(&bre("\\bcat\\b"), "a cat"));
    }

    #[test]
    fn star_of_a_group_does_not_loop_on_an_empty_body() {
        let re = bre("\\(a*\\)*b");
        assert!(matched(&re, "aaab"));
        assert!(!matched(&re, "aaa"));
    }
}

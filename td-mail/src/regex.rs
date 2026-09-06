//! Regular expressions for td-mail: td-txt's POSIX engine UNCHANGED (its own header
//! follows this one), plus a `UserRegex` adapter at the end of the file that
//! reads the Rust-`regex` dialect td-mail's rules files are written in. td-mail compiles
//! user-authored patterns from `rules.toml`, the two `[mail]` config settings,
//! and one hard-coded URL pattern; `find_urls` answers that last one without the
//! engine at all.
//!
//! The engine was copied byte for byte and then `cargo fmt`ed with the rest of
//! this crate, which reflows LINE BREAKS only -- td's committed file is not
//! default-rustfmt-clean. No token, comment or test of it differs, which is
//! checkable: `rustfmt td-txt/src/regex.rs` and diff it against the region
//! between this header and the adapter marker below.
//!
//! # The dialect
//!
//! POSIX Extended Regular Expressions over BYTES, with GNU's `\w \W \b \B` and
//! an optional LEADING `(?i)`. `UserRegex::compile` translates the Perl
//! spellings the `regex` crate accepted:
//!
//! - `\d` to `[0-9]`, `\D` to `[^0-9]`, `\s` to `[[:space:]]`, `\S` to
//!   `[^[:space:]]` — the ASCII sets, not the Unicode ones.
//! - `\w \W \b \B` pass through to the engine's GNU extensions.
//! - `\. \[ \] \( \) \| \+ \? \* \{ \} \^ \$ \\ \/ \-` are the literal
//!   character, and `\t \n \r` are the byte.
//! - `(?i)` at the START of the pattern sets case folding and is consumed; no
//!   `(?` reaches the engine.
//! - `(?:…)` is a plain group. td-mail reads no capture, so the numbering this
//!   changes is unobservable.
//! - A bracket expression is re-spelled rather than passed through, because the
//!   two flavours disagree about escapes and placement — see
//!   `Translator::bracket`. `[[:word:]]` and `[[:ascii:]]` translate too.
//! - A non-ASCII literal is grouped, so `é+` repeats the CHARACTER and not its
//!   last byte.
//!
//! # Refused, each naming its byte offset in the pattern
//!
//! `\uXXXX`, `\x…`, `\p{…}`/`\P{…}` and any other unknown escape; a
//! backreference; lookaround (`(?=` `(?!` `(?<=` `(?<!`); a named group; a
//! comment group; an inline flag group that is not the leading `(?i)`; a
//! non-greedy quantifier (`*?` `+?` `??` `{n,m}?`); `&&` inside a class; a
//! negated shorthand inside a class (`[\S]`); a non-ASCII character inside a
//! class; an unbalanced paren or bracket; and any pattern over
//! `MAX_USER_PATTERN` bytes.
//!
//! # Where the answer differs from the `regex` crate
//!
//! - Alternation is POSIX leftmost-LONGEST: `x|xy` matches `xy`, where the
//!   crate matches `x`. Which text a rule's CONDITION matches is unaffected;
//!   which SPAN `find_iter` yields is not.
//! - `.` is one BYTE and matches a newline, where the crate's is one character
//!   and does not. `^` and `$` are still the whole text's, as they are in the
//!   crate without `(?m)`.
//! - `(?i)`, `\w`, `\b` and the named classes are ASCII: `(?i)é` does not match
//!   `É`, and `\w` does not match `é`.
//! - Matching is backtracking under a step budget, so a pathological pattern
//!   reports `too complex` where the crate's automata always answer. `is_match`
//!   reads that as NO MATCH and `find_iter` as the end of the iteration;
//!   `try_is_match` hands it to a caller that can report it.
//! - A few shapes the crate refuses are accepted here — a `{` that opens no
//!   interval is the literal character, and a stacked quantifier (`a**`) is
//!   read as one. That can only widen what an accepted pattern matches, never
//!   change what an existing rule matched.
//!
//! ---- td-txt's `src/regex.rs` follows, verbatim: its header, then the engine.
//! The adapter this header describes is at the END of the file. ----

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
/// The one interval diagnostic that is NOT a brace grep can read as a literal:
/// `-E '{32768}a'` is refused for its SIZE where `-E '{}a'` is text.
const TOO_BIG: &str = "Regular expression too big";

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

/// Dialect and matching knobs. GNU sed's escape vocabulary is NOT one of them: it
/// is decoded into the pattern text before compiling (see `sed::normalize_regex`),
/// so by the time a pattern arrives here every escape left in it is regex syntax.
#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    pub ere: bool,
    pub icase: bool,
    /// Whether a repetition operator with nothing to repeat, stacked on another,
    /// or applied to a zero-width assertion is REFUSED. GNU sed refuses all three
    /// (`Invalid preceding regular expression`) -- so `a**` is its BRE's error and
    /// a quantified assertion its ERE's. GNU grep refuses NONE of them in either
    /// dialect: it prints `warning: * at start of expression` and compiles, which
    /// the vendored Spencer cases pin. Not a knob: it is which tool's syntax bits
    /// glibc was handed.
    pub strict_repeats: bool,
    /// sed's `--posix`, which drops GNU's regex extensions: each is read as the
    /// LITERAL character rather than refused, so `\w` matches a `w` and `a\+`
    /// the text `a+`. Three places read it -- `at_op` for the branch-level `\|`,
    /// `next_op` for the postfix `\+`/`\?`, and `parse_escape` for the class and
    /// anchor escapes. What POSIX itself defines is untouched: `\(`, `\)` around
    /// a real group, the interval, and backreferences all still work, and
    /// `--posix -E 'a{x}'` is still bad content, so this is not a second
    /// `strict_repeats`.
    ///
    /// GNU's `RE_NO_GNU_OPS`, which regexp.c:77 sets for POSIXLY_BASIC alone --
    /// so this is `--posix` and NOT the field below, which the environment
    /// variable also reaches.
    pub posix: bool,
    /// Whether a close-paren with nothing open is the ORDINARY character rather
    /// than an error, in both spellings (`)` in an ERE, `\)` in a BRE).
    ///
    /// Split from `posix` because GNU splits it: `RE_UNMATCHED_RIGHT_PAREN_ORD`
    /// is CLEARED only for POSIXLY_EXTENDED (regexp.c:70), so both of sed's
    /// lower posixicity levels set it and `POSIXLY_CORRECT=1 sed -E 's/a)/X/'`
    /// runs where the default refuses. Every `posix` caller sets this too --
    /// BASIC is a level below CORRECT, not a different axis -- and
    /// `the_paren_rule_is_implied_by_the_extension_rule` pins that on `Mode`,
    /// which is where the pair comes from. It does NOT check the construction
    /// below it: only the corpus catches a site that builds the pair by hand.
    pub unmatched_rparen_ordinary: bool,
    /// Whether a class written without its outer bracket (`[:alpha:]`) is
    /// ACCEPTED as the ordinary bracket expression it looks like, rather than
    /// refused with `CLASS_SYNTAX`.
    ///
    /// A per-applet constant rather than a question about the invocation:
    /// `false` in grep, which hands dfa `DFA_CONFUSING_BRACKETS_ERROR` so the
    /// pattern is refused here; `true` in sed, which raises the lint ITSELF,
    /// after the `s` command's reference check, because that is the order
    /// `compile_regex_1` puts them in (sed/regexp.c:118-133). Whether sed
    /// raises it at all is `POSIXLY_CORRECT`, decided where the deferred lint
    /// is built and not here.
    ///
    /// Only the DIAGNOSTIC is conditional. The pattern parses identically
    /// either way: `class_syntax` is a flag the bracket parser raises and the
    /// error is built from it after the parse, so accepting one costs nothing
    /// but the refusal.
    pub confusing_bracket_ok: bool,
    /// Which of GNU's two engines reads the MID-BRANCH `$` that `stray_anchor`
    /// creates. grep's dfa satisfies one at a true end of line, so `grep 'x$|*'`
    /// selects a line `x`; glibc satisfies one never, so the branch is dead and
    /// `sed 's/x$|*/X/'` matches nothing. Not a dialect difference -- `grep -o`
    /// and any pattern carrying a backreference run glibc too, which td-txt does
    /// not model (see `spec/README`) -- so this carries the caller's USUAL engine,
    /// and it is the only rule that reads it.
    pub glibc_engine: bool,
    /// Whether more of the LEX follows this pattern, which `stray_anchor` -- the
    /// only rule that reads this -- counts as a further byte. Two things put it
    /// there, and grep is the only caller with either: it joins its `-e`/`-f`
    /// patterns with `\n` and lexes the result as one, and `-x`/`-w` wrap the
    /// pattern in context. So `grep -e 'x$|' -e zz`, `grep -x 'x$|'` and
    /// `grep -w 'x$|'` all anchor a `$` that `grep 'x$|'` leaves a literal.
    pub lex_continues: bool,
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
    pub reg_newline: Option<Anchor>,
    /// GNU's `RE_NO_SUB` (sed/regexp.c:87), which an ADDRESS gets because it
    /// compiles with `needed_sub == 0` (compile.c:927). It is not a matching
    /// rule: it is what makes GNU RECOMPILE the pattern on the first use that
    /// wants registers. See `Regex::search_subst_recompiled`.
    pub no_sub: bool,
}

/// Where `M` puts `^` and `$`. Two bytes rather than one, because GNU reads
/// sed's record delimiter TWICE and at different times: glibc's
/// `newline_anchor` is set from the delimiter as it stood when the part was
/// COMPILED (sed/regexp.c:101), while the segment split that stands in for it
/// once the delimiter is not a newline is chosen from the delimiter in force
/// when the regex RUNS (regexp.c:303).
///
/// Those are the same byte unless `-z` FOLLOWS the `-e` it applies to, and
/// then a pattern anchors on both: `sed -n -e 's/^/>/Mgp' -z` marks the start
/// of every NUL record AND of every line inside one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Anchor {
    /// The delimiter the RUN splits on, which is also what confines a
    /// substitution (`Regex::segment`).
    pub sep: u8,
    /// glibc's `newline_anchor`: `buffer_delimiter == '\n'` at COMPILE time.
    pub newline_anchor: bool,
}

impl Anchor {
    fn holds(self, b: u8) -> bool {
        b == self.sep || (self.newline_anchor && b == b'\n')
    }
}

/// How ONE scan reads positions. Normally what the pattern compiled to, but
/// GNU's dfa prefilter is a scan of the SAME pattern under a different reading,
/// so this is a parameter of the scan rather than a property of the `Regex`.
#[derive(Clone, Copy)]
struct Reading {
    reg_newline: Option<Anchor>,
    /// What a substitution may not consume, and what moves its buffer anchors.
    segment: Option<u8>,
    /// Is this a SUBSTITUTION? See `Regex::search_subst`.
    subst: bool,
    /// Read a backreference as GNU's dfa does rather than as the pattern means
    /// it: one NULLABLE position matching any byte under no constraint
    /// (lib/dfa.c:2285,2796). Only the prefilter sets it -- see
    /// `Regex::veto_reading` for why filtering with the exact backreference is
    /// wrong in one direction and not filtering at all is wrong in the other.
    approx_backref: bool,
    /// glibc's `not_eol`: `$` does not hold at the end of THIS haystack. GNU sets
    /// it for `-w`'s shrink (grep/src/dfasearch.c:539), where the haystack is the
    /// line cut short and its end is not the line's.
    not_eol: bool,
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
        self.bits
            .get(word)
            .is_some_and(|w| w & (1u64 << (b & 63)) != 0)
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
    /// A position that never holds: glibc's reading of the mid-branch `$` above.
    Never,
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

/// One postfix repetition operator, as written.
enum Op {
    Star,
    Plus,
    Quest,
    Interval(u32, Option<u32>),
}

/// Whether a node asserts a POSITION rather than consuming input. A repetition
/// operator finds nothing to repeat in one, in either grammar.
fn is_assertion(node: &Node) -> bool {
    matches!(
        node,
        Node::Bol
            | Node::Eol
            | Node::Never
            | Node::BufStart
            | Node::BufEnd
            | Node::WordBoundary(_)
            | Node::WordEdge(_)
    )
}

/// A compiled pattern.
#[derive(Clone, Debug)]
pub struct Regex {
    /// A class was written without its outer bracket (`[:alpha:]`). Exposed
    /// rather than only raised because GNU raises it in `dfacomp`, a step
    /// AFTER the `s` command's out-of-range `\N` check (sed/regexp.c:118-133),
    /// so sed's `s` path has to compile first and refuse afterwards. `compile`
    /// still refuses for every caller that has nothing to run in between.
    pub class_syntax: bool,
    /// See `Options::no_sub`.
    pub no_sub: bool,
    /// The bytes GNU would name in `warning: stray \ before X`, in pattern
    /// order. Exposed rather than raised: whether a stray is REPORTED depends
    /// on the whole pattern SET, which only the caller knows. See
    /// `grep::gnu_runs_regex_matcher`.
    pub strays: Vec<u8>,
    root: Node,
    ngroups: usize,
    icase: bool,
    reg_newline: Option<Anchor>,
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
    /// The byte after each backslash GNU would lint as a stray, in pattern
    /// order. COLLECTED rather than printed because this parser is shared and
    /// only grep warns: sed reaches the same escapes silently.
    strays: Vec<u8>,
    /// Groups whose `)` has not been reached. A backreference may not name one:
    /// GNU refuses `\(\1\)` as an invalid back reference, where naming a group
    /// that HAS closed is fine from anywhere later in the pattern.
    open: Vec<usize>,
    /// Half-open group-number ranges belonging to EARLIER branches of the
    /// alternations still open here. GNU refuses a backreference into one
    /// (`(a)|b\1`), and allows the same reference once the alternation has
    /// closed (`((a)|b)\2` compiles, and fails to match instead).
    sibling: Vec<(usize, usize)>,
    /// A bracket list shaped like a class that lost its outer bracket. GNU lints
    /// this only once the WHOLE pattern has compiled, so any other error outranks
    /// it -- `[:alpha:]\` is `Trailing backslash`, not this.
    class_syntax: bool,
    /// Groups left unclosed because a grep ERE drop ATE the `)` they needed (see
    /// `drop_eats_paren`), settled by a `)` no drop has eaten. Judged after the
    /// pattern parses, so an error found further on wins -- `(a\b*)\` is
    /// `Trailing backslash` -- but this outranks the whole-pattern
    /// `class_syntax` lint, as GNU orders them.
    paren_debt: usize,
    /// The `)` most recently eaten. Eating is idempotent, and an eaten `)` is
    /// SPENT: it cannot also settle a debt, which is what separates `(*))` from
    /// `(*)\b*)`. One slot is enough -- the parser scans forward, so the paren
    /// just eaten is the next one anything asks about.
    eaten: Option<usize>,
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
        if self.posix_drops(ch) {
            return false;
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
        if self.posix_drops(ch) {
            return false;
        }
        self.peek() == Some(b'\\') && self.peek_at(1) == Some(ch)
    }

    /// The BRE operators `--posix` drops, leaving the character ordinary. `\(`,
    /// `\)` and the interval are POSIX's own and stay operators there. Shared so
    /// the test and the CONSUMER cannot disagree: only `\|` reaches either today
    /// (`next_op` handles the postfix pair, and `parse_concat` under the flag
    /// never leaves the parser sitting on one), but a consumer that took what the
    /// test beside it had just refused would restore GNU alternation under the
    /// flag with nothing anywhere to notice.
    fn posix_drops(&self, ch: u8) -> bool {
        self.opts.posix && matches!(ch, b'|' | b'+' | b'?')
    }

    /// Whether a BRE `$` anchors DESPITE more pattern following it. GNU tests the
    /// byte after the `$` against `|` and `)` -- the ERE spellings of the two
    /// operators whose BRE spellings (`\|`, `\)`) legitimately end a branch -- but
    /// keeps the length test those two-byte spellings need. So a BARE `|`/`)`
    /// anchors it too, unless that byte is the pattern's last: `x$|` selects the
    /// text `x$|`, while `x$||` and `x$|a` anchor and `x$a` does not. "Last" is of
    /// the whole LEX, which for grep is every `-e`/`-f` pattern joined by `\n`.
    fn stray_anchor(&self) -> bool {
        !self.opts.ere
            && matches!(self.peek(), Some(b'|' | b')'))
            && (self.peek_at(1).is_some() || self.opts.lex_continues)
    }

    /// Which `)` a grep ERE drop EATS, if it eats one. gnulib drops an operator
    /// with nothing to repeat and recurses on the next token, and an unmatched
    /// `)` reached that way is an ordinary character -- so the drop consumes it,
    /// and it neither closes a group nor pays for one. Asked where there is
    /// nothing to repeat: a branch start, and again just after an assertion,
    /// which puts the parser back in that state -- so `(*)` and `(a\b*)` eat
    /// alike while `(\b*a)` eats nothing, and it is the INNER paren that goes.
    ///
    /// `op` says an operator was dropped; a brace left with nothing to repeat
    /// eats one without an operator at all (`({)`, `(\b*{)`, but not `({a)` or
    /// `(a{)`). `repeatable` says something an operator could repeat stands here,
    /// which stops the eat: an INTERVAL puts it there even when dropped
    /// (`({2}*)`, `(*{2})`, `({2}{)`), and an assertion takes it away again, so
    /// `({2}\b*)` eats. It is the same per-token state the brace rule reads,
    /// which is why one assertion re-arms both -- `(a\b{{*)` is refused where
    /// `(a{{*)` is not.
    ///
    /// Depth is NOT a condition, and that is the whole of why `(*)\b*)` is
    /// refused where `(*))` compiles: the second drop eats the paren the group
    /// was still waiting for, at depth 0 as readily as inside.
    fn drop_eats_paren(&self, at: usize, op: bool, repeatable: bool) -> Option<usize> {
        if !self.opts.ere || self.opts.strict_repeats || repeatable {
            return None;
        }
        match self.pat.get(at) {
            // A brace with nothing to repeat eats the paren behind it, and the
            // operators it DOES carry ride along: `({*)` and `({**)` go the way
            // `({)` does. They are not dropped -- `{*a` still repeats the brace --
            // so this is the brace's own eat reaching past them, not theirs.
            Some(&b'{') => {
                let mut i = at + 1;
                while matches!(self.pat.get(i), Some(b'*' | b'+' | b'?')) {
                    i += 1;
                }
                match self.pat.get(i) {
                    Some(&b')') => Some(i),
                    _ => None,
                }
            }
            Some(&b')') if op => Some(at),
            _ => None,
        }
    }

    /// Eat the `)` at `close`, owing one if it was a paren a group needed.
    /// Idempotent because the same paren is reached from more than one place: a
    /// branch start runs twice over `(*{)`, dropping the operator on the first
    /// pass and meeting the brace on the second, and an assertion hands `(\b*{)`
    /// to that same block. Each eats one `)`, so `(*{))` and `(\b*{))` compile.
    fn eat_paren(&mut self, close: usize, depth: usize) {
        if self.eaten == Some(close) {
            return;
        }
        self.eaten = Some(close);
        if depth > 0 {
            self.paren_debt += 1;
        }
    }

    fn parse_alt(&mut self, depth: usize) -> Result<Node, Error> {
        // Groups nest by recursion here and in every AST walk below, so a pattern
        // of 20k `(` would exhaust the stack — which aborts, and an abort is not
        // an error a caller can report. Cap it far above any real pattern.
        if depth > MAX_NESTING {
            return Err(Error::new("regular expression is too complex"));
        }
        let start = self.ngroups;
        let mut branches = vec![self.parse_concat(depth)?];
        while self.eat_op(b'|') {
            // Every group opened so far in THIS alternation belongs to a branch
            // this one is not in, and is out of scope for a backreference until
            // the alternation closes again.
            self.sibling.push((start, self.ngroups));
            let branch = self.parse_concat(depth);
            self.sibling.pop();
            branches.push(branch?);
        }
        if branches.len() == 1 {
            return branches
                .pop()
                .ok_or_else(|| Error::new("empty alternation"));
        }
        Ok(Node::Alt(branches))
    }

    fn parse_concat(&mut self, depth: usize) -> Result<Node, Error> {
        let mut items: Vec<Node> = Vec::new();
        // Whether grep's ERE would read an unusable brace here as TEXT. Distinct
        // from `first`, which BRE and sed also read: an ASSERTION establishes this
        // state wherever it stands, so `a\b{}` is text where `a{}` is a complaint
        // about content, and `first` -- which can only be lost -- cannot say that.
        let mut brace_text = true;
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
            // POSIX leaves an ERE `)` with no open group undefined and the two
            // tools took different readings: grep makes it a literal at depth 0,
            // sed refuses it. Same split as the brace below, and the same flag.
            if depth == 0
                && self.opts.ere
                && self.opts.strict_repeats
                && !self.opts.unmatched_rparen_ordinary
                && self.at_op(b')')
            {
                return Err(Error::new("Unmatched ) or \\)"));
            }
            if items.len() >= MAX_CONCAT {
                return Err(Error::new("regular expression is too complex"));
            }
            // An ERE has nothing to repeat at the START of a branch. sed refuses
            // that; grep DROPS the operator, so `grep -E '*a'` is the pattern `a`
            // and `-E '*+a'` still is. A BRE reads the same characters as literals
            // in both tools (`sed 's/*a/-/'` matches `*a`). Only `*` and the
            // interval are named here: `parse_atom` already refuses a leading ERE
            // `+`/`?`, which grep's drop loop below consumes before reaching it.
            if self.opts.ere && first {
                if self.opts.strict_repeats {
                    // A brace with nothing to repeat is refused for THAT, before its
                    // content is judged: `-E '{}a'` is `Invalid preceding regular
                    // expression` where `-E 'a{}'` is `Invalid content of \{\}`.
                    let dup = match self.parse_interval() {
                        Ok(found) => found.is_some(),
                        Err(_) => true,
                    };
                    if self.peek() == Some(b'*') || dup {
                        return Err(Error::new("Invalid preceding regular expression"));
                    }
                } else {
                    let mut dropped = false;
                    // An INTERVAL dropped here does not count for the group rule
                    // below: `-E '({2})'` compiles where `-E '(*)'` does not.
                    let mut dropped_op = false;
                    loop {
                        if matches!(self.peek(), Some(b'*' | b'+' | b'?')) {
                            self.pos += 1;
                            dropped = true;
                            dropped_op = true;
                            continue;
                        }
                        // A brace GNU cannot read as an interval is a literal here,
                        // content and all: `grep -E '{}a'` selects the text `{}a`.
                        // Its SIZE limit is not that kind of complaint and stands.
                        let save = self.pos;
                        match self.parse_interval() {
                            Ok(Some(_)) => {
                                dropped = true;
                                // Dropped, and it still ends the state: `{2}{}`
                                // complains where `{}` and `*{}` do not.
                                brace_text = false;
                                continue;
                            }
                            Ok(None) => {}
                            // A brace that closes on bad content is the text it is
                            // while there is nothing to repeat, and the complaint it
                            // is once something to repeat has appeared: `{}` and
                            // `*{}` compile, `{2}{}` does not.
                            Err(e) if e.msg == TOO_BIG || !brace_text => return Err(e),
                            Err(_) => self.pos = save,
                        }
                        break;
                    }
                    if let Some(close) = self.drop_eats_paren(self.pos, dropped_op, !brace_text) {
                        self.eat_paren(close, depth);
                    }
                    // What is left may be the end of the branch: `-E '*'` is empty.
                    if dropped {
                        continue;
                    }
                }
            }
            // grep's ERE carries its nothing-to-repeat state past a brace it could
            // not read as an interval, exactly as it does past an assertion, so a
            // run of them keeps it: `{{{{}` compiles. Reaching `parse_atom` with a
            // `{` ahead is itself the test -- a brace that opened an interval was
            // consumed as one long before this.
            let brace_lit = self.opts.ere && !self.opts.strict_repeats && self.peek() == Some(b'{');
            let atom = self.parse_atom(depth, first, bol_ok)?;
            // An assertion is not something to repeat, so it does not give a
            // following operator one either. In a BRE that makes the operator a
            // LITERAL: GNU reads `\B\?a` the way it reads `\?a`, and `x\b*` selects
            // the `x*` in `x*y`. sed reads it that way ANYWHERE; grep only at the
            // start of a branch, repeating the assertion past it. An ERE keeps its
            // operators operators in both.
            let assertion = is_assertion(&atom);
            // An assertion gives its operators nothing to repeat, so a grep ERE
            // drops them and is back where a branch start is -- which is why
            // `(a\b*)` refuses like `(*)`, and `(\b*a)` does not.
            if assertion {
                let mut at = self.pos;
                while matches!(self.pat.get(at), Some(b'*' | b'+' | b'?')) {
                    at += 1;
                }
                if let Some(close) = self.drop_eats_paren(at, at > self.pos, false) {
                    self.eat_paren(close, depth);
                }
            }
            // A brace reached with nothing to repeat eats the paren behind it
            // wherever it stands, not only at a branch start: `(a\b{{*)` is
            // refused, the assertion having re-armed the state for the run of
            // braces after it, where `(a{{*)` has nothing to re-arm it.
            if brace_lit && brace_text {
                if let Some(close) = self.drop_eats_paren(self.pos, false, false) {
                    self.eat_paren(close, depth);
                }
            }
            let bare = assertion && !self.opts.ere && (self.opts.strict_repeats || first);
            let atom = match bare {
                true => atom,
                false => {
                    // The state this atom leaves: an assertion ESTABLISHES it, a
                    // brace already text only preserves it, anything else ends it.
                    let after = assertion || (brace_text && brace_lit);
                    let ere_text = after && self.opts.ere && !self.opts.strict_repeats;
                    let (atom, interval) = self.parse_repeats(atom, ere_text)?;
                    brace_text = after && !interval;
                    atom
                }
            };
            first = first && (assertion || brace_lit);
            bol_ok = false;
            items.push(atom);
        }
        match items.len() {
            0 => Ok(Node::Empty),
            1 => items.pop().ok_or_else(|| Error::new("empty branch")),
            _ => Ok(Node::Concat(items)),
        }
    }

    /// The next postfix repetition operator, consumed. `None` when what follows is
    /// not one in this dialect.
    /// `brace_literal` says grep's ERE has nothing to repeat here, where a brace
    /// that CLOSES on bad content is the text it is rather than a complaint about
    /// it -- `^{}` and `{{}` compile where `a{}` does not. A brace that never
    /// closes already rewinds itself in `parse_interval`, which is why `{2}{`
    /// needs none of this.
    fn next_op(&mut self, brace_literal: bool) -> Result<Option<Op>, Error> {
        if self.peek() == Some(b'*') {
            self.pos += 1;
            return Ok(Some(Op::Star));
        }
        if self.opts.ere && matches!(self.peek(), Some(b'+' | b'?')) {
            let plus = self.peek() == Some(b'+');
            self.pos += 1;
            return Ok(Some(match plus {
                true => Op::Plus,
                false => Op::Quest,
            }));
        }
        // GNU BRE: `\+` and `\?` are the one-or-more / optional operators, and are
        // the two a second operator may follow. `--posix` drops both, and this is
        // the second place that has to know -- `at_op` covers the branch-level
        // `\|`, this the postfix pair.
        if !self.opts.ere && !self.opts.posix && self.peek() == Some(b'\\') {
            match self.peek_at(1) {
                Some(b'+') => {
                    self.pos += 2;
                    return Ok(Some(Op::Plus));
                }
                Some(b'?') => {
                    self.pos += 2;
                    return Ok(Some(Op::Quest));
                }
                _ => {}
            }
        }
        let save = self.pos;
        match self.parse_interval() {
            Ok(Some((min, max))) => Ok(Some(Op::Interval(min, max))),
            Ok(None) => Ok(None),
            // A size limit is not a complaint about content and stands either way,
            // as it does at a branch start.
            Err(e) if !brace_literal || e.msg == TOO_BIG => Err(e),
            Err(_) => {
                self.pos = save;
                Ok(None)
            }
        }
    }

    /// One repetition operator applied to a zero-width assertion, which only grep's
    /// BRE past the start of a branch, and both ERE dialects, ever reach: sed's BRE
    /// hands the operator back as a literal, and sed's ERE refuses it. grep repeats
    /// the assertion itself, and asserting a position n times at one position
    /// asserts it once, so only the lower bound survives -- vacuous when the
    /// assertion may hold zero times, otherwise whatever is already there
    /// (`x\B\+b` selects `xb` and not `x b`).
    fn quantified_assertion(atom: Node, min: u32) -> Node {
        match min {
            0 => Node::Empty,
            _ => atom,
        }
    }

    /// Apply every postfix repetition operator that follows an atom. Neither
    /// grammar is the permissive one. A BRE refuses a second `*` or interval after
    /// ANY operator (`a**`, `a\{2\}*`, `\(a\)**`) while taking `\+` and `\?`
    /// there (`a*\+` and `a\?\?` compile). An ERE stacks all four freely, but sed
    /// refuses every one of them on a zero-width assertion (`^*`, `\b+`).
    /// A zero-width atom reaches here only from grep's BRE past the start of a
    /// branch, or from either ERE; sed's BRE hands the operator back as a literal
    /// before the call.
    /// The bool reports a valid INTERVAL among the operators consumed, which is
    /// what stops `drop_eats_paren` eating for the rest of the branch.
    fn parse_repeats(
        &mut self,
        mut atom: Node,
        brace_literal: bool,
    ) -> Result<(Node, bool), Error> {
        let zero_width = is_assertion(&atom);
        // A brace on an assertion has nothing to repeat, and sed reports THAT
        // before it reads the brace at all: `-E '^{}'`, `-E '^{2,1}'` and even
        // `-E '^{a}'` -- which after any other atom would be a literal -- are all
        // `Invalid preceding regular expression`, where `-E 'a{}'` is content.
        if self.opts.strict_repeats && zero_width && self.at_op(b'{') {
            return Err(Error::new("Invalid preceding regular expression"));
        }
        let mut repeated = false;
        let mut interval = false;
        // An INTERVAL ends the nothing-to-repeat state as it is consumed, so a
        // second brace behind it is judged with the state it leaves, not the one
        // it found: `\b{2}{}` is the complaint `\b{}` is not. `*`/`+`/`?` leave
        // the state alone, which is why `\b*{}` stays text.
        let mut brace_literal = brace_literal;
        while let Some(op) = self.next_op(brace_literal)? {
            interval = interval || matches!(op, Op::Interval(..));
            brace_literal = brace_literal && !interval;
            let stacked = match op {
                Op::Star => (self.opts.ere && zero_width) || (!self.opts.ere && repeated),
                Op::Plus | Op::Quest => self.opts.ere && zero_width,
                Op::Interval(..) => zero_width || (!self.opts.ere && repeated),
            };
            if self.opts.strict_repeats && stacked {
                return Err(Error::new("Invalid preceding regular expression"));
            }
            let (min, max) = match op {
                Op::Star => (0, None),
                Op::Plus => (1, None),
                Op::Quest => (0, Some(1)),
                Op::Interval(min, max) => (min, max),
            };
            if zero_width {
                atom = Self::quantified_assertion(atom, min);
                repeated = false;
                continue;
            }
            atom = Node::Repeat {
                node: Box::new(atom),
                min,
                max,
            };
            repeated = true;
        }
        Ok((atom, interval))
    }

    /// Whether the interval opened before `pos` CLOSES. Both BRE diagnostics need
    /// it and, since sed's ERE stopped falling back to a literal brace, so does
    /// that one -- which is the only difference between the two arms below: a BRE
    /// spells the closer `\}` and an ERE `}`. An escaped pair is consumed whole
    /// either way, so the `}` in BRE `a\{x\\}` and in ERE `a{x\}` closes nothing.
    fn interval_closes(&self) -> bool {
        let mut i = self.pos;
        while let Some(b) = self.pat.get(i).copied() {
            if b == b'\\' {
                // An escaped pair is skipped whole. In a BRE the closer IS such a
                // pair; in an ERE `\}` is an escaped brace and closes nothing, so
                // `a{x\}` is `Unmatched \{` and `a{x\}}` is bad content.
                match self.pat.get(i + 1) {
                    Some(b'}') if !self.opts.ere => return true,
                    Some(_) => i += 2,
                    None => return false,
                }
                continue;
            }
            // An ERE spells the closer with a bare brace.
            if self.opts.ere && b == b'}' {
                return true;
            }
            i += 1;
        }
        false
    }

    /// `{n}`, `{n,}`, `{n,m}` (ERE) / `\{…\}` (BRE). Returns `None` when what
    /// follows is not an interval at all, which only GREP's ERE has a reading for
    /// (a literal `{`); sed's ERE and both BREs must have one, and say so.
    fn parse_interval(&mut self) -> Result<Option<(u32, Option<u32>)>, Error> {
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
                // A `{` that opens no valid interval is a literal to GREP, whose
                // ERE reads it and its content as text. Everywhere else -- both
                // BREs and SED's ERE -- a `{` must open one, and GNU asks whether
                // it CLOSES before it judges the content, so `a{x}` is bad content
                // and `a{x` is unmatched.
                if self.opts.ere && !self.opts.strict_repeats {
                    self.pos = save;
                    return Ok(None);
                }
                return Err(Error::new(match self.interval_closes() {
                    true => "Invalid content of \\{\\}",
                    false => "Unmatched \\{",
                }));
            }
        };
        let max = if self.eat(b',') {
            self.parse_number()
        } else {
            Some(min)
        };
        if !self.eat_op(b'}') {
            if self.opts.ere && !self.opts.strict_repeats {
                self.pos = save;
                return Ok(None);
            }
            return Err(Error::new(match self.interval_closes() {
                true => "Invalid content of \\{\\}",
                false => "Unmatched \\{",
            }));
        }
        if let Some(m) = max {
            if m < min {
                return Err(Error::new("Invalid content of \\{\\}"));
            }
        }
        if min > RE_DUP_MAX || max.is_some_and(|m| m > RE_DUP_MAX) {
            return Err(Error::new(TOO_BIG));
        }
        Ok(Some((min, max)))
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
        let b = self
            .bump()
            .ok_or_else(|| Error::new("unexpected end of pattern"))?;
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
                } else if self.stray_anchor() {
                    Ok(match self.opts.glibc_engine {
                        true => Node::Never,
                        false => Node::Eol,
                    })
                } else {
                    Ok(self.literal(b'$'))
                }
            }
            b'*' if first => Ok(self.literal(b'*')), // leading `*` is literal
            b'(' if self.opts.ere => {
                self.ngroups += 1;
                let idx = self.ngroups;
                self.open.push(idx);
                let inner = self.parse_alt(depth + 1);
                self.open.pop();
                let inner = inner?;
                if !self.eat(b')') {
                    return Err(Error::new("Unmatched ( or \\("));
                }
                Ok(Node::Group(idx, Box::new(inner)))
            }
            b')' if self.opts.ere && depth == 0 => {
                // Ordinary here, and it settles a group a drop left open -- unless a
                // drop ATE this very paren, which spends it. `({))` compiles where
                // `)({)` (the spare coming first) and `(*)\b*)` (eaten) do not.
                if self.eaten != Some(self.pos.saturating_sub(1)) {
                    self.paren_debt = self.paren_debt.saturating_sub(1);
                }
                Ok(self.literal(b')'))
            }
            b'+' | b'?' if self.opts.ere => Err(Error::new("Invalid preceding regular expression")),
            b'\\' => self.parse_escape(depth),
            _ => Ok(self.literal(b)),
        }
    }

    /// `\B` where the escape leaves `B` a plain literal, recording it as a
    /// stray unless the byte is one GNU's lexer gives a case of its own. Only
    /// the arms that PRODUCE a literal call this: an escape handled above --
    /// `\b`, `\<`, a backreference -- is an operator and never a stray.
    fn stray(&mut self, b: u8) -> Node {
        // The metacharacters an escape merely makes literal. GNU reaches
        // `normal_char` for each through its own `case`, and reaching
        // `stray_backslash` instead is what MAKES an escape a stray
        // (dfa.c:1558-1580).
        const QUIET: &[u8] = b"$*.[\\]^|}";
        // Operators in an ERE, so escaping one is how a literal is written;
        // in a BRE the escape makes them the OPERATOR, which is the reverse.
        const QUIET_ERE: &[u8] = b"()+?{";
        let quiet = QUIET.contains(&b) || (self.opts.ere && QUIET_ERE.contains(&b));
        // A confusing bracket EXITS GNU's lexer (dfa.c:1143-1148), so nothing past
        // one is ever lexed and nothing past one can lint: `\d[:alpha:]\y`
        // names `d` and never `y`.
        if !quiet && !self.class_syntax {
            self.strays.push(b);
        }
        self.literal(b)
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
                self.open.push(idx);
                let inner = self.parse_alt(depth + 1);
                self.open.pop();
                let inner = inner?;
                if !self.eat_op(b')') {
                    return Err(Error::new("Unmatched ( or \\("));
                }
                Ok(Node::Group(idx, Box::new(inner)))
            }
            // A `\)` with nothing open. Both tools refuse it, unlike the ERE `)`
            // above that grep reads as the character -- but either POSIX level
            // makes it ordinary for this spelling too.
            b')' if !self.opts.ere => match self.opts.unmatched_rparen_ordinary {
                true => Ok(self.literal(b')')),
                false => Err(Error::new("Unmatched ) or \\)")),
            },
            // An interval reaching here has nothing to repeat (`\{2\}a`, `\<\{2\}a`).
            // sed refuses that; grep warns `stray \ before {` and reads the brace as
            // the character, so `grep '\{2\}'` matches the text `{2}`.
            b'{' if !self.opts.ere => match self.opts.strict_repeats {
                true => Err(Error::new("Invalid preceding regular expression")),
                // Reached only with nothing to repeat -- an interval is
                // consumed as a repeat before this -- which is exactly when
                // GNU's `laststart` sends `\{` to the stray lint.
                false => Ok(self.stray(b'{')),
            },
            // An interval consumes its own `\}`, so one reaching here closes nothing
            // and GNU reads it as the character: `s/a\}/-/` matches `a}`.
            b'}' if !self.opts.ere => Ok(self.literal(b'}')),
            b'1'..=b'9' => {
                let n = usize::from(b - b'0');
                let sibling = self.sibling.iter().any(|(lo, hi)| n > *lo && n <= *hi);
                if n > self.ngroups || self.open.contains(&n) || sibling {
                    return Err(Error::new("Invalid back reference"));
                }
                Ok(Node::Backref(n))
            }
            // The character-class and anchor escapes are GNU extensions too, and
            // `--posix` reads each as the LITERAL character rather than refusing
            // it: `\w` matches a `w`, `` \` `` a backtick. Listed rather than
            // caught by the fallthrough so a new escape must decide for itself.
            b'w' | b'W' | b's' | b'S' | b'b' | b'B' | b'<' | b'>' | b'`' | b'\''
                if self.opts.posix =>
            {
                Ok(self.literal(b))
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
            _ => Ok(self.stray(b)),
        }
    }

    /// A bracket expression: ranges, negation, `[:class:]`, `[.c.]`, `[=c=]`.
    fn parse_bracket(&mut self) -> Result<Node, Error> {
        let mut set = ByteSet::empty();
        let negated = self.eat(b'^');
        let body = self.pos;
        let mut sub = false;
        let mut ranged = false;
        let mut first = true;
        loop {
            let Some(b) = self.bump() else {
                // An EMPTY body that runs out is not an unmatched bracket to GNU
                // but a bad pattern: `[` and `[^` differ from `[a` and `[]`.
                return Err(Error::new(if first {
                    "Invalid regular expression"
                } else {
                    "Unmatched [, [^, [:, [., or [="
                }));
            };
            if b == b']' && !first {
                // GNU refuses the `[:alpha:]` that was meant to be `[[:alpha:]]`,
                // by the shape of the list rather than by the name in it: colons
                // at both ends, a NON-COLON somewhere between them, and neither a
                // sub-expression nor a range in it. So `[:*:]` is refused while
                // `[::]`, `[::::]`, `[:a[.b.]:]` and `[:a-z:]` are ordinary lists.
                let raw = self
                    .pat
                    .get(body..self.pos.saturating_sub(1))
                    .unwrap_or_default();
                if !sub
                    && !ranged
                    && raw.first() == Some(&b':')
                    && raw.last() == Some(&b':')
                    && raw.iter().any(|&c| c != b':')
                {
                    self.class_syntax = true;
                }
                break;
            }
            first = false;
            let lo = self.bracket_member(b, &mut set, false)?;
            sub = sub || lo.is_sub_expr();
            // A range, unless `-` is the last character before `]`.
            if self.peek() == Some(b'-') && self.peek_at(1).is_some_and(|c| c != b']') {
                self.pos += 1;
                let h = self
                    .bump()
                    .ok_or_else(|| Error::new("Unmatched [, [^, [:, [., or [="))?;
                let hi = self.bracket_member(h, &mut set, true)?;
                sub = sub || hi.is_sub_expr();
                match (lo.bound(), hi.bound()) {
                    (Some(lo), Some(hi)) if hi >= lo => set.insert_range(lo, hi),
                    _ => return Err(Error::new("Invalid range end")),
                }
                ranged = true;
                // A completed range names no single character either, so it
                // cannot bound the next one: `[a-b-c]` is an error where the
                // trailing `-` of `[a-b-]` is the literal it always is.
                if self.peek() == Some(b'-') && self.peek_at(1).is_some_and(|c| c != b']') {
                    return Err(Error::new("Invalid range end"));
                }
                continue;
            }
            if let Some(one) = lo.bound() {
                set.insert(one);
            }
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

    /// One member of a bracket list. `Some(byte)` may bound a range; `None` is a
    /// whole set already added, which POSIX forbids as a range end. Only a
    /// COLLATING ELEMENT names a single character, so `[[.a.]-z]` is the range
    /// `a-z` while `[[:alpha:]-z]` and `[[=a=]-z]` are `Invalid range end`.
    fn bracket_member(
        &mut self,
        b: u8,
        set: &mut ByteSet,
        range_end: bool,
    ) -> Result<Member, Error> {
        if b != b'[' {
            return Ok(Member::Byte(b));
        }
        let Some(kind @ (b':' | b'.' | b'=')) = self.peek() else {
            return Ok(Member::Byte(b));
        };
        self.pos += 1;
        // As the HIGH end of a range the kind alone decides, ahead of the name: a
        // class or equivalence class cannot end one whatever it is called, so
        // `[a-[:bogus:]]` is `Invalid range end` and not a bad class name. The LOW
        // end is a member first and is named before any `-` is seen, which is why
        // `[[:bogus:]-a]` reports the name instead.
        if range_end && kind != b'.' {
            return Err(Error::new("Invalid range end"));
        }
        let name = self.bracket_name(kind)?;
        if kind == b':' {
            self.add_named_class(set, &name)?;
            return Ok(Member::Set);
        }
        // A collating element / equivalence class of one byte is that byte in the
        // C locale; anything longer has no C-locale meaning.
        let [one] = name.as_slice() else {
            return Err(Error::new("Invalid collation character"));
        };
        if kind == b'=' {
            set.insert(*one);
            return Ok(Member::Set);
        }
        Ok(Member::Collating(*one))
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
            _ => return Err(Error::new("Invalid character class name")),
        }
        Ok(())
    }
}

/// One member of a bracket list, and whether it may bound a range. Named rather
/// than inferred from how far the parser moved: a future member that consumes
/// bytes for some other reason would silently read as a sub-expression.
#[derive(Clone, Copy)]
enum Member {
    /// An ordinary character.
    Byte(u8),
    /// `[.x.]` -- one character, spelled the long way, so it may bound a range.
    Collating(u8),
    /// `[:class:]` or `[=equiv=]`, already added: a set, never a range bound.
    Set,
}

impl Member {
    /// The single character this member names, if it names one.
    fn bound(self) -> Option<u8> {
        match self {
            Member::Byte(b) | Member::Collating(b) => Some(b),
            Member::Set => None,
        }
    }

    fn is_sub_expr(self) -> bool {
        !matches!(self, Member::Byte(_))
    }
}

/// POSIX's interval ceiling; GNU rejects a bound above it.
const RE_DUP_MAX: u32 = 32767;

/// How deeply groups may nest. Bounds the parser's recursion and every AST walk
/// that follows it. Sized against GNU grep 3.11, which handles 5k nested groups
/// and reports its own `stack overflow` past that; td-txt refuses one step
/// earlier with a diagnosed error instead of aborting.
const MAX_NESTING: usize = 5_000;

/// GNU's own words for a `[:alpha:]` written without its outer bracket. Named
/// because sed classifies it by text: it is the one PATTERN error GNU reports
/// bare and exits 4 for, where every other one is exit 1 behind an `-e
/// expression #N' prefix.
pub const CLASS_SYNTAX: &str = "character class syntax is [[:space:]], not [:space:]";

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
        let mut p = Parser {
            pat: pattern,
            pos: 0,
            opts,
            ngroups: 0,
            open: Vec::new(),
            sibling: Vec::new(),
            class_syntax: false,
            paren_debt: 0,
            eaten: None,
            strays: Vec::new(),
        };
        let root = p.parse_alt(0)?;
        if p.pos < pattern.len() {
            // Only an unbalanced `)` can stop the top-level parse early.
            return Err(Error::new("Unmatched ) or \\)"));
        }
        if p.paren_debt > 0 {
            return Err(Error::new("Unmatched ( or \\("));
        }
        if p.class_syntax && !opts.confusing_bracket_ok {
            return Err(Error::new(CLASS_SYNTAX));
        }
        let first = first_bytes(&root);
        let has_alt = has_alt(&root);
        let has_backref = has_backref(&root);
        Ok(Self {
            class_syntax: p.class_syntax,
            no_sub: opts.no_sub,
            strays: p.strays,
            root,
            ngroups: p.ngroups,
            icase: opts.icase,
            reg_newline: opts.reg_newline,
            // The RUN's delimiter, which is the one the split is on -- a part
            // compiled before `-z` still runs in NUL segments.
            segment: opts.reg_newline.map(|a| a.sep).filter(|sep| *sep != b'\n'),
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

    /// As `search_subst`, for a regex GNU would RECOMPILE before this call.
    ///
    /// An ADDRESS regex compiles with `RE_NO_SUB` (sed/regexp.c:87, from
    /// `needed_sub == 0` at compile.c:927), and the first use that wants
    /// registers -- which is only `s//…/` reusing one -- frees the pattern and
    /// the dfa and compiles it again (regexp.c:198-211). That second compile
    /// reads `buffer_delimiter` as it stands at RUN time, so BOTH halves of the
    /// flag agree there: `newline_anchor` follows the separator, and the filter
    /// built from the same byte has nothing left to disagree with.
    pub fn search_subst_recompiled(
        &self,
        hay: &[u8],
        from: usize,
    ) -> Result<Option<Captures>, Error> {
        let reading = Reading {
            reg_newline: self.reg_newline.map(|a| Anchor {
                sep: a.sep,
                newline_anchor: a.sep == b'\n',
            }),
            ..self.reading(true)
        };
        self.scan_reading(hay, from, None, OnBudget::Fail, reading, &mut 0)
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

    /// The length of the longest match ANCHORED at `at` within `hay[..limit]`,
    /// which is glibc's `re_match` under `not_eol` — the one primitive `grep -w`'s
    /// shrink needs (grep/src/dfasearch.c:540). `None` is "no match starts there";
    /// `Some(0)` is the empty match, which GNU's CALLER rejects rather than the
    /// primitive, so this does not fold the two together.
    ///
    /// Cutting the haystack is what makes the answer a SHORTER match rather than
    /// another one, and it is also why `not_eol` is set: the cut end is not the
    /// line's, so `$` must not hold at it.
    ///
    /// `match_from` rather than a scan: a scan would try every later start and be
    /// filtered back to this one, which answers the same and costs the window's
    /// length each time. The shrink runs inside a loop already linear in the line,
    /// so that second factor is what exhausts the shared budget — `grep -o -w
    /// '\.*a'` over 600 dots refused where GNU answers.
    pub fn match_anchored(
        &self,
        hay: &[u8],
        limit: usize,
        at: usize,
        steps: &mut u64,
    ) -> Result<Option<usize>, Error> {
        let Some(hay) = hay.get(..limit) else {
            return Ok(None);
        };
        if at > hay.len() {
            return Ok(None);
        }
        let reading = Reading {
            not_eol: true,
            ..self.reading(false)
        };
        let found = self.match_from(hay, at, steps, None, OnBudget::Fail, reading)?;
        Ok(found.map(|c| c.end() - at))
    }

    /// `search`, spending a budget the CALLER owns. `grep -o -w` slides its start
    /// across the line and searches again from each, and a fresh budget for every
    /// search would multiply the worst case by the line's length — the same
    /// argument that gives one scan one budget rather than one per start.
    pub fn search_budgeted(
        &self,
        hay: &[u8],
        from: usize,
        steps: &mut u64,
    ) -> Result<Option<Captures>, Error> {
        self.scan_reading(hay, from, None, OnBudget::Fail, self.reading(false), steps)
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
        self.scan_reading(hay, from, filter, on_budget, self.reading(subst), &mut 0)
    }

    /// `scan` under an explicit reading, which is what lets a recompiled regex
    /// be scanned without a second compiled copy of it.
    fn scan_reading(
        &self,
        hay: &[u8],
        from: usize,
        filter: Option<&Filter<'_>>,
        on_budget: OnBudget,
        reading: Reading,
        steps: &mut u64,
    ) -> Result<Option<Captures>, Error> {
        // GNU's dfa prefilter. `dfasyntax` takes its end-of-line from the
        // delimiter as it stood at COMPILE time (sed/regexp.c:130), so where
        // that differs from the run's the dfa knows the newline and not the
        // NUL -- and a buffer it rejects is no match AT ALL, even where the
        // NUL anchor would have matched. `veto_reading` is that reading; GNU
        // runs it only at `buf_start_offset == 0` (regexp.c:280), and so a `g`
        // loop's later searches are unfiltered.
        if from == 0 {
            if let Some(veto) = self.veto_reading(reading) {
                // A filter that could not DECIDE must not reject: GNU's dfa is
                // linear and always answers, so an exhausted budget here is an
                // artefact of filtering with a backtracker rather than an
                // answer about the pattern.
                //
                // It spends a COPY: what the filter costs is not the caller's,
                // GNU's dfa pass being linear where this one backtracks. The copy
                // starts at the caller's spend rather than at zero so the pair
                // still cannot exceed a bounded multiple of one budget.
                let mut filter_steps = *steps;
                if let Ok(None) =
                    self.scan_once(hay, 0, None, OnBudget::Existence, veto, &mut filter_steps)
                {
                    return Ok(None);
                }
            }
        }
        // The retry rewinds to what THIS call started with rather than to zero, so
        // "a bounded multiple of the budget" stays true of a call spending a
        // budget it does not own.
        let spent = *steps;
        match self.scan_once(hay, from, filter, on_budget, reading, steps) {
            Err(e) => match on_budget == OnBudget::Fail && !self.has_alt {
                // One retry, so the worst case stays a bounded multiple of the budget.
                true => {
                    *steps = spent;
                    self.scan_once(hay, from, filter, OnBudget::Existence, reading, steps)
                }
                false => Err(e),
            },
            found => found,
        }
    }

    fn reading(&self, subst: bool) -> Reading {
        Reading {
            reg_newline: self.reg_newline,
            segment: self.segment,
            subst,
            approx_backref: false,
            not_eol: false,
        }
    }

    /// The prefilter's reading, when there is one to disagree with: the compiled
    /// newline alone, and no segment, which is all the dfa was built to know.
    ///
    /// A BACKREFERENCE is APPROXIMATED rather than enforced or skipped, and
    /// both of the simpler answers are wrong in a measurable direction.
    /// Filtering with the exact backreference rejects where GNU accepts --
    /// `N;s/^\(.\)\1*$/</Mg` over `\na\0b\n` rewrites both bytes there. Not
    /// filtering at all accepts where GNU rejects: `dfaexec`'s `backref` flag
    /// only says a HIT needs verifying, so a MISS still returns 0, and
    /// `N;/^c\(x\)\1/Mp` over `a\nb\0cxx` prints nothing in GNU. So the pass
    /// reads a backreference as the dfa does -- see `Reading::approx_backref`.
    fn veto_reading(&self, reading: Reading) -> Option<Reading> {
        let a = reading.reg_newline?;
        // `segment` is not READ on this path -- `consumable` and `buf_anchor`
        // both consult it only for a substitution or a backreference, and the
        // pass is neither -- so `None` states the intent rather than an
        // observable, and no case can pin it.
        (a.newline_anchor && a.sep != b'\n').then_some(Reading {
            reg_newline: Some(Anchor {
                sep: b'\n',
                newline_anchor: true,
            }),
            segment: None,
            subst: false,
            approx_backref: true,
            not_eol: false,
        })
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
        reading: Reading,
        steps: &mut u64,
    ) -> Result<Option<Captures>, Error> {
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
                if let Some(caps) = self.match_from(hay, at, steps, filter, on_budget, reading)? {
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

    /// `is_match` for a regex GNU has already RECOMPILED -- see
    /// `search_subst_recompiled`. The recompile MUTATES the object, so every
    /// later use reads the run's delimiter, an address's included.
    pub fn is_match_recompiled(&self, hay: &[u8]) -> Result<bool, Error> {
        let reading = Reading {
            reg_newline: self.reg_newline.map(|a| Anchor {
                sep: a.sep,
                newline_anchor: a.sep == b'\n',
            }),
            ..self.reading(false)
        };
        Ok(self
            .scan_reading(hay, 0, None, OnBudget::Existence, reading, &mut 0)?
            .is_some())
    }

    /// Does some match cover `hay` exactly? Used by `grep -x`, which cannot be
    /// expressed by wrapping the pattern in `^\(…\)$` without renumbering the
    /// backreferences. Explores the whole space, so a match reaching the end is
    /// seen even when a greedier one does not.
    pub fn matches_whole(&self, hay: &[u8]) -> Result<bool, Error> {
        let mut steps = 0u64;
        match self.match_from(
            hay,
            0,
            &mut steps,
            None,
            OnBudget::Fail,
            self.reading(false),
        )? {
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
        reading: Reading,
    ) -> Result<Option<Captures>, Error> {
        let mut st = State {
            hay,
            icase: self.icase,
            reg_newline: reading.reg_newline,
            segment: reading.segment,
            subst: reading.subst,
            // Reading a backreference as the dfa does means reading it as no
            // backreference at all, so the confinement one brings with it
            // (`consumable`, `buf_anchor`) is not this scan's either.
            has_backref: self.has_backref && !reading.approx_backref,
            approx_backref: reading.approx_backref,
            not_eol: reading.not_eol,
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
    reg_newline: Option<Anchor>,
    /// Compile-time: the record separator, when it is not a newline. This is the
    /// half of `M` libc cannot express, so GNU splits on it itself.
    segment: Option<u8>,
    /// Per CALL: is this a SUBSTITUTION? It is confined to one segment, and its
    /// buffer anchors follow only a segment; an address sees the whole pattern space
    /// and its buffer anchors follow the separator. See `Regex::search_subst`.
    subst: bool,
    /// See `Regex::has_backref`: it confines an address as a substitution already is.
    has_backref: bool,
    /// See `Reading::approx_backref`: the prefilter reads a backreference as the
    /// dfa does, which is not what the pattern means.
    approx_backref: bool,
    /// See `Reading::not_eol`: this haystack's end is not the line's, so `$`
    /// must not hold there.
    not_eol: bool,
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
                .is_some_and(|a| self.byte(pos.wrapping_sub(1)).is_some_and(|b| a.holds(b)))
    }

    fn at_eol(&self, pos: usize) -> bool {
        (!self.not_eol && pos == self.hay.len())
            || self
                .reg_newline
                .is_some_and(|a| self.byte(pos).is_some_and(|b| a.holds(b)))
    }

    /// Which separator moves ``\` `` and `\'`, which is not the same for both callers.
    /// In an ADDRESS they are indistinguishable from `^`/`$` and move to the record
    /// separator with them. In a SUBSTITUTION only a segment moves them: `sed -n
    /// 'N;s/\`a/X/Mg'` over `a\na` rewrites only the first `a`, where `/\`a/M`
    /// matches at both, so REG_NEWLINE alone leaves them at the buffer's real ends.
    /// A substitution takes the SEGMENT alone -- never the compile-time newline,
    /// which is glibc's `newline_anchor` and moves only `^`/`$`. So `-z` after
    /// the `-e` splits the two apart: `s/\`x*/</Mg` marks each record where
    /// `s/^x*/</Mg` marks each record AND each line.
    ///
    /// A BACKREFERENCE puts an address on that side too, and it is the same
    /// condition `consumable` uses rather than a second rule: GNU hands such a
    /// pattern to glibc's backref matcher, which sees one segment and its true
    /// ends. Without `-z` there is no segment at all, so the anchors stop
    /// moving entirely -- `N;/\`b\(c\)\1/Mp` over `a\nbcc` finds nothing where
    /// the backref-free `N;/\`bcc/Mp` matches.
    fn buf_anchor(&self) -> Option<Anchor> {
        match self.subst || self.has_backref {
            true => self.segment.map(|sep| Anchor {
                sep,
                newline_anchor: false,
            }),
            false => self.reg_newline,
        }
    }

    fn at_buf_start(&self, pos: usize) -> bool {
        pos == 0
            || self
                .buf_anchor()
                .is_some_and(|a| self.byte(pos.wrapping_sub(1)).is_some_and(|b| a.holds(b)))
    }

    fn at_buf_end(&self, pos: usize) -> bool {
        pos == self.hay.len()
            || self
                .buf_anchor()
                .is_some_and(|a| self.byte(pos).is_some_and(|b| a.holds(b)))
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
fn m(
    st: &mut State,
    node: &Node,
    pos: usize,
    k: &mut dyn FnMut(&mut State, usize) -> bool,
) -> bool {
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
        Node::Never => false,
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
            if st.approx_backref {
                // Nullable, and any byte: the dfa cannot carry a captured group
                // forward, so it admits both. A group that did not participate
                // is not special here either, for the same reason.
                return k(st, pos) || (pos < st.hay.len() && k(st, pos + 1));
            }
            let Some((s, e)) = st.caps.get(*n).copied().flatten() else {
                // A group that did not PARTICIPATE has no text, and GNU makes the
                // reference fail rather than match the empty string it has no claim
                // to: `(x)*\1` matches nothing in a line without an `x`.
                return false;
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
        // A body that matches EMPTY still PARTICIPATES, and that is observable
        // now that an unset group makes a backreference fail: `^(x*)*\1$` selects
        // an empty line because the group ran once and captured the empty string,
        // where never running it leaves `\1` with nothing to name. Once only -- a
        // second iteration could not progress either.
        if count == 0 && may_more && m(st, node, pos, &mut |st, next| next == pos && k(st, pos)) {
            return true;
        }
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

    /// `M` where the two readings of the delimiter agree, which is every
    /// invocation but one: `-z` placed after the `-e` it applies to. That one
    /// has its own test.
    fn m(sep: u8) -> Option<Anchor> {
        Some(Anchor {
            sep,
            newline_anchor: sep == b'\n',
        })
    }

    /// The escapes GNU lints as strays, over every byte, in both dialects.
    /// GOLDEN, measured against GNU grep 3.11 under `LC_ALL=C` by compiling
    /// `\\X` as a single pattern -- single because the lint is gated on the
    /// pattern SET (see `grep::gnu_runs_regex_matcher`) and two patterns can
    /// silence it. A table rather than a rule because the quiet set is not
    /// derivable from anything td-txt knows: it is exactly the bytes GNU's
    /// lexer gives a `case` of its own.
    #[test]
    fn the_stray_escapes_are_gnus() {
        // Escaping one of these means "the literal character", so the
        // backslash is consumed rather than linted.
        const BRE_QUIET: &[u8] = b"$'*.<>BSW[\\]^`bsw|}";
        // An ERE reads its own operators as literal when escaped; a BRE reads
        // the same escapes as the OPERATORS, so it lints them instead.
        const ERE_EXTRA_QUIET: &[u8] = b"()+?{";
        // A backreference, and a BRE's unmatched group.
        const BRE_ERR: &[u8] = b"()123456789";
        const ERE_ERR: &[u8] = b"123456789";
        let ere_quiet = [BRE_QUIET, ERE_EXTRA_QUIET].concat();
        for &ere in &[false, true] {
            let (quiet, err): (&[u8], &[u8]) = match ere {
                false => (BRE_QUIET, BRE_ERR),
                true => (&ere_quiet, ERE_ERR),
            };
            for b in 1..=255u8 {
                // `\<newline>` never reaches this parser: grep splits the
                // pattern list on newlines first, so the backslash always ends
                // up last in a pattern of its own and is `Trailing backslash`.
                // grep-cli pins that; there is nothing here to have an opinion on.
                if b == b'\n' {
                    continue;
                }
                let pat = [b'\\', b];
                let opts = Options {
                    ere,
                    ..Options::default()
                };
                let got = Regex::compile(&pat, opts);
                if err.contains(&b) {
                    assert!(got.is_err(), "ere={ere} 0x{b:02x} should not compile");
                    continue;
                }
                let re = match got {
                    Ok(re) => re,
                    Err(e) => panic!("ere={ere} 0x{b:02x} did not compile: {}", e.msg),
                };
                let want: Vec<u8> = if quiet.contains(&b) { vec![] } else { vec![b] };
                assert_eq!(re.strays, want, "ere={ere} byte 0x{b:02x}");
            }
        }
    }

    /// The three repeat operators a BRE spells with a backslash lint only where
    /// there is nothing to repeat -- GNU's `laststart`, which is the same
    /// condition `strict_repeats` models. An ERE escapes them to mean the
    /// literal, so it never lints them at all.
    #[test]
    fn a_bre_repeat_lints_only_with_nothing_to_repeat() {
        for pat in ["\\?", "\\+", "\\{2\\}"] {
            assert!(!bre(pat).strays.is_empty(), "bare {pat} should lint");
        }
        for pat in ["a\\?", "a\\+", "a\\{2\\}", "a\\?\\?"] {
            assert_eq!(bre(pat).strays, Vec::<u8>::new(), "{pat} should be quiet");
        }
        // The first has nothing to repeat and becomes a literal `{`; the second
        // then HAS something, so one lint and not two.
        assert_eq!(bre("\\{2\\}\\{3\\}").strays, vec![b'{']);
        assert_eq!(bre("\\?\\?").strays, vec![b'?']);
        for pat in ["\\?", "a\\?", "\\{", "a\\{2\\}"] {
            assert_eq!(
                ere(pat).strays,
                Vec::<u8>::new(),
                "ERE {pat} should be quiet"
            );
        }
    }

    /// Strays are reported in PATTERN ORDER, since GNU emits each as the lexer
    /// reaches it.
    #[test]
    fn strays_keep_their_order() {
        assert_eq!(bre("a\\db\\yc").strays, vec![b'd', b'y']);
        assert_eq!(ere("\\y\\d").strays, vec![b'y', b'd']);
    }

    fn bre(pat: &str) -> Regex {
        Regex::compile(pat.as_bytes(), Options::default()).unwrap()
    }

    fn ere(pat: &str) -> Regex {
        Regex::compile(
            pat.as_bytes(),
            Options {
                ere: true,
                ..Options::default()
            },
        )
        .unwrap()
    }

    fn matched(re: &Regex, s: &str) -> bool {
        re.is_match(s.as_bytes()).unwrap()
    }

    /// `Options::default()` is grep's grammar, so every helper above tests only
    /// that one. These two compile sed's.
    fn sed_bre(pat: &str) -> Result<Regex, Error> {
        Regex::compile(
            pat.as_bytes(),
            Options {
                strict_repeats: true,
                glibc_engine: true,
                ..Options::default()
            },
        )
    }

    fn sed_ere(pat: &str) -> Result<Regex, Error> {
        Regex::compile(
            pat.as_bytes(),
            Options {
                ere: true,
                strict_repeats: true,
                glibc_engine: true,
                ..Options::default()
            },
        )
    }

    /// Only a COLLATING ELEMENT names a single character, so only it may bound a
    /// range; a class or equivalence class there is an error on either side.
    #[test]
    fn a_range_may_be_bounded_by_a_collating_element_and_by_nothing_else() {
        // A range, not the three characters that spell one.
        assert!(matched(&bre("[[.a.]-z]"), "m"));
        assert!(!matched(&bre("[[.a.]-z]"), "-"));
        assert!(matched(&bre("[a-[.z.]]"), "m"));
        assert!(matched(&bre("[[.a.]-[.z.]]"), "m"));
        for bad in [
            "[[:alpha:]-z]",
            "[[=a=]-z]",
            "[a-[:digit:]]",
            "[a-[=z=]]",
            "[z-a]",
        ] {
            assert_eq!(
                Regex::compile(bad.as_bytes(), Options::default())
                    .err()
                    .map(|e| e.msg),
                Some("Invalid range end".to_string()),
                "{bad}"
            );
        }
        // A completed range names no single character either.
        for bad in ["[a-b-c]", "[a-z-9]", "[[.a.]-z-x]"] {
            assert_eq!(
                Regex::compile(bad.as_bytes(), Options::default())
                    .err()
                    .map(|e| e.msg),
                Some("Invalid range end".to_string()),
                "{bad}"
            );
        }
        // A `-` last in the list is the literal it is anywhere else.
        assert!(matched(&bre("[a-b-]"), "-"));
        assert!(matched(&bre("[[:alpha:]-]"), "-"));
        assert!(matched(&bre("[[.a.]-]"), "a"));
    }

    /// GNU refuses `[:alpha:]` written without its outer bracket by the SHAPE of
    /// the list: colons at both ends, something between, no sub-expression.
    #[test]
    fn a_class_missing_its_outer_bracket_is_refused_by_shape_not_by_name() {
        for bad in [
            "[:alpha:]",
            "[:bogus:]",
            "[:*:]",
            "[:0:]",
            "[^:a:]",
            "[:a[b:]",
        ] {
            assert_eq!(
                Regex::compile(bad.as_bytes(), Options::default())
                    .err()
                    .map(|e| e.msg),
                Some(CLASS_SYNTAX.to_string()),
                "{bad}"
            );
        }
        // `confusing_bracket_ok` drops the REFUSAL and nothing else: the pattern
        // already parsed as the ordinary bracket expression GNU matches with, so
        // this pins the SET. `[:alpha:]` is {:, a, l, p, h} -- the name is text,
        // which is the whole reason GNU warns about the spelling.
        let ok = Options {
            confusing_bracket_ok: true,
            ..Options::default()
        };
        let re = Regex::compile(b"[:alpha:]", ok).unwrap();
        for m in [":", "a", "l", "p", "h"] {
            assert!(matched(&re, m), "{m} is a member");
        }
        for n in ["b", "z", "0", "-", "["] {
            assert!(!matched(&re, n), "{n} is not");
        }
        // and the refusal is all that moves: a member error still outranks it.
        assert_eq!(
            Regex::compile(b"[:a-:]", ok).err().map(|e| e.msg),
            Regex::compile(b"[:a-:]", Options::default())
                .err()
                .map(|e| e.msg),
        );
        // Only colons between the colons, or a range in the list, and it is an
        // ordinary set again -- both found by fuzzing, not by reading.
        assert!(matched(&bre("[:::]"), ":"));
        assert!(matched(&bre("[::::]"), ":"));
        assert!(matched(&bre("[:a-z:]"), "b"));
        assert!(matched(&bre("[:-z:]"), "b"));
        // Nothing between the colons, or a sub-expression in the list, and it is
        // an ordinary set again.
        assert!(matched(&bre("[::]"), ":"));
        assert!(matched(&bre("[:]"), ":"));
        assert!(matched(&bre("[:alpha]"), "a"));
        assert!(matched(&bre("[:a[.b.]:]"), "a"));
        // A member error outranks the heuristic.
        assert_eq!(
            Regex::compile(b"[:a-:]", Options::default())
                .err()
                .map(|e| e.msg),
            Some("Invalid range end".to_string())
        );
        // And the class-name message is GNU's, which says `name`.
        assert_eq!(
            Regex::compile(b"[[:a:]]", Options::default())
                .err()
                .map(|e| e.msg),
            Some("Invalid character class name".to_string())
        );
        // GNU lints this only once the WHOLE pattern compiles, so any other
        // error -- anywhere, before or after -- outranks it.
        for (pat, msg) in [
            (&b"[:alpha:]\\"[..], "Trailing backslash"),
            (b"[:alpha:]\\(", "Unmatched ( or \\("),
            (b"[:alpha:][z-a]", "Invalid range end"),
            (b"[z-a][:alpha:]", "Invalid range end"),
        ] {
            assert_eq!(
                Regex::compile(pat, Options::default()).err().map(|e| e.msg),
                Some(msg.to_string()),
                "{:?}",
                String::from_utf8_lossy(pat)
            );
        }
    }

    /// The kind of a sub-expression decides a range's HIGH end before its name is
    /// read; at the LOW end it is a member first, so the name is read first.
    #[test]
    fn a_range_end_is_refused_by_kind_where_a_member_is_refused_by_name() {
        for (pat, msg) in [
            (&b"[a-[:bogus:]]"[..], "Invalid range end"),
            (b"[a-[=ab=]]", "Invalid range end"),
            (b"[a-[.ab.]]", "Invalid collation character"),
            (b"[[:bogus:]-a]", "Invalid character class name"),
            (b"[[==]-a]", "Invalid collation character"),
        ] {
            assert_eq!(
                Regex::compile(pat, Options::default()).err().map(|e| e.msg),
                Some(msg.to_string()),
                "{:?}",
                String::from_utf8_lossy(pat)
            );
        }
        // An empty body that runs out is a bad pattern, not an unmatched bracket.
        for (pat, msg) in [
            (&b"["[..], "Invalid regular expression"),
            (b"[^", "Invalid regular expression"),
            (b"[a", "Unmatched [, [^, [:, [., or [="),
            (b"[]", "Unmatched [, [^, [:, [., or [="),
        ] {
            assert_eq!(
                Regex::compile(pat, Options::default()).err().map(|e| e.msg),
                Some(msg.to_string()),
                "{:?}",
                String::from_utf8_lossy(pat)
            );
        }
    }

    /// The three places `strict_repeats` changes an ANSWER rather than a
    /// diagnostic, none of which the grep-shaped helpers above can reach.
    #[test]
    fn seds_grammar_differs_from_greps_where_nothing_is_repeatable() {
        // An operator after an assertion is a literal ANYWHERE in sed's BRE,
        // where grep repeats the assertion past the start of a branch.
        assert!(
            matched(&sed_bre("x\\b*").unwrap(), "x*y"),
            "sed reads a literal star"
        );
        // Both dialects select `x*y` for that one, by different readings and over
        // different spans; `\B\+` is where the two readings disagree outright.
        assert!(
            !matched(&sed_bre("x\\B\\+b").unwrap(), "xb"),
            "a literal + needs one"
        );
        assert!(
            matched(&bre("x\\B\\+b"), "xb"),
            "grep asserts the boundary instead"
        );
        // Stacking, and an interval with nothing to repeat, are sed's refusals.
        assert!(sed_bre("a**").is_err());
        assert!(bre("a**").is_match(b"a").unwrap());
        assert!(sed_bre("\\{2\\}a").is_err());
        // An assertion may not be quantified at all in sed's ERE, and a brace on
        // one is refused before it is read.
        assert!(sed_ere("x\\b*b").is_err());
        assert!(sed_ere("^{a}").is_err());
        // A brace in sed's ERE is always an interval, so an unreadable one is bad
        // CONTENT rather than the literal grep reads. An earlier draft of this
        // test asserted the literal and was wrong: `sed -E 's@a{a}@X@'` is
        // `Invalid content of \{\}`.
        assert_eq!(
            sed_ere("a{a}").err().map(|e| e.msg),
            Some("Invalid content of \\{\\}".to_string())
        );
        assert_eq!(
            sed_ere("a{a").err().map(|e| e.msg),
            Some("Unmatched \\{".to_string())
        );
        // grep reads both as text.
        assert!(matched(&ere("a{a}"), "a{a}"));
        assert!(matched(&ere("a{a"), "a{a"));
    }

    /// sed's ERE is the strict one twice over: a `{` always opens an interval, and
    /// an unmatched `)` is an error where grep reads the character.
    #[test]
    fn seds_ere_refuses_the_braces_and_parens_greps_reads_as_text() {
        // Nothing to repeat wins over whatever the brace contains.
        for pat in ["{", "{2", "{x}", "{}", "{2}"] {
            assert_eq!(
                sed_ere(pat).err().map(|e| e.msg),
                Some("Invalid preceding regular expression".to_string()),
                "{pat}"
            );
        }
        // With something to repeat, whether it CLOSES decides which error.
        assert_eq!(
            sed_ere("a{").err().map(|e| e.msg),
            Some("Unmatched \\{".to_string())
        );
        assert_eq!(
            sed_ere("a{2").err().map(|e| e.msg),
            Some("Unmatched \\{".to_string())
        );
        assert_eq!(
            sed_ere("a{x}").err().map(|e| e.msg),
            Some("Invalid content of \\{\\}".to_string())
        );
        // An ESCAPED brace closes nothing; a real one after it does, and an
        // escaped backslash is skipped whole so the brace behind it counts.
        assert_eq!(
            sed_ere(r"a{x\}").err().map(|e| e.msg),
            Some("Unmatched \\{".to_string())
        );
        assert_eq!(
            sed_ere(r"a{x\}}").err().map(|e| e.msg),
            Some("Invalid content of \\{\\}".to_string())
        );
        assert_eq!(
            sed_ere(r"a{x\\}").err().map(|e| e.msg),
            Some("Invalid content of \\{\\}".to_string())
        );
        assert_eq!(
            sed_ere(r"a{x\\\}").err().map(|e| e.msg),
            Some("Unmatched \\{".to_string())
        );
        // A readable one is still an interval, and `{,}` still reads as `{0,}`.
        assert!(sed_ere("a{1,2}").is_ok());
        assert!(sed_ere("a{,}").is_ok());
        for pat in [")", "a)", "(a))"] {
            assert_eq!(
                sed_ere(pat).err().map(|e| e.msg),
                Some("Unmatched ) or \\)".to_string()),
                "{pat}"
            );
            // `--posix` drops that extension and the character is ordinary again;
            // it does NOT relax the interval rules above.
            let posix = Options {
                ere: true,
                strict_repeats: true,
                posix: true,
                unmatched_rparen_ordinary: true,
                ..Options::default()
            };
            assert!(
                Regex::compile(pat.as_bytes(), posix).is_ok(),
                "--posix {pat}"
            );
        }
        assert!(
            sed_ere("()").is_ok(),
            "an empty group is not an unmatched paren"
        );
        let posix = Options {
            ere: true,
            strict_repeats: true,
            posix: true,
            unmatched_rparen_ordinary: true,
            ..Options::default()
        };
        assert!(
            Regex::compile(b"a{x}", posix).is_err(),
            "--posix leaves intervals alone"
        );
        // The paren field ALONE, which is what POSIXLY_CORRECT hands the
        // compiler: the close-paren goes ordinary and every other extension --
        // `\w`, `\+`, `\|` -- stays, since `RE_NO_GNU_OPS` is BASIC's alone.
        let correct = Options {
            strict_repeats: true,
            unmatched_rparen_ordinary: true,
            ..Options::default()
        };
        assert!(
            Regex::compile(b"a\\)", correct).is_ok(),
            "CORRECT: \\) is ordinary"
        );
        // Anchored, and judged on text only the OPERATOR reading matches: `\w`
        // read as the literal `w` still matches a `w`, so only a non-letter
        // separates the two readings.
        let basic = Options {
            posix: true,
            ..correct
        };
        for (pat, operator_only) in [(&b"^\\w$"[..], "4"), (&b"^a\\+$"[..], "aa")] {
            assert!(matched(
                &Regex::compile(pat, correct).unwrap(),
                operator_only
            ));
            assert!(!matched(
                &Regex::compile(pat, basic).unwrap(),
                operator_only
            ));
        }
        // grep reads every one of them as text, which is why this is a flag and
        // not a fix.
        assert!(matched(&ere("a{"), "a{"));
        assert!(matched(&ere("a{x}"), "a{x}"));
        assert!(matched(&ere(")"), "a)b"));
        assert!(matched(&ere("a)"), "a)b"));
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
            easy.search_existence(hay.as_bytes(), 0)
                .unwrap()
                .unwrap()
                .end(),
        );
    }

    /// sed's `M` is POSIX REG_NEWLINE, and this is the half td-txt was missing: `.`
    /// and a NON-MATCHING bracket list stop at a separator.
    #[test]
    fn reg_newline_keeps_dot_and_negated_lists_off_the_newline() {
        let opts = Options {
            reg_newline: m(b'\n'),
            ..Options::default()
        };
        let dot = Regex::compile(b"c.d", opts).unwrap();
        assert!(dot.search(b"abc\ndef", 0).unwrap().is_none());
        let neg = Regex::compile(b"[^abc]", opts).unwrap();
        // Reaches the `d`, stepping over the newline rather than matching it.
        assert_eq!(neg.search(b"abc\ndef", 0).unwrap().unwrap().start(), 4);
        let star = Regex::compile(b".*", opts).unwrap();
        assert_eq!(star.search(b"abc\ndef", 0).unwrap().unwrap().end(), 3);
        // Without the flag, all three cross it.
        let plain = Options::default();
        assert!(Regex::compile(b"c.d", plain)
            .unwrap()
            .search(b"abc\ndef", 0)
            .unwrap()
            .is_some());
        assert_eq!(
            Regex::compile(b"[^abc]", plain)
                .unwrap()
                .search(b"abc\ndef", 0)
                .unwrap()
                .unwrap()
                .start(),
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
        let nul = Options {
            reg_newline: m(0),
            ..Options::default()
        };
        // Anchors at the NUL, for EITHER caller...
        assert!(Regex::compile(b"^b", nul)
            .unwrap()
            .search(b"a\0b", 0)
            .unwrap()
            .is_some());
        assert!(Regex::compile(b"^b", nul)
            .unwrap()
            .search_subst(b"a\0b", 0)
            .unwrap()
            .is_some());
        assert!(Regex::compile(b"a$", nul)
            .unwrap()
            .search(b"a\0b", 0)
            .unwrap()
            .is_some());
        // ...but not at a newline, which is not the separator here.
        assert!(Regex::compile(b"^d", nul)
            .unwrap()
            .search(b"abc\ndef", 0)
            .unwrap()
            .is_none());
        assert!(Regex::compile(b"c$", nul)
            .unwrap()
            .search(b"abc\ndef", 0)
            .unwrap()
            .is_none());
        // REG_NEWLINE drops the NEWLINE from `.` and a non-matching list in either
        // path, while the SEPARATOR is only out of reach in a substitution.
        assert!(Regex::compile(b"c.d", nul)
            .unwrap()
            .search(b"abc\ndef", 0)
            .unwrap()
            .is_none());
        assert!(Regex::compile(b"a.b", nul)
            .unwrap()
            .search_subst(b"a\0b", 0)
            .unwrap()
            .is_none());
        assert!(Regex::compile(b"a.b", nul)
            .unwrap()
            .search(b"a\0b", 0)
            .unwrap()
            .is_some());
        assert!(Regex::compile(b"[^a]", nul)
            .unwrap()
            .search_subst(b"a\0b", 0)
            .unwrap()
            .is_some_and(|c| c.start() == 2));
        // Without the flag both bytes are ordinary.
        let plain = Options::default();
        assert!(Regex::compile(b"a.b", plain)
            .unwrap()
            .search(b"a\0b", 0)
            .unwrap()
            .is_some());
        assert!(Regex::compile(b"^b", plain)
            .unwrap()
            .search(b"a\0b", 0)
            .unwrap()
            .is_none());
    }

    /// `newline_anchor` set while the separator is a NUL -- sed's `-z` placed
    /// after the `-e` it applies to, where GNU compiled the part against a
    /// newline delimiter and runs it against a NUL one.
    ///
    /// TWO rules, and the second is why this is not simply "more anchors".
    /// `^`/`$` hold at both bytes; but a buffer the pattern cannot match under
    /// the COMPILE-time reading ALONE is no match at all, because the dfa GNU
    /// filters with was built from that delimiter. So the late flag can match
    /// LESS than the early one as well as more.
    #[test]
    fn a_newline_anchor_over_a_nul_separator_holds_at_both_bytes() {
        let both = Options {
            reg_newline: Some(Anchor {
                sep: 0,
                newline_anchor: true,
            }),
            ..Options::default()
        };
        let nul = Options {
            reg_newline: m(0),
            ..Options::default()
        };
        // `N` under `-z` joins two records, one of which holds a newline -- so
        // one buffer carries both bytes and the pair is observable in it.
        let hay = &b"a\nb\0c"[..];
        let at = |o, pat: &[u8], from| {
            Regex::compile(pat, o)
                .unwrap()
                .search(hay, from)
                .unwrap()
                .map(|c| c.start())
        };
        // The newline anchors `b`, the NUL anchors `c`, under ONE pattern.
        assert_eq!(at(both, b"^[bc]", 0), Some(2));
        assert_eq!(
            at(both, b"^[bc]", 3),
            Some(4),
            "the veto is not re-run past 0"
        );
        // The NUL-only reading has the second alone.
        assert_eq!(at(nul, b"^[bc]", 0), Some(4));
        // The veto: `c` follows no newline, so the compile-time reading finds
        // nothing and the NUL anchor never gets to fire -- where the NUL-only
        // reading, which has no dfa to disagree with, matches.
        assert_eq!(at(both, b"^c", 0), None);
        assert_eq!(at(nul, b"^c", 0), Some(4));
        // The BUFFER anchors take the segment alone in a substitution, so the
        // compiled newline does not reach them -- while an address has them as
        // `^`/`$` by another spelling and does.
        assert!(Regex::compile(b"\\`d", both)
            .unwrap()
            .search(b"abc\ndef", 0)
            .unwrap()
            .is_some());
        assert!(Regex::compile(b"\\`d", both)
            .unwrap()
            .search_subst(b"abc\ndef", 0)
            .unwrap()
            .is_none());
        // The exclusion half was never about the separator, so it is unmoved.
        assert!(Regex::compile(b"c.d", both)
            .unwrap()
            .search(b"abc\ndef", 0)
            .unwrap()
            .is_none());
        // And the segment is still what a substitution may not consume.
        assert!(Regex::compile(b"a.b", both)
            .unwrap()
            .search_subst(b"a\0b", 0)
            .unwrap()
            .is_none());
        // A BACKREFERENCE is filtered too, with the reference APPROXIMATED as
        // the dfa approximates it. Enforced, `\1*` could take nothing that
        // keeps `$` on a newline and the first would be rejected; unfiltered,
        // the second would be accepted. Neither simpler answer gives both.
        let br = &b"\na\0b\n"[..];
        assert!(Regex::compile(b"^\\(.\\)\\1*$", both)
            .unwrap()
            .search(br, 0)
            .unwrap()
            .is_some());
        assert!(Regex::compile(b"^c\\(x\\)\\1", both)
            .unwrap()
            .search(b"a\nb\0cxx", 0)
            .unwrap()
            .is_none());
        // ...and with no filter to disagree, the same pattern matches.
        assert!(Regex::compile(b"^c\\(x\\)\\1", nul)
            .unwrap()
            .search(b"a\nb\0cxx", 0)
            .unwrap()
            .is_some());
    }

    /// A separator that is not a newline confines a SUBSTITUTION to the segments
    /// between them, so nothing consumes one there — while the same pattern used as
    /// an ADDRESS matches the whole space, and the newline half of the flag (libc's
    /// REG_NEWLINE, touching only `.` and a non-matching list) applies to both.
    #[test]
    fn a_non_newline_separator_is_consumed_by_nothing_in_a_substitution() {
        let nul = Options {
            reg_newline: m(0),
            ..Options::default()
        };
        // Nothing may cover the separator at index 1 in a substitution. The NUL is
        // written raw because sed decodes `\x00` before a pattern reaches this layer;
        // `[\0]` is here for the fast paths' sake, and `\s` because a NUL is not
        // whitespace either way.
        for pat in [
            &b"\0"[..],
            b"[\0]",
            b"\\W",
            b"\\s",
            b"[^a]",
            b".",
            b"\\(.\\)\\1",
        ] {
            let re = Regex::compile(pat, nul).unwrap();
            assert!(
                re.search_subst(b"a\0a", 0)
                    .unwrap()
                    .is_none_or(|c| c.end() <= 1 || c.start() >= 2),
                "{:?} consumed the separator",
                String::from_utf8_lossy(pat)
            );
        }
        // The ADDRESS path reaches it, which is what makes confinement a property of
        // the call and not of the compiled pattern. Only patterns that can match a
        // NUL at all show it — not `\s`.
        for pat in [&b"\0"[..], b"[\0]", b"\\W", b"[^a]", b"."] {
            let re = Regex::compile(pat, nul).unwrap();
            let hit = re.search(b"a\0a", 0).unwrap();
            assert!(
                hit.is_some(),
                "{:?} found nothing",
                String::from_utf8_lossy(pat)
            );
        }
        assert!(Regex::compile(b"a.a", nul)
            .unwrap()
            .search(b"a\0a", 0)
            .unwrap()
            .is_some());
        assert!(Regex::compile(b"a.a", nul)
            .unwrap()
            .search_subst(b"a\0a", 0)
            .unwrap()
            .is_none());
        // The newline is NOT confined either way: `\W` matches one under the flag,
        // which is what makes the two mechanisms distinguishable.
        let nl = Regex::compile(b"\\W", nul).unwrap();
        assert_eq!(nl.search_subst(b"a\nb", 0).unwrap().unwrap().start(), 1);
        assert_eq!(nl.search(b"a\nb", 0).unwrap().unwrap().start(), 1);
        // ...while `.` and a non-matching list still lose it, in EITHER path.
        assert!(Regex::compile(b"a.b", nul)
            .unwrap()
            .search(b"a\nb", 0)
            .unwrap()
            .is_none());
        assert!(Regex::compile(b"a.b", nul)
            .unwrap()
            .search_subst(b"a\nb", 0)
            .unwrap()
            .is_none());
        // With a NEWLINE separator nothing is confined; REG_NEWLINE alone applies.
        let opts = Options {
            reg_newline: m(b'\n'),
            ..Options::default()
        };
        let w = Regex::compile(b"\\W", opts).unwrap();
        assert_eq!(w.search(b"a\nb", 0).unwrap().unwrap().start(), 1);
        assert_eq!(w.search_subst(b"a\nb", 0).unwrap().unwrap().start(), 1);
    }

    /// The buffer anchors are `^`/`$` to an ADDRESS and true buffer anchors to a
    /// SUBSTITUTION, which is the third thing about `M` this engine had wrong.
    #[test]
    fn the_buffer_anchors_follow_the_separator_only_where_gnu_moves_them() {
        for (sep, hay) in [(b'\n', &b"a\nb"[..]), (0, b"a\0b")] {
            let opts = Options {
                reg_newline: m(sep),
                ..Options::default()
            };
            let (open, close) = (
                Regex::compile(b"\\`b", opts).unwrap(),
                Regex::compile(b"a\\'", opts).unwrap(),
            );
            // An address moves them to the record separator, whatever it is.
            assert!(
                open.search(hay, 0).unwrap().is_some(),
                "sep {sep}: address lost \\`"
            );
            assert!(
                close.search(hay, 0).unwrap().is_some(),
                "sep {sep}: address lost \\'"
            );
            // A substitution moves them only for a SEGMENT, which a newline is not.
            let moved = sep != b'\n';
            assert_eq!(open.search_subst(hay, 0).unwrap().is_some(), moved);
            assert_eq!(close.search_subst(hay, 0).unwrap().is_some(), moved);
        }
        // Without the flag neither caller moves them, and `^` is the contrast that
        // does move: `s/\`a/X/Mg` over `a\na` rewrites one `a`, `s/^a/X/Mg` both.
        let plain = Options::default();
        assert!(Regex::compile(b"\\`b", plain)
            .unwrap()
            .search(b"a\nb", 0)
            .unwrap()
            .is_none());
        let m = Options {
            reg_newline: m(b'\n'),
            ..Options::default()
        };
        assert!(Regex::compile(b"^b", m)
            .unwrap()
            .search_subst(b"a\nb", 0)
            .unwrap()
            .is_some());
    }

    /// A backreference confines an ADDRESS too, which no rule about `M` predicts —
    /// see `Regex::has_backref`.
    #[test]
    fn a_backreference_confines_the_address_path_as_well() {
        let nul = Options {
            reg_newline: m(0),
            ..Options::default()
        };
        let hay = &b"a\0\0b"[..];
        // Nothing crosses the separator once a backref is in the pattern, whether the
        // backref does the crossing or a literal does.
        for pat in [&b"\\(.\\)\\1"[..], b"\\(a\\)\0\\1*"] {
            let re = Regex::compile(pat, nul).unwrap();
            assert!(
                re.search(hay, 0).unwrap().is_none(),
                "{:?} crossed",
                String::from_utf8_lossy(pat)
            );
        }
        // Without one, an address crosses; and a match inside a segment is unaffected.
        assert!(Regex::compile(b"..", nul)
            .unwrap()
            .search(hay, 0)
            .unwrap()
            .is_some());
        assert!(Regex::compile(b"\\(a\\)\0", nul)
            .unwrap()
            .search(hay, 0)
            .unwrap()
            .is_some());
        assert!(Regex::compile(b"\\(b\\)\\1*", nul)
            .unwrap()
            .search(hay, 0)
            .unwrap()
            .is_some());
        // A newline separator has no segment, so a doubled NUL is an ordinary pair.
        let nl = Options {
            reg_newline: m(b'\n'),
            ..Options::default()
        };
        assert!(Regex::compile(b"\\(.\\)\\1", nl)
            .unwrap()
            .search(hay, 0)
            .unwrap()
            .is_some());
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
        let ere = Regex::compile(
            b"^^",
            Options {
                ere: true,
                ..Options::default()
            },
        )
        .unwrap();
        assert!(matched(&ere, "x"));
    }

    /// A BARE `|`/`)` anchors the `$` before it, though neither is an operator in a
    /// BRE. GNU tests the byte after the `$` against the ERE spellings of the two
    /// operators whose BRE spellings (`\|`, `\)`) end a branch, but keeps the length
    /// test those two-byte spellings need -- so the last byte of a pattern does not
    /// reach it, and `x$|` stays the literal a `$` is everywhere else.
    #[test]
    fn a_bare_pipe_or_paren_anchors_the_dollar_before_it() {
        assert!(matched(&bre("x$|"), "ax$|z"));
        assert!(matched(&bre("x$)"), "ax$)z"));
        // One byte further it anchors, so the text it looks like is NOT selected.
        assert!(!matched(&bre("x$||"), "x$||"));
        assert!(!matched(&bre("x$|a"), "x$|a"));
        assert!(!matched(&bre("x$))"), "x$))"));
        // Only those two bytes do it; an ordinary one leaves the `$` alone.
        assert!(matched(&bre("x$az"), "x$az"));
        assert!(matched(&bre("x$$|"), "x$$|"));
        // To grep's dfa the anchor is a true end of line, which `|*` can reach by
        // taking none of the pipe.
        assert!(matched(&bre("x$|*"), "x"));
        assert!(!matched(&bre("x$|*"), "x$"));
        // The count is of BYTES left in the PATTERN, not of the branch: an operator
        // after the bare byte does not hand the literal back.
        assert!(!matched(&bre(r"x$|\|zz"), "x$|"));
        assert!(matched(&bre(r"x$|\|zz"), "zz"));
        assert!(!matched(&bre(r"\(x$|\)"), "x$|"));
        // grep lexes its `-e`/`-f` patterns JOINED by `\n`, so for every one but the
        // last there is a further byte and the literal is gone.
        let joined = Options {
            lex_continues: true,
            ..Options::default()
        };
        assert!(!matched(&Regex::compile(b"x$|", joined).unwrap(), "ax$|z"));
        assert!(!matched(&Regex::compile(b"x$)", joined).unwrap(), "ax$)z"));
    }

    /// sed reaches that anchor through glibc, which satisfies a mid-branch one
    /// never, so the branch carrying it is dead where grep selects a line.
    #[test]
    fn glibc_leaves_the_mid_branch_dollar_unsatisfiable() {
        let dead = sed_bre("x$|*").unwrap();
        assert!(!matched(&dead, "x"));
        assert!(!matched(&dead, "x$"));
        // Only that branch dies; a sibling still matches.
        let alt = sed_bre(r"x$|a\|zz").unwrap();
        assert!(matched(&alt, "zz"));
        assert!(!matched(&alt, "x$|a"));
        // The rule is the BRE's: an ERE `$` anchors anywhere and `|` alternates.
        assert!(matched(&sed_ere("x$|a").unwrap(), "a"));
    }

    #[test]
    fn case_folding_covers_literals_and_classes() {
        let re = Regex::compile(
            b"[a-z]bc",
            Options {
                icase: true,
                ..Options::default()
            },
        )
        .unwrap();
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

// ---- td-mail adapter ---------------------------------------------------------
//
// Everything above is td-txt's engine, unchanged. Everything below translates
// the Rust-`regex` dialect td-mail's users write into the ERE the engine reads, and
// wraps the byte-oriented API in the `&str` one td-mail calls.

/// The largest user pattern accepted, in bytes. Patterns come from a
/// user-authored rules file and run against untrusted headers; one larger than
/// this is a mistake or an attack, not a rule.
pub const MAX_USER_PATTERN: usize = 4 << 10;

/// A user-authored pattern, compiled for matching against untrusted text.
///
/// `compile` accepts the Rust-`regex` spellings td-mail's rules and documentation
/// use and translates them; see the module header for the exact dialect. The
/// match itself is the engine above: bytes, ASCII, POSIX leftmost-longest, and
/// bounded by a step budget.
#[derive(Clone, Debug)]
pub struct UserRegex {
    re: Regex,
    src: String,
}

impl UserRegex {
    pub fn compile(pattern: &str) -> Result<Self, Error> {
        let (ere, icase) = translate(pattern)?;
        let re = Regex::compile(
            &ere,
            Options {
                ere: true,
                icase,
                ..Options::default()
            },
        )?;
        Ok(Self {
            re,
            src: pattern.to_string(),
        })
    }

    /// The pattern as the USER wrote it, which is what a rule description shows.
    /// The translated ERE is an implementation detail and is never displayed.
    pub fn as_str(&self) -> &str {
        &self.src
    }

    /// Does `text` match?
    ///
    /// A pattern too expensive to decide (the engine's step budget, see
    /// `OnBudget`) counts as NO MATCH: a rule that cannot be evaluated must not
    /// fire, and a mail client cannot wedge on one header. Use `try_is_match`
    /// where the caller can report the refusal instead of swallowing it.
    pub fn is_match(&self, text: &str) -> bool {
        self.try_is_match(text).unwrap_or(false)
    }

    /// `is_match`, surfacing the `too complex` refusal rather than reading it as
    /// no match.
    pub fn try_is_match(&self, text: &str) -> Result<bool, Error> {
        self.re.is_match(text.as_bytes())
    }

    /// Successive non-overlapping matches, left to right.
    ///
    /// The engine reports BYTE spans and nothing constrains a span to land on a
    /// character boundary, so each span is widened outward to the nearest
    /// boundaries: a yielded slice always covers every matched byte, and slicing
    /// can never panic. Iteration stops at the first `too complex` refusal, so a
    /// pathological pattern truncates the result rather than hanging.
    pub fn find_iter<'a>(&'a self, text: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        let mut at = 0usize;
        let mut done = false;
        std::iter::from_fn(move || {
            if done {
                return None;
            }
            let Ok(Some(caps)) = self.re.search(text.as_bytes(), at) else {
                done = true;
                return None;
            };
            let (start, end) = (caps.start(), caps.end());
            // An empty match would otherwise pin the scan in place, and the one
            // at the END of the text has nowhere to advance to: yield it and
            // stop, rather than yield it forever.
            let next = match end > start {
                true => end,
                false => ceil_boundary(text, end.saturating_add(1)),
            };
            done = next <= at;
            at = next;
            match text.get(floor_boundary(text, start)..ceil_boundary(text, end)) {
                Some(s) => Some(s),
                None => {
                    done = true;
                    None
                }
            }
        })
    }
}

/// The largest character boundary at or below `i`.
fn floor_boundary(text: &str, i: usize) -> usize {
    let mut i = i.min(text.len());
    while i > 0 && !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// The smallest character boundary at or above `i`.
fn ceil_boundary(text: &str, i: usize) -> usize {
    let mut i = i.min(text.len());
    while i < text.len() && !text.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// `https?://[^\s<>\]\)"'`]+`, by hand.
///
/// td-mail runs this over every message body it renders, which is the one regex use
/// hot enough to be worth not being a regex at all: the pattern is a literal
/// prefix and a byte class, so a scan decides it without the engine. Equivalence
/// with the compiled pattern is a test, not a comment.
pub fn find_urls(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut at = 0usize;
    while at < bytes.len() {
        match url_at(bytes, at) {
            // Non-overlapping, and a candidate start that fails advances by ONE
            // byte: in `https://<http://x` the first scheme matches nothing (the
            // class rejects `<`) and the SECOND one is the match, which resuming
            // past the failed prefix would miss.
            Some(end) => {
                if let Some(url) = text.get(at..end) {
                    out.push(url);
                }
                at = end;
            }
            None => at = at.saturating_add(1),
        }
    }
    out
}

/// The end of the URL starting at `at`, if one does.
fn url_at(bytes: &[u8], at: usize) -> Option<usize> {
    let rest = bytes.get(at..)?;
    let scheme = if rest.starts_with(b"https://") {
        8
    } else if rest.starts_with(b"http://") {
        7
    } else {
        return None;
    };
    let body = at.saturating_add(scheme);
    let mut end = body;
    while bytes.get(end).is_some_and(|b| !is_url_stop(*b)) {
        end = end.saturating_add(1);
    }
    // The class is `+`: a scheme with nothing after it is not a match.
    (end > body).then_some(end)
}

/// The complement of `[^\s<>\]\)"'`]`, where `\s` is `[[:space:]]`.
fn is_url_stop(b: u8) -> bool {
    matches!(
        b,
        b' ' | 0x09..=0x0d | b'<' | b'>' | b']' | b')' | b'"' | b'\'' | b'`'
    )
}

// ---- translation ---------------------------------------------------------

/// One member of a translated bracket expression.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ClassItem {
    Ch(u8),
    Range(u8, u8),
    /// A POSIX class NAME, e.g. `digit` for `[:digit:]`.
    Named(&'static str),
}

/// What one bracket member parsed to: a single character, which may bound a
/// range, or a set, which may not.
enum ClassMember {
    Ch(u8),
    Set(Vec<ClassItem>),
}

/// Refusal wording. Every one names the byte offset in the pattern AS WRITTEN,
/// which is what td-mail echoes back to whoever wrote the rules file.
fn refuse(what: &str, at: usize) -> Error {
    Error::new(format!(
        "{what} at byte {at} is not supported: patterns are POSIX ERE \
         with GNU extensions and an optional leading (?i)"
    ))
}

fn escape_name(b: u8) -> String {
    match b.is_ascii_graphic() {
        true => format!("\\{}", char::from(b)),
        false => format!("\\x{b:02x}"),
    }
}

/// Translate a user pattern into the ERE the engine reads, and whether a leading
/// `(?i)` asked for case folding.
fn translate(pattern: &str) -> Result<(Vec<u8>, bool), Error> {
    if pattern.len() > MAX_USER_PATTERN {
        return Err(Error::new(format!(
            "pattern is {} bytes, over the {MAX_USER_PATTERN} byte limit",
            pattern.len()
        )));
    }
    let (body, icase) = match pattern.strip_prefix("(?i)") {
        Some(rest) => (rest, true),
        None => (pattern, false),
    };
    let mut t = Translator {
        pat: body.as_bytes(),
        pos: 0,
        // Offsets are reported against the pattern as WRITTEN, so a stripped
        // `(?i)` still counts for its four bytes.
        offset: pattern.len().saturating_sub(body.len()),
        out: Vec::with_capacity(body.len().saturating_add(8)),
        open: Vec::new(),
    };
    t.run()?;
    Ok((t.out, icase))
}

struct Translator<'a> {
    pat: &'a [u8],
    pos: usize,
    offset: usize,
    out: Vec<u8>,
    /// Where each still-open group was written. Balance is checked HERE rather
    /// than left to the engine, whose grep grammar reads some unbalanced parens
    /// as text where Rust-`regex` refuses them: a rules file with a typo in it
    /// must be refused, not quietly read as a literal.
    open: Vec<usize>,
}

impl Translator<'_> {
    fn peek(&self) -> Option<u8> {
        self.pat.get(self.pos).copied()
    }

    fn peek_at(&self, off: usize) -> Option<u8> {
        self.pat.get(self.pos.saturating_add(off)).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let b = self.peek();
        if b.is_some() {
            self.pos = self.pos.saturating_add(1);
        }
        b
    }

    fn eat(&mut self, b: u8) -> bool {
        let hit = self.peek() == Some(b);
        if hit {
            self.pos = self.pos.saturating_add(1);
        }
        hit
    }

    /// The offset to report for whatever starts at `pos`.
    fn here(&self) -> usize {
        self.offset.saturating_add(self.pos)
    }

    fn run(&mut self) -> Result<(), Error> {
        while let Some(b) = self.peek() {
            match b {
                b'\\' => self.escape()?,
                b'[' => self.bracket()?,
                b'(' => self.group()?,
                b'*' | b'+' | b'?' => {
                    self.pos = self.pos.saturating_add(1);
                    self.out.push(b);
                    self.reject_lazy()?;
                }
                b'{' => self.interval()?,
                b')' => {
                    if self.open.pop().is_none() {
                        return Err(Error::new(format!("unmatched `)` at byte {}", self.here())));
                    }
                    self.pos = self.pos.saturating_add(1);
                    self.out.push(b);
                }
                // Everything left is either an operator both dialects spell the
                // same (`. ^ $ | ] }`) or an ordinary byte.
                _ if b.is_ascii() => {
                    self.pos = self.pos.saturating_add(1);
                    self.out.push(b);
                }
                _ => self.multibyte(),
            }
        }
        if let Some(at) = self.open.last() {
            return Err(Error::new(format!(
                "group opened at byte {at} is never closed"
            )));
        }
        Ok(())
    }

    /// A non-ASCII character, GROUPED. The engine matches bytes, so a bare `é+`
    /// would repeat the LAST byte of the character; `(é)+` repeats the
    /// character, which is what was written and what Rust-`regex` does.
    fn multibyte(&mut self) {
        let len = match self.peek() {
            Some(0xc0..=0xdf) => 2,
            Some(0xe0..=0xef) => 3,
            Some(0xf0..=0xf7) => 4,
            // Not reachable from a `&str`, but a lone byte is still coherent.
            _ => 1,
        };
        let end = self.pos.saturating_add(len).min(self.pat.len());
        let seq = self.pat.get(self.pos..end).unwrap_or_default().to_vec();
        self.out.push(b'(');
        self.out.extend_from_slice(&seq);
        self.out.push(b')');
        self.pos = end;
    }

    /// A quantifier may not be followed by `?`: that is Perl's lazy form, and
    /// POSIX repetition is greedy. Reading `a*?` as `a*` would silently change
    /// which text a rule matches, so it is refused instead.
    fn reject_lazy(&mut self) -> Result<(), Error> {
        match self.peek() == Some(b'?') {
            true => Err(refuse("non-greedy quantifier", self.here())),
            false => Ok(()),
        }
    }

    /// `{n}`, `{n,}`, `{n,m}` pass through. A brace opening no interval is the
    /// LITERAL character (GNU grep's reading) where Rust-`regex` refuses it;
    /// accepting more than the old engine did cannot change what an existing
    /// rule matches.
    fn interval(&mut self) -> Result<(), Error> {
        let mut scan = self.pos.saturating_add(1);
        let mut digits = false;
        let mut ok = false;
        while let Some(b) = self.pat.get(scan).copied() {
            match b {
                b'0'..=b'9' => digits = true,
                b',' => {}
                b'}' => {
                    ok = digits;
                    break;
                }
                _ => break,
            }
            scan = scan.saturating_add(1);
        }
        if !ok {
            self.pos = self.pos.saturating_add(1);
            self.out.extend_from_slice(b"\\{");
            return Ok(());
        }
        let end = scan.saturating_add(1);
        let text = self.pat.get(self.pos..end).unwrap_or_default().to_vec();
        self.out.extend_from_slice(&text);
        self.pos = end;
        self.reject_lazy()
    }

    /// `(` and the `(?…` forms. Only `(?:` survives translation: it groups
    /// without capturing, which POSIX spells `(` — td-mail reads no capture groups,
    /// so the numbering that changes is unobservable.
    fn group(&mut self) -> Result<(), Error> {
        let at = self.here();
        self.pos = self.pos.saturating_add(1);
        if self.peek() != Some(b'?') {
            self.open.push(at);
            self.out.push(b'(');
            return Ok(());
        }
        match self.peek_at(1) {
            Some(b':') => {
                self.pos = self.pos.saturating_add(2);
                self.open.push(at);
                self.out.push(b'(');
                Ok(())
            }
            Some(b'=' | b'!') => Err(refuse("lookahead", at)),
            Some(b'<') => match self.peek_at(2) {
                Some(b'=' | b'!') => Err(refuse("lookbehind", at)),
                _ => Err(refuse("named group", at)),
            },
            Some(b'P') => Err(refuse("named group", at)),
            Some(b'#') => Err(refuse("comment group", at)),
            Some(b'i' | b'm' | b's' | b'x' | b'u' | b'U' | b'-') => Err(Error::new(format!(
                "inline flag group at byte {at} is not supported: only a \
                 LEADING (?i) is, and this one does not start the pattern"
            ))),
            _ => Err(refuse("group extension `(?`", at)),
        }
    }

    /// An escape outside a bracket expression.
    fn escape(&mut self) -> Result<(), Error> {
        let at = self.here();
        self.pos = self.pos.saturating_add(1);
        let Some(b) = self.bump() else {
            return Err(Error::new(format!("trailing backslash at byte {at}")));
        };
        match b {
            // The Perl shorthands, as their ASCII POSIX classes. Rust-`regex`
            // reads these as UNICODE classes; see the module header.
            b'd' => self.out.extend_from_slice(b"[0-9]"),
            b'D' => self.out.extend_from_slice(b"[^0-9]"),
            b's' => self.out.extend_from_slice(b"[[:space:]]"),
            b'S' => self.out.extend_from_slice(b"[^[:space:]]"),
            // GNU's own extensions, which the engine already reads.
            b'w' | b'W' | b'b' | b'B' => {
                self.out.push(b'\\');
                self.out.push(b);
            }
            // An escaped operator is the literal in both dialects.
            b'.' | b'[' | b']' | b'(' | b')' | b'|' | b'+' | b'?' | b'*' | b'{' | b'}' | b'^'
            | b'$' | b'\\' => {
                self.out.push(b'\\');
                self.out.push(b);
            }
            // Not operators here, so a backslash would be a stray: emit the
            // character alone.
            b'/' | b'-' => self.out.push(b),
            b't' => self.out.push(b'\t'),
            b'n' => self.out.push(b'\n'),
            b'r' => self.out.push(b'\r'),
            b'u' => return Err(refuse("`\\u` escape", at)),
            b'x' => return Err(refuse("`\\x` escape", at)),
            b'p' | b'P' => return Err(refuse("Unicode property class", at)),
            b'1'..=b'9' => return Err(refuse("backreference", at)),
            _ => return Err(refuse(&format!("escape `{}`", escape_name(b)), at)),
        }
        Ok(())
    }

    /// A bracket expression in the Rust-`regex` flavour, re-emitted as POSIX.
    ///
    /// The flavours disagree about escapes and about placement: `\]` is how Rust
    /// spells a literal `]`, where POSIX has no escape inside a list at all and
    /// spells the same character by putting it FIRST. Rather than reorder, every
    /// member carrying a positional rule (`] ^ - [`) is emitted as the collating
    /// element `[.X.]`, which carries none.
    fn bracket(&mut self) -> Result<(), Error> {
        let open = self.here();
        self.pos = self.pos.saturating_add(1);
        let negated = self.eat(b'^');
        let mut items: Vec<ClassItem> = Vec::new();
        loop {
            let Some(b) = self.peek() else {
                return Err(Error::new(format!(
                    "character class opened at byte {open} is never closed"
                )));
            };
            if b == b']' {
                self.pos = self.pos.saturating_add(1);
                break;
            }
            if b == b'&' && self.peek_at(1) == Some(b'&') {
                return Err(refuse("character class set operation `&&`", self.here()));
            }
            let at = self.here();
            match self.class_member()? {
                ClassMember::Set(set) => items.extend(set),
                ClassMember::Ch(lo) => {
                    // A `-` is a range only BETWEEN two members: the `-` of
                    // `[a-]` and of `[-a]` is the literal in both dialects.
                    if self.peek() == Some(b'-') && !matches!(self.peek_at(1), None | Some(b']')) {
                        self.pos = self.pos.saturating_add(1);
                        let ClassMember::Ch(hi) = self.class_member()? else {
                            return Err(Error::new(format!(
                                "a character class shorthand cannot bound the range at byte {at}"
                            )));
                        };
                        if hi < lo {
                            return Err(Error::new(format!(
                                "range at byte {at} ends before it starts"
                            )));
                        }
                        items.push(ClassItem::Range(lo, hi));
                        continue;
                    }
                    items.push(ClassItem::Ch(lo));
                }
            }
        }
        if items.is_empty() {
            return Err(Error::new(format!(
                "character class at byte {open} is empty"
            )));
        }
        self.out.push(b'[');
        if negated {
            self.out.push(b'^');
        }
        for item in &items {
            match *item {
                ClassItem::Ch(c) => push_class_char(&mut self.out, c),
                ClassItem::Range(lo, hi) => {
                    push_class_char(&mut self.out, lo);
                    self.out.push(b'-');
                    push_class_char(&mut self.out, hi);
                }
                ClassItem::Named(name) => {
                    self.out.extend_from_slice(b"[:");
                    self.out.extend_from_slice(name.as_bytes());
                    self.out.extend_from_slice(b":]");
                }
            }
        }
        self.out.push(b']');
        Ok(())
    }

    /// One member of a bracket expression.
    fn class_member(&mut self) -> Result<ClassMember, Error> {
        let at = self.here();
        let Some(b) = self.bump() else {
            return Err(Error::new(format!(
                "character class at byte {at} is never closed"
            )));
        };
        match b {
            b'[' if self.peek() == Some(b':') => self.named_class(at),
            b'[' if matches!(self.peek(), Some(b'.' | b'=')) => {
                Err(refuse("collating element", at))
            }
            b'\\' => self.class_escape(at),
            // A byte-wise engine reads a multibyte character inside a list as
            // its BYTES, which would match a fragment of some OTHER character.
            // That is a wrong answer rather than a missing feature.
            _ if !b.is_ascii() => Err(Error::new(format!(
                "non-ASCII character inside the character class at byte {at} is \
                 not supported: matching is byte-wise"
            ))),
            _ => Ok(ClassMember::Ch(b)),
        }
    }

    /// `[:name:]` inside a bracket expression, positioned after its `[`.
    fn named_class(&mut self, at: usize) -> Result<ClassMember, Error> {
        self.pos = self.pos.saturating_add(1);
        let start = self.pos;
        while self.peek().is_some_and(|b| b != b':') {
            self.pos = self.pos.saturating_add(1);
        }
        let name = self.pat.get(start..self.pos).unwrap_or_default().to_vec();
        if !self.eat(b':') || !self.eat(b']') {
            return Err(Error::new(format!(
                "character class name at byte {at} is never closed"
            )));
        }
        let named = |n: &'static str| Ok(ClassMember::Set(vec![ClassItem::Named(n)]));
        match name.as_slice() {
            b"alpha" => named("alpha"),
            b"digit" => named("digit"),
            b"alnum" => named("alnum"),
            b"upper" => named("upper"),
            b"lower" => named("lower"),
            b"space" => named("space"),
            b"blank" => named("blank"),
            b"punct" => named("punct"),
            b"print" => named("print"),
            b"graph" => named("graph"),
            b"cntrl" => named("cntrl"),
            b"xdigit" => named("xdigit"),
            // Two Rust-`regex` names the engine does not have.
            b"word" => Ok(ClassMember::Set(vec![
                ClassItem::Named("alnum"),
                ClassItem::Ch(b'_'),
            ])),
            b"ascii" => Ok(ClassMember::Set(vec![ClassItem::Range(0, 0x7f)])),
            _ => Err(Error::new(format!(
                "unknown character class name `{}` at byte {at}",
                String::from_utf8_lossy(&name)
            ))),
        }
    }

    /// An escape INSIDE a bracket expression, positioned after its backslash.
    fn class_escape(&mut self, at: usize) -> Result<ClassMember, Error> {
        let Some(b) = self.bump() else {
            return Err(Error::new(format!("trailing backslash at byte {at}")));
        };
        match b {
            b'd' => Ok(ClassMember::Set(vec![ClassItem::Named("digit")])),
            b's' => Ok(ClassMember::Set(vec![ClassItem::Named("space")])),
            b'w' => Ok(ClassMember::Set(vec![
                ClassItem::Named("alnum"),
                ClassItem::Ch(b'_'),
            ])),
            // A POSIX list has no room for a negated MEMBER: `[^…]` negates the
            // whole list. Refused rather than approximated.
            b'D' | b'S' | b'W' => Err(Error::new(format!(
                "negated shorthand `{}` inside a character class at byte {at} is \
                 not supported: negate the whole class instead",
                escape_name(b)
            ))),
            b't' => Ok(ClassMember::Ch(b'\t')),
            b'n' => Ok(ClassMember::Ch(b'\n')),
            b'r' => Ok(ClassMember::Ch(b'\r')),
            b'.' | b'[' | b']' | b'(' | b')' | b'|' | b'+' | b'?' | b'*' | b'{' | b'}' | b'^'
            | b'$' | b'\\' | b'/' | b'-' => Ok(ClassMember::Ch(b)),
            b'u' => Err(refuse("`\\u` escape", at)),
            b'x' => Err(refuse("`\\x` escape", at)),
            b'p' | b'P' => Err(refuse("Unicode property class", at)),
            _ => Err(refuse(&format!("escape `{}`", escape_name(b)), at)),
        }
    }
}

/// One character of a bracket expression. `] ^ - [` each carry a POSIX
/// positional rule; the collating element spells any of them anywhere.
fn push_class_char(out: &mut Vec<u8>, c: u8) {
    if matches!(c, b']' | b'^' | b'-' | b'[') {
        out.extend_from_slice(b"[.");
        out.push(c);
        out.extend_from_slice(b".]");
        return;
    }
    out.push(c);
}

#[cfg(test)]
mod user_tests {
    use super::*;

    fn compiled(pat: &str) -> UserRegex {
        match UserRegex::compile(pat) {
            Ok(re) => re,
            Err(e) => panic!("{pat} did not compile: {}", e.msg),
        }
    }

    /// The ERE a user pattern translates to, for the cases where the exact
    /// output is the contract rather than the match it produces.
    fn ere(pat: &str) -> String {
        match translate(pat) {
            Ok((out, _)) => String::from_utf8_lossy(&out).into_owned(),
            Err(e) => panic!("{pat} did not translate: {}", e.msg),
        }
    }

    fn icase(pat: &str) -> bool {
        match translate(pat) {
            Ok((_, icase)) => icase,
            Err(e) => panic!("{pat} did not translate: {}", e.msg),
        }
    }

    /// The refusal message for a pattern that must not compile.
    fn refused(pat: &str) -> String {
        match UserRegex::compile(pat) {
            Ok(_) => panic!("{pat} compiled and should not have"),
            Err(e) => e.msg,
        }
    }

    /// Every pattern td-mail's `rules.rs` tests and its rules documentation write,
    /// with the outcome the `regex` crate gives. GOLDEN against that crate's
    /// documented semantics: unanchored search, greedy repetition, `(?i)` for
    /// case folding, `\.` for a literal dot.
    #[test]
    fn the_rules_corpus_matches_what_the_crate_matched() {
        let cases: &[(&str, &str, bool)] = &[
            // rules.toml, as the documentation writes them.
            ("newsletter@", "Weekly <newsletter@example.com>", true),
            ("newsletter@", "News <news@example.com>", false),
            (r"boss@example\.com", "boss@example.com", true),
            // The escaped dot is a dot and nothing else.
            (r"boss@example\.com", "boss@exampleXcom", false),
            ("(?i)urgent", "URGENT: the roof", true),
            ("(?i)urgent", "not important", false),
            (r"\[ALERT\]", "[ALERT] disk full", true),
            (r"\[ALERT\]", "ALERT disk full", false),
            ("dev-team@", "To: dev-team@example.com", true),
            ("boss@", "From: peon@example.com", false),
            ("spam", "spam", true),
            ("spam", "unsure", false),
            // rules.rs's own test fixtures.
            ("alice@", "Alice <alice@example.com>", true),
            ("alice@", "Bob <bob@example.com>", false),
            ("bob@", "Bob <bob@example.com>", true),
            ("charlie@", "Alice <alice@example.com>", false),
            ("dev", "dev-team@example.com", true),
            ("test", "Test Subject", false),
            ("test", "a test subject", true),
            ("Test", "Test Subject", true),
            ("(?i)test", "TEST SUBJECT", true),
            ("(?i)test", "Test Subject", true),
            ("anything", "anything at all", true),
            (".*", "", true),
            (".*", "whatever", true),
            // A header value scored by the spam classifier.
            ("^[5-9]", "5.5", true),
            ("^[5-9]", "4.9", false),
            ("^[5-9]", "15", false),
            // config.rs's two settings and their defaults.
            ("^INBOX$", "INBOX", true),
            ("^INBOX$", "INBOX/Alerts", false),
            ("^INBOX$", "MY INBOX", false),
            ("^$", "", true),
            ("^$", "x", false),
            (r"(?i)me@example\.com", "To: ME@Example.com", true),
            (r"(?i)me@example\.com", "To: you@example.com", false),
            (
                r"(?i)(timmy@example\.com|me@work\.com)",
                "To: Timmy <TIMMY@EXAMPLE.COM>",
                true,
            ),
            (
                r"(?i)(timmy@example\.com|me@work\.com)",
                "To: Someone <someone@work.com>",
                false,
            ),
        ];
        for (pat, text, want) in cases {
            let re = compiled(pat);
            assert_eq!(re.is_match(text), *want, "{pat} against {text:?}");
        }
    }

    /// The shapes the rules documentation teaches beyond the fixtures: an
    /// alternation, a class with a repeat, and a word boundary.
    #[test]
    fn the_documented_shapes_behave_as_the_crate_did() {
        let cases: &[(&str, &str, bool)] = &[
            ("a|b", "b", true),
            ("a|b", "c", false),
            (r"[a-z]+@example\.com", "alice@example.com", true),
            (r"[a-z]+@example\.com", "@example.com", false),
            (r"\bfoo\b", "a foo b", true),
            (r"\bfoo\b", "foobar", false),
            (r"\Bfoo", "foobar", false),
            (r"\Bfoo", "barfoo", true),
            (r"^\d{3}-\d{4}$", "555-1234", true),
            (r"^\d{3}-\d{4}$", "5555-1234", false),
            (r"\w+", "_a1", true),
            (r"^\W+$", "_a1", false),
            (r"\s", "a b", true),
            (r"^\S+$", "a b", false),
            (r"^\D+$", "abc", true),
            (r"^\D+$", "ab1", false),
            ("(?:ab)+c", "ababc", true),
            ("(?:ab)+c", "abbc", false),
        ];
        for (pat, text, want) in cases {
            let re = compiled(pat);
            assert_eq!(re.is_match(text), *want, "{pat} against {text:?}");
        }
    }

    /// The Perl shorthands become their ASCII POSIX classes, and GNU's own
    /// extensions are handed to the engine untouched.
    #[test]
    fn the_shorthands_translate_to_posix_classes() {
        assert_eq!(ere(r"\d+"), "[0-9]+");
        assert_eq!(ere(r"\D"), "[^0-9]");
        assert_eq!(ere(r"\s\S"), "[[:space:]][^[:space:]]");
        assert_eq!(ere(r"\w\W\b\B"), r"\w\W\b\B");
    }

    /// An escaped operator stays escaped; the two escapes that are not
    /// operators here lose the backslash rather than becoming a stray.
    #[test]
    fn escaped_literals_pass_through() {
        assert_eq!(
            ere(r"\.\[\]\(\)\|\+\?\*\{\}\^\$\\"),
            r"\.\[\]\(\)\|\+\?\*\{\}\^\$\\"
        );
        assert_eq!(ere(r"\/\-"), "/-");
        // And the literal really is literal.
        assert!(compiled(r"a\.c").is_match("a.c"));
        assert!(!compiled(r"a\.c").is_match("abc"));
        assert!(compiled(r"a\+").is_match("a+"));
        assert!(compiled(r"\$5").is_match("costs $5"));
        assert!(compiled(r"a\/b").is_match("a/b"));
    }

    #[test]
    fn control_escapes_become_the_byte() {
        assert_eq!(ere(r"\t\n\r"), "\t\n\r");
        assert!(compiled(r"a\tb").is_match("a\tb"));
        assert!(!compiled(r"a\tb").is_match("a b"));
    }

    /// `(?i)` is a leading flag and nothing else: it is consumed by the
    /// translator, so no `(?` reaches the engine.
    #[test]
    fn only_a_leading_icase_flag_is_accepted() {
        assert!(icase("(?i)abc"));
        assert_eq!(ere("(?i)abc"), "abc");
        assert!(!icase("abc"));
        // Anywhere else it names its position.
        let msg = refused("abc(?i)def");
        assert!(msg.contains("byte 3"), "{msg}");
        assert!(msg.contains("inline flag"), "{msg}");
        // Including a second one.
        assert!(refused("(?i)(?i)x").contains("byte 4"));
        // And a flag that is not `i` at all.
        assert!(refused("(?s)x").contains("inline flag"));
        assert!(refused("(?is)x").contains("inline flag"));
    }

    /// A bracket expression is re-spelled, not passed through: Rust escapes a
    /// member, POSIX places it, and the collating element is the one spelling
    /// with no placement rule.
    #[test]
    fn a_bracket_expression_is_respelled_for_posix() {
        // td-mail's URL class, character for character.
        assert_eq!(ere(r#"[^\s<>\]\)"'`]+"#), "[^[:space:]<>[.].])\"'`]+");
        assert_eq!(ere("[a-z0-9_-]"), "[a-z0-9_[.-.]]");
        assert_eq!(ere(r"[\^\]\[-]"), "[[.^.][.].][.[.][.-.]]");
        assert_eq!(ere(r"[\w]"), "[[:alnum:]_]");
        assert_eq!(ere(r"[\d\s]"), "[[:digit:][:space:]]");
        assert_eq!(ere("[[:alpha:]]"), "[[:alpha:]]");
        // Two names Rust-`regex` has that the engine does not.
        assert_eq!(ere("[[:word:]]"), "[[:alnum:]_]");
        assert_eq!(ere("[[:ascii:]]"), "[\u{0}-\u{7f}]");
        // A `-` at either end is the literal, not a range.
        assert_eq!(ere("[-a]"), "[[.-.]a]");
        assert_eq!(ere("[a-]"), "[a[.-.]]");
    }

    /// The re-spelling has to MATCH the same set, not merely parse.
    #[test]
    fn a_respelled_bracket_matches_the_same_set() {
        let re = compiled(r"[\^\]\[\-)]");
        for c in ["^", "]", "[", "-", ")"] {
            assert!(re.is_match(c), "{c} should match");
        }
        for c in ["a", "(", "."] {
            assert!(!re.is_match(c), "{c} should not match");
        }
        let neg = compiled(r"^[^\]a-c]+$");
        assert!(neg.is_match("xyz"));
        assert!(!neg.is_match("x]z"));
        assert!(!neg.is_match("xbz"));
        // A caret that is a member and not a negation.
        assert!(compiled(r"^[\^]$").is_match("^"));
        assert!(!compiled(r"^[\^]$").is_match("a"));
    }

    /// Everything the dialect refuses, refused with its position.
    #[test]
    fn unsupported_syntax_is_refused_by_name_and_position() {
        let cases: &[(&str, &str, &str)] = &[
            (r"\u0041", "byte 0", "\\u"),
            (r"a\x{263A}", "byte 1", "\\x"),
            (r"\p{L}+", "byte 0", "Unicode property"),
            (r"\P{L}+", "byte 0", "Unicode property"),
            ("a(?=b)", "byte 1", "lookahead"),
            ("a(?!b)", "byte 1", "lookahead"),
            ("(?<=a)b", "byte 0", "lookbehind"),
            ("(?<!a)b", "byte 0", "lookbehind"),
            ("(?P<name>a)", "byte 0", "named group"),
            ("(?<name>a)", "byte 0", "named group"),
            ("(?#comment)a", "byte 0", "comment group"),
            ("a*?b", "byte 2", "non-greedy"),
            ("a+?b", "byte 2", "non-greedy"),
            ("a??b", "byte 2", "non-greedy"),
            ("a{2,3}?b", "byte 6", "non-greedy"),
            (r"(a)\1", "byte 3", "backreference"),
            (r"a\q", "byte 1", "escape `\\q`"),
            (r"a\A", "byte 1", "escape `\\A`"),
            (r"a\z", "byte 1", "escape `\\z`"),
            (r"[\S]", "byte 1", "negate the whole class"),
            (r"[\D]", "byte 1", "negate the whole class"),
            (r"[a&&b]", "byte 2", "set operation"),
            (r"[\p{L}]", "byte 1", "Unicode property"),
            ("[é]", "byte 1", "non-ASCII"),
            ("[[:bogus:]]", "byte 1", "unknown character class name"),
            ("[[.a.]]", "byte 1", "collating element"),
            (r"[a-\d]", "byte 1", "cannot bound the range"),
            ("[z-a]", "byte 1", "ends before it starts"),
        ];
        for (pat, at, what) in cases {
            let msg = refused(pat);
            assert!(msg.contains(at), "{pat}: {msg}");
            assert!(msg.contains(what), "{pat}: {msg}");
        }
    }

    /// The malformed patterns td-mail's own tests write, plus the shapes a rules
    /// file gets wrong. The message matters less than the refusal, but it must
    /// not be empty.
    #[test]
    fn a_malformed_pattern_is_refused() {
        for pat in ["[invalid", "(", "a)", "[]", "[^]", r"a\", "(a", "[a-"] {
            let msg = refused(pat);
            assert!(!msg.is_empty(), "{pat} refused with an empty message");
        }
        assert!(refused("[invalid").contains("never closed"));
        assert!(refused("[]").contains("empty"));
        assert!(refused(r"a\").contains("trailing backslash"));
        // Balance is the translator's own check: the engine's grep grammar
        // reads a stray `)` as text, where the crate td-mail used refused it.
        assert!(refused("a)").contains("unmatched `)` at byte 1"));
        assert!(refused("(a").contains("group opened at byte 0"));
        assert!(refused("(?:a").contains("group opened at byte 0"));
    }

    /// A pattern is user input, so its SIZE is checked before anything else.
    #[test]
    fn an_oversized_pattern_is_refused() {
        let ok = "a".repeat(MAX_USER_PATTERN);
        assert!(UserRegex::compile(&ok).is_ok());
        let too_big = "a".repeat(MAX_USER_PATTERN + 1);
        let msg = refused(&too_big);
        assert!(msg.contains("4096 byte limit"), "{msg}");
    }

    /// The engine's step budget, reached: `(a*)*b` over a line of `a`s has no
    /// match and exponentially many ways to fail to find one. It must REPORT,
    /// not hang, and `is_match` must read the report as no match.
    #[test]
    fn a_pathological_pattern_reports_rather_than_hanging() {
        let re = compiled("(a*)*b");
        let hay = "a".repeat(60);
        let err = match re.try_is_match(&hay) {
            Err(e) => e.msg,
            Ok(v) => panic!("answered {v} instead of refusing"),
        };
        assert!(err.contains("too complex"), "{err}");
        // The reporting caller sees the refusal; the rule engine sees no match.
        assert!(!re.is_match(&hay));
        // And the same pattern still answers on input it can decide.
        assert!(re.is_match("aaab"));
    }

    /// An ordinary pattern over ordinary text is nowhere near the budget.
    #[test]
    fn a_realistic_header_is_decided() {
        let re = compiled(r"(?i)^(from|to|cc):\s*[^@]+@[a-z0-9.-]+$");
        assert!(re.try_is_match("From: alice@example.com").unwrap_or(false));
        assert!(!re.try_is_match("Subject: hello").unwrap_or(true));
    }

    #[test]
    fn find_iter_yields_successive_non_overlapping_matches() {
        let re = compiled("ab");
        let hits: Vec<&str> = re.find_iter("abXabab").collect();
        assert_eq!(hits, vec!["ab", "ab", "ab"]);
        // Overlap is impossible: the second `aa` starts after the first ends.
        let re = compiled("aa");
        let hits: Vec<&str> = re.find_iter("aaaa").collect();
        assert_eq!(hits, vec!["aa", "aa"]);
        // Longest at each start, POSIX-style.
        let re = compiled("a+");
        let hits: Vec<&str> = re.find_iter("aa b aaa").collect();
        assert_eq!(hits, vec!["aa", "aaa"]);
        // An anchor still means the TEXT's start, not the resumed scan's.
        let re = compiled("^a");
        let hits: Vec<&str> = re.find_iter("aaa").collect();
        assert_eq!(hits, vec!["a"]);
    }

    /// An empty match must not pin the scan in place.
    #[test]
    fn find_iter_makes_progress_on_an_empty_match() {
        let re = compiled("a*");
        let hits: Vec<&str> = re.find_iter("bab").collect();
        assert_eq!(hits, vec!["", "a", "", ""]);
        let re = compiled("x*");
        let hits: Vec<&str> = re.find_iter("é").collect();
        // One empty match before the character and one after it -- never one
        // BETWEEN its bytes.
        assert_eq!(hits, vec!["", ""]);
    }

    /// The engine reports byte spans; a slice is only ever taken on character
    /// boundaries.
    #[test]
    fn find_iter_stays_on_character_boundaries() {
        let text = "héllo wörld héllo";
        let re = compiled("h[a-z]llo");
        // The `é` is not `[a-z]` byte-wise, so this finds nothing rather than
        // half a character.
        assert_eq!(re.find_iter(text).count(), 0);
        let re = compiled("wörld");
        let hits: Vec<&str> = re.find_iter(text).collect();
        assert_eq!(hits, vec!["wörld"]);
        // A pattern that can land mid-character still yields whole characters.
        let re = compiled(".");
        for hit in re.find_iter("é☃") {
            assert!(!hit.is_empty());
            assert!("é☃".contains(hit), "{hit:?} is not a slice of the text");
        }
    }

    /// A multibyte literal is quantified as a CHARACTER, not as its last byte.
    #[test]
    fn a_multibyte_literal_is_grouped() {
        assert_eq!(ere("é+"), "(é)+");
        assert!(compiled("é+").is_match("éé"));
        assert!(compiled("é?x").is_match("x"));
        assert!(compiled("^é{2}$").is_match("éé"));
        assert!(!compiled("^é{2}$").is_match("é"));
    }

    /// `find_urls` is the compiled pattern's answer, without the engine.
    #[test]
    fn find_urls_agrees_with_the_engine() {
        // td-mail's pattern, character for character (email_view.rs and backend.rs).
        let re = compiled(r#"https?://[^\s<>\]\)"'`]+"#);
        let corpus = [
            "",
            "no url here",
            "see https://example.com/x for more",
            "<https://example.com/a>",
            "(http://example.com/b)",
            "\"http://q.example/c\"",
            "'http://s.example/d'",
            "`http://t.example/e`",
            "http://",
            "https://",
            "http:/x",
            "xhttp://a.example",
            "shttps://x.example",
            // The first scheme matches nothing, so the SECOND is the match.
            "https://<http://second.example/z",
            "a\nhttp://line.example\nb",
            "tabs\thttp://tab.example\tend",
            "http://ünïcode.example/päth ok",
            "brackets http://x.example/]y",
            "http://x.example/p?q=1&r=2#frag.",
            "HTTP://upper.example",
            "https://a.example https://b.example",
            "mail to http://a.example, then http://b.example.",
            "http://a.example/(paren)",
            "nested http://a.example/x<http://b.example/y",
            "httphttp://a.example",
            "https://https://a.example",
        ];
        for text in corpus {
            let want: Vec<&str> = re.find_iter(text).collect();
            assert_eq!(find_urls(text), want, "corpus entry {text:?}");
        }
    }

    /// The scanner's own boundaries, stated rather than only implied by the
    /// oracle above.
    #[test]
    fn find_urls_stops_where_the_class_does() {
        assert_eq!(
            find_urls("<https://a.example/b>"),
            vec!["https://a.example/b"]
        );
        assert_eq!(find_urls("http://"), Vec::<&str>::new());
        assert_eq!(find_urls("HTTP://a.example"), Vec::<&str>::new());
        assert_eq!(
            find_urls("http://a.example/x http://b.example/y"),
            vec!["http://a.example/x", "http://b.example/y"]
        );
        // td-mail trims trailing punctuation itself; the pattern does not.
        assert_eq!(
            find_urls("see http://a.example."),
            vec!["http://a.example."]
        );
    }

    /// Case folding is ASCII, because the engine is bytes. A rule that folded
    /// `É` to `é` under the `regex` crate does not here.
    #[test]
    fn case_folding_is_ascii_only() {
        assert!(compiled("(?i)inbox").is_match("INBOX"));
        assert!(compiled("(?i)[a-z]+").is_match("ABC"));
        assert!(!compiled("(?i)é").is_match("É"));
    }

    /// Two places where the byte engine's answer differs from the `regex`
    /// crate's, pinned so the difference is a decision and not a surprise: `.`
    /// is any BYTE, including a newline, and alternation is leftmost-LONGEST.
    #[test]
    fn the_documented_differences_hold() {
        assert!(compiled("a.b").is_match("a\nb"));
        // The anchors are still the whole text's, as they are in the crate.
        assert!(!compiled("^b$").is_match("a\nb"));
        let re = compiled("x|xy");
        let hits: Vec<&str> = re.find_iter("xy").collect();
        assert_eq!(hits, vec!["xy"], "POSIX takes the longest branch");
    }

    /// An interval passes through; a brace that opens none is the literal
    /// character, which is more than the crate accepted and never less.
    #[test]
    fn intervals_and_stray_braces() {
        assert_eq!(ere("a{2,3}"), "a{2,3}");
        assert_eq!(ere("a{2,}"), "a{2,}");
        assert_eq!(ere("a{"), "a\\{");
        assert!(compiled("^a{2,3}$").is_match("aaa"));
        assert!(!compiled("^a{2,3}$").is_match("a"));
        assert!(compiled(r"a\{b").is_match("a{b"));
        assert!(compiled("{x}").is_match("{x}"));
    }

    /// td-mail prints a rule as `Header =~ /pattern/`, and the pattern it prints is
    /// the one the user wrote -- never the translated ERE.
    #[test]
    fn as_str_is_the_pattern_as_written() {
        let pat = r"(?i)boss@example\.com";
        assert_eq!(compiled(pat).as_str(), pat);
    }
}

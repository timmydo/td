//! Regular expressions for td-mail: a `UserRegex` adapter that reads the
//! Rust-`regex` dialect td-mail's rules files are written in, over td-txt's
//! POSIX engine, the td-regex crate. td-mail compiles user-authored patterns
//! from `rules.toml`, the two `[mail]` config settings, and one hard-coded URL
//! pattern; `find_urls` answers that last one without the engine at all.
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

use td_regex::{Error, Options, Regex};

/// The largest user pattern accepted, in bytes. Patterns come from a
/// user-authored rules file and run against untrusted headers; one larger than
/// this is a mistake or an attack, not a rule.
pub const MAX_USER_PATTERN: usize = 4 << 10;

/// A user-authored pattern, compiled for matching against untrusted text.
///
/// `compile` accepts the Rust-`regex` spellings td-mail's rules and documentation
/// use and translates them; see the module header for the exact dialect. The
/// match itself is td-regex's engine: bytes, ASCII, POSIX leftmost-longest, and
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
    ///
    /// Only the tests read spans: no caller in the client does.
    #[cfg(test)]
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
#[cfg(test)]
fn floor_boundary(text: &str, i: usize) -> usize {
    let mut i = i.min(text.len());
    while i > 0 && !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// The smallest character boundary at or above `i`.
#[cfg(test)]
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

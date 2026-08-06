//! POSIX shell pattern matching (`*`, `?`, `[...]`), shared by `case`, the
//! `${x#pat}` / `${x%pat}` trims and pathname expansion.
//!
//! Patterns arrive as expanded characters, not text, so a quoted `*` stays a
//! literal asterisk: quoting is the one thing a flattened string cannot carry.

use crate::expand::QChar;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unit {
    Lit(char),
    /// `?`
    Any,
    /// `*`
    Star,
    Class {
        negated: bool,
        items: Vec<ClassItem>,
    },
    /// A trailing unquoted `\`, which has nothing to escape. glibc's `fnmatch`
    /// refuses such a pattern outright, so this matches no character and, by
    /// never being consumed, leaves the whole pattern unmatchable.
    Never,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClassItem {
    Ch(char),
    Range(char, char),
    /// `[:alpha:]` and friends.
    Named(String),
}

/// ash hands `fnmatch` a pattern in which QUOTING IS ITSELF a backslash escape
/// (its `preglob`), then lets `fnmatch` read every backslash the same way. So a
/// quoted `*` arrives as `\*` and an unquoted backslash before it escapes THAT
/// backslash, leaving the `*` a wildcard. Flattening the same way is the only
/// shape that gets this right: looking at one `QChar` at a time cannot tell
/// `\` + quoted `*` (literal backslash, then wildcard) from an unquoted `\*`
/// (literal asterisk), and ash distinguishes them.
pub fn preglob(pat: &[QChar]) -> Vec<char> {
    let mut out = Vec::with_capacity(pat.len() * 2);
    for q in pat {
        if q.quoted {
            out.push('\\');
        }
        out.push(q.c);
    }
    out
}

/// True when the pattern can match something other than itself — the cheap test
/// that decides whether a field needs pathname expansion at all. A backslash
/// escapes what follows, so `a\*b` has NO metacharacter and is left alone rather
/// than globbed; ash reaches the same answer the same way.
pub fn has_meta(pat: &[QChar]) -> bool {
    has_meta_chars(&preglob(pat))
}

/// `has_meta` on an already-flattened pattern, for callers that split the
/// `preglob` stream themselves -- pathname expansion, whose components are cut
/// out of it.
pub fn has_meta_chars(chars: &[char]) -> bool {
    let mut i = 0usize;
    while let Some(&c) = chars.get(i) {
        if c == '\\' {
            i += 2;
            continue;
        }
        if matches!(c, '*' | '?') {
            return true;
        }
        // An UNCLOSED `[` is not a metacharacter -- ash's `hasmeta` asks
        // `strchr(p + 1, ']')` and keeps scanning when there is none
        // (ash.c:7760), so `my[file*` globs on the `*` alone and `[\/` is not a
        // pattern at all. Answering yes here would glob a word ash leaves whole,
        // which is only visible once globbing also spends the escapes.
        if c == '[' && chars.get(i + 1..).is_some_and(|r| r.contains(&']')) {
            return true;
        }
        i += 1;
    }
    false
}

pub fn compile(pat: &[QChar]) -> Vec<Unit> {
    compile_chars(&preglob(pat))
}

/// `compile` on an already-flattened pattern. See `has_meta_chars`.
pub fn compile_chars(chars: &[char]) -> Vec<Unit> {
    let mut units = Vec::new();
    let mut i = 0usize;
    while let Some(&c) = chars.get(i) {
        match c {
            // ash's matcher is `fnmatch(pattern, string, 0)`, so a backslash
            // escapes the next character: `\f` is the letter f, `\*` a literal
            // asterisk. Quoted characters carry their own backslash from
            // `preglob`, which is what keeps them literal here.
            '\\' => match chars.get(i + 1) {
                Some(&n) => {
                    units.push(Unit::Lit(n));
                    i += 2;
                }
                None => {
                    units.push(Unit::Never);
                    i += 1;
                }
            },
            '*' => {
                // Collapse `**` so backtracking stays linear in the pattern.
                if !matches!(units.last(), Some(Unit::Star)) {
                    units.push(Unit::Star);
                }
                i += 1;
            }
            '?' => {
                units.push(Unit::Any);
                i += 1;
            }
            '[' => match compile_class(chars, i) {
                Some((unit, next)) => {
                    units.push(unit);
                    i = next;
                }
                // An unterminated `[` is a literal `[` (POSIX).
                None => {
                    units.push(Unit::Lit('['));
                    i += 1;
                }
            },
            _ => {
                units.push(Unit::Lit(c));
                i += 1;
            }
        }
    }
    units
}

/// A flattened pattern read as plain TEXT: every backslash escapes the next
/// character and is dropped. ash's `expmeta` does this to the literal path
/// components around the wildcard before `opendir`ing them (ash.c:7873), which
/// is why `d\ir/*` lists `dir/`. A TRAILING backslash has nothing to escape and
/// stays, as ash's `*p == '\\' && p[1]` guard leaves it.
pub fn unescape(chars: &[char]) -> String {
    let mut out = String::with_capacity(chars.len());
    let mut i = 0usize;
    while let Some(&c) = chars.get(i) {
        match (c, chars.get(i + 1)) {
            ('\\', Some(&n)) => {
                out.push(n);
                i += 2;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// Parse a bracket expression starting at `start` (which holds `[`). Returns the
/// unit and the index just past the closing `]`.
fn compile_class(chars: &[char], start: usize) -> Option<(Unit, usize)> {
    let mut i = start + 1;
    let mut negated = false;
    if matches!(chars.get(i), Some('!') | Some('^')) {
        negated = true;
        i += 1;
    }
    let mut items: Vec<ClassItem> = Vec::new();
    // A `]` in the first position is a literal member, not the terminator.
    if matches!(chars.get(i), Some(']')) {
        items.push(ClassItem::Ch(']'));
        i += 1;
    }
    loop {
        let &c = chars.get(i)?;
        if c == ']' {
            if items.is_empty() {
                return None;
            }
            return Some((Unit::Class { negated, items }, i + 1));
        }
        // `[:alpha:]`
        if c == '[' && matches!(chars.get(i + 1), Some(':')) {
            let mut name = String::new();
            let mut j = i + 2;
            loop {
                let &n = chars.get(j)?;
                if n == ':' && matches!(chars.get(j + 1), Some(']')) {
                    break;
                }
                name.push(n);
                j += 1;
            }
            items.push(ClassItem::Named(name));
            i = j + 2;
            continue;
        }
        // A member may be escaped, and so may a range's endpoints -- BOTH of
        // them: `[a-\z]` is the range a..z, not `a..\` with a stray `z` beside
        // it. The high endpoint is the one that is easy to miss, because the
        // low one goes through this same step.
        let (lo, j) = escaped_at(chars, i)?;
        if matches!(chars.get(j), Some('-')) && !matches!(chars.get(j + 1), Some(']') | None) {
            let (hi, k) = escaped_at(chars, j + 1)?;
            items.push(ClassItem::Range(lo, hi));
            i = k;
            continue;
        }
        items.push(ClassItem::Ch(lo));
        i = j;
    }
}

/// The character at `i`, honouring a backslash escape, and the index just past
/// what it consumed. `None` when a trailing backslash leaves nothing to escape.
fn escaped_at(chars: &[char], i: usize) -> Option<(char, usize)> {
    match chars.get(i)? {
        '\\' => Some((*chars.get(i + 1)?, i + 2)),
        &c => Some((c, i + 1)),
    }
}

/// The POSIX character-class names `named_matches` serves. Exposed because the
/// regex engine must REFUSE an unknown one at compile time where a glob merely
/// never matches it: glibc reports `[[:bogus:]]` as a bad pattern, and a
/// silently-empty class is worse than a diagnostic in the NEGATED case, where
/// it matches everything instead of nothing.
pub fn is_class_name(name: &str) -> bool {
    matches!(
        name,
        "alpha"
            | "digit"
            | "alnum"
            | "upper"
            | "lower"
            | "space"
            | "blank"
            | "punct"
            | "print"
            | "graph"
            | "cntrl"
            | "xdigit"
    )
}

fn named_matches(name: &str, c: char) -> bool {
    match name {
        "alpha" => c.is_alphabetic(),
        "digit" => c.is_ascii_digit(),
        "alnum" => c.is_alphanumeric(),
        "upper" => c.is_uppercase(),
        "lower" => c.is_lowercase(),
        "space" => c.is_whitespace(),
        "blank" => c == ' ' || c == '\t',
        "punct" => c.is_ascii_punctuation(),
        "print" => !c.is_control(),
        "graph" => !c.is_control() && !c.is_whitespace(),
        "cntrl" => c.is_control(),
        "xdigit" => c.is_ascii_hexdigit(),
        _ => false,
    }
}

/// Membership in a bracket expression. Shared with the regex engine so
/// `[[:alpha:]]`, ranges and negation mean ONE thing across the shell rather
/// than two implementations that drift.
pub fn class_matches(negated: bool, items: &[ClassItem], c: char) -> bool {
    let hit = items.iter().any(|item| match item {
        ClassItem::Ch(x) => *x == c,
        ClassItem::Range(lo, hi) => *lo <= c && c <= *hi,
        ClassItem::Named(n) => named_matches(n, c),
    });
    hit != negated
}

fn unit_matches(unit: &Unit, c: char) -> bool {
    match unit {
        Unit::Lit(l) => *l == c,
        Unit::Any => true,
        // Handled by the caller; a `*` never consumes exactly one character.
        Unit::Star => false,
        Unit::Never => false,
        Unit::Class { negated, items } => class_matches(*negated, items, c),
    }
}

/// Match `units` against the whole of `text`.
///
/// Iterative with a single backtrack point per `*`, so a pattern like `a*b*c`
/// cannot blow the stack the way naive recursion does on a long subject.
fn match_all(units: &[Unit], text: &[char]) -> bool {
    let mut ui = 0usize;
    let mut ti = 0usize;
    let mut star: Option<usize> = None;
    let mut star_ti = 0usize;
    while ti < text.len() {
        match units.get(ui) {
            Some(Unit::Star) => {
                star = Some(ui);
                star_ti = ti;
                ui += 1;
            }
            Some(u) if text.get(ti).is_some_and(|&c| unit_matches(u, c)) => {
                ui += 1;
                ti += 1;
            }
            _ => match star {
                // Give the last `*` one more character and retry from there.
                Some(su) => {
                    ui = su + 1;
                    star_ti += 1;
                    ti = star_ti;
                }
                None => return false,
            },
        }
    }
    // The subject is exhausted: only trailing `*`s can still match.
    while matches!(units.get(ui), Some(Unit::Star)) {
        ui += 1;
    }
    ui == units.len()
}

pub fn matches(units: &[Unit], text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    match_all(units, &chars)
}

/// The LONGEST match of `units` starting exactly at `from`, as the index just
/// past it. ash's `scanright` shortens the candidate until one matches, so
/// `${s/<*>/[]}` swallows all of `<html></html>` rather than the first tag.
fn match_at(units: &[Unit], chars: &[char], from: usize) -> Option<usize> {
    let mut k = chars.len();
    loop {
        if chars.get(from..k).is_some_and(|seg| match_all(units, seg)) {
            return Some(k);
        }
        if k <= from {
            return None;
        }
        k -= 1;
    }
}

/// `${x/pat/repl}` (`all` false) and `${x//pat/repl}`: scan left to right,
/// replacing the longest match at each position. The single form copies the
/// rest of the subject verbatim after the one replacement.
///
/// Only a match that CONSUMED something advances the scan, and only positions
/// that still have a character are tried. ash instead retries the empty match
/// at the end of the subject forever, which is why `${v//*/X}` hangs it.
pub fn replace(units: &[Unit], text: &str, repl: &str, all: bool) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    // An empty subject still gets one attempt, since `*` matches it: bash gives
    // `X` for `${v//*/X}` with `v` empty, and so does ash for the single form.
    if chars.is_empty() {
        if match_at(units, &chars, 0).is_some() {
            out.push_str(repl);
        }
        return out;
    }
    // A pattern that opens with `*` can be abandoned the moment it fails: any
    // prefix of the remainder that would match from i+1 is also a prefix of the
    // remainder at i, so failing here means failing everywhere after. ash relies
    // on this (`if (str[0] == '*') goto skip_matching`) and it is not merely a
    // saving -- without it `${x//*z/Q}` is cubic in the value's length.
    let leading_star = matches!(units.first(), Some(Unit::Star));
    let mut i = 0usize;
    while i < chars.len() {
        match match_at(units, &chars, i) {
            Some(end) if end > i => {
                out.push_str(repl);
                i = end;
                if !all {
                    break;
                }
            }
            // No match, or one that consumed nothing: keep the character and
            // move on, which is what guarantees the scan terminates.
            _ => {
                if leading_star {
                    break;
                }
                match chars.get(i) {
                    Some(&c) => {
                        out.push(c);
                        i += 1;
                    }
                    None => break,
                }
            }
        }
    }
    if let Some(rest) = chars.get(i..) {
        out.extend(rest);
    }
    out
}

/// `${x#pat}` / `${x##pat}`: drop the shortest (or longest) prefix of `text`
/// that `units` matches. Returns `None` when nothing matches.
pub fn strip_prefix(units: &[Unit], text: &str, longest: bool) -> Option<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut cuts: Vec<usize> = (0..=chars.len()).collect();
    if longest {
        cuts.reverse();
    }
    for k in cuts {
        let head = chars.get(..k)?;
        if match_all(units, head) {
            return Some(chars.get(k..)?.iter().collect());
        }
    }
    None
}

/// `${x%pat}` / `${x%%pat}`: drop the shortest (or longest) matching suffix.
pub fn strip_suffix(units: &[Unit], text: &str, longest: bool) -> Option<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut cuts: Vec<usize> = (0..=chars.len()).rev().collect();
    if longest {
        cuts.reverse();
    }
    for k in cuts {
        let tail = chars.get(k..)?;
        if match_all(units, tail) {
            return Some(chars.get(..k)?.iter().collect());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pat(s: &str) -> Vec<Unit> {
        compile(&QChar::literal_str(s))
    }

    /// An UNCLOSED `[` is not a metacharacter. ash keeps scanning past it for a
    /// real one, so `my[file*` globs on the `*` while `[\` is not a pattern at
    /// all -- and a word that is not a pattern keeps its backslashes, which is
    /// what makes the difference observable.
    #[test]
    fn an_unclosed_bracket_is_not_a_metacharacter() {
        let m = |s: &str| has_meta_chars(&s.chars().collect::<Vec<_>>());
        assert!(!m("[abc"));
        assert!(!m("["));
        assert!(!m("a[b"));
        assert!(!m("[\\"));
        // A `]` later in the SAME slice closes it, wherever it sits.
        assert!(m("[abc]"));
        assert!(m("a[b]c"));
        assert!(m("[]"));
        // ...and another metacharacter still counts even when the bracket does
        // not, which is the half a plain "no `]`, no glob" rule gets wrong.
        assert!(m("my[file*"));
        assert!(m("[abc?"));
        // An escaped `]` does not close it for this test -- ash asks a plain
        // `strchr`, which cannot see the escape.
        assert!(m("[a\\]"));
    }

    /// The literal path components around a wildcard are spent as TEXT, not
    /// matched, so their escapes come off here.
    #[test]
    fn unescape_spends_the_escape_and_keeps_a_trailing_backslash() {
        let u = |s: &str| unescape(&s.chars().collect::<Vec<_>>());
        assert_eq!(u("d\\ir"), "dir");
        assert_eq!(u("\\dir"), "dir");
        // A doubled backslash is one literal backslash, which is how a directory
        // actually NAMED `od\dd` is reached.
        assert_eq!(u("od\\\\dd"), "od\\dd");
        // Nothing to escape, so ash's `p[1]` guard leaves it in place.
        assert_eq!(u("dir\\"), "dir\\");
        assert_eq!(u("\\"), "\\");
        assert_eq!(u(""), "");
        // A metacharacter that reached here was escaped, so it is just text.
        assert_eq!(u("f\\*"), "f*");
    }

    #[test]
    fn an_unquoted_backslash_is_the_fnmatch_escape() {
        // ash matches with `fnmatch(p, s, 0)`, so `\f` is the letter f -- it does
        // NOT match a literal backslash-f, which is the whole point.
        assert!(matches(&pat("\\f"), "f"));
        assert!(!matches(&pat("\\f"), "\\f"));
        assert!(matches(&pat("\\*"), "*"));
        assert!(!matches(&pat("\\*"), "a"));
        assert!(matches(&pat("a\\b"), "ab"));
        // A TRAILING escape has nothing to escape, and glibc refuses the pattern
        // rather than matching a backslash: neither `a\` nor `a` matches `a\`.
        assert!(!matches(&pat("a\\"), "a\\"));
        assert!(!matches(&pat("a\\"), "a"));
        assert!(!matches(&pat("\\"), "\\"));
        // The escape reaches inside a bracket, so the class holds the asterisk
        // alone -- the backslash beside it is not a member.
        assert!(matches(&pat("[\\*]"), "*"));
        assert!(!matches(&pat("[\\*]"), "\\"));
        // Quoting suppresses it: a quoted backslash is data, which is why the
        // quoted forms of all of the above already agreed with ash.
        let quoted: Vec<QChar> = "\\f".chars().map(|c| QChar { c, quoted: true, expanded: true }).collect();
        assert!(matches(&compile(&quoted), "\\f"));
        assert!(!matches(&compile(&quoted), "f"));
    }

    /// Build a pattern from `(char, quoted)` pairs, which is the only way to
    /// reach the mixed-quoting boundary below.
    fn mixed(spec: &[(char, bool)]) -> Vec<Unit> {
        let qs: Vec<QChar> = spec
            .iter()
            .map(|&(c, quoted)| QChar { c, quoted, expanded: true })
            .collect();
        compile(&qs)
    }

    #[test]
    fn an_unquoted_backslash_escapes_the_quoting_of_the_next_character() {
        // Quoting is ITSELF a backslash in the pattern ash builds, so an
        // unquoted `\` before a QUOTED `*` escapes that backslash and leaves the
        // asterisk a WILDCARD -- literal backslash, then match anything.
        let p = mixed(&[('\\', false), ('*', true)]);
        assert!(matches(&p, "\\ab"));
        assert!(matches(&p, "\\*"));
        assert!(!matches(&p, "*"));
        // Both unquoted is the other reading of the same two characters, and it
        // means the opposite: a literal asterisk and nothing else.
        assert!(!matches(&pat("\\*"), "\\ab"));
        assert!(matches(&pat("\\*"), "*"));
        // A quoted backslash inside a class is a member, not an escape.
        let cls = mixed(&[('[', false), ('\\', true), (']', false)]);
        assert!(matches(&cls, "\\"));
    }

    #[test]
    fn a_bracket_range_escapes_both_of_its_endpoints() {
        // The HIGH endpoint is the one easily missed: read raw, `[a-\z]` becomes
        // the empty range a..\ plus a stray `z`, so `m` stops matching and -- the
        // sharper failure -- `[!a-\z]` starts matching it.
        assert!(matches(&pat("[a-\\z]"), "m"));
        assert!(!matches(&pat("[!a-\\z]"), "m"));
        assert!(matches(&pat("[\\a-\\c]"), "b"));
        // `-` before the closing bracket is a member, not a range.
        assert!(matches(&pat("[a\\-z]"), "-"));
        assert!(!matches(&pat("[a\\-z]"), "b"));
    }

    #[test]
    fn has_meta_honours_the_escape() {
        // `a\*b` has no metacharacter, so the field is left alone instead of
        // being globbed -- without this the pattern would match a file named
        // `a*b` and the word would change under pathname expansion.
        assert!(!has_meta(&QChar::literal_str("a\\*b")));
        assert!(!has_meta(&QChar::literal_str("\\?")));
        assert!(has_meta(&QChar::literal_str("a*b")));
        assert!(has_meta(&QChar::literal_str("\\[a]*")));
        assert!(!has_meta(&QChar::literal_str("plain")));
    }

    #[test]
    fn replace_takes_the_longest_match_at_each_position() {
        // Longest, not first: a shortest match would stop at the opening tag.
        assert_eq!(
            replace(&pat("<*>"), "begin <html></html> end", "[]", false),
            "begin [] end"
        );
        // `*b` at position 0 reaches the LAST b, so one replacement eats `abcab`.
        assert_eq!(replace(&pat("*b"), "abcabc", "X", true), "Xc");
        assert_eq!(replace(&pat("*b"), "abcabc", "X", false), "Xc");
        // The single form copies the tail verbatim; the global form keeps scanning.
        assert_eq!(replace(&pat("b"), "abcabc", "X", false), "aXcabc");
        assert_eq!(replace(&pat("b"), "abcabc", "X", true), "aXcaXc");
        // Scanning resumes AFTER the match, so `xx?` cannot re-consume its own `_`.
        assert_eq!(replace(&pat("xx?"), "xx_xx_xx", "yy_", true), "yy_yy_xx");
        assert_eq!(replace(&pat("a"), "aaa", "X", true), "XXX");
        assert_eq!(replace(&pat("z"), "abc", "X", true), "abc");
    }

    #[test]
    fn replace_terminates_where_ash_spins_on_an_empty_match() {
        // `*` matches the empty string at the end of the subject, and ash retries
        // it there forever. Trying only positions that still have a character
        // terminates AND gives bash's answer. Without that these three hang.
        assert_eq!(replace(&pat("*"), "abc", "X", true), "X");
        assert_eq!(replace(&pat("b*"), "abc", "X", true), "aX");
        assert_eq!(replace(&pat("*b"), "abcabc", "X", true), "Xc");
        // The empty subject is the one place an empty match must still replace:
        // skipping it outright would print nothing where both shells print X.
        assert_eq!(replace(&pat("*"), "", "X", true), "X");
        assert_eq!(replace(&pat("*"), "", "X", false), "X");
        assert_eq!(replace(&pat("a"), "", "X", true), "");
    }

    #[test]
    fn literals_and_wildcards() {
        assert!(matches(&pat("abc"), "abc"));
        assert!(!matches(&pat("abc"), "abcd"));
        assert!(matches(&pat("a*c"), "abbbc"));
        assert!(matches(&pat("a*c"), "ac"));
        assert!(!matches(&pat("a*c"), "ab"));
        assert!(matches(&pat("*"), ""));
        assert!(matches(&pat("a?c"), "abc"));
        assert!(!matches(&pat("a?c"), "ac"));
    }

    #[test]
    fn backtracking_over_several_stars() {
        assert!(matches(&pat("*a*b*c*"), "xxaxxbxxcxx"));
        assert!(!matches(&pat("*a*b*c*"), "xxaxxcxxbxx"));
        // A long subject that never matches must still terminate.
        let subject = "a".repeat(200);
        assert!(!matches(&pat("*a*b"), &subject));
    }

    #[test]
    fn bracket_expressions() {
        assert!(matches(&pat("[abc]"), "b"));
        assert!(!matches(&pat("[abc]"), "d"));
        assert!(matches(&pat("[a-z]*"), "hello"));
        assert!(matches(&pat("[!a-z]"), "A"));
        assert!(!matches(&pat("[!a-z]"), "a"));
        assert!(matches(&pat("[[:digit:]]"), "7"));
        assert!(!matches(&pat("[[:digit:]]"), "x"));
        // An unterminated bracket is a literal `[`.
        assert!(matches(&pat("[abc"), "[abc"));
    }

    #[test]
    fn quoted_metacharacters_are_literal() {
        let mut p = QChar::literal_str("a");
        p.push(QChar {
            c: '*',
            quoted: true,
            expanded: false,
        });
        let units = compile(&p);
        assert!(matches(&units, "a*"));
        assert!(!matches(&units, "abc"));
    }

    #[test]
    fn trims_pick_shortest_or_longest() {
        assert_eq!(
            strip_suffix(&pat(".*"), "archive.tar.gz", false),
            Some("archive.tar".into())
        );
        assert_eq!(
            strip_suffix(&pat(".*"), "archive.tar.gz", true),
            Some("archive".into())
        );
        assert_eq!(
            strip_prefix(&pat("/usr/"), "/usr/local/bin", false),
            Some("local/bin".into())
        );
        assert_eq!(
            strip_prefix(&pat("*/"), "/usr/local/bin", true),
            Some("bin".into())
        );
        assert_eq!(strip_prefix(&pat("zz"), "abc", false), None);
    }
}

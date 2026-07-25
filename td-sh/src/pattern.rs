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
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClassItem {
    Ch(char),
    Range(char, char),
    /// `[:alpha:]` and friends.
    Named(String),
}

/// True when the pattern can match something other than itself — the cheap test
/// that decides whether a field needs pathname expansion at all.
pub fn has_meta(pat: &[QChar]) -> bool {
    pat.iter()
        .any(|q| !q.quoted && matches!(q.c, '*' | '?' | '['))
}

pub fn compile(pat: &[QChar]) -> Vec<Unit> {
    let mut units = Vec::new();
    let mut i = 0usize;
    while let Some(q) = pat.get(i) {
        if q.quoted {
            units.push(Unit::Lit(q.c));
            i += 1;
            continue;
        }
        match q.c {
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
            '[' => match compile_class(pat, i) {
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
            c => {
                units.push(Unit::Lit(c));
                i += 1;
            }
        }
    }
    units
}

/// Parse a bracket expression starting at `start` (which holds `[`). Returns the
/// unit and the index just past the closing `]`.
fn compile_class(pat: &[QChar], start: usize) -> Option<(Unit, usize)> {
    let mut i = start + 1;
    let mut negated = false;
    if matches!(pat.get(i), Some(q) if !q.quoted && (q.c == '!' || q.c == '^')) {
        negated = true;
        i += 1;
    }
    let mut items: Vec<ClassItem> = Vec::new();
    // A `]` in the first position is a literal member, not the terminator.
    if matches!(pat.get(i), Some(q) if q.c == ']') {
        items.push(ClassItem::Ch(']'));
        i += 1;
    }
    loop {
        let q = pat.get(i)?;
        if !q.quoted && q.c == ']' {
            if items.is_empty() {
                return None;
            }
            return Some((Unit::Class { negated, items }, i + 1));
        }
        // `[:alpha:]`
        if !q.quoted && q.c == '[' && matches!(pat.get(i + 1), Some(n) if !n.quoted && n.c == ':') {
            let mut name = String::new();
            let mut j = i + 2;
            loop {
                let c = pat.get(j)?;
                if c.c == ':' && matches!(pat.get(j + 1), Some(e) if e.c == ']') {
                    break;
                }
                name.push(c.c);
                j += 1;
            }
            items.push(ClassItem::Named(name));
            i = j + 2;
            continue;
        }
        let lo = q.c;
        let dash = pat.get(i + 1);
        let hi = pat.get(i + 2);
        if matches!(dash, Some(d) if !d.quoted && d.c == '-')
            && matches!(hi, Some(h) if !(h.c == ']' && !h.quoted))
        {
            if let Some(h) = hi {
                items.push(ClassItem::Range(lo, h.c));
                i += 3;
                continue;
            }
        }
        items.push(ClassItem::Ch(lo));
        i += 1;
    }
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

fn unit_matches(unit: &Unit, c: char) -> bool {
    match unit {
        Unit::Lit(l) => *l == c,
        Unit::Any => true,
        // Handled by the caller; a `*` never consumes exactly one character.
        Unit::Star => false,
        Unit::Class { negated, items } => {
            let hit = items.iter().any(|item| match item {
                ClassItem::Ch(x) => *x == c,
                ClassItem::Range(lo, hi) => *lo <= c && c <= *hi,
                ClassItem::Named(n) => named_matches(n, c),
            });
            hit != *negated
        }
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

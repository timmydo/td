//! `fnmatch(3)`-style matching for `find -name`/`-path`: `*`, `?`, `[...]`
//! with ranges and `!`/`^` negation, and `\` quoting. `*` and `?` match `/`
//! here, as `find -path` requires (FNM_PATHNAME is never set).

/// Does `text` match `pat`? `fold` compares ASCII case-insensitively (`-iname`).
pub fn matches(pat: &[u8], text: &[u8], fold: bool) -> bool {
    let (mut p, mut t) = (0usize, 0usize);
    // The most recent `*`: where it was in the pattern, and how much of the
    // text it has absorbed so far. Backtracking only ever revisits that one.
    let mut star: Option<(usize, usize)> = None;
    loop {
        if let Some(&pc) = pat.get(p) {
            match pc {
                b'*' => {
                    star = Some((p, t));
                    p += 1;
                    continue;
                }
                b'?' if t < text.len() => {
                    p += 1;
                    t += 1;
                    continue;
                }
                b'[' => match class(pat, p, text.get(t), fold) {
                    Some((true, end)) => {
                        p = end;
                        t += 1;
                        continue;
                    }
                    Some((false, _)) => {}
                    // An unterminated `[` is an ordinary character.
                    None => {
                        if eq(b'[', text.get(t).copied(), fold) {
                            p += 1;
                            t += 1;
                            continue;
                        }
                    }
                },
                b'\\' => {
                    let lit = pat.get(p + 1).copied().unwrap_or(b'\\');
                    if eq(lit, text.get(t).copied(), fold) {
                        p += if pat.get(p + 1).is_some() { 2 } else { 1 };
                        t += 1;
                        continue;
                    }
                }
                _ => {
                    if eq(pc, text.get(t).copied(), fold) {
                        p += 1;
                        t += 1;
                        continue;
                    }
                }
            }
        } else if t == text.len() {
            return true;
        }
        // Mismatch: let the last `*` absorb one more byte, or fail.
        match star {
            Some((sp, st)) if st < text.len() => {
                star = Some((sp, st + 1));
                p = sp + 1;
                t = st + 1;
            }
            _ => return false,
        }
    }
}

fn eq(pc: u8, tc: Option<u8>, fold: bool) -> bool {
    match tc {
        Some(tc) if fold => pc.eq_ignore_ascii_case(&tc),
        Some(tc) => pc == tc,
        None => false,
    }
}

/// Parse the bracket expression starting at `pat[at] == b'['`. Returns whether
/// `c` is in it (false when `c` is None) and the index just past its `]`, or
/// None when the bracket is unterminated.
fn class(pat: &[u8], at: usize, c: Option<&u8>, fold: bool) -> Option<(bool, usize)> {
    let mut i = at + 1;
    let negate = matches!(pat.get(i), Some(b'!' | b'^'));
    if negate {
        i += 1;
    }
    let mut hit = false;
    let mut first = true;
    loop {
        let b = *pat.get(i)?;
        if b == b']' && !first {
            break;
        }
        first = false;
        if b == b'[' && pat.get(i + 1) == Some(&b':') {
            let name_at = i + 2;
            let len = pat.get(name_at..)?.windows(2).position(|w| w == b":]")?;
            let name = pat.get(name_at..name_at + len)?;
            if let Some(&c) = c {
                hit |= posix_class(name, c, fold)?;
            }
            i = name_at + len + 2;
            continue;
        }
        let lo = if b == b'\\' {
            i += 1;
            *pat.get(i)?
        } else {
            b
        };
        let (lo, hi) =
            if pat.get(i + 1) == Some(&b'-') && pat.get(i + 2).is_some_and(|&n| n != b']') {
                let hi = *pat.get(i + 2)?;
                i += 3;
                (lo, hi)
            } else {
                i += 1;
                (lo, lo)
            };
        if let Some(&c) = c {
            let inr = |c: u8| lo <= c && c <= hi;
            if inr(c) || (fold && (inr(c.to_ascii_lowercase()) || inr(c.to_ascii_uppercase()))) {
                hit = true;
            }
        }
    }
    Some((c.is_some() && hit != negate, i + 1))
}

/// Whether `c` is in the POSIX class `name`, in the C locale; None for a
/// name that is not one, which leaves the whole bracket unparsed.
fn posix_class(name: &[u8], c: u8, fold: bool) -> Option<bool> {
    Some(match name {
        b"alpha" => c.is_ascii_alphabetic(),
        b"digit" => c.is_ascii_digit(),
        b"alnum" => c.is_ascii_alphanumeric(),
        b"upper" if fold => c.is_ascii_alphabetic(),
        b"upper" => c.is_ascii_uppercase(),
        b"lower" if fold => c.is_ascii_alphabetic(),
        b"lower" => c.is_ascii_lowercase(),
        b"space" => matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c),
        b"blank" => matches!(c, b' ' | b'\t'),
        b"punct" => c.is_ascii_punctuation(),
        b"print" => c == b' ' || c.is_ascii_graphic(),
        b"graph" => c.is_ascii_graphic(),
        b"cntrl" => c.is_ascii_control(),
        b"xdigit" => c.is_ascii_hexdigit(),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(p: &str, t: &str) -> bool {
        matches(p.as_bytes(), t.as_bytes(), false)
    }

    #[test]
    fn stars_questions_and_literals() {
        assert!(m("*.c", "main.c"));
        assert!(!m("*.c", "main.h"));
        assert!(m("*", ""));
        assert!(m("a*b*c", "aXXbYYc"));
        assert!(!m("a*b*c", "aXXbYY"));
        assert!(m("?.o", "a.o"));
        assert!(!m("?.o", ".o"));
        assert!(m("*/Kconfig", "arch/x86/Kconfig"));
        assert!(m("abc", "abc"));
        assert!(!m("abc", "abcd"));
    }

    #[test]
    fn brackets_ranges_negation_and_escapes() {
        assert!(m("[abc].rs", "b.rs"));
        assert!(!m("[abc].rs", "d.rs"));
        assert!(m("[a-z]x", "qx"));
        assert!(!m("[!a-z]x", "qx"));
        assert!(m("[^a-z]x", "Qx"));
        assert!(m("[]]", "]"));
        assert!(m("\\*", "*"));
        assert!(!m("\\*", "a"));
        assert!(m("[", "["));
        assert!(m("a[", "a["));
    }

    #[test]
    fn case_folding_for_iname() {
        assert!(matches(b"*.TXT", b"notes.txt", true));
        assert!(matches(b"[A-C]*", b"bob", true));
        assert!(!matches(b"*.TXT", b"notes.txt", false));
    }

    #[test]
    fn posix_classes() {
        assert!(m("[[:digit:]]*", "1x"));
        assert!(!m("[[:digit:]]*", "x1"));
        assert!(m("[![:alpha:]]", "_"));
        assert!(m("[[:upper:][:digit:]]", "Q"));
        assert!(matches(b"[[:upper:]]", b"q", true));
        assert!(!m("[[:upper:]]", "q"));
        assert!(m("[a[:space:]]", " "));
        // An unknown class is no class: it matches none of its letters.
        assert!(!m("[[:nope:]]", "n"));
        assert!(!m("[[:nope:]]", ":"));
    }
}

//! OpenSSH's `*` and `?` patterns and comma lists with `!` negation, which the
//! client configuration, `known_hosts` and `from=` all use.

/// Matches without recursion's exponential worst case: backtrack only to the
/// most recent `*`, so a hostile pattern costs at most length squared.
pub fn glob(pattern: &str, text: &str) -> bool {
    let pattern = pattern.as_bytes();
    let text = text.as_bytes();
    let mut pattern_index = 0usize;
    let mut text_index = 0usize;
    let mut star: Option<(usize, usize)> = None;
    while let Some(current) = text.get(text_index) {
        match pattern.get(pattern_index) {
            Some(b'*') => {
                star = Some((pattern_index, text_index));
                pattern_index += 1;
            }
            Some(b'?') => {
                pattern_index += 1;
                text_index += 1;
            }
            Some(literal) if literal == current => {
                pattern_index += 1;
                text_index += 1;
            }
            _ => match star {
                Some((star_pattern, star_text)) => {
                    pattern_index = star_pattern + 1;
                    text_index = star_text + 1;
                    star = Some((star_pattern, star_text + 1));
                }
                None => return false,
            },
        }
    }
    pattern
        .get(pattern_index..)
        .unwrap_or_default()
        .iter()
        .all(|byte| *byte == b'*')
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListMatch {
    Accepted,
    /// A `!` entry matched, which beats any positive entry.
    Negated,
    None,
}

/// A comma-separated pattern list against one name, case-insensitively for
/// hostnames as OpenSSH lowercases them.
pub fn match_list(list: &str, name: &str) -> ListMatch {
    let lower = name.to_ascii_lowercase();
    let mut accepted = false;
    for entry in list.split(',') {
        let (negated, pattern) = match entry.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, entry),
        };
        if glob(&pattern.to_ascii_lowercase(), &lower) {
            if negated {
                return ListMatch::Negated;
            }
            accepted = true;
        }
    }
    if accepted {
        ListMatch::Accepted
    } else {
        ListMatch::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs_match_like_openssh() {
        for (pattern, text, expected) in [
            ("*", "", true),
            ("*", "anything", true),
            ("", "", true),
            ("", "x", false),
            ("a?c", "abc", true),
            ("a?c", "ac", false),
            ("*.example", "h.example", true),
            ("*.example", "example", false),
            ("a*b*c", "axxbyyc", true),
            ("a*b*c", "axxbyy", false),
            ("**", "x", true),
            ("?", "", false),
        ] {
            assert_eq!(glob(pattern, text), expected, "{pattern} vs {text}");
        }
    }

    #[test]
    fn a_hostile_pattern_finishes() {
        let pattern = "a*".repeat(2000) + "b";
        let text = "a".repeat(4000);
        assert!(!glob(&pattern, &text));
    }

    #[test]
    fn negation_wins_regardless_of_order() {
        assert_eq!(match_list("*.x,!a.x", "a.x"), ListMatch::Negated);
        assert_eq!(match_list("!a.x,*.x", "a.x"), ListMatch::Negated);
        assert_eq!(match_list("*.x,!a.x", "b.x"), ListMatch::Accepted);
        assert_eq!(match_list("!a.x", "b.x"), ListMatch::None);
        assert_eq!(match_list("HOST", "host"), ListMatch::Accepted);
    }
}

//! The link under a byte of a text, which a program follows on a
//! Control-press: an `http://` or `https://` scheme with at least one byte
//! after it, running to the next ASCII whitespace or `<>])"'` backtick,
//! trailing `.,;:!?` left out, the rule td-mail's link list uses
//! (`https?://[^\s<>\]\)"'`]+`), so a link a program lists is the one a
//! press follows. The search looks at most `MAX_BYTES` either side of the
//! byte, and a run of link bytes longer than that is no link, so a press
//! costs the same in a huge line as in a short one.

use std::ops::Range;

/// The most bytes a link runs to either side of the byte pressed.
pub const MAX_BYTES: usize = 8192;

const SCHEMES: [&[u8]; 2] = [b"https://", b"http://"];

/// Whether `byte` ends a link.
pub fn stop(byte: u8) -> bool {
    matches!(
        byte,
        b' ' | 0x09..=0x0d | b'<' | b'>' | b']' | b')' | b'"' | b'\'' | b'`'
    )
}

/// The byte range of the link over `at` in `text`, if one is: the run of
/// link bytes around it, from its first scheme that has a byte after it
/// to its end with trailing punctuation left out, when `at` lies inside
/// that.
pub fn at(text: &str, at: usize) -> Option<Range<usize>> {
    bounded(text, at, MAX_BYTES)
}

/// As `at` without the bound, for a caller that walks the text once
/// anyway, as a wrap does: a press finds such a link whole or, past the
/// bound, not at all.
pub fn around(text: &str, at: usize) -> Option<Range<usize>> {
    bounded(text, at, usize::MAX)
}

fn bounded(text: &str, at: usize, bound: usize) -> Option<Range<usize>> {
    let bytes = text.as_bytes();
    if stop(*bytes.get(at)?) {
        return None;
    }
    // One byte past the bound either way, where the run's stop may sit.
    let floor = at.saturating_sub(bound.saturating_add(1));
    let before = bytes.get(floor..at)?;
    let start = match before.iter().rposition(|&b| stop(b)) {
        Some(offset) => floor + offset + 1,
        None if floor == 0 => 0,
        None => return None,
    };
    let ceiling = at.saturating_add(bound.saturating_add(2)).min(bytes.len());
    let after = bytes.get(at..ceiling)?;
    let end = match after.iter().position(|&b| stop(b)) {
        Some(offset) => at + offset,
        None if ceiling == bytes.len() => ceiling,
        None => return None,
    };
    let link = within(bytes.get(start..end)?)?;
    let link = start + link.start..start + link.end;
    link.contains(&at).then_some(link)
}

/// Whether `text` is one link whole, as `at` finds it over any of its
/// bytes, whatever its length: what the opener admits.
pub fn whole(text: &str) -> bool {
    let bytes = text.as_bytes();
    !bytes.iter().any(|&b| stop(b)) && within(bytes) == Some(0..bytes.len())
}

/// The link in a run of link bytes: from its first scheme that has a byte
/// after it to its end, trailing punctuation left out, if a byte after the
/// scheme is left.
fn within(run: &[u8]) -> Option<Range<usize>> {
    let (first, scheme) = (0..run.len()).find_map(|i| {
        let rest = run.get(i..)?;
        let scheme = SCHEMES
            .iter()
            .find(|scheme| rest.starts_with(scheme) && rest.len() > scheme.len())?;
        Some((i, scheme.len()))
    })?;
    let mut last = run.len();
    while last > first
        && run
            .get(last - 1)
            .is_some_and(|b| matches!(b, b'.' | b',' | b';' | b':' | b'!' | b'?'))
    {
        last -= 1;
    }
    (last - first > scheme).then_some(first..last)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(text: &str, at: usize) -> Option<&str> {
        super::at(text, at).and_then(|range| text.get(range))
    }

    #[test]
    fn a_press_anywhere_on_a_link_finds_the_whole_link() {
        let text = "see https://example.com/a?b=c for more";
        for at in 4..29 {
            assert_eq!(link(text, at), Some("https://example.com/a?b=c"), "{at}");
        }
        for at in [0, 3, 29, 30, text.len() - 1, text.len()] {
            assert_eq!(link(text, at), None, "{at}");
        }
    }

    #[test]
    fn delimiters_end_a_link_and_trailing_punctuation_is_left_out() {
        for (text, expected) in [
            ("<https://e.example/a>", "https://e.example/a"),
            ("(http://e.example/b)", "http://e.example/b"),
            ("\"http://e.example/c\"", "http://e.example/c"),
            ("'http://e.example/d'", "http://e.example/d"),
            ("`http://e.example/e`", "http://e.example/e"),
            ("[x](http://e.example/f)", "http://e.example/f"),
            ("go to http://e.example/g.", "http://e.example/g"),
            ("http://e.example/h?!,;:.", "http://e.example/h"),
            ("line\thttp://e.example/i\nnext", "http://e.example/i"),
            ("see:http://e.example/j", "http://e.example/j"),
            ("xhttp://e.example/k", "http://e.example/k"),
            ("https://http://e.example/l", "https://http://e.example/l"),
            ("http://e.example/ü/m", "http://e.example/ü/m"),
        ] {
            let at = text.find("e.example").unwrap();
            assert_eq!(link(text, at), Some(expected), "{text}");
        }
    }

    #[test]
    fn a_scheme_with_nothing_after_it_is_no_link() {
        for text in [
            "http://", "https://", "http://.", "http:/x", "ftp://x", "HTTP://x",
        ] {
            for at in 0..text.len() {
                assert_eq!(link(text, at), None, "{text} {at}");
            }
        }
        // The first scheme with a body is the one the link starts at.
        assert_eq!(link("http://x", 0), Some("http://x"));
    }

    #[test]
    fn the_prefix_before_a_scheme_in_the_same_run_is_not_the_link() {
        let text = "url=https://e.example/";
        assert_eq!(link(text, 1), None);
        assert_eq!(link(text, 4), Some("https://e.example/"));
    }

    #[test]
    fn whole_admits_what_at_finds_at_any_length() {
        for text in [
            "https://e.example/",
            "http://x",
            "https://http://e.example/l",
            "http://e.example/ü/m",
        ] {
            assert!(whole(text), "{text}");
            assert_eq!(super::at(text, 0), Some(0..text.len()));
        }
        let long = format!("https://e.example/{}", "a".repeat(2 * MAX_BYTES));
        assert!(whole(&long));
        for text in [
            "",
            "-x",
            "e.example",
            "ftp://x",
            "http://",
            "http://.",
            " https://x",
            "https://x y",
            "https://x.",
            "xhttps://x",
            "https://x>",
        ] {
            assert!(!whole(text), "{text}");
        }
    }

    #[test]
    fn a_run_past_the_bound_is_no_link_and_one_within_it_is() {
        let long = format!("https://e.example/{}", "a".repeat(MAX_BYTES));
        assert_eq!(link(&long, 0), None);
        assert_eq!(link(&long, long.len() - 1), None);
        let fits = format!("https://e.example/{}", "a".repeat(MAX_BYTES - 20));
        assert_eq!(link(&fits, fits.len() - 1), Some(fits.as_str()));
        assert_eq!(link(&fits, 0), Some(fits.as_str()));
        // Exactly the bound either side, mid-text, is a link; one past is
        // not.
        let body = "a".repeat(MAX_BYTES - 18);
        let edge = format!(" https://e.example/{body}{body}x ");
        let at = 1 + MAX_BYTES;
        assert_eq!(edge.as_bytes()[at], b'a');
        assert_eq!(link(&edge, at), Some(edge.trim()));
        let tail = "a".repeat(MAX_BYTES - 18);
        let after = format!(" https://e.example/{tail}x ");
        assert_eq!(
            after.len() - 2,
            MAX_BYTES + 1,
            "the press's byte and the bound"
        );
        assert_eq!(link(&after, 1), Some(after.trim()));
        let over = format!(" https://e.example/{tail}xx ");
        assert_eq!(link(&over, 1), None);
        assert_eq!(super::around(&over, 1), Some(1..over.len() - 1));
        let past = format!(" xhttps://e.example/{body}{body}x ");
        assert!(link(&past, 1 + MAX_BYTES).is_some());
        assert_eq!(link(&past, 2 + MAX_BYTES), None);
        // A long run elsewhere in the line costs nothing.
        let line = format!("{} https://e.example/x", "b".repeat(4 * MAX_BYTES));
        assert_eq!(link(&line, line.len() - 1), Some("https://e.example/x"));
    }
}

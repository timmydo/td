//! Where a value sits in a JSON text, as a byte range, so that a value the
//! design requires sent back unmodified is spliced from the bytes that
//! carried it rather than re-encoded from a parse (DESIGN.md §5: an
//! assistant message's `reasoning_details`). The text is parsed whole by
//! `json` first, which refuses what is not JSON and duplicate keys; this
//! walks the same bytes to the value a path names, skipping the rest,
//! iteratively, so no depth of nesting recurses.

/// One step of a path into a JSON text.
#[derive(Clone, Copy, Debug)]
pub enum Step<'a> {
    Key(&'a str),
    Index(usize),
}

/// The byte range of the value at `path` in `text`, which must already
/// have parsed as JSON; `None` when the path names nothing there.
pub fn find(text: &[u8], path: &[Step<'_>]) -> Option<std::ops::Range<usize>> {
    let mut at = skip_ws(text, 0);
    for step in path {
        at = match *step {
            Step::Key(key) => member(text, at, key)?,
            Step::Index(index) => element(text, at, index)?,
        };
    }
    let end = skip_value(text, at)?;
    Some(at..end)
}

fn byte(text: &[u8], at: usize) -> Option<u8> {
    text.get(at).copied()
}

fn skip_ws(text: &[u8], mut at: usize) -> usize {
    while matches!(byte(text, at), Some(b' ' | b'\t' | b'\n' | b'\r')) {
        at += 1;
    }
    at
}

/// The end of the string starting at `at` (its opening quote).
fn skip_string(text: &[u8], at: usize) -> Option<usize> {
    if byte(text, at)? != b'"' {
        return None;
    }
    let mut at = at + 1;
    loop {
        match byte(text, at)? {
            b'"' => return Some(at + 1),
            b'\\' => at += 2,
            _ => at += 1,
        }
    }
}

/// The end of the value starting at `at`.
fn skip_value(text: &[u8], at: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut at = at;
    loop {
        match byte(text, at)? {
            b'"' => at = skip_string(text, at)?,
            b'{' | b'[' => {
                depth += 1;
                at += 1;
            }
            b'}' | b']' => {
                depth = depth.checked_sub(1)?;
                at += 1;
            }
            b',' | b':' | b' ' | b'\t' | b'\n' | b'\r' if depth > 0 => at += 1,
            _ if depth > 0 => at += 1,
            // A scalar at the top: a number or a literal runs to the next
            // delimiter.
            _ => {
                while !matches!(
                    byte(text, at),
                    None | Some(b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r')
                ) {
                    at += 1;
                }
                return Some(at);
            }
        }
        if depth == 0 {
            return Some(at);
        }
    }
}

/// The start of member `key`'s value in the object starting at `at`.
fn member(text: &[u8], at: usize, key: &str) -> Option<usize> {
    if byte(text, at)? != b'{' {
        return None;
    }
    let mut at = skip_ws(text, at + 1);
    if byte(text, at)? == b'}' {
        return None;
    }
    loop {
        let end = skip_string(text, at)?;
        // A key is compared as it decodes: the text may escape it.
        let name = crate::json::parse_slice(text.get(at..end)?).ok()?;
        at = skip_ws(text, end);
        if byte(text, at)? != b':' {
            return None;
        }
        at = skip_ws(text, at + 1);
        if name.as_str() == Some(key) {
            return Some(at);
        }
        at = skip_ws(text, skip_value(text, at)?);
        match byte(text, at)? {
            b',' => at = skip_ws(text, at + 1),
            _ => return None,
        }
    }
}

/// The start of element `index` in the array starting at `at`.
fn element(text: &[u8], at: usize, index: usize) -> Option<usize> {
    if byte(text, at)? != b'[' {
        return None;
    }
    let mut at = skip_ws(text, at + 1);
    if byte(text, at)? == b']' {
        return None;
    }
    for _ in 0..index {
        at = skip_ws(text, skip_value(text, at)?);
        match byte(text, at)? {
            b',' => at = skip_ws(text, at + 1),
            _ => return None,
        }
    }
    Some(at)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    fn at<'t>(text: &'t str, path: &[Step<'_>]) -> Option<&'t str> {
        crate::json::parse(text).unwrap();
        find(text.as_bytes(), path).map(|range| &text[range])
    }

    #[test]
    fn a_path_finds_the_bytes_of_its_value_as_written() {
        let text = r#" { "id" : "x", "choices" : [ { "message" : { "content" : "hi \"there\"",
            "reasoning_details" : [ {"type":"reasoning.encrypted", "data":"a\/bé==" ,"n": 1.50e+3} ] } } ] } "#;
        let path = [
            Step::Key("choices"),
            Step::Index(0),
            Step::Key("message"),
            Step::Key("reasoning_details"),
        ];
        assert_eq!(
            at(text, &path),
            Some(r#"[ {"type":"reasoning.encrypted", "data":"a\/bé==" ,"n": 1.50e+3} ]"#)
        );
        assert_eq!(
            at(
                text,
                &[
                    Step::Key("choices"),
                    Step::Index(0),
                    Step::Key("message"),
                    Step::Key("content")
                ]
            ),
            Some(r#""hi \"there\"""#)
        );
        assert_eq!(at(text, &[Step::Key("id")]), Some(r#""x""#));
        assert_eq!(at(" 12 ", &[]), Some("12"));
        assert_eq!(at("[1, true ,null]", &[Step::Index(1)]), Some("true"));
        assert_eq!(at("[1, true ,null]", &[Step::Index(2)]), Some("null"));
    }

    #[test]
    fn a_key_is_matched_as_it_decodes_and_absence_is_none() {
        let text = r#"{"ab":{"c":[]},"d":{}}"#;
        assert_eq!(at(text, &[Step::Key("ab"), Step::Key("c")]), Some("[]"));
        assert_eq!(at(text, &[Step::Key("ab"), Step::Key("x")]), None);
        assert_eq!(at(text, &[Step::Key("d"), Step::Key("x")]), None);
        assert_eq!(
            at(text, &[Step::Key("ab"), Step::Key("c"), Step::Index(0)]),
            None
        );
        assert_eq!(at(text, &[Step::Index(0)]), None);
        assert_eq!(at(r#"{"a":"}"}"#, &[Step::Key("b")]), None);
        assert_eq!(at(r#"{"a":"]\\","b":2}"#, &[Step::Key("b")]), Some("2"));
    }

    #[test]
    fn deep_nesting_is_skipped_without_recursion() {
        let deep = format!("{}{}", "[".repeat(100_000), "]".repeat(100_000));
        let text = format!(r#"{{"skip":{deep},"want":7}}"#);
        assert_eq!(
            find(text.as_bytes(), &[Step::Key("want")]).map(|r| &text[r]),
            Some("7")
        );
        // Truncated text ends in None, never a panic.
        assert_eq!(find(b"{\"a\":[1,", &[Step::Key("a"), Step::Index(3)]), None);
        assert_eq!(find(b"{\"a\":\"", &[Step::Key("a")]), None);
    }
}

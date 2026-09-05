//! The scans `build.rs` runs over this crate's sources to learn which `td-*`
//! directories a recipe reads: the wide one for a recipe file, where a stray
//! name widens one recipe's reach, and the embed one for a shared module,
//! whose reads are every recipe's. The build script includes this file by
//! `#[path]`; the library includes it for these tests alone.

/// The `td-*` directories `text` names: `td-sh/` with no identifier character
/// before it, the rule `builder/src/affected.rs` reads crates by, so a store
/// path's `xyz-td-sh-1.0/` is not one and `td-shell/` is not `td-sh/`. A name
/// in a comment or a script counts and only widens. Sorted and deduped.
pub(crate) fn td_dirs_named(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut from = 0usize;
    while let Some(at) = text.get(from..).and_then(|rest| rest.find("td-")) {
        let start = from.saturating_add(at);
        let before = text.get(..start).and_then(|s| s.chars().next_back());
        let rest = text.get(start..).unwrap_or("");
        let end = rest
            .find(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'))
            .unwrap_or(rest.len());
        let name = rest.get(..end).unwrap_or("");
        let slash = rest.get(end..).is_some_and(|r| r.starts_with('/'));
        let bounded = !before.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '-');
        if slash && bounded && name.len() > 3 && !out.iter().any(|n| n == name) {
            out.push(name.to_string());
        }
        from = start.saturating_add(1);
    }
    out.sort();
    out
}

/// The `td-*` directories `text` EMBEDS: those named in the string literal
/// that a `#[path = ...]` attribute or an `include_str!`, `include_bytes!`
/// or `include!` macro carries, in any spacing and with any delimiter, plain
/// or raw — the forms that compile another crate's file into this one. A
/// marker not followed by a literal (`include!(concat!(...))`) embeds
/// nothing rather than the next string in the file, and comments are cut
/// first, so a name in prose or an error string is never an embed.
pub(crate) fn td_dirs_embedded(text: &str) -> Vec<String> {
    let code = strip_comments(text);
    let mut out: Vec<String> = Vec::new();
    for (marker, attribute) in [
        ("#[path", true),
        ("include_str!", false),
        ("include_bytes!", false),
        ("include!", false),
    ] {
        let mut from = 0usize;
        while let Some(at) = code.get(from..).and_then(|rest| rest.find(marker)) {
            let after = from.saturating_add(at).saturating_add(marker.len());
            from = after;
            let rest = code.get(after..).unwrap_or("").trim_start();
            let rest = if attribute {
                rest.strip_prefix('=')
            } else {
                rest.strip_prefix(['(', '[', '{'])
            };
            let Some(literal) = rest.map(str::trim_start).and_then(string_literal_at) else {
                continue;
            };
            for dir in td_dirs_named(literal) {
                if !out.contains(&dir) {
                    out.push(dir);
                }
            }
        }
    }
    out.sort();
    out
}

/// The body of the string literal `text` begins with: `"..."` with `\"`
/// escapes, or a raw `r"..."` / `r#"..."#` at any hash depth. None where
/// `text` does not begin with one, or it never closes.
pub(crate) fn string_literal_at(text: &str) -> Option<&str> {
    string_literal_span(text).map(|(body, _)| body)
}

/// `string_literal_at` with the length of the whole literal beside the
/// body — quotes, hashes and the `r` included — for a scan that steps over
/// it.
pub(crate) fn string_literal_span(text: &str) -> Option<(&str, usize)> {
    if let Some(rest) = text.strip_prefix('"') {
        let mut escaped = false;
        for (i, c) in rest.char_indices() {
            match (escaped, c) {
                (true, _) => escaped = false,
                (false, '\\') => escaped = true,
                (false, '"') => return rest.get(..i).map(|body| (body, i.saturating_add(2))),
                _ => {}
            }
        }
        return None;
    }
    let rest = text.strip_prefix('r')?;
    let hashes = rest.chars().take_while(|c| *c == '#').count();
    let body = rest.get(hashes..)?.strip_prefix('"')?;
    let close = format!("\"{}", "#".repeat(hashes));
    let (lit, _) = body.split_once(close.as_str())?;
    let len = lit
        .len()
        .saturating_add(close.len())
        .saturating_add(hashes)
        .saturating_add(2);
    Some((lit, len))
}

/// The length of the char literal `rest` begins with — `'a'`, `'\n'`,
/// `'\''`, `'\u{1F600}'`, `'"'` — or 1 for the bare quote of a lifetime, so
/// a scan steps over the quote either way.
pub(crate) fn char_literal_len(rest: &str) -> usize {
    let mut chars = rest.char_indices().skip(1);
    match chars.next() {
        // The escaped character is stepped over before the closing quote is
        // sought, so `'\''` closes at its fourth byte, not its third.
        Some((_, '\\')) => rest
            .get(3..)
            .and_then(|r| r.find('\''))
            .map_or(1, |e| e.saturating_add(4)),
        Some(_) => match chars.next() {
            Some((j, '\'')) => j.saturating_add(1),
            _ => 1,
        },
        None => 1,
    }
}

/// `text` with every comment cut — a `//` to the end of its line, a
/// `/* */` to its close, nested — outside string and char literals, which
/// are stepped over whole as the block scan steps over them: plain, raw or
/// spanning lines, so a `//` inside quotes (`"https://..."`) is kept, a
/// `'"'` opens no string, and a `/*` inside a string opens no comment. A
/// raw byte string (`br"..."`) is read as a plain one, the bound the block
/// scan shares. The newlines inside a cut block are kept, so line numbers
/// hold; a block that never closes runs to the end, where the compiler
/// would refuse it.
pub(crate) fn strip_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut at = 0usize;
    while let Some(rest) = text.get(at..).filter(|r| !r.is_empty()) {
        let Some(c) = rest.chars().next() else { break };
        let ident_before = text
            .get(..at)
            .and_then(|s| s.chars().next_back())
            .is_some_and(|p| p.is_alphanumeric() || p == '_');
        let literal = if c == '"' || (c == 'r' && !ident_before) {
            string_literal_span(rest).map(|(_, len)| len)
        } else if c == '\'' {
            Some(char_literal_len(rest))
        } else {
            None
        };
        if let Some(len) = literal {
            out.push_str(rest.get(..len).unwrap_or(""));
            at = at.saturating_add(len);
        } else if rest.starts_with("//") {
            at = at.saturating_add(rest.find('\n').unwrap_or(rest.len()));
        } else if rest.starts_with("/*") {
            at = at.saturating_add(block_comment_len(rest, &mut out));
        } else {
            out.push(c);
            at = at.saturating_add(c.len_utf8());
        }
    }
    out
}

/// The length of the block comment `rest` begins with, nesting counted,
/// its newlines pushed to `out` so the lines after it keep their numbers;
/// all of `rest` where it never closes.
fn block_comment_len(rest: &str, out: &mut String) -> usize {
    let mut depth = 0usize;
    let mut at = 0usize;
    while let Some(inner) = rest.get(at..).filter(|r| !r.is_empty()) {
        if inner.starts_with("/*") {
            depth = depth.saturating_add(1);
            at = at.saturating_add(2);
        } else if inner.starts_with("*/") {
            depth = depth.saturating_sub(1);
            at = at.saturating_add(2);
            if depth == 0 {
                return at;
            }
        } else {
            let Some(c) = inner.chars().next() else { break };
            if c == '\n' {
                out.push('\n');
            }
            at = at.saturating_add(c.len_utf8());
        }
    }
    rest.len()
}

/// Where the block that opens with the `{` at byte `open` of `code` closes:
/// the byte of its `}`, counting braces outside string and char literals
/// and block comments, `code` having had its comments cut. None where
/// `open` is not a `{` or the block never closes. For the tests that read
/// the evaluator's own sources by module.
#[cfg(test)]
pub(crate) fn block_end(code: &str, open: usize) -> Option<usize> {
    if !code.get(open..)?.starts_with('{') {
        return None;
    }
    let mut depth = 0usize;
    let mut at = open;
    while let Some(rest) = code.get(at..).filter(|r| !r.is_empty()) {
        let c = rest.chars().next()?;
        let ident_before = code
            .get(..at)
            .and_then(|s| s.chars().next_back())
            .is_some_and(|p| p.is_alphanumeric() || p == '_');
        let skip = if c == '"' || (c == 'r' && !ident_before) {
            string_literal_span(rest).map(|(_, len)| len)
        } else if rest.starts_with("/*") {
            Some(rest.find("*/").map_or(rest.len(), |e| e.saturating_add(2)))
        } else if c == '\'' {
            Some(char_literal_len(rest))
        } else {
            None
        };
        if let Some(len) = skip {
            at = at.saturating_add(len);
            continue;
        }
        match c {
            '{' => depth = depth.saturating_add(1),
            '}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(at);
                }
            }
            _ => {}
        }
        at = at.saturating_add(c.len_utf8());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // Inputs are assembled at run time: this file is under the build
    // script's own scan, and a `td-<name>/` spelled here would read as a
    // name of this crate's, or as a crate it names without embedding.
    fn d(name: &str) -> String {
        ["td-", name, "/"].concat()
    }

    #[test]
    fn a_name_is_a_directory_with_a_slash_and_no_identifier_before_it() {
        let text = format!(
            "a {}src/x.rs b ../{}y (c){}z store/xyz-{}1.0/ {} {} td-nope",
            d("sh"),
            d("txt"),
            d("sh"),
            d("sh"),
            d("shell"),
            d("txt")
        );
        assert_eq!(td_dirs_named(&text), vec!["td-sh", "td-shell", "td-txt"]);
        assert!(td_dirs_named("nothing here, not even xtd-a/").is_empty());
    }

    #[test]
    fn an_embed_is_the_literal_after_a_marker_in_any_spelling() {
        let text = format!(
            "#[path = \"../../{}src/a.rs\"]\n\
             #[path=\"../{}b.rs\"]\n\
             include_str!(\"../{}c\");\n\
             include_str! (\"{}d\");\n\
             include_bytes![\"{}e\"];\n\
             include! {{ \"{}f\" }}\n\
             include_str!(r#\"../{}g\"#);\n",
            d("a"),
            d("b"),
            d("c"),
            d("dd"),
            d("e"),
            d("f"),
            d("g")
        );
        assert_eq!(
            td_dirs_embedded(&text),
            vec!["td-a", "td-b", "td-c", "td-dd", "td-e", "td-f", "td-g"]
        );
    }

    #[test]
    fn a_marker_without_a_literal_or_in_a_comment_embeds_nothing() {
        // No literal follows: the next string in the file is not the embed.
        let concat = format!(
            "include!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/../{}y.rs\"));\n\
             let e = \"{}z\";",
            d("x"),
            d("x")
        );
        assert!(td_dirs_embedded(&concat).is_empty(), "{concat}");
        assert_eq!(td_dirs_named(&concat), vec!["td-x"], "the wide scan still sees it");
        let comment = format!("// include_str!(\"../{}a.rs\")\n/// #[path = \"{}b\"]\n", d("c"), d("c"));
        assert!(td_dirs_embedded(&comment).is_empty());
        assert!(td_dirs_named(&strip_comments(&comment)).is_empty());
    }

    #[test]
    fn a_comment_is_cut_and_a_slash_pair_inside_a_string_is_kept() {
        let text = format!("let u = \"https://{}\"; // {}\nlet v = \"a \\\" // b\"; // c\n", d("k"), d("m"));
        let code = strip_comments(&text);
        assert_eq!(td_dirs_named(&code), vec!["td-k"]);
        assert!(code.contains("// b\""), "{code}");
        assert!(!code.contains("// c"), "{code}");
        // A quote in a char literal opens no string, so the comment after it
        // is still a comment.
        let code = strip_comments("let q = '\"'; let l: &'static str = \"x\"; // {\n");
        assert_eq!(code, "let q = '\"'; let l: &'static str = \"x\"; \n");
    }

    #[test]
    fn a_block_comment_is_cut_whole_however_many_lines_it_spans() {
        let (p, q) = (d("p"), d("q"));
        let text = [
            "let a = 1; /* ",
            p.as_str(),
            " // } */ let b = 2;\n/* open\n // ",
            q.as_str(),
            " }\n still */ let c = '\\''; let d = \"*/\"; /* /* nested */ } */ let e = 3;\n",
        ]
        .concat();
        let code = strip_comments(&text);
        assert_eq!(
            code,
            "let a = 1;  let b = 2;\n\n\n let c = '\\''; let d = \"*/\";  let e = 3;\n"
        );
        assert!(td_dirs_named(&code).is_empty(), "{code}");
        // An unclosed block runs to the end, and a `/*` in a string opens none.
        assert_eq!(strip_comments("let s = \"/*\"; /* x\ny"), "let s = \"/*\"; \n");
        assert_eq!(char_literal_len("'\\''x"), 4);
        assert_eq!(char_literal_len("'\\\\'"), 4);
    }

    #[test]
    fn a_string_spanning_lines_opens_no_comment() {
        // A `/*` in a backslash-continued string, as two of the evaluator's
        // checks carry in their shell, and a raw string holding a quote and
        // a `//` across lines: the stripper steps over them whole and reads
        // the rest of the file as code.
        let (p, q) = (d("p"), d("q"));
        let text = [
            "let s = \"for f in {dir}/*.log; do \\\n    echo $f; done\"; // ",
            p.as_str(),
            "\nlet r = r#\"a \" // b\n/* c\"#; let n = \"../",
            q.as_str(),
            "x.rs\"; /* ",
            q.as_str(),
            " */\n",
        ]
        .concat();
        let kept = [
            "let s = \"for f in {dir}/*.log; do \\\n    echo $f; done\"; \n",
            "let r = r#\"a \" // b\n/* c\"#; let n = \"../",
            q.as_str(),
            "x.rs\"; \n",
        ]
        .concat();
        assert_eq!(strip_comments(&text), kept);
        assert_eq!(td_dirs_named(&strip_comments(&text)), vec!["td-q"]);
    }

    #[test]
    fn a_block_ends_where_its_braces_balance_outside_literals_and_comments() {
        let code = "mod t {\n    fn f() { let s = \"}\"; let c = '}'; let r = r#\"{\"#; /* } */ \
                    let l: &'static str = \"\"; { } }\n}\nfn after() {}\n";
        let open = code.find('{').unwrap();
        let end = block_end(code, open).unwrap();
        assert_eq!(&code[end..], "}\nfn after() {}\n");
        assert_eq!(block_end("{ never", 0), None);
        assert_eq!(block_end("x{}", 0), None);
        assert_eq!(block_end("x{}", 1), Some(2));
        assert_eq!(string_literal_span("\"ab\" x"), Some(("ab", 4)));
        assert_eq!(string_literal_span("r##\"a\"##!"), Some(("a", 8)));
        assert_eq!(string_literal_span("\"open"), None);
        assert_eq!(char_literal_len("'a' x"), 3);
        assert_eq!(char_literal_len("'\\n' x"), 4);
        assert_eq!(char_literal_len("'\\u{1F600}'"), 11);
        assert_eq!(char_literal_len("'static str"), 1);
    }

    #[test]
    fn a_string_literal_is_read_plain_escaped_and_raw() {
        assert_eq!(string_literal_at("\"a b\" rest"), Some("a b"));
        assert_eq!(string_literal_at("\"a \\\" b\" rest"), Some("a \\\" b"));
        assert_eq!(string_literal_at("r\"a\" rest"), Some("a"));
        assert_eq!(string_literal_at("r##\"a \"# b\"## rest"), Some("a \"# b"));
        assert_eq!(string_literal_at("r#\"unterminated"), None);
        assert_eq!(string_literal_at("concat!(\"x\")"), None);
        assert_eq!(string_literal_at(""), None);
    }
}

//! Source-text scans for td's confinement tests: the crates whose `unsafe`
//! surface UNSAFE.md records prove the compiled crate is the audited one by
//! reading their own `src/` off disk, since the compiler cannot say "only
//! `sys.rs` holds an unsafe block" or "no module is reached by `#[path]`".
//!
//! It is a dev-dependency only (AGENTS.md principle 2): no recipe stages it and
//! no shipped binary links it, because the recipes build those crates without
//! `--test`. A change to how it reads comments, literals, or the tokens after
//! `unsafe` is a change to every consumer's confinement, and is reviewed as one.

#![forbid(unsafe_code)]

use std::path::Path;

/// A crate's source tree as the scans read it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tree {
    /// Every `.rs` file, keyed by its path RELATIVE to the root, never by
    /// basename: a `decoy/sys.rs` is not the `sys.rs` the crate compiles, and
    /// a key that could not tell them apart would let the decoy answer for the
    /// real file. The text is `strip_comments` of the file. Sorted.
    pub rs: Vec<(String, String)>,
    /// Every other file's relative path. A non-`.rs` file under `src/` is not
    /// inert — it is exactly what `include!` and `#[path]` compile — so it is
    /// collected rather than skipped, for the caller to assert absent. Sorted.
    pub other: Vec<String>,
}

/// Every file under `base`, READ FROM DISK rather than listed by hand.
///
/// A hand-written list is a hole: adding `mod newmod;` alongside a `newmod.rs`
/// full of `unsafe` would leave every assertion passing because none of them
/// saw the file. The directory is the authority. Sub-directories too: `mod
/// deep;` inside a module puts `deep.rs` one level down, and a scan that
/// stopped at the top would never read it. A name that is not UTF-8 is keyed
/// as the empty string, which no `mod` line declares.
pub fn read_tree(base: &Path) -> std::io::Result<Tree> {
    let mut tree = Tree::default();
    collect(base, base, &mut tree)?;
    tree.rs.sort();
    tree.other.sort();
    Ok(tree)
}

fn collect(base: &Path, dir: &Path, tree: &mut Tree) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect(base, &path, tree)?;
            continue;
        }
        let name = path
            .strip_prefix(base)
            .unwrap_or(&path)
            .to_str()
            .unwrap_or_default()
            .to_string();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            tree.other.push(name);
            continue;
        }
        let text = std::fs::read_to_string(&path)?;
        tree.rs.push((name, strip_comments(&text)));
    }
    Ok(())
}

/// The text of `name` among `sources`, or the empty string if no file has that
/// key: an assertion that a construct is absent from a missing file then
/// fails on the construct the caller expected to find, not on a panic here.
pub fn source(sources: &[(String, String)], name: &str) -> String {
    sources
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, text)| text.clone())
        .unwrap_or_default()
}

/// Where a `mod` line declared inside `file` looks for its submodule; `main.rs`
/// is the crate root.
pub fn submodule_dir(file: &str) -> String {
    if file == "main.rs" {
        String::new()
    } else {
        format!("{}/", file.trim_end_matches(".rs"))
    }
}

/// The file every out-of-line `mod NAME;` among `sources` resolves to, in
/// order. `pub mod` and `pub(crate) mod` declare a module just as much; an
/// inline `mod tests {` block is already in its file and is not one.
///
/// Reading the directory only equals reading the crate if the two agree, so a
/// caller asserts each of these was scanned and each scanned file is one of
/// these: a `mod` whose file the scan missed — an `#[path]` attribute pointing
/// outside `src/`, say — would make every other assertion vacuous for exactly
/// the file that needed checking.
pub fn declared_modules(sources: &[(String, String)]) -> Vec<String> {
    let mut declared = Vec::new();
    for (path, text) in sources {
        for line in text.lines() {
            let mut line = line.trim();
            if let Some(unprefixed) = line.strip_prefix("pub") {
                let unprefixed = unprefixed.trim_start();
                line = match unprefixed.strip_prefix('(') {
                    Some(vis) => match vis.split_once(')') {
                        Some((_, after)) => after.trim_start(),
                        None => unprefixed,
                    },
                    None => unprefixed,
                };
            }
            let Some(rest) = line.strip_prefix("mod ") else {
                continue;
            };
            let Some(name) = rest.strip_suffix(';') else {
                continue;
            };
            declared.push(format!("{}{name}.rs", submodule_dir(path)));
        }
    }
    declared
}

/// The constructs that would make the scanned text stop describing the
/// compiled crate, squeezed of whitespace as `squeeze` leaves the text they
/// are counted in. A caller that reaches a sibling's file by `#[path]` on
/// purpose removes that one occurrence before counting.
pub const DECOUPLING: &[&str] = &["[path=", ",path=", "include!", "macro_rules!"];

/// The scans read tokens off raw text, so a comment BETWEEN two tokens slips
/// past all of them: `un`+`safe /* here */ {` is one construct to the compiler.
/// Rust's lexer is not reachable from a test, so this is the part that matters
/// — comments, and the literals a comment marker hides inside. A block comment
/// becomes one space plus the newlines it held, so the per-line scans still
/// see lines and the tokens either side stay separate tokens.
pub fn strip_comments(text: &str) -> String {
    let src: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let at = |i: usize| src.get(i).copied();
    while i < src.len() {
        let c = at(i).unwrap_or(' ');
        if c == '/' && at(i + 1) == Some('/') {
            while i < src.len() && at(i) != Some('\n') {
                i += 1;
            }
            continue;
        }
        if c == '/' && at(i + 1) == Some('*') {
            let mut depth = 1usize; // Rust's block comments nest.
            let mut newlines = 0usize;
            i += 2;
            while i < src.len() && depth > 0 {
                if at(i) == Some('/') && at(i + 1) == Some('*') {
                    depth += 1;
                    i += 2;
                } else if at(i) == Some('*') && at(i + 1) == Some('/') {
                    depth -= 1;
                    i += 2;
                } else {
                    if at(i) == Some('\n') {
                        newlines += 1;
                    }
                    i += 1;
                }
            }
            out.push(' ');
            for _ in 0..newlines {
                out.push('\n');
            }
            continue;
        }
        // Raw strings hold no escapes, so their terminator is the quote
        // followed by as many hashes as opened them. Modelling them is not
        // optional: a raw string containing an unbalanced `/*` would otherwise
        // open a block comment that swallows the rest of the file, hiding a
        // real unsafe block from the count while it still compiles.
        if c == 'r' || (c == 'b' && at(i + 1) == Some('r')) {
            let mut k = if c == 'b' { i + 2 } else { i + 1 };
            let mut hashes = 0usize;
            while at(k) == Some('#') {
                hashes += 1;
                k += 1;
            }
            if at(k) == Some('"') {
                for j in i..=k {
                    if let Some(ch) = at(j) {
                        out.push(ch);
                    }
                }
                i = k + 1;
                while let Some(ch) = at(i) {
                    if ch == '"' {
                        let mut seen = 0usize;
                        while seen < hashes && at(i + 1 + seen) == Some('#') {
                            seen += 1;
                        }
                        if seen == hashes {
                            for j in 0..=hashes {
                                if let Some(c2) = at(i + j) {
                                    out.push(c2);
                                }
                            }
                            i += hashes + 1;
                            break;
                        }
                    }
                    out.push(ch);
                    i += 1;
                }
                continue;
            }
        }
        if c == '"' {
            out.push(c);
            i += 1;
            while i < src.len() {
                let s = at(i).unwrap_or(' ');
                out.push(s);
                i += 1;
                if s == '\\' {
                    if let Some(escaped) = at(i) {
                        out.push(escaped);
                        i += 1;
                    }
                    continue;
                }
                if s == '"' {
                    break;
                }
            }
            continue;
        }
        // A quote opens a char literal or a lifetime. A lifetime is a quote, an
        // identifier, and NO closing quote; only a char literal can hold a
        // comment marker, and it is copied whole so `'/'` cannot open one.
        if c == '\'' {
            let ident = at(i + 1).is_some_and(|n| n.is_alphabetic() || n == '_');
            if !(ident && at(i + 2) != Some('\'')) {
                let mut k = i + 1;
                let mut close = None;
                while let Some(ch) = at(k) {
                    match ch {
                        '\\' => k += 2,
                        '\'' => {
                            close = Some(k);
                            break;
                        }
                        _ => k += 1,
                    }
                }
                if let Some(end) = close {
                    for j in i..=end {
                        if let Some(ch) = at(j) {
                            out.push(ch);
                        }
                    }
                    i = end + 1;
                    continue;
                }
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

/// What follows each `unsafe` keyword, with everything the compiler treats as
/// noise in between removed: any gap (one space, several, a newline, a
/// stripped comment, or none at all before a brace) and the `extern "ABI"` of
/// an unsafe foreign item. Reading the following TOKEN rather than matching a
/// fixed spelling is what stops this whole family of evasions — `unsafe  fn`
/// and `unsafe extern "C" fn` are the same item to rustc.
pub fn after_unsafe(text: &str) -> Vec<&str> {
    let word = "unsafe";
    let mut out = Vec::new();
    for (offset, _) in text.match_indices(word) {
        let Some(rest) = text.get(offset + word.len()..) else {
            continue;
        };
        // `unsafe_code` is ONE identifier; a brace or a gap means two tokens.
        if rest.starts_with(|c: char| c.is_alphanumeric() || c == '_') {
            continue;
        }
        let mut rest = rest.trim_start();
        while let Some(tail) = rest.strip_prefix("extern") {
            rest = tail.trim_start();
            let Some(tail) = rest.strip_prefix('"') else {
                break;
            };
            match tail.split_once('"') {
                Some((_, after)) => rest = after.trim_start(),
                None => break,
            }
        }
        out.push(rest);
    }
    out
}

/// The `unsafe { .. }` blocks in `text`.
pub fn unsafe_blocks(text: &str) -> usize {
    after_unsafe(text)
        .iter()
        .filter(|rest| rest.starts_with('{'))
        .count()
}

/// The `unsafe KEYWORD` items in `text`: `fn`, `impl`, `trait`.
pub fn unsafe_items(text: &str, keyword: &str) -> usize {
    after_unsafe(text)
        .iter()
        .filter(|rest| match rest.strip_prefix(keyword) {
            Some(after) => !after.starts_with(|c: char| c.is_alphanumeric() || c == '_'),
            None => false,
        })
        .count()
}

/// `text` with whitespace squeezed out. Whitespace is not a token boundary the
/// compiler cares about — `# [path`, `include !` and `macro_rules !` all
/// compile — so a construct refused by exact substring is one space away from
/// being allowed.
pub fn squeeze(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Every source squeezed, one per line, so a construct cannot be assembled
/// across the end of one file and the start of the next.
pub fn squeezed(sources: &[(String, String)]) -> String {
    let mut out = String::new();
    for (_, text) in sources {
        out.push_str(&squeeze(text));
        out.push('\n');
    }
    out
}

/// The `allow(..)` and `expect(..)` groups that name the unsafe lint anywhere
/// inside them, so a multi-lint allow is worth exactly as much as a lone one.
/// `expect` re-permits the lint exactly as `allow` does — only noisily when
/// unused. Whitespace before the group is legal Rust and is skipped rather
/// than assumed absent: an `#[allow (…)]` the scan walked past would be a
/// permission nobody sees.
pub fn unsafe_allows(text: &str) -> usize {
    let lint = "unsafe_code";
    let mut count = 0;
    for keyword in ["allow", "expect"] {
        for (offset, _) in text.match_indices(keyword) {
            let Some(rest) = text.get(offset + keyword.len()..) else {
                continue;
            };
            let Some(rest) = rest.trim_start().strip_prefix('(') else {
                continue;
            };
            let group = match rest.match_indices(')').next() {
                Some((end, _)) => rest.get(..end).unwrap_or(rest),
                None => rest,
            };
            if group
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .any(|t| t == lint)
            {
                count += 1;
            }
        }
    }
    count
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;

    #[test]
    fn comments_go_and_their_lines_stay() {
        assert_eq!(strip_comments("a // b\nc"), "a \nc");
        assert_eq!(strip_comments("a/* b */c"), "a c");
        assert_eq!(strip_comments("a/* b\n\nc */d"), "a \n\nd");
        // Nested: the first `*/` closes the inner comment only.
        assert_eq!(strip_comments("a /* x /* y */ z */ b"), "a   b");
        // Unterminated: the rest of the file was a comment.
        assert_eq!(strip_comments("a /* b"), "a  ");
    }

    #[test]
    fn literals_keep_the_markers_they_hold() {
        assert_eq!(strip_comments(r#"x("//") // c"#), r#"x("//") "#);
        assert_eq!(strip_comments(r#"x("\"/*") y"#), r#"x("\"/*") y"#);
        assert_eq!(strip_comments("let c = '/'; // c"), "let c = '/'; ");
        assert_eq!(strip_comments(r"let c = '\''; // c"), r"let c = '\''; ");
        // An unbalanced `/*` inside a raw string opens nothing.
        assert_eq!(strip_comments("r#\"/*\"# unsafe {}"), "r#\"/*\"# unsafe {}");
        assert_eq!(strip_comments("br\"/*\" x"), "br\"/*\" x");
        // A lifetime is not a char literal, so the comment after it still goes.
        assert_eq!(strip_comments("fn f<'a>() {} // c"), "fn f<'a>() {} ");
    }

    #[test]
    fn every_spelling_of_an_unsafe_block_or_item_counts() {
        let text = strip_comments("unsafe {} unsafe/**/{} unsafe\n{}");
        assert_eq!(unsafe_blocks(&text), 3);
        assert_eq!(unsafe_items("unsafe  fn f()", "fn"), 1);
        assert_eq!(unsafe_items("unsafe extern \"C\" fn f()", "fn"), 1);
        assert_eq!(unsafe_items("unsafe impl Send for X {}", "impl"), 1);
        // `unsafe fnord` is no `fn`, and `unsafe_code` is one identifier.
        assert_eq!(unsafe_items("unsafe fnord", "fn"), 0);
        assert_eq!(unsafe_blocks("#![forbid(unsafe_code)] {}"), 0);
    }

    #[test]
    fn every_permission_for_the_lint_counts_once() {
        assert_eq!(unsafe_allows("#[allow(unsafe_code)]"), 1);
        assert_eq!(unsafe_allows("#![allow (unsafe_code)]"), 1);
        assert_eq!(unsafe_allows("#[expect(dead_code, unsafe_code)]"), 1);
        assert_eq!(unsafe_allows("#[allow(dead_code)] #[deny(unsafe_code)]"), 0);
        assert_eq!(unsafe_allows("#[allow(unsafe_code_extra)]"), 0);
    }

    #[test]
    fn squeezing_closes_the_whitespace_gaps() {
        assert_eq!(squeeze("# [path = \"x\"]"), "#[path=\"x\"]");
        let sources = vec![
            ("a.rs".to_string(), "inc".to_string()),
            ("b.rs".to_string(), "lude !".to_string()),
        ];
        // One file per line: no construct straddles two files.
        assert_eq!(squeezed(&sources), "inc\nlude!\n");
        assert!(!squeezed(&sources).contains("include!"));
    }

    #[test]
    fn modules_resolve_beside_their_declaring_file() {
        let sources = vec![
            (
                "main.rs".to_string(),
                "mod a;\npub mod b;\npub(crate) mod c;\nmod tests {\n}".to_string(),
            ),
            ("a.rs".to_string(), "mod deep;".to_string()),
        ];
        assert_eq!(
            declared_modules(&sources),
            ["a.rs", "b.rs", "c.rs", "a/deep.rs"]
        );
        assert_eq!(source(&sources, "a.rs"), "mod deep;");
        assert_eq!(source(&sources, "missing.rs"), "");
    }

    #[test]
    fn the_tree_is_read_from_disk_relative_and_sorted() {
        let root = std::env::temp_dir().join(format!(
            "td-source-scan-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("main.rs"), "mod sub; // c\n").unwrap();
        std::fs::write(root.join("sub/deep.rs"), "x /* c */ y").unwrap();
        std::fs::write(root.join("sys.inc"), "").unwrap();
        let tree = read_tree(&root).unwrap();
        assert_eq!(
            tree.rs,
            [
                ("main.rs".to_string(), "mod sub; \n".to_string()),
                ("sub/deep.rs".to_string(), "x   y".to_string()),
            ]
        );
        assert_eq!(tree.other, ["sys.inc"]);
        std::fs::remove_dir_all(&root).unwrap();
        assert!(read_tree(&root).is_err());
    }
}

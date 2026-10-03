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
    let mut out: Vec<String> = Vec::new();
    for (_, literal) in embed_literals(&strip_comments(text)) {
        for dir in td_dirs_named(&literal) {
            if !out.contains(&dir) {
                out.push(dir);
            }
        }
    }
    out.sort();
    out
}

/// Each embed in `code`, which has had its comments cut: the byte where its
/// marker starts and the literal it carries, by the rules `td_dirs_embedded`
/// states, in marker then file order.
pub(crate) fn embed_literals(code: &str) -> Vec<(usize, String)> {
    let mut out: Vec<(usize, String)> = Vec::new();
    for (marker, attribute) in [
        ("#[path", true),
        ("include_str!", false),
        ("include_bytes!", false),
        ("include!", false),
    ] {
        let mut from = 0usize;
        while let Some(at) = code.get(from..).and_then(|rest| rest.find(marker)) {
            let start = from.saturating_add(at);
            let after = start.saturating_add(marker.len());
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
            out.push((start, literal.to_string()));
        }
    }
    out
}

/// The byte spans of `code` (comments cut) that an exact `#[cfg(test)]`
/// gates over a MODULE: an attribute first on its line, outside string and
/// char literals, whose item — past any further attributes and a
/// visibility — is `mod name;` or `mod name { ... }`, spanning to that `;`
/// or the `}` closing the block. What lies in one is compiled only into
/// this crate's tests. An attribute on anything else — a field, an arm, a
/// function — gates nothing here, nor does any other spelling,
/// `#[cfg(all(test, ...))]` included: a missed test embed only counts as a
/// real one.
pub(crate) fn cfg_test_spans(code: &str) -> Vec<(usize, usize)> {
    const ATTR: &str = "#[cfg(test)]";
    let mut out = Vec::new();
    let mut at = 0usize;
    while let Some(rest) = code.get(at..).filter(|r| !r.is_empty()) {
        if let Some(len) = literal_len_at(code, at) {
            at = at.saturating_add(len);
            continue;
        }
        let first_on_line = code
            .get(..at)
            .and_then(|before| before.rsplit('\n').next())
            .is_some_and(|line| line.trim().is_empty());
        if !(first_on_line && rest.starts_with(ATTR)) {
            at = at.saturating_add(rest.chars().next().map_or(1, char::len_utf8));
            continue;
        }
        let after = at.saturating_add(ATTR.len());
        if let Some(open) = module_body_at(code, after) {
            let end = match code.get(open..).and_then(|r| r.chars().next()) {
                Some('{') => block_end(code, open).unwrap_or(code.len()),
                _ => open,
            };
            out.push((at, end));
            at = end.max(after);
        } else {
            at = after;
        }
    }
    out
}

/// Where the `mod` item that starts at or after byte `from` of `code` opens
/// its body — the byte of its `;` or `{` — past whitespace, attributes and
/// a visibility; None where the next item is not a module.
fn module_body_at(code: &str, from: usize) -> Option<usize> {
    let skip_ws = |at: usize| {
        let rest = code.get(at..).unwrap_or("");
        at.saturating_add(rest.len().saturating_sub(rest.trim_start().len()))
    };
    let mut cur = skip_ws(from);
    while code.get(cur..)?.starts_with("#[") {
        let mut depth = 0usize;
        loop {
            if let Some(len) = literal_len_at(code, cur) {
                cur = cur.saturating_add(len);
                continue;
            }
            let c = code.get(cur..)?.chars().next()?;
            cur = cur.saturating_add(c.len_utf8());
            match c {
                '[' => depth = depth.saturating_add(1),
                ']' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
        }
        cur = skip_ws(cur);
    }
    let ident_len = |s: &str| {
        s.find(|c: char| !(c.is_alphanumeric() || c == '_'))
            .unwrap_or(s.len())
    };
    let rest = code.get(cur..)?;
    if rest.starts_with("pub") && ident_len(rest) == 3 {
        cur = skip_ws(cur.saturating_add(3));
        if code.get(cur..)?.starts_with('(') {
            cur = cur.saturating_add(code.get(cur..)?.find(')')?.saturating_add(1));
            cur = skip_ws(cur);
        }
    }
    let rest = code.get(cur..)?;
    if !(rest.starts_with("mod") && rest.get(3..)?.starts_with(char::is_whitespace)) {
        return None;
    }
    cur = skip_ws(cur.saturating_add(3));
    let rest = code.get(cur..)?;
    let raw = if rest.starts_with("r#") { 2 } else { 0 };
    let name = ident_len(rest.get(raw..)?);
    if name == 0 {
        return None;
    }
    cur = skip_ws(cur.saturating_add(raw).saturating_add(name));
    matches!(code.get(cur..)?.chars().next()?, ';' | '{').then_some(cur)
}

/// The length of the string or char literal that starts at byte `at` of
/// `code`, by the rules `strip_comments` steps over them with; None where
/// none starts there.
fn literal_len_at(code: &str, at: usize) -> Option<usize> {
    let rest = code.get(at..)?;
    let c = rest.chars().next()?;
    let ident_before = code
        .get(..at)
        .and_then(|s| s.chars().next_back())
        .is_some_and(|p| p.is_alphanumeric() || p == '_');
    if c == '"' || (c == 'r' && !ident_before) {
        string_literal_span(rest).map(|(_, len)| len)
    } else if c == '\'' {
        Some(char_literal_len(rest))
    } else {
        None
    }
}

/// The `td-*` directories `text` names outside its comments, its
/// `#[cfg(test)]` modules, and the literals of its production embeds: a
/// name there may be a read `td_files_embedded` cannot resolve to a file —
/// a `concat!` path, a `cfg_attr` mount — so it stands for its whole
/// directory.
pub(crate) fn td_dirs_named_beside_embeds(text: &str) -> Vec<String> {
    let code = strip_comments(text);
    let mut cut = cfg_test_spans(&code);
    for (at, literal) in embed_literals(&code) {
        if cut.iter().any(|(s, e)| *s <= at && at <= *e) {
            continue;
        }
        if let Some(i) = code.get(at..).and_then(|r| r.find(literal.as_str())) {
            let start = at.saturating_add(i);
            cut.push((start, start.saturating_add(literal.len())));
        }
    }
    td_dirs_named(&blank(&code, &cut))
}

/// `text` with its comments cut and its `#[cfg(test)]` modules blanked,
/// literals kept: the code a recipe's evaluation runs, and the embeds it
/// compiles in.
pub(crate) fn production_code(text: &str) -> String {
    let code = strip_comments(text);
    let spans = cfg_test_spans(&code);
    blank(&code, &spans)
}

/// `code` with each byte span in `spans` (inclusive ends) replaced by
/// spaces, so what is left keeps its token boundaries.
fn blank(code: &str, spans: &[(usize, usize)]) -> String {
    code.char_indices()
        .map(|(i, c)| {
            if spans.iter().any(|(s, e)| *s <= i && i <= *e) {
                ' '
            } else {
                c
            }
        })
        .collect()
}

/// What a recipe file's production code reads of its sibling recipes: the
/// modules among `modules` it names by a `super::`, `super::super::` or
/// `registry::` path, sorted; and whether it may read any of them in a way
/// this cannot follow — a glob or a group after one of those, or a catalog
/// lookup by name (`catalog::` but not `catalog::registry::`), or `super`,
/// or a path ending in `registry` or `catalog`, named other than as a
/// path's head, which an alias (`use super as s;`) would hide behind
/// (`pub(super)` excepted).
/// Comments, test modules and literals are cut first: a test's `use
/// super::*` names its own recipe, and a string that spells a path reads
/// nothing. Whitespace around `::` is dropped, so a path split across lines
/// reads as one.
pub(crate) fn sibling_reads(text: &str, modules: &[&str]) -> (Vec<String>, bool) {
    let code = squeeze_paths(&blank_literals(&production_code(text)));
    let ident_char = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut reads: Vec<String> = Vec::new();
    let mut wide = false;
    for word in ["super", "registry", "catalog"] {
        let mut from = 0usize;
        while let Some(i) = code.get(from..).and_then(|r| r.find(word)) {
            let start = from.saturating_add(i);
            from = start.saturating_add(word.len());
            let head = code.get(..start).unwrap_or("");
            let before = head.chars().next_back();
            let after = code.get(from..).unwrap_or("");
            if before.is_some_and(ident_char)
                || after.starts_with(ident_char)
                || after.starts_with("::")
                || (word == "super" && before == Some('(') && after.starts_with(')'))
                // `registry` and `catalog` are names a binding may take too;
                // only the module itself, at a path's tail, can be aliased.
                || (word != "super" && !head.ends_with("::"))
            {
                continue;
            }
            wide = true;
        }
    }
    for prefix in ["super::", "registry::"] {
        let mut from = 0usize;
        while let Some(i) = code.get(from..).and_then(|r| r.find(prefix)) {
            let start = from.saturating_add(i);
            from = start.saturating_add(prefix.len());
            if code
                .get(..start)
                .and_then(|s| s.chars().next_back())
                .is_some_and(ident_char)
            {
                continue;
            }
            let mut rest = code.get(from..).unwrap_or("");
            let mut depth = usize::from(prefix == "super::");
            while let Some(r) = rest.strip_prefix("super::") {
                rest = r;
                depth = depth.saturating_add(1);
            }
            // A glob, a group, a macro's `$name`: not a name this can follow.
            if !rest.starts_with(ident_char) {
                wide = true;
                continue;
            }
            let rest = rest.strip_prefix("r#").unwrap_or(rest);
            let name = rest
                .get(..rest.find(|c: char| !ident_char(c)).unwrap_or(rest.len()))
                .unwrap_or("");
            if modules.contains(&name) {
                if !reads.iter().any(|r| r == name) {
                    reads.push(name.to_string());
                }
            } else if depth >= 2 && name != "registry" {
                // Past the recipe's own parent, what is not a recipe module is
                // catalog's (`lookup`, `all`): a read by name.
                wide = true;
            }
        }
    }
    let mut from = 0usize;
    while let Some(i) = code.get(from..).and_then(|r| r.find("catalog::")) {
        let start = from.saturating_add(i);
        from = start.saturating_add("catalog::".len());
        let bounded = !code
            .get(..start)
            .and_then(|s| s.chars().next_back())
            .is_some_and(ident_char);
        if bounded
            && !code
                .get(from..)
                .is_some_and(|r| r.starts_with("registry::"))
        {
            wide = true;
        }
    }
    reads.sort();
    (reads, wide)
}

/// What a recipe file's production code may not name: anything that reads
/// or changes state outside the recipe value it builds. Every check run
/// constructs every recipe (`catalog::all`), while a check's verdict key
/// holds only its closure's recipe sources, so a constructor outside the
/// closure must not be able to change the run.
const IMPURE: &[&str] = &[
    "set_var",
    "remove_var",
    "env::var",
    "env::args",
    "env::current_dir",
    "set_current_dir",
    "env::temp_dir",
    "env::home_dir",
    "set_hook",
    "thread::",
    "net::",
    "io::stdin",
    "fs::",
    "File::",
    "OpenOptions",
    "Command::",
    "process::",
    "static mut",
    "OnceLock",
    "LazyLock",
    "thread_local!",
    "Cell<",
    "Mutex",
    "RwLock",
    "Atomic",
    "SystemTime",
    "Instant::",
    "unsafe",
];

/// The `IMPURE` names a recipe file's production code holds outside its
/// literals and comments, in `IMPURE` order. A spelling scan: an alias
/// (`use std::fs as f`) passes it, and review covers that.
pub(crate) fn impure_names(text: &str) -> Vec<&'static str> {
    let code = squeeze_paths(&blank_literals(&production_code(text)));
    IMPURE
        .iter()
        .copied()
        .filter(|name| code.contains(name))
        .collect()
}

/// `code` with its string and char literals blanked to spaces.
fn blank_literals(code: &str) -> String {
    let mut literals = Vec::new();
    let mut at = 0usize;
    while let Some(rest) = code.get(at..).filter(|r| !r.is_empty()) {
        if let Some(len) = literal_len_at(code, at) {
            literals.push((at, at.saturating_add(len).saturating_sub(1)));
            at = at.saturating_add(len.max(1));
        } else {
            at = at.saturating_add(rest.chars().next().map_or(1, char::len_utf8));
        }
    }
    blank(code, &literals)
}

/// Whether `code` (comments cut) embeds a file by a path this cannot read:
/// an embed marker in code, not inside a literal, that carries no literal
/// path (a `concat!` of pieces, a macro's argument).
pub(crate) fn has_computed_embed(code: &str) -> bool {
    let bare = blank_literals(code);
    let markers = ["#[path", "include_str!", "include_bytes!", "include!"]
        .iter()
        .map(|m| bare.matches(m).count())
        .sum::<usize>();
    let literal = embed_literals(code)
        .iter()
        .filter(|(at, _)| bare.get(*at..).is_some_and(|r| !r.starts_with(' ')))
        .count();
    markers > literal
}

/// `code` with the whitespace on either side of each `::` removed.
fn squeeze_paths(code: &str) -> String {
    let mut out = String::with_capacity(code.len());
    let mut rest = code;
    while let Some(i) = rest.find("::") {
        out.push_str(rest.get(..i).unwrap_or("").trim_end());
        out.push_str("::");
        rest = rest.get(i.saturating_add(2)..).unwrap_or("").trim_start();
    }
    out.push_str(rest);
    out
}

/// Whether `code` (comments cut) holds any embed marker at all, with a
/// literal after it or not: a file that may read another, which a nested
/// read through `concat!` or a macro would otherwise hide.
pub(crate) fn has_embed_marker(code: &str) -> bool {
    ["#[path", "include_str!", "include_bytes!", "include!"]
        .iter()
        .any(|marker| code.contains(marker))
}

/// The crate files a module of this crate at repository-relative directory
/// `dir` compiles in outside its tests: each `#[path]` or `include_*!`
/// literal not under a `#[cfg(test)]`, resolved against `dir` and normalized,
/// that lands under a `td-*` directory. A literal that leaves the repository
/// when resolved stands for the `td-*` directories it names instead, which
/// is the wider answer.
pub(crate) fn td_files_embedded(dir: &str, text: &str) -> Vec<String> {
    let code = strip_comments(text);
    let tests = cfg_test_spans(&code);
    let mut out: Vec<String> = Vec::new();
    for (at, literal) in embed_literals(&code) {
        if tests.iter().any(|(s, e)| *s <= at && at < *e) {
            continue;
        }
        let found = match normalize_join(dir, &literal) {
            Some(path) => {
                if path
                    .split('/')
                    .next()
                    .is_some_and(|top| top.starts_with("td-"))
                {
                    vec![path]
                } else {
                    Vec::new()
                }
            }
            None => td_dirs_named(&literal),
        };
        for path in found {
            if !out.contains(&path) {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// Whether `code` (comments cut) declares a module whose body is another
/// file — `mod name;` — and so reads more than its own bytes.
pub(crate) fn declares_out_of_line_module(code: &str) -> bool {
    let mut from = 0usize;
    while let Some(at) = code.get(from..).and_then(|rest| rest.find("mod")) {
        let start = from.saturating_add(at);
        from = start.saturating_add(3);
        let bounded = !code
            .get(..start)
            .and_then(|s| s.chars().next_back())
            .is_some_and(|c| c.is_alphanumeric() || c == '_')
            && code
                .get(from..)
                .is_some_and(|r| r.starts_with(char::is_whitespace));
        let rest = code.get(from..).unwrap_or("").trim_start();
        let rest = rest.strip_prefix("r#").unwrap_or(rest);
        let name_len = rest
            .find(|c: char| !(c.is_alphanumeric() || c == '_'))
            .unwrap_or(rest.len());
        let after = rest.get(name_len..).unwrap_or("").trim_start();
        if bounded && name_len > 0 && after.starts_with(';') {
            return true;
        }
    }
    false
}

/// `rel` joined onto repository-relative directory `dir` with `.` and `..`
/// resolved; None where the result would leave the repository or `rel` is
/// absolute.
pub(crate) fn normalize_join(dir: &str, rel: &str) -> Option<String> {
    if rel.starts_with('/') {
        return None;
    }
    let mut parts: Vec<&str> = Vec::new();
    for part in dir.split('/').chain(rel.split('/')) {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            p => parts.push(p),
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
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
/// `open` is not a `{` or the block never closes. For `cfg_test_spans`, and
/// the tests that read the evaluator's own sources by module.
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

    // So is the builder's include scan, which refuses a composed
    // `include_str!` path: a fixture that needs one splices this in.
    const STR: &str = "include_str!";

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

    /// Files are resolved against the including module's directory; a test
    /// module, a `#[cfg(test)]` item and a commented marker contribute
    /// nothing; a literal outside every `td-*` directory is not an embed;
    /// and one that would leave the repository widens to the directories it
    /// names.
    #[test]
    fn a_shared_embed_is_the_file_it_resolves_to_outside_the_tests() {
        let text = format!(
            "#[path = \"../../{a}src/a.rs\"]\n\
             pub mod a;\n\
             #[cfg(test)]\n\
             #[path = \"../../{b}src/b.rs\"]\n\
             mod b;\n\
             const C: &str = include_str!(\"../../{c}./x/../c.txt\");\n\
             // include_str!(\"../../{d}d.rs\")\n\
             #[cfg(test)]\n\
             mod tests {{\n    const E: &str = include_str!(\"../../{e}e.rs\");\n    \
             fn f() {{ let _ = '}}'; }}\n}}\n\
             const G: &[u8] = include_bytes!(\"../../{g}g.bin\");\n\
             const H: &str = include_str!(\"../../engine/src/h.rs\");\n\
             const I: &str = include_str!(\"../../../../{i}i.rs\");\n",
            a = d("a"),
            b = d("b"),
            c = d("c"),
            d = d("d"),
            e = d("e"),
            g = d("g"),
            i = d("i"),
        );
        let path = |dir: &str, rest: &str| [dir, rest].concat();
        assert_eq!(
            td_files_embedded("recipes/src", &text),
            vec![
                path(&d("a"), "src/a.rs"),
                path(&d("c"), "c.txt"),
                path(&d("g"), "g.bin"),
                d("i").trim_end_matches('/').to_string(),
            ]
        );
        assert_eq!(
            normalize_join("recipes/src", "../../x/./y"),
            Some("x/y".into())
        );
        assert_eq!(normalize_join("recipes/src", "../../../x"), None);
        assert_eq!(normalize_join("recipes/src", "/abs"), None);
    }

    /// A gated item ends at its `;` or at the brace closing its first block,
    /// with braces and semicolons inside literals stepped over.
    #[test]
    fn a_cfg_test_span_covers_the_item_it_heads() {
        let code = "#[cfg(test)]\n#[path = \"a;b\"]\nmod a;\nfn keep() {}\n\
                    #[cfg(test)]\nmod t { fn f() { let s = \"}\"; } }\nfn after() {}\n";
        let spans = cfg_test_spans(code);
        assert_eq!(spans.len(), 2, "{spans:?}");
        let text = |(s, e): (usize, usize)| &code[s..=e];
        assert_eq!(text(spans[0]), "#[cfg(test)]\n#[path = \"a;b\"]\nmod a;");
        assert!(text(spans[1]).ends_with("} }"), "{}", text(spans[1]));
        assert!(code[spans[1].1..].starts_with("}\nfn after"));
        // The attribute quoted in a string gates nothing after it.
        let quoted = "const NOTE: &str = \"#[cfg(test)]\";\n#[path = \"x.rs\"]\npub mod x;\n";
        assert!(cfg_test_spans(quoted).is_empty());
        let raw = "const R: &str = r#\"#[cfg(test)] mod t;\"#;\nmod y;\n";
        assert!(cfg_test_spans(raw).is_empty());
        // On a field, an arm or a function it heads no module, and the
        // mount after it stays production.
        for gated in [
            "struct S {\n    #[cfg(test)]\n    f: u8,\n}\n#[path = \"x.rs\"]\nmod x;\n",
            "match v {\n    #[cfg(test)]\n    1 => {}\n    _ => {}\n}\n#[path = \"x.rs\"]\nmod x;\n",
            "#[cfg(test)]\nfn f() {}\n#[path = \"x.rs\"]\nmod x;\n",
            "let a = 1; #[cfg(test)]\nmod t;\n",
        ] {
            assert!(cfg_test_spans(gated).is_empty(), "{gated}");
        }
        let visible = "#[cfg(test)]\n#[allow(dead_code)]\npub(crate) mod r#t;\n";
        assert_eq!(cfg_test_spans(visible).len(), 1);
    }

    /// A sibling read is a module named after `super::` (any depth) or
    /// `registry::`; a glob, a group or a catalog lookup reads any; a test
    /// module, a comment, a string or a non-module item reads nothing.
    #[test]
    fn a_recipe_reads_the_siblings_its_code_names() {
        let modules = ["firefox", "td_login", "mail", "news", "true"];
        let reads = |text: &str| sibling_reads(text, &modules);
        let none: (Vec<String>, bool) = (vec![], false);
        assert_eq!(
            reads("fn r() { let f = super::firefox::recipe; super::super::td_login::source(); }"),
            (vec!["firefox".to_string(), "td_login".to_string()], false)
        );
        assert_eq!(
            reads("fn r() { crate::catalog::registry::r#true::recipe(); }"),
            (vec!["true".to_string()], false)
        );
        assert_eq!(reads("use super::*;\n").1, true);
        assert_eq!(reads("use super::{firefox, mail};\n").1, true);
        assert_eq!(reads("fn r() { crate::catalog::lookup(\"x\"); }").1, true);
        // Whitespace around `::` is no way around it.
        assert_eq!(
            reads("fn r() { super ::\n    super:: mail::recipe(); }"),
            (vec!["mail".to_string()], false)
        );
        for text in [
            "use super:: *;\n",
            "use crate::catalog::registry:: {mail};\n",
            "use super as s;\nfn r() { s::mail::recipe(); }\n",
            "use crate::catalog::registry as reg;\n",
            "use crate::catalog;\n",
        ] {
            assert!(reads(text).1, "{text}");
        }
        assert!(reads("macro_rules! r { ($m:ident) => { super::$m::recipe() } }\n").1);
        for text in [
            "fn r() { super::super::lookup(\"mail\"); }\n",
            "fn r() { super::super::all(); }\n",
        ] {
            assert!(reads(text).1, "{text}");
        }
        assert!(!reads("fn r() { super::super::registry::mail::recipe(); }\n").1);
        // An alias the caller passes as a module reads like one.
        assert_eq!(
            sibling_reads("use super::super::outline_face;\n", &["outline_face"]),
            (vec!["outline_face".to_string()], false)
        );
        assert_eq!(reads("pub(super) fn r() {}\n"), none);
        assert_eq!(reads("fn r(registry: R) { registry.catalog }\n"), none);
        for text in [
            "#[cfg(test)]\nmod tests {\n    use super::*;\n    fn f() { super::super::mail::recipe(); }\n}\n",
            "// super::news::recipe\nfn r() {}\n",
            "const S: &str = \"super::news::recipe catalog::all\";\n",
            "fn r() { super::recipe(); xsuper::firefox::recipe(); }\n",
        ] {
            assert_eq!(reads(text), none, "{text}");
        }
    }

    /// A recipe's production code may name nothing that touches state
    /// beyond its value; a test module, a comment or a string may.
    #[test]
    fn a_recipe_constructor_names_nothing_impure() {
        assert_eq!(
            impure_names("fn r() { std::env::set_var(\"A\", \"\"); std::fs::read(\"x\"); }\n"),
            ["set_var", "fs::"]
        );
        assert_eq!(
            impure_names("fn r() { std :: process :: exit(1) }\n"),
            ["process::"]
        );
        for pure in [
            "#[cfg(test)]\nmod tests {\n    fn t() { std::fs::read(\"x\"); }\n}\n",
            "// std::fs::read\nfn r() {}\n",
            "const S: &str = \"Command::new(\\\"sh\\\")\";\n",
        ] {
            assert!(impure_names(pure).is_empty(), "{pure}");
        }
    }

    /// An embed whose path is computed is one this cannot follow; a literal
    /// one, or a marker spelled inside a string, is not.
    #[test]
    fn a_computed_embed_path_is_told_apart() {
        // Spelled apart: the builder refuses a composed include in any tree
        // file (`stage0::every_literal_include_in_the_tree_resolves`).
        let inc = ["include", "_str!"].concat();
        for computed in [
            format!("const A: &str = {inc}(concat!(env!(\"D\"), \"/x.mk\"));\n"),
            "macro_rules! e { ($p:expr) => { include_bytes!($p) } }\n".to_string(),
        ] {
            assert!(has_computed_embed(&computed), "{computed}");
        }
        for literal in [
            format!("const A: &str = {inc}(\"x.mk\");\n#[path = \"y.rs\"]\nmod y;\n"),
            format!("const S: &str = \"{inc}(concat!(a))\";\n"),
            "fn f() {}\n".to_string(),
        ] {
            assert!(!has_computed_embed(&literal), "{literal}");
        }
    }

    /// A name inside a production embed's literal or a test module is
    /// accounted for; one anywhere else in code is not.
    #[test]
    fn a_name_beside_the_embeds_is_left_for_its_directory() {
        let text = format!(
            "#[path = \"../../{a}src/a.rs\"]\npub mod a;\n\
             const B: &str = {STR}(concat!(\"../../{b}\", \"x\"));\n\
             #[cfg(test)]\nmod t {{ const C: &str = \"{c}y\"; }}\n\
             // {d}z\n",
            a = d("a"),
            b = d("b"),
            c = d("c"),
            d = d("d"),
        );
        assert_eq!(td_dirs_named_beside_embeds(&text), vec!["td-b"]);
    }

    /// Any marker counts, whether or not a literal follows it.
    #[test]
    fn an_embed_marker_counts_without_a_literal() {
        assert!(has_embed_marker(&format!(
            "const T: &str = {STR}(concat!(\"zone\", \".tab\"));"
        )));
        assert!(has_embed_marker("#[path = \"a.rs\"] mod a;"));
        assert!(!has_embed_marker("const T: &str = \"plain\";"));
    }

    #[test]
    fn an_out_of_line_module_is_a_declaration_ending_in_a_semicolon() {
        assert!(declares_out_of_line_module("pub mod a;\n"));
        assert!(declares_out_of_line_module("mod r#b ;"));
        assert!(declares_out_of_line_module("mod\tc;"));
        assert!(declares_out_of_line_module("mod\nd\n;"));
        assert!(!declares_out_of_line_module("mod t { fn f() {} }"));
        assert!(!declares_out_of_line_module("let unmod = 1; fn xmod () {}"));
        assert!(!declares_out_of_line_module("// none\n"));
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
        assert_eq!(
            td_dirs_named(&concat),
            vec!["td-x"],
            "the wide scan still sees it"
        );
        let comment = format!(
            "// include_str!(\"../{}a.rs\")\n/// #[path = \"{}b\"]\n",
            d("c"),
            d("c")
        );
        assert!(td_dirs_embedded(&comment).is_empty());
        assert!(td_dirs_named(&strip_comments(&comment)).is_empty());
    }

    #[test]
    fn a_comment_is_cut_and_a_slash_pair_inside_a_string_is_kept() {
        let text = format!(
            "let u = \"https://{}\"; // {}\nlet v = \"a \\\" // b\"; // c\n",
            d("k"),
            d("m")
        );
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
        assert_eq!(
            strip_comments("let s = \"/*\"; /* x\ny"),
            "let s = \"/*\"; \n"
        );
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

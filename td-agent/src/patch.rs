//! `apply_patch`'s grammar, Codex's (DESIGN.md §12): one string holding
//! a patch that adds, deletes, updates and moves files, parsed here and
//! applied by the tool host. Matching is exact, as `edit_file`'s is: each
//! hunk's lines must appear exactly once after the previous hunk's, so a
//! patch never lands in a place its author did not see.

use std::path::{Component, Path, PathBuf};

/// The patch's first line.
pub const BEGIN: &str = "*** Begin Patch";
/// The patch's last line.
pub const END: &str = "*** End Patch";
const ADD: &str = "*** Add File:";
const DELETE: &str = "*** Delete File:";
const UPDATE: &str = "*** Update File:";
const MOVE: &str = "*** Move to:";
const END_OF_FILE: &str = "*** End of File";
/// The most files one patch names, a move's target included.
pub const MAX_FILES: usize = 64;

/// One parsed patch, its operations in order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Patch {
    pub ops: Vec<Op>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Op {
    /// A new file of `lines`, each ended by a newline.
    Add {
        path: String,
        lines: Vec<String>,
    },
    Delete {
        path: String,
    },
    /// `path` changed by `hunks`, then moved to `to` when given.
    Update {
        path: String,
        to: Option<String>,
        hunks: Vec<Hunk>,
    },
}

/// One `@@` hunk: the lines it expects (`old`, context and removals) and
/// the lines it leaves (`new`, context and additions), after each line
/// `headers` names in turn, and at the file's end when `end`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Hunk {
    pub headers: Vec<String>,
    pub old: Vec<String>,
    pub new: Vec<String>,
    pub end: bool,
}

impl Op {
    /// The file the operation reads or creates.
    pub fn path(&self) -> &str {
        match self {
            Self::Add { path, .. } | Self::Delete { path } | Self::Update { path, .. } => path,
        }
    }
}

impl Patch {
    /// Every path the patch names, a move's target included, in order.
    pub fn paths(&self) -> Vec<&str> {
        let mut paths = Vec::new();
        for op in &self.ops {
            paths.push(op.path());
            if let Op::Update { to: Some(to), .. } = op {
                paths.push(to.as_str());
            }
        }
        paths
    }

    /// The existing files the patch changes, whose last read it must
    /// match: those it updates, moves or deletes.
    pub fn existing(&self) -> Vec<&str> {
        self.ops
            .iter()
            .filter(|op| !matches!(op, Op::Add { .. }))
            .map(Op::path)
            .collect()
    }
}

/// `path` as its components say it, `.` and repeated slashes dropped and
/// `..` taking back the component before: two spellings of one path a
/// patch names are one (a link is the tool host's to find).
pub fn lexical(path: &str) -> PathBuf {
    let mut out = PathBuf::new();
    for component in Path::new(path).components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// The patch's lines: a patch whose every ended line that is not empty
/// ends `\r\n` loses the `\r` (its last line may have no end, and blank
/// lines about it no `\r`); any other `\r` is part of its line. A heredoc
/// around the patch, as a shell would take it (`<<'EOF'` … `EOF`), and
/// blank lines before and after it, and inside the heredoc, are dropped.
fn lines_of(text: &str) -> Vec<(usize, &str)> {
    let ended = text.ends_with('\n');
    let body = text.strip_suffix('\n').unwrap_or(text);
    let mut lines: Vec<(usize, &str)> = body
        .split('\n')
        .enumerate()
        .map(|(index, line)| (index.saturating_add(1), line))
        .collect();
    let last = lines.len();
    let mut judged = lines
        .iter()
        .filter(|(at, line)| !line.is_empty() && (ended || *at < last))
        .peekable();
    if judged.peek().is_some() && judged.all(|(_, line)| line.ends_with('\r')) {
        for (_, line) in &mut lines {
            *line = line.strip_suffix('\r').unwrap_or(line);
        }
    }
    let trimmed = |lines: &[(usize, &'_ str)]| -> (usize, usize) {
        let blank = |line: &&(usize, &str)| line.1.trim().is_empty();
        let start = lines.iter().take_while(blank).count();
        let end = lines
            .len()
            .saturating_sub(lines.iter().rev().take_while(blank).count());
        (start, end.max(start))
    };
    let (start, end) = trimmed(&lines);
    let lines = lines.get(start..end).unwrap_or(&[]);
    let heredoc = lines.first().is_some_and(|(_, line)| {
        let line = line.trim_end();
        line.ends_with("<<'EOF'") || line.ends_with("<<\"EOF\"") || line.ends_with("<<EOF")
    }) && lines.last().is_some_and(|(_, line)| line.trim() == "EOF");
    let lines = if heredoc {
        let inner = lines.get(1..lines.len().saturating_sub(1)).unwrap_or(&[]);
        let (start, end) = trimmed(inner);
        inner.get(start..end).unwrap_or(&[])
    } else {
        lines
    };
    lines.to_vec()
}

/// Parses `text`, refusing anything the grammar does not hold, two
/// spellings of one path and more than `MAX_FILES` paths, each with what
/// was wrong and, where a line is to blame, which.
pub fn parse(text: &str) -> Result<Patch, String> {
    let all = lines_of(text);
    let mut lines = all.into_iter().peekable();
    match lines.next() {
        Some((_, line)) if line.trim_end() == BEGIN => {}
        _ => return Err(format!("a patch begins with the line {BEGIN:?}")),
    }
    let mut ops = Vec::new();
    let mut ended = false;
    while let Some((at, line)) = lines.next() {
        let marker = line.trim_end();
        if marker == END {
            ended = true;
            break;
        }
        if let Some(path) = marker.strip_prefix(ADD) {
            let mut added = Vec::new();
            while let Some((_, next)) = lines.peek() {
                let Some(text) = next.strip_prefix('+') else {
                    break;
                };
                added.push(text.to_string());
                lines.next();
            }
            ops.push(Op::Add {
                path: named(path, at)?,
                lines: added,
            });
        } else if let Some(path) = marker.strip_prefix(DELETE) {
            ops.push(Op::Delete {
                path: named(path, at)?,
            });
        } else if let Some(path) = marker.strip_prefix(UPDATE) {
            let path = named(path, at)?;
            let mut to = None;
            if let Some((at, next)) = lines.peek() {
                if let Some(target) = next.trim_end().strip_prefix(MOVE) {
                    to = Some(named(target, *at)?);
                    lines.next();
                }
            }
            let mut hunks: Vec<Hunk> = Vec::new();
            while let Some((at, next)) = lines.peek() {
                let at = *at;
                let next = *next;
                let marker = next.trim_end();
                if marker.starts_with("*** ") && marker != END_OF_FILE {
                    break;
                }
                lines.next();
                if marker == "@@" || marker.starts_with("@@ ") {
                    let header = marker
                        .strip_prefix("@@ ")
                        .map(str::to_string)
                        .filter(|h| !h.is_empty());
                    // `@@` lines in a row narrow one hunk, each after the
                    // one before.
                    match hunks.last_mut() {
                        Some(hunk) if hunk.old.is_empty() && hunk.new.is_empty() && !hunk.end => {
                            hunk.headers.extend(header);
                        }
                        _ => hunks.push(Hunk {
                            headers: header.into_iter().collect(),
                            ..Hunk::default()
                        }),
                    }
                    continue;
                }
                if marker == END_OF_FILE {
                    match hunks.last_mut() {
                        Some(hunk) if !hunk.end => hunk.end = true,
                        _ => return Err(format!("line {at}: {END_OF_FILE:?} ends no hunk")),
                    }
                    continue;
                }
                // The first hunk may begin without its `@@` line.
                if hunks.is_empty() {
                    hunks.push(Hunk::default());
                }
                let Some(hunk) = hunks.last_mut().filter(|hunk| !hunk.end) else {
                    return Err(format!(
                        "line {at}: a hunk line after {END_OF_FILE:?}; begin another hunk with @@"
                    ));
                };
                // An empty line is an empty context line.
                let (mark, rest) = match next.chars().next() {
                    None => (' ', ""),
                    Some(mark) => (mark, next.get(mark.len_utf8()..).unwrap_or("")),
                };
                match mark {
                    ' ' => {
                        hunk.old.push(rest.to_string());
                        hunk.new.push(rest.to_string());
                    }
                    '-' => hunk.old.push(rest.to_string()),
                    '+' => hunk.new.push(rest.to_string()),
                    _ => {
                        return Err(format!(
                            "line {at}: a hunk line begins with ' ', '-' or '+', not {mark:?}"
                        ))
                    }
                }
            }
            if let Some(empty) = hunks
                .iter()
                .position(|h| h.old.is_empty() && h.new.is_empty())
            {
                return Err(format!(
                    "{path}: hunk {} changes nothing",
                    empty.saturating_add(1)
                ));
            }
            if hunks.is_empty() && to.is_none() {
                return Err(format!("{path}: an update with no hunk and no move"));
            }
            ops.push(Op::Update { path, to, hunks });
        } else {
            return Err(format!(
                "line {at}: expected {ADD:?}, {DELETE:?}, {UPDATE:?} or {END:?}, found {line:?}"
            ));
        }
    }
    if !ended {
        return Err(format!("a patch ends with the line {END:?}"));
    }
    if let Some((at, _)) = lines.next() {
        return Err(format!("line {at}: nothing follows {END:?}"));
    }
    if ops.is_empty() {
        return Err("the patch changes no file".into());
    }
    let patch = Patch { ops };
    let paths = patch.paths();
    if paths.len() > MAX_FILES {
        return Err(format!(
            "the patch names {} files; a patch names at most {MAX_FILES}, a move's target included",
            paths.len()
        ));
    }
    let mut seen = std::collections::BTreeMap::new();
    for path in paths {
        if let Some(first) = seen.insert(lexical(path), path) {
            return Err(if first == path {
                format!("{path} is named twice; a patch changes each file once")
            } else {
                format!("{first} and {path} are one file; a patch changes each file once")
            });
        }
    }
    Ok(patch)
}

fn named(path: &str, at: usize) -> Result<String, String> {
    let path = path.trim();
    if path.is_empty() {
        return Err(format!("line {at}: names no file"));
    }
    Ok(path.to_string())
}

/// The text of a file `lines` adds: each line ended by a newline.
pub fn added(lines: &[String]) -> String {
    let mut text = String::new();
    for line in lines {
        text.push_str(line);
        text.push('\n');
    }
    text
}

/// Where in `lines`, from `cursor` on, `wanted` appears, each place.
fn places(lines: &[String], cursor: usize, wanted: &[String], end: bool) -> Vec<usize> {
    let rest = lines.get(cursor..).unwrap_or(&[]);
    rest.windows(wanted.len().max(1))
        .enumerate()
        .filter(|(_, window)| *window == wanted)
        .map(|(at, _)| cursor.saturating_add(at))
        .filter(|at| !end || at.saturating_add(wanted.len()) == lines.len())
        .collect()
}

/// `text` with `hunks` applied in order, or which hunk did not match and
/// why. A file whose every line ends `\r\n` is matched without the `\r`
/// and keeps it; whether the file ends with a newline is kept.
pub fn apply(path: &str, text: &str, hunks: &[Hunk]) -> Result<String, String> {
    let crlf = text.contains("\r\n") && !text.replace("\r\n", "").contains('\n');
    let plain = if crlf {
        text.replace("\r\n", "\n")
    } else {
        text.to_string()
    };
    let ends_newline = plain.is_empty() || plain.ends_with('\n');
    // Split on `\n` alone: `str::lines` would drop a lone `\r`.
    let body = plain.strip_suffix('\n').unwrap_or(&plain);
    let mut lines: Vec<String> = if plain.is_empty() {
        Vec::new()
    } else {
        body.split('\n').map(str::to_string).collect()
    };
    let mut cursor = 0usize;
    for (index, hunk) in hunks.iter().enumerate() {
        let which = index.saturating_add(1);
        for header in &hunk.headers {
            let found = places(&lines, cursor, std::slice::from_ref(header), false);
            match found.as_slice() {
                [at] => cursor = at.saturating_add(1),
                [] => {
                    return Err(format!(
                        "{path}: hunk {which}'s @@ line {header:?} is not in the file after the hunk before it{}; read the file again",
                        whitespace_hint(&lines, cursor, std::slice::from_ref(header))
                    ))
                }
                _ => {
                    return Err(format!(
                        "{path}: hunk {which}'s @@ line {header:?} appears {} times after the hunk before it; name a line that appears once",
                        found.len()
                    ))
                }
            }
        }
        let (at, old, new) = if hunk.old.is_empty() {
            // Nothing to find: an addition goes at the end, or after the
            // last header line.
            let at = if hunk.end || hunk.headers.is_empty() {
                lines.len()
            } else {
                cursor
            };
            (at, hunk.old.as_slice(), hunk.new.as_slice())
        } else {
            let mut old = hunk.old.as_slice();
            let mut new = hunk.new.as_slice();
            let mut found = places(&lines, cursor, old, hunk.end);
            // A blank line ending a hunk is often a separator, not
            // context: tried again without it, as Codex does.
            while found.is_empty()
                && old.last().is_some_and(String::is_empty)
                && new.last().is_some_and(String::is_empty)
            {
                old = old.split_last().map_or(old, |(_, rest)| rest);
                new = new.split_last().map_or(new, |(_, rest)| rest);
                if old.is_empty() {
                    break;
                }
                found = places(&lines, cursor, old, hunk.end);
            }
            match found.as_slice() {
                [at] => (*at, old, new),
                [] => {
                    let first = hunk.old.first().map(String::as_str).unwrap_or("");
                    return Err(format!(
                        "{path}: hunk {which}'s lines were not found{} after the hunk before it (it expects {} line(s), from {first:?}){}; read the file again and give its lines exactly",
                        if hunk.end { " at the end of the file" } else { "" },
                        hunk.old.len(),
                        whitespace_hint(&lines, cursor, &hunk.old)
                    ));
                }
                _ => {
                    return Err(format!(
                        "{path}: hunk {which}'s lines appear {} times after the hunk before it; give more context or an @@ line",
                        found.len()
                    ))
                }
            }
        };
        let end = at.saturating_add(old.len());
        if end > lines.len() {
            return Err(format!("{path}: hunk {which} runs past the file's end"));
        }
        lines.splice(at..end, new.iter().cloned());
        cursor = at.saturating_add(new.len());
    }
    let newline = if crlf { "\r\n" } else { "\n" };
    let mut out = lines.join(newline);
    if !lines.is_empty() && ends_newline {
        out.push_str(newline);
    }
    Ok(out)
}

/// Says when `wanted` would be in `lines` from `cursor` on but for each
/// line's leading and trailing whitespace, which must match exactly.
fn whitespace_hint(lines: &[String], cursor: usize, wanted: &[String]) -> &'static str {
    let rest = lines.get(cursor..).unwrap_or(&[]);
    let close = rest.windows(wanted.len().max(1)).any(|window| {
        window.len() == wanted.len()
            && window
                .iter()
                .zip(wanted)
                .all(|(have, want)| have.trim() == want.trim())
    });
    if close {
        " (they are there but for leading or trailing whitespace, which must match exactly, indentation included)"
    } else {
        ""
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
    use super::*;

    fn hunks(patch: &str) -> Vec<Hunk> {
        match parse(patch).unwrap().ops.remove(0) {
            Op::Update { hunks, .. } => hunks,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_grammar_parses_every_operation_in_order() {
        let patch = parse(
            "*** Begin Patch\n\
             *** Add File: /w/new.txt\n\
             +one\n\
             +two\n\
             *** Delete File: /w/old.txt\n\
             *** Update File: /w/a.rs\n\
             *** Move to: /w/b.rs\n\
             @@ fn main() {\n\
             -    old();\n\
             +    new();\n\
             \x20    kept();\n\
             *** End Patch\n",
        )
        .unwrap();
        assert_eq!(
            patch.ops,
            vec![
                Op::Add {
                    path: "/w/new.txt".into(),
                    lines: vec!["one".into(), "two".into()]
                },
                Op::Delete {
                    path: "/w/old.txt".into()
                },
                Op::Update {
                    path: "/w/a.rs".into(),
                    to: Some("/w/b.rs".into()),
                    hunks: vec![Hunk {
                        headers: vec!["fn main() {".into()],
                        old: vec!["    old();".into(), "    kept();".into()],
                        new: vec!["    new();".into(), "    kept();".into()],
                        end: false,
                    }]
                },
            ]
        );
        assert_eq!(
            patch.paths(),
            ["/w/new.txt", "/w/old.txt", "/w/a.rs", "/w/b.rs"]
        );
        assert_eq!(patch.existing(), ["/w/old.txt", "/w/a.rs"]);
    }

    #[test]
    fn malformed_patches_say_what_and_where() {
        for (patch, says) in [
            ("", "begins with"),
            ("*** Begin Patch\n*** Add File: /a\n+x\n", "ends with"),
            ("*** Begin Patch\n*** End Patch\n", "changes no file"),
            ("*** Begin Patch\nhello\n*** End Patch\n", "line 2: expected"),
            ("*** Begin Patch\n*** Add File: \n*** End Patch\n", "line 2: names no file"),
            (
                "*** Begin Patch\n*** Update File: /a\n@@\n x\n*** End of File\n y\n*** End Patch\n",
                "after \"*** End of File\"",
            ),
            (
                "*** Begin Patch\n*** Update File: /a\n@@\n?x\n*** End Patch\n",
                "line 4: a hunk line begins",
            ),
            ("*** Begin Patch\n*** Update File: /a\n*** End Patch\n", "no hunk and no move"),
            ("*** Begin Patch\n*** Update File: /a\n@@\n*** End Patch\n", "changes nothing"),
            (
                "*** Begin Patch\n*** Delete File: /a\n*** Delete File: /a\n*** End Patch\n",
                "named twice",
            ),
            (
                "*** Begin Patch\n*** Update File: /a\n*** Move to: /a\n*** End Patch\n",
                "named twice",
            ),
            ("*** Begin Patch\n*** Delete File: /a\n*** End Patch\nmore\n", "nothing follows"),
        ] {
            let error = parse(patch).unwrap_err();
            assert!(error.contains(says), "{patch:?}: {error}");
        }
        let many: String = (0..=MAX_FILES)
            .map(|n| format!("*** Delete File: /f{n}\n"))
            .collect();
        let error = parse(&format!("{BEGIN}\n{many}{END}\n")).unwrap_err();
        assert!(error.contains("at most"), "{error}");
    }

    #[test]
    fn hunks_apply_exactly_once_each_after_the_one_before() {
        let text = "a\nb\nc\nb\nd\n";
        // The second `b` is after the first hunk, so it is the one changed.
        let applied = apply(
            "/f",
            text,
            &hunks("*** Begin Patch\n*** Update File: /f\n@@\n a\n-b\n+B\n@@\n c\n-b\n+X\n*** End Patch"),
        )
        .unwrap();
        assert_eq!(applied, "a\nB\nc\nX\nd\n");
        // Ambiguous: `b` alone appears twice.
        let error = apply(
            "/f",
            text,
            &hunks("*** Begin Patch\n*** Update File: /f\n-b\n+B\n*** End Patch"),
        )
        .unwrap_err();
        assert!(error.contains("appear 2 times"), "{error}");
        // An @@ line narrows it.
        let applied = apply(
            "/f",
            text,
            &hunks("*** Begin Patch\n*** Update File: /f\n@@ c\n-b\n+B\n*** End Patch"),
        )
        .unwrap();
        assert_eq!(applied, "a\nb\nc\nB\nd\n");
        // Not fuzzy: a trailing space is a different line.
        let error = apply(
            "/f",
            text,
            &hunks("*** Begin Patch\n*** Update File: /f\n-d \n+D\n*** End Patch"),
        )
        .unwrap_err();
        assert!(error.contains("were not found"), "{error}");
        // The end of the file, and an addition there.
        let applied = apply(
            "/f",
            text,
            &hunks("*** Begin Patch\n*** Update File: /f\n@@\n-b\n+Z\n d\n*** End of File\n*** End Patch"),
        )
        .unwrap();
        assert_eq!(applied, "a\nb\nc\nZ\nd\n");
        let applied = apply(
            "/f",
            text,
            &hunks("*** Begin Patch\n*** Update File: /f\n@@\n+e\n*** End Patch"),
        )
        .unwrap();
        assert_eq!(applied, "a\nb\nc\nb\nd\ne\n");
    }

    #[test]
    fn line_endings_and_a_missing_final_newline_are_kept() {
        let patch = hunks("*** Begin Patch\n*** Update File: /f\n-b\n+B\n*** End Patch");
        assert_eq!(
            apply("/f", "a\r\nb\r\nc\r\n", &patch).unwrap(),
            "a\r\nB\r\nc\r\n"
        );
        assert_eq!(apply("/f", "a\nb\nc", &patch).unwrap(), "a\nB\nc");
        // Mixed endings are matched as they are.
        let error = apply("/f", "a\r\nb\r\nc\n", &patch).unwrap_err();
        assert!(error.contains("were not found"), "{error}");
        assert_eq!(added(&["x".into(), "".into()]), "x\n\n");
    }

    #[test]
    fn what_models_emit_is_read_and_aliases_are_one_file() {
        // Two spellings of one path are one file.
        for (patch, says) in [
            (
                "*** Begin Patch\n*** Delete File: /w/a\n*** Delete File: /w/./a\n*** End Patch\n",
                "/w/a and /w/./a are one file",
            ),
            (
                "*** Begin Patch\n*** Delete File: /w//a\n*** Update File: /w/b/../a\n-x\n*** End Patch\n",
                "are one file",
            ),
        ] {
            let error = parse(patch).unwrap_err();
            assert!(error.contains(says), "{patch:?}: {error}");
        }
        // The bound counts a move's target.
        let moves: String = (0..=MAX_FILES / 2)
            .map(|n| format!("*** Update File: /f{n}\n*** Move to: /g{n}\n"))
            .collect();
        let error = parse(&format!("{BEGIN}\n{moves}{END}\n")).unwrap_err();
        assert!(error.contains("a move's target included"), "{error}");
        // A heredoc around it, blank lines about it, CRLF throughout.
        let wrapped =
            "\napply_patch <<'EOF'\n\n*** Begin Patch\n*** Delete File: /w/a\n*** End Patch\n\nEOF\n\n";
        assert_eq!(parse(wrapped).unwrap().paths(), ["/w/a"]);
        // CRLF with no end to its last line, or a blank line after it.
        for crlf in [
            "*** Begin Patch\r\n*** Update File: /f\r\n-b\r\n+B\r\n*** End Patch",
            "*** Begin Patch\r\n*** Update File: /f\r\n-b\r\n+B\r\n*** End Patch\r\n\n",
        ] {
            assert_eq!(
                apply("/f", "a\nb\n", &hunks(crlf)).unwrap(),
                "a\nB\n",
                "{crlf:?}"
            );
        }
        let crlf = "*** Begin Patch\r\n*** Update File: /w/a\r\n-x\r\n+y\r\n*** End Patch\r\n";
        assert_eq!(
            hunks(crlf),
            [Hunk {
                old: vec!["x".into()],
                new: vec!["y".into()],
                ..Hunk::default()
            }]
        );
        // A `\r` in a patch that is not CRLF throughout is its line's, and
        // matches a file's lone `\r`.
        let lone = hunks("*** Begin Patch\n*** Update File: /f\n-a\r\n+A\n*** End Patch\n");
        assert_eq!(apply("/f", "a\r\nb\n", &lone).unwrap(), "A\nb\n");
        // `@@` lines in a row narrow one hunk in turn.
        let nested = hunks(
            "*** Begin Patch\n*** Update File: /f\n@@ impl B\n@@ fn f\n-x\n+X\n*** End Patch\n",
        );
        assert_eq!(nested.len(), 1);
        assert_eq!(
            apply("/f", "impl A\nfn f\nx\nimpl B\nfn f\nx\n", &nested).unwrap(),
            "impl A\nfn f\nx\nimpl B\nfn f\nX\n"
        );
        // An addition after an @@ line goes just below it.
        let below = hunks("*** Begin Patch\n*** Update File: /f\n@@ impl B\n+new\n*** End Patch\n");
        assert_eq!(
            apply("/f", "impl A\nimpl B\nend\n", &below).unwrap(),
            "impl A\nimpl B\nnew\nend\n"
        );
        // A blank line ending a hunk, a separator, is tried without.
        let blank = hunks("*** Begin Patch\n*** Update File: /f\n x\n-y\n+Y\n\n*** End Patch\n");
        assert_eq!(apply("/f", "x\ny\n", &blank).unwrap(), "x\nY\n");
        // Indentation must match, and the refusal says it is what differs.
        let indented =
            hunks("*** Begin Patch\n*** Update File: /f\n-fn f() {}\n+fn g() {}\n*** End Patch\n");
        let error = apply("/f", "    fn f() {}\n", &indented).unwrap_err();
        assert!(error.contains("indentation"), "{error}");
        let error = apply("/f", "fn h() {}\n", &indented).unwrap_err();
        assert!(!error.contains("indentation"), "{error}");
    }
}

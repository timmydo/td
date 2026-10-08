//! A step's tool activity as the transcript shows it by default (DESIGN.md
//! §4): one summary line for a reply's calls ("read 4 files · grep ·
//! edited tools.rs (+12 −3)") and, behind it, each call with its edits
//! as bounded diffs. And what stands for a reply's text when it has none
//! but reasoning: its summary.

use crate::store::Call;
use crate::tools::visible;
use td_json::Json;

/// The most bytes of a summary line, within td-ui's label bound.
pub const MAX_SUMMARY: usize = 120;
/// The most lines one call's detail shows.
const CALL_LINES: usize = 40;
/// The most lines a step's detail shows, all its calls together.
const STEP_LINES: usize = 240;
/// The most characters of a line in the detail.
const LINE_CHARS: usize = 200;
/// The most characters of a command a summary names.
const COMMAND_CHARS: usize = 24;
/// The most characters of reasoning standing for a reply's text.
const REASONING_CHARS: usize = 300;

/// What one call did to the summary: a group it joins, by kind.
enum Part {
    Read(String),
    Search(String),
    Edit {
        file: String,
        added: usize,
        removed: usize,
        verb: &'static str,
    },
    Run(String),
    Other(String),
}

fn arguments(call: &Call) -> Option<Json> {
    td_json::parse(&call.arguments).ok()
}

fn text(value: &Option<Json>, name: &str) -> Option<String> {
    value
        .as_ref()
        .and_then(|v| v.get(name))
        .and_then(Json::as_str)
        .map(str::to_string)
}

/// The paths a call's `paths` array names.
fn paths(value: &Option<Json>) -> Vec<String> {
    value
        .as_ref()
        .and_then(|v| v.get("paths"))
        .and_then(Json::as_arr)
        .map(|all| {
            all.iter()
                .filter_map(Json::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The last component of `path`, as a summary names a file.
fn base(path: &str) -> String {
    let name = path
        .rsplit('/')
        .find(|part| !part.is_empty())
        .unwrap_or(path);
    visible(name)
}

/// The lines a text has, an empty one none.
fn lines(text: &str) -> usize {
    text.lines().count()
}

fn cut(text: &str, chars: usize) -> String {
    let mut out: String = text.chars().take(chars).collect();
    if text.chars().count() > chars {
        out.push('…');
    }
    out
}

/// A patch's parts, from its own lines: each file it adds, deletes or
/// updates, by the path last named, with the lines its hunks mark added
/// and removed. Parsed hunks keep no markers, and a line both kept and
/// removed would count as neither.
fn patch_parts(input: &str) -> Vec<Part> {
    let mut parts: Vec<Part> = Vec::new();
    for line in input.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let opened = [
            ("*** Add File:", "added"),
            ("*** Delete File:", "deleted"),
            ("*** Update File:", "edited"),
        ]
        .iter()
        .find_map(|(marker, verb)| line.strip_prefix(marker).map(|path| (path, *verb)));
        if let Some((path, verb)) = opened {
            parts.push(Part::Edit {
                file: path.trim().to_string(),
                added: 0,
                removed: 0,
                verb,
            });
            continue;
        }
        let Some(Part::Edit {
            file,
            added,
            removed,
            verb,
        }) = parts.last_mut()
        else {
            continue;
        };
        if let Some(to) = line.strip_prefix("*** Move to:") {
            *file = to.trim().to_string();
            *verb = "moved";
        } else if line.starts_with("***") {
        } else if line.starts_with('+') {
            *added += 1;
        } else if line.starts_with('-') {
            *removed += 1;
        }
    }
    parts
}

/// What a call adds to the summary, by whole path; a call whose
/// arguments name nothing is said by its tool's name.
fn parts(call: &Call) -> Vec<Part> {
    let value = arguments(call);
    let path = || text(&value, "path").unwrap_or_default();
    let mut parts = match call.name.as_str() {
        "read_file" => vec![Part::Read(path())],
        "sed" => paths(&value)
            .into_iter()
            .map(|file| Part::Edit {
                file,
                added: 0,
                removed: 0,
                verb: "edited",
            })
            .collect(),
        "glob" | "grep" => vec![Part::Search(call.name.clone())],
        "edit_file" => {
            let old = text(&value, "old_string").unwrap_or_default();
            let new = text(&value, "new_string").unwrap_or_default();
            vec![Part::Edit {
                file: path(),
                added: lines(&new),
                removed: lines(&old),
                verb: "edited",
            }]
        }
        "write_file" => vec![Part::Edit {
            file: path(),
            added: lines(&text(&value, "content").unwrap_or_default()),
            removed: 0,
            verb: "wrote",
        }],
        "apply_patch" => patch_parts(&text(&value, "input").unwrap_or_default()),
        "shell" => {
            let command = text(&value, "command").unwrap_or_default();
            let first = command.lines().next().unwrap_or_default();
            // One left running is said as started: its result, which
            // says so, is folded away.
            let background = value
                .as_ref()
                .and_then(|v| v.get("background"))
                .and_then(Json::as_bool)
                == Some(true);
            let verb = if background { "started" } else { "ran" };
            vec![Part::Run(format!(
                "{verb} {}",
                visible(&cut(first.trim(), COMMAND_CHARS))
            ))]
        }
        _ => Vec::new(),
    };
    // A file with no name is no file.
    parts.retain(|part| match part {
        Part::Read(file) | Part::Edit { file, .. } => !file.trim().is_empty(),
        _ => true,
    });
    if parts.is_empty() {
        vec![Part::Other(visible(&call.name))]
    } else {
        parts
    }
}

/// One line for a step's calls, in the order their kinds first came,
/// within `MAX_SUMMARY` bytes: reads counted, searches by tool, each file
/// changed with its lines added and removed, commands by their start.
pub fn summary(calls: &[Call]) -> String {
    let mut reads: Vec<String> = Vec::new();
    let mut searches: Vec<(String, usize)> = Vec::new();
    let mut edits: Vec<(String, usize, usize, &'static str)> = Vec::new();
    let mut runs: Vec<String> = Vec::new();
    let mut others: Vec<(String, usize)> = Vec::new();
    // The kinds in the order they first came: 0 reads, 1 searches by
    // name, 2 an edit by file, 3 a run, 4 another by name.
    let mut order: Vec<(u8, String)> = Vec::new();
    let first = |kind: u8, key: &str, order: &mut Vec<(u8, String)>| {
        if !order.iter().any(|(k, n)| *k == kind && n == key) {
            order.push((kind, key.to_string()));
        }
    };
    for call in calls {
        for part in parts(call) {
            match part {
                Part::Read(file) => {
                    first(0, "", &mut order);
                    if !reads.contains(&file) {
                        reads.push(file);
                    }
                }
                Part::Search(name) => {
                    first(1, &name, &mut order);
                    match searches.iter_mut().find(|(n, _)| *n == name) {
                        Some((_, count)) => *count += 1,
                        None => searches.push((name, 1)),
                    }
                }
                Part::Edit {
                    file,
                    added,
                    removed,
                    verb,
                } => {
                    first(2, &file, &mut order);
                    match edits.iter_mut().find(|(f, ..)| *f == file) {
                        Some((_, a, r, v)) => {
                            *a += added;
                            *r += removed;
                            // A file added or written and then edited
                            // is still what was done first.
                            if *v == "edited" {
                                *v = verb;
                            }
                        }
                        None => edits.push((file, added, removed, verb)),
                    }
                }
                Part::Run(command) => {
                    first(3, &command, &mut order);
                    runs.push(command);
                }
                Part::Other(name) => {
                    first(4, &name, &mut order);
                    match others.iter_mut().find(|(n, _)| *n == name) {
                        Some((_, count)) => *count += 1,
                        None => others.push((name, 1)),
                    }
                }
            }
        }
    }
    let times = |name: &str, count: usize| {
        if count > 1 {
            format!("{name} ×{count}")
        } else {
            name.to_string()
        }
    };
    let mut said: Vec<String> = Vec::new();
    for (kind, key) in &order {
        let piece = match kind {
            0 => match reads.as_slice() {
                [one] => format!("read {}", base(one)),
                all => format!("read {} files", all.len()),
            },
            1 => searches
                .iter()
                .find(|(n, _)| n == key)
                .map(|(n, c)| times(n, *c))
                .unwrap_or_default(),
            2 => edits
                .iter()
                .find(|(f, ..)| f == key)
                .map(|(file, added, removed, verb)| {
                    let file = base(file);
                    match *verb {
                        "deleted" => format!("deleted {file}"),
                        _ if *added == 0 && *removed == 0 => format!("{verb} {file}"),
                        _ => format!("{verb} {file} (+{added} −{removed})"),
                    }
                })
                .unwrap_or_default(),
            3 => {
                let count = runs.iter().filter(|c| *c == key).count();
                times(key, count)
            }
            _ => others
                .iter()
                .find(|(n, _)| n == key)
                .map(|(n, c)| times(n, *c))
                .unwrap_or_default(),
        };
        said.push(piece);
    }
    let line = said.join(" · ");
    // Never empty, which a section's title may not be.
    if line.trim().is_empty() {
        return "tool calls".into();
    }
    bounded(&line, MAX_SUMMARY)
}

/// `text` within `max` bytes, cut at a character with `…` when longer.
fn bounded(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max.saturating_sub('…'.len_utf8());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", text.get(..end).unwrap_or_default())
}

/// Each of a step's calls, a line naming it and then, for an edit, its
/// lines removed and added, for a command the command, each call within
/// `CALL_LINES` and the whole within `STEP_LINES`, every line made
/// visible and cut at `LINE_CHARS`.
pub fn detail(calls: &[Call]) -> String {
    let mut out: Vec<String> = Vec::new();
    for call in calls {
        let value = arguments(call);
        let path = text(&value, "path").map(|p| visible(&p));
        let name = visible(&call.name);
        out.push(cut(
            &match &path {
                Some(path) => format!("{name} {path}"),
                None => name,
            },
            LINE_CHARS,
        ));
        let mut body: Vec<String> = Vec::new();
        match call.name.as_str() {
            "edit_file" => {
                let old = text(&value, "old_string").unwrap_or_default();
                let new = text(&value, "new_string").unwrap_or_default();
                body.extend(old.lines().map(|l| format!("-{l}")));
                body.extend(new.lines().map(|l| format!("+{l}")));
            }
            "sed" => {
                body.extend(
                    text(&value, "script")
                        .unwrap_or_default()
                        .lines()
                        .map(|l| format!("script {l}")),
                );
                body.extend(paths(&value).iter().map(|p| format!("file {p}")));
            }
            "write_file" => {
                let content = text(&value, "content").unwrap_or_default();
                body.extend(content.lines().map(|l| format!("+{l}")));
            }
            "apply_patch" => {
                let input = text(&value, "input").unwrap_or_default();
                body.extend(
                    input
                        .lines()
                        .filter(|l| {
                            !l.starts_with("*** Begin Patch") && !l.starts_with("*** End Patch")
                        })
                        .map(str::to_string),
                );
            }
            "shell" => {
                let command = text(&value, "command").unwrap_or_default();
                body.extend(command.lines().map(|l| format!("$ {l}")));
            }
            "read_file" | "glob" | "grep" if path.is_some() => {
                if let Some(pattern) = text(&value, "pattern") {
                    body.push(format!("pattern {pattern}"));
                }
            }
            _ => {
                if path.is_none() {
                    body.push(call.arguments.clone());
                }
            }
        }
        let more = body.len().saturating_sub(CALL_LINES);
        out.extend(
            body.into_iter()
                .take(CALL_LINES)
                .map(|l| format!("  {}", cut(&visible(&l), LINE_CHARS))),
        );
        if more > 0 {
            out.push(format!("  … {more} more lines"));
        }
    }
    let more = out.len().saturating_sub(STEP_LINES);
    out.truncate(STEP_LINES);
    if more > 0 {
        out.push(format!("… {more} more lines"));
    }
    out.join("\n")
}

/// What stands for a reply's text when it has none: the summaries its
/// reasoning details carry, or else the last paragraph of its reasoning,
/// within `REASONING_CHARS`.
pub fn reasoning_summary(details: Option<&str>, reasoning: Option<&str>) -> Option<String> {
    let summaries: Vec<String> = details
        .and_then(|d| td_json::parse(d).ok())
        .and_then(|value| {
            value.as_arr().map(|all| {
                all.iter()
                    .filter(|one| {
                        one.get("type").and_then(Json::as_str) == Some("reasoning.summary")
                    })
                    .filter_map(|one| one.get("summary").and_then(Json::as_str))
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            })
        })
        .unwrap_or_default();
    let text = if summaries.is_empty() {
        reasoning?
            .split("\n\n")
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .last()?
            .to_string()
    } else {
        summaries.join("\n\n")
    };
    Some(cut(&text, REASONING_CHARS))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn call(name: &str, arguments: Json) -> Call {
        Call {
            id: "c".into(),
            name: name.into(),
            arguments: arguments.to_string(),
        }
    }

    fn args(pairs: &[(&str, &str)]) -> Json {
        Json::Obj(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), Json::Str(v.to_string())))
                .collect(),
        )
    }

    #[test]
    fn a_steps_calls_are_one_line_in_the_order_they_came() {
        let calls = vec![
            call("read_file", args(&[("path", "/w/a.rs")])),
            call("read_file", args(&[("path", "/w/b.rs")])),
            call("grep", args(&[("pattern", "x")])),
            call("read_file", args(&[("path", "/w/c.rs")])),
            call(
                "edit_file",
                args(&[
                    ("path", "/w/src/tools.rs"),
                    ("old_string", "a\nb\nc"),
                    ("new_string", "a\nB"),
                ]),
            ),
            call("grep", args(&[("pattern", "y")])),
            call(
                "shell",
                args(&[(
                    "command",
                    "cargo test --manifest-path td-agent/Cargo.toml\nmore",
                )]),
            ),
            call("todo_write", Json::Obj(Vec::new())),
        ];
        assert_eq!(
            summary(&calls),
            "read 3 files · grep ×2 · edited tools.rs (+2 −3) · ran cargo test --manifest-pa… · todo_write"
        );
        assert_eq!(
            summary(&[call("read_file", args(&[("path", "/w/a.rs")]))]),
            "read a.rs"
        );
        // A sed script names each file it was run over.
        let sed = Json::Obj(vec![
            ("script".into(), Json::Str("s/a/b/".into())),
            (
                "paths".into(),
                Json::Arr(vec![
                    Json::Str("/w/x.rs".into()),
                    Json::Str("/w/y.rs".into()),
                ]),
            ),
        ]);
        assert_eq!(
            summary(&[call("sed", sed.clone())]),
            "edited x.rs · edited y.rs"
        );
        assert_eq!(
            detail(&[call("sed", sed)]),
            "sed\n  script s/a/b/\n  file /w/x.rs\n  file /w/y.rs"
        );
        // Files are told apart by whole path, though named by their last
        // part; a call with nothing to name is said by its tool.
        assert_eq!(
            summary(&[
                call("read_file", args(&[("path", "/w/a/lib.rs")])),
                call("read_file", args(&[("path", "/w/b/lib.rs")])),
                call("sed", args(&[("script", "s/a/b/")])),
            ]),
            "read 2 files · sed"
        );
        assert_eq!(summary(&[call("", Json::Obj(Vec::new()))]), "tool calls");
        assert_eq!(
            summary(&[call("edit_file", Json::Obj(Vec::new()))]),
            "edit_file"
        );
        let two = Json::Obj(vec![
            ("script".into(), Json::Str("s/a/b/".into())),
            (
                "paths".into(),
                Json::Arr(vec![
                    Json::Str("/a/lib.rs".into()),
                    Json::Str("/b/lib.rs".into()),
                ]),
            ),
        ]);
        assert_eq!(
            summary(&[call("sed", two)]),
            "edited lib.rs · edited lib.rs"
        );
        let tight = "*** Begin Patch\n*** Update File:/w/t.rs\n@@\n-a\n+b\n*** End Patch\n";
        assert_eq!(
            summary(&[call("apply_patch", args(&[("input", tight)]))]),
            "edited t.rs (+1 −1)"
        );
        let started = Json::Obj(vec![
            ("command".into(), Json::Str("make serve".into())),
            ("background".into(), Json::Bool(true)),
        ]);
        assert_eq!(summary(&[call("shell", started)]), "started make serve");
        // A patch's lines are counted by their marks, a removed line
        // that is also kept still counted.
        let patch = "*** Begin Patch\n*** Update File: /w/k.rs\n@@\n a\n-a\n+b\n*** Move to: /w/l.rs\n*** End Patch\n";
        assert_eq!(
            summary(&[call("apply_patch", args(&[("input", patch)]))]),
            "moved l.rs (+1 −1)"
        );
        // A patch names each file it changes.
        let patch = "*** Begin Patch\n*** Add File: /w/n.txt\n+1\n+2\n*** Delete File: /w/old\n*** Update File: /w/m.rs\n@@\n x\n-y\n+Y\n+Z\n*** End Patch\n";
        assert_eq!(
            summary(&[call("apply_patch", args(&[("input", patch)]))]),
            "added n.txt (+2 −0) · deleted old · edited m.rs (+2 −1)"
        );
        // Bounded, at a character.
        let many: Vec<Call> = (0..40)
            .map(|n| call(&format!("tool_{n}é"), Json::Obj(Vec::new())))
            .collect();
        let line = summary(&many);
        assert!(line.len() <= MAX_SUMMARY && line.ends_with('…'), "{line}");
    }

    #[test]
    fn a_steps_detail_shows_edits_as_diffs_within_bounds() {
        let shown = detail(&[
            call(
                "edit_file",
                args(&[
                    ("path", "/w/a.rs"),
                    ("old_string", "old"),
                    ("new_string", "new\n\u{1b}x"),
                ]),
            ),
            call("shell", args(&[("command", "make")])),
        ]);
        assert_eq!(
            shown,
            "edit_file /w/a.rs\n  -old\n  +new\n  +<U+001B>x\nshell\n  $ make"
        );
        let long: String = (0..100).map(|n| format!("{n}\n")).collect();
        let shown = detail(&[call(
            "write_file",
            args(&[("path", "/w/l"), ("content", &long)]),
        )]);
        assert!(shown.ends_with("  … 60 more lines"), "{shown}");
    }

    #[test]
    fn a_reply_with_no_text_stands_on_its_reasoning_summary() {
        let details = r#"[{"type":"reasoning.encrypted","data":"x"},{"type":"reasoning.summary","summary":"Looking for the parser."}]"#;
        assert_eq!(
            reasoning_summary(Some(details), Some("raw")).as_deref(),
            Some("Looking for the parser.")
        );
        assert_eq!(
            reasoning_summary(None, Some("First I read.\n\nThen I edit the file.\n")).as_deref(),
            Some("Then I edit the file.")
        );
        assert_eq!(reasoning_summary(Some("[]"), None), None);
        assert_eq!(reasoning_summary(None, Some("  \n")), None);
    }
}

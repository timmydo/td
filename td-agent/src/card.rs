//! A repository workspace's card (DESIGN.md §7, The workspace card):
//! what the human should know of the tree, kept behind the status row.
//! It recommends against opening the tree in tools that execute on open
//! (§8), then shows each repository's worktrees, the commit they were
//! read at, whether each is checked out, and the project instructions
//! the model was given there (§13) as its conversation recorded them:
//! the text the model read, with every character that would not show,
//! or would show as another, named, so the human reads what is there.

use std::path::PathBuf;

use crate::repo::Instructions;
use crate::store::Instructed;
use crate::workspace::{Entry, Repositories};

/// What the card says first.
pub const OPENING: &str = "The model writes in these worktrees, so what is in them is \
code no one has reviewed. Do not open them in a tool that runs code when it opens a \
folder (an editor that trusts the folder's settings or tasks, a language server or \
extension that builds it, direnv): read each step's changes in the transcript first, \
and run what is there only as you would any code you have not reviewed.";
/// Its first entry's header.
pub const BEFORE: &str = "Before opening the worktrees";
/// The closing chord, as `C-S-m` closes the Messages window.
pub const CHORD: &str = "C-S-w";
/// How many of a commit's hex digits a header names.
const SHORT: usize = 12;
/// A group's or worktree's state when its record cannot be read.
const UNKNOWN: &str = "unknown, since the record above could not be read";

/// What the window reads for the card from the conversation's
/// directory: its recorded project instructions and the workspace
/// repositories it has prepared.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Record {
    pub instructions: Result<Vec<Instructed>, String>,
    pub prepared: Result<Vec<PathBuf>, String>,
}

/// The card's title row.
pub fn title(repositories: &Repositories) -> String {
    format!(
        "Workspace {}: C-c copies, C-S-c copies one, Escape closes",
        crate::tools::visible(&repositories.name)
    )
}

/// The card's entries, header and text: the recommendation, then one
/// for the worktrees of a remote read at one commit alike, as the prompt
/// groups them, and one for a remote's worktrees not read yet.
pub fn entries(repositories: &Repositories, record: &Record) -> Vec<(String, String)> {
    let mut out = vec![(BEFORE.to_string(), OPENING.to_string())];
    if let Err(why) = &record.instructions {
        out.push((
            "Project instructions".into(),
            format!(
                "The conversation's record of its project instructions could not be read: {}",
                crate::tools::visible(why)
            ),
        ));
    }
    if let Err(why) = &record.prepared {
        out.push((
            "Checkouts".into(),
            format!(
                "Which worktrees are checked out could not be read: {}",
                crate::tools::visible(why)
            ),
        ));
    }
    let recorded = record.instructions.as_deref().unwrap_or(&[]);
    let prepared = record.prepared.as_ref().ok();
    let known = record.instructions.is_ok();
    let mut groups: Vec<(&str, Option<&Instructed>, Vec<&Entry>)> = Vec::new();
    for entry in &repositories.entries {
        let read = recorded.iter().find(|r| r.checkout == entry.checkout);
        match groups
            .iter_mut()
            .find(|(remote, at, _)| *remote == entry.remote && alike(*at, read))
        {
            Some((_, _, entries)) => entries.push(entry),
            None => groups.push((&entry.remote, read, vec![entry])),
        }
    }
    for (remote, read, entries) in groups {
        out.push(group(remote, read, known, &entries, prepared));
    }
    out
}

/// `text` as the card shows upstream's: every character that would not
/// show, or would show as another, named as `<U+XXXX>`, and a `<` that
/// begins such a name in the text itself named too, so a name on the
/// card is always one.
fn shown(text: &str) -> String {
    crate::tools::visible(&text.replace("<U+", "<U+003C>U+"))
}

/// Whether two worktrees' records are one entry's.
fn alike(one: Option<&Instructed>, other: Option<&Instructed>) -> bool {
    match (one, other) {
        (Some(one), Some(other)) => one.base == other.base && one.read == other.read,
        (None, None) => true,
        _ => false,
    }
}

/// One entry: the remote, its worktrees and whether each is checked
/// out, then what the model is given at their commit. `known` says the
/// instructions' record could be read, and `prepared` is none when
/// `meta` could not.
fn group(
    remote: &str,
    read: Option<&Instructed>,
    known: bool,
    entries: &[&Entry],
    prepared: Option<&Vec<PathBuf>>,
) -> (String, String) {
    let name = remote
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(remote)
        .trim_end_matches(".git");
    let header = match read {
        Some(read) => format!(
            "{name} at {}",
            read.base.chars().take(SHORT).collect::<String>()
        ),
        None if known => format!("{name}, not read yet"),
        None => format!("{name}, record unreadable"),
    };
    let mut text = format!("{}\n", shown(remote));
    for entry in entries {
        let state = match prepared {
            Some(prepared) if prepared.contains(&entry.repository) => "checked out",
            Some(_) => "not checked out yet: it is being prepared, or is tried again when the conversation next starts",
            None => UNKNOWN,
        };
        text.push_str(&format!(
            "\n{} ({} on branch {}): {state}",
            shown(&entry.checkout.display().to_string()),
            shown(&entry.base),
            shown(&entry.branch),
        ));
    }
    text.push_str("\n\n");
    match read.map(|read| (&read.base, &read.read)) {
        None if known => text.push_str(
            "Their project instructions are read once their commit is fetched; the \
             conversation's first turn waits for them.",
        ),
        None => text.push_str(&format!("Their project instructions are {UNKNOWN}.")),
        Some((base, Instructions::Absent)) => text.push_str(&format!(
            "There is no AGENTS.md or CLAUDE.md at {}: the model is given no project \
             instructions for them.",
            shown(base)
        )),
        Some((base, Instructions::Unread { why })) => text.push_str(&format!(
            "Their project instructions at {} were not read, which the model is told: {}",
            shown(base),
            shown(why)
        )),
        Some((base, Instructions::Found { name, text: body })) => {
            text.push_str(&format!(
                "The model is given {} at {} as the project's guidance, below as it \
                 reads it; a character that would not show, or would show as another, \
                 is named as <U+XXXX>:\n\n",
                shown(name),
                shown(base)
            ));
            let lines: Vec<String> = crate::prompt::plain(body)
                .trim_end_matches('\n')
                .split('\n')
                .map(shown)
                .collect();
            text.push_str(&lines.join("\n"));
        }
    }
    (label(&header), text)
}

/// `text` as a header: nothing that would not show, within td-ui's
/// bound on a label, cut on a character with an ellipsis past it.
fn label(text: &str) -> String {
    let text = crate::tools::visible(text);
    let bound = td_ui::messages::MAX_LABEL_BYTES;
    if text.len() <= bound {
        return text;
    }
    let mut end = bound - '\u{2026}'.len_utf8();
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\u{2026}", text.get(..end).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;

    fn entry(remote: &str, checkout: &str) -> Entry {
        Entry {
            remote: remote.into(),
            base: "main".into(),
            branch: "td-agent/td-1".into(),
            sparse: None,
            store: "/s/td".into(),
            repository: format!("/r{checkout}").into(),
            id: "td".into(),
            checkout: checkout.into(),
        }
    }

    fn recorded(checkout: &str, base: char, read: Instructions) -> Instructed {
        Instructed {
            checkout: checkout.into(),
            base: base.to_string().repeat(40),
            read,
        }
    }

    /// The recommendation first; then the worktrees of one remote read
    /// at one commit alike together, each saying whether it is checked
    /// out, with the text the model was given, its hidden characters
    /// named; then an absent, an unread and a not yet read one, each
    /// said; and a record that could not be read, said.
    #[test]
    fn the_card_recommends_then_shows_each_commits_instructions() {
        let td = "https://github.com/timmydo/td";
        let repositories = Repositories {
            template: "td".into(),
            name: "td-1".into(),
            entries: vec![
                entry(td, "/w/td-1/td"),
                entry(td, "/w/td-1/td-docs"),
                entry("https://github.com/timmydo/other.git", "/w/td-1/other"),
                entry("https://github.com/timmydo/third", "/w/td-1/third"),
                entry("https://github.com/timmydo/fourth", "/w/td-1/fourth"),
            ],
        };
        let found = Instructions::Found {
            name: "AGENTS.md".into(),
            text: "# td\r\nRead DESIGN.md.\u{202e}gnorw\u{1} <U+202E>\n".into(),
        };
        let record = Record {
            instructions: Ok(vec![
                recorded("/w/td-1/td", 'a', found.clone()),
                recorded("/w/td-1/td-docs", 'a', found),
                recorded("/w/td-1/other", 'b', Instructions::Absent),
                recorded(
                    "/w/td-1/third",
                    'c',
                    Instructions::Unread {
                        why: "AGENTS.md is not UTF-8".into(),
                    },
                ),
            ]),
            prepared: Ok(vec!["/r/w/td-1/td".into()]),
        };
        let card = entries(&repositories, &record);
        let headers: Vec<&str> = card.iter().map(|(h, _)| h.as_str()).collect();
        assert_eq!(
            headers,
            [
                BEFORE,
                "td at aaaaaaaaaaaa",
                "other at bbbbbbbbbbbb",
                "third at cccccccccccc",
                "fourth, not read yet"
            ]
        );
        let text = |at: usize| card.get(at).map(|(_, t)| t.as_str()).unwrap();
        assert_eq!(text(0), OPENING);
        let td = text(1);
        assert!(td.starts_with("https://github.com/timmydo/td\n"), "{td}");
        assert!(td.contains("/w/td-1/td (main on branch td-agent/td-1): checked out"));
        assert!(td.contains("/w/td-1/td-docs (main on branch td-agent/td-1): not checked out yet"));
        assert!(td.contains("given AGENTS.md at"), "{td}");
        // The override named, a control as the model gets it, and the
        // text's own `<U+202E>` told from it.
        assert!(
            td.ends_with("# td\nRead DESIGN.md.<U+202E>gnorw\u{fffd} <U+003C>U+202E>"),
            "{td}"
        );
        assert!(text(2).contains("no AGENTS.md or CLAUDE.md"));
        assert!(text(3).contains("not read, which the model is told: AGENTS.md is not UTF-8"));
        assert!(text(4).contains("read once their commit is fetched"));

        let unreadable = Record {
            instructions: Err("instructions: not a list".into()),
            prepared: Err("meta: gone".into()),
        };
        let card = entries(&repositories, &unreadable);
        let all: String = card.iter().map(|(_, t)| t.as_str()).collect();
        assert!(all.contains("could not be read: instructions: not a list"));
        assert!(all.contains("could not be read: meta: gone"));
        // Nothing then says what it cannot know.
        assert!(!all.contains("not read yet") && !all.contains("first turn waits"));
        assert!(!all.contains("not checked out yet"));
        assert!(card
            .iter()
            .skip(3)
            .all(|(h, t)| h.ends_with("record unreadable") && t.contains(UNKNOWN)));
    }

    /// A header is a label td-ui takes: no control, within its bound.
    #[test]
    fn a_header_is_a_label_within_its_bound() {
        let long = format!("{}\u{1}", "é".repeat(200));
        let header = label(&long);
        assert!(header.len() <= td_ui::messages::MAX_LABEL_BYTES);
        assert!(header.ends_with('\u{2026}'));
        assert!(!header.chars().any(char::is_control));
        assert_eq!(label("td at a"), "td at a");
    }
}

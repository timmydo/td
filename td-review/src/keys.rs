//! The review window's keys, as the window's key list shows them on `F1`,
//! `?` or a press on the footer: a section per pane, and the two
//! table columns whose cells need spelling out, as prose. Each row's
//! description is whole, for the list to wrap to its panel.

use td_ui::keys::Section;

const LIST: &[(&str, &str)] = &[
    ("j/k", "move"),
    ("Space/b", "page down or up"),
    ("g/G", "first or last"),
    ("Return", "review the selected branch against the base"),
    ("f", "fetch and prune the base's remote (else origin)"),
    ("F", "fetch and prune every remote, mirrors included"),
    (
        "p",
        "push the base to its remote (else origin), then delete the branches \
         that push published — no confirmation",
    ),
    (
        "P",
        "the same, to every remote — bar the no_push ones and any with \
         `git config remote.<name>.skipPushAll true`, which `p` and a \
         hand-typed `git push` still reach",
    ),
    ("r", "re-read branches"),
    ("/", "filter by branch name (Escape clears)"),
    (
        "D",
        "delete the selected branch from the remote its row names — asked \
         first, unless every check can prove the delete takes nothing; the \
         pane names the one that could not, and then it asks",
    ),
    (
        "w",
        "sweep worktrees whose branch has fully landed (clean, unpushed, not \
         -rolling); every other one says why it stays",
    ),
    (
        "?",
        "show this list of keys, in any pane; a click on the footer shows it \
         too",
    ),
    ("q", "quit"),
];

// The column's whole vocabulary, because two of these cells are answers to
// a question the operator did not ask — what a LANDING would find, which no
// key spells out until one is pressed. Cells, not keys, so prose rows
// that quote the cell.
// Its two halves answer different questions now, which was self-evident
// while the whole cell was `%(ahead-behind:)` and is not any more.
const AHEAD_BEHIND: &[(&str, &str)] = &[
    (
        "",
        "The A half, before the slash: commits a landing would take — none on a \
         landed row, however many of its own the branch still carries",
    ),
    (
        "",
        "The B half, after it: commits the base has that the branch does not, by \
         ancestry: how far behind it has fallen",
    ),
];

const READY: &[(&str, &str)] = &[
    (
        "",
        "\"ok\": every commit carries the record AGENTS.md requires",
    ),
    ("", "\"n/m!\": n of its m commits do not"),
    ("", "\"?\": the records could not be read"),
    ("", "\"-\": no commits over the base"),
    (
        "",
        "\"landed\": nothing left to land: the base carries this work \
         already, under its own oids after a landing replayed them — rebase \
         the branch and it empties",
    ),
    (
        "",
        "\"!merge\": does not merge onto the base, so neither s nor r can \
         take it as it stands; rebase it, unless it shares no history with \
         the base at all, which nothing lands",
    ),
];

const REVIEW: &[(&str, &str)] = &[
    ("j/k", "scroll a line"),
    ("Space/b", "scroll a page"),
    ("g/G", "top or end"),
    ("drag", "select"),
    (
        "double-click",
        "select a word; a triple click selects a line",
    ),
    ("C-c", "copy the selection"),
    ("s", "land it squashed: one commit on the base — asks first"),
    (
        "r",
        "land it rebased: its own commits, replayed; lands on the keystroke \
         — no confirmation",
    ),
    ("q", "back to the list"),
];

// Ends with what a landing holds to, as prose after a blank row: rows with
// no keys, a sentence each.
const LANDING: &[(&str, &str)] = &[
    (
        "s y",
        "squash and commit, the message from the branch's commits",
    ),
    (
        "r",
        "replays each commit onto the base tip, message, author and all — \
         all of them or none, and with no confirmation: it commits, it does \
         not publish",
    ),
    (
        "q p",
        "publish it: push the base (P: every remote); the branches it \
         published are then deleted from the remotes it reached — no \
         further confirmation",
    ),
    ("", ""),
    (
        "",
        "The branch is pinned to the commit you reviewed: if the ref moves \
         before it runs, the landing refuses rather than committing work you \
         have not seen.",
    ),
    (
        "",
        "Both modes hold what lands to the diff this pane showed, and say so \
         on the log when they could not.",
    ),
    (
        "",
        "Landing only commits — the push is a separate, deliberate step, and \
         it publishes every local commit on the base.",
    ),
    (
        "",
        "Both p and P push straight away: the keystroke is the decision, and the \
         branches this session landed onto what it published go with it.",
    ),
    (
        "",
        "A remote copy that has moved off the commit you reviewed is left, as \
         is one on a mirror the push did not reach — until a push does.",
    ),
    ("", ""),
    (
        "",
        "Landing needs a clean work tree with the base branch checked out.",
    ),
];

/// The sections the key list shows, the review's and the landing's first
/// while a review is on screen.
pub fn sections(reviewing: bool) -> Vec<Section> {
    let list = [
        Section::new("Branch list", LIST),
        Section::new("A/B column cells", AHEAD_BEHIND),
        Section::new("READY column cells", READY),
    ];
    let review = [
        Section::new("Review", REVIEW),
        Section::new("Landing", LANDING),
    ];
    if reviewing {
        review.into_iter().chain(list).collect()
    } else {
        list.into_iter().chain(review).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use td_ui::keys::{self, row, Row};

    fn titles(sections: &[Section]) -> Vec<&'static str> {
        sections.iter().map(|s| s.title).collect()
    }

    #[test]
    fn the_list_comes_first_unless_a_review_is_on_screen() {
        let listed = sections(false);
        assert_eq!(
            titles(&listed),
            [
                "Branch list",
                "A/B column cells",
                "READY column cells",
                "Review",
                "Landing",
            ]
        );
        let reviewed = sections(true);
        assert_eq!(
            titles(&reviewed),
            [
                "Review",
                "Landing",
                "Branch list",
                "A/B column cells",
                "READY column cells",
            ]
        );
        let first = |sections: &[Section]| sections.first().and_then(|s| s.rows.first().copied());
        assert_eq!(first(&listed), Some(row(("j/k", "move"))));
        assert_eq!(first(&reviewed), Some(row(("j/k", "scroll a line"))));
    }

    /// Both orders of the list are spelled as td-ui's keymap spells chords,
    /// their titles capitalised and every keyed row described.
    #[test]
    fn every_variant_passes_the_key_list_check() {
        for reviewing in [false, true] {
            let problems = keys::check(&sections(reviewing));
            assert!(
                problems.is_empty(),
                "reviewing {reviewing}:\n{}",
                problems.join("\n")
            );
        }
    }

    /// Every keyed row carries its whole description, and rows with no keys
    /// are the column cells, a section of them each, and the landing's
    /// prose after a blank row: the list wraps a description itself, so a
    /// row broken by hand would read ragged.
    #[test]
    fn descriptions_are_whole_and_prose_is_the_cells_and_the_landing_s_end() {
        let all = sections(false);
        let rows = |title: &str| -> Vec<Row> {
            all.iter()
                .find(|s| s.title == title)
                .map(|s| s.rows.clone())
                .unwrap_or_default()
        };
        let list = rows("Branch list");
        assert!(list.contains(&row(("q", "quit"))));
        assert!(list.iter().any(|r| r.keys == "?"));
        assert!(rows("READY column cells").contains(&row((
            "",
            "\"!merge\": does not merge onto the base, so neither s nor r can \
             take it as it stands; rebase it, unless it shares no history with \
             the base at all, which nothing lands"
        ))));
        for section in &all {
            let cells = section.title.ends_with("column cells");
            let keyed = section
                .rows
                .iter()
                .take_while(|r| !r.keys.is_empty())
                .count();
            let prose = section.rows.get(keyed..).unwrap_or_default();
            if cells {
                // Each cell is quoted or named before what it means.
                assert_eq!(keyed, 0, "{}", section.title);
                assert!(prose.iter().all(|r| r.what.contains(": ")));
            } else {
                // A row with no keys never opens a keyed section.
                assert!(keyed > 0, "{}", section.title);
                if section.title == "Landing" {
                    assert_eq!(prose.first(), Some(&row(("", ""))));
                    assert!(prose.iter().all(|r| r.keys.is_empty()));
                } else {
                    assert!(prose.is_empty(), "{}: {prose:?}", section.title);
                }
            }
            for r in &section.rows {
                assert!(!r.what.contains("  "), "{}: {r:?}", section.title);
            }
        }
        assert_eq!(
            rows("Landing").last(),
            Some(&row((
                "",
                "Landing needs a clean work tree with the base branch checked out."
            )))
        );
    }
}

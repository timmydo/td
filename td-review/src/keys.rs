//! The review window's keys, as the window's key list shows them on `F1`
//! or `?`: a section per pane, and the two table columns whose cells need
//! spelling out. Each row's description is whole, for the list to wrap to
//! its panel; only the landing's closing prose has rows with no keys.

use td_ui::keys::Section;

const LIST: &[(&str, &str)] = &[
    ("j / k", "move"),
    ("space / b", "page down / up"),
    ("g / G", "first / last"),
    ("enter", "review the selected branch against the base"),
    ("f", "fetch + prune the base's remote (else origin)"),
    ("F", "fetch + prune every remote, mirrors included"),
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
    ("/", "filter by branch name (esc clears)"),
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
    ("?", "this list of keys, in any pane"),
    ("q", "quit"),
];

// The column's whole vocabulary, because two of these cells are answers to
// a question the operator did not ask — what a LANDING would find, which no
// key spells out until one is pressed. Titled as CELLS because `?` is also
// a key above, and the two columns of this list look alike.
// Its two halves answer different questions now, which was self-evident
// while the whole cell was `%(ahead-behind:)` and is not any more.
const AHEAD_BEHIND: &[(&str, &str)] = &[
    (
        "A",
        "commits a landing would take — none on a landed row, however many \
         of its own the branch still carries",
    ),
    (
        "B",
        "commits the base has that the branch does not, by ancestry: how far \
         behind it has fallen",
    ),
];

const READY: &[(&str, &str)] = &[
    ("ok", "every commit carries the record AGENTS.md requires"),
    ("n/m!", "n of its m commits do not"),
    ("?", "the records could not be read"),
    ("-", "no commits over the base"),
    (
        "landed",
        "nothing left to land: the base carries this work already, under its \
         own oids after a landing replayed them — rebase the branch and it \
         empties",
    ),
    (
        "!merge",
        "does not merge onto the base, so neither s nor r can take it as it \
         stands; rebase it, unless it shares no history with the base at \
         all, which nothing lands",
    ),
];

const REVIEW: &[(&str, &str)] = &[
    ("j / k, space / b", "scroll"),
    ("g / G", "top / end"),
    ("drag", "select; double, triple click a word, a line"),
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
        "s then y",
        "squash + commit, message from the branch's commits",
    ),
    (
        "r",
        "replays each commit onto the base tip, message, author and all — \
         all of them or none, and with no confirmation: it commits, it does \
         not publish",
    ),
    ("q, then p", "publish it: push the base (P = every remote)"),
    (
        "after the push",
        "the branches it published are deleted from the remotes it reached \
         — no further confirmation",
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
        "p and P push straight away: the keystroke is the decision, and the \
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
        Section::new("branch list", LIST),
        Section::new("A/B column (cells, not keys)", AHEAD_BEHIND),
        Section::new("READY column (cells, not keys)", READY),
    ];
    let review = [
        Section::new("review", REVIEW),
        Section::new("landing", LANDING),
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
    use td_ui::keys::{row, Row};

    fn titles(sections: &[Section]) -> Vec<&'static str> {
        sections.iter().map(|s| s.title).collect()
    }

    #[test]
    fn the_list_comes_first_unless_a_review_is_on_screen() {
        let listed = sections(false);
        assert_eq!(
            titles(&listed),
            [
                "branch list",
                "A/B column (cells, not keys)",
                "READY column (cells, not keys)",
                "review",
                "landing",
            ]
        );
        let reviewed = sections(true);
        assert_eq!(
            titles(&reviewed),
            [
                "review",
                "landing",
                "branch list",
                "A/B column (cells, not keys)",
                "READY column (cells, not keys)",
            ]
        );
        let first = |sections: &[Section]| sections.first().and_then(|s| s.rows.first().copied());
        assert_eq!(first(&listed), Some(row(("j / k", "move"))));
        assert_eq!(first(&reviewed), Some(row(("j / k, space / b", "scroll"))));
    }

    /// Every keyed row carries its whole description, and rows with no keys
    /// are only the landing's prose, after a blank row: the list wraps a
    /// description itself, so a row broken by hand would read ragged.
    #[test]
    fn descriptions_are_whole_and_only_the_landing_ends_in_prose() {
        let all = sections(false);
        let rows = |title: &str| -> Vec<Row> {
            all.iter()
                .find(|s| s.title == title)
                .map(|s| s.rows.clone())
                .unwrap_or_default()
        };
        let list = rows("branch list");
        assert!(list.contains(&row(("?", "this list of keys, in any pane"))));
        assert!(list.contains(&row(("q", "quit"))));
        assert!(rows("READY column (cells, not keys)").contains(&row((
            "!merge",
            "does not merge onto the base, so neither s nor r can take it as it \
             stands; rebase it, unless it shares no history with the base at \
             all, which nothing lands"
        ))));
        for section in &all {
            // A row with no keys never opens a section.
            assert!(
                section.rows.first().is_some_and(|r| !r.keys.is_empty()),
                "{}",
                section.title
            );
            let keyed = section
                .rows
                .iter()
                .take_while(|r| !r.keys.is_empty())
                .count();
            let prose = section.rows.get(keyed..).unwrap_or_default();
            if section.title == "landing" {
                assert_eq!(prose.first(), Some(&row(("", ""))));
                assert!(prose.iter().all(|r| r.keys.is_empty()));
            } else {
                assert!(prose.is_empty(), "{}: {prose:?}", section.title);
            }
            for r in &section.rows {
                assert!(!r.what.contains("  "), "{}: {r:?}", section.title);
            }
        }
        assert_eq!(
            rows("landing").last(),
            Some(&row((
                "",
                "Landing needs a clean work tree with the base branch checked out."
            )))
        );
    }
}

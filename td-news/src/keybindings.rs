//! The reader's keys as the window's key list shows them: one table of
//! `(keys, what)` rows per view, the one source the list is built from.

use td_ui::keys::Section;

const EVERYWHERE: &str = "Everywhere";
pub const FEEDS: &str = "Feeds";
pub const ARTICLES: &str = "Articles";
pub const ARTICLE: &str = "Article";
pub const LOG: &str = "Log";
const MOUSE_TITLE: &str = "Mouse";

const GLOBAL: &[(&str, &str)] = &[
    ("q", "quit, or go back"),
    ("?", "show this list of keys"),
    ("g", "refresh the current feed"),
    ("G", "refresh all feeds"),
];

const FEED_LIST: &[(&str, &str)] = &[
    ("j/k/Up/Down", "move"),
    ("n/p", "next or previous"),
    ("PageUp/PageDown", "move by a page"),
    ("Home/End", "jump to the top or the bottom"),
    ("Return", "open the feed, or the [Log]"),
    ("g", "refresh the current feed"),
    ("G", "refresh all feeds"),
    ("u", "mark the feed read"),
];

const ARTICLE_LIST: &[(&str, &str)] = &[
    ("j/k/Up/Down", "move"),
    ("n/p", "next or previous"),
    ("PageUp/PageDown", "move by a page"),
    ("Home/End", "jump to the top or the bottom"),
    ("Return", "open the article"),
    ("/", "search titles and text"),
    ("g", "refresh the current feed"),
    ("G", "refresh all feeds"),
    (
        "u",
        "mark read and go to the next unread; on a read article, mark it unread",
    ),
    ("H", "open the HTML digest in the browser"),
    ("o", "open the article's link"),
];

const ARTICLE_VIEW: &[(&str, &str)] = &[
    ("j/k", "scroll"),
    ("Space/PageDown", "page down"),
    ("PageUp", "page up"),
    ("arrows/Home/End", "move the caret; the view follows"),
    ("n/p", "next or previous article"),
    ("u", "toggle read"),
    ("o", "open the article's link"),
    (
        "b",
        "open a link in the text, from a picker if there are several",
    ),
    ("1..9", "open a link in the text by its number"),
];

const LOG_VIEW: &[(&str, &str)] = &[
    ("n", "show [News Log]"),
    ("d", "show [Debug Log]"),
    ("j/k/Up/Down", "scroll"),
    ("PageUp/PageDown", "scroll by a page"),
    ("Home/End", "scroll to the top or the bottom"),
    ("q", "back"),
];

const MOUSE: &[(&str, &str)] = &[
    (
        "click",
        "select a feed, or open an article or a link; a bar label is its key",
    ),
    ("wheel", "move the selection, or scroll the article"),
    ("drag", "in the article: select text"),
    (
        "C-click",
        "on a link in the article: open it in the browser",
    ),
];

/// The sections in the order the list shows them when no view leads.
const SECTIONS: &[(&str, &[(&str, &str)])] = &[
    (EVERYWHERE, GLOBAL),
    (FEEDS, FEED_LIST),
    (ARTICLES, ARTICLE_LIST),
    (ARTICLE, ARTICLE_VIEW),
    (LOG, LOG_VIEW),
    (MOUSE_TITLE, MOUSE),
];

/// The reader's key list: the section titled `lead`, the view the reader
/// is in, first, then the others in order.
pub fn sections(lead: &str) -> Vec<Section> {
    let (first, rest): (Vec<_>, Vec<_>) = SECTIONS.iter().partition(|(title, _)| *title == lead);
    first
        .into_iter()
        .chain(rest)
        .map(|&(title, rows)| Section::new(title, rows))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use td_ui::keys::row;

    #[test]
    fn the_view_the_reader_is_in_leads_and_every_section_follows_in_order() {
        let titles = |lead: &str| -> Vec<&str> {
            sections(lead).iter().map(|section| section.title).collect()
        };
        assert_eq!(
            titles(ARTICLE),
            [ARTICLE, EVERYWHERE, FEEDS, ARTICLES, LOG, MOUSE_TITLE]
        );
        assert_eq!(
            titles(""),
            [EVERYWHERE, FEEDS, ARTICLES, ARTICLE, LOG, MOUSE_TITLE]
        );
        let all = sections(FEEDS);
        let feeds = all.first().expect("the feeds lead");
        assert_eq!(feeds.rows.first(), Some(&row(("j/k/Up/Down", "move"))));
        assert!(feeds.rows.contains(&row(("u", "mark the feed read"))));
        let everywhere = all.get(1).expect("then everywhere");
        assert!(everywhere
            .rows
            .contains(&row(("?", "show this list of keys"))));
        let mouse = all.last().expect("the mouse last");
        assert_eq!(mouse.title, MOUSE_TITLE);
        assert_eq!(
            mouse.rows.get(1),
            Some(&row(("wheel", "move the selection, or scroll the article")))
        );
    }

    /// Every list the reader can show, whichever view leads, is spelled
    /// and written as td-ui's key list holds every program's.
    #[test]
    fn every_list_passes_the_key_list_check() {
        for lead in ["", EVERYWHERE, FEEDS, ARTICLES, ARTICLE, LOG, MOUSE_TITLE] {
            let problems = td_ui::keys::check(&sections(lead));
            assert!(
                problems.is_empty(),
                "lead {lead:?}:\n{}",
                problems.join("\n")
            );
        }
    }
}

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
    ("q", "quit/back"),
    ("?", "show this list of keys"),
    ("g", "refresh current feed"),
    ("G", "refresh all feeds"),
];

const FEED_LIST: &[(&str, &str)] = &[
    ("j/k or arrows", "move"),
    ("n/p", "next/prev"),
    ("PgUp/PgDn", "move by page"),
    ("Home/End", "jump to top/bottom"),
    ("Enter", "open feed/[Log]"),
    ("g", "refresh current feed"),
    ("G", "refresh all feeds"),
    ("u", "mark feed read"),
];

const ARTICLE_LIST: &[(&str, &str)] = &[
    ("j/k or arrows", "move"),
    ("n/p", "next/prev"),
    ("PgUp/PgDn", "move by page"),
    ("Home/End", "jump to top/bottom"),
    ("Enter", "open article"),
    ("/", "search titles and text"),
    ("g", "refresh current feed"),
    ("G", "refresh all feeds"),
    ("u", "mark read + next (toggle if read)"),
    ("H", "open HTML digest in browser"),
    ("o", "open link"),
];

const ARTICLE_VIEW: &[(&str, &str)] = &[
    ("j/k", "scroll"),
    ("Space/PgDn", "page down"),
    ("PgUp", "page up"),
    ("arrows, Home/End", "move the caret; the view follows"),
    ("n/p", "next/prev article"),
    ("u", "toggle read"),
    ("o", "open article link"),
    ("b", "open URL (picker if multiple)"),
    ("1-9", "open URL by number"),
];

const LOG_VIEW: &[(&str, &str)] = &[
    ("n", "show [News Log]"),
    ("d", "show [Debug Log]"),
    ("j/k or arrows", "scroll"),
    ("PgUp/PgDn", "page scroll"),
    ("Home/End", "top/bottom"),
    ("q", "back"),
];

const MOUSE: &[(&str, &str)] = &[
    ("click", "select the row; a bar label is its key"),
    ("click an article", "open it"),
    ("wheel", "move the selection, or scroll the article"),
    ("drag in the article", "select text"),
    (
        "Ctrl-click a link",
        "in the article: open it in the browser",
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
        assert_eq!(feeds.rows.first(), Some(&row(("j/k or arrows", "move"))));
        assert!(feeds.rows.contains(&row(("u", "mark feed read"))));
        let everywhere = all.get(1).expect("then everywhere");
        assert!(everywhere
            .rows
            .contains(&row(("?", "show this list of keys"))));
        let mouse = all.last().expect("the mouse last");
        assert_eq!(mouse.title, MOUSE_TITLE);
        assert_eq!(
            mouse.rows.get(2),
            Some(&row(("wheel", "move the selection, or scroll the article")))
        );
    }
}

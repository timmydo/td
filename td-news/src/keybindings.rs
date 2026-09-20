pub const GLOBAL: &[&str] = &[
    "q: quit/back",
    "?: help",
    "g: refresh current feed",
    "G: refresh all feeds",
];

pub const FEED_LIST: &[&str] = &[
    "j/k or arrows: move",
    "n/p: next/prev",
    "PgUp/PgDn: move by page",
    "Home/End: jump to top/bottom",
    "Enter: open feed/[Log]",
    "g: refresh current feed",
    "G: refresh all feeds",
    "u: mark feed read",
];

pub const ARTICLE_LIST: &[&str] = &[
    "j/k or arrows: move",
    "n/p: next/prev",
    "PgUp/PgDn: move by page",
    "Home/End: jump to top/bottom",
    "Enter: open article",
    "/: search titles and text",
    "g: refresh current feed",
    "G: refresh all feeds",
    "u: mark read + next (toggle if read)",
    "H: open HTML digest in browser",
    "o: open link",
];

pub const ARTICLE_VIEW: &[&str] = &[
    "j/k: scroll",
    "Space/PgDn: page down",
    "PgUp: page up",
    "arrows, Home/End: move the caret; the view follows",
    "n/p: next/prev article",
    "u: toggle read",
    "o: open article link",
    "b: open URL (picker if multiple)",
    "1-9: open URL by number",
];

pub const LOG_VIEW: &[&str] = &[
    "n: show [News Log]",
    "d: show [Debug Log]",
    "j/k or arrows: scroll",
    "PgUp/PgDn: page scroll",
    "Home/End: top/bottom",
    "q: back",
];

pub const MOUSE: &[&str] = &[
    "click: select the row; a bar label is its key",
    "click on an article: open it",
    "wheel: move the selection, or scroll the article",
    "drag in the article: select text",
];

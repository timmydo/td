pub const GLOBAL: &[&str] = &[
    "q: quit/back",
    "?: help",
    "g: refresh current feed",
    "G: refresh all feeds",
];

pub const FEED_LIST: &[&str] = &[
    "j/k or arrows: move",
    "n/p: next/prev",
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
    "Mouse wheel: move selection",
    "Mouse click: open article",
    "g: refresh current feed",
    "G: refresh all feeds",
    "u: mark read + next (toggle if read)",
    "H: open HTML digest in browser",
    "o: open link",
];

pub const ARTICLE_VIEW: &[&str] = &[
    "j/k or arrows: scroll",
    "Space/PgDn: page down",
    "PgUp: page up",
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

//! What the panes are made of: styled rows, the frame the state machine lays
//! them out in, the key vocabulary it reads, and the scrubbing every untrusted
//! string goes through before it is shown or printed.
//!
//! The window (`window.rs`) paints a [`Frame`] row by row in td-ui's cells and
//! reads its chords into [`Key`]s; nothing here knows about either, so the
//! state machine is testable without a compositor.

use std::io;

pub const RED: u8 = 31;
pub const GREEN: u8 = 32;
pub const YELLOW: u8 = 33;
pub const MAGENTA: u8 = 35;
pub const CYAN: u8 = 36;
// De-emphasis is `Style::dim()`, not a palette slot: the window draws it in
// the toolkit's muted ink, legible on its ground, where a fixed grey slot
// chosen here would not follow a palette change.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Style {
    pub fg: Option<u8>,
    pub bold: bool,
    pub dim: bool,
    pub invert: bool,
}

impl Style {
    pub const PLAIN: Style = Style {
        fg: None,
        bold: false,
        dim: false,
        invert: false,
    };

    pub const fn fg(code: u8) -> Style {
        Style {
            fg: Some(code),
            bold: false,
            dim: false,
            invert: false,
        }
    }

    pub const fn bold() -> Style {
        Style {
            fg: None,
            bold: true,
            dim: false,
            invert: false,
        }
    }

    pub const fn dim() -> Style {
        Style {
            fg: None,
            bold: false,
            dim: true,
            invert: false,
        }
    }

    pub const fn bar(code: u8) -> Style {
        Style {
            fg: Some(code),
            bold: true,
            dim: false,
            invert: true,
        }
    }

    pub const fn with_bold(self) -> Style {
        Style {
            fg: self.fg,
            bold: true,
            dim: self.dim,
            invert: self.invert,
        }
    }

    pub const fn with_invert(self) -> Style {
        Style {
            fg: self.fg,
            bold: self.bold,
            dim: self.dim,
            invert: true,
        }
    }
}

/// A single rendered row: plain text plus the style applied to the whole row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub text: String,
    pub style: Style,
}

impl Line {
    pub fn new(text: impl Into<String>, style: Style) -> Line {
        Line {
            text: text.into(),
            style,
        }
    }

    pub fn plain(text: impl Into<String>) -> Line {
        Line::new(text, Style::PLAIN)
    }

    pub fn blank() -> Line {
        Line::plain("")
    }
}

/// Placeholder for a character that must never be shown as itself.
const REPLACEMENT: char = '\u{b7}';

/// True for anything that could steer a terminal or misrepresent the text:
/// C0 and C1 controls (U+009B is CSI on xterm) plus the bidi overrides that
/// make a diff read in an order the bytes do not have. The window draws a
/// control as the replacement glyph anyway; the headless modes print to a
/// terminal, and one rule for both keeps a refname shown the same in each.
fn is_hostile(ch: char) -> bool {
    ch.is_control() || matches!(ch as u32, 0x202a..=0x202e | 0x2066..=0x2069 | 0x200e | 0x200f)
}

/// Replace anything hostile, leaving the text otherwise intact. For output
/// that does not go through a [`Frame`] — `--list` and the headless `--land`
/// log still print refnames, subjects and raw git output to a terminal.
pub fn scrub(text: &str) -> String {
    text.chars()
        .map(|c| if is_hostile(c) { REPLACEMENT } else { c })
        .collect()
}

/// `scrub` every line but keep the line breaks: a newline is a control
/// character, so `scrub` alone folds a diff into one unreadable row.
pub fn scrub_lines(text: &str) -> String {
    text.lines().map(scrub).collect::<Vec<_>>().join("\n")
}

/// Expand tabs to 8-column stops, replace hostile characters and clip to
/// `cols` cells. Returns the clipped text and the cells it occupies.
///
/// A cell is one scalar: td-ui's text run gives every scalar its own cell,
/// wide or combining alike, so a column padded here lines up in the window
/// only when it is measured the same way.
pub fn sanitize(text: &str, cols: usize) -> (String, usize) {
    let mut out = String::with_capacity(text.len().min(cols.saturating_mul(4)));
    let mut width = 0usize;
    for ch in text.chars() {
        if width >= cols {
            break;
        }
        if ch == '\t' {
            let target = ((width / 8 + 1) * 8).min(cols);
            while width < target {
                out.push(' ');
                width += 1;
            }
            continue;
        }
        out.push(if is_hostile(ch) { REPLACEMENT } else { ch });
        width += 1;
    }
    (out, width)
}

/// What a wrapped row's continuation starts with, so a piece of a diff line
/// that happens to begin `+` or `-` cannot read as a line of its own.
const CONTINUED: &str = "\u{21aa} ";

/// Rows `first..first + count` of `lines` laid out `cols` cells wide: a line
/// too long for one row goes on over as many more as it needs, each marked
/// as a continuation and in the line's own style, so a pane shows every byte
/// of a long line rather than clipping it at the window's edge. Tabs are
/// expanded first, as `sanitize` expands them. Only the rows asked for are
/// made: a pane over a long diff wraps the screenful it shows.
pub fn wrap_window(lines: &[Line], cols: usize, first: usize, count: usize) -> Vec<Line> {
    let mut out = Vec::with_capacity(count.min(lines.len()));
    let mut at = 0usize;
    for line in lines {
        if out.len() >= count {
            break;
        }
        let rows = rows_of(line, cols);
        if at.saturating_add(rows) <= first {
            at = at.saturating_add(rows);
            continue;
        }
        let mut skip = first.saturating_sub(at);
        at = at.saturating_add(rows);
        wrap_one(line, cols, &mut |row| {
            if skip > 0 {
                skip -= 1;
                return true;
            }
            out.push(row);
            out.len() < count
        });
    }
    out
}

/// Every row `wrap_window` would make of `lines`.
#[cfg(test)]
pub fn wrap(lines: &[Line], cols: usize) -> Vec<Line> {
    wrap_window(lines, cols, 0, usize::MAX)
}

/// How many rows `wrap_window` lays `lines` out on, counted, not made.
pub fn wrapped_len(lines: &[Line], cols: usize) -> usize {
    lines.iter().fold(0usize, |rows, line| {
        rows.saturating_add(rows_of(line, cols))
    })
}

/// The cells `text` takes once its tabs are expanded, as `sanitize` lays it.
fn cells(text: &str) -> usize {
    text.chars().fold(0usize, |width, ch| {
        if ch == '\t' {
            (width / 8 + 1) * 8
        } else {
            width.saturating_add(1)
        }
    })
}

/// The cells a continuation row's text has after its mark.
fn continued(cols: usize) -> usize {
    let lead = CONTINUED.chars().count();
    // A continuation needs a cell after its mark, or it would never advance;
    // too narrow for both, it goes unmarked.
    if cols > lead {
        cols - lead
    } else {
        cols
    }
}

fn rows_of(line: &Line, cols: usize) -> usize {
    let cols = cols.max(1);
    let width = cells(&line.text);
    if width <= cols {
        return 1;
    }
    1 + (width - cols).div_ceil(continued(cols))
}

/// `line`'s rows to `row`, in order, until it answers false.
fn wrap_one(line: &Line, cols: usize, row: &mut dyn FnMut(Line) -> bool) {
    let cols = cols.max(1);
    let (whole, width) = sanitize(&line.text, usize::MAX);
    if width <= cols {
        row(Line::new(whole, line.style));
        return;
    }
    let rest = continued(cols);
    let marked = rest < cols;
    let mut chars = whole.chars();
    let first: String = chars.by_ref().take(cols).collect();
    if !row(Line::new(first, line.style)) {
        return;
    }
    loop {
        let piece: String = chars.by_ref().take(rest).collect();
        if piece.is_empty() {
            return;
        }
        let text = if marked {
            format!("{CONTINUED}{piece}")
        } else {
            piece
        };
        if !row(Line::new(text, line.style)) {
            return;
        }
    }
}

/// Where a frame's scrolling region is and how far it has scrolled, for the
/// window's scrollbar: `visible` rows from frame row `row`, over `total`
/// lines of content with `first` at the top.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scroll {
    pub row: usize,
    pub visible: usize,
    pub total: usize,
    pub first: usize,
}

/// A `rows` by `cols` grid of styled rows, top down, which the window paints
/// whole. Rows past the last pushed one are blank paper.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub rows: usize,
    pub cols: usize,
    lines: Vec<Line>,
    scroll: Option<Scroll>,
    /// The branch each list row shows, by frame row, so a press is read
    /// against the frame it was made on rather than whatever the list has
    /// become since.
    picks: Vec<(usize, String)>,
}

impl Frame {
    pub fn new(rows: usize, cols: usize) -> Frame {
        Frame {
            rows,
            cols: cols.max(1),
            lines: Vec::with_capacity(rows),
            scroll: None,
            picks: Vec::new(),
        }
    }

    /// Rows still free below what has been pushed.
    pub fn room(&self) -> usize {
        self.rows.saturating_sub(self.lines.len())
    }

    pub fn push(&mut self, line: &Line) {
        self.push_parts(&line.text, line.style);
    }

    pub fn push_text(&mut self, text: &str, style: Style) {
        self.push_parts(text, style);
    }

    pub fn push_blank(&mut self) {
        self.push_parts("", Style::PLAIN);
    }

    /// The rows pushed next scroll: `visible` of them over `total` lines
    /// with `first` at the top.
    pub fn scrollbar(&mut self, visible: usize, total: usize, first: usize) {
        self.scroll = Some(Scroll {
            row: self.lines.len(),
            visible: visible.min(self.room()),
            total,
            first,
        });
    }

    /// The row pushed next shows the branch `refname`.
    pub fn pick(&mut self, refname: &str) {
        if self.lines.len() < self.rows {
            self.picks.push((self.lines.len(), refname.to_string()));
        }
    }

    /// The branch each list row shows, by frame row.
    pub fn picks(&self) -> &[(usize, String)] {
        &self.picks
    }

    /// The branch frame row `row` shows, if it shows one.
    pub fn picked(&self, row: usize) -> Option<&str> {
        self.picks
            .iter()
            .find(|(at, _)| *at == row)
            .map(|(_, refname)| refname.as_str())
    }

    fn push_parts(&mut self, text: &str, style: Style) {
        if self.lines.len() >= self.rows {
            return;
        }
        let (clipped, _) = sanitize(text, self.cols);
        self.lines.push(Line::new(clipped, style));
    }

    pub fn lines(&self) -> &[Line] {
        &self.lines
    }

    pub fn scroll(&self) -> Option<Scroll> {
        self.scroll
    }

    /// The rows' text, one line each, for a test to read.
    #[cfg(test)]
    pub fn text(&self) -> String {
        let mut out = String::new();
        for (i, line) in self.lines.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            out.push_str(&line.text);
        }
        out
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    Home,
    End,
    Enter,
    Esc,
    Tab,
    Backspace,
    Delete,
    Char(char),
    Ctrl(char),
}

/// The key a window chord names (`a`, `G`, `C-c`, `PageDown`), or none.
///
/// A shifted scalar arrives folded, with no shift (`G`, `?`), so a bare
/// scalar is the key itself. Control with a letter is `Ctrl`; any other
/// modified chord (`M-x`, `S-Up`, `C-S-d`) is nothing, so a chord the panes
/// never bound cannot reach a prompt as some other key.
pub fn key(chord: &str) -> Option<Key> {
    let mut scalars = chord.chars();
    if let (Some(c), None) = (scalars.next(), scalars.next()) {
        return (!c.is_control()).then_some(Key::Char(c));
    }
    if let Some(rest) = chord.strip_prefix("C-") {
        let mut scalars = rest.chars();
        return match (scalars.next(), scalars.next()) {
            (Some(c), None) if c.is_ascii_lowercase() => Some(Key::Ctrl(c)),
            _ => None,
        };
    }
    Some(match chord {
        "Space" => Key::Char(' '),
        "Return" => Key::Enter,
        "Escape" => Key::Esc,
        "Tab" => Key::Tab,
        "Backspace" => Key::Backspace,
        "Delete" => Key::Delete,
        "Up" => Key::Up,
        "Down" => Key::Down,
        "Left" => Key::Left,
        "Right" => Key::Right,
        "PageUp" => Key::PageUp,
        "PageDown" => Key::PageDown,
        "Home" => Key::Home,
        "End" => Key::End,
        _ => return None,
    })
}

/// Whether a held key may act again on the repeat clock: only a key that
/// moves the selection or a pane. Every other key acts once a press, so a
/// held `D` cannot go on to delete the row the first one's reload moved up,
/// and a held `y` answers no confirmation raised after it went down.
pub fn repeats(key: Key) -> bool {
    matches!(
        key,
        Key::Up
            | Key::Down
            | Key::Left
            | Key::Right
            | Key::PageUp
            | Key::PageDown
            | Key::Char('j')
            | Key::Char('k')
            | Key::Char(' ')
            | Key::Char('b')
            | Key::Ctrl('f')
            | Key::Ctrl('b')
            | Key::Ctrl('d')
            | Key::Ctrl('u')
            | Key::Backspace
    )
}

/// What the state machine needs from the window it is drawn in. A trait so a
/// test can stand in for the window: without this seam nothing that decides
/// whether a keystroke lands or deletes is reachable from a test.
pub trait Ui {
    /// The grid the next frame is laid out on: rows, then columns.
    fn size(&self) -> (usize, usize);
    /// Show `frame`: it replaces whatever was shown.
    fn draw(&mut self, frame: Frame) -> io::Result<()>;
    /// Throw away every input made before the frame drawn last was shown. A
    /// destructive confirmation must be answered by an input made AFTER it
    /// appeared.
    fn drain_input(&mut self) -> io::Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_expands_tabs_and_clips() {
        assert_eq!(sanitize("ab\tc", 40).0, "ab      c");
        assert_eq!(sanitize("abcdef", 3), ("abc".to_string(), 3));
        // An escape sequence in a commit subject is shown, never obeyed.
        assert_eq!(sanitize("a\x1b[2Jb", 40).0, "a\u{b7}[2Jb");
    }

    #[test]
    fn sanitize_replaces_c1_and_bidi() {
        // U+009B is CSI on xterm; the bidi overrides reorder rendered text.
        assert_eq!(sanitize("a\u{9b}2Jb", 40).0, "a\u{b7}2Jb");
        assert_eq!(sanitize("a\u{202e}b", 40).0, "a\u{b7}b");
        assert_eq!(sanitize("a\u{2066}b", 40).0, "a\u{b7}b");
    }

    /// The window draws one scalar a cell, wide and combining alike, so that
    /// is the width a column is padded to.
    #[test]
    fn sanitize_counts_one_cell_a_scalar() {
        assert_eq!(sanitize("世界", 40), ("世界".to_string(), 2));
        assert_eq!(sanitize("世界", 1), ("世".to_string(), 1));
        assert_eq!(sanitize("e\u{301}", 40).1, 2);
        for text in [
            "plain",
            "世界世界世界",
            "e\u{301}\u{301}x",
            "a\tb",
            "🚀🚀🚀",
        ] {
            for cols in 1..12 {
                let (shown, width) = sanitize(text, cols);
                assert!(width <= cols, "{text:?} at {cols} cols produced {width}");
                assert_eq!(shown.chars().count(), width, "{text:?} at {cols}");
            }
        }
    }

    #[test]
    fn scrub_neutralises_without_clipping() {
        assert_eq!(scrub("a\x1b[2Jb"), "a\u{b7}[2Jb");
        assert_eq!(scrub("a\u{202e}b"), "a\u{b7}b");
        assert_eq!(scrub("plain text stays"), "plain text stays");
        assert_eq!(scrub_lines("a\x1bb\nc"), "a\u{b7}b\nc");
    }

    #[test]
    fn frame_clips_rows_and_columns() {
        let mut f = Frame::new(2, 3);
        f.push_text("one", Style::PLAIN);
        f.pick("origin/two");
        f.push_text("two\x07", Style::dim());
        f.pick("origin/three");
        f.push_text("three", Style::PLAIN);
        assert_eq!(f.picked(1), Some("origin/two"));
        assert_eq!(f.picked(0), None);
        assert_eq!(f.picked(2), None, "a row past the frame shows nothing");
        assert_eq!(f.text(), "one\ntwo");
        assert_eq!(f.lines().len(), 2);
        assert_eq!(f.lines().get(1).map(|l| l.style), Some(Style::dim()));
        assert_eq!(f.room(), 0);
    }

    /// The scrollbar starts at the row pushed next and never claims more rows
    /// than the frame has left.
    #[test]
    fn the_scrollbar_names_the_rows_pushed_after_it() {
        let mut f = Frame::new(5, 10);
        f.push_text("title", Style::bar(CYAN));
        f.scrollbar(9, 40, 3);
        assert_eq!(
            f.scroll(),
            Some(Scroll {
                row: 1,
                visible: 4,
                total: 40,
                first: 3
            })
        );
    }

    /// A long line goes on over continuation rows in its own style, marked,
    /// with nothing lost; a short one is a row of its own; and the count is
    /// the rows `wrap` makes.
    #[test]
    fn long_lines_wrap_and_lose_nothing() {
        let lines = [
            Line::new("+abcdefghij", Style::fg(GREEN)),
            Line::plain("short"),
            Line::plain("a\tb"),
        ];
        let rows = wrap(&lines, 5);
        let texts: Vec<&str> = rows.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "+abcd",
                "\u{21aa} efg",
                "\u{21aa} hij",
                "short",
                "a    ",
                "\u{21aa}    ",
                "\u{21aa} b"
            ]
        );
        assert!(rows.iter().take(3).all(|l| l.style == Style::fg(GREEN)));
        assert_eq!(wrapped_len(&lines, 5), rows.len());
        // A window is the same rows, from any first row, of any count.
        for first in 0..rows.len() + 2 {
            for count in 0..rows.len() + 2 {
                let window = wrap_window(&lines, 5, first, count);
                let expected: Vec<Line> = rows.iter().skip(first).take(count).cloned().collect();
                assert_eq!(window, expected, "first {first} count {count}");
            }
        }
        for cols in 1..8 {
            let rows = wrap(&lines, cols);
            assert_eq!(wrapped_len(&lines, cols), rows.len());
            assert!(
                rows.iter().all(|l| l.text.chars().count() <= cols),
                "{cols}"
            );
            let kept: String = rows
                .iter()
                .map(|l| l.text.trim_start_matches(CONTINUED))
                .collect();
            if cols > 2 {
                assert!(kept.contains("abcdefghij"), "{cols}: {kept}");
            }
        }
    }

    #[test]
    fn chords_name_the_panes_keys_and_other_modified_ones_nothing() {
        for (chord, expected) in [
            ("a", Some(Key::Char('a'))),
            ("G", Some(Key::Char('G'))),
            ("?", Some(Key::Char('?'))),
            ("é", Some(Key::Char('é'))),
            ("Space", Some(Key::Char(' '))),
            ("Return", Some(Key::Enter)),
            ("Escape", Some(Key::Esc)),
            ("Tab", Some(Key::Tab)),
            ("Backspace", Some(Key::Backspace)),
            ("Delete", Some(Key::Delete)),
            ("Up", Some(Key::Up)),
            ("Down", Some(Key::Down)),
            ("Left", Some(Key::Left)),
            ("Right", Some(Key::Right)),
            ("PageUp", Some(Key::PageUp)),
            ("PageDown", Some(Key::PageDown)),
            ("Home", Some(Key::Home)),
            ("End", Some(Key::End)),
            ("C-c", Some(Key::Ctrl('c'))),
            ("C-f", Some(Key::Ctrl('f'))),
            ("C-S-d", None),
            ("C-Return", None),
            ("C-1", None),
            ("M-y", None),
            ("S-Up", None),
            ("F1", None),
            ("", None),
        ] {
            assert_eq!(key(chord), expected, "{chord}");
        }
    }

    #[test]
    fn only_movement_repeats() {
        for k in [Key::Down, Key::Char('j'), Key::PageDown, Key::Ctrl('d')] {
            assert!(repeats(k), "{k:?}");
        }
        for k in [
            Key::Char('D'),
            Key::Char('y'),
            Key::Char('p'),
            Key::Char('r'),
            Key::Char('s'),
            Key::Char('q'),
            Key::Enter,
            Key::Esc,
        ] {
            assert!(!repeats(k), "{k:?}");
        }
    }
}

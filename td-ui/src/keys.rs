//! The key list a window shows over its frame on `CHORD`: the sections of
//! keys its program says it binds, then the window's own, in a bordered
//! panel with a title bar, scrolled by the reading keys and the wheel and
//! closed by `Escape`, `q`, `CHORD` again or a left press. A program says
//! what its keys are as `Section`s of `Row`s, the keys spelled as the
//! keymap spells its chords (`j/Down`, `C-x C-s`) and what they do;
//! `check` holds a program's rows to that spelling, and `lines` shows
//! each description as a sentence. Pure: the lines, a chord's effect on
//! the list's state and a draw stream, and the `Overlay` a window keeps
//! them in.

use crate::chrome::{Item, List, ROW};
use crate::raster::{
    text_run, Draw, GlyphStyle, Primitive, Rect, Surface, ACCENT, BORDER, CHROME, PAPER, SELECTED,
};
use crate::xkb_symbols::COMMANDS;
use crate::{theme, CELL_WIDTH};

/// The chord that shows the key list, and hides it again.
pub const CHORD: &str = "F1";
/// The label of a bar's or a strip's button that opens the list.
pub const BUTTON: &str = "Help";
/// The label of the item that opens the list in a menu named `BUTTON`.
pub const ITEM: &str = "Keys";
/// How a keys cell writes the space bar, which the keymap spells `" "`
/// unmodified.
pub const SPACE: &str = "Space";
/// The words a keys cell may use, each whole, for what is not one key.
pub const WORDS: &[&str] = &[
    "a character",
    "characters",
    "any other key",
    "click",
    "C-click",
    "S-click",
    "double-click",
    "drag",
    "wheel",
    "arrows",
    "S-arrows",
    "C-arrows",
];
/// The widest keys column the lines pad to; a wider keys cell runs into
/// its description, two spaces after it.
pub const MAX_KEYS_COLUMN: usize = 24;
/// The panel's widest extent in cells.
pub const MAX_COLUMNS: usize = 100;
/// The panel's margin from the surface's edges, in cells.
pub const MARGIN: usize = 2;
pub const TITLE: &str = "Keys";
/// How to close the list, at the title bar's right where it fits beside
/// `TITLE`.
pub const TITLE_HINT: &str = "F1, q, Escape or a click closes";
/// The narrowest description a wrapped row keeps: below it the
/// description runs on and the list clips it.
pub const MIN_WRAP: usize = 16;

/// A key, or keys read together, and what it does. A row with empty keys
/// is a sentence of prose, or with an empty description too a spacer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Row {
    pub keys: &'static str,
    pub what: &'static str,
}

/// A row from a `(keys, what)` pair, for a program's constant tables.
pub const fn row((keys, what): (&'static str, &'static str)) -> Row {
    Row { keys, what }
}

/// A titled block of rows: a view's keys, a mode's, the window's.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Section {
    pub title: &'static str,
    pub rows: Vec<Row>,
}

impl Section {
    pub fn new(title: &'static str, rows: &[(&'static str, &'static str)]) -> Self {
        Self {
            title,
            rows: rows.iter().copied().map(row).collect(),
        }
    }
}

/// The window's own keys, which every list ends with.
pub fn window() -> Section {
    Section::new(
        "Window",
        &[
            (CHORD, "show or hide this list of keys"),
            (theme::CHORD, "next colour theme, kept for this program"),
        ],
    )
}

/// One problem per cell of `sections` that breaks the list's spelling or
/// style, naming its section and row: a keys cell `spelled` refuses, a
/// row with keys and a blank description, a description whose first
/// word is one letter other than `a` (a key's name or a variable, which
/// `sentence` would capitalise), or a section title not starting with an
/// upper-case letter. A row with empty keys is prose, or with an empty
/// description a spacer, and is held only to that first word.
pub fn check(sections: &[Section]) -> Vec<String> {
    checked(sections, |keys| {
        (!spelled(keys)).then_some("keys not spelled as the keymap spells them")
    })
}

/// `check` without the spelling, for a program whose rows show the
/// spelling of its own menus (td-editor's key profiles): the
/// descriptions and titles as `check` holds them, and alternatives
/// joined by a `/` with no space beside it.
pub fn check_style(sections: &[Section]) -> Vec<String> {
    checked(sections, |keys| {
        (keys.contains(" /") || keys.contains("/ "))
            .then_some("alternatives joined by a spaced `/`")
    })
}

fn checked(
    sections: &[Section],
    keys_problem: impl Fn(&str) -> Option<&'static str>,
) -> Vec<String> {
    let mut out = Vec::new();
    for section in sections {
        let title = section.title;
        if !title.chars().next().is_some_and(char::is_uppercase) {
            out.push(format!(
                "section {title:?}: title does not start with an upper-case letter"
            ));
        }
        for (index, row) in section.rows.iter().enumerate() {
            let number = index + 1;
            let keys = row.keys;
            if letter_first(row.what) {
                out.push(format!(
                    "section {title:?} row {number} keys {keys:?}: description starts with \
                     a one-letter word, a key or a variable the list would capitalise"
                ));
            }
            if keys.is_empty() {
                continue;
            }
            if let Some(problem) = keys_problem(keys) {
                out.push(format!(
                    "section {title:?} row {number} keys {keys:?}: {problem}"
                ));
            }
            if row.what.trim().is_empty() {
                out.push(format!(
                    "section {title:?} row {number} keys {keys:?}: blank description"
                ));
            }
        }
    }
    out
}

/// Whether `what`'s first word is one letter other than `a` or `A`: a
/// key's name or a variable, which `sentence` would capitalise.
fn letter_first(what: &str) -> bool {
    let first = what.split_whitespace().next().unwrap_or_default();
    let mut chars = first.chars();
    matches!(
        (chars.next(), chars.next()),
        (Some(c), None) if c.is_ascii_alphabetic() && !c.eq_ignore_ascii_case(&'a')
    )
}

/// Whether `cell` is keys as the keymap spells them: alternatives joined
/// by an unspaced `/`, each one of `WORDS` whole or a sequence of items
/// joined by one space, each item a chord or a range `A..B` of two
/// chords. A chord is the prefixes `C-`, `M-`, `S-` in that order, then
/// a key name the keymap emits, `SPACE`, or one printable ASCII
/// character. As the keymap spells them, `S-` with a character needs
/// `C-` or `M-` beside it (Shift alone types the shifted character), and
/// a letter under `C-` or `M-` is lower-case. The `/` key stands alone:
/// unmodified, it is a whole cell.
pub fn spelled(cell: &str) -> bool {
    if cell == "/" {
        return true;
    }
    let mut rest = cell;
    loop {
        let after = match WORDS.iter().find_map(|word| {
            rest.strip_prefix(word)
                .filter(|after| after.is_empty() || after.starts_with('/'))
        }) {
            Some(after) => after,
            None => match sequence(rest) {
                Some(after) => after,
                None => return false,
            },
        };
        if after.is_empty() {
            return true;
        }
        match after.strip_prefix('/') {
            Some(next) if !next.is_empty() => rest = next,
            _ => return false,
        }
    }
}

/// Items joined by one space, up to the end or a `/`; what follows.
fn sequence(text: &str) -> Option<&str> {
    let mut rest = item(text)?;
    while let Some(next) = rest.strip_prefix(' ') {
        rest = item(next)?;
    }
    Some(rest)
}

/// A chord, or a range of two; what follows.
fn item(text: &str) -> Option<&str> {
    let rest = chord(text)?;
    match rest.strip_prefix("..") {
        Some(next) => chord(next),
        None => Some(rest),
    }
}

/// A chord as the keymap spells it; what follows.
fn chord(text: &str) -> Option<&str> {
    let mut rest = text;
    let mut held = [false; 3];
    for (mark, slot) in ["C-", "M-", "S-"].into_iter().zip(held.iter_mut()) {
        if let Some(after) = rest.strip_prefix(mark) {
            *slot = true;
            rest = after;
        }
    }
    let [control, alt, shift] = held;
    // A name is a run of letters and digits; any other character is one
    // key by itself.
    let run = rest
        .find(|c: char| !c.is_ascii_alphanumeric())
        .unwrap_or(rest.len());
    let length = match run {
        0 => rest.chars().next()?.len_utf8(),
        run => run,
    };
    let (name, after) = rest.split_at_checked(length)?;
    let named = COMMANDS.iter().any(|(command, _)| *command == name);
    let character = match name.as_bytes() {
        [byte] if byte.is_ascii_graphic() => Some(char::from(*byte)),
        _ => None,
    };
    let typed = character.is_some() || name == SPACE;
    let fits = if named {
        true
    } else if typed {
        let chorded = control || alt;
        (!shift || chorded)
            && !(chorded && character.is_some_and(|c| c.is_ascii_uppercase()))
            && !(character == Some('/') && !(control || alt || shift))
    } else {
        false
    };
    fits.then_some(after)
}

/// One line of the list: a section's title, or a row as text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Line {
    pub text: String,
    pub title: bool,
}

/// The list's lines for `sections` at `columns` cells wide: each
/// section's title, then its rows, indented, with the keys padded to the
/// widest of them all but at most `MAX_KEYS_COLUMN`, a blank line between
/// sections. Each description is shown as a `sentence`. A description
/// wider than what is left of `columns` wraps at its spaces onto lines
/// under it, a word wider than that at its width; the space left is at
/// least `MIN_WRAP`.
pub fn lines(sections: &[Section], columns: usize) -> Vec<Line> {
    let width = sections
        .iter()
        .flat_map(|section| &section.rows)
        .map(|row| row.keys.chars().count())
        .max()
        .unwrap_or(0)
        .min(MAX_KEYS_COLUMN);
    let mut out = Vec::new();
    let plain = |text: String| Line { text, title: false };
    for section in sections {
        if !out.is_empty() {
            out.push(plain(String::new()));
        }
        out.push(Line {
            text: section.title.to_string(),
            title: true,
        });
        for row in &section.rows {
            let head = format!("  {:width$}  ", row.keys);
            let room = columns.saturating_sub(head.chars().count()).max(MIN_WRAP);
            let mut parts = crate::text::wrap(&sentence(row.what), room).into_iter();
            out.push(plain(format!("{head}{}", parts.next().unwrap_or_default())));
            let indent = " ".repeat(width + 4);
            out.extend(parts.map(|part| plain(format!("{indent}{part}"))));
        }
    }
    out
}

/// `what`, trimmed, as a sentence: a lower-case ASCII letter first is
/// upper-cased, and a `.` ends it unless it ends in `.`, `?`, `!` or `:`
/// already. A blank description stays blank.
pub fn sentence(what: &str) -> String {
    let what = what.trim();
    let mut chars = what.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    let mut out = String::with_capacity(what.len() + 1);
    out.push(first.to_ascii_uppercase());
    out.push_str(chars.as_str());
    if !what.ends_with(['.', '?', '!', ':']) {
        out.push('.');
    }
    out
}

/// What a chord did to the open list.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Step {
    /// The list closed.
    Closed,
    /// The list scrolled.
    Moved,
    /// Nothing moved: not one of the list's keys, or one held at an end.
    /// The list keeps it from the program all the same while it is open.
    Kept,
}

/// Whether the list is shown, and its first shown line.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Help {
    open: bool,
    first: usize,
}

impl Help {
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Shows the list from its top.
    pub fn open(&mut self) {
        *self = Self {
            open: true,
            first: 0,
        };
    }

    pub fn close(&mut self) {
        self.open = false;
    }

    pub fn first(&self) -> usize {
        self.first
    }

    /// A chord while the list is open over `total` lines, `page` of them
    /// shown: `Escape`, `q`, `?` and `CHORD` close it; `j`, `Down`, `k`,
    /// `Up`, `PageDown`, `" "`, `PageUp`, `Home`, `g`, `End` and `G`
    /// scroll it, stopping where the last line is on the last row.
    pub fn key(&mut self, chord: &str, total: usize, page: usize) -> Step {
        let page = page.max(1);
        let last = total.saturating_sub(page);
        let first = match chord {
            "Escape" | "q" | "?" | CHORD => {
                self.open = false;
                return Step::Closed;
            }
            "j" | "Down" => self.first.saturating_add(1),
            "k" | "Up" => self.first.saturating_sub(1),
            // The keymap spells an unmodified space as itself.
            "PageDown" | " " => self.first.saturating_add(page),
            "PageUp" => self.first.saturating_sub(page),
            "Home" | "g" => 0,
            "End" | "G" => last,
            _ => return Step::Kept,
        };
        let first = first.min(last);
        if first == self.first {
            // A held key at an end stops repeating and paints nothing.
            return Step::Kept;
        }
        self.first = first;
        Step::Moved
    }

    /// Holds the first line where the last line is on the last row, as
    /// the surface or the lines change under it.
    pub fn clamp(&mut self, total: usize, page: usize) {
        self.first = self.first.min(total.saturating_sub(page.max(1)));
    }

    /// Wheel travel of `rows` lines, down when positive.
    pub fn wheel(&mut self, rows: isize, total: usize, page: usize) {
        let last = total.saturating_sub(page.max(1));
        let travel = rows.unsigned_abs();
        self.first = if rows < 0 {
            self.first.saturating_sub(travel)
        } else {
            self.first.saturating_add(travel)
        }
        .min(last);
    }
}

/// The panel over a surface: `MARGIN` cells in from its edges and at most
/// `MAX_COLUMNS` cells wide, centred, a one-pixel border at the scale
/// around a `ROW`-tall title bar and the list under it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Panel {
    pub frame: Rect,
    pub title: Rect,
    pub list: List,
}

impl Panel {
    /// `None` when the surface cannot hold the title bar and one row.
    pub fn new(surface: Surface) -> Option<Self> {
        let s = surface.scale.value();
        let margin = MARGIN.checked_mul(CELL_WIDTH)?.checked_mul(s)?;
        let width = surface
            .width
            .checked_sub(margin.checked_mul(2)?)?
            .min(MAX_COLUMNS.checked_mul(CELL_WIDTH)?.checked_mul(s)?);
        let height = surface.height.checked_sub(margin.checked_mul(2)?)?;
        let frame = Rect {
            x: i64::try_from((surface.width - width) / 2).ok()?,
            y: i64::try_from(margin).ok()?,
            width: u32::try_from(width).ok()?,
            height: u32::try_from(height).ok()?,
        };
        let border = u32::try_from(s).ok()?;
        let inner = Rect {
            x: frame.x + i64::from(border),
            y: frame.y + i64::from(border),
            width: frame.width.checked_sub(border.checked_mul(2)?)?,
            height: frame.height.checked_sub(border.checked_mul(2)?)?,
        };
        let title = Rect {
            height: u32::try_from(ROW.checked_mul(s)?).ok()?,
            ..inner
        };
        let list = List::new(
            surface,
            Rect {
                y: inner.y + i64::from(title.height),
                height: inner.height.checked_sub(title.height)?,
                ..inner
            },
        )?;
        (list.rows() > 0).then_some(Self { frame, title, list })
    }

    /// The lines shown at once.
    pub fn page(&self) -> usize {
        self.list.rows()
    }

    /// The cells a line has: the row's text starts after the list's inset
    /// and its two-cell prefix, and stops an inset short of the gutter.
    pub fn columns(&self, surface: Surface) -> usize {
        let cell = CELL_WIDTH.saturating_mul(surface.scale.value()).max(1);
        (self.list.body().width as usize / cell).saturating_sub(4)
    }

    /// Paints the panel showing `lines` from `help`'s first: the border,
    /// the title bar in the selection's colours with `TITLE` at its left
    /// and `TITLE_HINT` at its right, and the lines as the list's rows,
    /// each title in `ACCENT`.
    pub fn emit(
        &self,
        surface: Surface,
        help: &Help,
        lines: &[Line],
        damage: Rect,
        sink: &mut dyn FnMut(Draw),
    ) {
        let s = surface.scale.value() as i64;
        let cell = CELL_WIDTH as i64 * s;
        fill(self.frame, BORDER, damage, sink);
        fill(self.title, SELECTED, damage, sink);
        let style = GlyphStyle::medium(PAPER, SELECTED);
        let top = self.title.y + 4 * s;
        text_run(
            surface.scale,
            TITLE.chars(),
            (self.title.x + cell, top),
            self.title,
            style,
            damage,
            sink,
        );
        // Where it would meet the title it is left out.
        let hint = TITLE_HINT.chars().count() as i64 + 1;
        let room = i64::from(self.title.width) / cell.max(1);
        if room >= hint + TITLE.chars().count() as i64 + 2 {
            text_run(
                surface.scale,
                TITLE_HINT.chars(),
                (
                    self.title.x + i64::from(self.title.width) - hint * cell,
                    top,
                ),
                self.title,
                style,
                damage,
                sink,
            );
        }
        let first = help.first.min(lines.len());
        let shown = lines.get(first..).unwrap_or_default();
        // A title's row is left blank here and painted in the accent below.
        let items = shown.iter().map(|line| Item {
            label: if line.title { "" } else { &line.text },
            meta: "",
            enabled: true,
            marked: false,
        });
        // No row is selected: the list is read, not chosen from.
        self.list
            .emit(items, first, usize::MAX, lines.len(), damage, sink);
        let accent = GlyphStyle::medium(ACCENT, CHROME);
        // Chrome's row label sits one cell's inset and its two-cell
        // prefix in, four pixels down at the scale.
        for (index, line) in shown.iter().enumerate() {
            let Some(rect) = self.list.row(index) else {
                break;
            };
            if line.title {
                text_run(
                    surface.scale,
                    line.text.chars(),
                    (rect.x + 3 * cell, rect.y + 4 * s),
                    rect,
                    accent,
                    damage,
                    sink,
                );
            }
        }
    }
}

/// The key list as a window keeps it over its frame: the list's state,
/// the sections it was opened with, and their lines laid out at the
/// surface's width. A window routes its keyboard's chords, its wheel and
/// its resizes here while the list is open, and paints it last, as
/// td-ui/DESIGN.md's "Key list" says.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Overlay {
    help: Help,
    sections: Vec<Section>,
    lines: Vec<Line>,
}

impl Overlay {
    pub fn is_open(&self) -> bool {
        self.help.is_open()
    }

    pub fn help(&self) -> Help {
        self.help
    }

    /// The lines as laid out for the surface last given.
    pub fn lines(&self) -> &[Line] {
        &self.lines
    }

    /// Opens from the top with `sections`, then `window`'s, laid out for
    /// `surface`.
    pub fn open(&mut self, mut sections: Vec<Section>, surface: Surface) {
        sections.push(window());
        self.sections = sections;
        self.help.open();
        self.lay_out(surface);
    }

    /// Lays the open list out again for `surface`, as after a resize,
    /// its first line held to the new last page.
    pub fn lay_out(&mut self, surface: Surface) {
        if !self.is_open() {
            return;
        }
        let columns = Panel::new(surface).map_or(usize::MAX, |panel| panel.columns(surface));
        self.lines = lines(&self.sections, columns);
        self.help.clamp(self.lines.len(), page(surface));
    }

    /// Closes the list and drops its lines, as for a focus or mode the
    /// window ends it on.
    pub fn close(&mut self) {
        *self = Self::default();
    }

    /// A chord while the list is open (`Help::key`), over `surface`'s
    /// page; closing drops the lines. A closed list keeps nothing.
    pub fn key(&mut self, chord: &str, surface: Surface) -> Step {
        if !self.is_open() {
            return Step::Kept;
        }
        let step = self.help.key(chord, self.lines.len(), page(surface));
        if step == Step::Closed {
            self.close();
        }
        step
    }

    /// A left-button press while the list is open: it closes the list as
    /// `Escape` does, and the window lets neither it nor the release
    /// after it reach anything else. On a closed list it returns `Kept`
    /// and changes nothing, so there is nothing to repaint.
    pub fn press(&mut self) -> Step {
        if !self.is_open() {
            return Step::Kept;
        }
        self.close();
        Step::Closed
    }

    /// Wheel travel of `rows` lines over `surface`'s page, while open.
    pub fn wheel(&mut self, rows: isize, surface: Surface) {
        if self.is_open() {
            self.help.wheel(rows, self.lines.len(), page(surface));
        }
    }

    /// Paints the open list over `surface`, its first line held to the
    /// last page first; nothing when it is closed or the surface holds no
    /// panel, the list keeping its keys all the same.
    pub fn emit(&mut self, surface: Surface, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        if !self.is_open() {
            return;
        }
        let Some(panel) = Panel::new(surface) else {
            return;
        };
        self.help.clamp(self.lines.len(), panel.page());
        panel.emit(surface, &self.help, &self.lines, damage, sink);
    }
}

/// The lines a panel over `surface` shows at once; one without a panel.
fn page(surface: Surface) -> usize {
    Panel::new(surface).map_or(1, |panel| panel.page())
}

fn fill(rect: Rect, color: u32, damage: Rect, sink: &mut dyn FnMut(Draw)) {
    if let Some(clip) = rect.intersection(damage) {
        sink(Draw {
            clip,
            primitive: Primitive::Fill { rect, color },
        });
    }
}

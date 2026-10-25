//! The account and regional settings form: its drafts, edited by keys,
//! and the page that renders them. Drafts are bounded tokens; wire
//! admission, policy checks, and catalog membership remain the service's.

use crate::listbox::{Choice, ListBox};
use td_ui::chrome::{Field, Status, TextEntry, ROW};
use td_ui::raster::{
    text_run, Composition, Draw, GlyphStyle, Primitive, Rect, Surface, CHROME, INK,
};
use td_ui::{CELL_HEIGHT, CELL_WIDTH};

const INSET: usize = CELL_WIDTH;
const LABEL_ROWS: [usize; 4] = [4, 8, 12, 16];
const LIMITS: [usize; 4] = [32, 63, 64, 64];
const LABELS: [&str; 4] = ["Username", "Hostname", "Keyboard layout", "Time zone"];
const PLACEHOLDERS: [&str; 4] = [
    "Choose a username",
    "Choose a hostname",
    "Select a layout",
    "Select a time zone",
];
const FOOTER: &str = "Account and region \u{b7} step 3 of 6";
/// The row under the last field that says what became of a review.
const NOTICE_ROW: usize = 18;
/// The one keyboard layout the installer offers.
pub const KEYBOARD: &str = "us";
/// The keyboard layouts its chooser lists: version 1's one.
const KEYBOARDS: &[&str] = &[KEYBOARD];
/// The keyboard layout field, a chooser like the time zone's.
const KEYBOARD_FIELD: usize = 2;
/// The rows an open chooser's list shows at most.
const LIST_ROWS: usize = 8;
/// A chooser row's mark that it opens a list.
const OPENS: &str = "\u{25be}";
/// The time zone chosen when the catalog has it and nothing else was.
const DEFAULT_ZONE: &str = "Etc/UTC";
/// The rows PageUp and PageDown move a time zone selection.
const ZONE_PAGE: usize = 16;
/// The time zone field, the last.
pub const TIME_ZONE: usize = 3;

/// What `Draft::key` does with a key outside the time zone row, for the
/// window's key list.
pub const FIELD_KEYS: &[(&str, &str)] = &[
    (
        "Tab/S-Tab",
        "the next or previous field, round from the last",
    ),
    ("Down/Return", "the next field"),
    ("Up", "the previous field"),
    (
        "Space/M-Down",
        "open the keyboard layout's list on its row: Return or a click there chooses, Escape closes it unchanged",
    ),
    ("click", "focus a field, or open a chooser's list"),
    ("Left/Right", "move the caret in the username or hostname"),
    ("Home/End", "the caret to the start or end of its text"),
    (
        "Backspace/Delete",
        "delete the character before or at the caret",
    ),
    (
        "a character",
        "typed at the caret in the username or hostname: printable ASCII, no space",
    ),
];

/// The keys the open time zone list takes; the window lists them first
/// while it is open. The keyboard layout's list is the form's row.
pub const LIST_KEYS: &[(&str, &str)] = &[
    ("Return", "keep the chosen entry and close the list"),
    (
        "Escape",
        "close the list, putting back the entry it opened on",
    ),
    (
        "Tab/S-Tab",
        "keep the choice and go to the next or previous field",
    ),
    ("Up/Down", "the time zone above or below"),
    ("PageUp/PageDown", "a page of time zones up or down"),
    ("Home/End", "the first or last time zone"),
    (
        "a character",
        "seek the first time zone beginning with what was typed",
    ),
    ("Backspace", "drop the last character sought"),
    (
        "click",
        "choose the entry under the pointer, or elsewhere close the list unchanged",
    ),
    ("wheel", "scroll the list"),
];

/// What `Draft::key` does with a key in the time zone row.
pub const ZONE_KEYS: &[(&str, &str)] = &[
    ("Tab/S-Tab", "the first or previous field"),
    ("Up/Down", "the time zone above or below"),
    ("PageUp/PageDown", "a page of time zones up or down"),
    ("Home/End", "the first or last time zone"),
    (
        "Space/M-Down",
        "open the list of time zones: there the same keys move, Return or a click chooses and Escape closes it unchanged",
    ),
    ("wheel", "scroll the open list"),
    (
        "a character",
        "seek the first time zone beginning with what was typed, ignoring case",
    ),
    ("Backspace", "drop the last character sought"),
];

/// The form's drafts, focus and carets, edited a key at a time. A draft
/// is only typed text: the service checks every value it is proposed.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Draft {
    texts: [String; 2],
    carets: [usize; 2],
    focused: usize,
    zones: Vec<String>,
    zone: Option<usize>,
    /// What has been typed toward a time zone since it was focused.
    seek: String,
    /// Whether the focused chooser's list is open, its first shown row,
    /// and the zone chosen when it opened, which Escape restores.
    open: bool,
    first: usize,
    before: Option<usize>,
}

/// An open chooser's list, as the page paints it: the shown rows from
/// `first`, out of `total`, and the chosen row's index in all of them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Dropdown<'a> {
    pub field: usize,
    pub shown: Vec<&'a str>,
    pub first: usize,
    pub total: usize,
    pub chosen: Option<usize>,
}

/// What a point on the form is: a field's row, or a row of the open list
/// by its index in the whole list.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Hit {
    Field(usize),
    Item(usize),
    /// The open list's scrollbar or border: inside it, but no entry.
    List,
}

impl Draft {
    /// The field that has focus: username, hostname, keyboard or time zone.
    pub fn focused(&self) -> usize {
        self.focused
    }

    /// What has been typed toward a time zone since its row was focused.
    pub fn seek(&self) -> &str {
        &self.seek
    }

    /// The four values as the page shows them; an unchosen zone is empty.
    pub fn values(&self) -> [&str; 4] {
        let [username, hostname] = &self.texts;
        let zone = self
            .zone
            .and_then(|index| self.zones.get(index))
            .map_or("", String::as_str);
        [username, hostname, KEYBOARD, zone]
    }

    /// The service's catalog. A zone already chosen stays chosen when the
    /// catalog still has it; otherwise UTC, or nothing, is.
    pub fn offer(&mut self, zones: Vec<String>) {
        let [.., chosen] = self.values();
        let zone = [chosen, DEFAULT_ZONE]
            .iter()
            .filter(|wanted| !wanted.is_empty())
            .find_map(|wanted| zones.iter().position(|zone| zone == wanted));
        self.zone = zone;
        self.zones = zones;
        self.seek.clear();
    }

    /// What must still be filled in before a review, if anything.
    pub fn missing(&self) -> Option<&'static str> {
        let [username, hostname, _, zone] = self.values();
        if username.is_empty() {
            Some("Enter a username before review.")
        } else if hostname.is_empty() {
            Some("Enter a hostname before review.")
        } else if zone.is_empty() {
            Some("Choose a time zone before review.")
        } else {
            None
        }
    }

    /// Whether there is a catalog to choose from.
    pub fn offered(&self) -> bool {
        !self.zones.is_empty()
    }

    /// Whether a chooser's list is open; it takes every key until closed.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// The open list, for the page to paint.
    pub fn dropdown(&self) -> Option<Dropdown<'_>> {
        if !self.open {
            return None;
        }
        let shown = if self.focused == TIME_ZONE {
            self.zones
                .iter()
                .skip(self.first)
                .take(LIST_ROWS)
                .map(String::as_str)
                .collect()
        } else {
            KEYBOARDS
                .iter()
                .skip(self.first)
                .take(LIST_ROWS)
                .copied()
                .collect()
        };
        Some(Dropdown {
            field: self.focused,
            shown,
            first: self.first,
            total: self.listed(),
            chosen: self.at(),
        })
    }

    /// A click on the form: a field focuses it, a chooser's row opening
    /// its list or closing the one open; a row of the open list chooses
    /// it and closes the list; anywhere else closes an open list
    /// unchanged. True when the draft changed.
    pub fn click(&mut self, hit: Option<Hit>) -> bool {
        let before = self.state();
        match hit {
            Some(Hit::Item(index)) if self.open && index < self.listed() => {
                if self.focused == TIME_ZONE {
                    self.zone = Some(index);
                }
                self.close(true);
            }
            Some(Hit::Field(field)) if self.open && field == self.focused => self.close(true),
            Some(Hit::Field(field)) if field < LABELS.len() => {
                self.close(false);
                self.focus(field);
                if field >= KEYBOARD_FIELD {
                    self.open_list();
                }
            }
            Some(Hit::List) => {}
            // Off the form a click closes an open list, and changes
            // nothing else, so it leaves a seek and the step buttons be.
            _ if self.open => self.close(false),
            _ => {}
        }
        before != self.state()
    }

    /// Closes an open list keeping its choice, as the page is left by
    /// something other than a key. True when one was open.
    pub fn shut(&mut self) -> bool {
        if !self.open {
            return false;
        }
        self.close(true);
        true
    }

    /// Scrolls the open list by `rows`, without choosing. True when it
    /// moved.
    pub fn scroll(&mut self, rows: isize) -> bool {
        if !self.open {
            return false;
        }
        let last = self.listed().saturating_sub(LIST_ROWS);
        let first = if rows < 0 {
            self.first.saturating_sub(rows.unsigned_abs())
        } else {
            self.first.saturating_add(rows.unsigned_abs()).min(last)
        };
        let moved = first != self.first;
        self.first = first;
        moved
    }

    /// The entries the focused chooser lists.
    fn listed(&self) -> usize {
        if self.focused == TIME_ZONE {
            self.zones.len()
        } else if self.focused == KEYBOARD_FIELD {
            KEYBOARDS.len()
        } else {
            0
        }
    }

    /// The focused chooser's chosen entry.
    fn at(&self) -> Option<usize> {
        if self.focused == TIME_ZONE {
            self.zone
        } else {
            KEYBOARDS.iter().position(|layout| *layout == KEYBOARD)
        }
    }

    fn open_list(&mut self) {
        if self.focused >= KEYBOARD_FIELD && self.listed() > 0 {
            self.open = true;
            self.before = self.zone;
            self.seek.clear();
            self.reveal();
        }
    }

    /// Closes the list, keeping what it chose or restoring the zone it
    /// opened on.
    fn close(&mut self, keep: bool) {
        if self.open && !keep {
            self.zone = self.before;
        }
        self.open = false;
        self.seek.clear();
    }

    /// Moves the list's first row as little as possible to show the
    /// chosen entry.
    fn reveal(&mut self) {
        let last = self.listed().saturating_sub(LIST_ROWS);
        if let Some(at) = self.at() {
            if at < self.first {
                self.first = at;
            } else if at >= self.first.saturating_add(LIST_ROWS) {
                self.first = at + 1 - LIST_ROWS;
            }
        }
        self.first = self.first.min(last);
    }

    fn state(&self) -> (String, Option<usize>, bool, usize, usize) {
        (
            self.seek.clone(),
            self.zone,
            self.open,
            self.first,
            self.focused,
        )
    }

    /// Applies one key chord, as td-ui names it; a key the focused field
    /// does not take changes nothing. True when the draft changed.
    pub fn key(&mut self, chord: &str) -> bool {
        // The catalog is not edited by keys, so it is left out.
        let before = (self.texts.clone(), self.carets, self.state());
        let fields = LABELS.len();
        match chord {
            // An open list takes every key: it moves, chooses or closes.
            "Escape" if self.open => self.close(false),
            "Return" if self.open => self.close(true),
            "Tab" | "S-Tab" if self.open => {
                self.close(true);
                self.key(chord);
            }
            _ if self.open => {
                if self.focused == TIME_ZONE {
                    self.choose(chord);
                    self.reveal();
                }
            }
            "Tab" => self.focus((self.focused + 1) % fields),
            "S-Tab" => self.focus((self.focused + fields - 1) % fields),
            " " | "M-Down" if self.focused >= KEYBOARD_FIELD => self.open_list(),
            _ if self.focused == TIME_ZONE => self.choose(chord),
            "Down" | "Return" => self.focus(self.focused + 1),
            "Up" => self.focus(self.focused.saturating_sub(1)),
            _ => self.type_key(chord),
        }
        before != (self.texts.clone(), self.carets, self.state())
    }

    fn focus(&mut self, field: usize) {
        self.focused = field.min(TIME_ZONE);
        self.seek.clear();
    }

    fn type_key(&mut self, chord: &str) {
        let field = self.focused;
        let (Some(text), Some(caret), Some(limit)) = (
            self.texts.get_mut(field),
            self.carets.get_mut(field),
            LIMITS.get(field),
        ) else {
            return;
        };
        match chord {
            "Left" => *caret = caret.saturating_sub(1),
            "Right" => *caret = (*caret + 1).min(text.len()),
            "Home" => *caret = 0,
            "End" => *caret = text.len(),
            "Backspace" if *caret > 0 => {
                *caret -= 1;
                text.remove(*caret);
            }
            "Delete" if *caret < text.len() => {
                text.remove(*caret);
            }
            _ => {
                if let Some(typed) = printable(chord) {
                    if text.len() < *limit {
                        text.insert(*caret, typed);
                        *caret += 1;
                    }
                }
            }
        }
    }

    /// Moves through the catalog, or seeks the first zone beginning with
    /// what was typed, ignoring case; a character that matches none is
    /// not taken.
    fn choose(&mut self, chord: &str) {
        let last = self.zones.len().saturating_sub(1);
        let at = self.zone;
        let step = |by: usize, down: bool| match at {
            Some(index) if down => index.saturating_add(by).min(last),
            Some(index) => index.saturating_sub(by),
            None => 0,
        };
        let moved = match chord {
            "Down" => step(1, true),
            "Up" => step(1, false),
            "PageDown" => step(ZONE_PAGE, true),
            "PageUp" => step(ZONE_PAGE, false),
            "Home" => 0,
            "End" => last,
            "Backspace" => {
                self.seek.pop();
                return;
            }
            _ => {
                if let Some(typed) = printable(chord) {
                    self.seek_zone(typed);
                }
                return;
            }
        };
        self.seek.clear();
        if moved < self.zones.len() {
            self.zone = Some(moved);
        }
    }

    fn seek_zone(&mut self, typed: char) {
        let mut seek = self.seek.clone();
        seek.push(typed);
        let found = self.zones.iter().position(|zone| {
            zone.get(..seek.len())
                .is_some_and(|head| head.eq_ignore_ascii_case(&seek))
        });
        if let Some(found) = found {
            self.seek = seek;
            self.zone = Some(found);
        }
    }

    /// The page over these drafts; `hint` replaces the time zone row's
    /// placeholder while there is no zone to show, and `notice` says what
    /// became of a review.
    pub fn page<'a>(
        &'a self,
        surface: Surface,
        hint: Option<&'a str>,
        notice: Option<&'a str>,
    ) -> Option<SettingsPage<'a>> {
        SettingsPage::new(
            surface,
            self.values(),
            Some(self.focused),
            self.carets,
            true,
        )
        .map(|page| {
            page.with_hint(hint)
                .with_notice(notice)
                .with_dropdown(self.dropdown())
        })
    }
}

/// The one character a chord types: printable, non-space ASCII, the only
/// kind a draft token holds.
fn printable(chord: &str) -> Option<char> {
    let mut chars = chord.chars();
    match (chars.next(), chars.next()) {
        (Some(typed), None) if typed.is_ascii_graphic() => Some(typed),
        _ => None,
    }
}

/// A form view: the two account fields are editable, while keyboard and time
/// zone are chooser rows for catalog selections. The caller owns the drafts,
/// selection, focus and caret; this page does not approve account or catalog
/// policy. Empty choices are allowed before the user selects them.
pub struct SettingsPage<'a> {
    surface: Surface,
    values: [&'a str; 4],
    focused: Option<usize>,
    carets: [usize; 2],
    caret_visible: bool,
    entries: [TextEntry; 2],
    choices: [ListBox; 2],
    footer: Status,
    hint: Option<&'a str>,
    notice: Option<&'a str>,
    /// The open list and the box it is painted in.
    dropdown: Option<(Dropdown<'a>, ListBox)>,
}

impl<'a> SettingsPage<'a> {
    /// Refuse invalid draft bytes and geometry before drawing the form.
    pub fn new(
        surface: Surface,
        values: [&'a str; 4],
        focused: Option<usize>,
        carets: [usize; 2],
        caret_visible: bool,
    ) -> Option<Self> {
        crate::supported_page(surface)?;
        let scale = surface.scale.value();
        if focused.is_some_and(|index| index >= values.len()) {
            return None;
        }
        for (index, value) in values.iter().enumerate() {
            if value.len() > *LIMITS.get(index)?
                || !value.bytes().all(|byte| byte.is_ascii_graphic())
            {
                return None;
            }
        }
        for (index, caret) in carets.iter().enumerate() {
            if *caret > values.get(index)?.len() {
                return None;
            }
        }
        let width = surface.width.checked_sub(2 * INSET * scale)?;
        let entry_at = |row: usize| {
            TextEntry::new(
                surface,
                Rect {
                    x: (INSET * scale) as i64,
                    y: ((row + 1) * ROW * scale) as i64,
                    width: width as u32,
                    height: (ROW * scale) as u32,
                },
            )
        };
        let choice_at = |row: usize| {
            ListBox::new(
                surface,
                Rect {
                    x: (INSET * scale) as i64,
                    y: ((row + 1) * ROW * scale) as i64,
                    width: width as u32,
                    height: (ROW * scale) as u32,
                },
            )
        };
        let entries = [
            entry_at(*LABEL_ROWS.first()?)?,
            entry_at(*LABEL_ROWS.get(1)?)?,
        ];
        let choices = [
            choice_at(*LABEL_ROWS.get(2)?)?,
            choice_at(*LABEL_ROWS.get(3)?)?,
        ];
        let footer = Status::new(surface);
        if choices.last()?.rect().y + i64::from(choices.last()?.rect().height) > footer.rect().y
            || ((NOTICE_ROW * ROW + CELL_HEIGHT) * scale) as i64 > footer.rect().y
        {
            return None;
        }
        Some(Self {
            surface,
            values,
            focused,
            carets,
            caret_visible,
            entries,
            choices,
            footer,
            hint: None,
            notice: None,
            dropdown: None,
        })
    }

    /// Shows `dropdown` under its chooser's row, or over it when the
    /// footer leaves no room below. One that cannot be placed is not
    /// shown.
    pub fn with_dropdown(mut self, dropdown: Option<Dropdown<'a>>) -> Self {
        self.dropdown = dropdown.and_then(|dropdown| {
            let chooser = self
                .choices
                .get(dropdown.field.checked_sub(KEYBOARD_FIELD)?)?;
            let s = self.surface.scale.value();
            let rows = dropdown.total.clamp(1, LIST_ROWS);
            let height = rows.checked_mul(ROW * s)?;
            let anchor = chooser.rect();
            let below = anchor.y + i64::from(anchor.height);
            let footer = self.footer.rect().y;
            let y = if below + height as i64 <= footer {
                below
            } else {
                anchor.y - height as i64
            };
            let rect = Rect {
                y,
                height: u32::try_from(height).ok()?,
                ..anchor
            };
            let list = ListBox::new(self.surface, rect)?;
            Some((dropdown, list))
        });
        self
    }

    /// What lies under the point: a row of the open list, by its index in
    /// the whole list, or a field's row.
    pub fn hit(&self, x: i64, y: i64) -> Option<Hit> {
        if let Some((dropdown, list)) = &self.dropdown {
            if let Some(row) = list.hit(x, y) {
                let index = dropdown.first.checked_add(row)?;
                return (index < dropdown.total).then_some(Hit::Item(index));
            }
            // What the list covers is the list's, never a field under it.
            if list.rect().contains(x, y) {
                return Some(Hit::List);
            }
        }
        let rows = self
            .entries
            .iter()
            .map(|entry| entry.rect())
            .chain(self.choices.iter().map(|choice| choice.rect()));
        rows.enumerate()
            .find(|(_, rect)| rect.contains(x, y))
            .map(|(field, _)| Hit::Field(field))
    }

    /// Shows `hint` in place of an empty time zone's placeholder.
    pub fn with_hint(mut self, hint: Option<&'a str>) -> Self {
        self.hint = hint;
        self
    }

    /// Shows `notice` on its own row under the fields, cut to the width.
    pub fn with_notice(mut self, notice: Option<&'a str>) -> Self {
        self.notice = notice;
        self
    }

    /// The currently focused field, if any.
    pub fn focused(&self) -> Option<usize> {
        self.focused
    }
}

impl Composition for SettingsPage<'_> {
    fn surface(&self) -> Surface {
        self.surface
    }

    fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        let bounds = self.surface.bounds();
        if let Some(clip) = bounds.intersection(damage) {
            sink(Draw {
                clip,
                primitive: Primitive::Fill {
                    rect: bounds,
                    color: CHROME,
                },
            });
        }
        let scale = self.surface.scale;
        let inset = (INSET * scale.value()) as i64;
        let heading = Rect {
            x: inset,
            y: (ROW * scale.value()) as i64,
            width: self.surface.width.saturating_sub(2 * INSET * scale.value()) as u32,
            height: (CELL_HEIGHT * scale.value()) as u32,
        };
        for (row, text) in [
            (1, "Account and regional settings"),
            (2, "No password or PIN is used for this installation."),
        ] {
            let rect = Rect {
                y: (row * ROW * scale.value()) as i64,
                ..heading
            };
            text_run(
                scale,
                text.chars(),
                (inset, rect.y),
                rect,
                GlyphStyle::medium(INK, CHROME),
                damage,
                sink,
            );
        }
        for (index, row) in LABEL_ROWS.iter().enumerate() {
            let Some(label) = LABELS.get(index) else {
                continue;
            };
            let rect = Rect {
                y: (row * ROW * scale.value()) as i64,
                ..heading
            };
            text_run(
                scale,
                label.chars(),
                (inset, rect.y),
                rect,
                GlyphStyle::medium(INK, CHROME),
                damage,
                sink,
            );
            let Some(value) = self.values.get(index) else {
                continue;
            };
            let Some(placeholder) = PLACEHOLDERS.get(index) else {
                continue;
            };
            let placeholder = match self.hint {
                Some(hint) if index == TIME_ZONE => hint,
                _ => placeholder,
            };
            if index < self.entries.len() {
                let Some(entry) = self.entries.get(index) else {
                    continue;
                };
                let Some(caret) = self.carets.get(index) else {
                    continue;
                };
                let focused = self.focused == Some(index);
                entry.emit(
                    Field {
                        text: value,
                        placeholder,
                        caret: *caret,
                        anchor: None,
                        first: entry.reveal(value.len(), *caret, 0),
                        masked: false,
                        focused,
                        caret_visible: focused && self.caret_visible,
                    },
                    damage,
                    sink,
                );
            } else {
                let Some(choice) = self.choices.get(index - self.entries.len()) else {
                    continue;
                };
                choice.emit(
                    [Choice {
                        label: if value.is_empty() { placeholder } else { value },
                        meta: OPENS,
                    }],
                    0,
                    (self.focused == Some(index)).then_some(0),
                    1,
                    damage,
                    sink,
                );
            }
        }
        if let Some(notice) = self.notice {
            let rect = Rect {
                y: (NOTICE_ROW * ROW * scale.value()) as i64,
                ..heading
            };
            let columns = (heading.width as usize) / (CELL_WIDTH * scale.value());
            text_run(
                scale,
                notice.chars().take(columns),
                (inset, rect.y),
                rect,
                GlyphStyle::medium(INK, CHROME),
                damage,
                sink,
            );
        }
        self.footer.emit(FOOTER.chars(), damage, sink);
        // The open list over the form, last.
        if let Some((dropdown, list)) = &self.dropdown {
            list.emit(
                dropdown
                    .shown
                    .iter()
                    .map(|label| Choice { label, meta: "" }),
                dropdown.first,
                dropdown.chosen,
                dropdown.total,
                damage,
                sink,
            );
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use td_ui::raster::Scale;

    fn surface(width: usize, height: usize) -> Surface {
        Surface::new(width, height, Scale::new(1).unwrap()).unwrap()
    }

    fn typed(draft: &mut Draft, chords: &[&str]) {
        for chord in chords {
            draft.key(chord);
        }
    }

    #[test]
    fn account_drafts_take_printable_characters_within_their_limits() {
        let mut draft = Draft::default();
        assert_eq!(draft.focused(), 0);
        typed(
            &mut draft,
            // As td-ui names them: a shifted character is itself, and
            // only a modified space is `Space`.
            &["a", "A", "_", " ", "Space", "C-a", "M-b", "\u{e9}", "Tab"],
        );
        assert_eq!(draft.values()[0], "aA_");
        assert_eq!(draft.focused(), 1);
        typed(
            &mut draft,
            &["t", "d", "Left", "Left", "x", "End", "y", "Home", "Delete"],
        );
        assert_eq!(draft.values()[1], "tdy");
        assert!(!draft.key("Backspace"), "nothing before the caret");
        typed(&mut draft, &["Right", "Backspace", "End", "Delete"]);
        assert_eq!(draft.values()[1], "dy");
        // Each field stops at its token limit.
        for (field, limit) in [(0, 32), (1, 63)] {
            let mut draft = Draft {
                focused: field,
                ..Draft::default()
            };
            for _ in 0..limit + 5 {
                draft.key("z");
            }
            assert_eq!(draft.values().get(field).unwrap().len(), limit);
            assert!(!draft.key("z"));
        }
    }

    #[test]
    fn focus_moves_between_the_four_fields() {
        let mut draft = Draft::default();
        typed(&mut draft, &["S-Tab"]);
        assert_eq!(draft.focused(), 3);
        typed(&mut draft, &["Tab"]);
        assert_eq!(draft.focused(), 0);
        typed(&mut draft, &["Up", "Down", "Return", "Down"]);
        assert_eq!(draft.focused(), 3);
        // The time zone row keeps Up, Down and Return for its own.
        typed(&mut draft, &["Down", "Return", "Up"]);
        assert_eq!(draft.focused(), 3);
        // The keyboard is fixed.
        draft.focused = 2;
        assert!(!draft.key("x"));
        assert_eq!(draft.values()[2], KEYBOARD);
    }

    fn catalog() -> Vec<String> {
        let mut zones: Vec<String> = (0..40).map(|n| format!("Area/Zone{n:02}")).collect();
        zones.extend(["America/New_York", "Etc/UTC", "Europe/London"].map(String::from));
        zones.sort();
        zones
    }

    #[test]
    fn the_time_zone_is_chosen_from_the_offered_catalog() {
        let mut draft = Draft {
            focused: 3,
            ..Draft::default()
        };
        assert!(!draft.offered());
        assert!(!draft.key("Down"), "nothing to choose from");
        assert_eq!(draft.values()[3], "");
        draft.offer(catalog());
        assert!(draft.offered());
        assert_eq!(draft.values()[3], "Etc/UTC");
        typed(&mut draft, &["Home"]);
        assert_eq!(draft.values()[3], "America/New_York");
        typed(&mut draft, &["Up", "Down"]);
        assert_eq!(draft.values()[3], "Area/Zone00");
        typed(&mut draft, &["PageDown"]);
        assert_eq!(draft.values()[3], "Area/Zone16");
        typed(&mut draft, &["PageUp", "PageUp"]);
        assert_eq!(draft.values()[3], "America/New_York");
        typed(&mut draft, &["End", "Down"]);
        assert_eq!(draft.values()[3], "Europe/London");
        // Typing seeks, ignoring case; a character matching nothing is
        // not taken, and Backspace takes back the last that was.
        typed(&mut draft, &["e", "T", "q"]);
        assert_eq!(draft.values()[3], "Etc/UTC");
        assert_eq!(draft.seek, "eT");
        typed(&mut draft, &["Backspace", "u"]);
        assert_eq!(draft.values()[3], "Europe/London");
        // Moving or leaving starts the seek over.
        typed(&mut draft, &["Up", "a"]);
        assert_eq!(draft.values()[3], "America/New_York");
        // A new catalog keeps the choice it still has, else UTC, else none.
        draft.offer(catalog());
        assert_eq!(draft.values()[3], "America/New_York");
        draft.offer(vec!["Etc/UTC".into(), "Europe/London".into()]);
        assert_eq!(draft.values()[3], "Etc/UTC");
        draft.offer(vec!["Europe/London".into()]);
        assert_eq!(draft.values()[3], "");
        // An empty entry is never taken for the empty choice.
        draft.offer(vec![String::new(), "Europe/London".into()]);
        assert_eq!(draft.zone, None);
        draft.offer(vec!["Europe/London".into()]);
        typed(&mut draft, &["Down"]);
        assert_eq!(draft.values()[3], "Europe/London");
    }

    #[test]
    fn the_draft_page_shows_the_hint_until_a_zone_is_chosen() {
        let surface = surface(crate::MIN_PAGE_WIDTH, crate::MIN_PAGE_HEIGHT);
        let glyphs = |draft: &Draft, hint| {
            let mut glyphs = String::new();
            draft
                .page(surface, hint, None)
                .unwrap()
                .emit(surface.bounds(), &mut |draw| {
                    if let Primitive::Glyph { scalar, .. } = draw.primitive {
                        glyphs.push(scalar);
                    }
                });
            glyphs
        };
        let mut draft = Draft::default();
        assert_eq!(draft.missing(), Some("Enter a username before review."));
        let shown = glyphs(&draft, Some("the time zones could not be read"));
        assert!(shown.contains("the time zones could not be read"));
        assert!(!shown.contains("Select a time zone"));
        draft.offer(catalog());
        let shown = glyphs(&draft, Some("the time zones could not be read"));
        assert!(shown.contains("Etc/UTC"));
        assert!(!shown.contains("could not be read"));
        typed(&mut draft, &["a", "Tab"]);
        assert_eq!(draft.missing(), Some("Enter a hostname before review."));
        typed(&mut draft, &["h"]);
        assert_eq!(draft.missing(), None);
        draft.offer(vec!["Europe/London".into()]);
        assert_eq!(draft.missing(), Some("Choose a time zone before review."));
    }

    #[test]
    fn a_notice_is_shown_under_the_fields_within_the_width() {
        let surface = surface(crate::MIN_PAGE_WIDTH, crate::MIN_PAGE_HEIGHT);
        let long = "the review was refused ".repeat(10);
        let draft = Draft::default();
        let page = draft.page(surface, None, Some(&long)).unwrap();
        let mut row = Vec::new();
        page.emit(surface.bounds(), &mut |draw| {
            if let Primitive::Glyph { y, scalar, .. } = draw.primitive {
                if y == (NOTICE_ROW * ROW) as i64 {
                    row.push(scalar);
                }
            }
        });
        let shown: String = row.into_iter().collect();
        assert!(long.starts_with(&shown) && shown.starts_with("the review was refused"));
        assert!(shown.len() < long.len());
    }

    #[test]
    fn minimum_size_and_draft_bounds_are_enforced() {
        let values = ["alice", "tdhost", "us", "America/Los_Angeles"];
        let carets = [5, 6];
        assert!(SettingsPage::new(surface(752, 480), values, Some(2), carets, true).is_some());
        assert!(SettingsPage::new(surface(751, 480), values, Some(2), carets, true).is_none());
        assert!(SettingsPage::new(surface(752, 479), values, Some(2), carets, true).is_none());
        assert!(SettingsPage::new(surface(800, 480), values, Some(4), carets, true).is_none());
        assert!(SettingsPage::new(surface(800, 480), values, Some(2), [6, 6], true).is_none());
        assert!(
            SettingsPage::new(surface(800, 480), ["a b", "", "", ""], None, [0; 2], false)
                .is_none()
        );
        assert!(
            SettingsPage::new(surface(800, 480), ["\n", "", "", ""], None, [0; 2], false).is_none()
        );
        assert!(
            SettingsPage::new(surface(800, 480), ["é", "", "", ""], None, [0; 2], false).is_none()
        );
        for (index, limit) in LIMITS.iter().enumerate() {
            let accepted = "a".repeat(*limit);
            let rejected = "a".repeat(*limit + 1);
            let mut values = [""; 4];
            *values.get_mut(index).unwrap() = &accepted;
            assert!(SettingsPage::new(surface(800, 480), values, None, [0; 2], false).is_some());
            *values.get_mut(index).unwrap() = &rejected;
            assert!(SettingsPage::new(surface(800, 480), values, None, [0; 2], false).is_none());
        }
    }

    /// Twelve zones, more than an open list shows.
    fn many() -> Vec<String> {
        (0..12).map(|index| format!("Zone/{index:02}")).collect()
    }

    #[test]
    fn a_chooser_list_opens_moves_chooses_and_closes_unchanged() {
        let mut draft = Draft::default();
        draft.offer(many());
        assert_eq!(draft.values()[3], "");
        // Space types nothing in a text field and opens nothing there.
        assert!(!draft.key(" "));
        assert!(!draft.is_open());
        typed(&mut draft, &["Tab", "Tab", "Tab"]);
        assert_eq!(draft.focused(), TIME_ZONE);
        assert!(draft.key(" "));
        assert!(draft.is_open());
        let open = draft.dropdown().unwrap();
        assert_eq!((open.field, open.total, open.first), (TIME_ZONE, 12, 0));
        assert_eq!(open.shown.len(), LIST_ROWS);
        // Keys move the choice and the shown rows follow it; Escape puts
        // back the zone it opened on, and leaves nothing else.
        typed(&mut draft, &["End"]);
        assert_eq!(draft.values()[3], "Zone/11");
        assert_eq!(draft.dropdown().unwrap().first, 12 - LIST_ROWS);
        assert!(draft.key("Escape"));
        assert!(!draft.is_open());
        assert_eq!(draft.values()[3], "");
        assert_eq!(draft.focused(), TIME_ZONE);
        // M-Down opens it too; Return keeps the choice.
        typed(&mut draft, &["M-Down", "Down", "Down", "Return"]);
        assert!(!draft.is_open());
        assert_eq!(draft.values()[3], "Zone/01");
        // Typing seeks while open; Tab keeps the choice and moves on.
        typed(&mut draft, &["Space"]);
        assert!(!draft.is_open(), "only an unmodified space opens");
        typed(&mut draft, &[" ", "Z", "o", "n", "e", "/", "0", "7", "Tab"]);
        assert!(!draft.is_open());
        assert_eq!(draft.values()[3], "Zone/07");
        assert_eq!(draft.focused(), 0);
        // The keyboard layout's list holds its one layout.
        typed(&mut draft, &["Tab", "Tab", " "]);
        assert_eq!(draft.focused(), KEYBOARD_FIELD);
        let keyboard = draft.dropdown().unwrap();
        assert_eq!(keyboard.shown, vec![KEYBOARD]);
        assert_eq!(keyboard.chosen, Some(0));
        typed(&mut draft, &["Down", "Return"]);
        assert_eq!(draft.values()[2], KEYBOARD);
        // No catalog, no list.
        let mut empty = Draft::default();
        typed(&mut empty, &["Tab", "Tab", "Tab", " "]);
        assert!(!empty.is_open());
    }

    #[test]
    fn the_wheel_scrolls_an_open_list_without_choosing() {
        let mut draft = Draft::default();
        draft.offer(many());
        assert!(!draft.scroll(3), "a closed list does not scroll");
        typed(&mut draft, &["Tab", "Tab", "Tab", " "]);
        let zone = draft.values()[3].to_string();
        assert!(draft.scroll(3));
        assert_eq!(draft.dropdown().unwrap().first, 3);
        assert!(draft.scroll(100));
        assert_eq!(draft.dropdown().unwrap().first, 12 - LIST_ROWS);
        assert!(!draft.scroll(1));
        assert!(draft.scroll(-100));
        assert_eq!(draft.dropdown().unwrap().first, 0);
        assert_eq!(draft.values()[3], zone);
    }

    #[test]
    fn a_click_focuses_fields_and_works_the_open_list() {
        let surface = surface(crate::MIN_PAGE_WIDTH, crate::MIN_PAGE_HEIGHT);
        let mut draft = Draft::default();
        draft.offer(many());
        let hit =
            |draft: &Draft, x: i64, y: i64| draft.page(surface, None, None).unwrap().hit(x, y);
        let row = |label_row: usize| ((label_row + 1) * ROW + 5) as i64;
        // Fields by their rows; the labels and the gap are none.
        for (field, label_row) in LABEL_ROWS.iter().enumerate() {
            assert_eq!(hit(&draft, 100, row(*label_row)), Some(Hit::Field(field)));
            assert_eq!(hit(&draft, 100, (label_row * ROW + 5) as i64), None);
        }
        // A click on the time zone's row focuses it and opens its list,
        // over the row: below it the footer leaves no room.
        assert!(draft.click(hit(&draft, 100, row(16))));
        assert_eq!(draft.focused(), TIME_ZONE);
        assert!(draft.is_open());
        let page = draft.page(surface, None, None).unwrap();
        let (_, list) = page.dropdown.as_ref().unwrap();
        assert_eq!(
            list.rect().y + i64::from(list.rect().height),
            row(16) - 5,
            "the list ends where the row begins"
        );
        let top = list.rect().y;
        // Its rows are items by their index in the whole list.
        assert_eq!(hit(&draft, 100, top + 5), Some(Hit::Item(0)));
        assert_eq!(
            hit(&draft, 100, top + 3 * ROW as i64 + 5),
            Some(Hit::Item(3))
        );
        draft.scroll(2);
        assert_eq!(
            hit(&draft, 100, top + 3 * ROW as i64 + 5),
            Some(Hit::Item(5))
        );
        // Choosing one closes the list with it.
        assert!(draft.click(Some(Hit::Item(5))));
        assert!(!draft.is_open());
        assert_eq!(draft.values()[3], "Zone/05");
        // A click elsewhere closes an open list unchanged, and one on
        // another field moves the focus there.
        draft.click(Some(Hit::Field(TIME_ZONE)));
        draft.key("Down");
        assert_eq!(draft.values()[3], "Zone/06");
        assert!(draft.click(None));
        assert!(!draft.is_open());
        assert_eq!(draft.values()[3], "Zone/05");
        draft.click(Some(Hit::Field(TIME_ZONE)));
        draft.key("Down");
        assert!(draft.click(Some(Hit::Field(0))));
        assert_eq!((draft.focused(), draft.is_open()), (0, false));
        assert_eq!(draft.values()[3], "Zone/05");
        // The open chooser's own row closes it, keeping the choice.
        draft.click(Some(Hit::Field(TIME_ZONE)));
        draft.key("Down");
        assert!(draft.click(Some(Hit::Field(TIME_ZONE))));
        assert!(!draft.is_open());
        assert_eq!(draft.values()[3], "Zone/06");
        // The keyboard layout's list opens under its row.
        draft.click(Some(Hit::Field(KEYBOARD_FIELD)));
        let page = draft.page(surface, None, None).unwrap();
        let (_, list) = page.dropdown.as_ref().unwrap();
        assert_eq!(list.rect().y, row(12) - 5 + ROW as i64);
    }

    #[test]
    fn the_open_list_s_scrollbar_is_the_list_s_and_a_click_off_a_closed_one_is_nothing() {
        let surface = surface(800, 600);
        let mut draft = Draft::default();
        draft.offer(many());
        typed(&mut draft, &["Tab", "Tab", "Tab", " ", "Down"]);
        let zone = draft.values()[3].to_string();
        let page = draft.page(surface, None, None).unwrap();
        let rect = page.dropdown.as_ref().unwrap().1.rect();
        // The gutter over the hostname's and the keyboard's rows.
        let gutter = rect.x + i64::from(rect.width) - 4;
        for y in [
            rect.y + 2,
            rect.y + 4 * ROW as i64 + 5,
            rect.y + i64::from(rect.height) - 1,
        ] {
            assert_eq!(page.hit(gutter, y), Some(Hit::List));
        }
        assert!(!draft.click(Some(Hit::List)));
        assert!(draft.is_open());
        assert_eq!(draft.values()[3], zone);
        // Closed, a click off the form leaves a seek and all else be.
        draft.key("Return");
        typed(&mut draft, &["Z"]);
        assert!(!draft.click(None));
        assert!(!draft.click(Some(Hit::List)));
        assert!(!draft.click(Some(Hit::Item(0))));
        assert_eq!(draft.values()[3], zone);
    }

    #[test]
    fn form_shows_all_choices_and_no_secret_field() {
        let surface = surface(crate::MIN_PAGE_WIDTH, crate::MIN_PAGE_HEIGHT);
        let page = SettingsPage::new(
            surface,
            ["alice", "tdhost", "us", "UTC"],
            Some(0),
            [5, 6],
            true,
        )
        .unwrap();
        assert_eq!(page.focused(), Some(0));
        let mut glyphs = String::new();
        page.emit(surface.bounds(), &mut |draw| {
            if let Primitive::Glyph { scalar, .. } = draw.primitive {
                glyphs.push(scalar);
            }
        });
        for value in [
            "Username",
            "Hostname",
            "Keyboard layout",
            "Time zone",
            "alice",
            "tdhost",
            "us",
            "UTC",
            "No password or PIN",
        ] {
            assert!(glyphs.contains(value), "missing {value}");
        }
        assert!(!glyphs.contains('\u{2022}'));
    }

    #[test]
    fn empty_choices_show_placeholders_and_only_a_live_focus_draws_a_caret() {
        let surface = surface(crate::MIN_PAGE_WIDTH, crate::MIN_PAGE_HEIGHT);
        let draws = |focused, caret_visible| {
            let page = SettingsPage::new(surface, [""; 4], focused, [0; 2], caret_visible).unwrap();
            let mut glyphs = String::new();
            let mut carets = 0;
            page.emit(surface.bounds(), &mut |draw| match draw.primitive {
                Primitive::Glyph { scalar, .. } => glyphs.push(scalar),
                Primitive::Fill { rect, color } if rect.width == 1 && color == INK => carets += 1,
                _ => {}
            });
            (glyphs, carets)
        };
        let (glyphs, carets) = draws(Some(0), true);
        for placeholder in PLACEHOLDERS {
            assert!(glyphs.contains(placeholder));
        }
        assert_eq!(carets, 1);
        assert_eq!(draws(Some(0), false).1, 0);
        assert_eq!(draws(None, true).1, 0);
    }
}

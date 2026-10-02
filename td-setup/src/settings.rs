//! The account and regional settings form: its drafts, edited by keys,
//! and the page that renders them. Drafts are bounded tokens; wire
//! admission, policy checks, and catalog membership remain the service's.

use td_ui::chrome::{Field, Item, List, Status, TextEntry, ROW};
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
const FOOTER: &str = "Account and region \u{b7} step 3 of 6 \u{b7} Enter on Time zone to review";
/// The row under the last field that says what became of a review.
const NOTICE_ROW: usize = 18;
/// The one keyboard layout the installer offers.
pub const KEYBOARD: &str = "us";
/// The time zone chosen when the catalog has it and nothing else was.
const DEFAULT_ZONE: &str = "Etc/UTC";
/// The rows Page Up and Page Down move a time zone selection.
const ZONE_PAGE: usize = 16;
/// The time zone field, the last.
pub const TIME_ZONE: usize = 3;

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

    /// Applies one key chord, as td-ui names it; a key the focused field
    /// does not take changes nothing. True when the draft changed.
    pub fn key(&mut self, chord: &str) -> bool {
        // The catalog is not edited by keys, so it is left out.
        let before = (
            self.texts.clone(),
            self.carets,
            self.focused,
            self.zone,
            self.seek.clone(),
        );
        let fields = LABELS.len();
        match chord {
            "Tab" => self.focus((self.focused + 1) % fields),
            "S-Tab" => self.focus((self.focused + fields - 1) % fields),
            _ if self.focused == TIME_ZONE => self.choose(chord),
            "Down" | "Return" => self.focus(self.focused + 1),
            "Up" => self.focus(self.focused.saturating_sub(1)),
            _ => self.type_key(chord),
        }
        before
            != (
                self.texts.clone(),
                self.carets,
                self.focused,
                self.zone,
                self.seek.clone(),
            )
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
        .map(|page| page.with_hint(hint).with_notice(notice))
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
    choices: [List; 2],
    footer: Status,
    hint: Option<&'a str>,
    notice: Option<&'a str>,
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
            List::new(
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
        })
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
                    [Item {
                        label: if value.is_empty() { placeholder } else { value },
                        meta: "Choose",
                        enabled: true,
                        marked: false,
                    }],
                    0,
                    if self.focused == Some(index) {
                        0
                    } else {
                        usize::MAX
                    },
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

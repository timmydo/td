//! The account and regional settings form. It renders bounded draft tokens;
//! wire admission, policy checks, and catalog membership remain separate.

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
const FOOTER: &str = "Account and region \u{b7} step 3 of 6";

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
        surface.check().ok()?;
        let scale = surface.scale.value();
        if surface.width < 800 * scale || surface.height < 480 * scale {
            return None;
        }
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
        if choices.last()?.rect().y + i64::from(choices.last()?.rect().height) > footer.rect().y {
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
        })
    }

    /// The currently focused field, if any, for the future turn loop.
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

    #[test]
    fn minimum_size_and_draft_bounds_are_enforced() {
        let values = ["alice", "tdhost", "us", "America/Los_Angeles"];
        let carets = [5, 6];
        assert!(SettingsPage::new(surface(800, 480), values, Some(2), carets, true).is_some());
        assert!(SettingsPage::new(surface(799, 480), values, Some(2), carets, true).is_none());
        assert!(SettingsPage::new(surface(800, 479), values, Some(2), carets, true).is_none());
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
        let surface = surface(800, 480);
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
        let surface = surface(800, 480);
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

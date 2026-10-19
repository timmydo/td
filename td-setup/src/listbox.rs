//! A listbox over td-ui's `List` geometry: a `PAPER` ground inside a
//! `BORDER` bezel, as the form's text fields have, so its rows read as
//! choices to click rather than page text, and the chosen row in the
//! `SELECTED` colour with paper ink, as a focused button shows. The
//! caller owns the rows, the selection and the first shown row; the box
//! holds no state.

use td_ui::chrome::List;
use td_ui::raster::{
    text_run, Draw, GlyphStyle, Primitive, Rect, Scale, Surface, BORDER, INK, LINE_NUMBER, PAPER,
    SELECTED,
};
use td_ui::CELL_WIDTH;

/// The text inset inside a row, in font pixels: a cell in, four down.
const INSET: (i64, i64) = (8, 4);

/// One row: its label and a right-aligned column.
#[derive(Clone, Copy, Debug)]
pub struct Choice<'a> {
    pub label: &'a str,
    pub meta: &'a str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ListBox {
    list: List,
    scale: Scale,
}

impl ListBox {
    /// The box filling `rect`, under `List::new`'s rules.
    pub fn new(surface: Surface, rect: Rect) -> Option<Self> {
        Some(Self {
            list: List::new(surface, rect)?,
            scale: surface.scale,
        })
    }

    pub fn rect(self) -> Rect {
        self.list.rect()
    }

    /// The rows it shows at once.
    pub fn rows(self) -> usize {
        self.list.rows()
    }

    /// The row area, less the scrollbar's gutter.
    pub fn body(self) -> Rect {
        self.list.body()
    }

    /// The shown row under the point, counted from the first shown.
    pub fn hit(self, x: i64, y: i64) -> Option<usize> {
        self.list.hit(x, y)
    }

    /// `first` moved as little as possible so `selected` is shown.
    pub fn reveal(self, total: usize, selected: usize, first: usize) -> usize {
        self.list.reveal(total, selected, first)
    }

    /// Paints the rows `choices` gives from `first`, `selected` (an index
    /// in all `total`) highlighted, and a scrollbar thumb only when there
    /// are more rows than it shows.
    pub fn emit<'a>(
        &self,
        choices: impl IntoIterator<Item = Choice<'a>>,
        first: usize,
        selected: Option<usize>,
        total: usize,
        damage: Rect,
        sink: &mut dyn FnMut(Draw),
    ) {
        let s = self.scale.value() as i64;
        let outer = self.list.rect();
        let face = Rect {
            x: outer.x + s,
            y: outer.y + s,
            width: outer.width.saturating_sub(2 * s as u32),
            height: outer.height.saturating_sub(2 * s as u32),
        };
        // The bezel is four bands round the face, as td-ui's chrome paints
        // it, so damage inside the face streams no border.
        let bottom = face.y + i64::from(face.height);
        let right = face.x + i64::from(face.width);
        for band in [
            Rect {
                height: (face.y - outer.y) as u32,
                ..outer
            },
            Rect {
                y: bottom,
                height: (outer.y + i64::from(outer.height) - bottom).max(0) as u32,
                ..outer
            },
            Rect {
                y: face.y,
                width: (face.x - outer.x) as u32,
                height: face.height,
                ..outer
            },
            Rect {
                x: right,
                y: face.y,
                width: (outer.x + i64::from(outer.width) - right).max(0) as u32,
                height: face.height,
            },
        ] {
            fill(band, BORDER, damage, sink);
        }
        fill(face, PAPER, damage, sink);
        let Some(damage) = damage.intersection(face) else {
            return;
        };
        let bar = self.list.scrollbar(total, first);
        for (index, choice) in choices.into_iter().enumerate() {
            // The text keeps td-ui's row geometry; the face clips only the
            // fill and the glyphs, so every row's text sits alike.
            let Some(full) = self.list.row(index) else {
                break;
            };
            let Some(row) = full.intersection(face) else {
                continue;
            };
            let chosen = selected.and_then(|at| at.checked_sub(first)) == Some(index);
            let (ground, ink) = if chosen {
                (SELECTED, PAPER)
            } else {
                (PAPER, INK)
            };
            if chosen {
                // With nothing to scroll the gutter is the rows' own, so
                // the chosen row's colour runs on to the bezel.
                let band = if bar.enabled() {
                    row
                } else {
                    Rect {
                        width: (face.x + i64::from(face.width) - row.x).max(0) as u32,
                        ..row
                    }
                };
                fill(band, ground, damage, sink);
            }
            let style = GlyphStyle::medium(ink, ground);
            let cells = choice.meta.chars().count() as i64;
            let meta_x = full.x + i64::from(full.width) - (INSET.0 + cells * CELL_WIDTH as i64) * s;
            let text_y = full.y + INSET.1 * s;
            let label_bounds = Rect {
                width: (meta_x - row.x).max(0) as u32,
                ..row
            };
            text_run(
                self.scale,
                choice.label.chars(),
                (full.x + INSET.0 * s, text_y),
                label_bounds,
                style,
                damage,
                sink,
            );
            text_run(
                self.scale,
                choice.meta.chars(),
                (meta_x, text_y),
                row,
                style,
                damage,
                sink,
            );
        }
        if bar.enabled() {
            fill(bar.thumb, LINE_NUMBER, damage, sink);
        }
    }
}

fn fill(rect: Rect, color: u32, damage: Rect, sink: &mut dyn FnMut(Draw)) {
    if let Some(area) = rect.intersection(damage) {
        sink(Draw {
            clip: area,
            primitive: Primitive::Fill { rect: area, color },
        });
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn surface(scale: u8) -> Surface {
        let s = usize::from(scale);
        Surface::new(800 * s, 600 * s, Scale::new(scale).unwrap()).unwrap()
    }

    fn listbox(scale: u8) -> ListBox {
        let s = i64::from(scale);
        ListBox::new(
            surface(scale),
            Rect {
                x: 8 * s,
                y: 96 * s,
                width: (784 * s) as u32,
                height: (72 * s) as u32,
            },
        )
        .unwrap()
    }

    /// The colour each draw leaves at a point, last draw winning.
    fn at(draws: &[Draw], x: i64, y: i64) -> Option<u32> {
        draws.iter().rev().find_map(|draw| match draw.primitive {
            Primitive::Fill { rect, color } if rect.contains(x, y) => Some(color),
            _ => None,
        })
    }

    fn draws(
        listbox: ListBox,
        choices: &[Choice],
        first: usize,
        selected: Option<usize>,
        total: usize,
    ) -> Vec<Draw> {
        let mut out = Vec::new();
        listbox.emit(
            choices.iter().copied(),
            first,
            selected,
            total,
            Rect {
                x: 0,
                y: 0,
                width: 3200,
                height: 2400,
            },
            &mut |draw| out.push(draw),
        );
        out
    }

    #[test]
    fn rows_sit_on_paper_inside_a_border_and_the_chosen_one_is_selected() {
        for scale in 1..=4u8 {
            let s = i64::from(scale);
            let listbox = listbox(scale);
            let rows = [
                Choice {
                    label: "vda",
                    meta: "17.2 GB",
                },
                Choice {
                    label: "vdb",
                    meta: "1.0 TB",
                },
            ];
            let painted = draws(listbox, &rows, 0, Some(1), 2);
            let rect = listbox.list.rect();
            assert_eq!(at(&painted, rect.x, rect.y), Some(BORDER));
            let row = listbox.list.row(0).unwrap();
            assert_eq!(at(&painted, row.x + 4 * s, row.y + 4 * s), Some(PAPER));
            let chosen = listbox.list.row(1).unwrap();
            assert_eq!(
                at(&painted, chosen.x + 4 * s, chosen.y + 4 * s),
                Some(SELECTED)
            );
            // Three rows shown and two items: no thumb, the gutter paper.
            let gutter = rect.x + i64::from(rect.width) - 8 * s;
            assert_eq!(at(&painted, gutter, rect.y + 8 * s), Some(PAPER));
            let text: String = painted
                .iter()
                .filter_map(|draw| match draw.primitive {
                    Primitive::Glyph { scalar, style, .. } => Some((scalar, style.ink == PAPER)),
                    _ => None,
                })
                .map(|(scalar, light)| {
                    if light {
                        scalar.to_ascii_uppercase()
                    } else {
                        scalar
                    }
                })
                .collect();
            assert_eq!(text, "vda17.2 GBVDB1.0 TB");
        }
    }

    #[test]
    fn every_row_places_its_text_as_the_shared_list_does() {
        // The visible glyphs by row: (x, y, scalar), spaces left out.
        fn glyphs(draws: &[Draw]) -> Vec<(i64, i64, char)> {
            draws
                .iter()
                .filter_map(|draw| match draw.primitive {
                    Primitive::Glyph { x, y, scalar, .. } if scalar != ' ' => Some((x, y, scalar)),
                    _ => None,
                })
                .collect()
        }
        for scale in 1..=4u8 {
            let s = i64::from(scale);
            let listbox = listbox(scale);
            let labels = ["vda", "vdb", "vdc"];
            let ours = glyphs(&draws(
                listbox,
                &labels.map(|label| Choice { label, meta: "9GB" }),
                0,
                Some(1),
                3,
            ));
            let mut shared = Vec::new();
            listbox.list.emit(
                labels.map(|label| td_ui::chrome::Item {
                    label,
                    meta: "9GB",
                    enabled: true,
                    marked: false,
                }),
                0,
                1,
                3,
                Rect {
                    x: 0,
                    y: 0,
                    width: 3200,
                    height: 2400,
                },
                &mut |draw| shared.push(draw),
            );
            let shared = glyphs(&shared);
            // Each row: three label glyphs, then three of the column.
            assert_eq!((ours.len(), shared.len()), (18, 18));
            for (index, (mine, theirs)) in ours.iter().zip(&shared).enumerate() {
                let row = listbox.list.row(index / 6).unwrap();
                assert_eq!(mine.2, theirs.2);
                assert_eq!(mine.1, row.y + 4 * s, "scale {scale}: {mine:?}");
                assert_eq!(mine.1, theirs.1);
                // The shared list marks a label with two cells first; the
                // column sits where it does.
                let shift = if index % 6 < 3 { 2 * 8 * s } else { 0 };
                assert_eq!(mine.0, theirs.0 - shift, "scale {scale}: {mine:?}");
            }
            let first_row = listbox.list.row(0).unwrap();
            assert_eq!(ours[0].0, first_row.x + 8 * s);
        }
    }

    #[test]
    fn a_scrolled_box_shows_a_thumb_and_hits_shown_rows_only() {
        let listbox = listbox(1);
        let rows = [Choice {
            label: "a",
            meta: "",
        }; 3];
        let painted = draws(listbox, &rows, 2, Some(2), 9);
        let thumb = listbox.list.scrollbar(9, 2).thumb;
        assert_eq!(at(&painted, thumb.x, thumb.y), Some(LINE_NUMBER));
        // The selection is first-relative: index 2 shown from 2 is row 0.
        let row = listbox.list.row(0).unwrap();
        assert_eq!(at(&painted, row.x + 4, row.y + 4), Some(SELECTED));
        assert_eq!(listbox.hit(row.x + 4, row.y + 4), Some(0));
        assert_eq!(listbox.hit(row.x + 4, row.y + 2 * 24 + 4), Some(2));
        assert_eq!(listbox.hit(row.x + 4, row.y + 3 * 24 + 4), None);
        assert_eq!(listbox.hit(thumb.x, thumb.y), None);
    }

    #[test]
    fn damage_off_the_box_paints_nothing_and_damage_inside_is_clipped() {
        let listbox = listbox(1);
        let rows = [Choice {
            label: "vda",
            meta: "1 GB",
        }];
        let mut out = Vec::new();
        let off = Rect {
            x: 0,
            y: 0,
            width: 800,
            height: 90,
        };
        listbox.emit(rows, 0, Some(0), 1, off, &mut |draw| out.push(draw));
        assert!(out.is_empty());
        let inside = Rect {
            x: 100,
            y: 100,
            width: 10,
            height: 10,
        };
        listbox.emit(rows, 0, Some(0), 1, inside, &mut |draw| out.push(draw));
        assert!(!out.is_empty());
        for draw in out {
            assert!(inside.intersection(draw.clip) == Some(draw.clip));
        }
    }
}

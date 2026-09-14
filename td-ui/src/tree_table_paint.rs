//! Visible-cell-only semantic painting for the tree table.
use crate::raster::{
    self, Draw, GlyphStyle, Primitive, Rect, BORDER, CHROME, INACTIVE_SELECTION, INK, PAPER,
    SELECTED,
};
use crate::tree_table::{Controller, Focus};
use crate::tree_table_geometry::{CellRect, INDENT};
use crate::tree_table_model::Cell;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Direction {
    Ascending,
    Descending,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Sort {
    pub column: usize,
    pub direction: Direction,
}
fn fill(rect: Rect, color: u32, damage: Rect, sink: &mut dyn FnMut(Draw)) {
    if let Some(clip) = rect.intersection(damage) {
        sink(Draw {
            clip,
            primitive: Primitive::Fill { rect, color },
        });
    }
}
impl<I: Copy + Ord> Controller<I> {
    fn text(
        &self,
        text: &str,
        cell: CellRect,
        origin: i64,
        style: GlyphStyle,
        damage: Rect,
        sink: &mut dyn FnMut(Draw),
    ) {
        let scale = self.surface().scale;
        let cw = (crate::CELL_WIDTH * scale.value()) as i64;
        let skip = (cell.clip.x.saturating_sub(origin) / cw).max(0) as usize;
        let x = origin.saturating_add(skip as i64 * cw);
        let y = cell.rect.y + 4 * scale.value() as i64;
        raster::text_run(
            scale,
            text.chars().skip(skip),
            (x, y),
            cell.clip,
            style,
            damage,
            sink,
        );
    }
    /// Visit only intersecting visible cells; the consumer retains cell values.
    /// A sort indicator describes the consumer's ordering and never sorts rows.
    pub fn emit<'a>(
        &self,
        sort: Option<Sort>,
        damage: Rect,
        values: &mut impl FnMut(I, usize) -> Cell<'a>,
        sink: &mut dyn FnMut(Draw),
    ) {
        let Some(g) = self.geometry() else {
            return;
        };
        let Some(damage) = damage.intersection(g.rect()) else {
            return;
        };
        let scale = self.surface().scale.value() as u32;
        let inset = (crate::CELL_WIDTH as u32 * scale) as i64;
        fill(g.rect(), PAPER, damage, sink);
        fill(
            Rect {
                width: g.rect().width,
                ..g.header()
            },
            CHROME,
            damage,
            sink,
        );
        if let Some(bar) = g.horizontal() {
            fill(
                Rect {
                    x: g.vertical().track.x,
                    width: g.vertical().track.width,
                    ..bar.track
                },
                CHROME,
                damage,
                sink,
            );
        }
        for (column, heading) in self.model().columns().iter().enumerate() {
            let Some(cell) = g.header_cell(column) else {
                continue;
            };
            if cell.clip.intersection(damage).is_none() {
                continue;
            }
            let focused = self.focus() == Focus::Header(column);
            let background = if focused { SELECTED } else { CHROME };
            if focused {
                fill(cell.clip, background, damage, sink);
            }
            let style = GlyphStyle::medium(if focused { PAPER } else { INK }, background);
            self.text(
                heading.title(),
                cell,
                cell.rect.x + inset,
                style,
                damage,
                sink,
            );
            if let Some(sort) = sort.filter(|sort| sort.column == column) {
                let mark = if sort.direction == Direction::Ascending {
                    "^"
                } else {
                    "v"
                };
                self.text(
                    mark,
                    cell,
                    cell.rect.x + i64::from(cell.rect.width) - inset - i64::from(scale),
                    style,
                    damage,
                    sink,
                );
            }
            let right = Rect {
                x: cell.rect.x + i64::from(cell.rect.width - scale),
                width: scale,
                ..cell.rect
            };
            if let Some(right) = right.intersection(cell.clip) {
                fill(right, BORDER, damage, sink);
            }
        }
        fill(
            Rect {
                y: g.header().y + i64::from(g.header().height - scale),
                width: g.rect().width,
                height: scale,
                ..g.header()
            },
            BORDER,
            damage,
            sink,
        );
        let end = g
            .first()
            .saturating_add(g.visible())
            .min(self.model().rows().len());
        for index in g.first()..end {
            let Some(row) = self.model().rows().get(index) else {
                continue;
            };
            let Some(band) = g.row(index) else {
                continue;
            };
            if band.intersection(damage).is_none() {
                continue;
            }
            let selected = self.selected() == Some(row.id);
            let background = if selected {
                if self.focus() == Focus::Rows {
                    SELECTED
                } else {
                    INACTIVE_SELECTION
                }
            } else {
                PAPER
            };
            let ink = if selected && self.focus() == Focus::Rows {
                PAPER
            } else {
                INK
            };
            if selected {
                fill(band, background, damage, sink);
            }
            let style = GlyphStyle::medium(ink, background);
            for (column, heading) in self.model().columns().iter().enumerate() {
                let Some(cell) = g.cell(index, column) else {
                    continue;
                };
                if cell.clip.intersection(damage).is_none() {
                    continue;
                }
                let text = values(row.id, column).text();
                let indent = if column == 0 {
                    i64::from((u32::from(row.depth) + 1) * INDENT * scale)
                } else {
                    0
                };
                let origin = if heading.numeric() {
                    (cell.rect.x + i64::from(cell.rect.width)
                        - inset
                        - text.chars().count() as i64 * inset)
                        .max(cell.rect.x + indent + inset)
                } else {
                    cell.rect.x + indent + inset
                };
                self.text(text, cell, origin, style, damage, sink);
                if column == 0 && row.children {
                    if let Some(disclosure) = g.disclosure(index, row.depth) {
                        let full = Rect {
                            x: cell.rect.x + i64::from(u32::from(row.depth) * INDENT * scale),
                            width: INDENT * scale,
                            ..cell.rect
                        };
                        self.text(
                            if row.expanded { "v" } else { ">" },
                            CellRect {
                                rect: full,
                                clip: disclosure,
                            },
                            full.x + 4 * i64::from(scale),
                            style,
                            damage,
                            sink,
                        );
                    }
                }
            }
        }
        for bar in std::iter::once(g.vertical()).chain(g.horizontal()) {
            fill(bar.track, CHROME, damage, sink);
            fill(
                bar.thumb,
                if bar.enabled() { BORDER } else { CHROME },
                damage,
                sink,
            );
        }
    }
}

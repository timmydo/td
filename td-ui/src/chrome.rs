//! The chrome bands td-owned windows share, over `raster`: the menu bar
//! with its panel, a wrapped text block, a tab strip and a status row.
//! Each is a geometry over a `Surface` in the reference renderer's units
//! (24-pixel rows of 8x16 cells, scaled by the surface) and a painter
//! that streams the complete band inside a damage rectangle. Nothing
//! here reads a clock, a file or the environment.

use crate::raster::{
    text_run, Draw, GlyphStyle, Primitive, Rect, Scale, Surface, BORDER, CHROME, INK, PAPER,
};
use crate::{CELL_HEIGHT, CELL_WIDTH};

/// A chrome row in font pixels: the bar's, a tab's, a panel row's and the
/// status row's.
pub const ROW: usize = 24;
/// A panel's width in font pixels.
pub const PANEL_WIDTH: usize = 320;
/// The rows a panel may have.
pub const PANEL_ROWS: usize = 13;
/// A tab's width in font pixels, and its close mark's.
pub const TAB_WIDTH: usize = 160;
pub const CLOSE_WIDTH: usize = 24;
/// The cells a status line shows at most.
pub const STATUS_COLUMNS: usize = 512;
/// The rows a text block may reserve, and the scalars its painter visits
/// at most.
pub const BLOCK_ROWS: usize = 15;
pub const BLOCK_SCALARS: usize = BLOCK_ROWS * 73;
/// A panel's selected-row background and its disabled ink.
pub const SELECTED_ROW: u32 = 0xffc9c1b2;
pub const DISABLED: u32 = 0xff827a6d;

/// The text inset inside a chrome row: one cell in, four pixels down.
const INSET: (i64, i64) = (8, 4);

fn fill(rect: Rect, color: u32, damage: Rect, sink: &mut dyn FnMut(Draw)) {
    if let Some(area) = rect.intersection(damage) {
        sink(Draw {
            clip: area,
            primitive: Primitive::Fill { rect: area, color },
        });
    }
}

/// The menu bar: `labels` on the first row from cell column one, three
/// cells between them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Bar<'a> {
    labels: &'a [&'a str],
    surface: Surface,
}

impl<'a> Bar<'a> {
    pub fn new(surface: Surface, labels: &'a [&'a str]) -> Self {
        Self { labels, surface }
    }

    fn scale(self) -> usize {
        self.surface.scale.value()
    }

    /// The bar's row.
    pub fn rect(self) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: self.surface.width as u32,
            height: (ROW * self.scale()) as u32,
        }
    }

    /// The header of label `index`: its cells and the three after it,
    /// none after the last.
    pub fn header(self, index: usize) -> Option<Rect> {
        let s = self.scale();
        let last = self.labels.len().checked_sub(1)?;
        let mut column = 1;
        for (i, label) in self.labels.iter().enumerate() {
            let columns = label.chars().count() + if i == last { 0 } else { 3 };
            if i == index {
                return Some(Rect {
                    x: (column * CELL_WIDTH * s) as i64,
                    y: 0,
                    width: (columns * CELL_WIDTH * s) as u32,
                    height: (ROW * s) as u32,
                });
            }
            column += columns;
        }
        None
    }

    /// The label whose header holds the point.
    pub fn hit(self, x: i64, y: i64) -> Option<usize> {
        (0..self.labels.len()).find(|&index| self.header(index).is_some_and(|r| r.contains(x, y)))
    }

    /// Paints the row: chrome, then the labels joined by three spaces.
    pub fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        let Some(damage) = damage.intersection(self.surface.bounds()) else {
            return;
        };
        let s = self.scale() as i64;
        let rect = self.rect();
        fill(rect, CHROME, damage, sink);
        let last = self.labels.len().saturating_sub(1);
        let chars = self.labels.iter().enumerate().flat_map(|(i, label)| {
            label
                .chars()
                .chain(if i == last { "" } else { "   " }.chars())
        });
        text_run(
            self.surface.scale,
            chars,
            (INSET.0 * s, INSET.1 * s),
            rect,
            GlyphStyle::medium(INK, CHROME),
            damage,
            sink,
        );
    }
}

/// One panel row as painted: its label, the shortcut shown at its right,
/// whether it can be chosen and whether it carries the check mark.
#[derive(Clone, Copy, Debug)]
pub struct Row<'a> {
    pub label: &'a str,
    pub shortcut: &'a str,
    pub enabled: bool,
    pub checked: bool,
}

/// A menu panel under its header: `PANEL_WIDTH` wide, one `ROW` per
/// entry, complete inside the surface with a status row below it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Panel {
    rect: Rect,
    scale: Scale,
}

impl Panel {
    /// The panel of `rows` under `header`, its left edge clamped to the
    /// surface's right edge; `None` when there are no rows or too many,
    /// or the surface cannot hold the panel whole above a status row.
    pub fn new(surface: Surface, header: Rect, rows: usize) -> Option<Self> {
        if rows == 0 || rows > PANEL_ROWS {
            return None;
        }
        let s = surface.scale.value();
        let width = PANEL_WIDTH * s;
        let height = rows * ROW * s;
        let y = header.y.checked_add(i64::from(header.height))?;
        let bottom = y
            .checked_add(height as i64)?
            .checked_add((ROW * s) as i64)?;
        if y < 0 || surface.width < width || bottom > surface.height as i64 {
            return None;
        }
        Some(Self {
            rect: Rect {
                x: header.x.min((surface.width - width) as i64).max(0),
                y,
                width: width as u32,
                height: height as u32,
            },
            scale: surface.scale,
        })
    }

    pub fn rect(self) -> Rect {
        self.rect
    }

    pub fn rows(self) -> usize {
        self.rect.height as usize / (ROW * self.scale.value())
    }

    /// The row holding the point.
    pub fn hit(self, x: i64, y: i64) -> Option<usize> {
        self.rect
            .contains(x, y)
            .then(|| (y - self.rect.y) as usize / (ROW * self.scale.value()))
    }

    /// Row `index`'s rectangle.
    pub fn row(self, index: usize) -> Option<Rect> {
        if index >= self.rows() {
            return None;
        }
        let height = (ROW * self.scale.value()) as i64;
        Some(Rect {
            y: self.rect.y + index as i64 * height,
            height: height as u32,
            ..self.rect
        })
    }

    /// Paints the rows in order, the selected one highlighted, a disabled
    /// one dim, a checked one prefixed with a plus, each shortcut at the
    /// row's right edge; rows beyond the panel's are not painted.
    pub fn emit<'a>(
        &self,
        rows: impl IntoIterator<Item = Row<'a>>,
        selected: usize,
        damage: Rect,
        sink: &mut dyn FnMut(Draw),
    ) {
        let s = self.scale.value() as i64;
        for (index, row) in rows.into_iter().enumerate() {
            let Some(rect) = self.row(index) else {
                break;
            };
            let background = if index == selected {
                SELECTED_ROW
            } else {
                CHROME
            };
            let ink = if row.enabled { INK } else { DISABLED };
            let style = GlyphStyle::medium(ink, background);
            fill(rect, background, damage, sink);
            let prefix = if row.checked { "+ " } else { "  " };
            text_run(
                self.scale,
                prefix.chars().chain(row.label.chars()),
                (rect.x + INSET.0 * s, rect.y + INSET.1 * s),
                rect,
                style,
                damage,
                sink,
            );
            let cells = row.shortcut.chars().count() as i64;
            let start = rect.x + i64::from(rect.width) - (INSET.0 + cells * CELL_WIDTH as i64) * s;
            text_run(
                self.scale,
                row.shortcut.chars(),
                (start, rect.y + INSET.1 * s),
                rect,
                style,
                damage,
                sink,
            );
        }
    }
}

/// The next row from `selected` among `count`, forward or backward with
/// wrap, skipping rows `enabled` refuses; `selected` again after a full
/// turn finds none.
pub fn step(
    count: usize,
    selected: usize,
    backward: bool,
    enabled: impl Fn(usize) -> bool,
) -> usize {
    let mut next = selected;
    for _ in 0..count {
        next = if backward {
            (next + count - 1) % count
        } else {
            (next + 1) % count
        };
        if enabled(next) {
            break;
        }
    }
    next
}

/// A tab strip on one row: `count` tabs `TAB_WIDTH` wide from the left,
/// the active one always in the strip; a surface narrower than one tab
/// clips that tab.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Strip {
    surface: Surface,
    y: i64,
    active: usize,
    count: usize,
}

impl Strip {
    /// The strip always fills its row; `None` only when there are tabs but
    /// `active` is not one of them.
    pub fn new(surface: Surface, y: i64, active: usize, count: usize) -> Option<Self> {
        (count == 0 || active < count).then_some(Self {
            surface,
            y,
            active,
            count,
        })
    }

    fn scale(self) -> usize {
        self.surface.scale.value()
    }

    /// The strip's row.
    pub fn rect(self) -> Rect {
        Rect {
            x: 0,
            y: self.y,
            width: self.surface.width as u32,
            height: (ROW * self.scale()) as u32,
        }
    }

    /// Tab `index`'s rectangle, `None` when it is not a tab or is scrolled
    /// out of the strip to keep the active one in.
    pub fn tab(self, index: usize) -> Option<Rect> {
        if index >= self.count {
            return None;
        }
        let s = self.scale();
        let width = TAB_WIDTH * s;
        let visible = (self.surface.width / width).max(1);
        let first = self.active.saturating_sub(visible - 1);
        let slot = index.checked_sub(first)?;
        if slot >= visible {
            return None;
        }
        Some(Rect {
            x: (slot * width) as i64,
            y: self.y,
            width: width as u32,
            height: (ROW * s) as u32,
        })
    }

    /// Tab `index`'s close mark, its rightmost `CLOSE_WIDTH`.
    pub fn close(self, index: usize) -> Option<Rect> {
        let tab = self.tab(index)?;
        let width = (CLOSE_WIDTH * self.scale()) as u32;
        Some(Rect {
            x: tab.x + i64::from(tab.width - width),
            width,
            ..tab
        })
    }

    /// Paints the row: chrome, then each visible tab in order from
    /// `tabs` (its title and whether it is dirty): paper for the active
    /// tab and chrome otherwise, a top and a right border, the title
    /// prefixed with a star when dirty and clipped short of the close
    /// mark, and the mark.
    pub fn emit<'a>(
        &self,
        tabs: impl IntoIterator<Item = (&'a str, bool)>,
        damage: Rect,
        sink: &mut dyn FnMut(Draw),
    ) {
        let Some(damage) = damage.intersection(self.surface.bounds()) else {
            return;
        };
        let s = self.scale() as i64;
        fill(self.rect(), CHROME, damage, sink);
        for (index, (title, dirty)) in tabs.into_iter().enumerate() {
            let (Some(rect), Some(close)) = (self.tab(index), self.close(index)) else {
                continue;
            };
            let background = if index == self.active { PAPER } else { CHROME };
            let style = GlyphStyle::medium(INK, background);
            fill(rect, background, damage, sink);
            fill(
                Rect {
                    height: s as u32,
                    ..rect
                },
                BORDER,
                damage,
                sink,
            );
            fill(
                Rect {
                    x: rect.x + i64::from(rect.width) - s,
                    width: s as u32,
                    ..rect
                },
                BORDER,
                damage,
                sink,
            );
            let title_rect = Rect {
                width: rect.width.saturating_sub(close.width),
                ..rect
            };
            text_run(
                self.surface.scale,
                if dirty { "*" } else { "" }.chars().chain(title.chars()),
                (rect.x + INSET.0 * s, rect.y + INSET.1 * s),
                title_rect,
                style,
                damage,
                sink,
            );
            text_run(
                self.surface.scale,
                "x".chars(),
                (close.x + INSET.0 * s, close.y + INSET.1 * s),
                close,
                style,
                damage,
                sink,
            );
        }
    }
}

/// A wrapped text block: `rows` cell rows across the surface at `y`, its
/// text wrapped at the block's columns.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Block {
    surface: Surface,
    y: i64,
    rows: usize,
}

impl Block {
    /// `None` beyond `BLOCK_ROWS`.
    pub fn new(surface: Surface, y: i64, rows: usize) -> Option<Self> {
        (rows <= BLOCK_ROWS).then_some(Self { surface, y, rows })
    }

    fn scale(self) -> usize {
        self.surface.scale.value()
    }

    pub fn rows(self) -> usize {
        self.rows
    }

    /// The block's area: full width, its rows tall.
    pub fn rect(self) -> Rect {
        Rect {
            x: 0,
            y: self.y,
            width: self.surface.width as u32,
            height: (self.rows * CELL_HEIGHT * self.scale()) as u32,
        }
    }

    /// The cells a row holds: the width less a cell each side, at least
    /// one.
    pub fn columns(self) -> usize {
        let s = self.scale();
        (self.surface.width.saturating_sub(2 * CELL_WIDTH * s) / (CELL_WIDTH * s)).max(1)
    }

    /// Paints the block: chrome, then `text` from its top-left cell,
    /// wrapping at the columns, a newline starting the next row, at most
    /// `BLOCK_SCALARS` scalars visited and the rows that fit painted.
    pub fn emit(&self, text: &str, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        let Some(damage) = damage.intersection(self.surface.bounds()) else {
            return;
        };
        let s = self.scale();
        let rect = self.rect();
        fill(rect, CHROME, damage, sink);
        let Some(clip) = rect.intersection(damage) else {
            return;
        };
        let columns = self.columns();
        let style = GlyphStyle::medium(INK, CHROME);
        let (mut column, mut row) = (0, 0);
        for scalar in text.chars().take(BLOCK_SCALARS) {
            if scalar == '\n' || column == columns {
                row += 1;
                column = 0;
            }
            if row >= self.rows {
                break;
            }
            if scalar == '\n' {
                continue;
            }
            sink(Draw {
                clip,
                primitive: Primitive::Glyph {
                    x: ((INSET.0 as usize + column * CELL_WIDTH) * s) as i64,
                    y: rect.y + (row * CELL_HEIGHT * s) as i64,
                    scalar,
                    style,
                },
            });
            column += 1;
        }
    }
}

/// The status row: the surface's bottom `ROW`, one line of text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Status {
    surface: Surface,
}

impl Status {
    pub fn new(surface: Surface) -> Self {
        Self { surface }
    }

    fn scale(self) -> usize {
        self.surface.scale.value()
    }

    /// The bottom row, `ROW` scaled tall; a shorter surface clips it on
    /// emit, not here.
    pub fn rect(self) -> Rect {
        let height = ROW * self.scale();
        Rect {
            x: 0,
            y: self.surface.height.saturating_sub(height) as i64,
            width: self.surface.width as u32,
            height: height as u32,
        }
    }

    /// The cells a line shows: the width less a cell each side, at most
    /// `STATUS_COLUMNS`.
    pub fn columns(self) -> usize {
        let s = self.scale();
        (self.surface.width.saturating_sub(2 * CELL_WIDTH * s) / (CELL_WIDTH * s))
            .min(STATUS_COLUMNS)
    }

    /// Fills the row chrome under a one-pixel top border, without a line;
    /// a consumer that lays out its own status text frames it with this.
    pub fn frame(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        let Some(damage) = damage.intersection(self.surface.bounds()) else {
            return;
        };
        let s = self.scale() as i64;
        let rect = self.rect();
        fill(rect, CHROME, damage, sink);
        fill(
            Rect {
                height: s as u32,
                ..rect
            },
            BORDER,
            damage,
            sink,
        );
    }

    /// Paints the row: its `frame`, then `line` in whole cells from the
    /// first, a control scalar blank, and the last cell an ellipsis when
    /// the line is longer than the row shows.
    pub fn emit(&self, line: impl Iterator<Item = char>, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        self.frame(damage, sink);
        let Some(damage) = damage.intersection(self.surface.bounds()) else {
            return;
        };
        let s = self.scale() as i64;
        let rect = self.rect();
        let columns = self.columns();
        let mut chars = line.peekable();
        let shown = (0..columns).map_while(|column| {
            let scalar = chars.next()?;
            Some(if column + 1 == columns && chars.peek().is_some() {
                '…'
            } else if scalar.is_control() {
                ' '
            } else {
                scalar
            })
        });
        text_run(
            self.surface.scale,
            shown,
            (INSET.0 * s, rect.y + INSET.1 * s),
            rect,
            GlyphStyle::medium(INK, CHROME),
            damage,
            sink,
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn surface(width: usize, height: usize, scale: u8) -> Surface {
        Surface::new(width, height, Scale::new(scale).unwrap()).unwrap()
    }

    #[test]
    fn a_panel_needs_two_rows_of_headroom_and_clamps_to_the_right_edge() {
        for scale in 1..=4u8 {
            let s = scale as usize;
            let header = Bar::new(surface(320 * s, 600, scale), &["A", "B"])
                .header(1)
                .unwrap();
            for rows in [1, 7, 13] {
                let height = (rows + 2) * ROW * s;
                assert!(Panel::new(surface(320 * s, height - 1, scale), header, rows).is_none());
                assert!(Panel::new(surface(320 * s - 1, height, scale), header, rows).is_none());
                let panel = Panel::new(surface(320 * s, height, scale), header, rows).unwrap();
                assert_eq!(panel.rows(), rows);
                assert_eq!(panel.rect().x, 0, "clamped");
                assert_eq!(panel.rect().y, (ROW * s) as i64);
            }
            let ample = surface(320 * s, 600, scale);
            assert!(Panel::new(ample, header, 0).is_none());
            assert!(Panel::new(ample, header, PANEL_ROWS + 1).is_none());
        }
    }

    #[test]
    fn stepping_skips_disabled_rows_and_wraps() {
        let enabled = |i: usize| i != 1 && i != 3;
        assert_eq!(step(4, 0, false, enabled), 2);
        assert_eq!(step(4, 2, false, enabled), 0);
        assert_eq!(step(4, 0, true, enabled), 2);
        assert_eq!(step(4, 0, false, |_| false), 0, "a full turn finds none");
        assert_eq!(step(0, 5, false, |_| true), 5, "no rows");
    }
}

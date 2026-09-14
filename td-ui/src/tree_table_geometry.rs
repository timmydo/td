//! Geometry shared by tree-table drawing and hits.
use crate::raster::{Rect, Scrollbar, Surface};
use crate::tree_table_model::{COLUMNS, COLUMN_WIDTH};
pub const ROW: u32 = 24;
pub const GUTTER: u32 = 16;
pub const INDENT: u32 = 16;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidSurface,
    InvalidRect,
    InvalidWidths,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidSurface => "invalid table surface",
            Self::InvalidRect => "table rectangle lies outside the surface",
            Self::InvalidWidths => "invalid table column widths",
        })
    }
}
impl std::error::Error for Error {}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CellRect {
    pub rect: Rect,
    pub clip: Rect,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Hit {
    Header(usize),
    Cell { row: usize, column: usize },
    Vertical,
    Horizontal,
}
#[derive(Clone, Copy, Debug)]
pub struct Geometry {
    surface: Surface,
    rect: Rect,
    header: Rect,
    body: Rect,
    widths: [u32; COLUMNS],
    columns: usize,
    total_width: usize,
    offset: usize,
    first: usize,
    rows: usize,
    visible: usize,
    vertical: Scrollbar,
    horizontal: Option<Scrollbar>,
}
impl Geometry {
    pub fn new(
        surface: Surface,
        rect: Rect,
        widths: &[u32],
        rows: usize,
        first: usize,
        offset: usize,
    ) -> Result<Option<Self>, Error> {
        surface.check().map_err(|_| Error::InvalidSurface)?;
        if rect.x < 0
            || rect.y < 0
            || i128::from(rect.x) + i128::from(rect.width) > surface.width as i128
            || i128::from(rect.y) + i128::from(rect.height) > surface.height as i128
        {
            return Err(Error::InvalidRect);
        }
        if widths.is_empty()
            || widths.len() > COLUMNS
            || widths
                .iter()
                .any(|width| *width == 0 || *width > COLUMN_WIDTH)
        {
            return Err(Error::InvalidWidths);
        }
        let scale = surface.scale.value() as u32;
        let gutter = GUTTER * scale;
        let row = ROW * scale;
        if rect.width < (32 + GUTTER) * scale {
            return Ok(None);
        }
        let total_width = widths
            .iter()
            .map(|width| (*width * scale) as usize)
            .sum::<usize>();
        let body_width = rect.width - gutter;
        let horizontal = total_width > body_width as usize;
        let footer = if horizontal { gutter } else { 0 };
        if rect.height < 2 * row + footer {
            return Ok(None);
        }
        let header = Rect {
            width: body_width,
            height: row,
            ..rect
        };
        let body = Rect {
            y: rect.y + i64::from(row),
            width: body_width,
            height: rect.height - row - footer,
            ..rect
        };
        let visible = (body.height / row) as usize;
        let first = first.min(rows.saturating_sub(visible));
        let offset = offset.min(total_width.saturating_sub(body_width as usize));
        let vertical = Scrollbar::new(
            Rect {
                x: rect.x + i64::from(body_width),
                width: gutter,
                ..body
            },
            visible,
            rows,
            first,
            surface.scale,
            false,
        );
        let horizontal = horizontal.then(|| {
            Scrollbar::new(
                Rect {
                    x: rect.x,
                    y: rect.y + i64::from(rect.height - footer),
                    width: body_width,
                    height: footer,
                },
                body_width as usize,
                total_width,
                offset,
                surface.scale,
                true,
            )
        });
        let mut captured = [0; COLUMNS];
        for (slot, width) in captured.iter_mut().zip(widths) {
            *slot = *width * scale;
        }
        Ok(Some(Self {
            surface,
            rect,
            header,
            body,
            widths: captured,
            columns: widths.len(),
            total_width,
            offset,
            first,
            rows,
            visible,
            vertical,
            horizontal,
        }))
    }
    pub fn rect(self) -> Rect {
        self.rect
    }
    pub fn header(self) -> Rect {
        self.header
    }
    pub fn body(self) -> Rect {
        self.body
    }
    pub fn visible(self) -> usize {
        self.visible
    }
    pub fn first(self) -> usize {
        self.first
    }
    pub fn offset(self) -> usize {
        self.offset
    }
    pub fn total_width(self) -> usize {
        self.total_width
    }
    pub fn vertical(self) -> Scrollbar {
        self.vertical
    }
    pub fn horizontal(self) -> Option<Scrollbar> {
        self.horizontal
    }
    pub fn row(self, index: usize) -> Option<Rect> {
        let slot = index.checked_sub(self.first)?;
        if index >= self.rows || slot >= self.visible {
            return None;
        }
        let height = ROW * self.surface.scale.value() as u32;
        Some(Rect {
            y: self.body.y + slot as i64 * i64::from(height),
            height,
            ..self.body
        })
    }
    fn column(self, index: usize, band: Rect) -> Option<CellRect> {
        if index >= self.columns {
            return None;
        }
        let leading = self
            .widths
            .iter()
            .take(index)
            .map(|width| u64::from(*width))
            .sum::<u64>();
        let rect = Rect {
            x: self.rect.x + leading as i64 - self.offset as i64,
            width: *self.widths.get(index)?,
            ..band
        };
        Some(CellRect {
            rect,
            clip: rect.intersection(band)?,
        })
    }
    pub fn header_cell(self, column: usize) -> Option<CellRect> {
        self.column(column, self.header)
    }
    pub fn cell(self, row: usize, column: usize) -> Option<CellRect> {
        self.column(column, self.row(row)?)
    }
    pub fn disclosure(self, row: usize, depth: u16) -> Option<Rect> {
        let cell = self.cell(row, 0)?;
        let scale = self.surface.scale.value() as u32;
        let rect = Rect {
            x: cell.rect.x + i64::from(u32::from(depth) * INDENT * scale),
            width: INDENT * scale,
            ..cell.rect
        };
        rect.intersection(cell.clip)
    }
    pub fn hit(self, x: i64, y: i64) -> Option<Hit> {
        if !self.rect.contains(x, y) {
            return None;
        }
        if self.vertical.track.contains(x, y) {
            return Some(Hit::Vertical);
        }
        if self.horizontal.is_some_and(|bar| bar.track.contains(x, y)) {
            return Some(Hit::Horizontal);
        }
        if self.header.contains(x, y) {
            return (0..self.columns)
                .find(|column| {
                    self.header_cell(*column)
                        .is_some_and(|cell| cell.clip.contains(x, y))
                })
                .map(Hit::Header);
        }
        if !self.body.contains(x, y) {
            return None;
        }
        let row =
            self.first + (y - self.body.y) as usize / (ROW as usize * self.surface.scale.value());
        self.row(row)?;
        (0..self.columns)
            .find(|column| {
                self.cell(row, *column)
                    .is_some_and(|cell| cell.clip.contains(x, y))
            })
            .map(|column| Hit::Cell { row, column })
    }
}

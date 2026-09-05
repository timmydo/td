//! Allocation-free visual rows and the shared glyph/caret position map.

use crate::{text, Error, Result};
use std::ops::Range;

/// Unscaled bitmap-cell geometry shared with the future raster/pointer adapter.
pub const CELL_WIDTH: usize = 8;
/// Row height in unscaled font pixels; viewport geometry itself is in cells.
pub const CELL_HEIGHT: usize = 16;
pub const MAX_COLUMNS: usize = 1024;
pub const MAX_ROWS: usize = 512;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Break {
    Soft,
    Newline,
    End,
}

/// At a soft break one byte boundary has two visual positions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Affinity {
    Upstream,
    Downstream,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Caret {
    pub byte: usize,
    pub affinity: Affinity,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Position {
    pub row: usize,
    pub column: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Cell {
    pub bytes: Range<usize>,
    pub scalar: char,
    pub column: usize,
    pub width: usize,
}

#[derive(Clone, Debug)]
pub struct Row<'a> {
    text: &'a str,
    start: usize,
    ending: Break,
}

impl Row<'_> {
    pub fn bytes(&self) -> Range<usize> {
        self.start..self.start + self.text.len()
    }

    pub fn ending(&self) -> Break {
        self.ending
    }

    /// Drawing and selection use these same intervals as hit testing.
    pub fn cells(&self) -> impl Iterator<Item = Cell> + '_ {
        let mut column = 0;
        self.text.char_indices().map(move |(offset, scalar)| {
            let width = scalar_width(scalar, column);
            let cell = Cell {
                bytes: self.start + offset..self.start + offset + scalar.len_utf8(),
                scalar,
                column,
                width,
            };
            column += width;
            cell
        })
    }

    pub fn columns(&self) -> usize {
        self.cells().map(|cell| cell.width).sum()
    }

    /// Unscaled font-pixel x, relative to this row before horizontal scrolling.
    /// Nearest endpoint wins, with midpoint ties before the scalar.
    pub fn hit_test(&self, x: usize) -> Caret {
        for cell in self.cells() {
            let left = cell.column * CELL_WIDTH;
            let right = (cell.column + cell.width) * CELL_WIDTH;
            if x <= right {
                let byte = if x.saturating_sub(left) <= (right - left) / 2 {
                    cell.bytes.start
                } else {
                    cell.bytes.end
                };
                return self.caret(byte);
            }
        }
        self.caret(self.bytes().end)
    }

    fn caret(&self, byte: usize) -> Caret {
        Caret {
            byte,
            affinity: if self.ending == Break::Soft && byte == self.bytes().end {
                Affinity::Upstream
            } else {
                Affinity::Downstream
            },
        }
    }
}

fn scalar_width(scalar: char, column: usize) -> usize {
    if scalar == '\t' {
        text::TAB_WIDTH - column % text::TAB_WIDTH
    } else {
        1
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Layout<'a> {
    text: &'a str,
    columns: usize,
    soft_wrap: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Metrics {
    pub rows: usize,
    /// Longest row including the caret cell after its last glyph.
    pub columns: usize,
}

impl<'a> Layout<'a> {
    /// Validates normalized external text in O(bytes); do not rebuild per hit.
    pub fn new(text: &'a str, columns: usize, soft_wrap: bool) -> Result<Self> {
        let layout = Self::with_geometry(text, columns, soft_wrap)?;
        text::validate(text)?;
        Ok(layout)
    }

    /// The model already validated text. Borrowing prevents concurrent edits.
    pub fn for_document(
        document: &'a crate::model::Document,
        columns: usize,
        soft_wrap: bool,
    ) -> Result<Self> {
        Self::with_geometry(document.text(), columns, soft_wrap)
    }

    fn with_geometry(text: &'a str, columns: usize, soft_wrap: bool) -> Result<Self> {
        if columns == 0 || columns > MAX_COLUMNS {
            return Err(Error::InvalidArgument);
        }
        if text.len() > text::MAX_FILE_BYTES {
            return Err(Error::Limit);
        }
        Ok(Self {
            text,
            columns,
            soft_wrap,
        })
    }

    pub fn rows(&self) -> Rows<'a> {
        Rows {
            layout: *self,
            next: Some(0),
        }
    }

    /// O(bytes). Cache per revision, wrap mode and width, not per scroll event.
    pub fn metrics(&self) -> Metrics {
        self.rows().fold(
            Metrics {
                rows: 0,
                columns: 1,
            },
            |mut metrics, row| {
                metrics.rows += 1;
                metrics.columns = metrics.columns.max(row.columns() + 1);
                metrics
            },
        )
    }

    pub fn position(&self, caret: Caret) -> Result<Position> {
        if caret.byte > self.text.len() || !self.text.is_char_boundary(caret.byte) {
            return Err(Error::InvalidPosition);
        }
        for (index, row) in self.rows().enumerate() {
            let range = row.bytes();
            if caret.byte < range.end
                || (caret.byte == range.end
                    && (row.ending != Break::Soft || caret.affinity == Affinity::Upstream))
            {
                let column = row
                    .cells()
                    .take_while(|cell| cell.bytes.end <= caret.byte)
                    .map(|cell| cell.width)
                    .sum();
                return Ok(Position { row: index, column });
            }
        }
        Err(Error::InvalidPosition)
    }

    /// The caller retains desired_column across consecutive vertical moves.
    /// Delta may also be a page's signed visible-row count.
    pub fn vertical(&self, caret: Caret, delta: isize, desired_column: usize) -> Result<Caret> {
        let position = self.position(caret)?;
        let target = position.row.saturating_add_signed(delta);
        let mut last = None;
        for (index, row) in self.rows().enumerate() {
            if index == target {
                return Ok(row.hit_test(desired_column.saturating_mul(CELL_WIDTH)));
            }
            last = Some(row);
        }
        last.map(|row| row.hit_test(desired_column.saturating_mul(CELL_WIDTH)))
            .ok_or(Error::InvalidPosition)
    }
}

#[derive(Clone, Debug)]
pub struct Rows<'a> {
    layout: Layout<'a>,
    next: Option<usize>,
}

impl<'a> Iterator for Rows<'a> {
    type Item = Row<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let start = self.next.take()?;
        let remaining = self.layout.text.get(start..)?;
        let mut column = 0;
        let mut separator = None;
        let mut has_word = false;
        for (offset, scalar) in remaining.char_indices() {
            if scalar == '\n' {
                let row = Row {
                    text: remaining.get(..offset)?,
                    start,
                    ending: Break::Newline,
                };
                self.next = Some(start + offset + 1);
                return Some(row);
            }
            let width = scalar_width(scalar, column);
            if self.layout.soft_wrap && column + width > self.layout.columns && offset != 0 {
                let end = separator.unwrap_or(offset);
                let row = Row {
                    text: remaining.get(..end)?,
                    start,
                    ending: Break::Soft,
                };
                self.next = Some(start + end);
                return Some(row);
            }
            column += width;
            if scalar == ' ' || scalar == '\t' {
                if has_word {
                    separator = Some(offset + scalar.len_utf8());
                }
            } else {
                has_word = true;
            }
        }
        Some(Row {
            text: remaining,
            start,
            ending: Break::End,
        })
    }
}

impl std::iter::FusedIterator for Rows<'_> {}

/// Store one per tab. Geometry is in unscaled font cells, never pixels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Viewport {
    columns: usize,
    rows: usize,
    first_row: usize,
    left_column: usize,
}

impl Viewport {
    pub fn new(columns: usize, rows: usize) -> Result<Self> {
        if columns == 0 || columns > MAX_COLUMNS || rows == 0 || rows > MAX_ROWS {
            return Err(Error::InvalidArgument);
        }
        Ok(Self {
            columns,
            rows,
            first_row: 0,
            left_column: 0,
        })
    }

    pub fn origin(&self) -> Position {
        Position {
            row: self.first_row,
            column: self.left_column,
        }
    }

    pub fn dimensions(&self) -> (usize, usize) {
        (self.columns, self.rows)
    }

    /// Use this to keep the document wrap width equal to the viewport width.
    pub fn layout<'a>(
        &self,
        document: &'a crate::model::Document,
        soft_wrap: bool,
    ) -> Result<Layout<'a>> {
        Layout::for_document(document, self.columns, soft_wrap)
    }

    /// Top of the one-pixel caret in unscaled viewport pixels, if visible.
    /// Soft-wrap end positions clamp onto the last visible pixel column.
    pub fn caret_pixel(&self, position: Position, soft_wrap: bool) -> Option<(usize, usize)> {
        let row = position.row.checked_sub(self.first_row)?;
        if row >= self.rows {
            return None;
        }
        let x = if soft_wrap {
            position
                .column
                .saturating_mul(CELL_WIDTH)
                .min(self.columns * CELL_WIDTH - 1)
        } else {
            let column = position.column.checked_sub(self.left_column)?;
            if column >= self.columns {
                return None;
            }
            column * CELL_WIDTH
        };
        Some((x, row * CELL_HEIGHT))
    }

    /// Preserve scroll offsets on resize, then clamp against the new layout.
    /// max_columns includes the final caret cell on an unwrapped longest row.
    pub fn resize(
        &mut self,
        columns: usize,
        rows: usize,
        total_rows: usize,
        max_columns: usize,
        soft_wrap: bool,
    ) -> Result<()> {
        let dimensions = Self::new(columns, rows)?;
        self.columns = dimensions.columns;
        self.rows = dimensions.rows;
        self.scroll(0, total_rows);
        self.scroll_horizontal(0, max_columns, soft_wrap);
        Ok(())
    }

    /// max_columns includes the caret cell after the longest row's last glyph.
    pub fn scroll_horizontal(&mut self, delta: isize, max_columns: usize, soft_wrap: bool) {
        self.left_column = if soft_wrap {
            0
        } else {
            self.left_column
                .saturating_add_signed(delta)
                .min(max_columns.saturating_sub(self.columns))
        };
    }

    pub fn scroll(&mut self, delta: isize, total_rows: usize) {
        self.first_row = self
            .first_row
            .saturating_add_signed(delta)
            .min(total_rows.saturating_sub(self.rows));
    }

    /// The validated caret position and row count come from the same layout.
    pub fn reveal(&mut self, position: Position, total_rows: usize, soft_wrap: bool) {
        if position.row < self.first_row {
            self.first_row = position.row;
        } else if position.row.saturating_sub(self.first_row) >= self.rows {
            self.first_row = position.row.saturating_sub(self.rows - 1);
        }
        self.first_row = self.first_row.min(total_rows.saturating_sub(self.rows));
        if soft_wrap {
            self.left_column = 0;
        } else if position.column < self.left_column {
            self.left_column = position.column;
        } else if position.column.saturating_sub(self.left_column) >= self.columns {
            self.left_column = position.column.saturating_sub(self.columns - 1);
        }
    }
}

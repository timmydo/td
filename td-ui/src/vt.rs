//! The terminal model and its byte-stream parser: td-term's screen, and any
//! td-owned program's that embeds a terminal. Bytes, sizes and keys in;
//! cells, cursor, modes, history and replies out. Nothing here reads a
//! descriptor, clock or environment.

use std::collections::VecDeque;

/// The largest row or column count a grid may have.
pub const MAX_DIMENSION: usize = 16_384;
const MAX_SCREEN_CELLS: usize = 1_048_576;
const MAX_SCREEN_BYTES: usize = 16 * 1024 * 1024;
const MAX_HISTORY_CELLS: usize = 1_048_576;
const MAX_HISTORY_BYTES: usize = 16 * 1024 * 1024;
const MAX_HISTORY_LINES: usize = 16_384;
const HISTORY_ALLOCATOR_MARGIN: usize = 1024 * 1024;
const MAX_CSI_PARAMS: usize = 32;
const MAX_REPLY_BYTES: usize = 64 * 1024;
/// An OSC's payload past this is dropped whole.
const MAX_OSC: usize = 4096;
/// The OSC 8 links the model remembers; a cell whose link has been
/// forgotten is no link.
const MAX_LINKS: usize = 1024;
/// The longest OSC 8 URI kept.
const MAX_URI: usize = 2048;
/// The longest OSC 8 `id=` that joins links; a longer one is no id.
const MAX_LINK_ID: usize = 256;
/// The longest OSC 0 or 2 title kept, in characters: td's compositor keeps
/// no more, and at four bytes each its request stays far inside a Wayland
/// message, which a whole OSC payload would not.
const MAX_TITLE: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Color {
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

/// An underline's style, `SGR 4:n`'s `n`: `4` alone is `Single`, `24`
/// and `4:0` are `None`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Underline {
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

impl Underline {
    fn from_style(style: u16) -> Option<Self> {
        match style {
            0 => Some(Self::None),
            1 => Some(Self::Single),
            2 => Some(Self::Double),
            3 => Some(Self::Curly),
            4 => Some(Self::Dotted),
            5 => Some(Self::Dashed),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Attributes {
    pub bold: bool,
    pub faint: bool,
    pub italic: bool,
    pub underline: Underline,
    pub inverse: bool,
    pub strike: bool,
    pub foreground: Color,
    pub background: Color,
    /// `SGR 58`'s; `Default` draws the underline in the foreground.
    pub underline_color: Color,
    /// The OSC 8 link the cell was written in (`Terminal::link`), 0 for
    /// none. SGR does not touch it; an erase does.
    pub link: u32,
    /// The cell is the first written after an OSC 133;A, where a shell
    /// prompt starts (`Terminal::prompt`). Writing over it keeps it, as a
    /// line editor redraws its prompt; an erase drops it.
    pub prompt: bool,
}

impl Default for Attributes {
    fn default() -> Self {
        Self {
            bold: false,
            faint: false,
            italic: false,
            underline: Underline::None,
            inverse: false,
            strike: false,
            foreground: Color::Default,
            background: Color::Default,
            underline_color: Color::Default,
            link: 0,
            prompt: false,
        }
    }
}

impl Attributes {
    fn erased(self) -> Self {
        Self {
            foreground: self.foreground,
            background: self.background,
            ..Self::default()
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Cell {
    pub scalar: char,
    pub attributes: Attributes,
}

impl Cell {
    fn blank(attributes: Attributes) -> Self {
        Self {
            scalar: ' ',
            attributes: attributes.erased(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Charset {
    Ascii,
    DecSpecial,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SavedCursor {
    row: usize,
    column: usize,
    pending_wrap: bool,
}

impl SavedCursor {
    fn resized(self, row_offset: usize, rows: usize, old_columns: usize, columns: usize) -> Self {
        let (column, pending_wrap) = if self.pending_wrap && columns > old_columns {
            (old_columns.min(columns.saturating_sub(1)), false)
        } else {
            let column = self.column.min(columns.saturating_sub(1));
            (
                column,
                self.pending_wrap && columns == old_columns && column.saturating_add(1) == columns,
            )
        };
        Self {
            row: self
                .row
                .saturating_sub(row_offset)
                .min(rows.saturating_sub(1)),
            column,
            pending_wrap,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SavedState {
    cursor: SavedCursor,
    attributes: Attributes,
    origin_mode: bool,
    auto_wrap: bool,
    g0: Charset,
    g1: Charset,
    use_g1: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct HistoryLine {
    start: usize,
    length: usize,
    wrapped: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct History {
    lines: VecDeque<HistoryLine>,
    arena: Vec<Cell>,
    write: usize,
    cells: usize,
    max_cells: usize,
    max_lines: usize,
    /// Lines ever appended, which eviction does not decrease. An open
    /// viewport names the line it is looking at in this space, so output
    /// arriving underneath it does not drag the view along.
    pushed: u64,
    /// Which numbering `pushed` is counting in. Clearing starts a new one,
    /// so an anchor from the old one can be told apart from a line that
    /// happens to have been given the same number since.
    epoch: u64,
}

impl History {
    fn new(enabled: bool) -> Self {
        let lines = if enabled {
            VecDeque::with_capacity(MAX_HISTORY_LINES)
        } else {
            VecDeque::new()
        };
        let record_bytes = lines
            .capacity()
            .saturating_mul(std::mem::size_of::<HistoryLine>());
        let payload_budget = MAX_HISTORY_BYTES
            .saturating_sub(HISTORY_ALLOCATOR_MARGIN)
            .saturating_sub(record_bytes);
        let desired_cells = if enabled {
            MAX_HISTORY_CELLS.min(payload_budget / std::mem::size_of::<Cell>())
        } else {
            0
        };
        let mut arena = Vec::with_capacity(desired_cells);
        if arena.capacity().saturating_mul(std::mem::size_of::<Cell>()) > payload_budget {
            arena = Vec::new();
        }
        let max_cells = arena.capacity().min(MAX_HISTORY_CELLS);
        Self {
            lines,
            arena,
            write: 0,
            cells: 0,
            max_cells,
            max_lines: if enabled { MAX_HISTORY_LINES } else { 0 },
            pushed: 0,
            epoch: 0,
        }
    }

    fn prepare_line(&mut self, columns: usize) -> bool {
        if self.max_lines == 0 || columns > self.max_cells {
            return false;
        }
        while self.lines.len() >= self.max_lines
            || self.cells.saturating_add(columns) > self.max_cells
        {
            let Some(line) = self.lines.pop_front() else {
                return false;
            };
            self.cells = self.cells.saturating_sub(line.length);
        }
        true
    }

    fn push_line(&mut self, screen_cells: &[Cell], start: usize, columns: usize, wrapped: bool) {
        let Some(end) = start.checked_add(columns) else {
            return;
        };
        let Some(source_cells) = screen_cells.get(start..end) else {
            return;
        };
        if !self.prepare_line(columns) {
            return;
        }
        let line_start = self.write;
        // In runs up to the arena's end: appended while it grows, written
        // over from `write` once it is full, wrapping to its start.
        let mut rest = source_cells;
        while !rest.is_empty() {
            let room = self.max_cells.saturating_sub(self.write);
            let growing = self.arena.len() < self.max_cells;
            let run = if growing {
                room.min(self.max_cells.saturating_sub(self.arena.len()))
            } else {
                room
            }
            .min(rest.len());
            if run == 0 {
                break;
            }
            let (now, later) = rest.split_at(run);
            if growing {
                self.arena.extend_from_slice(now);
            } else if let Some(target) = self
                .arena
                .get_mut(self.write..self.write.saturating_add(run))
            {
                target.copy_from_slice(now);
            }
            self.write = self.write.saturating_add(run);
            if self.write >= self.max_cells {
                self.write = 0;
            }
            rest = later;
        }
        self.cells = self.cells.saturating_add(columns);
        self.lines.push_back(HistoryLine {
            start: line_start,
            length: columns,
            wrapped,
        });
        self.pushed = self.pushed.saturating_add(1);
    }

    fn clear(&mut self) {
        self.lines.clear();
        self.arena.clear();
        self.write = 0;
        self.cells = 0;
        // The counter goes with the lines it counted, and the numbering it
        // counted in is retired with it: zeroing `pushed` alone would let an
        // old anchor come back into range as new lines arrived, reopening a
        // view the clear had closed.
        self.pushed = 0;
        self.epoch = self.epoch.saturating_add(1);
    }

    #[cfg(test)]
    fn storage_bytes(&self) -> usize {
        self.arena
            .capacity()
            .saturating_mul(std::mem::size_of::<Cell>())
            .saturating_add(
                self.lines
                    .capacity()
                    .saturating_mul(std::mem::size_of::<HistoryLine>()),
            )
    }

    /// The newest line no longer goes on at the screen's first row, which
    /// was replaced or cleared without being pushed here.
    fn unwrap_newest(&mut self) {
        if let Some(line) = self.lines.back_mut() {
            line.wrapped = false;
        }
    }

    /// A line stored at another width than the screen's has no wrap at the
    /// view's edge: there is no reflow, so it is padded or clipped there.
    fn line_wrapped(&self, line: usize, columns: usize) -> bool {
        self.lines
            .get(line)
            .is_some_and(|line| line.wrapped && line.length == columns)
    }

    fn line_cell(&self, line: usize, column: usize) -> Option<Cell> {
        let line = self.lines.get(line)?;
        if column >= line.length || self.max_cells == 0 {
            return None;
        }
        let index = line.start.saturating_add(column) % self.max_cells;
        self.arena.get(index).copied()
    }
}

/// The grid is a ring of rows: `origin` is the stored row the screen's
/// first row is, so scrolling the whole screen moves `origin` and clears
/// the rows it uncovers rather than copying every cell up or down.
#[derive(Clone, Debug)]
struct Screen {
    rows: usize,
    columns: usize,
    cells: Vec<Cell>,
    origin: usize,
    cursor_row: usize,
    cursor_column: usize,
    pending_wrap: bool,
    scroll_top: usize,
    scroll_bottom: usize,
    tabs: Vec<bool>,
    history: History,
    ansi_saved: Option<SavedCursor>,
    wrapped_rows: Vec<bool>,
    damaged_rows: Vec<bool>,
}

/// Two screens are equal when they show the same rows, wherever the ring
/// has put them.
impl PartialEq for Screen {
    fn eq(&self, other: &Self) -> bool {
        // Named in full, so a new field cannot be left out of equality.
        let Self {
            rows,
            columns,
            cells: _,
            origin: _,
            cursor_row,
            cursor_column,
            pending_wrap,
            scroll_top,
            scroll_bottom,
            tabs,
            history,
            ansi_saved,
            wrapped_rows,
            damaged_rows,
        } = self;
        *rows == other.rows
            && *columns == other.columns
            && (0..*rows).all(|row| self.row(row) == other.row(row))
            && *cursor_row == other.cursor_row
            && *cursor_column == other.cursor_column
            && *pending_wrap == other.pending_wrap
            && *scroll_top == other.scroll_top
            && *scroll_bottom == other.scroll_bottom
            && *tabs == other.tabs
            && *history == other.history
            && *ansi_saved == other.ansi_saved
            && *wrapped_rows == other.wrapped_rows
            && *damaged_rows == other.damaged_rows
    }
}

impl Eq for Screen {}

fn checked_cell_count(rows: usize, columns: usize) -> Result<usize, String> {
    if rows == 0 || columns == 0 {
        return Err("terminal dimensions must be nonzero".into());
    }
    if rows > MAX_DIMENSION || columns > MAX_DIMENSION {
        return Err(format!(
            "terminal dimensions {rows}x{columns} exceed {MAX_DIMENSION}"
        ));
    }
    let cells = rows
        .checked_mul(columns)
        .ok_or_else(|| "terminal cell count overflow".to_string())?;
    let bytes = cells
        .checked_mul(std::mem::size_of::<Cell>())
        .ok_or_else(|| "terminal screen allocation overflow".to_string())?;
    if cells > MAX_SCREEN_CELLS || bytes > MAX_SCREEN_BYTES {
        return Err(format!(
            "terminal screen {rows}x{columns} exceeds the resource limit"
        ));
    }
    Ok(cells)
}

fn default_tabs(columns: usize) -> Vec<bool> {
    (0..columns)
        .map(|column| column != 0 && column % 8 == 0)
        .collect()
}

impl Screen {
    fn new(
        rows: usize,
        columns: usize,
        attributes: Attributes,
        history_enabled: bool,
    ) -> Result<Self, String> {
        let count = checked_cell_count(rows, columns)?;
        Ok(Self {
            rows,
            columns,
            cells: vec![Cell::blank(attributes); count],
            origin: 0,
            cursor_row: 0,
            cursor_column: 0,
            pending_wrap: false,
            scroll_top: 0,
            scroll_bottom: rows,
            tabs: default_tabs(columns),
            history: History::new(history_enabled),
            ansi_saved: None,
            wrapped_rows: vec![false; rows],
            damaged_rows: vec![true; rows],
        })
    }

    /// Where `row`'s cells start in `cells`.
    fn row_start(&self, row: usize) -> Option<usize> {
        if row >= self.rows {
            return None;
        }
        let stored = self.origin.checked_add(row)?.checked_rem(self.rows)?;
        stored.checked_mul(self.columns)
    }

    fn row(&self, row: usize) -> Option<&[Cell]> {
        let start = self.row_start(row)?;
        self.cells.get(start..start.checked_add(self.columns)?)
    }

    fn row_mut(&mut self, row: usize) -> Option<&mut [Cell]> {
        let start = self.row_start(row)?;
        self.cells.get_mut(start..start.checked_add(self.columns)?)
    }

    /// Copies row `from` over row `to`, whole.
    fn copy_row(&mut self, from: usize, to: usize) {
        let (Some(source), Some(target)) = (self.row_start(from), self.row_start(to)) else {
            return;
        };
        let Some(end) = source.checked_add(self.columns) else {
            return;
        };
        if end <= self.cells.len() && target.saturating_add(self.columns) <= self.cells.len() {
            self.cells.copy_within(source..end, target);
        }
    }

    fn blank_rows(&mut self, rows: std::ops::Range<usize>, attributes: Attributes) {
        for row in rows {
            if let Some(cells) = self.row_mut(row) {
                cells.fill(Cell::blank(attributes));
            }
        }
    }

    fn index(&self, row: usize, column: usize) -> Option<usize> {
        if column >= self.columns {
            return None;
        }
        self.row_start(row)?.checked_add(column)
    }

    fn cell(&self, row: usize, column: usize) -> Option<Cell> {
        self.index(row, column)
            .and_then(|index| self.cells.get(index))
            .copied()
    }

    fn row_wrapped(&self, row: usize) -> bool {
        self.wrapped_rows.get(row).copied().unwrap_or(false)
    }

    /// The row no longer goes on at the next: what reached the edge was
    /// erased or shifted, or the row it went on at was replaced.
    fn unwrap_row(&mut self, row: usize) {
        if let Some(wrapped) = self.wrapped_rows.get_mut(row) {
            *wrapped = false;
        }
    }

    fn set_cell(&mut self, row: usize, column: usize, cell: Cell) {
        if let Some(index) = self.index(row, column) {
            if let Some(target) = self.cells.get_mut(index) {
                *target = cell;
                self.damage(row);
            }
        }
    }

    fn damage(&mut self, row: usize) {
        if let Some(damaged) = self.damaged_rows.get_mut(row) {
            *damaged = true;
        }
    }

    fn damage_range(&mut self, start: usize, end: usize) {
        let bounded_end = end.min(self.rows);
        for row in start.min(bounded_end)..bounded_end {
            self.damage(row);
        }
    }

    fn save_cursor(&mut self) {
        self.ansi_saved = Some(SavedCursor {
            row: self.cursor_row,
            column: self.cursor_column,
            pending_wrap: self.pending_wrap,
        });
    }

    fn restore_cursor(&mut self) {
        if let Some(saved) = self.ansi_saved {
            self.cursor_row = saved.row.min(self.rows.saturating_sub(1));
            self.cursor_column = saved.column.min(self.columns.saturating_sub(1));
            self.pending_wrap = saved.pending_wrap && self.cursor_column + 1 == self.columns;
        }
    }

    fn clear_history(&mut self) {
        self.history.clear();
    }

    fn record_history_rows(&mut self, start: usize, count: usize) {
        let end = start.saturating_add(count).min(self.rows);
        for row in start..end {
            let Some(cell_start) = self.row_start(row) else {
                continue;
            };
            let wrapped = self.wrapped_rows.get(row).copied().unwrap_or(false);
            self.history
                .push_line(&self.cells, cell_start, self.columns, wrapped);
        }
    }

    fn clear_row(&mut self, row: usize, attributes: Attributes) {
        for column in 0..self.columns {
            self.set_cell(row, column, Cell::blank(attributes));
        }
        if row == 0 {
            self.history.unwrap_newest();
        }
        if let Some(wrapped) = self.wrapped_rows.get_mut(row) {
            *wrapped = false;
        }
    }

    fn clear_segment(&mut self, row: usize, start: usize, end: usize, attributes: Attributes) {
        let bounded_end = end.min(self.columns);
        for column in start.min(bounded_end)..bounded_end {
            self.set_cell(row, column, Cell::blank(attributes));
        }
        if start < bounded_end && bounded_end == self.columns {
            self.unwrap_row(row);
            if start == 0 && row == 0 {
                self.history.unwrap_newest();
            }
        }
    }

    fn scroll_up(
        &mut self,
        top: usize,
        bottom: usize,
        count: usize,
        attributes: Attributes,
        record_history: bool,
    ) {
        let top = top.min(self.rows);
        let bottom = bottom.min(self.rows);
        if top >= bottom {
            return;
        }
        let count = count.min(bottom - top);
        if count == 0 {
            return;
        }
        if record_history && top == 0 && bottom == self.rows {
            self.record_history_rows(top, count);
        } else if top == 0 {
            self.history.unwrap_newest();
        }
        // The rows at either edge of the region lose what they went on at:
        // the one above sees its next row replaced, and the last moves up
        // away from the row below the region.
        if let Some(above) = top.checked_sub(1) {
            self.unwrap_row(above);
        }
        if let Some(last) = bottom.checked_sub(1) {
            self.unwrap_row(last);
        }
        if top == 0 && bottom == self.rows {
            // The whole screen: its first `count` rows become its last.
            self.origin = self
                .origin
                .saturating_add(count)
                .checked_rem(self.rows)
                .unwrap_or(0);
        } else {
            for row in top..bottom - count {
                self.copy_row(row.saturating_add(count), row);
            }
        }
        self.blank_rows(bottom - count..bottom, attributes);
        self.wrapped_rows
            .copy_within(top.saturating_add(count)..bottom, top);
        if let Some(rows) = self
            .wrapped_rows
            .get_mut(bottom.saturating_sub(count)..bottom)
        {
            rows.fill(false);
        }
        self.damage_range(top, bottom);
    }

    fn scroll_down(&mut self, top: usize, bottom: usize, count: usize, attributes: Attributes) {
        let top = top.min(self.rows);
        let bottom = bottom.min(self.rows);
        if top >= bottom {
            return;
        }
        let count = count.min(bottom - top);
        if count == 0 {
            return;
        }
        if let Some(above) = top.checked_sub(1) {
            self.unwrap_row(above);
        } else {
            self.history.unwrap_newest();
        }
        if top == 0 && bottom == self.rows {
            // The whole screen: its last `count` rows become its first.
            self.origin = self
                .origin
                .saturating_add(self.rows - count)
                .checked_rem(self.rows)
                .unwrap_or(0);
        } else {
            for row in (top + count..bottom).rev() {
                self.copy_row(row - count, row);
            }
        }
        self.blank_rows(top..top + count, attributes);
        self.wrapped_rows
            .copy_within(top..bottom.saturating_sub(count), top.saturating_add(count));
        if let Some(rows) = self.wrapped_rows.get_mut(top..top.saturating_add(count)) {
            rows.fill(false);
        }
        // Moved down to the region's last row, a row went on at one the
        // scroll dropped; the row below the region is not it.
        if let Some(last) = bottom.checked_sub(1) {
            self.unwrap_row(last);
        }
        self.damage_range(top, bottom);
    }

    fn linefeed(&mut self, attributes: Attributes, record_history: bool, wrapped: bool) {
        self.pending_wrap = false;
        if let Some(marker) = self.wrapped_rows.get_mut(self.cursor_row) {
            *marker = wrapped;
        }
        if self.cursor_row >= self.scroll_top && self.cursor_row < self.scroll_bottom {
            if self.cursor_row + 1 >= self.scroll_bottom {
                self.scroll_up(
                    self.scroll_top,
                    self.scroll_bottom,
                    1,
                    attributes,
                    record_history,
                );
                // The scroll ended the margin row's wrap, as for any row
                // leaving a region's last row; this one goes on at the row
                // the cursor now writes, so it is marked where it moved.
                if let Some(moved) = self
                    .cursor_row
                    .checked_sub(1)
                    .filter(|row| *row >= self.scroll_top)
                {
                    if let Some(marker) = self.wrapped_rows.get_mut(moved) {
                        *marker = wrapped;
                    }
                }
            } else {
                self.cursor_row += 1;
            }
        } else if self.cursor_row + 1 < self.rows {
            self.cursor_row += 1;
        }
    }

    fn reverse_index(&mut self, attributes: Attributes) {
        self.pending_wrap = false;
        if self.cursor_row == self.scroll_top {
            self.scroll_down(self.scroll_top, self.scroll_bottom, 1, attributes);
        } else if self.cursor_row > 0 {
            self.cursor_row -= 1;
        }
    }

    fn insert_characters(&mut self, count: usize, attributes: Attributes) {
        let remaining = self.columns.saturating_sub(self.cursor_column);
        let count = count.max(1).min(remaining);
        if count == 0 {
            return;
        }
        let Some(row_start) = self.row_start(self.cursor_row) else {
            return;
        };
        let Some(start) = row_start.checked_add(self.cursor_column) else {
            return;
        };
        let Some(end) = row_start.checked_add(self.columns) else {
            return;
        };
        self.cells.copy_within(
            start..end.saturating_sub(count),
            start.saturating_add(count),
        );
        if let Some(cells) = self.cells.get_mut(start..start.saturating_add(count)) {
            cells.fill(Cell::blank(attributes));
        }
        self.unwrap_row(self.cursor_row);
        self.damage(self.cursor_row);
        self.pending_wrap = false;
    }

    fn delete_characters(&mut self, count: usize, attributes: Attributes) {
        let remaining = self.columns.saturating_sub(self.cursor_column);
        let count = count.max(1).min(remaining);
        if count == 0 {
            return;
        }
        let Some(row_start) = self.row_start(self.cursor_row) else {
            return;
        };
        let Some(start) = row_start.checked_add(self.cursor_column) else {
            return;
        };
        let Some(end) = row_start.checked_add(self.columns) else {
            return;
        };
        self.cells
            .copy_within(start.saturating_add(count)..end, start);
        if let Some(cells) = self.cells.get_mut(end.saturating_sub(count)..end) {
            cells.fill(Cell::blank(attributes));
        }
        self.unwrap_row(self.cursor_row);
        self.damage(self.cursor_row);
        self.pending_wrap = false;
    }

    fn erase_characters(&mut self, count: usize, attributes: Attributes) {
        let end = self
            .cursor_column
            .saturating_add(count.max(1))
            .min(self.columns);
        self.clear_segment(self.cursor_row, self.cursor_column, end, attributes);
        self.pending_wrap = false;
    }

    fn erase_in_line(&mut self, mode: u16, attributes: Attributes) {
        match mode {
            0 => self.clear_segment(
                self.cursor_row,
                self.cursor_column,
                self.columns,
                attributes,
            ),
            1 => self.clear_segment(
                self.cursor_row,
                0,
                self.cursor_column.saturating_add(1),
                attributes,
            ),
            2 => self.clear_row(self.cursor_row, attributes),
            _ => {}
        }
        self.pending_wrap = false;
    }

    fn erase_in_display(&mut self, mode: u16, attributes: Attributes) {
        match mode {
            0 => {
                self.erase_in_line(0, attributes);
                for row in self.cursor_row.saturating_add(1)..self.rows {
                    self.clear_row(row, attributes);
                }
            }
            1 => {
                for row in 0..self.cursor_row {
                    self.clear_row(row, attributes);
                }
                self.erase_in_line(1, attributes);
            }
            2 => {
                for row in 0..self.rows {
                    self.clear_row(row, attributes);
                }
            }
            _ => {}
        }
        self.pending_wrap = false;
    }

    fn next_tab(&mut self, count: usize) {
        for _ in 0..count.max(1) {
            let mut target = self.columns.saturating_sub(1);
            for column in self.cursor_column.saturating_add(1)..self.columns {
                if self.tabs.get(column).copied().unwrap_or(false) {
                    target = column;
                    break;
                }
            }
            self.cursor_column = target;
        }
        self.pending_wrap = false;
    }

    fn previous_tab(&mut self, count: usize) {
        for _ in 0..count.max(1) {
            let mut target = 0;
            let mut column = self.cursor_column;
            while column > 0 {
                column -= 1;
                if self.tabs.get(column).copied().unwrap_or(false) {
                    target = column;
                    break;
                }
            }
            self.cursor_column = target;
        }
        self.pending_wrap = false;
    }

    fn row_is_blank(&self, row: usize) -> bool {
        if self.wrapped_rows.get(row).copied().unwrap_or(false) {
            return false;
        }
        let blank = Cell::blank(Attributes::default());
        for column in 0..self.columns {
            if self.cell(row, column) != Some(blank) {
                return false;
            }
        }
        true
    }

    fn resize(
        &mut self,
        rows: usize,
        columns: usize,
        attributes: Attributes,
        record_history: bool,
    ) -> Result<usize, String> {
        let count = checked_cell_count(rows, columns)?;
        let mut cells = vec![Cell::blank(attributes); count];
        let old_rows = self.rows;
        let old_columns = self.columns;
        let old_cursor_row = self.cursor_row;
        let old_cursor_column = self.cursor_column;
        let old_pending_wrap = self.pending_wrap;
        let mut last_preserved = old_cursor_row.min(old_rows.saturating_sub(1));
        for row in 0..old_rows {
            if !self.row_is_blank(row) {
                last_preserved = last_preserved.max(row);
            }
        }
        let row_offset = last_preserved
            .saturating_add(1)
            .saturating_sub(rows)
            .min(old_rows.saturating_sub(rows));
        if record_history && row_offset > 0 {
            self.record_history_rows(0, row_offset);
        } else if row_offset > 0 {
            self.history.unwrap_newest();
        }
        let copy_rows = old_rows.saturating_sub(row_offset).min(rows);
        let copy_columns = self.columns.min(columns);
        for row in 0..copy_rows {
            let source_row = row.saturating_add(row_offset);
            for column in 0..copy_columns {
                let Some(source) = self.cell(source_row, column) else {
                    continue;
                };
                let target_index = row
                    .checked_mul(columns)
                    .and_then(|base| base.checked_add(column))
                    .ok_or_else(|| "resized terminal cell index overflow".to_string())?;
                if let Some(target) = cells.get_mut(target_index) {
                    *target = source;
                }
            }
        }
        self.rows = rows;
        self.columns = columns;
        self.cells = cells;
        self.origin = 0;
        let cursor = SavedCursor {
            row: old_cursor_row,
            column: old_cursor_column,
            pending_wrap: old_pending_wrap,
        }
        .resized(row_offset, rows, old_columns, columns);
        self.cursor_row = cursor.row;
        self.cursor_column = cursor.column;
        self.pending_wrap = cursor.pending_wrap;
        self.ansi_saved = self
            .ansi_saved
            .map(|saved| saved.resized(row_offset, rows, old_columns, columns));
        self.scroll_top = 0;
        self.scroll_bottom = rows;
        let mut tabs = default_tabs(columns);
        for column in 0..old_columns.min(columns) {
            if let (Some(source), Some(target)) = (self.tabs.get(column), tabs.get_mut(column)) {
                *target = *source;
            }
        }
        self.tabs = tabs;
        // Without reflow a row of another width no longer wraps at the edge.
        let mut wrapped_rows = vec![false; rows];
        let marked_rows = if columns == old_columns { copy_rows } else { 0 };
        for row in 0..marked_rows {
            let source_row = row.saturating_add(row_offset);
            if let (Some(source), Some(target)) =
                (self.wrapped_rows.get(source_row), wrapped_rows.get_mut(row))
            {
                *target = *source;
            }
        }
        self.wrapped_rows = wrapped_rows;
        self.damaged_rows = vec![true; rows];
        Ok(row_offset)
    }

    fn reset(&mut self, attributes: Attributes) {
        for row in 0..self.rows {
            self.clear_row(row, attributes);
        }
        self.cursor_row = 0;
        self.cursor_column = 0;
        self.pending_wrap = false;
        self.scroll_top = 0;
        self.scroll_bottom = self.rows;
        self.tabs = default_tabs(self.columns);
        self.history.clear();
        self.ansi_saved = None;
        self.wrapped_rows.fill(false);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StringKind {
    Osc,
    Dcs,
    Sos,
    Apc,
    Pm,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Csi {
    params: [u16; MAX_CSI_PARAMS],
    present: [bool; MAX_CSI_PARAMS],
    /// Whether a colon began each parameter, making it a subparameter of
    /// the one before.
    sub: [bool; MAX_CSI_PARAMS],
    count: usize,
    private: bool,
    intermediate: bool,
    overflow: bool,
}

impl Csi {
    fn new() -> Self {
        Self {
            params: [0; MAX_CSI_PARAMS],
            present: [false; MAX_CSI_PARAMS],
            sub: [false; MAX_CSI_PARAMS],
            count: 1,
            private: false,
            intermediate: false,
            overflow: false,
        }
    }

    fn digit(&mut self, digit: u8) {
        let index = self.count.saturating_sub(1);
        let Some(value) = self.params.get_mut(index) else {
            self.overflow = true;
            return;
        };
        let next = value
            .checked_mul(10)
            .and_then(|current| current.checked_add(u16::from(digit)));
        match next {
            Some(next) => {
                *value = next;
                if let Some(present) = self.present.get_mut(index) {
                    *present = true;
                }
            }
            None => self.overflow = true,
        }
    }

    fn separator(&mut self) {
        if self.count >= MAX_CSI_PARAMS {
            self.overflow = true;
        } else {
            self.count += 1;
        }
    }

    fn colon(&mut self) {
        if let Some(sub) = self.sub.get_mut(self.count) {
            *sub = true;
        }
        self.separator();
    }

    fn sub(&self, index: usize) -> bool {
        self.sub.get(index).copied().unwrap_or(false)
    }

    fn subparameters(&self) -> bool {
        self.sub.iter().take(self.count).any(|sub| *sub)
    }

    fn value(&self, index: usize, default: u16) -> u16 {
        match (
            self.params.get(index).copied(),
            self.present.get(index).copied(),
        ) {
            (Some(value), Some(true)) => value,
            _ => default,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ParserState {
    Ground,
    Escape,
    EscapeIgnore,
    Charset(u8),
    Csi(Csi),
    String(StringKind),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Utf8Decoder {
    codepoint: u32,
    minimum: u32,
    remaining: u8,
}

enum Decode {
    Ascii(u8),
    Pending,
    Scalar(char),
    ScalarAndRetry(char, u8),
}

impl Utf8Decoder {
    fn new() -> Self {
        Self {
            codepoint: 0,
            minimum: 0,
            remaining: 0,
        }
    }

    fn reset(&mut self) {
        self.codepoint = 0;
        self.minimum = 0;
        self.remaining = 0;
    }

    fn push(&mut self, byte: u8) -> Decode {
        if self.remaining == 0 {
            return match byte {
                0x00..=0x7f => Decode::Ascii(byte),
                0xc2..=0xdf => {
                    self.codepoint = u32::from(byte & 0x1f);
                    self.minimum = 0x80;
                    self.remaining = 1;
                    Decode::Pending
                }
                0xe0..=0xef => {
                    self.codepoint = u32::from(byte & 0x0f);
                    self.minimum = 0x800;
                    self.remaining = 2;
                    Decode::Pending
                }
                0xf0..=0xf4 => {
                    self.codepoint = u32::from(byte & 0x07);
                    self.minimum = 0x1_0000;
                    self.remaining = 3;
                    Decode::Pending
                }
                _ => Decode::Scalar('\u{fffd}'),
            };
        }
        if byte & 0xc0 != 0x80 {
            self.reset();
            return Decode::ScalarAndRetry('\u{fffd}', byte);
        }
        self.codepoint = (self.codepoint << 6) | u32::from(byte & 0x3f);
        self.remaining -= 1;
        if self.remaining != 0 {
            return Decode::Pending;
        }
        let codepoint = self.codepoint;
        let minimum = self.minimum;
        self.reset();
        if codepoint < minimum || (0xd800..=0xdfff).contains(&codepoint) || codepoint > 0x10_ffff {
            return Decode::Scalar('\u{fffd}');
        }
        Decode::Scalar(char::from_u32(codepoint).unwrap_or('\u{fffd}'))
    }
}

/// The longest query `Terminal::search` takes, in scalars.
pub const MAX_QUERY: usize = 256;

/// Which way `Terminal::search` looks from where it starts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Toward {
    Older,
    Newer,
}

/// A place in the text `Terminal::search` reads: a line, in a numbering
/// output does not shift, and a column. Line `pushed - lines + n` is
/// history line `n` and line `pushed + r` the screen's row `r`, so the
/// row a view scrolled back `offset` lines shows at its row `v` is line
/// `pushed - offset + v`.
pub type Place = (u64, usize);

/// One match, its first and last cells inclusive.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Found {
    pub start: Place,
    pub end: Place,
}

/// A search's query: its scalars, folded when it has no uppercase letter.
struct Query {
    scalars: Vec<char>,
    fold: bool,
}

impl Query {
    fn new(query: &str) -> Option<Query> {
        let scalars: Vec<char> = query.chars().take(MAX_QUERY.saturating_add(1)).collect();
        if scalars.is_empty() || scalars.len() > MAX_QUERY {
            return None;
        }
        let fold = !scalars.iter().any(|scalar| scalar.is_uppercase());
        let mut query = Query {
            scalars: Vec::new(),
            fold,
        };
        query.scalars = scalars
            .into_iter()
            .map(|scalar| query.fold(scalar))
            .collect();
        Some(query)
    }

    fn fold(&self, scalar: char) -> char {
        if self.fold {
            scalar.to_lowercase().next().unwrap_or(scalar)
        } else {
            scalar
        }
    }
}

/// The text a search reads, as rows numbered from the oldest: history
/// while the primary screen is active, then the active screen's rows, all
/// as wide as the screen.
struct Searched<'a> {
    terminal: &'a Terminal,
    history: usize,
    first: u64,
    rows: usize,
    columns: usize,
}

impl<'a> Searched<'a> {
    fn of(terminal: &'a Terminal) -> Self {
        let history = if terminal.alternate_active {
            0
        } else {
            terminal.history_lines()
        };
        Searched {
            terminal,
            history,
            first: terminal
                .history_pushed()
                .saturating_sub(u64::try_from(history).unwrap_or(u64::MAX)),
            rows: history.saturating_add(terminal.rows()),
            columns: terminal.columns(),
        }
    }

    fn place(&self, row: usize) -> u64 {
        self.first
            .saturating_add(u64::try_from(row).unwrap_or(u64::MAX))
    }

    fn row(&self, line: u64) -> Option<usize> {
        usize::try_from(line.checked_sub(self.first)?)
            .ok()
            .filter(|row| *row < self.rows)
    }

    fn wrapped(&self, row: usize) -> bool {
        if row < self.history {
            self.terminal.history_wrapped(row)
        } else {
            self.terminal.wrapped(row.saturating_sub(self.history))
        }
    }

    fn cell(&self, row: usize, column: usize) -> Option<Cell> {
        if column >= self.columns {
            return None;
        }
        if row < self.history {
            self.terminal.history_cell(row, column)
        } else {
            self.terminal.cell(row.saturating_sub(self.history), column)
        }
    }

    /// Lines as the child wrote them, each the rows a wrap joined.
    fn lines(&self) -> Vec<(usize, usize)> {
        let mut lines = Vec::new();
        let mut start = 0;
        for row in 0..self.rows {
            if !self.wrapped(row) || row.saturating_add(1) == self.rows {
                lines.push((start, row));
                start = row.saturating_add(1);
            }
        }
        lines
    }
}

/// Which pointer events the child asked to have reported (DEC private modes
/// 9, 1000, 1002 and 1003). Setting one replaces whichever was set; resetting
/// the one that is set turns reporting off, and resetting another does
/// nothing, as in foot; xterm turns reporting off on any such reset.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MouseTracking {
    #[default]
    Off,
    /// Mode 9, X10's: presses only, with no modifiers.
    Press,
    /// Mode 1000: presses and releases.
    Click,
    /// Mode 1002: presses, releases, and motion while a button is held.
    Drag,
    /// Mode 1003: presses, releases, and all motion.
    Motion,
}

/// The pointer reporting the child asked for: which events, and whether in
/// SGR's encoding (mode 1006) rather than X10's single bytes.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MouseMode {
    pub tracking: MouseTracking,
    pub sgr: bool,
}

/// What one boundary in `reply_ends` costs, charged against td-term/DESIGN.md §2's reply
/// ceiling beside the bytes it delimits.
const REPLY_OVERHEAD: usize = std::mem::size_of::<usize>();

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Terminal {
    primary: Screen,
    alternate: Screen,
    alternate_active: bool,
    parser: ParserState,
    utf8: Utf8Decoder,
    attributes: Attributes,
    origin_mode: bool,
    auto_wrap: bool,
    cursor_visible: bool,
    application_cursor: bool,
    bracketed_paste: bool,
    mouse: MouseMode,
    g0: Charset,
    g1: Charset,
    use_g1: bool,
    dec_primary: Option<SavedState>,
    dec_alternate: Option<SavedState>,
    last_printed: Option<char>,
    replies: Vec<u8>,
    /// Where each reply ends in `replies`. td-term/DESIGN.md §2's atomicity unit is ONE reply,
    /// so the loop has to be able to admit one and refuse the next; a flat
    /// buffer alone makes a batch look like a single sequence.
    reply_ends: Vec<usize>,
    bell_pending: bool,
    /// The OSC being received, and whether it outgrew `MAX_OSC`.
    osc: Vec<u8>,
    osc_overflow: bool,
    /// The OSC 8 links cells name, oldest first, ids rising, at most
    /// `MAX_LINKS`; an id is never given twice, so a forgotten link's
    /// cells can never name a later one.
    links: VecDeque<Link>,
    next_link: u32,
    /// An OSC 133;A came: the next cell written starts a prompt.
    prompt_pending: bool,
    /// The title the last OSC 0 or 2 set, `None` before one or after an
    /// empty one.
    title: Option<String>,
}

/// An OSC 8 link: its id, the `id=` parameter that lets separately
/// written runs of cells be one link, and its URI.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Link {
    id: u32,
    key: Vec<u8>,
    uri: String,
}

impl Terminal {
    pub fn new(rows: usize, columns: usize) -> Result<Self, String> {
        let attributes = Attributes::default();
        Ok(Self {
            primary: Screen::new(rows, columns, attributes, true)?,
            alternate: Screen::new(rows, columns, attributes, false)?,
            alternate_active: false,
            parser: ParserState::Ground,
            utf8: Utf8Decoder::new(),
            attributes,
            origin_mode: false,
            auto_wrap: true,
            cursor_visible: true,
            application_cursor: false,
            bracketed_paste: false,
            mouse: MouseMode::default(),
            g0: Charset::Ascii,
            g1: Charset::Ascii,
            use_g1: false,
            dec_primary: None,
            dec_alternate: None,
            last_printed: None,
            replies: Vec::new(),
            reply_ends: Vec::new(),
            bell_pending: false,
            osc: Vec::new(),
            osc_overflow: false,
            links: VecDeque::new(),
            next_link: 1,
            prompt_pending: false,
            title: None,
        })
    }

    /// The window title the child set with OSC 0 or 2, while it has one.
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// The URI of the OSC 8 link `id` names, while the model remembers it.
    pub fn link(&self, id: u32) -> Option<&str> {
        let at = self.links.binary_search_by_key(&id, |link| link.id).ok()?;
        self.links.get(at).map(|link| link.uri.as_str())
    }

    fn screen(&self) -> &Screen {
        if self.alternate_active {
            &self.alternate
        } else {
            &self.primary
        }
    }

    fn screen_mut(&mut self) -> &mut Screen {
        if self.alternate_active {
            &mut self.alternate
        } else {
            &mut self.primary
        }
    }

    fn records_history(&self) -> bool {
        !self.alternate_active
    }

    fn save_dec_state(&mut self) {
        let screen = self.screen();
        let saved = SavedState {
            cursor: SavedCursor {
                row: screen.cursor_row,
                column: screen.cursor_column,
                pending_wrap: screen.pending_wrap,
            },
            attributes: self.attributes,
            origin_mode: self.origin_mode,
            auto_wrap: self.auto_wrap,
            g0: self.g0,
            g1: self.g1,
            use_g1: self.use_g1,
        };
        if self.alternate_active {
            self.dec_alternate = Some(saved);
        } else {
            self.dec_primary = Some(saved);
        }
    }

    fn restore_dec_state(&mut self) {
        let saved = if self.alternate_active {
            self.dec_alternate
        } else {
            self.dec_primary
        };
        let Some(saved) = saved else {
            return;
        };
        // An OSC 8 link lasts until an OSC 8 ends it, not with the cursor.
        self.attributes = Attributes {
            link: self.attributes.link,
            ..saved.attributes
        };
        self.origin_mode = saved.origin_mode;
        self.auto_wrap = saved.auto_wrap;
        self.g0 = saved.g0;
        self.g1 = saved.g1;
        self.use_g1 = saved.use_g1;
        let screen = self.screen_mut();
        screen.cursor_row = saved.cursor.row.min(screen.rows.saturating_sub(1));
        screen.cursor_column = saved.cursor.column.min(screen.columns.saturating_sub(1));
        screen.pending_wrap =
            saved.cursor.pending_wrap && screen.cursor_column.saturating_add(1) == screen.columns;
    }

    pub fn rows(&self) -> usize {
        self.screen().rows
    }

    pub fn columns(&self) -> usize {
        self.screen().columns
    }

    pub fn cursor(&self) -> (usize, usize, bool) {
        let screen = self.screen();
        (screen.cursor_row, screen.cursor_column, screen.pending_wrap)
    }

    pub fn cell(&self, row: usize, column: usize) -> Option<Cell> {
        self.screen().cell(row, column)
    }

    pub fn row_text(&self, row: usize) -> Result<String, String> {
        if row >= self.rows() {
            return Err(format!("terminal row {row} is out of bounds"));
        }
        let mut text = String::with_capacity(self.columns());
        for column in 0..self.columns() {
            let cell = self
                .cell(row, column)
                .ok_or_else(|| format!("terminal cell {row},{column} is missing"))?;
            text.push(cell.scalar);
        }
        Ok(text)
    }

    pub fn mode(&self, name: &str) -> Option<bool> {
        match name {
            "alternate-screen" => Some(self.alternate_active),
            "application-cursor" => Some(self.application_cursor),
            "bracketed-paste" => Some(self.bracketed_paste),
            "mouse-press" => Some(self.mouse.tracking == MouseTracking::Press),
            "mouse-click" => Some(self.mouse.tracking == MouseTracking::Click),
            "mouse-drag" => Some(self.mouse.tracking == MouseTracking::Drag),
            "mouse-motion" => Some(self.mouse.tracking == MouseTracking::Motion),
            "mouse-sgr" => Some(self.mouse.sgr),
            "autowrap" => Some(self.auto_wrap),
            "cursor-visible" => Some(self.cursor_visible),
            "origin" => Some(self.origin_mode),
            _ => None,
        }
    }

    /// The pointer reporting the child asked for.
    pub fn mouse(&self) -> MouseMode {
        self.mouse
    }

    #[cfg(test)]
    pub fn replies(&self) -> &[u8] {
        &self.replies
    }

    /// The pending replies, each on its own, in the order the child asked.
    pub fn take_replies(&mut self) -> Vec<Vec<u8>> {
        let replies = std::mem::take(&mut self.replies);
        let ends = std::mem::take(&mut self.reply_ends);
        let mut sequences = Vec::with_capacity(ends.len());
        let mut start = 0;
        for end in ends {
            if let Some(reply) = replies.get(start..end) {
                sequences.push(reply.to_vec());
            }
            start = end;
        }
        sequences
    }

    /// td-term/DESIGN.md §2's third source of the visual bell, beside C0 BEL and a reply the
    /// model itself could not hold: a sequence the main loop could not admit
    /// to the child WHOLE. The model owns the bit because the renderer reads
    /// it from a snapshot, so the loop has nowhere else to put one.
    pub fn ring(&mut self) {
        self.bell_pending = true;
    }

    pub fn take_bell(&mut self) -> bool {
        std::mem::take(&mut self.bell_pending)
    }

    pub fn history_cells(&self) -> usize {
        self.primary.history.cells
    }

    /// Scrollback is primary-screen only, so these read the primary history
    /// even while the alternate screen is active — which is what lets the
    /// viewport show the shell a full-screen program is covering.
    pub fn history_lines(&self) -> usize {
        self.primary.history.lines.len()
    }

    /// Lines ever pushed to primary history. Eviction lowers
    /// `history_lines()` but never this, which is what lets a viewport name
    /// a line rather than a distance from a moving bottom.
    ///
    pub fn history_pushed(&self) -> u64 {
        self.primary.history.pushed
    }

    /// Which numbering `history_pushed` is counting in. A clear -- a reset,
    /// or `CSI 3 J` -- retires the old one.
    pub fn history_epoch(&self) -> u64 {
        self.primary.history.epoch
    }

    /// The three numbers a viewport needs, read together so they cannot
    /// describe different moments.
    pub fn scrollback(&self) -> crate::vt_keys::Scrollback {
        crate::vt_keys::Scrollback {
            epoch: self.history_epoch(),
            pushed: self.history_pushed(),
            lines: self.history_lines(),
        }
    }

    /// Oldest line first: `history_lines() - 1` is the row that most
    /// recently scrolled off, the one immediately above the live screen.
    /// `None` past a line's stored width, which a resize can leave shorter
    /// than the current grid.
    pub fn history_cell(&self, line: usize, column: usize) -> Option<Cell> {
        self.primary.history.line_cell(line, column)
    }

    /// Whether the active screen's `row` was ended by an autowrap: its text
    /// goes on at the start of the next row rather than having been ended
    /// by the child. The mark goes when what reached the edge is erased or
    /// shifted (an erase to the last column, ICH, DCH), when the row it went
    /// on at is replaced (rows inserted, deleted or scrolled below it, or
    /// it scrolled away from a region's last row), and when the width
    /// changes.
    pub fn wrapped(&self, row: usize) -> bool {
        self.screen().row_wrapped(row)
    }

    /// `wrapped` for the primary screen whichever screen is active, as
    /// `primary_cell` is.
    pub fn primary_wrapped(&self, row: usize) -> bool {
        self.primary.row_wrapped(row)
    }

    /// `wrapped` for a history line, numbered as `history_cell` numbers
    /// them; the newest goes on at the primary screen's first row, until
    /// that row is cleared whole or replaced without being pushed. Only a
    /// line stored at the screen's present width can wrap at its edge.
    pub fn history_wrapped(&self, line: usize) -> bool {
        self.primary
            .history
            .line_wrapped(line, self.primary.columns)
    }

    /// The primary screen's cell whichever screen is active. An open
    /// scrollback viewport reads through this so it shows one coherent
    /// primary scroll region; `cell` would put primary history above the
    /// split and a full-screen program's alternate rows below it.
    pub fn primary_cell(&self, row: usize, column: usize) -> Option<Cell> {
        self.primary.cell(row, column)
    }

    /// The match of `query` nearest `from` toward older or newer text, not
    /// at `from` itself; from the newest (or oldest) end when `from` is
    /// `None`. The text is the active screen and, while that is the
    /// primary, its history, as wide as the screen: a line runs on across
    /// the rows the terminal wrapped it over, so a match can span a wrap.
    /// A query with no uppercase letter matches either case, each scalar
    /// folded to one, as foot's `towlower` does. An empty query, or one
    /// past `MAX_QUERY`, matches nothing.
    pub fn search(&self, query: &str, from: Option<Place>, toward: Toward) -> Option<Found> {
        let query = Query::new(query)?;
        let text = Searched::of(self);
        let lines = text.lines();
        let width = text.columns.max(1);
        let mut line_text: Vec<char> = Vec::new();
        for step in 0..lines.len() {
            let index = match toward {
                Toward::Older => lines.len().saturating_sub(step.saturating_add(1)),
                Toward::Newer => step,
            };
            let Some(&(top, bottom)) = lines.get(index) else {
                continue;
            };
            let skip = match (toward, from) {
                (Toward::Older, Some((line, _))) => text.place(top) > line,
                (Toward::Newer, Some((line, _))) => text.place(bottom) < line,
                (_, None) => false,
            };
            if skip {
                continue;
            }
            line_text.clear();
            for row in top..=bottom {
                for column in 0..text.columns {
                    let Some(cell) = text.cell(row, column) else {
                        break;
                    };
                    line_text.push(query.fold(cell.scalar));
                }
            }
            // Every row but a line's last is a whole row wide, which is
            // what a wrap mark requires, so a scalar's place is arithmetic.
            let place = |at: usize| (text.place(top.saturating_add(at / width)), at % width);
            let length = query.scalars.len();
            let mut best: Option<Found> = None;
            for at in 0..line_text.len().saturating_sub(length.saturating_sub(1)) {
                if line_text.get(at..at.saturating_add(length)) != Some(query.scalars.as_slice()) {
                    continue;
                }
                let found = Found {
                    start: place(at),
                    end: place(at.saturating_add(length).saturating_sub(1)),
                };
                let beyond = match (toward, from) {
                    (Toward::Older, Some(from)) => found.start < from,
                    (Toward::Newer, Some(from)) => found.start > from,
                    (_, None) => true,
                };
                let better = match (toward, best) {
                    (_, None) => true,
                    (Toward::Older, Some(best)) => found.start > best.start,
                    (Toward::Newer, Some(best)) => found.start < best.start,
                };
                if beyond && better {
                    best = Some(found);
                }
            }
            if best.is_some() {
                return best;
            }
        }
        None
    }

    /// The line, in `search`'s numbering, of the nearest row older or
    /// newer than the line `from` that holds a prompt's start (OSC 133;A),
    /// for a jump between a shell's prompts. None on the alternate screen,
    /// whose programs are not the shell's.
    pub fn prompt(&self, from: u64, toward: Toward) -> Option<u64> {
        if self.alternate_active {
            return None;
        }
        let text = Searched::of(self);
        let marked = |row: &usize| {
            (0..text.columns).any(|column| {
                text.cell(*row, column)
                    .is_some_and(|cell| cell.attributes.prompt)
            })
        };
        let row = text.row(from);
        let before = from < text.first;
        let found = match toward {
            Toward::Older => {
                let bound = row.unwrap_or(if before { 0 } else { text.rows });
                (0..bound).rev().find(marked)
            }
            Toward::Newer => {
                let start = match row {
                    Some(row) => row.saturating_add(1),
                    None if before => 0,
                    None => text.rows,
                };
                (start..text.rows).find(marked)
            }
        };
        found.map(|row| text.place(row))
    }

    /// Whether `found` still spells `query` where it is: output since it
    /// was found can have rewritten those cells, or ended the wrap a match
    /// ran across.
    pub fn still_matches(&self, query: &str, found: Found) -> bool {
        let Some(query) = Query::new(query) else {
            return false;
        };
        let text = Searched::of(self);
        let Some(mut row) = text.row(found.start.0) else {
            return false;
        };
        let mut column = found.start.1;
        for (index, wanted) in query.scalars.iter().enumerate() {
            if index > 0 {
                column = column.saturating_add(1);
                if column >= text.columns {
                    if !text.wrapped(row) {
                        return false;
                    }
                    row = row.saturating_add(1);
                    column = 0;
                }
            }
            match text.cell(row, column) {
                Some(cell) if query.fold(cell.scalar) == *wanted => {}
                _ => return false,
            }
        }
        (text.place(row), column) == found.end
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.feed_byte(*byte);
        }
    }

    fn feed_byte(&mut self, byte: u8) {
        let mut pending = Some(byte);
        while let Some(current) = pending.take() {
            let state = std::mem::replace(&mut self.parser, ParserState::Ground);
            match state {
                ParserState::Ground => {
                    if matches!(current, 0x00..=0x17 | 0x19 | 0x1c..=0x1f) {
                        self.execute_c0(current);
                        self.parser = ParserState::Ground;
                        continue;
                    }
                    if current == 0x7f {
                        self.parser = ParserState::Ground;
                        continue;
                    }
                    match self.utf8.push(current) {
                        Decode::Ascii(ascii) => self.ground_ascii(ascii),
                        Decode::Pending => self.parser = ParserState::Ground,
                        Decode::Scalar(scalar) => {
                            self.put_char(scalar);
                            self.parser = ParserState::Ground;
                        }
                        Decode::ScalarAndRetry(scalar, retry) => {
                            self.put_char(scalar);
                            self.parser = ParserState::Ground;
                            pending = Some(retry);
                        }
                    }
                }
                ParserState::Escape => self.escape_byte(current),
                ParserState::EscapeIgnore => {
                    if (0x30..=0x7e).contains(&current) || matches!(current, 0x18 | 0x1a) {
                        self.parser = ParserState::Ground;
                    } else if current == 0x1b {
                        self.parser = ParserState::Escape;
                    } else if matches!(current, 0x00..=0x17 | 0x19 | 0x1c..=0x1f) {
                        self.execute_c0(current);
                        self.parser = ParserState::EscapeIgnore;
                    } else {
                        self.parser = ParserState::EscapeIgnore;
                    }
                }
                ParserState::Charset(slot) => {
                    if matches!(current, 0x18 | 0x1a) {
                        self.parser = ParserState::Ground;
                        continue;
                    }
                    if current == 0x1b {
                        self.parser = ParserState::Escape;
                        continue;
                    }
                    if matches!(current, 0x00..=0x17 | 0x19 | 0x1c..=0x1f) {
                        self.execute_c0(current);
                        self.parser = ParserState::Charset(slot);
                        continue;
                    }
                    if current == 0x7f {
                        self.parser = ParserState::Charset(slot);
                        continue;
                    }
                    if (0x20..=0x2f).contains(&current) {
                        self.parser = ParserState::EscapeIgnore;
                        continue;
                    }
                    let charset = match current {
                        b'B' => Some(Charset::Ascii),
                        b'0' => Some(Charset::DecSpecial),
                        _ => None,
                    };
                    if let Some(charset) = charset {
                        if slot == 0 {
                            self.g0 = charset;
                        } else {
                            self.g1 = charset;
                        }
                    }
                    self.parser = ParserState::Ground;
                }
                ParserState::Csi(csi) => self.csi_byte(csi, current),
                ParserState::String(kind) => self.string_byte(kind, current),
            }
        }
    }

    fn ground_ascii(&mut self, byte: u8) {
        match byte {
            0x1b => self.parser = ParserState::Escape,
            0x20..=0x7e => {
                let scalar = self.map_charset(char::from(byte));
                self.put_char(scalar);
                self.parser = ParserState::Ground;
            }
            _ => {
                self.execute_c0(byte);
                self.parser = ParserState::Ground;
            }
        }
    }

    fn execute_c0(&mut self, byte: u8) {
        match byte {
            0x07 => self.bell_pending = true,
            0x08 => {
                let screen = self.screen_mut();
                screen.cursor_column = screen.cursor_column.saturating_sub(1);
                screen.pending_wrap = false;
            }
            0x09 => self.screen_mut().next_tab(1),
            0x0a..=0x0c => {
                let attributes = self.attributes;
                let history = self.records_history();
                self.screen_mut().linefeed(attributes, history, false);
            }
            0x0d => {
                let screen = self.screen_mut();
                screen.cursor_column = 0;
                screen.pending_wrap = false;
            }
            0x0e => {
                self.use_g1 = true;
            }
            0x0f => {
                self.use_g1 = false;
            }
            _ => {}
        }
    }

    fn map_charset(&self, scalar: char) -> char {
        let charset = if self.use_g1 { self.g1 } else { self.g0 };
        if charset == Charset::Ascii {
            return scalar;
        }
        match scalar {
            '_' => ' ',
            '`' => '◆',
            'a' => '▒',
            'b' => '␉',
            'c' => '␌',
            'd' => '␍',
            'e' => '␊',
            'f' => '°',
            'g' => '±',
            'h' => '␤',
            'i' => '␋',
            'j' => '┘',
            'k' => '┐',
            'l' => '┌',
            'm' => '└',
            'n' => '┼',
            'o' => '⎺',
            'p' => '⎻',
            'q' => '─',
            'r' => '⎼',
            's' => '⎽',
            't' => '├',
            'u' => '┤',
            'v' => '┴',
            'w' => '┬',
            'x' => '│',
            'y' => '≤',
            'z' => '≥',
            '{' => 'π',
            '|' => '≠',
            '}' => '£',
            '~' => '·',
            _ => scalar,
        }
    }

    fn put_char(&mut self, scalar: char) {
        let attributes = Attributes {
            prompt: std::mem::take(&mut self.prompt_pending),
            ..self.attributes
        };
        let history = self.records_history();
        let auto_wrap = self.auto_wrap;
        let screen = self.screen_mut();
        if screen.pending_wrap {
            if auto_wrap {
                screen.cursor_column = 0;
                screen.linefeed(attributes, history, true);
            } else {
                screen.pending_wrap = false;
            }
        }
        // A prompt redrawn over its mark keeps it; only an erase drops it.
        let marked = screen
            .cell(screen.cursor_row, screen.cursor_column)
            .is_some_and(|cell| cell.attributes.prompt);
        let attributes = Attributes {
            prompt: attributes.prompt || marked,
            ..attributes
        };
        screen.set_cell(
            screen.cursor_row,
            screen.cursor_column,
            Cell { scalar, attributes },
        );
        if screen.cursor_column + 1 >= screen.columns {
            screen.pending_wrap = auto_wrap;
        } else {
            screen.cursor_column += 1;
            screen.pending_wrap = false;
        }
        self.last_printed = Some(scalar);
    }

    fn escape_byte(&mut self, byte: u8) {
        match byte {
            b'[' => self.parser = ParserState::Csi(Csi::new()),
            b']' => self.start_string(StringKind::Osc),
            b'P' => self.start_string(StringKind::Dcs),
            b'X' => self.start_string(StringKind::Sos),
            b'_' => self.start_string(StringKind::Apc),
            b'^' => self.start_string(StringKind::Pm),
            b'(' => self.parser = ParserState::Charset(0),
            b')' => self.parser = ParserState::Charset(1),
            b'7' => {
                self.save_dec_state();
                self.parser = ParserState::Ground;
            }
            b'8' => {
                self.restore_dec_state();
                self.parser = ParserState::Ground;
            }
            b'D' => {
                let attributes = self.attributes;
                let history = self.records_history();
                self.screen_mut().linefeed(attributes, history, false);
                self.parser = ParserState::Ground;
            }
            b'E' => {
                let attributes = self.attributes;
                let history = self.records_history();
                let screen = self.screen_mut();
                screen.cursor_column = 0;
                screen.linefeed(attributes, history, false);
                self.parser = ParserState::Ground;
            }
            b'H' => {
                let screen = self.screen_mut();
                if let Some(tab) = screen.tabs.get_mut(screen.cursor_column) {
                    *tab = true;
                }
                self.parser = ParserState::Ground;
            }
            b'M' => {
                let attributes = self.attributes;
                self.screen_mut().reverse_index(attributes);
                self.parser = ParserState::Ground;
            }
            b'c' => {
                self.reset_model();
                self.parser = ParserState::Ground;
            }
            0x00..=0x17 | 0x19 | 0x1c..=0x1f => {
                self.execute_c0(byte);
                self.parser = ParserState::Escape;
            }
            0x18 | 0x1a => self.parser = ParserState::Ground,
            0x1b => self.parser = ParserState::Escape,
            0x7f => self.parser = ParserState::Escape,
            0x20..=0x2f => self.parser = ParserState::EscapeIgnore,
            _ => self.parser = ParserState::Ground,
        }
    }

    fn start_string(&mut self, kind: StringKind) {
        self.osc.clear();
        self.osc_overflow = false;
        self.parser = ParserState::String(kind);
    }

    /// A string's byte. An OSC's payload is kept, to `MAX_OSC`, and acted
    /// on when BEL or ESC (the start of ST) ends it; CAN and SUB cancel it.
    fn string_byte(&mut self, kind: StringKind, byte: u8) {
        if matches!(byte, 0x18 | 0x1a) {
            self.parser = ParserState::Ground;
            return;
        }
        if matches!(byte, 0x07 | 0x1b) {
            if kind == StringKind::Osc && !self.osc_overflow {
                self.dispatch_osc();
            }
            self.parser = if byte == 0x1b {
                ParserState::Escape
            } else {
                ParserState::Ground
            };
            return;
        }
        if kind == StringKind::Osc {
            if self.osc.len() < MAX_OSC {
                self.osc.push(byte);
            } else {
                self.osc_overflow = true;
            }
        }
        self.parser = ParserState::String(kind);
    }

    /// An OSC this profile acts on: 0 or 2, the window title, 8, a
    /// hyperlink, or 133;A, a shell prompt's start, whatever parameters
    /// follow. Any other is ignored.
    fn dispatch_osc(&mut self) {
        let mut payload = std::mem::take(&mut self.osc);
        if let Some(text) = payload
            .strip_prefix(b"0;")
            .or_else(|| payload.strip_prefix(b"2;"))
        {
            self.osc_title(text);
        } else if let Some(rest) = payload.strip_prefix(b"8;") {
            self.osc_link(rest);
        } else if payload == b"133;A" || payload.starts_with(b"133;A;") {
            self.prompt_pending = true;
        }
        payload.clear();
        self.osc = payload;
    }

    /// `OSC 0 ; text` or `OSC 2 ; text`: the window's title, malformed
    /// UTF-8 replaced by U+FFFD, cut to `MAX_TITLE` characters, each one a
    /// title would not show (`reportable`) a space. Empty text is no title.
    fn osc_title(&mut self, text: &[u8]) {
        let title: String = String::from_utf8_lossy(text)
            .chars()
            .take(MAX_TITLE)
            .map(|character| {
                if crate::reportable::reportable(character) {
                    character
                } else {
                    ' '
                }
            })
            .collect();
        self.title = (!title.is_empty()).then_some(title);
    }

    /// `OSC 8 ; params ; URI`: the cells written after it are the link's,
    /// until one with an empty URI ends it. A URI of more than `MAX_URI`
    /// bytes, or any byte that is not printable ASCII, ends it too, as no
    /// link. Two links with the same nonempty `id=` parameter, of at most
    /// `MAX_LINK_ID` bytes, and URI are one link, however far apart their
    /// cells were written.
    fn osc_link(&mut self, rest: &[u8]) {
        let Some(split) = rest.iter().position(|&byte| byte == b';') else {
            return;
        };
        let (params, uri) = (rest.get(..split), rest.get(split.saturating_add(1)..));
        let (Some(params), Some(uri)) = (params, uri) else {
            return;
        };
        let printable = |bytes: &[u8]| bytes.iter().all(|byte| (0x21..=0x7e).contains(byte));
        if uri.is_empty() || uri.len() > MAX_URI || !printable(uri) {
            self.attributes.link = 0;
            return;
        }
        let Ok(uri) = std::str::from_utf8(uri) else {
            self.attributes.link = 0;
            return;
        };
        let key = params
            .split(|&byte| byte == b':')
            .find_map(|param| param.strip_prefix(b"id="))
            .filter(|key| key.len() <= MAX_LINK_ID)
            .unwrap_or_default();
        let known = (!key.is_empty())
            .then(|| {
                self.links
                    .iter()
                    .find(|link| link.key == key && link.uri == uri)
                    .map(|link| link.id)
            })
            .flatten();
        if let Some(id) = known {
            self.attributes.link = id;
            return;
        }
        let id = self.next_link;
        let Some(next) = id.checked_add(1) else {
            // Every id given: no more links, rather than a reused one.
            self.attributes.link = 0;
            return;
        };
        self.next_link = next;
        self.links.push_back(Link {
            id,
            key: key.to_vec(),
            uri: uri.to_owned(),
        });
        while self.links.len() > MAX_LINKS {
            self.links.pop_front();
        }
        self.attributes.link = id;
    }

    fn csi_byte(&mut self, mut csi: Csi, byte: u8) {
        match byte {
            b'0'..=b'9' if !csi.intermediate => {
                csi.digit(byte - b'0');
                self.parser = ParserState::Csi(csi);
            }
            b';' if !csi.intermediate => {
                csi.separator();
                self.parser = ParserState::Csi(csi);
            }
            b':' if !csi.intermediate => {
                csi.colon();
                self.parser = ParserState::Csi(csi);
            }
            b'?' if csi.count == 1
                && !csi.present.first().copied().unwrap_or(false)
                && !csi.private =>
            {
                csi.private = true;
                self.parser = ParserState::Csi(csi);
            }
            0x20..=0x2f => {
                csi.intermediate = true;
                self.parser = ParserState::Csi(csi);
            }
            0x00..=0x17 | 0x19 | 0x1c..=0x1f => {
                self.execute_c0(byte);
                self.parser = ParserState::Csi(csi);
            }
            0x30..=0x3f => {
                csi.overflow = true;
                self.parser = ParserState::Csi(csi);
            }
            0x40..=0x7e => {
                // Subparameters are SGR's alone; elsewhere a colon is a
                // sequence this profile does not know.
                if !csi.overflow
                    && !csi.intermediate
                    && (!csi.private || matches!(byte, b'h' | b'l'))
                    && (!csi.subparameters() || (byte == b'm' && !csi.private))
                {
                    self.dispatch_csi(&csi, byte);
                }
                self.parser = ParserState::Ground;
            }
            0x18 | 0x1a => self.parser = ParserState::Ground,
            0x1b => self.parser = ParserState::Escape,
            _ => self.parser = ParserState::Csi(csi),
        }
    }

    fn dispatch_csi(&mut self, csi: &Csi, final_byte: u8) {
        let count = usize::from(csi.value(0, 1).max(1));
        match final_byte {
            b'A' | b'k' => self.move_vertical(-1, count),
            b'B' | b'e' => self.move_vertical(1, count),
            b'C' | b'a' => self.move_horizontal(1, count),
            b'D' | b'j' => self.move_horizontal(-1, count),
            b'E' => {
                self.move_vertical(1, count);
                self.screen_mut().cursor_column = 0;
            }
            b'F' => {
                self.move_vertical(-1, count);
                self.screen_mut().cursor_column = 0;
            }
            b'G' | b'`' => self.set_column(usize::from(csi.value(0, 1).max(1) - 1)),
            b'H' | b'f' => {
                let row = usize::from(csi.value(0, 1).max(1) - 1);
                let column = usize::from(csi.value(1, 1).max(1) - 1);
                self.set_position(row, column);
            }
            b'd' => {
                let row = usize::from(csi.value(0, 1).max(1) - 1);
                let column = self.screen().cursor_column;
                self.set_position(row, column);
            }
            b'J' => {
                let attributes = self.attributes;
                let mode = csi.value(0, 0);
                if mode == 3 {
                    self.primary.clear_history();
                } else {
                    self.screen_mut().erase_in_display(mode, attributes);
                }
            }
            b'K' => {
                let attributes = self.attributes;
                self.screen_mut().erase_in_line(csi.value(0, 0), attributes);
            }
            b'@' => {
                let attributes = self.attributes;
                self.screen_mut().insert_characters(count, attributes);
            }
            b'P' => {
                let attributes = self.attributes;
                self.screen_mut().delete_characters(count, attributes);
            }
            b'X' => {
                let attributes = self.attributes;
                self.screen_mut().erase_characters(count, attributes);
            }
            b'L' => self.insert_lines(count),
            b'M' => self.delete_lines(count),
            b'S' => {
                let attributes = self.attributes;
                let history = self.records_history();
                let screen = self.screen_mut();
                screen.scroll_up(
                    screen.scroll_top,
                    screen.scroll_bottom,
                    count,
                    attributes,
                    history,
                );
            }
            b'T' => {
                let attributes = self.attributes;
                let screen = self.screen_mut();
                screen.scroll_down(screen.scroll_top, screen.scroll_bottom, count, attributes);
            }
            b'I' => {
                let bounded = count.min(self.columns());
                self.screen_mut().next_tab(bounded);
            }
            b'Z' => {
                let bounded = count.min(self.columns());
                self.screen_mut().previous_tab(bounded);
            }
            b'g' => self.clear_tabs(csi.value(0, 0)),
            b'm' => self.apply_sgr(csi),
            b'r' if !csi.private => self.set_margins(csi),
            b'h' => self.set_modes(csi, true),
            b'l' => self.set_modes(csi, false),
            b'n' if !csi.private => self.report_status(csi.value(0, 0)),
            b'c' if !csi.private => self.append_reply(b"\x1b[?1;0c"),
            b'b' => {
                if let Some(scalar) = self.last_printed {
                    let screen = self.screen();
                    let remaining = if screen.pending_wrap {
                        0
                    } else {
                        screen.columns.saturating_sub(screen.cursor_column)
                    };
                    for _ in 0..count.min(remaining) {
                        self.put_char(scalar);
                    }
                }
            }
            b's' => self.screen_mut().save_cursor(),
            b'u' => self.screen_mut().restore_cursor(),
            _ => {}
        }
    }

    fn move_vertical(&mut self, direction: isize, count: usize) {
        let screen = self.screen_mut();
        if direction < 0 {
            let minimum = if screen.cursor_row >= screen.scroll_top {
                screen.scroll_top
            } else {
                0
            };
            screen.cursor_row = screen.cursor_row.saturating_sub(count).max(minimum);
        } else {
            let maximum = if screen.cursor_row < screen.scroll_bottom {
                screen.scroll_bottom.saturating_sub(1)
            } else {
                screen.rows.saturating_sub(1)
            };
            screen.cursor_row = screen.cursor_row.saturating_add(count).min(maximum);
        }
        screen.pending_wrap = false;
    }

    fn move_horizontal(&mut self, direction: isize, count: usize) {
        let screen = self.screen_mut();
        if direction < 0 {
            screen.cursor_column = screen.cursor_column.saturating_sub(count);
        } else {
            screen.cursor_column = screen
                .cursor_column
                .saturating_add(count)
                .min(screen.columns.saturating_sub(1));
        }
        screen.pending_wrap = false;
    }

    fn set_column(&mut self, column: usize) {
        let screen = self.screen_mut();
        screen.cursor_column = column.min(screen.columns.saturating_sub(1));
        screen.pending_wrap = false;
    }

    fn set_position(&mut self, row: usize, column: usize) {
        let origin_mode = self.origin_mode;
        let screen = self.screen_mut();
        screen.cursor_row = if origin_mode {
            screen
                .scroll_top
                .saturating_add(row)
                .min(screen.scroll_bottom.saturating_sub(1))
        } else {
            row.min(screen.rows.saturating_sub(1))
        };
        screen.cursor_column = column.min(screen.columns.saturating_sub(1));
        screen.pending_wrap = false;
    }

    fn insert_lines(&mut self, count: usize) {
        let attributes = self.attributes;
        let screen = self.screen_mut();
        if screen.cursor_row >= screen.scroll_top && screen.cursor_row < screen.scroll_bottom {
            screen.scroll_down(screen.cursor_row, screen.scroll_bottom, count, attributes);
        }
        screen.pending_wrap = false;
    }

    fn delete_lines(&mut self, count: usize) {
        let attributes = self.attributes;
        let screen = self.screen_mut();
        if screen.cursor_row >= screen.scroll_top && screen.cursor_row < screen.scroll_bottom {
            screen.scroll_up(
                screen.cursor_row,
                screen.scroll_bottom,
                count,
                attributes,
                false,
            );
        }
        screen.pending_wrap = false;
    }

    fn clear_tabs(&mut self, mode: u16) {
        let screen = self.screen_mut();
        match mode {
            0 => {
                if let Some(tab) = screen.tabs.get_mut(screen.cursor_column) {
                    *tab = false;
                }
            }
            3 => screen.tabs.fill(false),
            _ => {}
        }
    }

    fn apply_sgr(&mut self, csi: &Csi) {
        let mut index = 0;
        while index < csi.count {
            // A subparameter no parameter took, after a form that changed
            // nothing, is no parameter of its own.
            if csi.sub(index) {
                index = index.saturating_add(1);
                continue;
            }
            let code = csi.value(index, 0);
            // A parameter's subparameters run to the next semicolon.
            let first = index.saturating_add(1);
            let mut end = first;
            while end < csi.count && csi.sub(end) {
                end = end.saturating_add(1);
            }
            if end > first {
                self.apply_sgr_subparameters(csi, code, first, end);
                index = end;
                continue;
            }
            match code {
                0 => {
                    self.attributes = Attributes {
                        link: self.attributes.link,
                        ..Attributes::default()
                    }
                }
                1 => self.attributes.bold = true,
                2 => self.attributes.faint = true,
                3 => self.attributes.italic = true,
                4 => self.attributes.underline = Underline::Single,
                7 => self.attributes.inverse = true,
                9 => self.attributes.strike = true,
                22 => {
                    self.attributes.bold = false;
                    self.attributes.faint = false;
                }
                23 => self.attributes.italic = false,
                24 => self.attributes.underline = Underline::None,
                27 => self.attributes.inverse = false,
                29 => self.attributes.strike = false,
                30..=37 => self.attributes.foreground = Color::Indexed((code - 30) as u8),
                39 => self.attributes.foreground = Color::Default,
                40..=47 => self.attributes.background = Color::Indexed((code - 40) as u8),
                49 => self.attributes.background = Color::Default,
                90..=97 => self.attributes.foreground = Color::Indexed((code - 90 + 8) as u8),
                100..=107 => self.attributes.background = Color::Indexed((code - 100 + 8) as u8),
                38 | 48 | 58 => {
                    let selector = csi.value(index.saturating_add(1), u16::MAX);
                    let remaining = csi.count.saturating_sub(index.saturating_add(1));
                    // Operands, or the parameter after them, with
                    // subparameters mix the forms: a form it does not know.
                    let mixed = |operands: usize| {
                        (index.saturating_add(1)..=index.saturating_add(operands).saturating_add(1))
                            .any(|at| csi.sub(at))
                    };
                    if selector == 5 {
                        let operands = remaining.min(2);
                        if operands == 2 && !mixed(operands) {
                            let value = csi.value(index.saturating_add(2), u16::MAX);
                            if let Ok(value) = u8::try_from(value) {
                                self.set_color(code, Color::Indexed(value));
                            }
                        }
                        index = index.saturating_add(operands);
                    } else if selector == 2 {
                        let operands = remaining.min(4);
                        if operands == 4 && !mixed(operands) {
                            let red = u8::try_from(csi.value(index.saturating_add(2), u16::MAX));
                            let green = u8::try_from(csi.value(index.saturating_add(3), u16::MAX));
                            let blue = u8::try_from(csi.value(index.saturating_add(4), u16::MAX));
                            if let (Ok(red), Ok(green), Ok(blue)) = (red, green, blue) {
                                self.set_color(code, Color::Rgb(red, green, blue));
                            }
                        }
                        index = index.saturating_add(operands);
                    } else {
                        index = index.saturating_add(remaining);
                    }
                }
                59 => self.attributes.underline_color = Color::Default,
                _ => {}
            }
            index += 1;
        }
    }

    /// One SGR parameter with the subparameters `first..end` after it:
    /// an underline's style (`4:n`), or a color, indexed (`38:5:n`) or
    /// direct (`38:2:r:g:b`, or `38:2:id:r:g:b` with a color space id,
    /// which is ignored, as foot ignores it and any fields after blue),
    /// an empty component being 0.
    /// Any other parameter with subparameters, or a form that is none of
    /// these, changes nothing.
    fn apply_sgr_subparameters(&mut self, csi: &Csi, code: u16, first: usize, end: usize) {
        let count = end.saturating_sub(first);
        let at = |offset: usize, default: u16| csi.value(first.saturating_add(offset), default);
        match code {
            4 if count == 1 => {
                if let Some(style) = Underline::from_style(at(0, u16::MAX)) {
                    self.attributes.underline = style;
                }
            }
            38 | 48 | 58 => {
                let rgb = |offset: usize| {
                    let red = u8::try_from(at(offset, 0)).ok()?;
                    let green = u8::try_from(at(offset.saturating_add(1), 0)).ok()?;
                    let blue = u8::try_from(at(offset.saturating_add(2), 0)).ok()?;
                    Some(Color::Rgb(red, green, blue))
                };
                let color = match (at(0, u16::MAX), count) {
                    (5, 2) => u8::try_from(at(1, u16::MAX)).ok().map(Color::Indexed),
                    (2, 4) => rgb(1),
                    // T.416's fields after blue, as foot, are ignored.
                    (2, count) if count >= 5 => rgb(2),
                    _ => None,
                };
                if let Some(color) = color {
                    self.set_color(code, color);
                }
            }
            _ => {}
        }
    }

    /// `38`, `48` or `58`'s color: the foreground, background or underline.
    fn set_color(&mut self, code: u16, color: Color) {
        match code {
            38 => self.attributes.foreground = color,
            48 => self.attributes.background = color,
            58 => self.attributes.underline_color = color,
            _ => {}
        }
    }

    fn set_margins(&mut self, csi: &Csi) {
        let rows = self.rows();
        let top = usize::from(csi.value(0, 1).max(1) - 1);
        let bottom = usize::from(csi.value(1, u16::try_from(rows).unwrap_or(u16::MAX)));
        let bottom = if bottom == 0 { rows } else { bottom.min(rows) };
        if top.saturating_add(1) >= bottom {
            return;
        }
        let origin_mode = self.origin_mode;
        let screen = self.screen_mut();
        screen.scroll_top = top;
        screen.scroll_bottom = bottom;
        screen.cursor_row = if origin_mode { top } else { 0 };
        screen.cursor_column = 0;
        screen.pending_wrap = false;
    }

    fn set_modes(&mut self, csi: &Csi, enabled: bool) {
        if !csi.private {
            return;
        }
        for index in 0..csi.count {
            match csi.value(index, 0) {
                1 => self.application_cursor = enabled,
                2004 => self.bracketed_paste = enabled,
                9 => self.track_mouse(MouseTracking::Press, enabled),
                1000 => self.track_mouse(MouseTracking::Click, enabled),
                1002 => self.track_mouse(MouseTracking::Drag, enabled),
                1003 => self.track_mouse(MouseTracking::Motion, enabled),
                1006 => self.mouse.sgr = enabled,
                6 => {
                    self.origin_mode = enabled;
                    let row = if enabled { self.screen().scroll_top } else { 0 };
                    let screen = self.screen_mut();
                    screen.cursor_row = row;
                    screen.cursor_column = 0;
                    screen.pending_wrap = false;
                }
                7 => self.auto_wrap = enabled,
                25 => self.cursor_visible = enabled,
                1048 if enabled => self.save_dec_state(),
                1048 => self.restore_dec_state(),
                1049 => self.set_alternate(enabled),
                _ => {}
            }
        }
    }

    fn track_mouse(&mut self, tracking: MouseTracking, enabled: bool) {
        if enabled {
            self.mouse.tracking = tracking;
        } else if self.mouse.tracking == tracking {
            self.mouse.tracking = MouseTracking::Off;
        }
    }

    fn set_alternate(&mut self, enabled: bool) {
        if enabled == self.alternate_active {
            return;
        }
        if enabled {
            self.save_dec_state();
            self.alternate.reset(Attributes::default());
            self.alternate_active = true;
        } else {
            self.alternate_active = false;
            self.restore_dec_state();
        }
        self.last_printed = None;
        // A prompt mark is the screen's it came on.
        self.prompt_pending = false;
    }

    fn report_status(&mut self, status: u16) {
        match status {
            5 => self.append_reply(b"\x1b[0n"),
            6 => {
                let screen = self.screen();
                let row = if self.origin_mode {
                    screen.cursor_row.saturating_sub(screen.scroll_top)
                } else {
                    screen.cursor_row
                };
                let response = format!(
                    "\x1b[{};{}R",
                    row.saturating_add(1),
                    screen.cursor_column.saturating_add(1)
                );
                self.append_reply(response.as_bytes());
            }
            _ => {}
        }
    }

    fn append_reply(&mut self, bytes: &[u8]) {
        // An empty reply is not one, and admitting it would add a boundary
        // without adding a byte — the one way this storage could grow while
        // the ceiling below saw nothing. No caller sends one; this is what
        // keeps that a property rather than a habit.
        if bytes.is_empty() {
            return;
        }
        // The boundary is charged too. td-term/DESIGN.md §2 caps the reply storage, and an
        // index of one `usize` per reply is storage: uncharged, a child
        // spamming the four-byte status report would hold the ceiling in
        // bytes and twice it again in offsets.
        let stored = self
            .reply_ends
            .len()
            .checked_mul(REPLY_OVERHEAD)
            .and_then(|index| index.checked_add(self.replies.len()));
        let next = stored
            .and_then(|stored| stored.checked_add(bytes.len()))
            .and_then(|next| next.checked_add(REPLY_OVERHEAD));
        if next.is_some_and(|next| next <= MAX_REPLY_BYTES) {
            self.replies.extend_from_slice(bytes);
            self.reply_ends.push(self.replies.len());
        } else {
            self.bell_pending = true;
        }
    }

    fn reset_model(&mut self) {
        self.attributes = Attributes::default();
        self.links.clear();
        self.prompt_pending = false;
        self.primary.reset(self.attributes);
        self.alternate.reset(self.attributes);
        self.alternate_active = false;
        self.origin_mode = false;
        self.auto_wrap = true;
        self.cursor_visible = true;
        self.application_cursor = false;
        self.bracketed_paste = false;
        self.mouse = MouseMode::default();
        self.g0 = Charset::Ascii;
        self.g1 = Charset::Ascii;
        self.use_g1 = false;
        self.dec_primary = None;
        self.dec_alternate = None;
        self.last_printed = None;
        self.utf8.reset();
    }

    pub fn resize(&mut self, rows: usize, columns: usize) -> Result<(), String> {
        checked_cell_count(rows, columns)?;
        let attributes = self.attributes;
        let old_columns = self.columns();
        let primary_offset =
            self.primary
                .resize(rows, columns, attributes, !self.alternate_active)?;
        let alternate_offset = self.alternate.resize(rows, columns, attributes, false)?;
        if let Some(saved) = self.dec_primary.as_mut() {
            saved.cursor = saved
                .cursor
                .resized(primary_offset, rows, old_columns, columns);
        }
        if let Some(saved) = self.dec_alternate.as_mut() {
            saved.cursor = saved
                .cursor
                .resized(alternate_offset, rows, old_columns, columns);
        }
        Ok(())
    }
}

pub fn selftest() -> Result<(), String> {
    let mut terminal = Terminal::new(2, 4)?;
    terminal.feed(b"td\x1b[31m!\x1b[6n\x07");
    let replies = terminal.take_replies();
    let bell = terminal.take_bell();
    if terminal.rows() != 2
        || terminal.columns() != 4
        || terminal.row_text(0)? != "td! "
        || terminal.cursor() != (0, 3, false)
        || terminal.mode("autowrap") != Some(true)
        || replies.concat() != b"\x1b[1;4R"
        || !bell
        || terminal.history_cells() != 0
        || terminal.cell(0, 2).map(|cell| cell.attributes.foreground) != Some(Color::Indexed(1))
    {
        return Err("terminal model selftest did not preserve state".into());
    }
    terminal.resize(3, 5)?;
    if terminal.row_text(0)? != "td!  " {
        return Err("terminal resize selftest did not preserve cells".into());
    }
    Ok(())
}

#[cfg(test)]
#[path = "vt_spec.rs"]
mod spec;

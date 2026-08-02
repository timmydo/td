use std::collections::VecDeque;

const MAX_SCREEN_CELLS: usize = 1_048_576;
const MAX_SCREEN_BYTES: usize = 16 * 1024 * 1024;
const MAX_HISTORY_CELLS: usize = 1_048_576;
const MAX_HISTORY_BYTES: usize = 16 * 1024 * 1024;
const MAX_HISTORY_LINES: usize = 16_384;
const HISTORY_ALLOCATOR_MARGIN: usize = 1024 * 1024;
const MAX_CSI_PARAMS: usize = 32;
const MAX_REPLY_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Color {
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Attributes {
    pub bold: bool,
    pub faint: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
    pub strike: bool,
    pub foreground: Color,
    pub background: Color,
}

impl Default for Attributes {
    fn default() -> Self {
        Self {
            bold: false,
            faint: false,
            italic: false,
            underline: false,
            inverse: false,
            strike: false,
            foreground: Color::Default,
            background: Color::Default,
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
pub(crate) struct Cell {
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
                self.pending_wrap
                    && columns == old_columns
                    && column.saturating_add(1) == columns,
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
        for source in source_cells {
            if self.arena.len() < self.max_cells {
                self.arena.push(*source);
            } else if let Some(target) = self.arena.get_mut(self.write) {
                *target = *source;
            }
            self.write = self.write.saturating_add(1);
            if self.write == self.max_cells {
                self.write = 0;
            }
        }
        self.cells = self.cells.saturating_add(columns);
        self.lines.push_back(HistoryLine {
            start: line_start,
            length: columns,
            wrapped,
        });
    }

    fn clear(&mut self) {
        self.lines.clear();
        self.arena.clear();
        self.write = 0;
        self.cells = 0;
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

    #[cfg(test)]
    fn line_cell(&self, line: usize, column: usize) -> Option<Cell> {
        let line = self.lines.get(line)?;
        if column >= line.length || self.max_cells == 0 {
            return None;
        }
        let index = line.start.saturating_add(column) % self.max_cells;
        self.arena.get(index).copied()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Screen {
    rows: usize,
    columns: usize,
    cells: Vec<Cell>,
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

fn checked_cell_count(rows: usize, columns: usize) -> Result<usize, String> {
    if rows == 0 || columns == 0 {
        return Err("terminal dimensions must be nonzero".into());
    }
    if rows > crate::MAX_UI_DIMENSION || columns > crate::MAX_UI_DIMENSION {
        return Err(format!(
            "terminal dimensions {rows}x{columns} exceed {}",
            crate::MAX_UI_DIMENSION
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

    fn index(&self, row: usize, column: usize) -> Option<usize> {
        if row >= self.rows || column >= self.columns {
            return None;
        }
        row.checked_mul(self.columns)?.checked_add(column)
    }

    fn cell(&self, row: usize, column: usize) -> Option<Cell> {
        self.index(row, column)
            .and_then(|index| self.cells.get(index))
            .copied()
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
            let Some(cell_start) = row.checked_mul(self.columns) else {
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
        if let Some(wrapped) = self.wrapped_rows.get_mut(row) {
            *wrapped = false;
        }
    }

    fn clear_segment(&mut self, row: usize, start: usize, end: usize, attributes: Attributes) {
        let bounded_end = end.min(self.columns);
        for column in start.min(bounded_end)..bounded_end {
            self.set_cell(row, column, Cell::blank(attributes));
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
        }
        let Some(source_start) = top.saturating_add(count).checked_mul(self.columns) else {
            return;
        };
        let Some(source_end) = bottom.checked_mul(self.columns) else {
            return;
        };
        let Some(destination) = top.checked_mul(self.columns) else {
            return;
        };
        self.cells
            .copy_within(source_start..source_end, destination);
        let Some(clear_start) = bottom.saturating_sub(count).checked_mul(self.columns) else {
            return;
        };
        if let Some(cells) = self.cells.get_mut(clear_start..source_end) {
            cells.fill(Cell::blank(attributes));
        }
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
        let Some(source_start) = top.checked_mul(self.columns) else {
            return;
        };
        let Some(source_end) = bottom.saturating_sub(count).checked_mul(self.columns) else {
            return;
        };
        let Some(destination) = top.saturating_add(count).checked_mul(self.columns) else {
            return;
        };
        self.cells
            .copy_within(source_start..source_end, destination);
        if let Some(cells) = self.cells.get_mut(source_start..destination) {
            cells.fill(Cell::blank(attributes));
        }
        self.wrapped_rows
            .copy_within(top..bottom.saturating_sub(count), top.saturating_add(count));
        if let Some(rows) = self.wrapped_rows.get_mut(top..top.saturating_add(count)) {
            rows.fill(false);
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
        let Some(row_start) = self.cursor_row.checked_mul(self.columns) else {
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
        self.damage(self.cursor_row);
        self.pending_wrap = false;
    }

    fn delete_characters(&mut self, count: usize, attributes: Attributes) {
        let remaining = self.columns.saturating_sub(self.cursor_column);
        let count = count.max(1).min(remaining);
        if count == 0 {
            return;
        }
        let Some(row_start) = self.cursor_row.checked_mul(self.columns) else {
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
        let mut wrapped_rows = vec![false; rows];
        for row in 0..copy_rows {
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Terminal {
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
    g0: Charset,
    g1: Charset,
    use_g1: bool,
    dec_primary: Option<SavedState>,
    dec_alternate: Option<SavedState>,
    last_printed: Option<char>,
    replies: Vec<u8>,
    bell_pending: bool,
}

impl Terminal {
    pub(crate) fn new(rows: usize, columns: usize) -> Result<Self, String> {
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
            g0: Charset::Ascii,
            g1: Charset::Ascii,
            use_g1: false,
            dec_primary: None,
            dec_alternate: None,
            last_printed: None,
            replies: Vec::new(),
            bell_pending: false,
        })
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
        self.attributes = saved.attributes;
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

    pub(crate) fn rows(&self) -> usize {
        self.screen().rows
    }

    pub(crate) fn columns(&self) -> usize {
        self.screen().columns
    }

    pub(crate) fn cursor(&self) -> (usize, usize, bool) {
        let screen = self.screen();
        (screen.cursor_row, screen.cursor_column, screen.pending_wrap)
    }

    pub(crate) fn cell(&self, row: usize, column: usize) -> Option<Cell> {
        self.screen().cell(row, column)
    }

    pub(crate) fn row_text(&self, row: usize) -> Result<String, String> {
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

    pub(crate) fn mode(&self, name: &str) -> Option<bool> {
        match name {
            "alternate-screen" => Some(self.alternate_active),
            "application-cursor" => Some(self.application_cursor),
            "autowrap" => Some(self.auto_wrap),
            "cursor-visible" => Some(self.cursor_visible),
            "origin" => Some(self.origin_mode),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn replies(&self) -> &[u8] {
        &self.replies
    }

    pub(crate) fn take_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.replies)
    }

    pub(crate) fn take_bell(&mut self) -> bool {
        std::mem::take(&mut self.bell_pending)
    }

    pub(crate) fn history_cells(&self) -> usize {
        self.primary.history.cells
    }

    pub(crate) fn feed(&mut self, bytes: &[u8]) {
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
        let attributes = self.attributes;
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
        self.parser = ParserState::String(kind);
    }

    fn string_byte(&mut self, kind: StringKind, byte: u8) {
        if matches!(byte, 0x07 | 0x18 | 0x1a) {
            self.parser = ParserState::Ground;
            return;
        }
        if byte == 0x1b {
            self.parser = ParserState::Escape;
            return;
        }
        self.parser = ParserState::String(kind);
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
                if !csi.overflow
                    && !csi.intermediate
                    && (!csi.private || matches!(byte, b'h' | b'l'))
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
            let code = csi.value(index, 0);
            match code {
                0 => self.attributes = Attributes::default(),
                1 => self.attributes.bold = true,
                2 => self.attributes.faint = true,
                3 => self.attributes.italic = true,
                4 => self.attributes.underline = true,
                7 => self.attributes.inverse = true,
                9 => self.attributes.strike = true,
                22 => {
                    self.attributes.bold = false;
                    self.attributes.faint = false;
                }
                23 => self.attributes.italic = false,
                24 => self.attributes.underline = false,
                27 => self.attributes.inverse = false,
                29 => self.attributes.strike = false,
                30..=37 => self.attributes.foreground = Color::Indexed((code - 30) as u8),
                39 => self.attributes.foreground = Color::Default,
                40..=47 => self.attributes.background = Color::Indexed((code - 40) as u8),
                49 => self.attributes.background = Color::Default,
                90..=97 => self.attributes.foreground = Color::Indexed((code - 90 + 8) as u8),
                100..=107 => self.attributes.background = Color::Indexed((code - 100 + 8) as u8),
                38 | 48 | 58 => {
                    let foreground = code == 38;
                    let selector = csi.value(index.saturating_add(1), u16::MAX);
                    let remaining = csi.count.saturating_sub(index.saturating_add(1));
                    if selector == 5 {
                        let operands = remaining.min(2);
                        if code != 58 && operands == 2 {
                            let value = csi.value(index.saturating_add(2), u16::MAX);
                            if let Ok(value) = u8::try_from(value) {
                                if foreground {
                                    self.attributes.foreground = Color::Indexed(value);
                                } else {
                                    self.attributes.background = Color::Indexed(value);
                                }
                            }
                        }
                        index = index.saturating_add(operands);
                    } else if selector == 2 {
                        let operands = remaining.min(4);
                        if code != 58 && operands == 4 {
                            let red = u8::try_from(csi.value(index.saturating_add(2), u16::MAX));
                            let green = u8::try_from(csi.value(index.saturating_add(3), u16::MAX));
                            let blue = u8::try_from(csi.value(index.saturating_add(4), u16::MAX));
                            if let (Ok(red), Ok(green), Ok(blue)) = (red, green, blue) {
                                let color = Color::Rgb(red, green, blue);
                                if foreground {
                                    self.attributes.foreground = color;
                                } else {
                                    self.attributes.background = color;
                                }
                            }
                        }
                        index = index.saturating_add(operands);
                    } else {
                        index = index.saturating_add(remaining);
                    }
                }
                59 => {}
                _ => {}
            }
            index += 1;
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
        let Some(next) = self.replies.len().checked_add(bytes.len()) else {
            self.bell_pending = true;
            return;
        };
        if next <= MAX_REPLY_BYTES {
            self.replies.extend_from_slice(bytes);
        } else {
            self.bell_pending = true;
        }
    }

    fn reset_model(&mut self) {
        self.attributes = Attributes::default();
        self.primary.reset(self.attributes);
        self.alternate.reset(self.attributes);
        self.alternate_active = false;
        self.origin_mode = false;
        self.auto_wrap = true;
        self.cursor_visible = true;
        self.application_cursor = false;
        self.g0 = Charset::Ascii;
        self.g1 = Charset::Ascii;
        self.use_g1 = false;
        self.dec_primary = None;
        self.dec_alternate = None;
        self.last_printed = None;
        self.utf8.reset();
    }

    pub(crate) fn resize(&mut self, rows: usize, columns: usize) -> Result<(), String> {
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

pub(crate) fn selftest() -> Result<(), String> {
    let mut terminal = Terminal::new(2, 4)?;
    terminal.feed(b"td\x1b[31m!\x1b[6n\x07");
    let replies = terminal.take_replies();
    let bell = terminal.take_bell();
    if terminal.rows() != 2
        || terminal.columns() != 4
        || terminal.row_text(0)? != "td! "
        || terminal.cursor() != (0, 3, false)
        || terminal.mode("autowrap") != Some(true)
        || replies != b"\x1b[1;4R"
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
#[path = "term_spec.rs"]
mod spec;

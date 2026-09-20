//! The cell screen: a grid of styled scalars laid over a surface in the
//! toolkit's 8x16 cells, for a program that draws in rows and columns —
//! td-mail and td-news, which drew a terminal and now draw this. The
//! screen is a [`Composition`]: it streams one fill per run of cells that
//! share a ground and one glyph per non-blank cell, so a raster paints it
//! and `driven::text` reads it back. With it come the key vocabulary such
//! a program reads, translated from the keyboard's chords, and the input
//! event its window delivers. Nothing here reads a clock, a descriptor or
//! the environment.

use crate::raster::{Composition, Draw, Error, GlyphStyle, Primitive, Rect, Surface, Weight};
use crate::{CELL_HEIGHT, CELL_WIDTH};

/// The default ground and ink, the toolkit's paper and ink.
pub const PAPER: u32 = crate::raster::PAPER;
pub const INK: u32 = crate::raster::INK;

/// A cell's colours and weight. `reversed` swaps ink and ground, which is
/// what a program without a theme uses for its selection and status rows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Style {
    pub ink: u32,
    pub background: u32,
    pub weight: Weight,
}

impl Default for Style {
    fn default() -> Self {
        Self::new(INK, PAPER)
    }
}

impl Style {
    pub const fn new(ink: u32, background: u32) -> Self {
        Self {
            ink,
            background,
            weight: Weight::Regular,
        }
    }
    pub const fn bold(self) -> Self {
        Self {
            weight: Weight::Medium,
            ..self
        }
    }
    pub const fn reversed(self) -> Self {
        Self {
            ink: self.background,
            background: self.ink,
            weight: self.weight,
        }
    }
    fn glyph(self) -> GlyphStyle {
        GlyphStyle {
            ink: self.ink,
            background: self.background,
            weight: self.weight,
        }
    }
}

/// One cell: a scalar in a style. A blank is a space in the ground.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Cell {
    pub scalar: char,
    pub style: Style,
}

/// The grid over a surface: `height / (16 * scale)` rows of
/// `width / (8 * scale)` columns, the remainder painted in the ground of
/// the last clear. A surface smaller than a cell is a screen of no cells.
pub struct Screen {
    surface: Surface,
    rows: usize,
    columns: usize,
    cells: Vec<Cell>,
    ground: u32,
}

impl Screen {
    /// A screen laid out for `surface`, every cell blank in `style`.
    pub fn new(surface: Surface, style: Style) -> Result<Self, Error> {
        let mut screen = Self {
            surface,
            rows: 0,
            columns: 0,
            cells: Vec::new(),
            ground: style.background,
        };
        screen.resize(surface, style)?;
        Ok(screen)
    }

    /// Lays the grid out for a new surface and clears it in `style`; the
    /// cell storage is reused when it fits. The surface's own ceiling
    /// bounds the grid: a frame the raster admits is at most
    /// `MAX_FRAME_BYTES / 4` pixels, a cell is 128 or more.
    pub fn resize(&mut self, surface: Surface, style: Style) -> Result<(), Error> {
        surface.check()?;
        let scale = surface.scale.value();
        let mut columns = surface.width / (CELL_WIDTH * scale);
        let mut rows = surface.height / (CELL_HEIGHT * scale);
        // A surface under a cell on either axis holds no cell: a grid with
        // rows but no columns would be one a program could not lay out.
        if rows == 0 || columns == 0 {
            rows = 0;
            columns = 0;
        }
        let count = rows.checked_mul(columns).ok_or(Error::Limit)?;
        self.surface = surface;
        self.rows = rows;
        self.columns = columns;
        self.cells.clear();
        self.cells.resize(count, blank(style));
        self.ground = style.background;
        Ok(())
    }

    pub fn surface(&self) -> Surface {
        self.surface
    }
    pub fn rows(&self) -> usize {
        self.rows
    }
    pub fn columns(&self) -> usize {
        self.columns
    }
    /// The ground the remainder outside the grid is painted in: the
    /// background of the style `clear` last painted; `clear_row` leaves it.
    pub fn ground(&self) -> u32 {
        self.ground
    }

    /// Every cell blank in `style`, whose background becomes the ground.
    pub fn clear(&mut self, style: Style) {
        self.cells.fill(blank(style));
        self.ground = style.background;
    }

    /// Fills the cells of `row` from `column` to the right edge with blanks
    /// in `style`, the ground unchanged; a row or column off the grid fills
    /// nothing.
    pub fn clear_row(&mut self, row: usize, column: usize, style: Style) {
        if row >= self.rows || column >= self.columns {
            return;
        }
        let start = row * self.columns + column;
        let end = (row + 1) * self.columns;
        if let Some(cells) = self.cells.get_mut(start..end) {
            cells.fill(blank(style));
        }
    }

    /// Puts one scalar at a cell; a control scalar becomes the
    /// replacement character, as the raster's text run draws it. Off the
    /// grid nothing is written and `false` is returned.
    pub fn put(&mut self, row: usize, column: usize, scalar: char, style: Style) -> bool {
        if row >= self.rows || column >= self.columns {
            return false;
        }
        let Some(cell) = self.cells.get_mut(row * self.columns + column) else {
            return false;
        };
        *cell = Cell {
            scalar: if scalar.is_control() {
                '\u{fffd}'
            } else {
                scalar
            },
            style,
        };
        true
    }

    /// Writes `text` one scalar per cell from `(row, column)` rightward,
    /// stopping at the right edge, and returns how many cells it wrote.
    pub fn write(&mut self, row: usize, column: usize, text: &str, style: Style) -> usize {
        let mut written = 0;
        for scalar in text.chars() {
            if !self.put(row, column + written, scalar, style) {
                break;
            }
            written += 1;
        }
        written
    }

    pub fn cell(&self, row: usize, column: usize) -> Option<Cell> {
        if row >= self.rows || column >= self.columns {
            return None;
        }
        self.cells.get(row * self.columns + column).copied()
    }

    /// The scalars of one row, trailing spaces trimmed whatever their
    /// style; another space-like scalar in the last cells is kept, since a
    /// program wrote it.
    pub fn line(&self, row: usize) -> Option<String> {
        if row >= self.rows {
            return None;
        }
        let start = row * self.columns;
        let cells = self.cells.get(start..start + self.columns)?;
        let kept = cells
            .iter()
            .rposition(|cell| cell.scalar != ' ')
            .map_or(0, |last| last + 1);
        Some(cells.iter().take(kept).map(|cell| cell.scalar).collect())
    }

    /// The cell under a surface pixel, or none outside the grid.
    pub fn hit(&self, x: i64, y: i64) -> Option<(usize, usize)> {
        if x < 0 || y < 0 {
            return None;
        }
        let scale = self.surface.scale.value();
        let column = usize::try_from(x).ok()? / (CELL_WIDTH * scale);
        let row = usize::try_from(y).ok()? / (CELL_HEIGHT * scale);
        (row < self.rows && column < self.columns).then_some((row, column))
    }

    /// The pixel rectangle of one cell.
    fn rect(&self, row: usize, column: usize, span: usize) -> Rect {
        let scale = self.surface.scale.value();
        let cw = CELL_WIDTH * scale;
        let ch = CELL_HEIGHT * scale;
        Rect {
            x: (column * cw) as i64,
            y: (row * ch) as i64,
            width: (span * cw) as u32,
            height: ch as u32,
        }
    }
}

fn blank(style: Style) -> Cell {
    Cell { scalar: ' ', style }
}

impl Composition for Screen {
    fn surface(&self) -> Surface {
        self.surface
    }

    /// The ground over the whole surface, then per row one fill for each
    /// run of cells whose background is not the ground and one glyph for
    /// each cell that is not a blank, every draw clipped to `damage`.
    fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        let Some(clip) = self.surface.bounds().intersection(damage) else {
            return;
        };
        sink(Draw {
            clip,
            primitive: Primitive::Fill {
                rect: self.surface.bounds(),
                color: self.ground,
            },
        });
        let scale = self.surface.scale.value();
        let cw = (CELL_WIDTH * scale) as i64;
        let ch = (CELL_HEIGHT * scale) as i64;
        // The rows and columns the damage touches; the clip is inside the
        // surface, so both are within the grid or just past it.
        let first_row = usize::try_from(clip.y / ch).unwrap_or(0);
        let end_row = usize::try_from((clip.y + i64::from(clip.height) + ch - 1) / ch)
            .unwrap_or(self.rows)
            .min(self.rows);
        let first_column = usize::try_from(clip.x / cw).unwrap_or(0);
        let end_column = usize::try_from((clip.x + i64::from(clip.width) + cw - 1) / cw)
            .unwrap_or(self.columns)
            .min(self.columns);
        for row in first_row..end_row {
            let Some(cells) = self.cells.get(row * self.columns..(row + 1) * self.columns) else {
                continue;
            };
            let mut column = first_column;
            while column < end_column {
                let Some(first) = cells.get(column) else {
                    break;
                };
                let background = first.style.background;
                let mut span = 1;
                while column + span < end_column
                    && cells
                        .get(column + span)
                        .is_some_and(|cell| cell.style.background == background)
                {
                    span += 1;
                }
                if background != self.ground {
                    sink(Draw {
                        clip,
                        primitive: Primitive::Fill {
                            rect: self.rect(row, column, span),
                            color: background,
                        },
                    });
                }
                for (offset, cell) in cells.iter().skip(column).take(span).enumerate() {
                    if cell.scalar == ' ' {
                        continue;
                    }
                    let rect = self.rect(row, column + offset, 1);
                    sink(Draw {
                        clip,
                        primitive: Primitive::Glyph {
                            x: rect.x,
                            y: rect.y,
                            scalar: cell.scalar,
                            style: cell.style.glyph(),
                        },
                    });
                }
                column += span;
            }
        }
    }
}

/// A key as a screen program reads it: a printable scalar, or a named
/// key. The modifiers travel beside it in a [`Press`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Key {
    Char(char),
    Enter,
    Escape,
    Backspace,
    Tab,
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    Home,
    End,
    Insert,
    Delete,
    Function(u8),
}

/// One translated keypress: the key with the modifiers the chord named.
/// Shift is folded into a printable scalar by the keyboard, so `shift` is
/// set only for a named key or beside control or alt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Press {
    pub key: Key,
    pub control: bool,
    pub alt: bool,
    pub shift: bool,
}

impl Press {
    pub const fn plain(key: Key) -> Self {
        Self {
            key,
            control: false,
            alt: false,
            shift: false,
        }
    }
    /// The scalar of a press with no modifiers, if it is one.
    pub fn plain_char(self) -> Option<char> {
        match self.key {
            Key::Char(c) if !self.control && !self.alt && !self.shift => Some(c),
            _ => None,
        }
    }
}

/// The keyboard's chord as a press: `C-`, `M-` and `S-` in that order,
/// then one printable ASCII scalar, the bare space the keyboard emits
/// for an unmodified space bar, `Space` (its spelling under a modifier),
/// a key name, or `F1` through `F12`. Anything else, a chord over
/// `driven::KEY_BYTES` included, is none.
pub fn press(chord: &str) -> Option<Press> {
    if chord.is_empty() || chord.len() > crate::driven::KEY_BYTES {
        return None;
    }
    let mut rest = chord;
    let mut modifiers = [false; 3];
    for (index, prefix) in ["C-", "M-", "S-"].iter().enumerate() {
        if rest.len() > prefix.len() {
            if let Some(after) = rest.strip_prefix(prefix) {
                if let Some(flag) = modifiers.get_mut(index) {
                    *flag = true;
                }
                rest = after;
            }
        }
    }
    let modified = modifiers.iter().any(|flag| *flag);
    let key = match rest {
        "Space" => Key::Char(' '),
        "Return" => Key::Enter,
        "Escape" => Key::Escape,
        "Backspace" => Key::Backspace,
        "Tab" => Key::Tab,
        "Up" => Key::Up,
        "Down" => Key::Down,
        "Left" => Key::Left,
        "Right" => Key::Right,
        "PageUp" => Key::PageUp,
        "PageDown" => Key::PageDown,
        "Home" => Key::Home,
        "End" => Key::End,
        "Insert" => Key::Insert,
        "Delete" => Key::Delete,
        _ => {
            let mut scalars = rest.chars();
            match (scalars.next(), scalars.next()) {
                // The bare space is the unmodified space bar's spelling
                // alone; under a modifier the keyboard spells it `Space`.
                (Some(c), None) if c.is_ascii_graphic() || (c == ' ' && !modified) => {
                    Key::Char(c)
                }
                _ => {
                    // Digits only, and none leading: `F+5` and `F01` are
                    // spellings the keyboard never emits.
                    let digits = rest.strip_prefix('F')?;
                    if digits.is_empty()
                        || digits.starts_with('0')
                        || !digits.bytes().all(|byte| byte.is_ascii_digit())
                    {
                        return None;
                    }
                    let number: u8 = digits.parse().ok()?;
                    if !(1..=12).contains(&number) {
                        return None;
                    }
                    Key::Function(number)
                }
            }
        }
    };
    let [control, alt, shift] = modifiers;
    Some(Press {
        key,
        control,
        alt,
        shift,
    })
}

/// What a screen window delivers to its program: a keypress, the left
/// button pressed on a cell, whole cells of wheel travel (positive is
/// down and right), the grid laid out anew, keyboard focus, or the
/// compositor asking the window to close.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Input {
    Key(Press),
    Click { row: usize, column: usize },
    Wheel { rows: isize, columns: isize },
    Resize { rows: usize, columns: usize },
    Focus(bool),
    Close,
}

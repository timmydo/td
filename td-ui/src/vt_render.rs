//! The terminal's model-to-pixels rendering (td-term/DESIGN.md, "Font,
//! keyboard, and rendering").
//!
//! Pure: it reads a snapshot and writes XRGB8888. It opens nothing, owns
//! nothing, and allocates nothing in the cell loop but what an outline face
//! covers into its atlas the first time it draws a glyph, so the exact P6
//! PPM oracle can drive it without a compositor, a framebuffer, or a child
//! process.

use crate::atlas::{Entry, Slot, PAGE_WIDTH};
use crate::face::{Face, Sizing};
use crate::font::Font;
use crate::raster::{self, Scale, Surface};
use crate::vt::{Attributes, Cell, Color, Terminal};

pub const BYTES_PER_PIXEL: usize = 4;

/// foot's default first sixteen (1.15 and later: starlight). Only these
/// are a table: 16..232 is xterm's 6x6x6 cube and 232..256 its grey ramp,
/// which foot keeps too, both computed from the arithmetic that defines
/// them so the entries cannot drift from it.
const BASE: [[u8; 3]; 16] = [
    [0x24, 0x24, 0x24],
    [0xf6, 0x2b, 0x5a],
    [0x47, 0xb4, 0x13],
    [0xe3, 0xc4, 0x01],
    [0x24, 0xac, 0xd4],
    [0xf2, 0xaf, 0xfd],
    [0x13, 0xc2, 0x99],
    [0xe6, 0xe6, 0xe6],
    [0x61, 0x61, 0x61],
    [0xff, 0x4d, 0x51],
    [0x35, 0xd4, 0x50],
    [0xe9, 0xe8, 0x36],
    [0x5d, 0xc5, 0xf8],
    [0xfe, 0xab, 0xf2],
    [0x24, 0xdf, 0xc4],
    [0xff, 0xff, 0xff],
];
const CUBE_START: usize = 16;
const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
const RAMP_START: usize = 232;
const RAMP_BASE: u8 = 8;
const RAMP_STEP: u8 = 10;

/// Default ink is its own pair, as foot's is, so `SGR 39`/`49` restore it
/// rather than a palette entry: the foot.ini td-term replaces sets this.
const DEFAULT_FOREGROUND: [u8; 3] = [0xdc, 0xdc, 0xcc];
const DEFAULT_BACKGROUND: [u8; 3] = [0x22, 0x22, 0x22];

/// Underline sits two rows above the cell's bottom edge and strike at its
/// middle. Both are fixed, so a rendition's presentation is a property of
/// the cell geometry rather than of the glyph in it.
const UNDERLINE_INSET: usize = 2;

/// What a row past the end of the model shows. Written out rather than
/// `Attributes::default()` so adding a rendition is a compile error here,
/// where the renderer has to decide what to do with it.
const BLANK: Cell = Cell {
    scalar: ' ',
    attributes: Attributes {
        bold: false,
        faint: false,
        italic: false,
        underline: false,
        inverse: false,
        strike: false,
        foreground: Color::Default,
        background: Color::Default,
    },
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Palette {
    entries: [[u8; 3]; 256],
    foreground: [u8; 3],
    background: [u8; 3],
}

impl Palette {
    pub fn pinned() -> Self {
        let mut entries = [[0u8; 3]; 256];
        for (index, slot) in entries.iter_mut().enumerate() {
            *slot = if index < CUBE_START {
                BASE.get(index).copied().unwrap_or([0, 0, 0])
            } else if index < RAMP_START {
                let offset = index.saturating_sub(CUBE_START);
                [
                    cube_level(offset / 36),
                    cube_level(offset / 6),
                    cube_level(offset),
                ]
            } else {
                let step = u8::try_from(index.saturating_sub(RAMP_START)).unwrap_or(u8::MAX);
                let grey = RAMP_BASE.saturating_add(step.saturating_mul(RAMP_STEP));
                [grey, grey, grey]
            };
        }
        Self {
            entries,
            foreground: DEFAULT_FOREGROUND,
            background: DEFAULT_BACKGROUND,
        }
    }

    pub fn entry(&self, index: u8) -> [u8; 3] {
        self.entries
            .get(usize::from(index))
            .copied()
            .unwrap_or([0, 0, 0])
    }

    pub fn foreground(&self) -> [u8; 3] {
        self.foreground
    }

    pub fn background(&self) -> [u8; 3] {
        self.background
    }

    fn resolve(&self, color: Color, default: [u8; 3]) -> [u8; 3] {
        match color {
            Color::Default => default,
            Color::Indexed(index) => self.entry(index),
            Color::Rgb(red, green, blue) => [red, green, blue],
        }
    }
}

fn cube_level(axis: usize) -> u8 {
    CUBE_LEVELS
        .get(axis % CUBE_LEVELS.len())
        .copied()
        .unwrap_or(0)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Cursor {
    pub row: usize,
    pub column: usize,
    pub visible: bool,
}

/// An inclusive row-major terminal selection in viewport coordinates.
/// Keeping the range in cells makes rendering independent of pixel geometry
/// and gives extraction and highlighting one exact pair of endpoints.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Selection {
    pub anchor: (usize, usize),
    pub extent: (usize, usize),
}

impl Selection {
    fn contains(self, row: usize, column: usize) -> bool {
        let cell = (row, column);
        let (start, end) = if self.anchor <= self.extent {
            (self.anchor, self.extent)
        } else {
            (self.extent, self.anchor)
        };
        cell >= start && cell <= end
    }
}

/// What a press selects by: its cell, the word under it (a second press),
/// or its row (a third), as foot's mouse bindings do.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Unit {
    #[default]
    Cell,
    Word,
    Row,
}

/// foot's default `word-delimiters`. A run of them is a unit of its own,
/// as a run of blanks is, so a word stops at either.
pub const WORD_DELIMITERS: &str = ",\u{2502}`|:\"'()[]{}<>";

#[derive(Clone, Copy, Eq, PartialEq)]
enum Class {
    Blank,
    Delimiter,
    Word,
}

fn class(scalar: char) -> Class {
    if scalar.is_whitespace() || scalar == '\0' {
        Class::Blank
    } else if WORD_DELIMITERS.contains(scalar) {
        Class::Delimiter
    } else {
        Class::Word
    }
}

/// A complete screen as one frame sees it: the model, where the cursor is,
/// how far back the scrollback viewport is scrolled, whether the surface
/// holds the keyboard, and the one coalesced visual-bell bit.
pub struct Snapshot<'a> {
    terminal: &'a Terminal,
    cursor: Cursor,
    viewport: usize,
    focused: bool,
    bell: bool,
    selection: Option<Selection>,
}

impl<'a> Snapshot<'a> {
    pub fn new(terminal: &'a Terminal, focused: bool, bell: bool) -> Self {
        // `pending_wrap` is deliberately dropped: the model already reports
        // the column the cursor still occupies, so a wrap that has not
        // happened yet must not move where it is drawn.
        let (row, column, _) = terminal.cursor();
        Self {
            terminal,
            cursor: Cursor {
                row,
                column,
                visible: terminal.mode("cursor-visible").unwrap_or(true),
            },
            viewport: 0,
            focused,
            bell,
            selection: None,
        }
    }

    /// Overrides the model's cursor. Nothing in production does: the
    /// viewport shifts the cursor rather than hiding it, so only the spec
    /// calls this.
    #[allow(dead_code)]
    pub fn with_cursor(mut self, cursor: Cursor) -> Self {
        self.cursor = cursor;
        self
    }

    /// Scroll the viewport back by `lines` of primary-screen history. Rows
    /// above the split come from history and the rest from the live screen,
    /// so a partially scrolled viewport shows both at once.
    pub fn scrolled_back(mut self, lines: usize) -> Self {
        self.viewport = lines.min(self.terminal.history_lines());
        self
    }

    pub fn with_selection(mut self, selection: Option<Selection>) -> Self {
        self.selection = selection;
        self
    }

    pub fn rows(&self) -> usize {
        self.terminal.rows()
    }

    pub fn columns(&self) -> usize {
        self.terminal.columns()
    }

    pub fn focused(&self) -> bool {
        self.focused
    }

    pub fn bell(&self) -> bool {
        self.bell
    }

    #[allow(dead_code)]
    pub fn viewport(&self) -> usize {
        self.viewport
    }

    /// Infallible so the cell loop has no error path: anything the model
    /// cannot answer for — a short history line, an out-of-range row — is
    /// blank rather than a failure that would abandon the frame.
    ///
    /// An open viewport reads the PRIMARY screen below the split, not the
    /// active one. History is primary-only, so reading the active screen
    /// there would stack primary history on top of whatever full-screen
    /// program holds the alternate screen — two unrelated scroll regions
    /// in one frame.
    pub fn cell(&self, row: usize, column: usize) -> Cell {
        let found = if row < self.viewport {
            self.viewport
                .checked_sub(row)
                .and_then(|back| self.terminal.history_lines().checked_sub(back))
                .and_then(|line| self.terminal.history_cell(line, column))
        } else {
            row.checked_sub(self.viewport).and_then(|row| {
                if self.viewport == 0 {
                    self.terminal.cell(row, column)
                } else {
                    self.terminal.primary_cell(row, column)
                }
            })
        };
        let mut cell = found.unwrap_or(BLANK);
        if self
            .selection
            .is_some_and(|selection| selection.contains(row, column))
        {
            cell.attributes.inverse = !cell.attributes.inverse;
        }
        cell
    }

    /// The first and last columns of the unit at a cell in this view: the
    /// cell; the run of word, blank or delimiter cells around it, as
    /// foot's word is; or the whole row. A row is the screen's, so a line
    /// the terminal wrapped is selected a row at a time.
    pub fn span(&self, unit: Unit, row: usize, column: usize) -> (usize, usize) {
        let last = self.columns().saturating_sub(1);
        let column = column.min(last);
        match unit {
            Unit::Cell => (column, column),
            Unit::Row => (0, last),
            Unit::Word => {
                let kind = class(self.cell(row, column).scalar);
                let same = |at: usize| class(self.cell(row, at).scalar) == kind;
                let mut start = column;
                while start > 0 && same(start.saturating_sub(1)) {
                    start = start.saturating_sub(1);
                }
                let mut end = column;
                while end < last && same(end.saturating_add(1)) {
                    end = end.saturating_add(1);
                }
                (start, end)
            }
        }
    }

    /// The selection a press at `anchor` dragged to `extent` makes by
    /// `unit`: from the anchor's unit to the extent's, whichever way the
    /// drag went, so the anchor's whole unit stays selected.
    pub fn select(&self, unit: Unit, anchor: (usize, usize), extent: (usize, usize)) -> Selection {
        let (anchor_start, anchor_end) = self.span(unit, anchor.0, anchor.1);
        let (extent_start, extent_end) = self.span(unit, extent.0, extent.1);
        if anchor <= extent {
            Selection {
                anchor: (anchor.0, anchor_start),
                extent: (extent.0, extent_end),
            }
        } else {
            Selection {
                anchor: (anchor.0, anchor_end),
                extent: (extent.0, extent_start),
            }
        }
    }

    /// Where the cursor is drawn, or `None` when it is hidden or the
    /// viewport has scrolled it off the bottom of the surface.
    pub fn cursor(&self) -> Option<(usize, usize)> {
        if !self.cursor.visible {
            return None;
        }
        let row = self.cursor.row.checked_add(self.viewport)?;
        if row >= self.rows() || self.cursor.column >= self.columns() {
            return None;
        }
        Some((row, self.cursor.column))
    }
}

/// Resolved ink for one cell: what the renditions decided, before any
/// glyph bit is looked at.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Ink {
    foreground: [u8; 3],
    background: [u8; 3],
}

impl Ink {
    fn new(attributes: &Attributes, palette: &Palette) -> Self {
        let mut foreground = palette.resolve(attributes.foreground, palette.foreground());
        let mut background = palette.resolve(attributes.background, palette.background());
        if attributes.inverse {
            std::mem::swap(&mut foreground, &mut background);
        }
        // Faint dims what is actually drawn, so it follows the exchange
        // rather than preceding it; inverse-and-faint otherwise brightens.
        if attributes.faint {
            foreground = blend_half(foreground, background);
        }
        Self {
            foreground,
            background,
        }
    }
}

/// Halfway, in integer channel arithmetic and without a widening cast: the
/// halves plus the carry of the two low bits is exactly `(from + to) / 2`.
fn blend_half(from: [u8; 3], to: [u8; 3]) -> [u8; 3] {
    let mut blended = [0u8; 3];
    for (slot, (from, to)) in blended.iter_mut().zip(from.iter().zip(to.iter())) {
        *slot = (from / 2) + (to / 2) + ((from % 2) + (to % 2)) / 2;
    }
    blended
}

/// Italic's bounded shear: the top half of the cell leans one pixel right
/// and the bottom half does not, so the slant never exceeds one pixel no
/// matter how tall the face is.
fn shear(glyph_row: usize, height: usize) -> usize {
    usize::from(glyph_row < height / 2)
}

/// Render `snapshot` into a tightly packed XRGB8888 surface.
///
/// `pixels` must be exactly `width * height * 4` bytes, matching the wl_shm
/// pool td-term allocates; a stride is therefore not a separate degree of
/// freedom. Everything is clipped to the surface, so a grid larger than the
/// surface renders its visible corner rather than failing.
pub fn render(
    snapshot: &Snapshot,
    palette: &Palette,
    font: &Font,
    pixels: &mut [u8],
    width: usize,
    height: usize,
) -> Result<(), String> {
    render_with(snapshot, palette, font, None, pixels, width, height)
}

/// The cell the grid is laid on: the outline face's when there is one,
/// whether fitted to the bitmap face's cell or at a size of its own, and
/// otherwise the bitmap face's. A program hit-tests and sizes its grid by
/// the same cell it draws with.
pub fn cell_size(font: &Font, outline: Option<&Face>) -> (usize, usize) {
    outline.map_or((font.width(), font.height()), |face| {
        let cell = face.cell();
        (cell.width, cell.height)
    })
}

/// A zoom step in pixels per em: foot's half a point at 96 dots per inch.
pub const ZOOM_STEP: f32 = 2.0 / 3.0;
/// The most steps one zoom takes looking for a cell that moves its way.
const MAX_ZOOM_STEPS: usize = 4;

/// Where a font chord moves a terminal's outline face: a step larger, a
/// step smaller, or back to the size it started at.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ZoomTo {
    In,
    Out,
    Start,
}

/// A terminal's outline face and its zoom: the sizing it started at, the
/// size in pixels per em that gave, and how many `ZOOM_STEP`s it has moved
/// since. A step resizes the face from the bytes it holds
/// (`Face::resized`), so zooming reads nothing.
pub struct Zoom {
    face: Face,
    start: Sizing,
    size: f32,
    steps: i16,
}

impl Zoom {
    pub fn new(face: Face, start: Sizing) -> Self {
        let size = face.size();
        Self {
            face,
            start,
            size,
            steps: 0,
        }
    }

    pub fn face(&self) -> &Face {
        &self.face
    }

    pub fn face_mut(&mut self) -> &mut Face {
        &mut self.face
    }

    /// Moves the face `to`, and says whether it changed: not when it is
    /// already there, and not past the face's bounds, which leave it as it
    /// was. Back at the start is the start's own sizing, so a face fitted
    /// to a cell is fitted again rather than sized near it. A step in or
    /// out goes on past a size whose cell would move against it in either
    /// axis, a few steps at most: a fitted face's cell clips its line box,
    /// so the face's own cell a step smaller can be taller, and zooming
    /// out would show fewer rows.
    pub fn zoom(&mut self, to: ZoomTo) -> bool {
        let current = self.face.cell();
        let mut steps = self.steps;
        for _ in 0..MAX_ZOOM_STEPS {
            steps = match to {
                ZoomTo::In => steps.saturating_add(1),
                ZoomTo::Out => steps.saturating_sub(1),
                ZoomTo::Start => 0,
            };
            if steps == self.steps {
                return false;
            }
            let sizing = if steps == 0 {
                self.start
            } else {
                Sizing::PixelsPerEm(self.size + f32::from(steps) * ZOOM_STEP)
            };
            let Ok(face) = self.face.resized(sizing) else {
                return false;
            };
            let cell = face.cell();
            let along = match to {
                ZoomTo::In => cell.width >= current.width && cell.height >= current.height,
                ZoomTo::Out => cell.width <= current.width && cell.height <= current.height,
                ZoomTo::Start => true,
            };
            if along {
                self.face = face;
                self.steps = steps;
                return true;
            }
        }
        false
    }
}

/// `render`, drawing each glyph the `outline` face has through it, on the
/// face's cell (`cell_size`): the cursor and every rule take that cell,
/// and a scalar the face lacks is the bitmap face's glyph centred in it.
pub fn render_with(
    snapshot: &Snapshot,
    palette: &Palette,
    font: &Font,
    mut outline: Option<&mut Face>,
    pixels: &mut [u8],
    width: usize,
    height: usize,
) -> Result<(), String> {
    let expected = width
        .checked_mul(height)
        .and_then(|count| count.checked_mul(BYTES_PER_PIXEL))
        .ok_or_else(|| format!("surface {width}x{height} overflows a byte count"))?;
    if pixels.len() != expected {
        return Err(format!(
            "surface {width}x{height} needs {expected} bytes, not {}",
            pixels.len()
        ));
    }
    let (cell_width, cell_height) = cell_size(font, outline.as_deref());
    if cell_width == 0 || cell_height == 0 {
        return Err("font cells have no area".into());
    }

    fill(pixels, palette.background());

    // Only the cells that can reach the surface are visited; `div_ceil`
    // keeps a partially visible last row or column.
    let rows = snapshot.rows().min(height.div_ceil(cell_height));
    let columns = snapshot.columns().min(width.div_ceil(cell_width));
    for row in 0..rows {
        let Some(origin_y) = row.checked_mul(cell_height) else {
            break;
        };
        for column in 0..columns {
            let Some(origin_x) = column.checked_mul(cell_width) else {
                break;
            };
            let cell = snapshot.cell(row, column);
            paint_cell(
                pixels,
                width,
                height,
                font,
                outline.as_deref_mut(),
                palette,
                &cell,
                (origin_x, origin_y),
            );
        }
    }

    if let Some((row, column)) = snapshot.cursor() {
        paint_cursor(
            pixels,
            width,
            height,
            font,
            outline,
            palette,
            snapshot,
            (row, column),
        );
    }

    if snapshot.bell() {
        invert_ring(pixels, width, height);
    }
    Ok(())
}

fn fill(pixels: &mut [u8], color: [u8; 3]) {
    let [red, green, blue] = color;
    let packed = [blue, green, red, 0];
    for pixel in pixels.as_chunks_mut::<BYTES_PER_PIXEL>().0 {
        *pixel = packed;
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_cell(
    pixels: &mut [u8],
    width: usize,
    height: usize,
    font: &Font,
    outline: Option<&mut Face>,
    palette: &Palette,
    cell: &Cell,
    (origin_x, origin_y): (usize, usize),
) {
    let attributes = &cell.attributes;
    let ink = Ink::new(attributes, palette);
    let (cell_width, cell_height) = cell_size(font, outline.as_deref());
    if let Some(face) = outline {
        let slot = face.glyph(face.style(attributes.bold, attributes.italic), cell.scalar);
        if slot != Slot::Missing {
            let (x, y) = (origin_x, origin_y);
            let rules = rules(attributes, cell_height);
            let coverage = |column: usize, row: usize| match slot {
                Slot::Placed(entry) => covered(face, entry, column, row),
                _ => 0,
            };
            for row in 0..cell_height {
                let Some(y) = y.checked_add(row).filter(|&y| y < height) else {
                    return;
                };
                let ruled = rules.contains(&Some(row));
                for column in 0..cell_width {
                    let Some(x) = x.checked_add(column).filter(|&x| x < width) else {
                        break;
                    };
                    let color = if ruled {
                        ink.foreground
                    } else {
                        mix(ink.background, ink.foreground, coverage(column, row))
                    };
                    put_pixel(pixels, width, height, x, y, color);
                }
            }
            return;
        }
    }
    let glyph = font.index(cell.scalar);
    let rules = rules(attributes, cell_height);
    // Centred: a larger cell frames the glyph and a smaller one clips it
    // about its middle, an odd difference trimming the right column or the
    // bottom row once more, and the bitmap face's own cell holds it
    // exactly.
    let inset = |cell: usize, glyph: usize| {
        let signed = |value: usize| i64::try_from(value).unwrap_or(i64::MAX);
        (signed(cell).saturating_sub(signed(glyph))) / 2
    };
    let (inset_x, inset_y) = (
        inset(cell_width, font.width()),
        inset(cell_height, font.height()),
    );
    let within = |at: usize, inset: i64, glyph: usize| {
        i64::try_from(at)
            .ok()
            .and_then(|at| usize::try_from(at.saturating_sub(inset)).ok())
            .filter(|&at| at < glyph)
    };
    for row in 0..cell_height {
        let Some(y) = origin_y.checked_add(row) else {
            return;
        };
        if y >= height {
            return;
        }
        let ruled = rules.contains(&Some(row));
        let glyph_row = within(row, inset_y, font.height());
        let lean = match glyph_row {
            Some(glyph_row) if attributes.italic => shear(glyph_row, font.height()),
            _ => 0,
        };
        for column in 0..cell_width {
            let Some(x) = origin_x.checked_add(column) else {
                break;
            };
            if x >= width {
                break;
            }
            let inked = match (within(column, inset_x, font.width()), glyph_row) {
                (Some(glyph_column), Some(glyph_row)) => {
                    lit(font, glyph, glyph_column, glyph_row, attributes.bold, lean)
                }
                _ => false,
            };
            let color = if ruled || inked {
                ink.foreground
            } else {
                ink.background
            };
            put_pixel(pixels, width, height, x, y, color);
        }
    }
}

/// The rows the underline and the strike take in a cell `height` tall,
/// each when the rendition asks for it: the cell's, not the glyph's, so a
/// rule is where it is whichever face draws the cell.
fn rules(attributes: &Attributes, height: usize) -> [Option<usize>; 2] {
    [
        attributes
            .underline
            .then(|| height.saturating_sub(UNDERLINE_INSET)),
        attributes.strike.then_some(height / 2),
    ]
}

/// The coverage `entry` puts at (`column`, `row`) of the cell: the glyph's
/// pixels from the atlas page, placed from the face's pen and baseline and
/// clipped to the cell, so a lean or an overhang never reaches the next.
fn covered(face: &Face, entry: Entry, column: usize, row: usize) -> u8 {
    let cell = face.cell();
    let signed = |value: usize| i64::try_from(value).unwrap_or(i64::MAX);
    let x = signed(column)
        .saturating_sub(signed(cell.pen))
        .saturating_sub(i64::from(entry.left));
    let y = signed(row)
        .saturating_sub(signed(cell.baseline))
        .saturating_add(i64::from(entry.top));
    let (Ok(x), Ok(y)) = (usize::try_from(x), usize::try_from(y)) else {
        return 0;
    };
    if x >= entry.width || y >= entry.height {
        return 0;
    }
    entry
        .y
        .checked_add(y)
        .and_then(|page_row| page_row.checked_mul(PAGE_WIDTH))
        .and_then(|start| start.checked_add(entry.x))
        .and_then(|start| start.checked_add(x))
        .and_then(|at| face.atlas().page().get(at).copied())
        .unwrap_or(0)
}

/// `from` moved toward `to` by `alpha` of 255, per channel, rounded.
fn mix(from: [u8; 3], to: [u8; 3], alpha: u8) -> [u8; 3] {
    let alpha = u16::from(alpha);
    let mut mixed = [0u8; 3];
    for (slot, (from, to)) in mixed.iter_mut().zip(from.iter().zip(to.iter())) {
        let sum = u32::from(*from) * u32::from(255 - alpha) + u32::from(*to) * u32::from(alpha);
        *slot = u8::try_from((sum + 127) / 255).unwrap_or(u8::MAX);
    }
    mixed
}

/// Whether the cell's pixel at (`glyph_column`, `glyph_row`) is set, after
/// the shear moves which source column it reads and bold adds the column
/// to its left. Both effects are clipped by construction: a source column
/// left of the glyph does not exist, and a set bit pushed past the cell's
/// right edge is never asked for.
fn lit(
    font: &Font,
    glyph: usize,
    glyph_column: usize,
    glyph_row: usize,
    bold: bool,
    lean: usize,
) -> bool {
    let Some(source) = glyph_column.checked_sub(lean) else {
        return false;
    };
    if font.pixel(glyph, source, glyph_row) {
        return true;
    }
    bold && source
        .checked_sub(1)
        .is_some_and(|left| font.pixel(glyph, left, glyph_row))
}

#[allow(clippy::too_many_arguments)]
fn paint_cursor(
    pixels: &mut [u8],
    width: usize,
    height: usize,
    font: &Font,
    outline: Option<&mut Face>,
    palette: &Palette,
    snapshot: &Snapshot,
    (row, column): (usize, usize),
) {
    let (cell_width, cell_height) = cell_size(font, outline.as_deref());
    let (Some(origin_x), Some(origin_y)) =
        (column.checked_mul(cell_width), row.checked_mul(cell_height))
    else {
        return;
    };
    let cell = snapshot.cell(row, column);
    if snapshot.focused() {
        // A focused cursor is the cell drawn with its ink exchanged, which
        // is inverse's presentation — so a cursor over an already-inverse
        // cell reads as the surrounding text, as on a real terminal.
        let mut attributes = cell.attributes;
        attributes.inverse = !attributes.inverse;
        let flipped = Cell {
            scalar: cell.scalar,
            attributes,
        };
        paint_cell(
            pixels,
            width,
            height,
            font,
            outline,
            palette,
            &flipped,
            (origin_x, origin_y),
        );
        return;
    }
    // Unfocused it is a hollow box: present, but not claiming the keyboard.
    let ink = Ink::new(&cell.attributes, palette);
    for glyph_row in 0..cell_height {
        for glyph_column in 0..cell_width {
            let edge = glyph_row == 0
                || glyph_column == 0
                || glyph_row.saturating_add(1) == cell_height
                || glyph_column.saturating_add(1) == cell_width;
            if !edge {
                continue;
            }
            let (Some(x), Some(y)) = (
                origin_x.checked_add(glyph_column),
                origin_y.checked_add(glyph_row),
            ) else {
                continue;
            };
            put_pixel(pixels, width, height, x, y, ink.foreground);
        }
    }
}

/// The visual bell: invert the outermost one-pixel ring of the surface.
/// Each pixel is visited once, because inverting a corner twice would
/// restore it and leave a ring with holes in it.
fn invert_ring(pixels: &mut [u8], width: usize, height: usize) {
    if width == 0 || height == 0 {
        return;
    }
    let last_row = height.saturating_sub(1);
    let last_column = width.saturating_sub(1);
    for x in 0..width {
        invert_pixel(pixels, width, height, x, 0);
        if last_row != 0 {
            invert_pixel(pixels, width, height, x, last_row);
        }
    }
    for y in 1..last_row {
        invert_pixel(pixels, width, height, 0, y);
        if last_column != 0 {
            invert_pixel(pixels, width, height, last_column, y);
        }
    }
}

fn offset_of(width: usize, x: usize, y: usize) -> Option<usize> {
    y.checked_mul(width)
        .and_then(|row| row.checked_add(x))
        .and_then(|index| index.checked_mul(BYTES_PER_PIXEL))
}

fn put_pixel(pixels: &mut [u8], width: usize, height: usize, x: usize, y: usize, color: [u8; 3]) {
    if x >= width || y >= height {
        return;
    }
    let Some(offset) = offset_of(width, x, y) else {
        return;
    };
    let Some(end) = offset.checked_add(BYTES_PER_PIXEL) else {
        return;
    };
    let Some(pixel) = pixels.get_mut(offset..end) else {
        return;
    };
    let [red, green, blue] = color;
    // XRGB8888 is little-endian in memory: blue, green, red, unused.
    pixel.copy_from_slice(&[blue, green, red, 0]);
}

fn invert_pixel(pixels: &mut [u8], width: usize, height: usize, x: usize, y: usize) {
    if x >= width || y >= height {
        return;
    }
    let Some(offset) = offset_of(width, x, y) else {
        return;
    };
    let Some(end) = offset.checked_add(BYTES_PER_PIXEL) else {
        return;
    };
    let Some(pixel) = pixels.get_mut(offset..end) else {
        return;
    };
    // The unused byte stays zero, which the compositor's frame assertions
    // require; only the three colour channels invert.
    for channel in pixel.iter_mut().take(3) {
        *channel = !*channel;
    }
}

/// Encode a rendered surface as binary P6 PPM, the visual oracle: the
/// raster's own encoder over exactly `width` by `height` pixels.
pub fn ppm(pixels: &[u8], width: usize, height: usize) -> Result<Vec<u8>, String> {
    let expected = width
        .checked_mul(height)
        .and_then(|count| count.checked_mul(BYTES_PER_PIXEL))
        .ok_or_else(|| format!("surface {width}x{height} overflows a byte count"))?;
    if pixels.len() != expected {
        return Err(format!(
            "surface {width}x{height} needs {expected} bytes, not {}",
            pixels.len()
        ));
    }
    let surface = Surface::new(width, height, Scale::default())
        .map_err(|error| format!("surface {width}x{height}: {error}"))?;
    let rgb = raster::rgb(pixels, surface, width * BYTES_PER_PIXEL)
        .map_err(|error| format!("surface {width}x{height}: {error}"))?;
    Ok(raster::ppm(surface, &rgb))
}

/// Decode a binary P6 PPM back to `(pixels, width, height)` in the same
/// XRGB8888 form `render` writes, so a committed golden and a fresh frame
/// are compared as pixels rather than as bytes that merely look alike.
pub fn from_ppm(bytes: &[u8]) -> Result<(Vec<u8>, usize, usize), String> {
    let mut fields = Vec::new();
    let mut rest = bytes;
    for _ in 0..4 {
        let start = rest
            .iter()
            .position(|byte| !byte.is_ascii_whitespace())
            .ok_or_else(|| "ppm ended before its header".to_string())?;
        let tail = rest.get(start..).unwrap_or_default();
        let length = tail
            .iter()
            .position(u8::is_ascii_whitespace)
            .ok_or_else(|| "ppm header field is unterminated".to_string())?;
        // Exactly one whitespace byte follows each field, so a CRLF header
        // would leave its `\n` at the head of the payload and shift the
        // whole image one byte. Refuse rather than decode the shift.
        if tail.get(length) == Some(&b'\r') {
            return Err("ppm header uses CRLF line endings".into());
        }
        let field = tail.get(..length).unwrap_or_default();
        fields.push(
            std::str::from_utf8(field)
                .map_err(|error| format!("ppm header field is not text: {error}"))?
                .to_string(),
        );
        // One whitespace byte after the maxval, per the format: the pixel
        // payload starts at the next byte and may itself be whitespace.
        rest = tail.get(length.saturating_add(1)..).unwrap_or_default();
    }
    let field = |index: usize| fields.get(index).map(String::as_str).unwrap_or_default();
    if field(0) != "P6" {
        return Err(format!("ppm magic is {:?}, not P6", field(0)));
    }
    let number = |index: usize, name: &str| {
        field(index)
            .parse::<usize>()
            .map_err(|error| format!("ppm {name} {:?}: {error}", field(index)))
    };
    let width = number(1, "width")?;
    let height = number(2, "height")?;
    if number(3, "maxval")? != 255 {
        return Err("ppm maxval is not 255".into());
    }
    let triples = width
        .checked_mul(height)
        .and_then(|count| count.checked_mul(3))
        .ok_or_else(|| format!("ppm {width}x{height} overflows a byte count"))?;
    if rest.len() != triples {
        return Err(format!(
            "ppm {width}x{height} carries {} payload bytes, not {triples}",
            rest.len()
        ));
    }
    let mut pixels = Vec::with_capacity(triples / 3 * BYTES_PER_PIXEL);
    let (chunks, _) = rest.as_chunks::<3>();
    for [red, green, blue] in chunks.iter().copied() {
        pixels.extend_from_slice(&[blue, green, red, 0]);
    }
    Ok((pixels, width, height))
}

pub fn selftest() -> Result<(), String> {
    let font = crate::font::pinned()?;
    let palette = Palette::pinned();
    let mut terminal = Terminal::new(1, 2)?;
    terminal.feed(b"hi");
    let snapshot = Snapshot::new(&terminal, true, false);
    let width = font
        .width()
        .checked_mul(2)
        .ok_or_else(|| "font cell width overflows a surface".to_string())?;
    let height = font.height();
    let bytes = width
        .checked_mul(height)
        .and_then(|count| count.checked_mul(BYTES_PER_PIXEL))
        .ok_or_else(|| "selftest surface overflows a byte count".to_string())?;
    let mut pixels = vec![0; bytes];
    render(&snapshot, &palette, &font, &mut pixels, width, height)?;
    // The 'h' in the first cell must leave ink; the cursor rests in the
    // second, so it cannot.
    if !first_cell_inked(&pixels, width, font.width(), palette.background())? {
        return Err("render selftest drew no glyph pixels".into());
    }
    let encoded = ppm(&pixels, width, height)?;
    let (decoded, decoded_width, decoded_height) = from_ppm(&encoded)?;
    if (decoded, decoded_width, decoded_height) != (pixels, width, height) {
        return Err("render selftest did not round-trip through P6".into());
    }
    Ok(())
}

/// Whether the first `cell` columns of a `width`-wide XRGB8888 frame hold
/// any pixel unlike `background`, the colour the frame was cleared to.
fn first_cell_inked(
    pixels: &[u8],
    width: usize,
    cell: usize,
    background: [u8; 3],
) -> Result<bool, String> {
    let [red, green, blue] = background;
    let cleared = [blue, green, red, 0];
    let row_bytes = width
        .checked_mul(BYTES_PER_PIXEL)
        .filter(|bytes| *bytes > 0)
        .ok_or_else(|| "selftest row overflows a byte count".to_string())?;
    let cell_bytes = cell
        .checked_mul(BYTES_PER_PIXEL)
        .ok_or_else(|| "selftest cell overflows a byte count".to_string())?;
    Ok(pixels.chunks_exact(row_bytes).any(|row| {
        let (first, _) = row.split_at(cell_bytes.min(row.len()));
        first
            .as_chunks::<BYTES_PER_PIXEL>()
            .0
            .iter()
            .any(|pixel| *pixel != cleared)
    }))
}

#[cfg(test)]
#[path = "vt_render_spec.rs"]
mod spec;

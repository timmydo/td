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
use crate::vt::{Attributes, Cell, Color, Terminal, Underline};

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

/// A double underline's second rule, this many rows above the first.
const DOUBLE_GAP: usize = 2;

/// The unit of a curly underline's wave and a dotted one's dots: this
/// fraction of the cell's height, and at least a pixel.
const PATTERN_SCALE: usize = 16;

/// What a row past the end of the model shows. Written out rather than
/// `Attributes::default()` so adding a rendition is a compile error here,
/// where the renderer has to decide what to do with it.
const BLANK: Cell = Cell {
    scalar: ' ',
    attributes: Attributes {
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

/// What td-term rules under a Control-hover: a link found in a row's
/// text, by its cells, or an OSC 8 link, by the id its cells were written
/// in, wherever they are on the view.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Hover {
    Span(LinkSpan),
    Link(u32),
}

/// A link's cells on one row of the view, `start..end`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LinkSpan {
    pub row: usize,
    pub start: usize,
    pub end: usize,
}

impl LinkSpan {
    fn contains(&self, row: usize, column: usize) -> bool {
        row == self.row && (self.start..self.end).contains(&column)
    }
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

/// Which row of the view a status line covers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Edge {
    Top,
    Bottom,
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
    status: Option<(Vec<char>, Edge)>,
    link: Option<Hover>,
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
            status: None,
            link: None,
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

    /// Rules `link`'s cells (`linked`).
    pub fn with_link(mut self, link: Option<Hover>) -> Self {
        self.link = link;
        self
    }

    /// Lays a status line over the view's first or last row: `text`, cut
    /// at the row's width, in inverse video to the edge, with the cursor
    /// hidden. td-term's search shows its query here, as foot's search box
    /// does.
    pub fn with_status(mut self, text: Option<&str>, edge: Edge) -> Self {
        self.status = text.map(|text| (text.chars().take(self.columns()).collect(), edge));
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

    /// Whether `row` is the one the status line covers.
    fn status_row(&self, row: usize) -> bool {
        self.status.as_ref().is_some_and(|(_, edge)| match edge {
            Edge::Top => row == 0,
            Edge::Bottom => row.saturating_add(1) == self.rows(),
        })
    }

    /// Whether the cell is one of the hovered link's, ruled over whatever
    /// it holds; none on the status line's row, which covers them.
    pub fn linked(&self, row: usize, column: usize) -> bool {
        if self.status_row(row) {
            return false;
        }
        match self.link {
            Some(Hover::Span(span)) => span.contains(row, column),
            Some(Hover::Link(id)) => id != 0 && self.cell(row, column).attributes.link == id,
            None => false,
        }
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
        if let Some((status, _)) = self.status.as_ref().filter(|_| self.status_row(row)) {
            let mut cell = BLANK;
            cell.scalar = status.get(column).copied().unwrap_or(' ');
            cell.attributes.inverse = true;
            return cell;
        }
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

    /// Whether this view's `row` goes on at the start of the next: the
    /// terminal wrapped it there rather than the child ending it. Read from
    /// the same place `cell` reads the row, so history above the split and
    /// the primary screen below it while scrolled back. Never for the
    /// view's last row, which has no next row in view.
    pub fn wrapped(&self, row: usize) -> bool {
        if row.saturating_add(1) >= self.rows() {
            return false;
        }
        if row < self.viewport {
            self.viewport
                .checked_sub(row)
                .and_then(|back| self.terminal.history_lines().checked_sub(back))
                .is_some_and(|line| self.terminal.history_wrapped(line))
        } else {
            row.checked_sub(self.viewport).is_some_and(|row| {
                if self.viewport == 0 {
                    self.terminal.wrapped(row)
                } else {
                    self.terminal.primary_wrapped(row)
                }
            })
        }
    }

    /// The first and last columns of the unit at a cell on its row: the
    /// cell; the run of word, blank or delimiter cells around it, as
    /// foot's word is; or the whole row. `select` carries a word or a row
    /// on across the rows the terminal wrapped.
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

    /// The first and last cells of the unit at `at`: its `span`, carried
    /// on across rows the terminal wrapped, as foot's word and row are. A
    /// row unit is the whole wrapped line; a word goes on while the row it
    /// reaches the edge of was wrapped and the next row starts with the
    /// same kind of cell.
    fn reach(&self, unit: Unit, at: (usize, usize)) -> ((usize, usize), (usize, usize)) {
        let (start, end) = self.span(unit, at.0, at.1);
        let (mut first, mut last) = ((at.0, start), (at.0, end));
        if unit == Unit::Cell {
            return (first, last);
        }
        let edge = self.columns().saturating_sub(1);
        let kind = (unit == Unit::Word).then(|| class(self.cell(at.0, at.1.min(edge)).scalar));
        let joins = |row: usize, column: usize| {
            kind.is_none_or(|kind| class(self.cell(row, column).scalar) == kind)
        };
        while first.1 == 0 {
            let Some(row) = first.0.checked_sub(1) else {
                break;
            };
            if !self.wrapped(row) || !joins(row, edge) {
                break;
            }
            first = (row, self.span(unit, row, edge).0);
        }
        while last.1 == edge && self.wrapped(last.0) {
            let row = last.0.saturating_add(1);
            if !joins(row, 0) {
                break;
            }
            last = (row, self.span(unit, row, 0).1);
        }
        (first, last)
    }

    /// The selection a press at `anchor` dragged to `extent` makes by
    /// `unit`: from the anchor's unit to the extent's, whichever way the
    /// drag went, so the anchor's whole unit stays selected.
    pub fn select(&self, unit: Unit, anchor: (usize, usize), extent: (usize, usize)) -> Selection {
        let (anchor_start, anchor_end) = self.reach(unit, anchor);
        let (extent_start, extent_end) = self.reach(unit, extent);
        if anchor <= extent {
            Selection {
                anchor: anchor_start,
                extent: extent_end,
            }
        } else {
            Selection {
                anchor: anchor_end,
                extent: extent_start,
            }
        }
    }

    /// Where the cursor is drawn, or `None` when it is hidden or the
    /// viewport has scrolled it off the bottom of the surface.
    pub fn cursor(&self) -> Option<(usize, usize)> {
        if !self.cursor.visible || self.status.is_some() {
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
    underline: [u8; 3],
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
        // An underline color of its own is drawn as it is; without one the
        // underline is the ink the glyph is drawn in.
        let underline = palette.resolve(attributes.underline_color, foreground);
        Self {
            foreground,
            background,
            underline,
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

    // Only the cells that can reach the surface are visited; `div_ceil`
    // keeps a partially visible last row or column.
    let rows = snapshot.rows().min(height.div_ceil(cell_height));
    let columns = snapshot.columns().min(width.div_ceil(cell_width));

    // Every cell writes each of its pixels, so only what the grid leaves
    // uncovered is cleared.
    let grid = (
        columns.checked_mul(cell_width).map(|w| w.min(width)),
        rows.checked_mul(cell_height).map(|h| h.min(height)),
    );
    match grid {
        (Some(grid_width), Some(grid_height)) => {
            fill_margin(
                pixels,
                width,
                (grid_width, grid_height),
                palette.background(),
            );
        }
        _ => fill(pixels, palette.background()),
    }
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
            if snapshot.linked(row, column) {
                let ground = Ink::new(&cell.attributes, palette).background;
                paint_link_rule(
                    pixels,
                    (width, height),
                    ground,
                    (origin_x, origin_y),
                    (cell_width, cell_height),
                );
            }
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
    pixels
        .as_chunks_mut::<BYTES_PER_PIXEL>()
        .0
        .fill(packed(color));
}

/// Clears what lies right of and below the grid's `width` by `height`
/// corner of a surface `stride` pixels wide.
fn fill_margin(pixels: &mut [u8], stride: usize, (width, height): (usize, usize), color: [u8; 3]) {
    let color = packed(color);
    let surface = pixels.as_chunks_mut::<BYTES_PER_PIXEL>().0;
    let Some(covered) = stride.checked_mul(height) else {
        surface.fill(color);
        return;
    };
    let (grid, below) = surface.split_at_mut(covered.min(surface.len()));
    below.fill(color);
    if width < stride {
        for row in grid.chunks_mut(stride) {
            if let Some(right) = row.get_mut(width..) {
                right.fill(color);
            }
        }
    }
}

/// XRGB8888 is little-endian in memory: blue, green, red, unused.
fn packed([red, green, blue]: [u8; 3]) -> [u8; BYTES_PER_PIXEL] {
    [blue, green, red, 0]
}

/// The `length` pixels of row `y` from column `x`, or none unless all of
/// them are inside the surface.
fn span(
    pixels: &mut [u8],
    (width, height): (usize, usize),
    (x, y): (usize, usize),
    length: usize,
) -> Option<&mut [[u8; BYTES_PER_PIXEL]]> {
    if y >= height || x.checked_add(length)? > width {
        return None;
    }
    let start = offset_of(width, x, y)?;
    let end = start.checked_add(length.checked_mul(BYTES_PER_PIXEL)?)?;
    Some(pixels.get_mut(start..end)?.as_chunks_mut().0)
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
    // The cell's columns left of the surface's right edge; each row is
    // written whole through them, a rule laid over it afterwards.
    let visible = width.saturating_sub(origin_x).min(cell_width);
    if visible == 0 {
        return;
    }
    let ruled = attributes.strike || attributes.underline != Underline::None;
    let (foreground, background) = (packed(ink.foreground), packed(ink.background));
    let underline = packed(ink.underline);
    let rule_over = |pixels: &mut [[u8; BYTES_PER_PIXEL]], row: usize| {
        if !ruled {
            return;
        }
        for (column, pixel) in pixels.iter_mut().enumerate() {
            let x = origin_x.saturating_add(column);
            match rule(attributes, (cell_width, cell_height), x, column, row) {
                Some(Rule::Strike) => *pixel = foreground,
                Some(Rule::Underline) => *pixel = underline,
                None => {}
            }
        }
    };
    if let Some(face) = outline {
        let slot = face.glyph(face.style(attributes.bold, attributes.italic), cell.scalar);
        if slot != Slot::Missing {
            let glyph = match slot {
                Slot::Placed(entry) => Some(Coverage::new(face, entry)),
                _ => None,
            };
            for row in 0..cell_height {
                let Some(y) = origin_y.checked_add(row) else {
                    return;
                };
                let Some(pixels) = span(pixels, (width, height), (origin_x, y), visible) else {
                    return;
                };
                match glyph.as_ref().and_then(|glyph| glyph.row(face, row)) {
                    Some((offset, coverage)) => {
                        for (column, pixel) in pixels.iter_mut().enumerate() {
                            let alpha = offset
                                .entry_column(column)
                                .and_then(|at| coverage.get(at))
                                .copied()
                                .unwrap_or(0);
                            *pixel = match alpha {
                                0 => background,
                                u8::MAX => foreground,
                                _ => packed(mix(ink.background, ink.foreground, alpha)),
                            };
                        }
                    }
                    None => pixels.fill(background),
                }
                rule_over(pixels, row);
            }
            return;
        }
    }
    let glyph = font.index(cell.scalar);
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
        let Some(pixels) = span(pixels, (width, height), (origin_x, y), visible) else {
            return;
        };
        let bits = within(row, inset_y, font.height())
            .and_then(|glyph_row| Some((glyph_row, font.row(glyph, glyph_row)?)))
            .filter(|(_, bits)| bits.iter().any(|&byte| byte != 0));
        match bits {
            Some((glyph_row, bits)) => {
                let lean = if attributes.italic {
                    shear(glyph_row, font.height())
                } else {
                    0
                };
                for (column, pixel) in pixels.iter_mut().enumerate() {
                    let inked = within(column, inset_x, font.width()).is_some_and(|column| {
                        lit(bits, font.width(), column, attributes.bold, lean)
                    });
                    *pixel = if inked { foreground } else { background };
                }
            }
            None => pixels.fill(background),
        }
        rule_over(pixels, row);
    }
}

/// A placed glyph's coverage, read a cell row at a time from the atlas
/// page, placed from the face's pen and baseline and clipped to the cell,
/// so a lean or an overhang never reaches the next.
struct Coverage {
    entry: Entry,
    offset: Offset,
    /// The entry row under the cell's row 0, negative while the entry
    /// starts below it.
    top: i64,
}

/// Where the entry's first column lands against the cell's first.
#[derive(Clone, Copy)]
enum Offset {
    /// This many columns left of the cell, as an overhang starts.
    Before(usize),
    /// This many columns into the cell.
    Inside(usize),
}

impl Offset {
    /// The entry column under the cell's `column`, none left of the entry.
    fn entry_column(self, column: usize) -> Option<usize> {
        match self {
            Offset::Before(before) => column.checked_add(before),
            Offset::Inside(inside) => column.checked_sub(inside),
        }
    }
}

impl Coverage {
    fn new(face: &Face, entry: Entry) -> Self {
        let cell = face.cell();
        let signed = |value: usize| i64::try_from(value).unwrap_or(i64::MAX);
        let start = signed(cell.pen).saturating_add(i64::from(entry.left));
        let offset = match usize::try_from(start) {
            Ok(inside) => Offset::Inside(inside),
            Err(_) => Offset::Before(usize::try_from(start.unsigned_abs()).unwrap_or(usize::MAX)),
        };
        let top = i64::from(entry.top).saturating_sub(signed(cell.baseline));
        Self { entry, offset, top }
    }

    /// The entry's coverage row under the cell's `row` with its offset,
    /// none above or below the entry: the whole row, or as much of it as
    /// the page holds.
    fn row<'a>(&self, face: &'a Face, row: usize) -> Option<(Offset, &'a [u8])> {
        let row = i64::try_from(row).ok()?;
        let y = usize::try_from(row.saturating_add(self.top)).ok()?;
        if y >= self.entry.height {
            return None;
        }
        let start = self
            .entry
            .y
            .checked_add(y)?
            .checked_mul(PAGE_WIDTH)?
            .checked_add(self.entry.x)?;
        let rest = face.atlas().page().get(start..).unwrap_or(&[]);
        Some((self.offset, rest.get(..self.entry.width).unwrap_or(rest)))
    }
}

/// A hovered link's rule, on the underline's row across the cell: black
/// or white, whichever stands out from the cell's drawn `ground`, so a
/// link in its background's color, which an underline in its own ink
/// would leave unseen, still shows how far it runs.
fn paint_link_rule(
    pixels: &mut [u8],
    (width, height): (usize, usize),
    ground: [u8; 3],
    (origin_x, origin_y): (usize, usize),
    (cell_width, cell_height): (usize, usize),
) {
    let [red, green, blue] = ground.map(u32::from);
    let luma = (red * 299 + green * 587 + blue * 114) / 1000;
    let rule = if luma > 127 {
        [0, 0, 0]
    } else {
        [255, 255, 255]
    };
    let Some(y) = origin_y.checked_add(cell_height.saturating_sub(UNDERLINE_INSET)) else {
        return;
    };
    for column in 0..cell_width {
        let Some(x) = origin_x.checked_add(column) else {
            break;
        };
        put_pixel(pixels, width, height, x, y, rule);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Rule {
    Strike,
    Underline,
}

/// The rule, if any, over the pixel at `column` and `row` of a cell
/// `width` by `height`, `x` across the surface: the strike at the cell's
/// middle, else the underline in its style. The cell places them, not
/// the glyph, so a rule is where it is whichever face draws the cell. A
/// curly underline is a triangle wave up from the cell's last row two
/// units and back, a dotted one alternates unit-long dots and gaps, both
/// running on the surface's `x` so they meet across cells, and a dashed
/// one leaves a gap of the cell's last quarter, at least a pixel.
fn rule(
    attributes: &Attributes,
    (width, height): (usize, usize),
    x: usize,
    column: usize,
    row: usize,
) -> Option<Rule> {
    if !attributes.strike && attributes.underline == Underline::None {
        return None;
    }
    if attributes.strike && row == height / 2 {
        return Some(Rule::Strike);
    }
    let base = height.saturating_sub(UNDERLINE_INSET);
    let unit = (height / PATTERN_SCALE).max(1);
    let under = match attributes.underline {
        Underline::None => false,
        Underline::Single => row == base,
        Underline::Double => row == base || Some(row) == base.checked_sub(DOUBLE_GAP),
        Underline::Curly => {
            let crest = unit.saturating_mul(2);
            let period = crest.saturating_mul(2);
            let phase = x.checked_rem(period).unwrap_or(0);
            let rise = if phase <= crest {
                phase
            } else {
                period.saturating_sub(phase)
            };
            Some(row)
                == height
                    .checked_sub(1)
                    .and_then(|last| last.checked_sub(rise))
        }
        Underline::Dotted => row == base && x.checked_div(unit).is_some_and(|dot| dot % 2 == 0),
        Underline::Dashed => row == base && column < width.saturating_sub((width / 4).max(1)),
    };
    under.then_some(Rule::Underline)
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

/// Whether the pixel at `glyph_column` of a glyph row's `bits`, `width`
/// columns wide, is set, after the shear moves which source column it
/// reads and bold adds the column to its left. Both effects are clipped by
/// construction: a source column left of the glyph does not exist, and a
/// set bit pushed past the cell's right edge is never asked for.
fn lit(bits: &[u8], width: usize, glyph_column: usize, bold: bool, lean: usize) -> bool {
    // Most-significant bit first, as PSF2 stores a row (`Font::pixel`).
    let set = |column: usize| {
        column < width
            && bits
                .get(column / 8)
                .is_some_and(|byte| byte >> (7 - column % 8) & 1 == 1)
    };
    let Some(source) = glyph_column.checked_sub(lean) else {
        return false;
    };
    set(source) || (bold && source.checked_sub(1).is_some_and(set))
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

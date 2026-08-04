//! Model-to-pixels rendering (DESIGN.md section 11).
//!
//! Pure: it reads a snapshot and writes XRGB8888. It opens nothing, owns
//! nothing, and allocates nothing in the cell loop, so section 14's exact
//! P6 PPM oracle can drive it without a compositor, a framebuffer, or a
//! child process.

use crate::font::Font;
use crate::term::{Attributes, Cell, Color, Terminal};

pub const BYTES_PER_PIXEL: usize = 4;

/// xterm's default first sixteen. Only these are a table: 16..232 is the
/// 6x6x6 cube and 232..256 the grey ramp, both computed from the
/// arithmetic that defines them so the entries cannot drift from it.
const BASE: [[u8; 3]; 16] = [
    [0x00, 0x00, 0x00],
    [0xcd, 0x00, 0x00],
    [0x00, 0xcd, 0x00],
    [0xcd, 0xcd, 0x00],
    [0x00, 0x00, 0xee],
    [0xcd, 0x00, 0xcd],
    [0x00, 0xcd, 0xcd],
    [0xe5, 0xe5, 0xe5],
    [0x7f, 0x7f, 0x7f],
    [0xff, 0x00, 0x00],
    [0x00, 0xff, 0x00],
    [0xff, 0xff, 0x00],
    [0x5c, 0x5c, 0xff],
    [0xff, 0x00, 0xff],
    [0x00, 0xff, 0xff],
    [0xff, 0xff, 0xff],
];
const CUBE_START: usize = 16;
const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
const RAMP_START: usize = 232;
const RAMP_BASE: u8 = 8;
const RAMP_STEP: u8 = 10;

/// Default ink is entry 7 on entry 0 rather than a seventeenth colour, so
/// `SGR 39`/`49` land back on the palette the child can also name.
const DEFAULT_FOREGROUND: usize = 7;
const DEFAULT_BACKGROUND: usize = 0;

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
        let foreground = entries
            .get(DEFAULT_FOREGROUND)
            .copied()
            .unwrap_or([0xff, 0xff, 0xff]);
        let background = entries.get(DEFAULT_BACKGROUND).copied().unwrap_or([0, 0, 0]);
        Self {
            entries,
            foreground,
            background,
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
    CUBE_LEVELS.get(axis % CUBE_LEVELS.len()).copied().unwrap_or(0)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Cursor {
    pub row: usize,
    pub column: usize,
    pub visible: bool,
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
        }
    }

    /// The main loop overrides the model's cursor while the scrollback
    /// viewport is open; until that loop lands, only the spec calls this.
    #[allow(dead_code)]
    pub fn with_cursor(mut self, cursor: Cursor) -> Self {
        self.cursor = cursor;
        self
    }

    /// Scroll the viewport back by `lines` of primary-screen history. Rows
    /// above the split come from history and the rest from the live screen,
    /// so a partially scrolled viewport shows both at once.
    #[allow(dead_code)]
    pub fn scrolled_back(mut self, lines: usize) -> Self {
        self.viewport = lines.min(self.terminal.history_lines());
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
        found.unwrap_or(BLANK)
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
    let (cell_width, cell_height) = (font.width(), font.height());
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
                pixels, width, height, font, palette, &cell, origin_x, origin_y,
            );
        }
    }

    if let Some((row, column)) = snapshot.cursor() {
        paint_cursor(pixels, width, height, font, palette, snapshot, row, column);
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
    palette: &Palette,
    cell: &Cell,
    origin_x: usize,
    origin_y: usize,
) {
    let attributes = &cell.attributes;
    let ink = Ink::new(attributes, palette);
    let glyph = font.index(cell.scalar);
    let underline = font.height().saturating_sub(UNDERLINE_INSET);
    let strike = font.height() / 2;
    for glyph_row in 0..font.height() {
        let Some(y) = origin_y.checked_add(glyph_row) else {
            return;
        };
        if y >= height {
            return;
        }
        let ruled = (attributes.underline && glyph_row == underline)
            || (attributes.strike && glyph_row == strike);
        let lean = if attributes.italic {
            shear(glyph_row, font.height())
        } else {
            0
        };
        for glyph_column in 0..font.width() {
            let Some(x) = origin_x.checked_add(glyph_column) else {
                break;
            };
            if x >= width {
                break;
            }
            let set = ruled || lit(font, glyph, glyph_column, glyph_row, attributes.bold, lean);
            let color = if set { ink.foreground } else { ink.background };
            put_pixel(pixels, width, height, x, y, color);
        }
    }
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
    palette: &Palette,
    snapshot: &Snapshot,
    row: usize,
    column: usize,
) {
    let (Some(origin_x), Some(origin_y)) = (
        column.checked_mul(font.width()),
        row.checked_mul(font.height()),
    ) else {
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
            pixels, width, height, font, palette, &flipped, origin_x, origin_y,
        );
        return;
    }
    // Unfocused it is a hollow box: present, but not claiming the keyboard.
    let ink = Ink::new(&cell.attributes, palette);
    for glyph_row in 0..font.height() {
        for glyph_column in 0..font.width() {
            let edge = glyph_row == 0
                || glyph_column == 0
                || glyph_row.saturating_add(1) == font.height()
                || glyph_column.saturating_add(1) == font.width();
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

/// Encode a rendered surface as binary P6 PPM, section 14's visual oracle.
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
    let header = format!("P6\n{width} {height}\n255\n");
    let body = expected / BYTES_PER_PIXEL * 3;
    let mut out = Vec::with_capacity(header.len().saturating_add(body));
    out.extend_from_slice(header.as_bytes());
    let (chunks, _) = pixels.as_chunks::<BYTES_PER_PIXEL>();
    for [blue, green, red, _] in chunks.iter().copied() {
        out.push(red);
        out.push(green);
        out.push(blue);
    }
    Ok(out)
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
    let (chunks, _) = pixels.as_chunks::<BYTES_PER_PIXEL>();
    if !chunks.iter().any(|pixel| *pixel != [0, 0, 0, 0]) {
        return Err("render selftest drew no glyph pixels".into());
    }
    let encoded = ppm(&pixels, width, height)?;
    let (decoded, decoded_width, decoded_height) = from_ppm(&encoded)?;
    if (decoded, decoded_width, decoded_height) != (pixels, width, height) {
        return Err("render selftest did not round-trip through P6".into());
    }
    Ok(())
}

#[cfg(test)]
#[path = "render_spec.rs"]
mod spec;

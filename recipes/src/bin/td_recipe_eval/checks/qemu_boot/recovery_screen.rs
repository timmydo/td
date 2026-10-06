//! Reads the recovery key td-setup's recovery-key page shows from a
//! 1280x800 capture of the display, and nothing else: the host learns the
//! key from the page's pixels alone (td-install/ENCRYPTION.md "Acceptance
//! evidence", increment 7). Its references are the ten digits and the
//! hyphen drawn by td-ui's own rasterizer, `Face` over the outline face's
//! regular style fitted to the grid at the page's scale, in the colours the
//! page draws the key in (td-setup `recovery.rs`: `GlyphStyle::medium(INK,
//! CHROME)` in the default theme). Every cell of the key's row must equal
//! exactly one reference; a row whose hyphens match and one of whose digit
//! cells does not is a misread, and fails rather than guess.

use super::update::mix;
use crate::atlas::{Slot, Style, PAGE_WIDTH};
use crate::face::Face;

/// td-ui's palette ink and chrome (`td_ui::raster::INK`, `CHROME`), which
/// the default theme draws as themselves.
const INK: [u8; 3] = [0x48, 0x45, 0x3f];
const CHROME: [u8; 3] = [0xe1, 0xdb, 0xcf];
const SCREEN_WIDTH: usize = 1280;
const SCREEN_HEIGHT: usize = 800;
/// td-ui's bitmap grid, which the face is fitted to.
const GRID_WIDTH: usize = 8;
const GRID_HEIGHT: usize = 16;
/// The scales td-setup's page may be drawn at.
const SCALES: &[usize] = &[1, 2];
pub(super) const DIGITS: usize = 48;
const GROUP: usize = 6;
/// The display form's cells: eight groups of six digits, seven hyphens.
const COLUMNS: usize = DIGITS + DIGITS / GROUP - 1;

/// The reference cells at one scale: the ten digits, then the hyphen.
struct Cells {
    width: usize,
    height: usize,
    cells: Vec<Vec<[u8; 3]>>,
}

impl Cells {
    fn render(regular: &[u8], scale: usize) -> Result<Self, String> {
        let (width, height) = (GRID_WIDTH * scale, GRID_HEIGHT * scale);
        let mut face = Face::fit(regular.into(), None, width, height)
            .map_err(|error| format!("key face at scale {scale}: {error}"))?;
        let cell = face.cell();
        if (cell.width, cell.height) != (width, height) {
            return Err(format!(
                "the key face's cell is {}x{}, not the grid's {width}x{height}",
                cell.width, cell.height
            ));
        }
        let mut cells = Vec::new();
        for character in ('0'..='9').chain(['-']) {
            let mut pixels = vec![CHROME; width * height];
            let Slot::Placed(entry) = face.glyph(Style::Regular, character) else {
                return Err(format!("the key face does not draw {character:?}"));
            };
            let page = face.atlas().page();
            for row in 0..entry.height {
                for column in 0..entry.width {
                    let alpha = page
                        .get((entry.y + row) * PAGE_WIDTH + entry.x + column)
                        .copied()
                        .unwrap_or(0);
                    if alpha == 0 {
                        continue;
                    }
                    // Clipped to the cell, as the raster clips each glyph.
                    let x = i64::try_from(cell.pen + column)
                        .map_err(|_| "key glyph column")?
                        .saturating_add(i64::from(entry.left));
                    let y = i64::try_from(cell.baseline + row)
                        .map_err(|_| "key glyph row")?
                        .saturating_sub(i64::from(entry.top));
                    let (Ok(x), Ok(y)) = (usize::try_from(x), usize::try_from(y)) else {
                        continue;
                    };
                    if x < width && y < height {
                        if let Some(pixel) = pixels.get_mut(y * width + x) {
                            *pixel = mix(CHROME, INK, alpha);
                        }
                    }
                }
            }
            if pixels.iter().all(|pixel| *pixel == CHROME) {
                return Err(format!("the key face draws {character:?} blank"));
            }
            cells.push(pixels);
        }
        for (index, cell) in cells.iter().enumerate() {
            if cells.iter().skip(index + 1).any(|other| other == cell) {
                return Err(format!(
                    "two of the key face's references are alike at scale {scale}"
                ));
            }
        }
        Ok(Self {
            width,
            height,
            cells,
        })
    }

    /// Whether the capture's cell at `(x, y)` is reference `index`.
    fn is(&self, pixels: &[u8], x: usize, y: usize, index: usize) -> bool {
        let Some(cell) = self.cells.get(index) else {
            return false;
        };
        (0..self.height).all(|row| {
            let start = ((y + row) * SCREEN_WIDTH + x) * 3;
            pixels
                .get(start..start + self.width * 3)
                .is_some_and(|line| {
                    line.as_chunks::<3>()
                        .0
                        .iter()
                        .zip(cell.iter().skip(row * self.width))
                        .all(|(seen, drawn)| seen == drawn)
                })
        })
    }

    /// The key whose row starts at `(x, y)`, if every hyphen is there:
    /// `Ok(None)` when one is not, an error when a digit cell is no digit.
    fn key_at(&self, pixels: &[u8], x: usize, y: usize) -> Result<Option<[u8; DIGITS]>, String> {
        let hyphen = 10;
        let at = |column: usize| x + column * self.width;
        if !(1..DIGITS / GROUP).all(|group| self.is(pixels, at(group * (GROUP + 1) - 1), y, hyphen))
        {
            return Ok(None);
        }
        let mut digits = [0; DIGITS];
        let mut slots = digits.iter_mut();
        for column in (0..COLUMNS).filter(|column| (column + 1) % (GROUP + 1) != 0) {
            let found: Vec<usize> = (0..10)
                .filter(|digit| self.is(pixels, at(column), y, *digit))
                .collect();
            let (Some(slot), [digit]) = (slots.next(), found.as_slice()) else {
                return Err(format!(
                    "the key's cell {column} at ({}, {y}) matches {} digits, not one",
                    at(column),
                    found.len()
                ));
            };
            *slot = b'0' + u8::try_from(*digit).map_err(|_| "digit index")?;
        }
        Ok(Some(digits))
    }
}

/// The references at every scale the page may be drawn at.
pub(crate) struct KeyGlyphs {
    scales: Vec<Cells>,
}

impl KeyGlyphs {
    /// From the face file's regular style.
    pub(crate) fn render(regular: &[u8]) -> Result<Self, String> {
        let scales = SCALES
            .iter()
            .map(|scale| Cells::render(regular, *scale))
            .collect::<Result<_, _>>()?;
        Ok(Self { scales })
    }

    /// The one key row a 1280x800 RGB capture shows: `Ok(None)` while none
    /// is shown, an error for a misread cell or a second row.
    pub(crate) fn read(&self, pixels: &[u8]) -> Result<Option<[u8; DIGITS]>, String> {
        if pixels.len() != SCREEN_WIDTH * SCREEN_HEIGHT * 3 {
            return Err("the key capture is not 1280x800 RGB".into());
        }
        let mut found = None;
        for cells in &self.scales {
            let row_width = COLUMNS * cells.width;
            for y in 0..=SCREEN_HEIGHT - cells.height {
                for x in 0..=SCREEN_WIDTH - row_width {
                    if let Some(key) = cells.key_at(pixels, x, y)? {
                        if found.replace(key).is_some() {
                            return Err("the capture shows more than one key row".into());
                        }
                    }
                }
            }
        }
        Ok(found)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    /// Bytes that are not a face draw no reference: refused before anything
    /// is read.
    #[test]
    fn a_face_that_is_not_one_is_refused() {
        assert!(KeyGlyphs::render(&[0; 64]).is_err());
    }

    /// Distinct references drawn into a capture are read back, at either
    /// scale and anywhere; a cell that is no digit is a misread, and a
    /// capture with no hyphen row shows no key.
    #[test]
    fn a_key_row_is_read_exactly_or_refused() {
        let reference = |scale: usize| {
            let (width, height) = (GRID_WIDTH * scale, GRID_HEIGHT * scale);
            let cells = (0..11u8)
                .map(|index| {
                    let mut cell = vec![CHROME; width * height];
                    // Index `i` inks its first `i + 1` pixels: all distinct.
                    for pixel in cell.iter_mut().take(usize::from(index) + 1) {
                        *pixel = INK;
                    }
                    cell
                })
                .collect();
            Cells {
                width,
                height,
                cells,
            }
        };
        let glyphs = KeyGlyphs {
            scales: SCALES.iter().map(|scale| reference(*scale)).collect(),
        };
        let key: Vec<u8> = (0..DIGITS)
            .map(|index| b'0' + (index * 7 % 10) as u8)
            .collect();
        let draw = |scale: usize, left: usize, top: usize, key: &[u8]| {
            let cells = reference(scale);
            let mut pixels = CHROME.repeat(SCREEN_WIDTH * SCREEN_HEIGHT);
            let mut digits = key.iter();
            for column in 0..COLUMNS {
                let index = if (column + 1) % (GROUP + 1) == 0 {
                    10
                } else {
                    usize::from(digits.next().unwrap() - b'0')
                };
                let cell = &cells.cells[index];
                for row in 0..cells.height {
                    for col in 0..cells.width {
                        let x = left + column * cells.width + col;
                        let at = ((top + row) * SCREEN_WIDTH + x) * 3;
                        pixels[at..at + 3].copy_from_slice(&cell[row * cells.width + col]);
                    }
                }
            }
            pixels
        };
        for (scale, left, top) in [(1, 8, 120), (2, 16, 240), (1, 0, 0)] {
            let pixels = draw(scale, left, top, &key);
            assert_eq!(glyphs.read(&pixels).unwrap().unwrap().to_vec(), key);
        }
        assert_eq!(
            glyphs
                .read(&CHROME.repeat(SCREEN_WIDTH * SCREEN_HEIGHT))
                .unwrap(),
            None
        );
        // One digit cell changed by one pixel matches no digit.
        let mut misread = draw(1, 8, 120, &key);
        let at = (130 * SCREEN_WIDTH + 8 + 5) * 3;
        misread[at..at + 3].copy_from_slice(&[0, 0, 0]);
        assert!(glyphs.read(&misread).is_err());
        assert!(glyphs.read(&[0; 12]).is_err());
    }
}

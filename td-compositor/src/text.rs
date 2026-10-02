//! The chrome's text: title bands, the status bar, the launcher and the key
//! sheet. Each character is one cell of Unifont's 8x16 grid, drawn through
//! td-ui's pinned outline face fitted to that cell when the compositor was
//! handed one (td-ui/DESIGN.md, "The grid fit"), and through Unifont where it
//! was not or where the face lacks the character. The cells coincide, so the
//! face never moves a layout or a hit test.
//!
//! The trusted attention display does not draw through here: it paints only
//! compiled-in glyphs, so no file the compositor reads at runtime reaches it.

use std::cell::RefCell;
use std::ffi::OsStr;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use crate::atlas::{Entry, Slot, Style, PAGE_WIDTH};
use crate::face::Face;
use crate::font::Font;

/// td-ui's test encoder: real sfnt bytes, so no fetched font is an oracle.
/// The target recipe stages it flat, as it stages td-authd's test files.
#[cfg(test)]
#[cfg_attr(
    not(feature = "target-recipe"),
    path = "../../td-ui/tests/fonts/mod.rs"
)]
#[cfg_attr(feature = "target-recipe", path = "tests/fonts.rs")]
mod fonts;

pub(crate) const CELL_WIDTH: usize = 8;
pub(crate) const CELL_HEIGHT: usize = 16;

/// `(left, top, width, height)`, as `ui::fill` takes it.
pub(crate) type Rect = (usize, usize, usize, usize);

/// Parsed once per process: a scene is built per test, and the face is
/// hundreds of kilobytes of hex. A face that failed to parse draws no text,
/// which the committed face's own test rules out.
fn unifont() -> Option<&'static Font> {
    static UNIFONT: OnceLock<Option<Font>> = OnceLock::new();
    UNIFONT.get_or_init(|| crate::font::pinned().ok()).as_ref()
}

/// The chrome's text for a compositor starting with `setting` as its
/// `TD_UI_FACE`: `bitmap` keeps Unifont and reads nothing. Under terminal
/// authority only the image's immutable face directory is read, so no file a
/// user can write reaches the parser of the process that owns the trusted
/// path; a direct compositor searches the host's font places as td-ui's
/// programs do. Any failure says so once and draws Unifont.
pub(crate) fn load(setting: Option<&OsStr>, authority: bool) -> Text {
    use crate::face_file::{self, REGULAR};
    if !face_file::wanted(setting) {
        return Text::default();
    }
    let dir = if authority {
        Ok(PathBuf::from(face_file::DIR))
    } else {
        face_file::find(&face_file::host_places())
    };
    let loaded = dir.and_then(|dir| {
        let bytes = face_file::read(&dir, REGULAR)?;
        Text::fitted(bytes.into()).map_err(|why| format!("{}: {why}", dir.join(REGULAR).display()))
    });
    loaded.unwrap_or_else(|why| {
        eprintln!("td-compositor: the chrome draws with Unifont: {why}");
        Text::default()
    })
}

/// Whether Unifont, which draws whatever the face lacks, has `character`:
/// what a caller with a fixed vocabulary asserts its strings against.
#[cfg(test)]
pub(crate) fn covered(character: char) -> bool {
    unifont().is_some_and(|font| font.covers(character))
}

/// The atlas is filled on first use of each character, while the scene paints
/// through `&self`; one thread paints, so the cell is never contended, and a
/// contended borrow would draw Unifont rather than fail.
#[derive(Default)]
pub(crate) struct Text {
    outline: RefCell<Option<Face>>,
}

impl Text {
    /// The pinned face's regular style, fitted to the cell.
    pub(crate) fn fitted(regular: Arc<[u8]>) -> Result<Text, String> {
        let face =
            Face::fit(regular, None, CELL_WIDTH, CELL_HEIGHT).map_err(|why| why.to_string())?;
        Ok(Text {
            outline: RefCell::new(Some(face)),
        })
    }

    #[cfg(test)]
    pub(crate) fn outline(&self) -> bool {
        self.outline.try_borrow().is_ok_and(|face| face.is_some())
    }

    /// One cell per character, not per byte: titles are client UTF-8.
    pub(crate) fn width(text: &str) -> usize {
        text.chars().count().saturating_mul(CELL_WIDTH)
    }

    /// `text` from (`x`, `y`) inside `clip`, in `ink`. `ground` is what the
    /// caller filled beneath it: the outline's edge pixels are blended from
    /// it toward the ink, as td-ui's raster blends, never from a read of the
    /// frame.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn draw(
        &self,
        frame: &mut [u8],
        width: usize,
        height: usize,
        stride: usize,
        (x, y): (usize, usize),
        text: &str,
        (ink, ground): ([u8; 4], [u8; 4]),
        clip: Rect,
    ) {
        let clip = crate::ui::intersect(clip, (0, 0, width, height));
        if crate::ui::intersect((x, y, usize::MAX, CELL_HEIGHT), clip).3 == 0 {
            return;
        }
        let clip_right = clip.0.saturating_add(clip.2);
        let mut outline = self.outline.try_borrow_mut().ok();
        for (column, character) in text.chars().enumerate() {
            let left = x.saturating_add(column.saturating_mul(CELL_WIDTH));
            // The origin only advances, so nothing after this lands either;
            // a title is up to 256 characters against a band that holds few.
            if left >= clip_right {
                break;
            }
            let cell = crate::ui::intersect((left, y, CELL_WIDTH, CELL_HEIGHT), clip);
            if cell.2 == 0 || cell.3 == 0 {
                continue;
            }
            let face = outline.as_mut().and_then(|borrow| borrow.as_mut());
            let slot = face.map(|face| {
                let slot = face.glyph(Style::Regular, character);
                (slot, face.cell())
            });
            let mut target = Target {
                frame: &mut *frame,
                stride,
                clip: cell,
            };
            match slot {
                Some((Slot::Blank, _)) => {}
                Some((Slot::Placed(entry), metrics)) => {
                    let page = outline
                        .as_ref()
                        .and_then(|borrow| borrow.as_ref())
                        .map(|face| face.atlas().page());
                    if let Some(page) = page {
                        let pen = (
                            left.saturating_add(metrics.pen),
                            y.saturating_add(metrics.baseline),
                        );
                        target.blend(page, entry, pen, ink, ground);
                    }
                }
                Some((Slot::Missing, _)) | None => target.bitmap(character, (left, y), ink),
            }
        }
    }
}

struct Target<'a> {
    frame: &'a mut [u8],
    stride: usize,
    /// One cell, already inside the frame.
    clip: Rect,
}

impl Target<'_> {
    fn put(&mut self, x: usize, y: usize, color: [u8; 4]) {
        let (left, top, width, height) = self.clip;
        if x < left || y < top || x >= left.saturating_add(width) || y >= top.saturating_add(height)
        {
            return;
        }
        let Some(at) = y
            .checked_mul(self.stride)
            .and_then(|row| row.checked_add(x.checked_mul(4)?))
        else {
            return;
        };
        if let Some(pixel) = at
            .checked_add(4)
            .and_then(|end| self.frame.get_mut(at..end))
        {
            pixel.copy_from_slice(&color);
        }
    }

    fn bitmap(&mut self, character: char, (left, top): (usize, usize), ink: [u8; 4]) {
        let Some(font) = unifont() else {
            return;
        };
        let index = font.index(character);
        for row in 0..font.height().min(CELL_HEIGHT) {
            for column in 0..font.width().min(CELL_WIDTH) {
                if font.pixel(index, column, row) {
                    self.put(left.saturating_add(column), top.saturating_add(row), ink);
                }
            }
        }
    }

    /// `entry`'s coverage from the atlas page, its bearing taken from the
    /// pen: each covered pixel is `ground` moved toward `ink` by it.
    fn blend(
        &mut self,
        page: &[u8],
        entry: Entry,
        (pen_x, pen_y): (usize, usize),
        ink: [u8; 4],
        ground: [u8; 4],
    ) {
        // Widened before the top is negated, so no bearing can overflow.
        let origin = |pen: usize, bearing: i64| {
            i64::try_from(pen)
                .ok()
                .and_then(|pen| pen.checked_add(bearing))
        };
        let (Some(glyph_x), Some(glyph_y)) = (
            origin(pen_x, i64::from(entry.left)),
            origin(pen_y, -i64::from(entry.top)),
        ) else {
            return;
        };
        for row in 0..entry.height {
            let Some(y) = i64::try_from(row)
                .ok()
                .and_then(|row| glyph_y.checked_add(row))
                .and_then(|y| usize::try_from(y).ok())
            else {
                continue;
            };
            let Some(line) = entry
                .y
                .checked_add(row)
                .and_then(|row| row.checked_mul(PAGE_WIDTH))
                .and_then(|line| line.checked_add(entry.x))
            else {
                return;
            };
            for column in 0..entry.width {
                let Some(x) = i64::try_from(column)
                    .ok()
                    .and_then(|column| glyph_x.checked_add(column))
                    .and_then(|x| usize::try_from(x).ok())
                else {
                    continue;
                };
                let alpha = line
                    .checked_add(column)
                    .and_then(|at| page.get(at))
                    .copied()
                    .unwrap_or(0);
                if alpha != 0 {
                    self.put(x, y, mix(ground, ink, alpha));
                }
            }
        }
    }
}

/// `from` moved toward `to` by `alpha` of 255 in each colour byte, rounded,
/// as td-ui's raster mixes; the fourth byte is the ink's.
fn mix(from: [u8; 4], to: [u8; 4], alpha: u8) -> [u8; 4] {
    let alpha = u16::from(alpha);
    let mut color = to;
    for (out, (from, to)) in color.iter_mut().zip(from.iter().zip(to.iter())).take(3) {
        let mixed = (u16::from(*from) * (255 - alpha) + u16::from(*to) * alpha + 127) / 255;
        *out = u8::try_from(mixed).unwrap_or(u8::MAX);
    }
    color
}

#[cfg(test)]
mod tests {
    use super::*;

    const INK: [u8; 4] = [0xf0, 0xe0, 0xd0, 0];
    const GROUND: [u8; 4] = [0x10, 0x20, 0x30, 0];

    fn drawn(text: &Text, string: &str, clip: Rect) -> Vec<u8> {
        let (width, height) = (64usize, CELL_HEIGHT);
        let mut frame = vec![0u8; width * height * 4];
        for pixel in frame.as_chunks_mut::<4>().0 {
            *pixel = GROUND;
        }
        text.draw(
            &mut frame,
            width,
            height,
            width * 4,
            (0, 0),
            string,
            (INK, GROUND),
            clip,
        );
        frame
    }

    #[test]
    fn the_cell_is_unifonts() {
        let font = unifont().unwrap();
        assert_eq!((font.width(), font.height()), (CELL_WIDTH, CELL_HEIGHT));
        assert_eq!(Text::width("td-mail"), 7 * CELL_WIDTH);
        assert_eq!(Text::width("é"), CELL_WIDTH);
    }

    #[test]
    fn without_a_face_each_character_is_its_unifont_cell() {
        let text = Text::default();
        assert!(!text.outline());
        let frame = drawn(&text, "Ab", (0, 0, 64, CELL_HEIGHT));
        let font = unifont().unwrap();
        for (column, character) in "Ab".chars().enumerate() {
            let index = font.index(character);
            for row in 0..CELL_HEIGHT {
                for x in 0..CELL_WIDTH {
                    let at = (row * 64 + column * CELL_WIDTH + x) * 4;
                    let expected = if font.pixel(index, x, row) {
                        INK
                    } else {
                        GROUND
                    };
                    assert_eq!(frame[at..at + 4], expected, "{character} at {x},{row}");
                }
            }
        }
        // Lower case stays lower case: the old chrome glyphs folded it.
        assert_ne!(
            drawn(&text, "a", (0, 0, 64, CELL_HEIGHT)),
            drawn(&text, "A", (0, 0, 64, CELL_HEIGHT))
        );
    }

    #[test]
    fn nothing_lands_outside_the_clip() {
        let text = Text::default();
        let frame = drawn(&text, "MMMMMMMM", (3, 2, 10, 9));
        for (index, pixel) in frame.as_chunks::<4>().0.iter().enumerate() {
            let (x, y) = (index % 64, index / 64);
            if !(3..13).contains(&x) || !(2..11).contains(&y) {
                assert_eq!(*pixel, GROUND, "{x},{y}");
            }
        }
        assert!(frame.as_chunks::<4>().0.contains(&INK));
    }

    #[test]
    fn a_coverage_blend_runs_from_the_ground_to_the_ink() {
        assert_eq!(mix(GROUND, INK, 0)[..3], GROUND[..3]);
        assert_eq!(mix(GROUND, INK, 255)[..3], INK[..3]);
        let half = mix([0, 0, 0, 0], [255, 255, 255, 9], 128);
        assert_eq!(half, [128, 128, 128, 9]);
    }

    fn face() -> Text {
        use fonts::{square, Builder, Glyph, Segment};
        let mut builder = Builder::new(vec![
            Glyph::Empty,
            Glyph::Simple(vec![square(50, 0, 400)]),
            Glyph::Empty,
        ]);
        let map = |scalar: char, glyph: u16| {
            let code = u32::from(scalar) as u16;
            Segment::Delta(code, code, glyph.wrapping_sub(code))
        };
        builder.format4 = vec![map(' ', 2), map('0', 1)];
        Text::fitted(builder.font().into()).unwrap()
    }

    #[test]
    fn a_face_draws_its_glyphs_in_their_cells_and_unifont_the_rest() {
        let outline = face();
        assert!(outline.outline());
        let clip = (0, 0, 64, CELL_HEIGHT);
        let frame = drawn(&outline, "0 Z", clip);
        let column = |frame: &[u8], cell: usize| -> Vec<[u8; 4]> {
            (0..CELL_HEIGHT)
                .flat_map(|y| (0..CELL_WIDTH).map(move |x| (x, y)))
                .map(|(x, y)| {
                    let at = (y * 64 + cell * CELL_WIDTH + x) * 4;
                    frame[at..at + 4].try_into().unwrap()
                })
                .collect()
        };
        let zero = column(&frame, 0);
        // Covered: ink inside, and the edges between ground and ink.
        assert!(zero.contains(&INK));
        assert!(zero.iter().any(|pixel| *pixel != INK && *pixel != GROUND));
        assert_ne!(zero, column(&drawn(&Text::default(), "0", clip), 0));
        // The face has a space and it covers nothing.
        assert!(column(&frame, 1).iter().all(|pixel| *pixel == GROUND));
        // The face lacks Z, so Z is Unifont's.
        assert_eq!(
            column(&frame, 2),
            column(&drawn(&Text::default(), "  Z", clip), 2)
        );
        // Past the text, nothing.
        for (index, pixel) in frame.as_chunks::<4>().0.iter().enumerate() {
            if index % 64 >= 3 * CELL_WIDTH {
                assert_eq!(*pixel, GROUND, "{} past the text", index % 64);
            }
        }
    }

    #[test]
    fn a_refused_face_is_an_error_not_a_text() {
        assert!(Text::fitted(Arc::from(&b"not a font"[..])).is_err());
    }

    #[test]
    fn the_bitmap_setting_reads_nothing() {
        // Either mode: `bitmap` is answered before any path is chosen.
        for authority in [false, true] {
            assert!(!load(Some(OsStr::new("bitmap")), authority).outline());
        }
    }
}

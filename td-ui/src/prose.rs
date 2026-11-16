//! Prose: text in proportional faces, each glyph its own width, laid
//! into lines by pixel width (td-ui/DESIGN.md, "Proportional faces").
//! `Faces` holds the proportional families' style bytes, and the
//! monospace face's when a program has it; `Metrics` measures with them
//! at a surface's scale, at the prose size or any `Size` within its
//! bound, and answers the cell without them; `wrap` lays styled runs into
//! lines and `draw` streams a line's glyphs. A raster given the same
//! `Faces` (`Raster::with_prose`) draws what `Metrics` measured, from the
//! face `Metrics::source` names at the glyph's size. Nothing here reads
//! the environment, a clock, a descriptor or the filesystem: the
//! consumer hands over the bytes (`pinned_face::prose_or_note` reads the
//! pinned ones).

use td_alloc::Budget;
mod cache;
mod items;
mod pages;
mod wrap;
pub use items::Items;
use std::ops::Range;
use std::sync::Arc;
pub use wrap::{wrap, wrap_at_most, Line, Piece, Placed};

use crate::atlas::{Atlas, Slot, Style, PAGE_HEIGHT, PAGE_WIDTH};
use crate::face::Face;
use crate::font::stand_in;
use crate::raster::{Draw, GlyphStyle, Primitive, Rect, Scale, TextFamily};
use crate::sfnt::{Error, Font};
use crate::typeface::Typeface;
use crate::{CELL_HEIGHT, CELL_WIDTH};

/// The prose size in pixels per em at scale one; the device size is the
/// scale times it.
pub const SIZE: u16 = 16;
/// The least prose size a glyph may name, in device pixels per em: the
/// outline face's least, at which a Latin x-height is three pixels.
pub const MIN_SIZE: u16 = 6;
/// The greatest prose size a glyph may name, in device pixels per em: a
/// 64-pixel heading at scale three, 96 at two. An atlas page holds 44 to
/// 58 Latin glyphs of Noto at it.
pub const MAX_SIZE: u16 = 192;
/// The pages the prose faces hold for sizes other than a raster's own
/// (`SIZE` at its scale): one family at one size each, the page drawn
/// from longest ago remade in its place when another is needed.
pub const PAGES: usize = 8;
/// The keys a page of another size holds before it resets, so its key map
/// is bounded with its page: ample for a frame's text in one family at
/// one size, its four styles together.
pub const PAGE_KEYS: usize = 1024;
/// A key map node's bytes at most: the ordered map's internal node of
/// eleven keys and twelve edges, 728 bytes for these keys and slots.
const KEY_NODE: usize = 768;
/// A page of another size at most: its coverage; its key map, whose
/// nodes but the root hold at least five keys each, so at most a quarter
/// of its keys in nodes, and two more; and its shelves, each at least two
/// rows, their array's capacity at most twice their count.
const PAGE_BYTES: usize =
    PAGE_WIDTH * PAGE_HEIGHT + (PAGE_KEYS / 4 + 2) * KEY_NODE + PAGE_HEIGHT * 48;
/// The coverage scratch the pages share, at most: an accumulator of
/// 514 by 512 floats, a 512-square mask and an outline at its point,
/// contour and flag bounds, each at most twice that in capacity (2.9 MB).
const SCRATCH: usize = 3 << 20;
/// The memory the prose faces hold for sizes other than a raster's own,
/// at most: `PAGES` pages with their key maps and shelves, and their
/// shared scratch; 13,512,704 bytes (13.5 MB, 12.9 MiB).
pub const ATLAS: usize = PAGES * PAGE_BYTES + SCRATCH;
/// The bitmap face's baseline below its cell's top: Unifont's 16 rows hold
/// 14 above it and 2 below.
pub const CELL_BASELINE: usize = 14;
/// Subpixels per pixel in which advances are summed.
pub const UNIT: u32 = 64;
/// How far left of its pen a glyph's ink may hang and still be drawn by
/// `draw`: one em of the prose size at the largest scale. A glyph of
/// another `Size` hangs as far as `overhang` says.
pub const OVERHANG: u32 = SIZE as u32 * MAX_SCALE as u32;
/// Measurements `Metrics` holds before it forgets them all and starts
/// again, so its memory is bounded whatever text it measures.
pub const MAX_MEASURES: usize = 8192;
/// The largest scale `Scale` admits, at which `Faces::new` checks a face
/// still sizes.
const MAX_SCALE: u8 = 4;

/// A prose glyph's size: `DEFAULT`, the prose size `SIZE` at the
/// surface's scale, or a whole number of device pixels per em within
/// `MIN_SIZE` and `MAX_SIZE`, which does not move with the scale. A size
/// equal to the default at a raster's scale draws exactly as the default.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Size(u8);

// A size is one byte, so a `GlyphStyle` naming one is no larger than one
// without: wrapping copies a style with every glyph.
const _: () = assert!(MAX_SIZE <= u8::MAX as u16);

impl Size {
    /// The prose size at the surface's scale.
    pub const DEFAULT: Self = Self(0);

    /// `pixels` device pixels per em, refused outside `MIN_SIZE` and
    /// `MAX_SIZE` as `Error::Limit("prose size")`.
    pub fn new(pixels: u32) -> Result<Self, Error> {
        u16::try_from(pixels)
            .ok()
            .filter(|pixels| (MIN_SIZE..=MAX_SIZE).contains(pixels))
            .and_then(|pixels| u8::try_from(pixels).ok())
            .map(Self)
            .ok_or(Error::Limit("prose size"))
    }

    /// `pixels` device pixels per em brought within `MIN_SIZE` and
    /// `MAX_SIZE`.
    pub fn clamped(pixels: u32) -> Self {
        let pixels = pixels.clamp(u32::from(MIN_SIZE), u32::from(MAX_SIZE));
        Self(u8::try_from(pixels).unwrap_or(u8::MAX))
    }

    pub fn is_default(self) -> bool {
        self.0 == 0
    }

    /// Device pixels per em at `scale`.
    pub fn pixels(self, scale: Scale) -> u16 {
        match self.0 {
            0 => default_pixels(scale),
            pixels => u16::from(pixels),
        }
    }

    /// Device pixels per em at any scale, or none for the default.
    fn fixed(self) -> Option<u16> {
        (self.0 != 0).then_some(u16::from(self.0))
    }
}

/// How far left of its pen a glyph of `size` may hang its ink at `scale`,
/// and so how far past a clip `draw` streams it: one em, in pixels.
pub fn overhang(size: Size, scale: Scale) -> u32 {
    u32::from(size.pixels(scale))
}

/// The prose size at `scale`, in pixels per em.
fn default_pixels(scale: Scale) -> u16 {
    SIZE.saturating_mul(u16::try_from(scale.value()).unwrap_or(1))
}

/// The bitmap face's whole scale for prose at `pixels` per em: its 16-row
/// em nearest the size, at least one; the surface's scale at the prose
/// size.
pub(crate) fn bitmap_scale(pixels: u16) -> usize {
    usize::from(pixels.saturating_add(SIZE / 2) / SIZE).max(1)
}

/// Which face draws a scalar: the cell (no prose faces, or a `Cell`
/// glyph), the prose family in its style or regular, the monospace face
/// (its stand-in included), or the bitmap face.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Source {
    Cell,
    Family,
    Mono,
    Bitmap,
}

/// One family's style bytes: the regular, then bold, italic and bold
/// italic as it has them.
#[derive(Clone, Debug)]
struct Styles {
    regular: Arc<[u8]>,
    others: [Option<Arc<[u8]>>; 3],
}

impl Styles {
    /// Every style parsed, and the face sized at the least and greatest
    /// prose sizes and the prose size at the least and greatest scales, so
    /// a face that exists sizes at every size a glyph may name.
    fn checked(regular: Arc<[u8]>, others: [Option<Arc<[u8]>>; 3]) -> Result<Self, (Style, Error)> {
        let styles = Self { regular, others };
        for style in [
            Style::Regular,
            Style::Bold,
            Style::Italic,
            Style::BoldItalic,
        ] {
            let bytes = match style {
                Style::Regular => Some(&styles.regular),
                _ => styles.bytes(style),
            };
            if let Some(bytes) = bytes {
                Font::parse(bytes).map_err(|why| (style, why))?;
            }
        }
        for scale in [1, MAX_SCALE] {
            let scale = Scale::new(scale).map_err(|_| (Style::Regular, Error::Limit("scale")))?;
            styles
                .face(default_pixels(scale))
                .map_err(|why| (Style::Regular, why))?;
        }
        for pixels in [MIN_SIZE, MAX_SIZE] {
            styles.face(pixels).map_err(|why| (Style::Regular, why))?;
        }
        Ok(styles)
    }

    /// A style's bytes other than the regular one's.
    fn bytes(&self, style: Style) -> Option<&Arc<[u8]>> {
        let [bold, italic, bold_italic] = &self.others;
        match style {
            Style::Regular => None,
            Style::Bold => bold.as_ref(),
            Style::Italic => italic.as_ref(),
            Style::BoldItalic => bold_italic.as_ref(),
        }
    }

    /// The style a glyph is drawn in, as `Face::style` resolves it.
    fn resolve(&self, style: Style) -> Style {
        let bold = matches!(style, Style::Bold | Style::BoldItalic);
        let italic = matches!(style, Style::Italic | Style::BoldItalic);
        [
            (bold && italic, Style::BoldItalic),
            (italic, Style::Italic),
            (bold, Style::Bold),
        ]
        .into_iter()
        .find(|&(asked, style)| asked && self.bytes(style).is_some())
        .map_or(Style::Regular, |(_, style)| style)
    }

    /// Every style's bytes, the regular first.
    fn each(&self) -> impl Iterator<Item = &Arc<[u8]>> {
        std::iter::once(&self.regular).chain(self.others.iter().flatten())
    }

    /// The fonts a glyph in `style` is looked for in, in order, as
    /// `Face::glyph` looks, each with its style: the resolved style, then
    /// the regular.
    fn fonts(&self, style: Style) -> impl Iterator<Item = (Style, Font<'_>)> {
        let resolved = self.resolve(style);
        [
            (resolved, self.bytes(resolved)),
            (Style::Regular, Some(&self.regular)),
        ]
        .into_iter()
        .filter_map(|(style, bytes)| Font::parse(bytes?).ok().map(|font| (style, font)))
    }

    /// A style's units per em, the regular's for a style it lacks.
    fn per_em(&self, style: Style) -> u16 {
        Font::parse(self.bytes(style).unwrap_or(&self.regular))
            .map_or(0, |font| font.units_per_em())
    }

    /// Every style as one face at `pixels` per em.
    fn face(&self, pixels: u16) -> Result<Face, Error> {
        self.face_in(Atlas::new(), pixels)
    }

    /// `face`, covering into `atlas`'s storage.
    fn face_in(&self, atlas: Atlas, pixels: u16) -> Result<Face, Error> {
        let [bold, italic, bold_italic] = &self.others;
        Face::sized_in(atlas, self.regular.clone(), bold.clone(), f32::from(pixels))?
            .with_slant(italic.clone(), bold_italic.clone())
    }
}

#[derive(Clone, Debug)]
struct Bytes {
    sans: Styles,
    serif: Styles,
    mono: Option<Styles>,
}

/// A family's style `Faces::new` refused, and why.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Refused {
    pub family: TextFamily,
    pub style: Style,
    pub error: Error,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?} {:?}: {}", self.family, self.style, self.error)
    }
}

impl std::error::Error for Refused {}

/// The prose faces: Sans and Serif in four styles each, and the monospace
/// face's styles when a program has them (`with_mono`), held once and
/// shared. A raster draws through the faces sized at its scale, made on
/// first use and remade when the scale changes, and a glyph of another
/// `Size` through a face at that size on one of at most `PAGES` pages
/// beside them, `ATLAS` bytes in all; a clone shares the bytes and sizes
/// its own.
#[derive(Debug)]
pub struct Faces {
    bytes: Bytes,
    /// The faces at the scale last asked for, or their refusal.
    at: Option<(usize, Option<AtSize>)>,
}

impl Clone for Faces {
    fn clone(&self) -> Self {
        Self {
            bytes: self.bytes.clone(),
            at: None,
        }
    }
}

impl Faces {
    /// Sans and Serif, each its regular, bold, italic and bold italic
    /// styles. A style that does not parse, or a family that does not
    /// size at `MIN_SIZE`, `MAX_SIZE` and the prose size at every scale,
    /// is refused by name.
    pub fn new(sans: [Vec<u8>; 4], serif: [Vec<u8>; 4]) -> Result<Self, Refused> {
        let family = |family: TextFamily, [regular, bold, italic, bold_italic]: [Vec<u8>; 4]| {
            Styles::checked(
                regular.into(),
                [
                    Some(bold.into()),
                    Some(italic.into()),
                    Some(bold_italic.into()),
                ],
            )
            .map_err(|(style, error)| Refused {
                family,
                style,
                error,
            })
        };
        Ok(Self {
            bytes: Bytes {
                sans: family(TextFamily::Sans, sans)?,
                serif: family(TextFamily::Serif, serif)?,
                mono: None,
            },
            at: None,
        })
    }

    /// These faces with the monospace face's styles, shared with
    /// `typeface`: the `Mono` family's outlines, and the first fallback of
    /// a scalar Sans or Serif lacks. A style that does not parse, or a face
    /// that does not size as `new` asks, is refused with the reader's
    /// error.
    pub fn with_mono(mut self, typeface: &Typeface) -> Result<Self, Error> {
        let (regular, others) = typeface.styles();
        self.bytes.mono = Some(Styles::checked(regular, others).map_err(|(_, why)| why)?);
        self.at = None;
        Ok(self)
    }

    /// Whether the monospace face is among them.
    pub fn has_mono(&self) -> bool {
        self.bytes.mono.is_some()
    }

    /// The pages of sizes other than the prose size held at the scale
    /// last drawn at, at most `PAGES`; the scale's own faces are not
    /// among them.
    pub fn pages_held(&self) -> usize {
        self.held().map_or(0, |at| at.pages.held())
    }

    /// The pages made for sizes other than the prose size since the scale
    /// last drawn at was taken, each one remade after it was let go
    /// counted again.
    pub fn pages_made(&self) -> u64 {
        self.held().map_or(0, |at| at.pages.made())
    }

    /// The keys the pages held reset at, the most of any: `PAGE_KEYS`;
    /// none while no page is held.
    pub fn page_keys(&self) -> Option<usize> {
        self.held().and_then(|at| at.pages.keys())
    }

    fn held(&self) -> Option<&AtSize> {
        self.at.as_ref().and_then(|(_, at)| at.as_ref())
    }

    /// The faces sized at `scale`, made when the scale changes; none only
    /// if one is refused there, which `new`'s checks rule out.
    pub(crate) fn at(&mut self, scale: Scale) -> Option<&mut AtSize> {
        let value = scale.value();
        if self.at.as_ref().map(|(at, _)| *at) != Some(value) {
            // The old scale's faces go before the new ones are made.
            self.at = None;
            self.at = Some((value, AtSize::new(&self.bytes, scale).ok()));
        }
        self.at.as_mut().and_then(|(_, at)| at.as_mut())
    }
}

/// The faces at one scale's prose size, each with its own atlas page, the
/// metrics that pick which draws a scalar, and the pages of other sizes.
#[derive(Debug)]
pub(crate) struct AtSize {
    pub(crate) metrics: Metrics,
    sans: Face,
    serif: Face,
    mono: Option<Face>,
    pages: pages::Pages,
}

impl AtSize {
    fn new(bytes: &Bytes, scale: Scale) -> Result<Self, Error> {
        let pixels = default_pixels(scale);
        Ok(Self {
            metrics: Metrics::of(Some(bytes.clone()), scale),
            sans: bytes.sans.face(pixels)?,
            serif: bytes.serif.face(pixels)?,
            mono: bytes
                .mono
                .as_ref()
                .map(|mono| mono.face(pixels))
                .transpose()?,
            pages: pages::Pages::default(),
        })
    }

    /// `scalar`'s slot in `style` from the face `source` names for a
    /// glyph of `family` at `pixels` per em, with the atlas it is on: the
    /// scale's own face at its prose size, else one on the pages of other
    /// sizes, made when none holds it. None when there is no such face.
    pub(crate) fn glyph(
        &mut self,
        (source, family): (Source, TextFamily),
        pixels: u16,
        style: Style,
        scalar: char,
    ) -> Option<(Slot, &Atlas)> {
        if pixels == default_pixels(self.metrics.scale) {
            let face = match (source, family) {
                (Source::Family, TextFamily::Sans) => Some(&mut self.sans),
                (Source::Family, TextFamily::Serif) => Some(&mut self.serif),
                (Source::Mono, _) => self.mono.as_mut(),
                _ => None,
            }?;
            let slot = face.glyph(style, scalar);
            return Some((slot, face.atlas()));
        }
        let bytes = self.metrics.bytes.as_ref()?;
        let (family, styles) = match (source, family) {
            (Source::Family, TextFamily::Sans) => (TextFamily::Sans, &bytes.sans),
            (Source::Family, TextFamily::Serif) => (TextFamily::Serif, &bytes.serif),
            (Source::Mono, _) => (TextFamily::Mono, bytes.mono.as_ref()?),
            _ => return None,
        };
        self.pages.glyph(styles, family, pixels, style, scalar)
    }
}

/// A prose line's box: its height and its baseline below its top, in
/// pixels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LineBox {
    pub height: u32,
    pub baseline: u32,
}

/// How far a glyph's ink may reach past its line box, in pixels: above
/// its top and below its bottom.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Reach {
    pub above: u32,
    pub below: u32,
}

/// A family's regular style's vertical metrics in its own units.
#[derive(Clone, Copy, Debug)]
struct Vertical {
    ascender: i16,
    descender: i16,
    units_per_em: u16,
}

/// What a prose consumer measures with at one scale: each scalar's
/// advance in `UNIT`s of a pixel, from the face that draws it, and the
/// line box, at the prose size or another `Size`. Built from the faces a
/// raster draws with, so measuring and drawing agree; without them it
/// answers the cell at every size.
#[derive(Clone, Debug)]
pub struct Metrics {
    scale: Scale,
    bytes: Option<Bytes>,
    /// Sans's and Serif's regular vertical metrics, each as it parses.
    vertical: [Option<Vertical>; 2],
    /// The line box at the prose size, or the cell's.
    line: LineBox,
    /// The monospace face's one advance and its units per em.
    mono: Option<(u16, u16)>,
    /// Sans's and Serif's units per em in each style, as `STYLES` orders
    /// them.
    per_em: [[u16; 4]; 2],
    /// Every style's lowest and highest reach as its `head` states them,
    /// and its units per em: Sans's, Serif's and the monospace face's.
    bounds: [Option<(i16, i16, u16)>; 12],
    measures: cache::Cache,
}

impl Metrics {
    /// The metrics of `faces` at `scale`, or of the cell without them.
    pub fn new(faces: Option<&Faces>, scale: Scale) -> Self {
        Self::of(faces.map(|faces| faces.bytes.clone()), scale)
    }

    /// Measures using a caller's shared budget for the optional cache.
    /// Cache admission failure falls back to the same uncached measurements.
    pub fn new_in(faces: Option<&Faces>, scale: Scale, budget: &Budget) -> Self {
        let mut out = Self::new(faces, scale);
        out.measures = cache::Cache::new(budget);
        out
    }

    fn of(bytes: Option<Bytes>, scale: Scale) -> Self {
        let cell = LineBox {
            height: (CELL_HEIGHT * scale.value()) as u32,
            baseline: (CELL_BASELINE * scale.value()) as u32,
        };
        let vertical = bytes.as_ref().map_or([None, None], |bytes| {
            [&bytes.sans, &bytes.serif].map(|styles| {
                Font::parse(&styles.regular).ok().map(|font| Vertical {
                    ascender: font.ascender(),
                    descender: font.descender(),
                    units_per_em: font.units_per_em(),
                })
            })
        });
        let line = line_of(&vertical, default_pixels(scale));
        let per_em = bytes.as_ref().map_or([[0; 4]; 2], |bytes| {
            [&bytes.sans, &bytes.serif].map(|styles| STYLES.map(|style| styles.per_em(style)))
        });
        let mut bounds = [None; 12];
        if let Some(bytes) = &bytes {
            let styles = [&bytes.sans, &bytes.serif]
                .into_iter()
                .chain(bytes.mono.as_ref())
                .flat_map(Styles::each);
            for (bound, style) in bounds.iter_mut().zip(styles) {
                *bound = Font::parse(style)
                    .ok()
                    .map(|font| (font.y_min(), font.y_max(), font.units_per_em()));
            }
        }
        let mono = bytes
            .as_ref()
            .and_then(|bytes| bytes.mono.as_ref())
            .and_then(|mono| Font::parse(&mono.regular).ok())
            .and_then(|font| {
                ['0', 'M'].into_iter().find_map(|scalar| {
                    let units = font.advance(font.glyph(scalar)?)?;
                    Some((units, font.units_per_em()))
                })
            });
        Self {
            scale,
            bytes: line.and(bytes),
            vertical,
            line: line.unwrap_or(cell),
            mono,
            per_em,
            bounds,
            measures: cache::Cache::default(),
        }
    }

    pub fn scale(&self) -> Scale {
        self.scale
    }

    /// Whether it measures proportional faces rather than the cell.
    pub fn proportional(&self) -> bool {
        self.bytes.is_some()
    }

    /// The line box: the cell's, `CELL_HEIGHT` at the scale with the
    /// bitmap face's baseline, without proportional faces.
    pub fn line(&self) -> LineBox {
        self.line
    }

    /// The line box at `size`: from the higher of Sans's and Serif's
    /// regular ascenders to the lower of their descenders, each summed in
    /// `UNIT`s at the size and rounded once; the cell's at every size
    /// without proportional faces.
    pub fn line_at(&self, size: Size) -> LineBox {
        match (&self.bytes, size.fixed()) {
            (Some(_), Some(pixels)) => line_of(&self.vertical, pixels).unwrap_or(self.line),
            _ => self.line,
        }
    }

    /// The top a glyph of `size` is drawn at (a `Glyph`'s `y`) for its
    /// baseline to fall at `baseline`: what aligns glyphs of several sizes
    /// on one baseline.
    pub fn top_at(&self, size: Size, baseline: i64) -> i64 {
        baseline.saturating_sub(i64::from(self.line_at(size).baseline))
    }

    /// How far ink of a glyph of `size` may reach past its line box
    /// (`line_at`): every face's highest and lowest reach as its `head`
    /// states them at the size, a pixel more for coverage's rounding, and
    /// the bitmap face's cell at its scale for the size; none without
    /// proportional faces, whose cell is the line. A consumer culling by
    /// line boxes widens them by it.
    pub fn reach_at(&self, size: Size) -> Reach {
        if self.bytes.is_none() {
            return Reach::default();
        }
        let pixels = size.pixels(self.scale);
        let size_pixels = i64::from(pixels);
        let up = |units: i64, per_em: u16| {
            let per_em = i64::from(per_em).max(1);
            (units.saturating_mul(size_pixels) + per_em - 1).div_euclid(per_em) + 1
        };
        let bitmap = bitmap_scale(pixels) as i64;
        let (top, bottom) = self.bounds.iter().flatten().fold(
            (
                CELL_BASELINE as i64 * bitmap,
                (CELL_HEIGHT - CELL_BASELINE) as i64 * bitmap,
            ),
            |(top, bottom), &(low, high, per_em)| {
                (
                    top.max(up(i64::from(high), per_em)),
                    bottom.max(up(-i64::from(low), per_em)),
                )
            },
        );
        let line = self.line_at(size);
        let (ascent, descent) = (
            i64::from(line.baseline),
            i64::from(line.height) - i64::from(line.baseline),
        );
        let past = |reach: i64| u32::try_from(reach.max(0)).unwrap_or(u32::MAX);
        Reach {
            above: past(top - ascent),
            below: past(bottom - descent),
        }
    }

    /// `scalar`'s advance in `family` and `style`, in `UNIT`s of a pixel:
    /// that of the face that draws it, `CELL_WIDTH` at the scale for the
    /// cell and the bitmap face.
    pub fn advance(&mut self, family: TextFamily, style: Style, scalar: char) -> u32 {
        self.measure(family, style, scalar).3
    }

    /// `advance` at `size`: a family's advance in its units at the size's
    /// pixels per em, unhinted, rounded to a `UNIT`; the monospace face's
    /// one advance at the size; the bitmap face's `CELL_WIDTH` at its
    /// whole scale for the size (`bitmap_scale`); the cell's at the
    /// surface's scale whatever the size.
    #[inline]
    pub fn advance_at(
        &mut self,
        size: Size,
        family: TextFamily,
        style: Style,
        scalar: char,
    ) -> u32 {
        let (source, measured, units, advance) = self.measure(family, style, scalar);
        let Some(pixels) = size.fixed() else {
            return advance;
        };
        if pixels == default_pixels(self.scale) {
            return advance;
        }
        match source {
            Source::Cell => (CELL_WIDTH * self.scale.value()) as u32 * UNIT,
            Source::Family => {
                let family = usize::from(family == TextFamily::Serif);
                let per_em = self
                    .per_em
                    .get(family)
                    .and_then(|styles| styles.get(style_index(measured)))
                    .copied()
                    .unwrap_or(0);
                units_at(units, per_em, i64::from(pixels))
            }
            Source::Mono => self.mono.map_or(0, |(units, units_per_em)| {
                units_at(units, units_per_em, i64::from(pixels))
            }),
            Source::Bitmap => (CELL_WIDTH * bitmap_scale(pixels)) as u32 * UNIT,
        }
    }

    /// Which face draws `scalar` in `family` and `style`, at every size:
    /// the family's style, else its regular; else the monospace face in
    /// its style or regular, or its stand-in; else the bitmap face.
    /// `Mono` starts at the monospace face; a `Cell` glyph, or any without
    /// proportional faces, is the cell's.
    pub fn source(&mut self, family: TextFamily, style: Style, scalar: char) -> Source {
        self.measure(family, style, scalar).0
    }

    /// The width in pixels of `text` set in one run from a pen at zero:
    /// its advances summed in `UNIT`s, then rounded.
    pub fn width(&mut self, family: TextFamily, style: Style, text: &str) -> u32 {
        let total = text.chars().fold(0u64, |total, scalar| {
            total.saturating_add(u64::from(self.advance(family, style, shown(scalar))))
        });
        pixels(total)
    }

    /// `width` at `size`.
    pub fn width_at(&mut self, size: Size, family: TextFamily, style: Style, text: &str) -> u32 {
        let total = text.chars().fold(0u64, |total, scalar| {
            let advance = self.advance_at(size, family, style, shown(scalar));
            total.saturating_add(u64::from(advance))
        });
        pixels(total)
    }

    /// Which face draws a scalar; from a family, the style whose font
    /// measured it and its advance in that font's units, which every size
    /// scales; and its advance at the prose size.
    fn measure(
        &mut self,
        family: TextFamily,
        style: Style,
        scalar: char,
    ) -> (Source, Style, u16, u32) {
        if self.bytes.is_none() || family == TextFamily::Cell {
            let cell = (CELL_WIDTH * self.scale.value()) as u32 * UNIT;
            return (Source::Cell, Style::Regular, 0, cell);
        }
        let key = (family, style, scalar);
        if let Some(measured) = self.measures.get(key) {
            return measured;
        }
        self.measure_anew(key)
    }

    /// `measure` of a key the cache does not hold, from the fonts.
    #[cold]
    fn measure_anew(&mut self, key: cache::Key) -> (Source, Style, u16, u32) {
        let (family, style, scalar) = key;
        let cell = (CELL_WIDTH * self.scale.value()) as u32 * UNIT;
        let Some(bytes) = &self.bytes else {
            return (Source::Cell, Style::Regular, 0, cell);
        };
        let styles = match family {
            TextFamily::Sans => Some(&bytes.sans),
            TextFamily::Serif => Some(&bytes.serif),
            TextFamily::Mono | TextFamily::Cell => None,
        };
        let in_family = styles.and_then(|styles| {
            styles.fonts(style).find_map(|(style, font)| {
                let units = font.advance(font.glyph(scalar)?)?;
                Some((style, units, font.units_per_em()))
            })
        });
        let in_mono = || {
            bytes.mono.as_ref().is_some_and(|mono| {
                [Some(scalar), stand_in(scalar)]
                    .into_iter()
                    .flatten()
                    .any(|wanted| {
                        mono.fonts(style)
                            .any(|(_, font)| font.glyph(wanted).is_some())
                    })
            })
        };
        let size = i64::from(default_pixels(self.scale));
        let measured = match in_family {
            Some((style, units, per_em)) => {
                (Source::Family, style, units, units_at(units, per_em, size))
            }
            None if in_mono() => (
                Source::Mono,
                Style::Regular,
                0,
                self.mono
                    .map_or(0, |(units, per_em)| units_at(units, per_em, size)),
            ),
            None => (Source::Bitmap, Style::Regular, 0, cell),
        };
        self.measures.insert(key, measured);
        measured
    }
}

/// The line box at `pixels` per em over the families' vertical metrics:
/// the higher ascender to the lower descender, each in `UNIT`s, rounded
/// once; none when neither family parsed.
fn line_of(vertical: &[Option<Vertical>; 2], pixels: u16) -> Option<LineBox> {
    let size = i64::from(pixels);
    vertical
        .iter()
        .flatten()
        .map(|metrics| {
            let units_per_em = i64::from(metrics.units_per_em);
            let units = |metric: i16| {
                round_div(i64::from(metric) * size * i64::from(UNIT), units_per_em).max(0)
            };
            (
                units(metrics.ascender),
                units(metrics.descender.saturating_neg()),
            )
        })
        .reduce(|(a, d), (b, e)| (a.max(b), d.max(e)))
        .map(|(ascent, descent)| LineBox {
            height: pixels_of(ascent.saturating_add(descent)).max(1),
            baseline: pixels_of(ascent),
        })
}

/// Non-negative `UNIT`s rounded to the nearest pixel.
fn pixels_of(units: i64) -> u32 {
    pixels(u64::try_from(units).unwrap_or(0))
}

/// The four styles, in the order a style's index names.
const STYLES: [Style; 4] = [
    Style::Regular,
    Style::Bold,
    Style::Italic,
    Style::BoldItalic,
];

/// `style`'s place in `STYLES`.
fn style_index(style: Style) -> usize {
    match style {
        Style::Regular => 0,
        Style::Bold => 1,
        Style::Italic => 2,
        Style::BoldItalic => 3,
    }
}

/// `numerator / denominator` rounded half up, for a positive denominator.
fn round_div(numerator: i64, denominator: i64) -> i64 {
    numerator
        .saturating_mul(2)
        .saturating_add(denominator)
        .div_euclid(denominator.max(1).saturating_mul(2))
}

/// `units` of a face with `upem` units per em at `size` pixels per em, in
/// `UNIT`s of a pixel, rounded.
fn units_at(units: u16, upem: u16, size: i64) -> u32 {
    let scaled = i64::from(units) * size * i64::from(UNIT);
    round_div(scaled, i64::from(upem)).clamp(0, i64::from(u32::MAX)) as u32
}

/// `UNIT`s rounded to the nearest pixel.
fn pixels(units: u64) -> u32 {
    u32::try_from(units.saturating_add(u64::from(UNIT / 2)) / u64::from(UNIT)).unwrap_or(u32::MAX)
}

/// The scalar prose shows for `scalar`: a tab is a space, another control
/// the replacement character, as a text run draws one.
pub fn shown(scalar: char) -> char {
    match scalar {
        '\t' => ' ',
        scalar if scalar.is_control() => '\u{fffd}',
        scalar => scalar,
    }
}

/// A piece of styled text handed to `wrap`: its glyphs' style, family
/// included, and a tag of the caller's that comes back on every piece of
/// it (a link's index, say).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Run<'a> {
    pub text: &'a str,
    pub style: GlyphStyle,
    pub tag: usize,
}

/// Streams `line`'s glyphs with its left at `x` and its top at `y`, each
/// in its piece's style, inside `bounds` and clipped to `damage`; nothing
/// when the two do not meet, and no glyph whose pen is past the clip's
/// right by `OVERHANG` or by the em of the line's largest `Size`,
/// whichever is more, so a glyph whose ink hangs left of its pen still
/// draws its part inside the clip and a repaint of part of a line equals
/// the whole line's inside its damage. Each glyph's line box at its size
/// has its top at `y`: glyphs of several sizes are top-aligned, not on
/// one baseline (`Metrics::top_at` places them on one).
pub fn draw(
    line: &Line,
    (x, y): (i64, i64),
    bounds: Rect,
    damage: Rect,
    sink: &mut dyn FnMut(Draw),
) {
    let Some(clip) = bounds.intersection(damage) else {
        return;
    };
    let reach = line
        .pieces
        .iter()
        .filter_map(|piece| piece.style.size.fixed())
        .map(u32::from)
        .fold(OVERHANG, u32::max);
    let right = clip
        .x
        .saturating_add(i64::from(clip.width))
        .saturating_add(i64::from(reach));
    for piece in &line.pieces {
        for placed in line.glyphs.get(piece.glyphs.clone()).unwrap_or(&[]) {
            let pen = x.saturating_add(i64::from(placed.x));
            if pen >= right {
                return;
            }
            sink(Draw {
                clip,
                primitive: Primitive::Glyph {
                    x: pen,
                    y,
                    scalar: placed.scalar,
                    style: piece.style,
                },
            });
        }
    }
}

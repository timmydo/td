//! An outline face at one pixel size: the style bytes a consumer read, the
//! cell the face's metrics give or the grid it is fitted to, and the atlas
//! its glyphs are covered into on first use. A lookup answers from the atlas; only a miss parses
//! the face, finds the glyph and covers it. Nothing here reads the
//! environment, a clock, a descriptor or the filesystem: the consumer
//! hands over the bytes.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::atlas::{Atlas, Slot, Style};
use crate::coverage::{Mask, Rasterizer, MAX_MASK_AXIS};
use crate::font::stand_in;
use crate::sfnt::{Error, Font, Outline};

pub const MIN_PIXELS_PER_EM: u16 = 6;
pub const MAX_PIXELS_PER_EM: u16 = 256;
/// A cell's width and height, at most; a face whose metrics give more is
/// refused.
pub const MAX_CELL_AXIS: usize = 512;

/// How a face is sized: fitted to a fixed grid cell (`Face::fit`), or at
/// a size in pixels per em, possibly fractional, on the cell its own
/// metrics give (`Face::sized`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Sizing {
    Cell { width: usize, height: usize },
    PixelsPerEm(f32),
}

/// The grid a face lays text on at its pixel size: the width of one
/// advance, the height of one line, the baseline's distance from the
/// cell's top, and the pen's from its left.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Cell {
    pub width: usize,
    pub height: usize,
    pub baseline: usize,
    pub pen: usize,
}

/// What covering a glyph uses between glyphs: the outline, the area
/// accumulator and the mask, kept so a steady state allocates nothing.
/// A face holds its own; faces covering in turn may share one
/// (`Face::glyph_with`).
#[derive(Clone, Debug, Default)]
pub struct Scratch {
    rasterizer: Rasterizer,
    outline: Outline,
    mask: Mask,
}

#[derive(Clone, Debug)]
pub struct Face {
    /// Unique to each face made, and each `with_slant`, within the process;
    /// a clone keeps it, since it draws the same glyphs.
    id: u64,
    regular: Arc<[u8]>,
    /// The bold, italic and bold italic styles, each with its pixels per
    /// font unit, since units per em may differ between styles.
    styles: [Option<(Arc<[u8]>, f32)>; 3],
    /// Pixels per font unit for the regular style.
    scale: f32,
    /// The size glyphs are covered at, unrounded.
    size: f32,
    pixels_per_em: u16,
    cell: Cell,
    atlas: Atlas,
    scratch: Scratch,
}

/// A face identity, unique within the process: at one a nanosecond, the
/// count would take centuries to wrap.
fn next_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

impl Face {
    /// Parses each style and derives the cell from the regular one: the
    /// rounded advance of `0` (else `M`) wide, the rounded ascender to the
    /// baseline, and the rounded descender and line gap below it.
    pub fn new(regular: Vec<u8>, bold: Option<Vec<u8>>, pixels_per_em: u16) -> Result<Self, Error> {
        if !(MIN_PIXELS_PER_EM..=MAX_PIXELS_PER_EM).contains(&pixels_per_em) {
            return Err(Error::Limit("pixels per em"));
        }
        Self::sized(
            regular.into(),
            bold.map(Arc::from),
            f32::from(pixels_per_em),
        )
    }

    /// `new` over shared style bytes at `size` pixels per em, which may be
    /// fractional: the cell is the size's rounded metrics, as `new`'s.
    pub fn sized(regular: Arc<[u8]>, bold: Option<Arc<[u8]>>, size: f32) -> Result<Self, Error> {
        Self::sized_on(Atlas::new(), regular, bold, size)
    }

    /// `sized` covering into `atlas`, emptied first, rather than a new
    /// one: a caller remaking faces in turn keeps their pages' storage.
    pub fn sized_in(
        mut atlas: Atlas,
        regular: Arc<[u8]>,
        bold: Option<Arc<[u8]>>,
        size: f32,
    ) -> Result<Self, Error> {
        atlas.reset();
        Self::sized_on(atlas, regular, bold, size)
    }

    fn sized_on(
        atlas: Atlas,
        regular: Arc<[u8]>,
        bold: Option<Arc<[u8]>>,
        size: f32,
    ) -> Result<Self, Error> {
        let range = f32::from(MIN_PIXELS_PER_EM)..=f32::from(MAX_PIXELS_PER_EM);
        if !range.contains(&size) {
            return Err(Error::Limit("pixels per em"));
        }
        let font = Font::parse(&regular)?;
        let scale = size / f32::from(font.units_per_em());
        let pixels = |units: f32| (units * scale).round().max(0.0) as usize;
        let width = pixels(f32::from(advance(&font)?));
        let baseline = pixels(f32::from(font.ascender()));
        let height =
            baseline + pixels(-f32::from(font.descender())) + pixels(f32::from(font.line_gap()));
        if width == 0 || height == 0 {
            return Err(Error::Malformed("cell metrics"));
        }
        let cell = Cell {
            width,
            height,
            baseline,
            pen: 0,
        };
        Self::build(regular, bold, size, cell, atlas)
    }

    /// `fit` or `sized`, as `sizing` asks.
    pub fn with_sizing(
        regular: Arc<[u8]>,
        bold: Option<Arc<[u8]>>,
        sizing: Sizing,
    ) -> Result<Self, Error> {
        match sizing {
            Sizing::Cell { width, height } => Self::fit(regular, bold, width, height),
            Sizing::PixelsPerEm(size) => Self::sized(regular, bold, size),
        }
    }

    /// This face's styles again at another sizing, with an empty atlas.
    pub fn resized(&self, sizing: Sizing) -> Result<Self, Error> {
        let [bold, italic, bold_italic] = &self.styles;
        let bytes =
            |style: &Option<(Arc<[u8]>, f32)>| style.as_ref().map(|(bytes, _)| bytes.clone());
        Self::with_sizing(self.regular.clone(), bytes(bold), sizing)?
            .with_slant(bytes(italic), bytes(bold_italic))
    }

    /// A face fitted to a fixed `width` by `height` grid cell, so text laid
    /// out on that grid needs no other change: the largest size, fractional,
    /// whose advance fits the width and whose em fits the height, so a
    /// width-bound advance fills the cell and full-cell glyphs meet their
    /// neighbours. The advance is centred across the cell and the line box
    /// (ascender to descender) down it, the baseline below the cell if that
    /// is where centring puts it; what the line box holds past the cell's
    /// height is clipped with it. A bold style is covered at the same
    /// size, scaled by its own units per em.
    pub fn fit(
        regular: Arc<[u8]>,
        bold: Option<Arc<[u8]>>,
        width: usize,
        height: usize,
    ) -> Result<Self, Error> {
        if width == 0 || height == 0 {
            return Err(Error::Malformed("cell metrics"));
        }
        let font = Font::parse(&regular)?;
        let units_per_em = f64::from(font.units_per_em());
        let advance = f64::from(advance(&font)?);
        if advance == 0.0 {
            return Err(Error::Malformed("cell metrics"));
        }
        let pixels_per_em = (width as f64 * units_per_em / advance)
            .min(height as f64)
            .min(f64::from(MAX_PIXELS_PER_EM));
        let scale = pixels_per_em / units_per_em;
        let pen = ((width as f64 - advance * scale) / 2.0).round().max(0.0);
        let middle = (f64::from(font.ascender()) + f64::from(font.descender())) / 2.0;
        // Below the cell when a tall line box leans up: centring decides
        // what is clipped, and the raster clips to the cell.
        let baseline = (height as f64 / 2.0 + middle * scale).round().max(0.0);
        let cell = Cell {
            width,
            height,
            baseline: baseline as usize,
            pen: pen as usize,
        };
        Self::build(regular, bold, pixels_per_em as f32, cell, Atlas::new())
    }

    fn build(
        regular: Arc<[u8]>,
        bold: Option<Arc<[u8]>>,
        pixels_per_em: f32,
        cell: Cell,
        atlas: Atlas,
    ) -> Result<Self, Error> {
        let range = f32::from(MIN_PIXELS_PER_EM)..=f32::from(MAX_PIXELS_PER_EM);
        if !range.contains(&pixels_per_em) {
            return Err(Error::Limit("pixels per em"));
        }
        if cell.width > MAX_CELL_AXIS || cell.height > MAX_CELL_AXIS {
            return Err(Error::Limit("cell"));
        }
        let scale = scale_of(&regular, pixels_per_em)?;
        let bold = bold
            .map(|bold| scale_of(&bold, pixels_per_em).map(|scale| (bold, scale)))
            .transpose()?;
        Ok(Self {
            id: next_id(),
            regular,
            styles: [bold, None, None],
            scale,
            size: pixels_per_em,
            pixels_per_em: pixels_per_em.round() as u16,
            cell,
            atlas,
            scratch: Scratch::default(),
        })
    }

    /// The face's atlas, its page's storage for another face
    /// (`sized_in`).
    pub fn into_atlas(self) -> Atlas {
        self.atlas
    }

    /// This face with italic and bold italic styles, covered at its size
    /// on its cell, and its atlas emptied. Either may be absent; a slanted
    /// style the face lacks draws upright.
    pub fn with_slant(
        mut self,
        italic: Option<Arc<[u8]>>,
        bold_italic: Option<Arc<[u8]>>,
    ) -> Result<Self, Error> {
        let size = self.size;
        let scaled = |bytes: Option<Arc<[u8]>>| {
            bytes
                .map(|bytes| scale_of(&bytes, size).map(|scale| (bytes, scale)))
                .transpose()
        };
        let [_, italic_slot, bold_italic_slot] = &mut self.styles;
        *italic_slot = scaled(italic)?;
        *bold_italic_slot = scaled(bold_italic)?;
        self.id = next_id();
        // A slot covered, or found missing, before the change would
        // otherwise answer for the new styles.
        self.atlas.reset();
        Ok(self)
    }

    pub fn cell(&self) -> Cell {
        self.cell
    }

    /// The size glyphs are covered at, rounded: a fitted face's is
    /// fractional.
    pub fn pixels_per_em(&self) -> u16 {
        self.pixels_per_em
    }

    /// An identity no other face made in this process shares, but a clone,
    /// which draws the same glyphs: a cache keyed on it holds one face's
    /// pixels.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// The size glyphs are covered at, unrounded.
    pub fn size(&self) -> f32 {
        self.size
    }

    pub fn atlas(&self) -> &Atlas {
        &self.atlas
    }

    /// The atlas's dirty band, taken: the rows a GPU backend uploads.
    pub fn take_dirty(&mut self) -> Option<(usize, usize)> {
        self.atlas.take_dirty()
    }

    /// The style a glyph is drawn in: the one asked for when the face has
    /// it, else bold italic falls to italic and then bold, and any style to
    /// regular.
    pub fn style(&self, bold: bool, italic: bool) -> Style {
        let has = |style: Style| self.source(style).is_some();
        [
            (bold && italic, Style::BoldItalic),
            (italic, Style::Italic),
            (bold, Style::Bold),
        ]
        .into_iter()
        .find(|&(asked, style)| asked && has(style))
        .map_or(Style::Regular, |(_, style)| style)
    }

    /// A style's bytes and scale other than the regular one's.
    fn source(&self, style: Style) -> Option<&(Arc<[u8]>, f32)> {
        let [bold, italic, bold_italic] = &self.styles;
        match style {
            Style::Regular => None,
            Style::Bold => bold.as_ref(),
            Style::Italic => italic.as_ref(),
            Style::BoldItalic => bold_italic.as_ref(),
        }
    }

    /// The scalar's slot in `style`, as `style` resolves it from what the
    /// face has, covering it on a miss. A scalar a style lacks is covered
    /// from the regular one, and one the face lacks from its
    /// `font::stand_in`; one neither covers, or whose glyph it refuses,
    /// is recorded missing so the caller's fallback answers without another
    /// parse. A placed entry is valid until the atlas's next reset, which
    /// any later miss may cause: a caller holding entries across lookups
    /// compares `atlas().epoch()`.
    pub fn glyph(&mut self, style: Style, scalar: char) -> Slot {
        let mut scratch = std::mem::take(&mut self.scratch);
        let slot = self.glyph_with(&mut scratch, style, scalar);
        self.scratch = scratch;
        slot
    }

    /// `glyph`, covering in `scratch` rather than the face's own.
    pub fn glyph_with(&mut self, scratch: &mut Scratch, style: Style, scalar: char) -> Slot {
        let bold = matches!(style, Style::Bold | Style::BoldItalic);
        let italic = matches!(style, Style::Italic | Style::BoldItalic);
        let style = self.style(bold, italic);
        if let Some(slot) = self.atlas.get(style, scalar) {
            return slot;
        }
        let styled = self
            .source(style)
            .map(|(bytes, scale)| (bytes.clone(), *scale));
        let regular = (self.regular.clone(), self.scale);
        let fonts = [styled.as_ref(), Some(&regular)].map(|source| {
            source.and_then(|(bytes, scale)| Font::parse(bytes).ok().map(|font| (font, *scale)))
        });
        let Scratch {
            rasterizer,
            outline,
            mask,
        } = scratch;
        // The scalar from its style or regular before its stand-in.
        let covered = [Some(scalar), stand_in(scalar)]
            .into_iter()
            .flatten()
            .any(|wanted| {
                fonts.iter().flatten().any(|(font, scale)| {
                    font.glyph(wanted)
                        .and_then(|glyph| {
                            font.outline(glyph, outline).ok()?;
                            rasterizer
                                .rasterize(outline, fitted(outline, *scale), mask)
                                .ok()
                        })
                        .is_some()
                })
            });
        let covered = covered.then_some(());
        match covered {
            Some(()) => self.atlas.place(style, scalar, &scratch.mask),
            None => {
                self.atlas.record(style, scalar, Slot::Missing);
                Slot::Missing
            }
        }
    }
}

/// `scale`, or for an outline whose mask at it would pass
/// `MAX_MASK_AXIS` on an axis, the largest scale at which it fits: such a
/// glyph is covered smaller, scaled about its pen, rather than refused,
/// so its ink stays inside what its outline spans at full scale: inside
/// its advance where the outline is, and inside `prose`'s reach.
fn fitted(outline: &Outline, scale: f32) -> f32 {
    let mut bounds: Option<(f32, f32, f32, f32)> = None;
    for point in outline.points() {
        let (x, y) = (point.x, point.y);
        bounds = Some(match bounds {
            None => (x, y, x, y),
            Some((left, bottom, right, top)) => {
                (left.min(x), bottom.min(y), right.max(x), top.max(y))
            }
        });
    }
    let Some((left, bottom, right, top)) = bounds else {
        return scale;
    };
    let axis = MAX_MASK_AXIS as f32;
    let pixels = |low: f32, high: f32| (high * scale).ceil() - (low * scale).floor();
    if pixels(left, right) <= axis && pixels(bottom, top) <= axis {
        return scale;
    }
    // Two pixels short of the axis, for the floor and the ceiling.
    let extent = (right - left).max(top - bottom);
    if extent > 0.0 && extent.is_finite() {
        scale.min((axis - 2.0) / extent)
    } else {
        scale
    }
}

/// Pixels per font unit at `pixels_per_em` for the face in `bytes`.
fn scale_of(bytes: &[u8], pixels_per_em: f32) -> Result<f32, Error> {
    Font::parse(bytes).map(|font| pixels_per_em / f32::from(font.units_per_em()))
}

/// The advance a cell is one of: that of `0`, else `M`.
fn advance(font: &Font) -> Result<u16, Error> {
    ['0', 'M']
        .into_iter()
        .find_map(|scalar| font.glyph(scalar).and_then(|glyph| font.advance(glyph)))
        .ok_or(Error::Missing("cell advance"))
}

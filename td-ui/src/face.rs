//! An outline face at one pixel size: the style bytes a consumer read, the
//! cell the face's metrics give or the grid it is fitted to, and the atlas
//! its glyphs are covered into on first use. A lookup answers from the atlas; only a miss parses
//! the face, finds the glyph and covers it. Nothing here reads the
//! environment, a clock, a descriptor or the filesystem: the consumer
//! hands over the bytes.

use std::sync::Arc;

use crate::atlas::{Atlas, Slot, Style};
use crate::coverage::{Mask, Rasterizer};
use crate::sfnt::{Error, Font, Outline};

pub const MIN_PIXELS_PER_EM: u16 = 6;
pub const MAX_PIXELS_PER_EM: u16 = 256;
/// A cell's width and height, at most; a face whose metrics give more is
/// refused.
pub const MAX_CELL_AXIS: usize = 512;

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

#[derive(Clone, Debug)]
pub struct Face {
    regular: Arc<[u8]>,
    bold: Option<Arc<[u8]>>,
    pixels_per_em: u16,
    /// Pixels per font unit, for the regular style and the bold one,
    /// whose units per em may differ.
    scales: [f32; 2],
    cell: Cell,
    atlas: Atlas,
    rasterizer: Rasterizer,
    outline: Outline,
    mask: Mask,
}

impl Face {
    /// Parses each style and derives the cell from the regular one: the
    /// rounded advance of `0` (else `M`) wide, the rounded ascender to the
    /// baseline, and the rounded descender and line gap below it.
    pub fn new(regular: Vec<u8>, bold: Option<Vec<u8>>, pixels_per_em: u16) -> Result<Self, Error> {
        if !(MIN_PIXELS_PER_EM..=MAX_PIXELS_PER_EM).contains(&pixels_per_em) {
            return Err(Error::Limit("pixels per em"));
        }
        let font = Font::parse(&regular)?;
        let scale = f32::from(pixels_per_em) / f32::from(font.units_per_em());
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
        Self::build(
            regular.into(),
            bold.map(Arc::from),
            f32::from(pixels_per_em),
            cell,
        )
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
        Self::build(regular, bold, pixels_per_em as f32, cell)
    }

    fn build(
        regular: Arc<[u8]>,
        bold: Option<Arc<[u8]>>,
        pixels_per_em: f32,
        cell: Cell,
    ) -> Result<Self, Error> {
        let range = f32::from(MIN_PIXELS_PER_EM)..=f32::from(MAX_PIXELS_PER_EM);
        if !range.contains(&pixels_per_em) {
            return Err(Error::Limit("pixels per em"));
        }
        if cell.width > MAX_CELL_AXIS || cell.height > MAX_CELL_AXIS {
            return Err(Error::Limit("cell"));
        }
        let scale_of = |bytes: &[u8]| {
            Font::parse(bytes).map(|font| pixels_per_em / f32::from(font.units_per_em()))
        };
        let scale = scale_of(&regular)?;
        let bold_scale = match &bold {
            Some(bold) => scale_of(bold)?,
            None => scale,
        };
        Ok(Self {
            regular,
            bold,
            pixels_per_em: pixels_per_em.round() as u16,
            scales: [scale, bold_scale],
            cell,
            atlas: Atlas::new(),
            rasterizer: Rasterizer::new(),
            outline: Outline::new(),
            mask: Mask::default(),
        })
    }

    pub fn cell(&self) -> Cell {
        self.cell
    }

    /// The size glyphs are covered at, rounded: a fitted face's is
    /// fractional.
    pub fn pixels_per_em(&self) -> u16 {
        self.pixels_per_em
    }

    pub fn atlas(&self) -> &Atlas {
        &self.atlas
    }

    /// The atlas's dirty band, taken: the rows a GPU backend uploads.
    pub fn take_dirty(&mut self) -> Option<(usize, usize)> {
        self.atlas.take_dirty()
    }

    /// The style a glyph is drawn in: bold only when the face has it.
    pub fn style(&self, bold: bool) -> Style {
        if bold && self.bold.is_some() {
            Style::Bold
        } else {
            Style::Regular
        }
    }

    /// The scalar's slot in `style` (bold only when the face has it),
    /// covering it on a miss. A scalar the bold style lacks is covered from
    /// the regular one; one the face lacks, or whose glyph it refuses, is
    /// recorded missing so the caller's fallback answers without another
    /// parse. A placed entry is valid until the atlas's next reset, which
    /// any later miss may cause: a caller holding entries across lookups
    /// compares `atlas().epoch()`.
    pub fn glyph(&mut self, style: Style, scalar: char) -> Slot {
        let style = self.style(style == Style::Bold);
        if let Some(slot) = self.atlas.get(style, scalar) {
            return slot;
        }
        let [scale, bold_scale] = self.scales;
        let bold = match (style, &self.bold) {
            (Style::Bold, Some(bold)) => Some((bold, bold_scale)),
            _ => None,
        };
        let sources = bold.into_iter().chain([(&self.regular, scale)]);
        let (outline, rasterizer, mask) = (&mut self.outline, &mut self.rasterizer, &mut self.mask);
        let covered = sources.into_iter().any(|(bytes, scale)| {
            Font::parse(bytes)
                .ok()
                .and_then(|font| {
                    let glyph = font.glyph(scalar)?;
                    font.outline(glyph, outline).ok()?;
                    rasterizer.rasterize(outline, scale, mask).ok()
                })
                .is_some()
        });
        let covered = covered.then_some(());
        match covered {
            Some(()) => self.atlas.place(style, scalar, &self.mask),
            None => {
                self.atlas.record(style, scalar, Slot::Missing);
                Slot::Missing
            }
        }
    }
}

/// The advance a cell is one of: that of `0`, else `M`.
fn advance(font: &Font) -> Result<u16, Error> {
    ['0', 'M']
        .into_iter()
        .find_map(|scalar| font.glyph(scalar).and_then(|glyph| font.advance(glyph)))
        .ok_or(Error::Missing("cell advance"))
}

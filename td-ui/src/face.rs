//! An outline face at one pixel size: the style bytes a consumer read, the
//! cell the face's metrics give, and the atlas its glyphs are covered
//! into on first use. A lookup answers from the atlas; only a miss parses
//! the face, finds the glyph and covers it. Nothing here reads the
//! environment, a clock, a descriptor or the filesystem: the consumer
//! hands over the bytes.

use crate::atlas::{Atlas, Slot, Style};
use crate::coverage::{Mask, Rasterizer};
use crate::sfnt::{Error, Font, Outline};

pub const MIN_PIXELS_PER_EM: u16 = 6;
pub const MAX_PIXELS_PER_EM: u16 = 256;
/// A cell's width and height, at most; a face whose metrics give more is
/// refused.
pub const MAX_CELL_AXIS: usize = 512;

/// The grid a face lays text on at its pixel size: the width of one
/// advance, the height of one line, and the baseline's distance from the
/// cell's top.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Cell {
    pub width: usize,
    pub height: usize,
    pub baseline: usize,
}

#[derive(Clone, Debug)]
pub struct Face {
    regular: Vec<u8>,
    bold: Option<Vec<u8>>,
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
        let bold_scale = match &bold {
            Some(bold) => f32::from(pixels_per_em) / f32::from(Font::parse(bold)?.units_per_em()),
            None => scale,
        };
        let advance = ['0', 'M']
            .into_iter()
            .find_map(|scalar| font.glyph(scalar).and_then(|glyph| font.advance(glyph)))
            .ok_or(Error::Missing("cell advance"))?;
        let pixels = |units: f32| (units * scale).round().max(0.0) as usize;
        let width = pixels(f32::from(advance));
        let baseline = pixels(f32::from(font.ascender()));
        let height =
            baseline + pixels(-f32::from(font.descender())) + pixels(f32::from(font.line_gap()));
        if width == 0 || height == 0 {
            return Err(Error::Malformed("cell metrics"));
        }
        if width > MAX_CELL_AXIS || height > MAX_CELL_AXIS {
            return Err(Error::Limit("cell"));
        }
        Ok(Self {
            regular,
            bold,
            pixels_per_em,
            scales: [scale, bold_scale],
            cell: Cell {
                width,
                height,
                baseline,
            },
            atlas: Atlas::new(),
            rasterizer: Rasterizer::new(),
            outline: Outline::new(),
            mask: Mask::default(),
        })
    }

    pub fn cell(&self) -> Cell {
        self.cell
    }

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

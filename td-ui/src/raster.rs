//! Clipped, allocation-free XRGB painting over the pinned 8x16 face and
//! the 4x5 hint face: the rectangle, glyph and mark primitives, the
//! integer scale, the palette td-owned chrome shares, scrollbar geometry,
//! the text-run and hint-run painters and the raster that writes them
//! into a caller-owned buffer. A program's scene composes these and
//! streams draws through [`Composition`]; nothing here reads the
//! environment, a clock or a descriptor. A raster given an outline face
//! (`with_face`) executes glyphs through its atlas instead, below the same
//! draw stream.

use crate::atlas::{Entry, Slot, Style, PAGE_WIDTH};
use crate::face::Face;
use crate::font::Font;
use crate::hint;
use crate::theme::{Theme, SAND};
use crate::typeface::Typeface;
use crate::{CELL_HEIGHT, CELL_WIDTH};

pub const MAX_AXIS: usize = 8192;
pub const MAX_FRAME_BYTES: usize = 32 * 1024 * 1024;

pub const PAPER: u32 = 0xeee8dc;
pub const INK: u32 = 0x48453f;
pub const CHROME: u32 = 0xe1dbcf;
pub const BORDER: u32 = 0xb5ada0;
pub const SELECTED: u32 = 0x536b73;
pub const INACTIVE_SELECTION: u32 = 0xc8c4bb;
pub const LINE_NUMBER: u32 = 0x817a6f;
pub const MISSPELLED: u32 = 0x9c5548;
/// Status inks a line is drawn in for what it means, legible on paper and
/// as a band under paper ink.
pub const SUCCESS: u32 = 0x4d6b3c;
pub const WARNING: u32 = 0x86601a;
pub const ACCENT: u32 = 0x7a4d74;

/// What the raster refuses: an argument outside its contract, or a size
/// past a ceiling. Validation precedes every write.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidArgument,
    Limit,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidArgument => "invalid-argument",
            Self::Limit => "limit",
        })
    }
}

impl std::error::Error for Error {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Weight {
    Regular,
    Medium,
}

/// Transparent glyph paint; the caller supplies the already-painted background.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GlyphStyle {
    pub ink: u32,
    pub background: u32,
    pub weight: Weight,
}

impl GlyphStyle {
    pub fn medium(ink: u32, background: u32) -> Self {
        Self {
            ink,
            background,
            weight: Weight::Medium,
        }
    }

    fn fringe(self) -> u32 {
        // One-third ink, two-thirds background, independently per RGB channel.
        // Explicit colors make partial/repeated repaints independent of old pixels.
        let mut color = 0;
        for shift in [0, 8, 16] {
            let ink = (self.ink >> shift) & 255;
            let background = (self.background >> shift) & 255;
            color |= ((ink + 2 * background) / 3) << shift;
        }
        color
    }
}

/// An integer scale, 1 through 4, applied to every cell.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Scale(u8);

impl Default for Scale {
    fn default() -> Self {
        Self(1)
    }
}

impl Scale {
    pub fn new(value: u8) -> Result<Self, Error> {
        if !(1..=4).contains(&value) {
            return Err(Error::InvalidArgument);
        }
        Ok(Self(value))
    }
    pub fn value(self) -> usize {
        usize::from(self.0)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rect {
    pub x: i64,
    pub y: i64,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub fn contains(self, x: i64, y: i64) -> bool {
        x >= self.x
            && y >= self.y
            && x < self.x.saturating_add(i64::from(self.width))
            && y < self.y.saturating_add(i64::from(self.height))
    }

    pub fn intersection(self, other: Self) -> Option<Self> {
        let left = self.x.max(other.x);
        let top = self.y.max(other.y);
        let right = self
            .x
            .saturating_add(i64::from(self.width))
            .min(other.x.saturating_add(i64::from(other.width)));
        let bottom = self
            .y
            .saturating_add(i64::from(self.height))
            .min(other.y.saturating_add(i64::from(other.height)));
        if right <= left || bottom <= top {
            return None;
        }
        Some(Self {
            x: left,
            y: top,
            width: u32::try_from(right.checked_sub(left)?).ok()?,
            height: u32::try_from(bottom.checked_sub(top)?).ok()?,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Primitive {
    Fill {
        rect: Rect,
        color: u32,
    },
    Glyph {
        x: i64,
        y: i64,
        scalar: char,
        style: GlyphStyle,
    },
    /// A scalar in the hint face (`hint`): `hint::WIDTH` by
    /// `hint::HEIGHT` at the scale, ink only, over whatever the caller
    /// painted.
    Mark {
        x: i64,
        y: i64,
        scalar: char,
        ink: u32,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Draw {
    pub clip: Rect,
    pub primitive: Primitive,
}

/// The surface a composition is laid out for and a raster paints: pixel
/// axes and one integer scale. `new` holds the axes to the ceilings below;
/// `Raster::new` holds them again, so a literal is admitted on the same
/// terms as a constructed value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Surface {
    pub width: usize,
    pub height: usize,
    pub scale: Scale,
}

impl Surface {
    pub fn new(width: usize, height: usize, scale: Scale) -> Result<Self, Error> {
        let surface = Self {
            width,
            height,
            scale,
        };
        surface.check()?;
        Ok(surface)
    }
    /// Nonzero axes through `MAX_AXIS`, and at most `MAX_FRAME_BYTES` of
    /// tight four-byte pixels.
    pub fn check(self) -> Result<(), Error> {
        if self.width == 0 || self.height == 0 || self.width > MAX_AXIS || self.height > MAX_AXIS {
            return Err(Error::InvalidArgument);
        }
        if self
            .width
            .checked_mul(self.height)
            .and_then(|n| n.checked_mul(4))
            .is_none_or(|n| n > MAX_FRAME_BYTES)
        {
            return Err(Error::Limit);
        }
        Ok(())
    }
    pub fn bounds(self) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: u32::try_from(self.width).unwrap_or(u32::MAX),
            height: u32::try_from(self.height).unwrap_or(u32::MAX),
        }
    }
}

/// What a raster paints: a composition that knows the surface it was laid
/// out for and streams the draws inside a damage rectangle, in order, with
/// no retained operation list.
pub trait Composition {
    fn surface(&self) -> Surface;
    fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw));
}

/// A scrollbar's track and thumb for a viewport of `visible` units over
/// `total`, with the first visible unit at `first`, along one axis.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Scrollbar {
    pub track: Rect,
    pub thumb: Rect,
    maximum: usize,
    horizontal: bool,
}

impl Scrollbar {
    /// A viewport of `visible` units over `total` along a track, either
    /// count possibly zero: nothing to scroll is a disabled thumb, never a
    /// division by zero.
    pub fn new(
        track: Rect,
        visible: usize,
        total: usize,
        first: usize,
        scale: Scale,
        horizontal: bool,
    ) -> Self {
        let scale = scale.value();
        let maximum = total.saturating_sub(visible);
        let length = if horizontal {
            track.width
        } else {
            track.height
        };
        let size =
            (u128::from(length) * visible as u128 / total.max(visible).max(1) as u128) as u32;
        // A track shorter than a cell (no consumer lays one out) keeps the
        // travel rule: `scale` pixels stay free when scrolling is possible
        // and the thumb shrinks to fit; on a track no longer than that, the
        // thumb is nothing and the whole track is the travel. Nothing
        // underflows.
        let ceiling = length.saturating_sub(if maximum == 0 { 0 } else { scale as u32 });
        let size = size.max((24 * scale) as u32).min(ceiling);
        let travel = length - size;
        let offset =
            (u128::from(travel) * first.min(maximum) as u128 / maximum.max(1) as u128) as i64;
        let thumb = if horizontal {
            Rect {
                x: track.x.saturating_add(offset),
                width: size,
                ..track
            }
        } else {
            Rect {
                y: track.y.saturating_add(offset),
                height: size,
                ..track
            }
        };
        Self {
            track,
            thumb,
            maximum,
            horizontal,
        }
    }
    pub fn horizontal(self) -> bool {
        self.horizontal
    }
    pub fn coordinate(self, x: i64, y: i64) -> i64 {
        if self.horizontal {
            x
        } else {
            y
        }
    }
    pub fn enabled(self) -> bool {
        self.maximum != 0
    }

    pub fn position_at(self, coordinate: i64, grab: i64, origin: usize) -> usize {
        // The fields are public, so a thumb wider than its track is a
        // caller's edit, not an underflow.
        let travel = if self.horizontal {
            self.track.width.saturating_sub(self.thumb.width)
        } else {
            self.track.height.saturating_sub(self.thumb.height)
        };
        let start = self.coordinate(self.track.x, self.track.y);
        let leading_edge = coordinate.saturating_sub(grab);
        let delta = leading_edge.saturating_sub(self.coordinate(self.thumb.x, self.thumb.y));
        // Anchor to the exact original position: a click/release must not
        // jump by the units lost when the thumb was rounded to pixels.
        if delta == 0 {
            return origin.min(self.maximum);
        }
        if leading_edge <= start {
            return 0;
        }
        if leading_edge >= start.saturating_add(i64::from(travel)) {
            return self.maximum;
        }
        let distance = i128::from(delta) * self.maximum as i128;
        let units = (distance.abs() + i128::from(travel / 2)) / i128::from(travel.max(1));
        (origin as i128 + units * distance.signum()).clamp(0, self.maximum as i128) as usize
    }
}

/// A run of scalars painted left to right from `origin` inside `bounds`,
/// one cell each at the surface's scale, clipped to `damage`; a control
/// scalar draws as the replacement character.
pub fn text_run(
    scale: Scale,
    chars: impl Iterator<Item = char>,
    (x, y): (i64, i64),
    bounds: Rect,
    style: GlyphStyle,
    damage: Rect,
    sink: &mut dyn FnMut(Draw),
) {
    let Some(clip) = bounds.intersection(damage) else {
        return;
    };
    let cw = (CELL_WIDTH * scale.value()) as i64;
    let slots = (bounds
        .x
        .saturating_add(i64::from(bounds.width))
        .saturating_sub(x)
        / cw)
        .max(0) as usize;
    for (index, scalar) in chars.take(slots).enumerate() {
        let scalar = if scalar.is_control() {
            '\u{fffd}'
        } else {
            scalar
        };
        sink(Draw {
            clip,
            primitive: Primitive::Glyph {
                x: x.saturating_add(index as i64 * cw),
                y,
                scalar,
                style,
            },
        });
    }
}

/// A run of scalars in the hint face painted left to right from
/// `origin` inside `bounds`, `hint::ADVANCE` apart at the surface's
/// scale, as many as fit whole (the last needs no space after it),
/// clipped to `damage`; a scalar the face lacks draws as its box.
pub fn hint_run(
    scale: Scale,
    chars: impl Iterator<Item = char>,
    (x, y): (i64, i64),
    bounds: Rect,
    ink: u32,
    damage: Rect,
    sink: &mut dyn FnMut(Draw),
) {
    let Some(clip) = bounds.intersection(damage) else {
        return;
    };
    let advance = (hint::ADVANCE * scale.value()) as i64;
    let space = ((hint::ADVANCE - hint::WIDTH) * scale.value()) as i64;
    let slots = (bounds
        .x
        .saturating_add(i64::from(bounds.width))
        .saturating_sub(x)
        .saturating_add(space)
        / advance)
        .max(0) as usize;
    for (index, scalar) in chars.take(slots).enumerate() {
        sink(Draw {
            clip,
            primitive: Primitive::Mark {
                x: x.saturating_add(index as i64 * advance),
                y,
                scalar,
                ink,
            },
        });
    }
}

/// The tight RGB rows of a painted frame: `pixels` as a `Raster` over
/// `surface` at `stride` left them, three bytes per pixel, row-major, for
/// a PPM body or a page of one. Validation mirrors `Raster::new`.
pub fn rgb(pixels: &[u8], surface: Surface, stride: usize) -> Result<Vec<u8>, Error> {
    surface.check()?;
    let row_bytes = surface.width * 4;
    if stride < row_bytes || !stride.is_multiple_of(4) {
        return Err(Error::InvalidArgument);
    }
    let needed = stride.checked_mul(surface.height).ok_or(Error::Limit)?;
    if needed > MAX_FRAME_BYTES {
        return Err(Error::Limit);
    }
    if pixels.len() < needed {
        return Err(Error::InvalidArgument);
    }
    let mut out = Vec::with_capacity(surface.width * surface.height * 3);
    for row in pixels.chunks(stride).take(surface.height) {
        // The raster is XRGB little-endian ([B, G, R, X]); PPM wants RGB.
        for [blue, green, red, _] in row.get(..row_bytes).unwrap_or(&[]).as_chunks::<4>().0 {
            out.extend_from_slice(&[*red, *green, *blue]);
        }
    }
    Ok(out)
}

/// A binary PPM (`P6`, 255 maximum) over rows `rgb` produced.
pub fn ppm(surface: Surface, rgb: &[u8]) -> Vec<u8> {
    let header = format!("P6\n{} {}\n255\n", surface.width, surface.height);
    let mut out = Vec::with_capacity(header.len() + rgb.len());
    out.extend_from_slice(header.as_bytes());
    out.extend_from_slice(rgb);
    out
}

/// What a primitive paints inside its rectangle: every pixel, the face's
/// pixels of a glyph (and its fringe when medium), or the hint face's
/// rows of a mark.
enum Shape {
    Solid,
    Glyph {
        index: usize,
        fringe: Option<[u8; 4]>,
    },
    Mark([u8; hint::HEIGHT]),
}

pub struct Raster<'pixels, 'font> {
    pixels: &'pixels mut [u8],
    font: &'font Font,
    face: Option<&'font mut Face>,
    theme: &'static Theme,
    surface: Surface,
    stride: usize,
}

impl<'pixels, 'font> Raster<'pixels, 'font> {
    /// Validation precedes all writes. Padding and excess storage stay intact.
    pub fn new(
        pixels: &'pixels mut [u8],
        font: &'font Font,
        surface: Surface,
        stride: usize,
    ) -> Result<Self, Error> {
        surface.check()?;
        if (font.width(), font.height()) != (CELL_WIDTH, CELL_HEIGHT)
            || stride < surface.width * 4
            || !stride.is_multiple_of(4)
        {
            return Err(Error::InvalidArgument);
        }
        let needed = stride.checked_mul(surface.height).ok_or(Error::Limit)?;
        if needed > MAX_FRAME_BYTES {
            return Err(Error::Limit);
        }
        if pixels.len() < needed {
            return Err(Error::InvalidArgument);
        }
        Ok(Self {
            pixels,
            font,
            face: None,
            theme: &SAND,
            surface,
            stride,
        })
    }

    /// Executes every `Glyph` through `face` from here on: in the face's
    /// cell at the draw's origin, from its pen, covered into its atlas on
    /// first use and blended over the style's explicit background, every
    /// weight in the regular style (`Weight::Medium` is the bitmap face's
    /// body text, not bold). A scalar
    /// the face lacks draws from the bitmap face, centred in the cell. The
    /// face is sized for the surface's pixels, scale included, by its
    /// owner, and the composition must lay its glyphs out on the face's
    /// cell: one laid out on the bitmap grid overlaps or gaps wherever the
    /// two cells differ, which a face fitted to that grid (`Face::fit`)
    /// never does.
    pub fn with_face(mut self, face: &'font mut Face) -> Self {
        self.face = Some(face);
        self
    }

    /// `with_face` through `typeface`'s face fitted to the grid at this
    /// raster's scale; the bitmap face still draws when there is no
    /// typeface or the fit is refused.
    pub fn with_typeface(self, typeface: Option<&'font mut Typeface>) -> Self {
        let scale = self.surface.scale;
        match typeface.and_then(|typeface| typeface.face(scale)) {
            Some(face) => self.with_face(face),
            None => self,
        }
    }

    /// Draws every colour of the shared palette in `theme`'s from here on
    /// (`Theme::map`); other colours are drawn as they are given.
    pub fn with_theme(mut self, theme: &'static Theme) -> Self {
        self.theme = theme;
        self
    }

    /// Paints a composition laid out for this exact surface; a mismatch is
    /// refused before anything is written.
    pub fn paint(
        &mut self,
        scene: &(impl Composition + ?Sized),
        damage: Rect,
    ) -> Result<(), Error> {
        if self.surface != scene.surface() {
            return Err(Error::InvalidArgument);
        }
        scene.emit(damage, &mut |draw| self.draw(draw));
        Ok(())
    }

    pub fn draw(&mut self, draw: Draw) {
        let Some(clip) = draw.clip.intersection(self.surface.bounds()) else {
            return;
        };
        let draw = Draw {
            primitive: self.theme.primitive(draw.primitive),
            ..draw
        };
        let Primitive::Glyph {
            x,
            y,
            scalar,
            style,
        } = draw.primitive
        else {
            return self.shape(clip, draw.primitive);
        };
        let Some(face) = self.face.as_deref_mut() else {
            return self.shape(clip, draw.primitive);
        };
        let cell = face.cell();
        let bounds = Rect {
            x,
            y,
            width: u32::try_from(cell.width).unwrap_or(u32::MAX),
            height: u32::try_from(cell.height).unwrap_or(u32::MAX),
        };
        let Some(clip) = clip.intersection(bounds) else {
            return;
        };
        // The stream's weights are the bitmap face's: Medium is its body
        // text, the fringe thickening a thin face, so every weight draws the
        // outline's regular style.
        match face.glyph(Style::Regular, scalar) {
            Slot::Placed(entry) => {
                let origin = (
                    x.saturating_add(cell.pen as i64),
                    y.saturating_add(cell.baseline as i64),
                );
                blend(
                    self.pixels,
                    self.stride,
                    face.atlas().page(),
                    entry,
                    origin,
                    clip,
                    style,
                );
            }
            Slot::Blank => {}
            Slot::Missing => {
                let scale = self.surface.scale.value();
                let inset = |cell: usize, glyph: usize| (cell as i64 - (glyph * scale) as i64) / 2;
                let primitive = Primitive::Glyph {
                    x: x.saturating_add(inset(cell.width, CELL_WIDTH)),
                    y: y.saturating_add(inset(cell.height, CELL_HEIGHT)),
                    scalar,
                    style,
                };
                self.shape(clip, primitive);
            }
        }
    }

    fn shape(&mut self, clip: Rect, primitive: Primitive) {
        let scale = self.surface.scale.value();
        let (rect, color, shape) = match primitive {
            Primitive::Fill { rect, color } => (rect, color, Shape::Solid),
            Primitive::Glyph {
                x,
                y,
                scalar,
                style,
            } => (
                Rect {
                    x,
                    y,
                    width: (CELL_WIDTH * scale) as u32,
                    height: (CELL_HEIGHT * scale) as u32,
                },
                style.ink,
                Shape::Glyph {
                    index: self.font.index(scalar),
                    fringe: (style.weight == Weight::Medium)
                        .then(|| (style.fringe() | 0xff000000).to_le_bytes()),
                },
            ),
            Primitive::Mark { x, y, scalar, ink } => (
                Rect {
                    x,
                    y,
                    width: (hint::WIDTH * scale) as u32,
                    height: (hint::HEIGHT * scale) as u32,
                },
                ink,
                Shape::Mark(hint::glyph(scalar)),
            ),
        };
        let Some(area) = rect.intersection(clip) else {
            return;
        };
        let bytes = (color | 0xff000000).to_le_bytes();
        for y in area.y..area.y + i64::from(area.height) {
            // Intersection with an at-most-32x64 shape makes these
            // differences small even for hostile signed origins; a fill
            // needs neither.
            let row = y.saturating_sub(rect.y) as usize / scale;
            let start = y as usize * self.stride;
            for x in area.x..area.x + i64::from(area.width) {
                let mut paint = &bytes;
                match &shape {
                    Shape::Solid => {}
                    Shape::Glyph { index, fringe } => {
                        let col = x.saturating_sub(rect.x) as usize / scale;
                        if !self.font.pixel(*index, col, row) {
                            let Some(fringe) = fringe else {
                                continue;
                            };
                            if !col
                                .checked_sub(1)
                                .is_some_and(|left| self.font.pixel(*index, left, row))
                            {
                                continue;
                            }
                            paint = fringe;
                        }
                    }
                    Shape::Mark(rows) => {
                        let col = x.saturating_sub(rect.x) as usize / scale;
                        let lit = rows.get(row).is_some_and(|bits| {
                            (hint::WIDTH - 1)
                                .checked_sub(col)
                                .is_some_and(|bit| bits >> bit & 1 != 0)
                        });
                        if !lit {
                            continue;
                        }
                    }
                }
                let at = start + x as usize * 4;
                if let Some(pixel) = self.pixels.get_mut(at..at + 4) {
                    pixel.copy_from_slice(paint);
                }
            }
        }
    }
}

/// Covers `entry` from the atlas page over the pen at `origin`, inside
/// `clip`: each pixel's colour is the style's background moved toward its
/// ink by the coverage, never a read of the buffer.
fn blend(
    pixels: &mut [u8],
    stride: usize,
    page: &[u8],
    entry: Entry,
    (x, y): (i64, i64),
    clip: Rect,
    style: GlyphStyle,
) {
    let rect = Rect {
        x: x.saturating_add(i64::from(entry.left)),
        y: y.saturating_sub(i64::from(entry.top)),
        width: u32::try_from(entry.width).unwrap_or(0),
        height: u32::try_from(entry.height).unwrap_or(0),
    };
    let Some(area) = rect.intersection(clip) else {
        return;
    };
    // The clip lies inside the surface, so the area's coordinates are
    // non-negative and its offsets into the glyph are within the entry.
    for py in area.y..area.y + i64::from(area.height) {
        let row = (entry.y + (py - rect.y) as usize) * PAGE_WIDTH + entry.x;
        let start = py as usize * stride;
        for px in area.x..area.x + i64::from(area.width) {
            let alpha = page.get(row + (px - rect.x) as usize).copied().unwrap_or(0);
            if alpha == 0 {
                continue;
            }
            let color = mix(style.background, style.ink, alpha) | 0xff000000;
            let at = start + px as usize * 4;
            if let Some(pixel) = pixels.get_mut(at..at + 4) {
                pixel.copy_from_slice(&color.to_le_bytes());
            }
        }
    }
}

/// `from` moved toward `to` by `alpha` of 255, per channel, rounded.
fn mix(from: u32, to: u32, alpha: u8) -> u32 {
    let alpha = u32::from(alpha);
    let mut color = 0;
    for shift in [0, 8, 16] {
        let from = (from >> shift) & 255;
        let to = (to >> shift) & 255;
        color |= ((from * (255 - alpha) + to * alpha + 127) / 255) << shift;
    }
    color
}

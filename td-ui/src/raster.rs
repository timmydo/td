//! Clipped, allocation-free XRGB painting over the pinned 8x16 face: the
//! rectangle and glyph primitives, the integer scale, the palette td-owned
//! chrome shares, scrollbar geometry, the text-run painter and the raster
//! that writes them into a caller-owned buffer. A program's scene composes
//! these and streams draws through [`Composition`]; nothing here reads the
//! environment, a clock or a descriptor.

use crate::font::Font;
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

pub struct Raster<'pixels, 'font> {
    pixels: &'pixels mut [u8],
    font: &'font Font,
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
            surface,
            stride,
        })
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
        let scale = self.surface.scale.value();
        let (rect, color, glyph) = match draw.primitive {
            Primitive::Fill { rect, color } => (rect, color, None),
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
                Some((
                    self.font.index(scalar),
                    (style.weight == Weight::Medium)
                        .then(|| (style.fringe() | 0xff000000).to_le_bytes()),
                )),
            ),
        };
        let Some(area) = rect.intersection(clip) else {
            return;
        };
        let bytes = (color | 0xff000000).to_le_bytes();
        for y in area.y..area.y + i64::from(area.height) {
            let row = if glyph.is_some() {
                y.saturating_sub(rect.y) as usize / scale
            } else {
                0
            };
            let start = y as usize * self.stride;
            for x in area.x..area.x + i64::from(area.width) {
                let mut paint = &bytes;
                if let Some((index, fringe)) = &glyph {
                    // Intersection with an at-most-32x64 glyph makes these
                    // differences small even for hostile signed origins.
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
                let at = start + x as usize * 4;
                if let Some(pixel) = self.pixels.get_mut(at..at + 4) {
                    pixel.copy_from_slice(paint);
                }
            }
        }
    }
}

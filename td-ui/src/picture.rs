//! Pictures: owned RGBA images a composition shows (a pasted screenshot,
//! an image in a reply), each fitted once to the size it is drawn at.
//! Decoding is the owner's (td-png, td-jpeg); the toolkit takes pixels.
//! A fit is an area average over premultiplied samples, never an
//! enlargement, into the raster's own byte order, so drawing it is a
//! blend and nothing more.

use std::collections::TryReserveError;

/// The longest axis a picture may have.
pub const MAX_PICTURE_AXIS: usize = 8192;
/// The most pixels one picture holds: 16 Mi, 64 MiB of RGBA.
pub const MAX_PICTURE_PIXELS: usize = 16 << 20;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// An axis of zero, one past `MAX_PICTURE_AXIS`, or a picture past
    /// `MAX_PICTURE_PIXELS`.
    Size,
    /// Pixels that are not four bytes for each of `width` by `height`.
    Length,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Size => f.write_str("picture size out of range"),
            Self::Length => f.write_str("picture pixels are not its size"),
        }
    }
}

impl std::error::Error for Error {}

/// Eight-bit RGBA, rows top to bottom, alpha straight; owns the decoded
/// buffer without allocating or copying it.
pub struct Picture {
    width: usize,
    height: usize,
    rgba: Vec<u8>,
}

impl std::fmt::Debug for Picture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Picture({}x{})", self.width, self.height)
    }
}

impl Picture {
    /// A picture of `width` by `height` taking ownership of its RGBA bytes
    /// as given, without another allocation.
    pub fn new(width: usize, height: usize, rgba: Vec<u8>) -> Result<Self, Error> {
        if width == 0 || height == 0 || width > MAX_PICTURE_AXIS || height > MAX_PICTURE_AXIS {
            return Err(Error::Size);
        }
        let pixels = width.checked_mul(height).ok_or(Error::Size)?;
        if pixels > MAX_PICTURE_PIXELS {
            return Err(Error::Size);
        }
        if Some(rgba.len()) != pixels.checked_mul(4) {
            return Err(Error::Length);
        }
        Ok(Self {
            width,
            height,
            rgba,
        })
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    /// The bytes it holds.
    pub fn bytes(&self) -> usize {
        self.rgba.len()
    }

    /// The size it is drawn at within `most_width` by `most_height`: its
    /// own, or shrunk to fit with its aspect kept, an axis at least one
    /// pixel; never enlarged.
    pub fn fit_size(&self, most_width: usize, most_height: usize) -> (usize, usize) {
        let (w, h) = (self.width, self.height);
        let (mw, mh) = (most_width.max(1), most_height.max(1));
        if w <= mw && h <= mh {
            return (w, h);
        }
        // Width-bound when w / h >= mw / mh.
        if (w as u128) * (mh as u128) >= (h as u128) * (mw as u128) {
            (mw, (h * mw / w).max(1))
        } else {
            ((w * mh / h).max(1), mh)
        }
    }

    /// The picture area-averaged to `width` by `height` (at most its own
    /// size; a larger asks for its own), premultiplied, in the raster's
    /// byte order; none when its memory cannot be had.
    pub fn fit(&self, width: usize, height: usize) -> Option<Fitted> {
        self.fit_with(width, height, Vec::try_reserve_exact)
    }

    fn fit_with(
        &self,
        width: usize,
        height: usize,
        reserve: impl FnOnce(&mut Vec<u8>, usize) -> Result<(), TryReserveError>,
    ) -> Option<Fitted> {
        let width = width.clamp(1, self.width);
        let height = height.clamp(1, self.height);
        let mut bgra = Vec::new();
        reserve(&mut bgra, width * height * 4).ok()?;
        bgra.resize(width * height * 4, 0);
        let total = (self.width * self.height) as u64;
        for y in 0..height {
            for x in 0..width {
                let (mut blue, mut green, mut red, mut alpha) = (0u64, 0u64, 0u64, 0u64);
                for (source_y, row_weight) in span(y, height, self.height) {
                    let row = source_y * self.width * 4;
                    for (source_x, column_weight) in span(x, width, self.width) {
                        let at = row + source_x * 4;
                        let &[r, g, b, a] = self.rgba.get(at..at + 4)?.first_chunk::<4>()?;
                        let a64 = u64::from(a) * row_weight * column_weight;
                        blue += u64::from(b) * a64;
                        green += u64::from(g) * a64;
                        red += u64::from(r) * a64;
                        alpha += a64;
                    }
                }
                let colour = |s: u64| ((s + total * 255 / 2) / (total * 255)) as u8;
                let at = (y * width + x) * 4;
                bgra.get_mut(at..at + 4)?.copy_from_slice(&[
                    colour(blue),
                    colour(green),
                    colour(red),
                    ((alpha + total / 2) / total) as u8,
                ]);
            }
        }
        Some(Fitted {
            width,
            height,
            bgra,
        })
    }
}

/// Exact overlap weights for one output pixel. Computing them on demand
/// avoids a heap allocation for every output row and column.
fn span(i: usize, out: usize, from: usize) -> impl Iterator<Item = (usize, u64)> {
    let (lo, hi) = (i * from, (i + 1) * from);
    (lo / out..hi.div_ceil(out).min(from)).filter_map(move |s| {
        let overlap = hi.min((s + 1) * out).saturating_sub(lo.max(s * out));
        (overlap > 0).then_some((s, overlap as u64))
    })
}

/// A picture fitted to the size it is drawn at: premultiplied, in the
/// raster's byte order (blue, green, red, alpha).
#[derive(Debug, Eq, PartialEq)]
pub struct Fitted {
    pub width: usize,
    pub height: usize,
    pub bgra: Vec<u8>,
}

impl Fitted {
    /// The premultiplied pixel at `x`, `y`.
    pub fn pixel(&self, x: usize, y: usize) -> Option<[u8; 4]> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let at = (y * self.width + x) * 4;
        self.bgra.get(at..at + 4)?.first_chunk::<4>().copied()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn failed_fit_allocation_keeps_the_picture_usable() {
        let image = Picture::new(1, 1, vec![10, 20, 30, 255]).unwrap();
        let refused = image.fit_with(1, 1, |out, _| out.try_reserve_exact(usize::MAX));
        assert!(refused.is_none());
        assert_eq!(
            image.fit(1, 1).unwrap().pixel(0, 0),
            Some([30, 20, 10, 255])
        );
    }
}

//! Pictures: owned RGBA images a composition shows (a pasted screenshot,
//! an image in a reply), each fitted once to the size it is drawn at.
//! Decoding is the owner's (td-png, td-jpeg); the toolkit takes pixels.
//! A fit is an area average over premultiplied samples, never an
//! enlargement but by `Fitted::fit_exact`, into the raster's own byte
//! order, so drawing it is a blend and nothing more.

use td_alloc::Error as AllocError;

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

    /// The picture at its own size, premultiplied, in the raster's byte
    /// order, in its own buffer: what `fit` makes of it at its own size,
    /// with no allocation.
    pub fn into_fitted(self) -> Fitted {
        let Self {
            width,
            height,
            mut rgba,
        } = self;
        for pixel in rgba.as_chunks_mut::<4>().0 {
            let [r, g, b, a] = *pixel;
            let alpha = u32::from(a);
            let colour = |c: u8| ((u32::from(c) * alpha + 127) / 255) as u8;
            *pixel = [colour(b), colour(g), colour(r), a];
        }
        Fitted {
            width,
            height,
            bgra: rgba,
        }
    }

    /// The picture area-averaged to `width` by `height` (at most its own
    /// size; a larger asks for its own), premultiplied, in the raster's
    /// byte order; none when its memory cannot be had.
    pub fn fit(&self, width: usize, height: usize) -> Option<Fitted> {
        self.fit_with(width, height, td_alloc::reserve_exact)
    }

    fn fit_with(
        &self,
        width: usize,
        height: usize,
        reserve: impl FnOnce(&mut Vec<u8>, usize) -> Result<(), AllocError>,
    ) -> Option<Fitted> {
        let width = width.clamp(1, self.width);
        let height = height.clamp(1, self.height);
        let mut bgra = zeroed(width * height * 4, reserve)?;
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

/// `len` zero bytes, reserved by `reserve` first; none when refused.
fn zeroed(
    len: usize,
    reserve: impl FnOnce(&mut Vec<u8>, usize) -> Result<(), AllocError>,
) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    reserve(&mut bytes, len).ok()?;
    bytes.resize(len, 0);
    Some(bytes)
}

/// What a `Fitted::fit_exact` costs (`Fitted::fit_exact_cost`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FitCost {
    /// Overlap steps, a source pixel's four channels each.
    pub steps: u64,
    /// The bytes of the pass between the two, held while it runs.
    pub scratch: usize,
    across_first: bool,
}

/// `bgra`, `width` by `rows`, averaged across to `out` wide.
fn across(
    bgra: &[u8],
    width: usize,
    rows: usize,
    out: usize,
    stop: &dyn Fn() -> bool,
) -> Option<Vec<u8>> {
    let mut to = zeroed(
        out.checked_mul(rows)?.checked_mul(4)?,
        td_alloc::reserve_exact,
    )?;
    let total = width as u64;
    for y in 0..rows {
        if stop() {
            return None;
        }
        let row = bgra.get(y * width * 4..(y + 1) * width * 4)?;
        let into = to.get_mut(y * out * 4..(y + 1) * out * 4)?;
        for (x, pixel) in into.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let mut sums = [0u64; 4];
            for (from, weight) in span(x, out, width) {
                let source = row.get(from * 4..from * 4 + 4)?;
                for (sum, &c) in sums.iter_mut().zip(source) {
                    *sum += u64::from(c) * weight;
                }
            }
            for (c, sum) in pixel.iter_mut().zip(sums) {
                *c = ((sum + total / 2) / total) as u8;
            }
        }
    }
    Some(to)
}

/// `bgra`, `width` by `rows`, averaged down to `out` rows.
fn down(
    bgra: &[u8],
    width: usize,
    rows: usize,
    out: usize,
    stop: &dyn Fn() -> bool,
) -> Option<Vec<u8>> {
    let mut to = zeroed(
        width.checked_mul(out)?.checked_mul(4)?,
        td_alloc::reserve_exact,
    )?;
    let total = rows as u64;
    let line = width * 4;
    let mut sums = [0u64; 4];
    for y in 0..out {
        if stop() {
            return None;
        }
        let into = to.get_mut(y * line..(y + 1) * line)?;
        for (x, pixel) in into.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            sums.fill(0);
            for (from, weight) in span(y, out, rows) {
                let at = from * line + x * 4;
                let source = bgra.get(at..at + 4)?;
                for (sum, &c) in sums.iter_mut().zip(source) {
                    *sum += u64::from(c) * weight;
                }
            }
            for (c, sum) in pixel.iter_mut().zip(sums) {
                *c = ((sum + total / 2) / total) as u8;
            }
        }
    }
    Some(to)
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
    /// It area-averaged to exactly `width` by `height`, enlarged on an
    /// axis where that is larger than its own: each output pixel the
    /// premultiplied pixels under it, each weighted by its exact overlap
    /// with it, an axis at a time, so one that lies within a single pixel
    /// is that pixel. None past `MAX_PICTURE_AXIS` or `MAX_PICTURE_PIXELS`,
    /// for a fit with no pixels, or when its memory cannot be had.
    pub fn fit_exact(&self, width: usize, height: usize) -> Option<Fitted> {
        self.fit_exact_until(width, height, &|| false)
    }

    /// What `fit_exact` to `width` by `height` costs: the overlap steps
    /// of its two passes, each axis in turn in the cheaper order, and the
    /// bytes of the one pass between them. None where it makes no fit.
    pub fn fit_exact_cost(&self, width: usize, height: usize) -> Option<FitCost> {
        let (sw, sh) = (self.width, self.height);
        if sw == 0 || sh == 0 || self.bgra.len() != sw.checked_mul(sh)?.checked_mul(4)? {
            return None;
        }
        if width == 0
            || height == 0
            || width > MAX_PICTURE_AXIS
            || height > MAX_PICTURE_AXIS
            || width.checked_mul(height)? > MAX_PICTURE_PIXELS
        {
            return None;
        }
        let steps = |rows: usize, from: usize, out: usize| {
            (rows as u64).saturating_mul((from as u64).saturating_add(out as u64))
        };
        // Across first: a pass of `width` by `sh`, then down.
        let across = steps(sh, sw, width).saturating_add(steps(width, sh, height));
        // Down first: a pass of `sw` by `height`, then across.
        let down = steps(sw, sh, height).saturating_add(steps(height, sw, width));
        let across_first = across <= down;
        let between = if across_first {
            width.checked_mul(sh)?
        } else {
            sw.checked_mul(height)?
        };
        Some(FitCost {
            steps: across.min(down),
            scratch: between.checked_mul(4)?,
            across_first,
        })
    }

    /// `fit_exact`, given up between rows once `stop` says so.
    pub fn fit_exact_until(
        &self,
        width: usize,
        height: usize,
        stop: &dyn Fn() -> bool,
    ) -> Option<Fitted> {
        let cost = self.fit_exact_cost(width, height)?;
        let (sw, sh) = (self.width, self.height);
        let out = if cost.across_first {
            let between = across(&self.bgra, sw, sh, width, stop)?;
            down(&between, width, sh, height, stop)?
        } else {
            let between = down(&self.bgra, sw, sh, height, stop)?;
            across(&between, sw, height, width, stop)?
        };
        Some(Fitted {
            width,
            height,
            bgra: out,
        })
    }

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

    /// A picture at its own size is its fit at that size, in place; an
    /// exact fit of that enlarges by overlap, a pixel within one pixel
    /// that pixel, and shrinks as `fit` does, within a level.
    #[test]
    fn a_picture_into_its_own_fit_and_an_exact_fit_of_that() {
        let rgba = vec![255, 0, 0, 255, 0, 0, 255, 255, 10, 200, 30, 100, 7, 7, 7, 0];
        let image = Picture::new(2, 2, rgba.clone()).unwrap();
        let fit = image.fit(2, 2).unwrap();
        let own = Picture::new(2, 2, rgba).unwrap().into_fitted();
        assert_eq!(own, fit);
        let double = own.fit_exact(4, 4).unwrap();
        for y in 0..4 {
            for x in 0..4 {
                assert_eq!(double.pixel(x, y), own.pixel(x / 2, y / 2));
            }
        }
        // Three from two: the middle column half of each.
        let wide = own.fit_exact(3, 2).unwrap();
        assert_eq!(wide.pixel(1, 0), Some([128, 0, 128, 255]));
        let image = Picture::new(2, 2, vec![255; 16]).unwrap();
        let one = image.fit(1, 1).unwrap().pixel(0, 0).unwrap();
        let exact = image
            .into_fitted()
            .fit_exact(1, 1)
            .unwrap()
            .pixel(0, 0)
            .unwrap();
        assert_eq!(one, exact);
        assert!(own.fit_exact(MAX_PICTURE_AXIS + 1, 1).is_none());
        assert!(own.fit_exact(4096, 4097).is_none());
    }

    #[test]
    fn failed_fit_allocation_keeps_the_picture_usable() {
        let image = Picture::new(1, 1, vec![10, 20, 30, 255]).unwrap();
        let refused = image.fit_with(1, 1, |out, _| td_alloc::reserve_exact(out, usize::MAX));
        assert!(refused.is_none());
        assert_eq!(
            image.fit(1, 1).unwrap().pixel(0, 0),
            Some([30, 20, 10, 255])
        );
    }
}

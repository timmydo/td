//! Pictures: owned RGBA images a composition shows (a pasted screenshot,
//! an image in a reply), each fitted once to the size it is drawn at.
//! Decoding is the owner's (td-png, td-jpeg); the toolkit takes pixels.
//! A fit is an area average over premultiplied samples, never an
//! enlargement, into the raster's own byte order, so drawing it is a
//! blend and nothing more.

use std::sync::Arc;

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

/// Eight-bit RGBA, rows top to bottom, alpha straight; shared, so a
/// message and its owner hold one copy.
#[derive(Clone)]
pub struct Picture {
    width: usize,
    height: usize,
    rgba: Arc<[u8]>,
}

impl std::fmt::Debug for Picture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Picture({}x{})", self.width, self.height)
    }
}

impl Picture {
    /// A picture of `width` by `height` from its RGBA bytes, kept shared
    /// as given.
    pub fn new(width: usize, height: usize, rgba: impl Into<Arc<[u8]>>) -> Result<Self, Error> {
        let rgba: Arc<[u8]> = rgba.into();
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
        let width = width.clamp(1, self.width);
        let height = height.clamp(1, self.height);
        let mut bgra = Vec::new();
        bgra.try_reserve_exact(width * height * 4).ok()?;
        bgra.resize(width * height * 4, 0);
        let columns = spans(width, self.width);
        let rows = spans(height, self.height);
        // Every output pixel's weights sum to the source's width by its
        // height; colour sums carry a further 255 from the alpha.
        let total = (self.width * self.height) as u64;
        let mut sums = vec![[0u64; 4]; width];
        for (y, row_span) in rows.iter().enumerate() {
            sums.iter_mut().for_each(|sum| *sum = [0; 4]);
            for &(source_y, row_weight) in row_span {
                let row = source_y * self.width * 4;
                for (sum, column_span) in sums.iter_mut().zip(&columns) {
                    for &(source_x, column_weight) in column_span {
                        let at = row + source_x * 4;
                        let Some(&[r, g, b, a]) =
                            self.rgba.get(at..at + 4).and_then(|p| p.first_chunk::<4>())
                        else {
                            continue;
                        };
                        let weight = row_weight * column_weight;
                        let a64 = u64::from(a) * weight;
                        sum[0] += u64::from(b) * a64;
                        sum[1] += u64::from(g) * a64;
                        sum[2] += u64::from(r) * a64;
                        sum[3] += a64;
                    }
                }
            }
            for (x, sum) in sums.iter().enumerate() {
                let at = (y * width + x) * 4;
                if let Some(out) = bgra.get_mut(at..at + 4) {
                    let colour = |s: u64| ((s + total * 255 / 2) / (total * 255)) as u8;
                    out.copy_from_slice(&[
                        colour(sum[0]),
                        colour(sum[1]),
                        colour(sum[2]),
                        ((sum[3] + total / 2) / total) as u8,
                    ]);
                }
            }
        }
        Some(Fitted {
            width,
            height,
            bgra,
        })
    }
}

/// Each of `out` output pixels' source pixels along an axis of `from`,
/// with the length of each one's overlap: output pixel `i` covers
/// `[i * from, (i + 1) * from)` and source pixel `s` covers
/// `[s * out, (s + 1) * out)`, so the weights are exact and each output
/// pixel's sum to `from`.
fn spans(out: usize, from: usize) -> Vec<Vec<(usize, u64)>> {
    (0..out)
        .map(|i| {
            let (lo, hi) = (i * from, (i + 1) * from);
            (lo / out..hi.div_ceil(out).min(from))
                .filter_map(|s| {
                    let overlap = hi.min((s + 1) * out).saturating_sub(lo.max(s * out));
                    (overlap > 0).then_some((s, overlap as u64))
                })
                .collect()
        })
        .collect()
}

/// A picture fitted to the size it is drawn at: premultiplied, in the
/// raster's byte order (blue, green, red, alpha).
#[derive(Clone, Debug, Eq, PartialEq)]
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

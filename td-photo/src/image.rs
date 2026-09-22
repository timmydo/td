//! Owned RGB image buffers and the binary PPM writer. The writer takes any
//! `Write`; it opens nothing itself. The fields are public so a pipeline
//! can fill a buffer in place, which is why every accessor checks the axes
//! against the buffer rather than trusting them.

use std::io::{self, Write};

/// The longest axis any RGB buffer here may have; the raw reader's ceiling
/// is the same number, and a test holds them together.
pub const MAX_AXIS: usize = 16384;
/// The most pixels one image buffer holds: 64 Mi, so the largest `f32`
/// RGB buffer the pipeline makes (12 bytes a pixel) is 768 MiB, under the
/// allocation limit of a 32-bit target.
pub const MAX_IMAGE_PIXELS: usize = 64 << 20;

/// Eight-bit interleaved RGB, rows top to bottom, no padding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rgb8 {
    pub width: usize,
    pub height: usize,
    pub data: Vec<u8>,
}

impl Rgb8 {
    /// A black image, or `None` for a zero axis, one past `MAX_AXIS`, or
    /// more than `MAX_IMAGE_PIXELS`.
    pub fn new(width: usize, height: usize) -> Option<Self> {
        if width == 0
            || height == 0
            || width > MAX_AXIS
            || height > MAX_AXIS
            || width * height > MAX_IMAGE_PIXELS
        {
            return None;
        }
        Some(Self {
            width,
            height,
            data: vec![0; width * height * 3],
        })
    }

    /// Whether the axes are nonzero, within `MAX_AXIS`, the pixel count
    /// within `MAX_IMAGE_PIXELS`, and the buffer exactly
    /// `width * height * 3` bytes.
    pub fn is_consistent(&self) -> bool {
        self.width != 0
            && self.height != 0
            && self.width <= MAX_AXIS
            && self.height <= MAX_AXIS
            && self.width * self.height <= MAX_IMAGE_PIXELS
            && self.width * self.height * 3 == self.data.len()
    }

    /// The pixel at (x, y), or `None` outside the image or its buffer.
    pub fn pixel(&self, x: usize, y: usize) -> Option<[u8; 3]> {
        if x >= self.width || y >= self.height {
            return None;
        }
        // Checked: the fields are public, so the axes need not agree with
        // the buffer.
        let at = y.checked_mul(self.width)?.checked_add(x)?.checked_mul(3)?;
        let bytes = self.data.get(at..at.checked_add(3)?)?;
        Some([*bytes.first()?, *bytes.get(1)?, *bytes.get(2)?])
    }
}

/// Reads a binary `P6` PPM with a 255 maximum in the exact shape
/// `write_ppm` produces (single-space header, one newline after each
/// field, no comments), refusing anything else or any size past the
/// ceilings: the cache reads back only what this crate wrote.
pub fn read_ppm(bytes: &[u8]) -> Option<Rgb8> {
    let rest = bytes.strip_prefix(b"P6\n")?;
    let newline = rest.iter().position(|b| *b == b'\n')?;
    let (header, rest) = rest.split_at(newline);
    let rest = rest.strip_prefix(b"\n")?.strip_prefix(b"255\n")?;
    let header = std::str::from_utf8(header).ok()?;
    let (w, h) = header.split_once(' ')?;
    let width: usize = w.parse().ok()?;
    let height: usize = h.parse().ok()?;
    // The payload must be exactly the header's size before anything is
    // allocated for it: a header alone buys no buffer.
    let expected = width.checked_mul(height)?.checked_mul(3)?;
    if rest.len() != expected {
        return None;
    }
    let mut image = Rgb8::new(width, height)?;
    image.data.copy_from_slice(rest);
    Some(image)
}

/// Writes a binary `P6` PPM with a 255 maximum.
pub fn write_ppm(image: &Rgb8, out: &mut dyn Write) -> io::Result<()> {
    if !image.is_consistent() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "image buffer does not match its axes",
        ));
    }
    write!(out, "P6\n{} {}\n255\n", image.width, image.height)?;
    out.write_all(&image.data)?;
    out.flush()
}

/// The size a `width` by `height` image shrinks to for a long edge of
/// `long_edge`: the longer axis to `long_edge`, the other to the nearest
/// pixel of the same ratio (one at least), or the size as it is when the
/// long edge covers it, since an export is never enlarged.
pub fn shrunk(width: usize, height: usize, long_edge: usize) -> (usize, usize) {
    let long = width.max(height);
    if long_edge >= long || long == 0 || long_edge == 0 {
        return (width, height);
    }
    let short = width.min(height);
    let other = (short.saturating_mul(long_edge).saturating_add(long / 2) / long).max(1);
    if width >= height {
        (long_edge, other)
    } else {
        (other, long_edge)
    }
}

/// The source indices output index `at` of a `source` to `out` shrink
/// covers, from the first, and each one's share of the output pixel in
/// units of `1 / out`: the output pixel spans `source` such units and a
/// source pixel `out`, so the shares sum to `source`. Empty past the
/// output or for a zero axis.
fn span(source: usize, out: usize, at: usize) -> (usize, Vec<u64>) {
    if out == 0 || at >= out {
        return (0, Vec::new());
    }
    let (source, out, at) = (source as u64, out as u64, at as u64);
    let (from, to) = (at * source, (at + 1) * source);
    let first = from / out;
    let last = to.div_ceil(out);
    let shares = (first..last)
        .map(|index| {
            let (lo, hi) = (index * out, (index + 1) * out);
            to.min(hi).saturating_sub(from.max(lo))
        })
        .collect();
    (first as usize, shares)
}

/// The area shrink of a `width` by `height` image to `out_width` by
/// `out_height`, fed source rows a band at a time from the top and
/// giving output rows back as they complete, so no more than a band of
/// source and the rows in progress are held whatever the ratio: each
/// output pixel is the mean of the source box it covers, a source pixel
/// on the box's edge weighted by the part of it inside, in whole
/// arithmetic rounded half up, so the rows are the same whatever the
/// bands. Only a shrink: an output axis past its source is refused.
pub struct Shrink {
    width: usize,
    height: usize,
    out_width: usize,
    out_height: usize,
    /// Each output column's first source column and shares (`span`).
    columns: Vec<(usize, Vec<u64>)>,
    /// The next source row expected.
    source_row: usize,
    /// The next output row to give back, and the weighted sums of the
    /// rows from it that a fed source row has touched, in order.
    out_row: usize,
    pending: Vec<Vec<[u64; 3]>>,
}

impl Shrink {
    /// The shrink, or `None` for a zero axis, an output axis past its
    /// source, or a size no buffer holds.
    pub fn new(width: usize, height: usize, out_width: usize, out_height: usize) -> Option<Self> {
        if out_width == 0 || out_height == 0 || out_width > width || out_height > height {
            return None;
        }
        Rgb8::new(out_width, 1)?;
        (width as u64).checked_mul(height as u64)?;
        Some(Shrink {
            width,
            height,
            out_width,
            out_height,
            columns: (0..out_width).map(|x| span(width, out_width, x)).collect(),
            source_row: 0,
            out_row: 0,
            pending: Vec::new(),
        })
    }

    /// The output rows the source rows fed so far complete, in order:
    /// all of them once the last source row is in.
    pub fn done(&self) -> usize {
        self.out_row
    }

    /// Feeds the next source rows, `band` being the rows from the next
    /// expected, and gives back the output rows they complete, `None` for
    /// none yet. `Some(Err)` when the band is inconsistent, not `width`
    /// wide, or runs past the source's foot.
    pub fn push(&mut self, band: &Rgb8) -> Option<std::result::Result<Rgb8, ()>> {
        if !band.is_consistent()
            || band.width != self.width
            || self.source_row.saturating_add(band.height) > self.height
        {
            return Some(Err(()));
        }
        let stride = self.width * 3;
        let total = (self.width as u64) * (self.height as u64);
        let mut out: Vec<u8> = Vec::new();
        for (offset, row) in band.data.chunks_exact(stride).enumerate() {
            let source_row = self.source_row + offset;
            // The output rows this source row lies in: the same shares as
            // an output pixel's over the source, the axes swapped.
            let (first, shares) = span(self.out_height, self.height, source_row);
            for (index, row_share) in shares.iter().enumerate() {
                let Some(slot) = (first + index).checked_sub(self.out_row) else {
                    continue;
                };
                while self.pending.len() <= slot {
                    self.pending.push(vec![[0u64; 3]; self.out_width]);
                }
                let Some(sums) = self.pending.get_mut(slot) else {
                    return Some(Err(()));
                };
                for (sum, (column_first, column_shares)) in sums.iter_mut().zip(&self.columns) {
                    for (index, column_share) in column_shares.iter().enumerate() {
                        let at = (column_first + index) * 3;
                        let Some(pixel) = row.get(at..at + 3) else {
                            return Some(Err(()));
                        };
                        let weight = row_share * column_share;
                        for (channel, value) in sum.iter_mut().zip(pixel) {
                            *channel += u64::from(*value) * weight;
                        }
                    }
                }
            }
            // An output row is complete once the source row past its last
            // is fed: the rows before the one this source row's foot lies
            // in, or every row once the source's foot is in.
            let fed = source_row + 1;
            let complete = if fed == self.height {
                self.out_height
            } else {
                // The output row the next source row starts in.
                fed * self.out_height / self.height
            };
            while self.out_row < complete {
                if self.pending.is_empty() {
                    // A row no source row touched: cannot happen, since
                    // every output row covers at least one source row.
                    return Some(Err(()));
                }
                let sums = self.pending.remove(0);
                for channel in sums.iter().flatten() {
                    // The shares sum to `width` across the box and to
                    // `height` down it, so the box's weight is their
                    // product.
                    out.push(u8::try_from((channel + total / 2) / total).unwrap_or(u8::MAX));
                }
                self.out_row += 1;
            }
        }
        self.source_row += band.height;
        if out.is_empty() {
            return None;
        }
        let height = out.len() / (self.out_width * 3);
        // The rows are counted done above, so a buffer refused here (none
        // is, under a band and a row of the ceiling) is a fault, not none.
        let Some(mut rows) = Rgb8::new(self.out_width, height) else {
            return Some(Err(()));
        };
        rows.data = out;
        Some(Ok(rows))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(width: usize, height: usize, at: impl Fn(usize, usize) -> [u8; 3]) -> Rgb8 {
        let mut image = Rgb8::new(width, height).unwrap();
        for y in 0..height {
            for x in 0..width {
                image.data[(y * width + x) * 3..][..3].copy_from_slice(&at(x, y));
            }
        }
        image
    }

    #[test]
    fn shrunk_keeps_the_ratio_and_never_enlarges() {
        assert_eq!(shrunk(8256, 5504, 2048), (2048, 1365));
        assert_eq!(shrunk(5504, 8256, 2048), (1365, 2048));
        assert_eq!(shrunk(3000, 2000, 3000), (3000, 2000));
        assert_eq!(shrunk(3000, 2000, 9000), (3000, 2000));
        assert_eq!(shrunk(3000, 1, 3), (3, 1));
        assert_eq!(shrunk(0, 0, 100), (0, 0));
        assert_eq!(shrunk(3000, 2000, 0), (3000, 2000));
    }

    #[test]
    fn a_span_shares_the_output_pixel_over_the_source_it_covers() {
        // Three source pixels into two: the second is split down its
        // middle, a share of one unit in two each side.
        assert_eq!(span(3, 2, 0), (0, vec![2, 1]));
        assert_eq!(span(3, 2, 1), (1, vec![1, 2]));
        // A whole ratio: each output pixel two whole source pixels.
        assert_eq!(span(4, 2, 1), (2, vec![2, 2]));
        // Past the output, or a zero axis: nothing.
        assert_eq!(span(4, 2, 2), (0, vec![]));
        assert_eq!(span(4, 0, 0), (0, vec![]));
        // The axes swapped: the output pixels a source pixel lies in.
        assert_eq!(span(2, 3, 1), (0, vec![1, 1]));
    }

    /// The whole shrink, fed in bands of `rows` source rows.
    fn shrink(source: &Rgb8, out_width: usize, out_height: usize, rows: usize) -> Rgb8 {
        let mut shrink = Shrink::new(source.width, source.height, out_width, out_height).unwrap();
        let mut data = Vec::new();
        let mut first = 0;
        while first < source.height {
            let height = rows.min(source.height - first);
            let band = image(source.width, height, |x, y| {
                source.pixel(x, first + y).unwrap()
            });
            if let Some(out) = shrink.push(&band) {
                data.extend_from_slice(&out.unwrap().data);
            }
            first += height;
        }
        assert_eq!(shrink.done(), out_height);
        let mut out = Rgb8::new(out_width, out_height).unwrap();
        out.data = data;
        assert!(out.is_consistent());
        out
    }

    #[test]
    fn the_shrink_is_the_weighted_mean_rounded_half_up() {
        // One row, three pixels 0, 90, 180 into two: the first is a whole
        // 0 and half a 90 over a width of one and a half, 30; the second
        // half a 90 and a whole 180, 150.
        let source = image(3, 1, |x, _| [(x * 90) as u8; 3]);
        assert_eq!(shrink(&source, 2, 1, 1).data, [30, 30, 30, 150, 150, 150]);
        // A 2:1 shrink of a checker is the mean of its two values.
        let checker = image(
            4,
            2,
            |x, y| if (x + y) % 2 == 0 { [0; 3] } else { [255; 3] },
        );
        assert_eq!(shrink(&checker, 2, 1, 2).data, [128; 6]);
        // A flat image stays flat at any ratio, and a shrink to the same
        // size is the identity.
        let flat = image(7, 5, |_, _| [17, 200, 3]);
        assert_eq!(shrink(&flat, 3, 2, 5).data, [17, 200, 3].repeat(6));
        assert_eq!(shrink(&flat, 7, 5, 2), flat);
    }

    #[test]
    fn bands_of_any_size_give_the_same_rows_and_hold_only_the_rows_in_progress() {
        let source = image(11, 9, |x, y| {
            [(x * 23) as u8, (y * 29) as u8, ((x ^ y) * 7) as u8]
        });
        let whole = shrink(&source, 5, 4, 9);
        for rows in 1..=9 {
            assert_eq!(shrink(&source, 5, 4, rows), whole, "{rows} rows a band");
        }
        // Fed one source row at a time, the rows come out as each
        // completes and no more than two are in progress: the tall
        // shrink (nine into two) gives none until its fifth row.
        let mut tall = Shrink::new(11, 9, 5, 2).unwrap();
        let mut given = Vec::new();
        for y in 0..9 {
            let band = image(11, 1, |x, _| source.pixel(x, y).unwrap());
            let out = tall.push(&band);
            assert!(
                tall.pending.len() <= 2,
                "row {y}: {} pending",
                tall.pending.len()
            );
            match (y, out) {
                (4, Some(Ok(rows))) | (8, Some(Ok(rows))) => {
                    assert_eq!(rows.height, 1);
                    given.push(rows);
                }
                (_, None) => {}
                (_, other) => panic!("row {y}: {:?}", other.map(|r| r.map(|i| i.height))),
            }
        }
        assert_eq!(given.len(), 2);
        assert_eq!(tall.done(), 2);
        // Refusals: an axis past its source or zero, a band of the wrong
        // width, and one past the foot.
        assert!(Shrink::new(11, 9, 12, 4).is_none());
        assert!(Shrink::new(11, 9, 5, 10).is_none());
        assert!(Shrink::new(11, 9, 0, 4).is_none());
        let mut shrink = Shrink::new(11, 9, 5, 4).unwrap();
        let narrow = image(10, 2, |_, _| [0; 3]);
        assert_eq!(shrink.push(&narrow), Some(Err(())));
        let tall = image(11, 10, |_, _| [0; 3]);
        assert_eq!(shrink.push(&tall), Some(Err(())));
    }
}

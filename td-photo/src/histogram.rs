//! The develop box's histogram: how many of the shown frame's pixels hold
//! each 8-bit value, per channel, and the columns the pane paints of it.

use crate::image::Rgb8;

/// One bin per 8-bit value.
pub const BINS: usize = 256;

/// Each 8-bit value's count in the red, green and blue channels.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Histogram {
    counts: Vec<[u32; 3]>,
}

impl Histogram {
    /// The counts of `image`, `None` for an empty one or a buffer that is
    /// not its axes' size.
    pub fn of(image: &Rgb8) -> Option<Histogram> {
        let pixels = image.width.checked_mul(image.height)?;
        if pixels == 0 || image.data.len() != pixels.checked_mul(3)? {
            return None;
        }
        let mut counts = vec![[0u32; 3]; BINS];
        for pixel in image.data.as_chunks::<3>().0 {
            for (channel, value) in pixel.iter().enumerate() {
                if let Some(slot) = counts
                    .get_mut(usize::from(*value))
                    .and_then(|bin| bin.get_mut(channel))
                {
                    *slot = slot.saturating_add(1);
                }
            }
        }
        Some(Histogram { counts })
    }

    /// The bins' counts, black first.
    pub fn counts(&self) -> &[[u32; 3]] {
        &self.counts
    }

    /// Each of `columns` columns' channel heights in `0..=height`: a
    /// column the mean of the bins it spans, the tallest column spanning
    /// neither end bin full height, and a column on black or white (where
    /// clipped pixels pile up, more than one when there are more columns
    /// than bins) held to full height rather than scaling the rest down
    /// to a sliver.
    pub fn heights(&self, columns: usize, height: u32) -> Vec<[u32; 3]> {
        if columns == 0 {
            return Vec::new();
        }
        // Each column's channel means and whether it spans an end bin.
        let means: Vec<([f64; 3], bool)> = (0..columns)
            .map(|column| {
                let from = (column * BINS / columns).min(BINS - 1);
                let to = ((column + 1) * BINS / columns).clamp(from + 1, BINS);
                let span = self.counts.get(from..to).unwrap_or(&[]);
                let mut sum = [0.0f64; 3];
                for bin in span {
                    for (total, count) in sum.iter_mut().zip(bin) {
                        *total += f64::from(*count);
                    }
                }
                let mean = sum.map(|total| total / span.len().max(1) as f64);
                (mean, from == 0 || to == BINS)
            })
            .collect();
        let tallest = |inner: bool| {
            means
                .iter()
                .filter(|(_, end)| !inner || !end)
                .flat_map(|(mean, _)| mean.iter().copied())
                .fold(0.0f64, f64::max)
        };
        // With nothing between the ends, the ends scale themselves.
        let inner = tallest(true);
        let top = if inner > 0.0 { inner } else { tallest(false) };
        if top <= 0.0 {
            return vec![[0; 3]; columns];
        }
        means
            .iter()
            .map(|(column, _)| {
                column.map(|mean| ((mean / top).min(1.0) * f64::from(height)).round() as u32)
            })
            .collect()
    }
}

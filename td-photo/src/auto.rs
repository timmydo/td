//! Auto exposure and contrast: from a photo developed at 0 EV with no
//! contrast or look, the exposure that lifts it toward a typical key and
//! the contrast that spreads its tones to a typical range. DESIGN.md says
//! where the constants come from.

use crate::color::{srgb_encode, LUMA};
use crate::image::Rgb8;
use crate::look::contrast_curve;

/// The exposure, in stops, a photo whose key is `KEY` is given.
const BASE: f64 = 1.0;
/// The key, the geometric mean of the linear luminance, `BASE` is for.
const KEY: f64 = 0.06;
/// The share of a photo's distance from `KEY`, in stops, the exposure
/// makes up.
const ADAPT: f64 = 0.5;
/// The floor under a luminance before its logarithm, so black pixels do
/// not run the key to minus infinity.
const FLOOR: f64 = 1e-5;
/// The most the exposure may make of the luminance at the `HIGHLIGHT`
/// share, one stop past white: a photo dark overall around a bright
/// part is lifted no further than clips that part by a stop.
const HEADROOM: f64 = 2.0;
/// The share of the pixels at or below the luminance `HEADROOM` holds.
const HIGHLIGHT: f64 = 0.995;
/// The exposure's range, hundredths of a stop.
const EXPOSURE: (i32, i32) = (-100, 250);
/// The encoded distance, `0..=1`, the contrast puts between the tenth and
/// ninetieth percentiles of the luminance.
const SPREAD: f64 = 0.60;
/// The contrast's range, hundredths.
const CONTRAST: (i32, i32) = (40, 90);
/// The long edge the measure is developed at.
pub const EDGE: usize = 600;

/// What auto chooses, in the sidecar's hundredths.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Auto {
    pub exposure: i32,
    pub contrast: i32,
}

/// The sRGB decode of each 8-bit value.
fn linear_table() -> [f64; 256] {
    let mut table = [0.0; 256];
    for (index, slot) in table.iter_mut().enumerate() {
        let v = index as f64 / 255.0;
        *slot = if v <= 0.040_45 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        };
    }
    table
}

/// The value at share `p` of `values`, the index `p n` rounded down and
/// held within the slice. It permutes the slice in place, which leaves
/// every value there for the next call.
fn percentile(values: &mut [f64], p: f64) -> Option<f64> {
    let last = values.len().checked_sub(1)?;
    let index = ((p * values.len() as f64) as usize).min(last);
    let (_, value, _) = values.select_nth_unstable_by(index, f64::total_cmp);
    Some(*value)
}

/// The settings for `frame`, the photo developed at 0 EV with no contrast
/// or look over its crop; `None` for an empty frame.
pub fn choose(frame: &Rgb8) -> Option<Auto> {
    let table = linear_table();
    let mut luminance = Vec::with_capacity(frame.width.saturating_mul(frame.height));
    let mut log_sum = 0.0;
    let linear = |v: u8| table.get(usize::from(v)).copied().unwrap_or(1.0);
    for pixel in frame.data.as_chunks::<3>().0 {
        let [r, g, b] = pixel.map(linear);
        let y = f64::from(LUMA[0]) * r + f64::from(LUMA[1]) * g + f64::from(LUMA[2]) * b;
        log_sum += y.max(FLOOR).log2();
        luminance.push(y);
    }
    if luminance.is_empty() {
        return None;
    }
    let key = log_sum / luminance.len() as f64;
    let low = percentile(&mut luminance, 0.1)?;
    let high = percentile(&mut luminance, 0.9)?;
    let highlight = percentile(&mut luminance, HIGHLIGHT)?;
    let stops = (BASE + ADAPT * (KEY.log2() - key)).min((HEADROOM / highlight.max(FLOOR)).log2());
    let exposure = ((stops * 100.0).round() as i32).clamp(EXPOSURE.0, EXPOSURE.1);
    let gain = 2f64.powf(f64::from(exposure) / 100.0);
    let encode = |x: f64| f64::from(srgb_encode(x as f32));
    // The spread each contrast gives, the first closest to `SPREAD` taken:
    // the display of a percentile is the curve's image of it, the develop
    // being monotone in luminance.
    let contrast = (CONTRAST.0..=CONTRAST.1)
        .map(|hundredths| {
            let curve = contrast_curve(hundredths);
            let spread = encode(curve(high * gain)) - encode(curve(low * gain));
            (hundredths, (spread - SPREAD).abs())
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map_or(CONTRAST.0, |(hundredths, _)| hundredths);
    Some(Auto { exposure, contrast })
}

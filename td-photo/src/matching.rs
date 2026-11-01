//! Fitting a look to the camera's own rendering (`td-photo match-look`):
//! pairs of a raw developed plain and the camera's embedded preview of
//! the same frame, pooled, give a tone curve by matching their
//! luminance quantiles, applied to each channel as a camera's is, and a
//! saturation by the ratio of their chroma after it (DESIGN.md, Matching
//! the camera). Pure: 8-bit pixels in, a
//! look's text out; `main` develops the photos and writes the file.

use std::fmt::Write as _;

use crate::color::LUMA;
use crate::image::Rgb8;
use crate::look::{self, Look, MAX_CURVE_POINTS};

/// The long edge both sides of a pair are made at.
pub const EDGE: usize = 256;
/// Photos of a roll matched at most, spread over it.
pub const MAX_PHOTOS: usize = 16;
/// The fitted look's name for its file's stem: "Camera match", with the
/// stem when it is not the default and the name has the room.
pub fn name(stem: &str) -> String {
    let named = format!("Camera match ({stem})");
    if stem == "camera-match" || named.chars().count() > look::MAX_NAME {
        "Camera match".to_string()
    } else {
        named
    }
}

/// The quantiles the curve passes through, between its black, its clip
/// point and its white: denser at the ends, where a camera's curve
/// bends.
const LEVELS: &[f32] = &[
    0.01, 0.03, 0.07, 0.13, 0.21, 0.31, 0.42, 0.54, 0.66, 0.77, 0.86, 0.93, 0.97,
];
// Black, the clip point and white besides the levels.
const _: () = assert!(LEVELS.len() + 3 <= MAX_CURVE_POINTS);
/// A point closer than this in x to the one before it is left out, so
/// the points ascend as the grammar asks and the spline stays tame.
const GAP: f32 = 0.002;
/// A developed value at or past this is clipped: no quantile there is
/// fitted, and no such pixel's chroma counted.
const CLIPPED: f32 = 0.98;
/// A median chroma share below this, on either side, is a roll too
/// neutral to measure a saturation on: the ratio of two noises.
const MIN_SHARE: f32 = 0.05;
/// The luminance band whose chroma the saturation is fitted on: dark
/// pixels' chroma is noise, bright ones are near the clip.
const CHROMA_BAND: (f32, f32) = (0.02, 0.8);

/// A fitted look: the tone curve's points, `x y` in linear values from
/// black `0 0` to white at x 1 ascending in x, each channel taken
/// through it, and the saturation after it; `wanted` the ratio the
/// saturation was held from (past 2.00 the grammar's limit), `None` for
/// a roll too neutral to measure one on.
#[derive(Clone, Debug, PartialEq)]
pub struct Fit {
    pub points: Vec<(f32, f32)>,
    pub saturation: f32,
    pub wanted: Option<f32>,
}

/// The linear value of an 8-bit sRGB one.
pub fn decode(value: u8) -> f32 {
    let v = f32::from(value) / 255.0;
    if v <= 0.040_45 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

fn linear(image: &Rgb8) -> impl Iterator<Item = [f32; 3]> + '_ {
    image
        .data
        .as_chunks::<3>()
        .0
        .iter()
        .map(|&[r, g, b]| [decode(r), decode(g), decode(b)])
}

fn luma(rgb: [f32; 3]) -> f32 {
    LUMA[0] * rgb[0] + LUMA[1] * rgb[1] + LUMA[2] * rgb[2]
}

/// The quantile `q` of `sorted`, the nearest rank.
fn quantile(sorted: &[f32], q: f32) -> Option<f32> {
    let last = sorted.len().checked_sub(1)?;
    sorted.get((q * last as f32).round() as usize).copied()
}

/// Chroma relative to luminance, `(max - min) / Y`: what `saturation`
/// scales by its value, so the ratio of the two sides' is the value.
fn relative_chroma(rgb: [f32; 3]) -> Option<f32> {
    let y = luma(rgb);
    let max = rgb[0].max(rgb[1]).max(rgb[2]);
    let min = rgb[0].min(rgb[1]).min(rgb[2]);
    (y >= CHROMA_BAND.0 && y <= CHROMA_BAND.1 && max < CLIPPED).then(|| (max - min) / y)
}

fn median(mut values: Vec<f32>) -> Option<f32> {
    values.sort_by(f32::total_cmp);
    quantile(&values, 0.5)
}

/// The look that takes each pair's developed side (`.0`) nearest its
/// preview (`.1`), pooled over every pair: the curve through the
/// developed luminance quantiles to the preview's (`LEVELS`, black held,
/// a clipped or crowded quantile left out), then at the developed clip
/// the preview's level at that rank, when the developed side reaches
/// it, so a highlight the camera rolls off is matched up to the clip
/// and only what clipped (no detail left) rises to white; then the
/// saturation that brings the developed chroma after the curve to the
/// preview's (median ratio, held to `0..=2`). The curve lifting each
/// channel compresses their ratios, so the saturation fitted after it
/// also makes up what the lift took; none (1.00) on a roll whose
/// median share is under `MIN_SHARE` on either side. `None` with no
/// pixel.
pub fn fit(pairs: &[(&Rgb8, &Rgb8)]) -> Option<Fit> {
    let mut developed: Vec<f32> = Vec::new();
    let mut preview: Vec<f32> = Vec::new();
    for (raw, camera) in pairs {
        developed.extend(linear(raw).map(luma));
        preview.extend(linear(camera).map(luma));
    }
    developed.sort_by(f32::total_cmp);
    preview.sort_by(f32::total_cmp);
    let mut points = vec![(0.0f32, 0.0f32)];
    for &level in LEVELS {
        let (Some(x), Some(y)) = (quantile(&developed, level), quantile(&preview, level)) else {
            return None;
        };
        let (last_x, last_y) = points.last().copied().unwrap_or((0.0, 0.0));
        if x > last_x + GAP && x < CLIPPED {
            points.push((x, y.clamp(last_y, 1.0)));
        }
    }
    // Where the developed side reaches its clip, the camera's level at
    // that rank is the clip's: the roll-off below it is matched, and
    // only the clipped, which hold no detail, rise from it to white.
    let below = developed.partition_point(|&v| v < CLIPPED);
    let (last_x, last_y) = points.last().copied().unwrap_or((0.0, 0.0));
    if below < developed.len() && CLIPPED > last_x + GAP {
        let rank = below as f32 / developed.len().saturating_sub(1).max(1) as f32;
        let level = quantile(&preview, rank.min(1.0)).unwrap_or(1.0);
        points.push((CLIPPED, level.clamp(last_y, 1.0)));
    }
    points.push((1.0, 1.0));
    let curve = Look::parse(text(&points, None, "fit").as_bytes()).ok()?;
    let after: Vec<f32> = pairs
        .iter()
        .flat_map(|(raw, _)| linear(raw))
        .filter_map(|rgb| relative_chroma(curve.apply(rgb)))
        .collect();
    let camera: Vec<f32> = pairs
        .iter()
        .flat_map(|(_, camera)| linear(camera))
        .filter_map(relative_chroma)
        .collect();
    let wanted = match (median(after), median(camera)) {
        (Some(after), Some(camera)) if after >= MIN_SHARE && camera >= MIN_SHARE => {
            Some(camera / after)
        }
        _ => None,
    };
    let saturation = wanted.map_or(1.0, |ratio| ratio.clamp(0.0, 2.0));
    Some(Fit {
        points,
        saturation: (saturation * 100.0).round() / 100.0,
        wanted,
    })
}

/// A look's text: its header, `name`, the curve through `points` on each
/// channel, then the saturation when there is one.
fn text(points: &[(f32, f32)], saturation: Option<f32>, name: &str) -> String {
    let mut out = format!("{}\nname {name}\n", look::HEADER);
    for channel in ["r", "g", "b"] {
        let _ = write!(out, "curve {channel}");
        for (x, y) in points {
            let _ = write!(out, " {x:.4} {y:.4}");
        }
        out.push('\n');
    }
    if let Some(s) = saturation {
        let _ = writeln!(out, "saturation {s:.2}");
    }
    out
}

impl Fit {
    /// The look file for the fit, named `name`: the saturation left out
    /// when it is 1.00.
    pub fn text(&self, name: &str) -> String {
        let saturation = (self.saturation != 1.0).then_some(self.saturation);
        text(&self.points, saturation, name)
    }
}

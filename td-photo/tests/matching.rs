#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

//! Fitting a look to the camera's rendering: the quantile-matched curve
//! and the chroma-ratio saturation recover a known rendering, an
//! identical pair is the identity, and the text is a look.

use td_photo::color::{srgb_encode, LUMA};
use td_photo::image::Rgb8;
use td_photo::look::{Look, MAX_CURVE_POINTS};
use td_photo::matching::{decode, fit};

/// A frame of every luminance with hues around it, linear, as pixels.
fn scene() -> Vec<[f32; 3]> {
    let mut pixels = Vec::new();
    for y in 0..48 {
        for x in 0..64 {
            let level = 0.003 + 0.85 * ((x + 64 * y) as f32 / (64.0 * 48.0)).powf(2.2);
            let hue = (x % 6) as f32;
            let tint = match hue as u32 {
                0 => [1.2, 0.9, 0.8],
                1 => [0.85, 1.1, 0.9],
                2 => [0.8, 0.9, 1.3],
                3 => [1.0, 1.0, 1.0],
                4 => [1.15, 1.05, 0.7],
                _ => [0.9, 0.95, 1.2],
            };
            pixels.push(tint.map(|t| t * level));
        }
    }
    pixels
}

fn encode(pixels: &[[f32; 3]]) -> Rgb8 {
    Rgb8 {
        width: 64,
        height: 48,
        data: pixels
            .iter()
            .flat_map(|p| p.map(|v| (srgb_encode(v) * 255.0 + 0.5).floor() as u8))
            .collect(),
    }
}

fn luma(rgb: [f32; 3]) -> f32 {
    LUMA[0] * rgb[0] + LUMA[1] * rgb[1] + LUMA[2] * rgb[2]
}

#[test]
fn the_srgb_decode_inverts_the_encode() {
    assert_eq!(decode(0), 0.0);
    assert!((decode(255) - 1.0).abs() < 1e-6);
    for v in [0.001f32, 0.01, 0.18, 0.5, 0.9] {
        let byte = (srgb_encode(v) * 255.0 + 0.5).floor() as u8;
        assert!((decode(byte) - v).abs() < v * 0.03 + 0.0005, "{v}");
    }
}

#[test]
fn an_identical_pair_fits_the_identity() {
    let image = encode(&scene());
    let fitted = fit(&[(&image, &image)]).unwrap();
    assert!(fitted.points.len() <= MAX_CURVE_POINTS);
    assert_eq!(fitted.points.first(), Some(&(0.0, 0.0)));
    assert_eq!(fitted.points.last(), Some(&(1.0, 1.0)));
    for &(x, y) in &fitted.points {
        assert_eq!(x, y);
    }
    assert_eq!(fitted.saturation, 1.0);
    let text = fitted.text("Camera match");
    assert!(!text.contains("saturation"), "{text}");
    assert!(text.starts_with("td-photo look 1\nname Camera match\ncurve r 0.0000 0.0000 "));
    assert!(text.contains("\ncurve g 0.0000 0.0000 ") && text.contains("\ncurve b 0.0000 0.0000 "));
    assert!(Look::parse(text.as_bytes()).is_ok());
}

#[test]
fn a_cameras_curve_and_saturation_are_recovered() {
    // The "camera" lifts each luminance to Y^0.8 (scaling the pixel) and
    // spreads chroma by 1.3 about it: the fit must find both, pooled
    // over two pairs.
    let plain = scene();
    let rendered: Vec<[f32; 3]> = plain
        .iter()
        .map(|&rgb| {
            let y = luma(rgb);
            let lifted = rgb.map(|v| v * y.powf(0.8) / y);
            let y = luma(lifted);
            lifted.map(|v| y + 1.3 * (v - y))
        })
        .collect();
    let (raw, camera) = (encode(&plain), encode(&rendered));
    let fitted = fit(&[(&raw, &camera), (&raw, &camera)]).unwrap();
    assert!(fitted.points.len() >= 8, "{:?}", fitted.points);
    for pair in fitted.points.windows(2) {
        assert!(pair[1].0 > pair[0].0 && pair[1].1 >= pair[0].1, "{pair:?}");
    }
    for &(x, y) in &fitted.points[1..fitted.points.len() - 1] {
        let want = x.powf(0.8);
        assert!((y - want).abs() < 0.03 * want + 0.003, "{x} {y} {want}");
    }
    // The curve lifting each channel compresses their ratios, so the
    // saturation fitted after it also makes up what the lift took: past
    // the camera's 1.3.
    assert!(fitted.saturation > 1.3, "{}", fitted.saturation);
    let text = fitted.text("Camera match");
    assert!(text.ends_with(&format!("saturation {:.2}\n", fitted.saturation)));
    let look = Look::parse(text.as_bytes()).unwrap();
    assert_eq!(look.name(), Some("Camera match"));
    // The look takes the plain frame near the rendering: its luminance,
    // and its chroma's share of it in the midtones at the median.
    assert_eq!(look.operations(), 4);
    let mut error = 0.0;
    for (p, r) in plain.iter().zip(&rendered) {
        error += (luma(look.apply(*p)) - luma(*r)).abs();
    }
    assert!(error / (plain.len() as f32) < 0.015, "{error}");
    let share = |rgb: [f32; 3]| {
        let y = luma(rgb);
        let spread = rgb[0].max(rgb[1]).max(rgb[2]) - rgb[0].min(rgb[1]).min(rgb[2]);
        (0.02..=0.8).contains(&y).then(|| spread / y)
    };
    let median = |mut v: Vec<f32>| {
        v.sort_by(f32::total_cmp);
        v[v.len() / 2]
    };
    let got = median(plain.iter().filter_map(|p| share(look.apply(*p))).collect());
    let want = median(rendered.iter().filter_map(|r| share(*r)).collect());
    assert!((got / want - 1.0).abs() < 0.08, "{got} {want}");
}

#[test]
fn a_clipped_develop_takes_the_cameras_white_not_white() {
    // The plain develop three times brighter than the camera: it clips
    // where the camera rolls off. Below the clip the camera's levels
    // are matched with no steep ramp; at the clip the curve is at the
    // camera's level there, and only the clipped rise to white.
    let rendered = scene();
    let plain: Vec<[f32; 3]> = rendered.iter().map(|p| p.map(|v| v * 3.0)).collect();
    let (raw, camera) = (encode(&plain), encode(&rendered));
    let fitted = fit(&[(&raw, &camera)]).unwrap();
    let n = fitted.points.len();
    assert_eq!(fitted.points[n - 1], (1.0, 1.0));
    let (x, y) = fitted.points[n - 2];
    assert_eq!(x, 0.98);
    assert!(y < 0.6, "{:?}", fitted.points);
    for pair in fitted.points[..n - 1].windows(2) {
        let slope = (pair[1].1 - pair[0].1) / (pair[1].0 - pair[0].0);
        assert!(slope < 4.0, "{pair:?}");
    }
    assert!(Look::parse(fitted.text("Camera match").as_bytes()).is_ok());
}

#[test]
fn a_roll_clipping_a_little_keeps_every_level_and_the_clip() {
    // A ramp with no crowded quantile and a few clipped pixels: every
    // level is kept, and the clip point beside black and white, within
    // the grammar's points.
    let pixels: Vec<[f32; 3]> = (0..64 * 48)
        .map(|i| {
            let v = if i >= 64 * 48 - 10 {
                1.0
            } else {
                0.9 * i as f32 / (64.0 * 48.0)
            };
            [v, v, v]
        })
        .collect();
    let image = encode(&pixels);
    let fitted = fit(&[(&image, &image)]).unwrap();
    assert_eq!(fitted.points.len(), MAX_CURVE_POINTS, "{:?}", fitted.points);
    assert!(
        fitted.points.iter().any(|&(x, _)| x == 0.98),
        "{:?}",
        fitted.points
    );
    assert!(Look::parse(fitted.text("Camera match").as_bytes()).is_ok());
}

#[test]
fn a_neutral_roll_measures_no_saturation_and_a_large_one_is_held() {
    let grey: Vec<[f32; 3]> = scene()
        .iter()
        .map(|&p| {
            let y = luma(p);
            [y, y, y]
        })
        .collect();
    let image = encode(&grey);
    let fitted = fit(&[(&image, &image)]).unwrap();
    assert_eq!((fitted.wanted, fitted.saturation), (None, 1.0));
    // A camera spreading chroma fourfold asks past the grammar's 2.00.
    let plain = scene();
    let vivid: Vec<[f32; 3]> = plain
        .iter()
        .map(|&p| {
            let y = luma(p);
            p.map(|v| (y + 4.0 * (v - y)).max(0.0))
        })
        .collect();
    let (raw, camera) = (encode(&plain), encode(&vivid));
    let fitted = fit(&[(&raw, &camera)]).unwrap();
    assert_eq!(fitted.saturation, 2.0);
    assert!(fitted.wanted.unwrap() > 2.0, "{:?}", fitted.wanted);
}

#[test]
fn nothing_to_fit_is_none() {
    let empty = Rgb8 {
        width: 0,
        height: 0,
        data: Vec::new(),
    };
    assert_eq!(fit(&[]), None);
    assert_eq!(fit(&[(&empty, &empty)]), None);
}

//! The auto rule, `td_photo::auto::choose`, over grey frames whose
//! luminance is each pixel's decoded value.

use td_photo::auto::{choose, Auto};
use td_photo::color::srgb_encode;
use td_photo::image::Rgb8;
use td_photo::look::contrast_curve;

/// The 8-bit sRGB value of linear `v`.
fn byte(v: f64) -> u8 {
    (srgb_encode(v as f32) * 255.0 + 0.5) as u8
}

/// The linear value of 8-bit sRGB `b`.
fn linear(b: u8) -> f64 {
    let v = f64::from(b) / 255.0;
    if v <= 0.040_45 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

/// A one-row grey frame of `count` pixels at each 8-bit value.
fn frame(levels: &[(u8, usize)]) -> Rgb8 {
    let data: Vec<u8> = levels
        .iter()
        .flat_map(|&(value, count)| std::iter::repeat_n(value, count * 3))
        .collect();
    Rgb8 {
        width: data.len() / 3,
        height: 1,
        data,
    }
}

#[test]
fn nothing_to_measure_is_no_choice() {
    let empty = Rgb8 {
        width: 0,
        height: 0,
        data: Vec::new(),
    };
    assert_eq!(choose(&empty), None);
}

/// A frame at the key the rule is for is given a stop; one two stops
/// darker is given half the difference more; black and white reach the
/// range's ends.
#[test]
fn the_exposure_makes_up_half_the_distance_to_the_key() {
    let at_key = choose(&frame(&[(byte(0.06), 100)])).unwrap();
    assert!((at_key.exposure - 100).abs() <= 2, "{at_key:?}");
    let darker = choose(&frame(&[(byte(0.015), 100)])).unwrap();
    assert!(
        (darker.exposure - at_key.exposure - 100).abs() <= 4,
        "{darker:?} {at_key:?}"
    );
    assert_eq!(choose(&frame(&[(0, 100)])).unwrap().exposure, 250);
    assert_eq!(choose(&frame(&[(255, 100)])).unwrap().exposure, -100);
}

/// The luminance weighs the channels as `LUMA` does: a white red frame is
/// a stop and a half dimmer to the rule than a white green one.
#[test]
fn the_luminance_weighs_the_channels() {
    let solid = |rgb: [u8; 3]| {
        let data: Vec<u8> = std::iter::repeat_n(rgb, 100).flatten().collect();
        choose(&Rgb8 {
            width: 100,
            height: 1,
            data,
        })
        .unwrap()
    };
    // 1 + (log2(0.06) - log2(y)) / 2 stops for luminance y.
    let expected = |y: f64| ((1.0 + (0.06f64.log2() - y.log2()) / 2.0) * 100.0).round() as i32;
    let [r, g, _] = td_photo::color::LUMA.map(f64::from);
    assert_eq!(solid([255, 0, 0]).exposure, expected(r));
    assert_eq!(solid([0, 255, 0]).exposure, expected(g));
}

/// A dark frame with a bright part is lifted no further than puts that
/// part's 99.5th percentile a stop past white.
#[test]
fn the_exposure_keeps_the_highlights_within_a_stop_of_white() {
    let dark = choose(&frame(&[(byte(0.015), 980)])).unwrap();
    assert!(dark.exposure > 190, "{dark:?}");
    let lit = choose(&frame(&[(byte(0.015), 980), (255, 20)])).unwrap();
    assert_eq!(lit.exposure, 100);
    // Below the share the guard holds, a bright speck does not bind.
    let speck = choose(&frame(&[(byte(0.015), 998), (255, 2)])).unwrap();
    assert!(speck.exposure > 190, "{speck:?}");
}

/// The contrast is the one in range whose curve puts the tenth and
/// ninetieth percentiles closest to the target spread once exposed: a
/// narrow frame is given more than a wide one, and each choice beats its
/// neighbours.
#[test]
fn the_contrast_spreads_the_tones_to_the_target() {
    let spread = |levels: &[(u8, usize)], auto: Auto, contrast: i32| {
        let mut values: Vec<f64> = levels
            .iter()
            .flat_map(|&(value, count)| std::iter::repeat_n(linear(value), count))
            .collect();
        values.sort_by(f64::total_cmp);
        let at = |p: f64| values[((p * values.len() as f64) as usize).min(values.len() - 1)];
        let gain = 2f64.powf(f64::from(auto.exposure) / 100.0);
        let curve = contrast_curve(contrast);
        let encode = |x: f64| f64::from(srgb_encode(curve(x * gain) as f32));
        encode(at(0.9)) - encode(at(0.1))
    };
    let narrow: &[(u8, usize)] = &[(byte(0.05), 50), (byte(0.09), 50)];
    let middle: &[(u8, usize)] = &[
        (byte(0.02), 20),
        (byte(0.04), 20),
        (byte(0.06), 20),
        (byte(0.10), 20),
        (byte(0.25), 20),
    ];
    let wide: &[(u8, usize)] = &[(byte(0.004), 50), (byte(0.5), 50)];
    let chosen: Vec<Auto> = [narrow, middle, wide]
        .iter()
        .map(|levels| choose(&frame(levels)).unwrap())
        .collect();
    assert_eq!(chosen[0].contrast, 90, "{chosen:?}");
    assert!((41..90).contains(&chosen[1].contrast), "{chosen:?}");
    assert_eq!(chosen[2].contrast, 40, "{chosen:?}");
    for (levels, auto) in [narrow, middle, wide].iter().zip(&chosen) {
        let distance = |contrast| (spread(levels, *auto, contrast) - 0.60).abs();
        for other in 40..=90 {
            assert!(
                distance(auto.contrast) <= distance(other),
                "{auto:?} against {other}"
            );
        }
    }
}

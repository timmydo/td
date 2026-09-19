#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

//! The colour and development contract: the camera matrix construction,
//! the transfer table, the superpixel demosaic, the resampler, the fit
//! rule, orientation, and a complete development of synthetic frames to
//! expected 8-bit values.

use td_photo::camera;
use td_photo::color::{
    self, apply, camera_color, invert, multiply, srgb_encode, Transfer, MIDDLE_GREY,
};
use td_photo::develop::{
    self, bilinear, export_band, export_geometry, fit, level2, level3, orient, resample,
    resample_u16, superpixel, Export, Level1, Level2, Params, Region, Source,
};
use td_photo::image::Rgb8;
use td_photo::look::Look;
use td_photo::nef::{Cfa, Crop, Decoded};

fn close(a: f32, b: f32, tolerance: f32) -> bool {
    (a - b).abs() <= tolerance
}

fn z8() -> &'static camera::Camera {
    camera::find("NIKON CORPORATION", "NIKON Z 8").unwrap()
}

// ------------------------------------------------------------------ colour

#[test]
fn the_camera_table_matches_on_trimmed_strings() {
    assert!(camera::find("NIKON CORPORATION", "NIKON Z 8").is_some());
    assert!(camera::find("NIKON CORPORATION\0", " NIKON Z 8 \0").is_some());
    assert!(camera::find("NIKON CORPORATION", "NIKON Z 9").is_none());
    assert!(camera::find("Canon", "NIKON Z 8").is_none());
    let c = z8();
    assert_eq!((c.black, c.white), (1008, 15892));
    assert_eq!(c.xyz_to_cam[0], [11423, -4564, -1123]);
    assert_eq!(c.xyz_to_cam[1], [-4816, 12895, 2119]);
    assert_eq!(c.xyz_to_cam[2], [-210, 1061, 7282]);
}

#[test]
fn the_z8_colour_maps_balanced_white_to_display_white() {
    let color = camera_color(&z8().xyz_to_cam).unwrap();
    let white = apply(&color.rgb_cam, [1.0, 1.0, 1.0]);
    for channel in white {
        assert!(close(channel, 1.0, 1e-4), "{white:?}");
    }
    assert!(close(color.daylight[1], 1.0, 1e-6));
    // Nikon sensors need red and blue raised under daylight.
    assert!(
        color.daylight[0] > 1.5 && color.daylight[0] < 2.6,
        "{:?}",
        color.daylight
    );
    assert!(
        color.daylight[2] > 1.1 && color.daylight[2] < 1.9,
        "{:?}",
        color.daylight
    );
    // Each row of rgb_cam sums to one, the normalization's other face.
    for row in color.rgb_cam {
        assert!(close(row.iter().sum::<f32>(), 1.0, 1e-4), "{row:?}");
    }
}

#[test]
fn matrix_inversion_recovers_the_identity_and_refuses_singular() {
    let m = [[2.0, 0.5, 0.1], [0.3, 1.5, 0.2], [0.1, 0.2, 0.9]];
    let inv = invert(&m).unwrap();
    let id = multiply(&m, &inv);
    for (i, row) in id.iter().enumerate() {
        for (j, cell) in row.iter().enumerate() {
            let expected = if i == j { 1.0 } else { 0.0 };
            assert!(close(*cell, expected, 1e-5), "{id:?}");
        }
    }
    assert!(invert(&[[1.0, 2.0, 3.0], [2.0, 4.0, 6.0], [0.0, 1.0, 1.0]]).is_none());
    assert_eq!(
        camera_color(&[[0; 3]; 3]).unwrap_err(),
        color::Error::Singular
    );
}

#[test]
fn the_srgb_transfer_is_monotone_with_pinned_endpoints() {
    let t = Transfer::srgb();
    assert_eq!(t.encode(0.0), 0);
    assert_eq!(t.encode(1.0), 255);
    assert_eq!(t.encode(-1.0), 0);
    assert_eq!(t.encode(7.0), 255);
    assert_eq!(t.encode(f32::NAN), 0);
    assert!(close(srgb_encode(0.003_130_8), 0.040_45, 1e-4));
    assert!(close(srgb_encode(0.5), 0.735_36, 1e-4));
    assert_eq!(t.encode(MIDDLE_GREY), 119);
    let mut last = 0u8;
    for i in 0..=65535u32 {
        let v = t.encode(i as f32 / 65535.0);
        assert!(v >= last, "not monotone at {i}");
        last = v;
    }
    assert_eq!(Transfer::default().encode(0.5), t.encode(0.5));
}

// --------------------------------------------------------------- demosaic

fn frame(width: usize, height: usize, samples: Vec<u16>) -> Decoded {
    Decoded {
        width,
        height,
        samples,
        corrupt: 0,
    }
}

#[test]
fn superpixel_scales_reds_and_blues_and_averages_the_greens() {
    // One RGGB quad: R 600, G 350 / 850, B 1100 over black 100, white 1100.
    let decoded = frame(2, 2, vec![600, 350, 850, 1100]);
    let level1 = superpixel(
        &decoded,
        Cfa::RGGB,
        Crop {
            left: 0,
            top: 0,
            width: 2,
            height: 2,
        },
        100,
        1100,
        4,
    )
    .unwrap();
    assert_eq!((level1.width, level1.height), (1, 1));
    assert_eq!(level1.rgb, vec![32767, 32767, 65535]);
}

#[test]
fn superpixel_clamps_below_black_and_above_white() {
    let decoded = frame(2, 2, vec![50, 5000, 5000, 100]);
    let level1 = superpixel(
        &decoded,
        Cfa::RGGB,
        Crop {
            left: 0,
            top: 0,
            width: 2,
            height: 2,
        },
        100,
        1100,
        1,
    )
    .unwrap();
    assert_eq!(level1.rgb, vec![0, 65535, 0]);
}

#[test]
fn superpixel_follows_the_pattern_and_an_odd_crop_origin() {
    // A 4x4 BGGR frame; the crop starts at (1, 1), so its top-left sample
    // is the pattern's bottom-right: red.
    let mut samples = vec![0u16; 16];
    let cfa = Cfa::from_codes(&[2, 1, 1, 0]).unwrap();
    for y in 0..4 {
        for x in 0..4 {
            samples[y * 4 + x] = match cfa.at(x, y) {
                td_photo::nef::Channel::Red => 300,
                td_photo::nef::Channel::Green => 200,
                td_photo::nef::Channel::Blue => 100,
            };
        }
    }
    let decoded = frame(4, 4, samples);
    let crop = Crop {
        left: 1,
        top: 1,
        width: 2,
        height: 2,
    };
    let level1 = superpixel(&decoded, cfa, crop, 0, 400, 2).unwrap();
    let scale = |v: u32| (v * 65535 / 400) as u16;
    assert_eq!(level1.rgb, vec![scale(300), scale(200), scale(100)]);
    // The same frame from (0, 0) gives the same colour: the pattern is read
    // from sensor coordinates, not from the crop's.
    let full = superpixel(
        &decoded,
        cfa,
        Crop {
            left: 0,
            top: 0,
            width: 4,
            height: 4,
        },
        0,
        400,
        16,
    )
    .unwrap();
    assert_eq!((full.width, full.height), (2, 2));
    for px in full.rgb.as_chunks::<3>().0 {
        assert_eq!(*px, [scale(300), scale(200), scale(100)]);
    }
}

#[test]
fn superpixel_refuses_bad_crops_levels_and_buffers() {
    let decoded = frame(4, 4, vec![0; 16]);
    let full = Crop {
        left: 0,
        top: 0,
        width: 4,
        height: 4,
    };
    assert_eq!(
        superpixel(
            &decoded,
            Cfa::RGGB,
            Crop {
                left: 2,
                top: 0,
                width: 4,
                height: 4
            },
            0,
            100,
            1
        )
        .unwrap_err(),
        develop::Error::Crop
    );
    assert_eq!(
        superpixel(&decoded, Cfa::RGGB, full, 100, 100, 1).unwrap_err(),
        develop::Error::Levels
    );
    let short = frame(4, 4, vec![0; 15]);
    assert_eq!(
        superpixel(&short, Cfa::RGGB, full, 0, 100, 1).unwrap_err(),
        develop::Error::Size
    );
    // Odd crop axes drop the last sample row and column.
    let odd = superpixel(
        &frame(5, 5, vec![0; 25]),
        Cfa::RGGB,
        Crop {
            left: 0,
            top: 0,
            width: 5,
            height: 5,
        },
        0,
        100,
        3,
    )
    .unwrap();
    assert_eq!((odd.width, odd.height), (2, 2));
}

// --------------------------------------------------------------- resample

fn grey(values: &[f32]) -> Vec<f32> {
    values.iter().flat_map(|v| [*v, *v, *v]).collect()
}

#[test]
fn resample_keeps_a_constant_image_constant() {
    let src = vec![0.25f32; 7 * 5 * 3];
    for (dw, dh) in [(3, 2), (7, 5), (10, 9), (1, 1)] {
        let out = resample(&src, 7, 5, dw, dh, 4).unwrap();
        assert_eq!(out.len(), dw * dh * 3);
        for v in out {
            assert!(close(v, 0.25, 1e-5), "{dw}x{dh}: {v}");
        }
    }
}

#[test]
fn resample_reduces_by_area_and_enlarges_bilinearly() {
    let step = grey(&[0.0, 0.0, 1.0, 1.0]);
    let halved = resample(&step, 4, 1, 2, 1, 1).unwrap();
    assert_eq!(halved, grey(&[0.0, 1.0]));
    let uneven = resample(&grey(&[0.0, 0.0, 1.0]), 3, 1, 2, 1, 2).unwrap();
    assert!(close(uneven[0], 0.0, 1e-6));
    assert!(close(uneven[3], 2.0 / 3.0, 1e-6));
    let enlarged = resample(&grey(&[0.0, 1.0]), 2, 1, 4, 1, 1).unwrap();
    for (got, want) in enlarged
        .as_chunks::<3>()
        .0
        .iter()
        .zip([0.0, 0.25, 0.75, 1.0])
    {
        assert!(close(got[0], want, 1e-6), "{enlarged:?}");
    }
    // The vertical pass does the same.
    let column = grey(&[0.0, 0.0, 1.0, 1.0]);
    let halved = resample(&column, 1, 4, 1, 2, 3).unwrap();
    assert_eq!(halved, grey(&[0.0, 1.0]));
    assert_eq!(
        resample(&column, 1, 4, 0, 2, 1).unwrap_err(),
        develop::Error::Size
    );
    assert_eq!(
        resample(&column, 2, 4, 1, 2, 1).unwrap_err(),
        develop::Error::Size
    );
}

#[test]
fn fit_keeps_aspect_and_never_enlarges() {
    assert_eq!(fit(4140, 2760, 1600), (1600, 1067));
    assert_eq!(fit(2760, 4140, 1600), (1067, 1600));
    assert_eq!(fit(400, 300, 1600), (400, 300));
    assert_eq!(fit(4140, 2760, 0), (1, 1));
    assert_eq!(fit(1000, 10, 100), (100, 1));
}

// ------------------------------------------------------------ orientation

#[test]
fn orientation_turns_the_image() {
    let image = Rgb8 {
        width: 2,
        height: 1,
        data: vec![1, 1, 1, 2, 2, 2],
    };
    let cw = orient(image.clone(), 6);
    assert_eq!((cw.width, cw.height), (1, 2));
    assert_eq!(cw.data, vec![1, 1, 1, 2, 2, 2]);
    let ccw = orient(image.clone(), 8);
    assert_eq!((ccw.width, ccw.height), (1, 2));
    assert_eq!(ccw.data, vec![2, 2, 2, 1, 1, 1]);
    let half = orient(image.clone(), 3);
    assert_eq!((half.width, half.height), (2, 1));
    assert_eq!(half.data, vec![2, 2, 2, 1, 1, 1]);
    assert_eq!(orient(image.clone(), 1), image);
    assert_eq!(orient(image.clone(), 7), image);
    // A 2x3 quarter turn, pixel by pixel.
    let tall = Rgb8 {
        width: 2,
        height: 3,
        data: (0..18).map(|i| (i / 3) as u8).collect(),
    };
    let turned = orient(tall.clone(), 6);
    assert_eq!((turned.width, turned.height), (3, 2));
    for oy in 0..2 {
        for ox in 0..3 {
            assert_eq!(turned.pixel(ox, oy), tall.pixel(oy, 2 - ox));
        }
    }
}

// ------------------------------------------------------------- pipeline

fn level1_of(pixels: &[[f32; 3]], width: usize, height: usize) -> Level1 {
    Level1 {
        width,
        height,
        rgb: pixels
            .iter()
            .flat_map(|p| p.map(|v| (v * 65535.0 + 0.5) as u16))
            .collect(),
    }
}

#[test]
fn render_develops_neutral_patches_to_expected_values() {
    let color = camera_color(&z8().xyz_to_cam).unwrap();
    let transfer = Transfer::srgb();
    // Camera-native greys need the daylight multipliers undone to be
    // neutral in camera space, so build the patches as balanced greys
    // divided by the multipliers.
    let wb = color.daylight;
    let grey = |v: f32| [v / wb[0], v / wb[1], v / wb[2]];
    let level1 = level1_of(&[grey(0.0), grey(MIDDLE_GREY), grey(1.0), grey(0.5)], 2, 2);
    let image = develop::render(
        &level1,
        None,
        2,
        1,
        wb,
        &color,
        &transfer,
        &Params {
            exposure: 0.0,
            threads: 2,
            look: None,
        },
    )
    .unwrap();
    assert_eq!((image.width, image.height), (2, 2));
    let black = image.pixel(0, 0).unwrap();
    let mid = image.pixel(1, 0).unwrap();
    let white = image.pixel(0, 1).unwrap();
    let half = image.pixel(1, 1).unwrap();
    assert_eq!(black, [0, 0, 0]);
    assert_eq!(white, [255, 255, 255]);
    for channel in mid {
        assert!((116..=120).contains(&channel), "{mid:?}");
    }
    for channel in half {
        assert!((186..=189).contains(&channel), "{half:?}");
    }
    // One stop up doubles the light: middle grey lands near 163.
    let brighter = develop::render(
        &level1,
        None,
        2,
        1,
        wb,
        &color,
        &transfer,
        &Params {
            exposure: 1.0,
            threads: 1,
            look: None,
        },
    )
    .unwrap();
    for channel in brighter.pixel(1, 0).unwrap() {
        assert!((160..=166).contains(&channel), "{:?}", brighter.pixel(1, 0));
    }
    // Clipped camera channels stay neutral: a patch past the white level in
    // red alone is still white after the clip.
    let hot = level1_of(&[[1.0, 1.0, 1.0]], 1, 1);
    let out = develop::render(
        &hot,
        None,
        1,
        1,
        [2.0, 1.0, 1.5],
        &color,
        &transfer,
        &Params {
            exposure: 0.0,
            threads: 1,
            look: None,
        },
    )
    .unwrap();
    assert_eq!(out.pixel(0, 0), Some([255, 255, 255]));
}

#[test]
fn resample_u16_matches_the_converted_f32_path() {
    // Reading the u16 level as f32/65535 during the resample gives the same
    // values, in the same order, as converting the whole level first, so
    // the window's direct path and the old whole-frame path agree exactly.
    let src: Vec<u16> = (0..7 * 5 * 3).map(|i| (i * 811 % 65536) as u16).collect();
    let converted: Vec<f32> = src.iter().map(|v| f32::from(*v) / 65535.0).collect();
    for (dw, dh) in [(3, 2), (7, 5), (11, 9), (1, 1)] {
        let direct = resample_u16(&src, 7, 5, dw, dh, 4).unwrap();
        let via_f32 = resample(&converted, 7, 5, dw, dh, 4).unwrap();
        assert_eq!(direct, via_f32, "{dw}x{dh}");
    }
    assert_eq!(
        resample_u16(&[0u16; 5], 7, 5, 3, 2, 1).unwrap_err(),
        develop::Error::Size
    );
}

#[test]
fn the_levels_split_composes_to_render() {
    let color = camera_color(&z8().xyz_to_cam).unwrap();
    let transfer = Transfer::srgb();
    let wb = color.daylight;
    let grey = |v: f32| [v / wb[0], v / wb[1], v / wb[2]];
    // A non-square frame: level 2 fits the long edge, then a quarter turn
    // swaps the axes to what render ends on.
    let mut pixels = Vec::new();
    for y in 0..4 {
        for x in 0..6 {
            pixels.push(grey(
                if x < 3 { 0.25 } else { 0.75 } * (1.0 - y as f32 * 0.1),
            ));
        }
    }
    let level1 = level1_of(&pixels, 6, 4);
    let flat = level2(&level1, None, 3, 1, 3).unwrap();
    assert_eq!((flat.width, flat.height), (3, 2));
    let turned: Level2 = level2(&level1, None, 3, 6, 3).unwrap();
    assert_eq!((turned.width, turned.height), (2, 3));
    // The new ordering (orient in level 2, then the per-pixel tail) equals
    // the old ordering (the tail on a held unturned level 2, then orient the
    // u8 output) for every orientation, exposure and look. The two sides run
    // different orient instantiations, f32 and u8, so this is no tautology,
    // and it holds render against a level 3 rerun off one held level 2 (the
    // memoization an exposure or look edit relies on).
    let look = Look::parse(b"td-photo look 1\nsaturation 1.5\n").unwrap();
    let held = level2(&level1, None, 3, 1, 3).unwrap();
    for look_opt in [None, Some(&look)] {
        for orientation in [1u16, 3, 6, 8] {
            for exposure in [-1.0, 0.0, 0.4] {
                let params = Params {
                    exposure,
                    threads: 3,
                    look: look_opt,
                };
                let new = develop::render(
                    &level1,
                    None,
                    3,
                    orientation,
                    wb,
                    &color,
                    &transfer,
                    &params,
                )
                .unwrap();
                let old = orient(
                    level3(&held, wb, &color, &transfer, &params).unwrap(),
                    orientation,
                );
                assert_eq!(
                    new,
                    old,
                    "orientation {orientation} exposure {exposure} look {}",
                    look_opt.is_some()
                );
            }
        }
    }
    // Level 2 and level 3 refuse a buffer that is not its axes.
    assert_eq!(
        level2(
            &Level1 {
                width: 6,
                height: 4,
                rgb: vec![0; 5],
            },
            None,
            3,
            1,
            1
        )
        .unwrap_err(),
        develop::Error::Size
    );
    let params = Params {
        exposure: 0.0,
        threads: 1,
        look: None,
    };
    assert_eq!(
        level3(
            &Level2 {
                width: 3,
                height: 2,
                rgb: vec![0.0; 5],
            },
            wb,
            &color,
            &transfer,
            &params
        )
        .unwrap_err(),
        develop::Error::Size
    );
}

#[test]
fn a_crop_selects_the_oriented_region_for_every_orientation() {
    // The crop is fractions of the *oriented* image; level 2 maps them back
    // through the inverse of the orientation to the un-oriented level 1. With
    // a long edge past the frame nothing resamples, so developing the crop
    // must equal the same rectangle cut from a develop of the whole frame:
    // this pins source_rect's per-orientation arithmetic against the orient
    // it inverts. The frame varies on both axes, so a wrong arm shows.
    let color = camera_color(&z8().xyz_to_cam).unwrap();
    let transfer = Transfer::srgb();
    let wb = color.daylight;
    let grey = |v: f32| [v / wb[0], v / wb[1], v / wb[2]];
    let (w, h) = (8usize, 6usize);
    let mut pixels = Vec::new();
    for y in 0..h {
        for x in 0..w {
            pixels.push(grey(0.1 + 0.02 * x as f32 + 0.13 * y as f32));
        }
    }
    let level1 = level1_of(&pixels, w, h);
    let params = Params {
        exposure: 0.0,
        threads: 3,
        look: None,
    };
    // A long edge past the frame, so fit never shrinks and the compare is
    // exact; the same fractions land on pixel boundaries at either orientation.
    let long_edge = 100;
    let fractions = [0.25f32, 0.5, 0.5, 0.5];
    for orientation in [1u16, 3, 6, 8] {
        // The oriented axes: swapped from level 1 for a quarter turn.
        let (ow, oh) = if matches!(orientation, 6 | 8) {
            (h, w)
        } else {
            (w, h)
        };
        let px = |f: f32, dim: usize| (f * dim as f32).round() as usize;
        let (ox, oy) = (px(fractions[0], ow), px(fractions[1], oh));
        let (ocw, och) = (px(fractions[2], ow), px(fractions[3], oh));
        let full = develop::render(
            &level1,
            None,
            long_edge,
            orientation,
            wb,
            &color,
            &transfer,
            &params,
        )
        .unwrap();
        assert_eq!((full.width, full.height), (ow, oh));
        let cropped = develop::render(
            &level1,
            Some(fractions),
            long_edge,
            orientation,
            wb,
            &color,
            &transfer,
            &params,
        )
        .unwrap();
        assert_eq!(
            (cropped.width, cropped.height),
            (ocw, och),
            "orientation {orientation} crop size"
        );
        // The same rectangle cut out of the whole-frame develop.
        let mut want = Vec::with_capacity(ocw * och * 3);
        for row in oy..oy + och {
            let start = (row * ow + ox) * 3;
            want.extend_from_slice(&full.data[start..start + ocw * 3]);
        }
        assert_eq!(cropped.data, want, "orientation {orientation} crop pixels");
    }
}

#[test]
fn a_degenerate_or_edge_crop_is_refused_not_panicked() {
    // A crop whose origin sits at the far edge, or whose extent rounds to
    // nothing, or a fraction past the unit square, is no crop: Error::Crop on
    // either axis and at every orientation, never a panic. (A boundary the
    // sidecar's own grammar can reach on a small oriented axis, and any input
    // through the public develop API.)
    let color = camera_color(&z8().xyz_to_cam).unwrap();
    let transfer = Transfer::srgb();
    let wb = color.daylight;
    let level1 = level1_of(&[[0.5, 0.5, 0.5]; 24], 6, 4);
    let params = Params {
        exposure: 0.0,
        threads: 1,
        look: None,
    };
    let refused = |crop: [f32; 4], orientation: u16| {
        develop::render(
            &level1,
            Some(crop),
            100,
            orientation,
            wb,
            &color,
            &transfer,
            &params,
        )
        .unwrap_err()
    };
    for orientation in [1u16, 3, 6, 8] {
        for crop in [
            [1.0, 0.0, 0.5, 0.5], // origin at the far edge in x
            [0.0, 1.0, 0.5, 0.5], // origin at the far edge in y
            [0.0, 0.0, 0.0, 0.5], // a zero-width request
            [0.0, 0.0, 0.5, 0.0], // a zero-height request
            [2.0, 0.0, 0.5, 0.5], // a fraction past the unit square
        ] {
            assert_eq!(
                refused(crop, orientation),
                develop::Error::Crop,
                "orientation {orientation} crop {crop:?}"
            );
        }
    }
}

#[test]
fn orient_leaves_a_zero_axis_or_odd_orientation_alone() {
    // A zero axis, or an orientation that is not 3, 6 or 8, is returned as
    // it came, never a panic: the generic orient core guards a zero axis for
    // any caller, above what its two callers already reject.
    for orientation in [0u16, 1, 2, 3, 6, 8, 9] {
        let empty = Rgb8 {
            width: 0,
            height: 0,
            data: Vec::new(),
        };
        assert_eq!(
            orient(empty, orientation),
            Rgb8 {
                width: 0,
                height: 0,
                data: Vec::new(),
            }
        );
    }
    let img = Rgb8 {
        width: 2,
        height: 1,
        data: vec![1, 2, 3, 4, 5, 6],
    };
    let same = Rgb8 {
        width: 2,
        height: 1,
        data: vec![1, 2, 3, 4, 5, 6],
    };
    assert_eq!(orient(img, 1), same);
}

#[test]
fn render_reduces_orients_and_refuses_bad_buffers() {
    let color = camera_color(&z8().xyz_to_cam).unwrap();
    let transfer = Transfer::srgb();
    let mut pixels = Vec::new();
    for y in 0..4 {
        for x in 0..6 {
            let v = if x < 3 { 0.0 } else { 1.0 } * (1.0 - y as f32 * 0.1);
            pixels.push([v / color.daylight[0], v, v / color.daylight[2]]);
        }
    }
    let level1 = level1_of(&pixels, 6, 4);
    let params = Params {
        exposure: 0.0,
        threads: 3,
        look: None,
    };
    let small = develop::render(
        &level1,
        None,
        3,
        1,
        color.daylight,
        &color,
        &transfer,
        &params,
    )
    .unwrap();
    assert_eq!((small.width, small.height), (3, 2));
    let turned = develop::render(
        &level1,
        None,
        3,
        6,
        color.daylight,
        &color,
        &transfer,
        &params,
    )
    .unwrap();
    assert_eq!((turned.width, turned.height), (2, 3));
    let bad = Level1 {
        width: 6,
        height: 4,
        rgb: vec![0; 5],
    };
    assert_eq!(
        develop::render(&bad, None, 3, 1, color.daylight, &color, &transfer, &params).unwrap_err(),
        develop::Error::Size
    );
}

#[test]
fn ppm_writer_emits_the_binary_header() {
    let image = Rgb8 {
        width: 2,
        height: 1,
        data: vec![255, 0, 0, 0, 0, 255],
    };
    let mut out = Vec::new();
    td_photo::image::write_ppm(&image, &mut out).unwrap();
    assert_eq!(&out[..11], b"P6\n2 1\n255\n");
    assert_eq!(&out[11..], &[255, 0, 0, 0, 0, 255]);
    let broken = Rgb8 {
        width: 2,
        height: 2,
        data: vec![0; 3],
    };
    assert!(td_photo::image::write_ppm(&broken, &mut Vec::new()).is_err());
    assert!(Rgb8::new(usize::MAX, 2).is_none());
    assert_eq!(Rgb8::new(2, 2).unwrap().data.len(), 12);
}

#[test]
fn image_buffers_are_bounded_and_checked() {
    use td_photo::image::MAX_AXIS;
    assert!(Rgb8::new(0, 1).is_none());
    assert!(Rgb8::new(1, 0).is_none());
    assert!(Rgb8::new(MAX_AXIS + 1, 1).is_none());
    assert!(Rgb8::new(1, MAX_AXIS + 1).is_none());
    // Within the axis ceiling but past the pixel budget.
    assert!(Rgb8::new(MAX_AXIS, MAX_AXIS).is_none());
    assert!(Rgb8::new(MAX_AXIS, 4097).is_none());
    assert!(!Rgb8 {
        width: MAX_AXIS,
        height: 4097,
        data: vec![],
    }
    .is_consistent());
    let edge = Rgb8::new(MAX_AXIS, 1).unwrap();
    assert!(edge.is_consistent());
    assert_eq!(edge.data.len(), MAX_AXIS * 3);
    assert_eq!(edge.pixel(MAX_AXIS - 1, 0), Some([0, 0, 0]));
    assert_eq!(edge.pixel(MAX_AXIS, 0), None);
    assert_eq!(edge.pixel(0, 1), None);
    let image = Rgb8 {
        width: 2,
        height: 2,
        data: (0..12).collect(),
    };
    assert_eq!(image.pixel(1, 1), Some([9, 10, 11]));
    assert_eq!(image.pixel(2, 0), None);
    for broken in [
        Rgb8 {
            width: 0,
            height: 2,
            data: vec![],
        },
        Rgb8 {
            width: MAX_AXIS + 1,
            height: 1,
            data: vec![],
        },
        Rgb8 {
            width: 2,
            height: 2,
            data: vec![0; 11],
        },
        Rgb8 {
            width: usize::MAX,
            height: usize::MAX,
            data: vec![],
        },
    ] {
        assert!(!broken.is_consistent());
        assert_eq!(broken.pixel(1, 1), None);
        assert!(td_photo::image::write_ppm(&broken, &mut Vec::new()).is_err());
        assert_eq!(orient(broken.clone(), 6), broken);
    }
}

#[test]
fn resampler_and_demosaic_refuse_oversize_and_overflowing_axes() {
    let src = [0.5f32; 12];
    for (sw, sh, dw, dh) in [
        (usize::MAX, 2, 1, 1),
        (2, usize::MAX, 1, 1),
        (2, 2, 0, 1),
        (2, 2, 1, 0),
        (2, 2, 16385, 1),
        (2, 2, 1, 16385),
        (16385, 1, 1, 1),
        (3, 2, 1, 1),
    ] {
        assert_eq!(
            resample(&src, sw, sh, dw, dh, 1).unwrap_err(),
            develop::Error::Size,
            "{sw}x{sh} -> {dw}x{dh}"
        );
    }
    assert_eq!(resample(&src, 2, 2, 1, 1, 1).unwrap(), vec![0.5; 3]);
    // Within the axis ceiling but past the pixel budget, for the
    // destination and for the middle buffer (destination width by source
    // height): refused before anything that size is allocated.
    assert_eq!(
        resample(&[0.0; 3], 1, 1, 16384, 16384, 1).unwrap_err(),
        develop::Error::Size
    );
    assert_eq!(
        resample(&[0.0; 3], 1, 1, 16384, 4097, 1).unwrap_err(),
        develop::Error::Size
    );
    let tall = vec![0.0f32; 3 * 8192];
    assert_eq!(
        resample(&tall, 1, 8192, 16384, 1, 1).unwrap_err(),
        develop::Error::Size
    );
    // A hand-built frame past the raw axis ceiling is refused even when its
    // buffer is consistent (the sample-count ceiling would need a 128 Mi
    // buffer to drive and is not exercised here).
    let wide = frame(32770, 2, vec![0; 65540]);
    let full = Crop {
        left: 0,
        top: 0,
        width: 32770,
        height: 2,
    };
    assert_eq!(
        superpixel(&wide, Cfa::RGGB, full, 0, 65535, 1).unwrap_err(),
        develop::Error::Size
    );
    let crop = Crop {
        left: 0,
        top: 0,
        width: 2,
        height: 2,
    };
    let overflowing = frame(usize::MAX, 2, vec![]);
    assert_eq!(
        superpixel(&overflowing, Cfa::RGGB, crop, 0, 65535, 1).unwrap_err(),
        develop::Error::Size
    );
    let short = frame(4, 2, vec![0; 7]);
    assert_eq!(
        superpixel(&short, Cfa::RGGB, crop, 0, 65535, 1).unwrap_err(),
        develop::Error::Size
    );
    let color = camera_color(&z8().xyz_to_cam).unwrap();
    let transfer = Transfer::srgb();
    let params = Params {
        exposure: 0.0,
        threads: 1,
        look: None,
    };
    for (width, height) in [(usize::MAX, 1), (1, usize::MAX), (16385, 1), (0, 4)] {
        let bad = Level1 {
            width,
            height,
            rgb: vec![],
        };
        assert_eq!(
            develop::render(&bad, None, 3, 1, color.daylight, &color, &transfer, &params)
                .unwrap_err(),
            develop::Error::Size
        );
    }
}

#[test]
fn work_is_the_same_on_one_thread_and_many() {
    let (w, h) = (70, 46);
    let mut seed = 99u64;
    let samples: Vec<u16> = (0..w * h)
        .map(|_| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((seed >> 40) & 0x3fff) as u16
        })
        .collect();
    let decoded = frame(w, h, samples);
    let crop = Crop {
        left: 3,
        top: 2,
        width: 62,
        height: 41,
    };
    let one = superpixel(&decoded, Cfa::RGGB, crop, 1008, 15892, 1).unwrap();
    assert_eq!((one.width, one.height), (31, 20));
    for threads in [0, 2, 7, 16, 99] {
        let many = superpixel(&decoded, Cfa::RGGB, crop, 1008, 15892, threads).unwrap();
        assert_eq!(many, one, "{threads} threads");
    }
    let linear: Vec<f32> = one.rgb.iter().map(|v| f32::from(*v) / 65535.0).collect();
    let small = resample(&linear, 31, 20, 13, 7, 1).unwrap();
    let large = resample(&linear, 31, 20, 47, 29, 1).unwrap();
    for threads in [0, 3, 16, 99] {
        assert_eq!(resample(&linear, 31, 20, 13, 7, threads).unwrap(), small);
        assert_eq!(resample(&linear, 31, 20, 47, 29, threads).unwrap(), large);
    }
    let color = camera_color(&z8().xyz_to_cam).unwrap();
    let transfer = Transfer::srgb();
    let wb = [1.99, 1.0, 1.55];
    let render = |threads: usize| {
        let params = Params {
            exposure: 0.7,
            threads,
            look: None,
        };
        develop::render(&one, None, 17, 8, wb, &color, &transfer, &params).unwrap()
    };
    let single = render(1);
    assert_eq!((single.width, single.height), (11, 17));
    for threads in [0, 4, 16, 99] {
        assert_eq!(render(threads), single, "{threads} threads");
    }
}

#[test]
fn fit_clamps_hostile_axes_before_multiplying() {
    use td_photo::image::MAX_AXIS;
    assert_eq!(
        fit(usize::MAX, usize::MAX, usize::MAX),
        (MAX_AXIS, MAX_AXIS)
    );
    assert_eq!(fit(usize::MAX, 1, usize::MAX), (MAX_AXIS, 1));
    assert_eq!(fit(usize::MAX, usize::MAX, 2), (2, 2));
    assert_eq!(fit(1, usize::MAX, 3), (1, 3));
    assert_eq!(fit(0, 0, 100), (1, 1));
    assert_eq!(fit(3000, 2000, 1500), (1500, 1000));
}

// ------------------------------------------------------------------ export

fn full(w: usize, h: usize) -> Crop {
    Crop {
        left: 0,
        top: 0,
        width: w,
        height: h,
    }
}

fn region(left: usize, top: usize, width: usize, height: usize) -> Region {
    Region {
        left,
        top,
        width,
        height,
    }
}

/// A frame whose photosites hold a value by channel, so every demosaiced
/// pixel is the same triple wherever it is.
fn by_channel(w: usize, h: usize, cfa: Cfa, values: [u16; 3]) -> Decoded {
    let mut samples = Vec::with_capacity(w * h);
    for y in 0..h {
        for x in 0..w {
            samples.push(values[cfa.at(x, y) as usize]);
        }
    }
    frame(w, h, samples)
}

#[test]
fn bilinear_keeps_the_sample_and_averages_each_other_channel() {
    // Constant per channel: every pixel, edges included, is the triple.
    let decoded = by_channel(6, 4, Cfa::RGGB, [100, 200, 300]);
    let source = Source {
        decoded: &decoded,
        cfa: Cfa::RGGB,
        crop: full(6, 4),
        black: 0,
        white: 1000,
    };
    let level = bilinear(&source, region(0, 0, 6, 4), 3).unwrap();
    assert_eq!((level.width, level.height), (6, 4));
    let scaled = |v: u32| (v * 65535 / 1000) as u16;
    for px in level.rgb.as_chunks::<3>().0 {
        assert_eq!(*px, [scaled(100), scaled(200), scaled(300)]);
    }
    // One bright green site at (1, 0) over zeros: its neighbours take it
    // in their green with the rounded mean over the neighbours they have,
    // the edge pixels fewer of them.
    let mut samples = vec![0u16; 16];
    samples[1] = 1000;
    let decoded = frame(4, 4, samples);
    let source = Source {
        decoded: &decoded,
        crop: full(4, 4),
        ..source
    };
    let level = bilinear(&source, region(0, 0, 4, 4), 1).unwrap();
    let at = |x: usize, y: usize| -> [u16; 3] {
        let i = (y * 4 + x) * 3;
        [level.rgb[i], level.rgb[i + 1], level.rgb[i + 2]]
    };
    // (0,0) is red: greens at (1,0) and (0,1), so (1000 + 0 + 1) / 2.
    assert_eq!(at(0, 0), [0, scaled(500), 0]);
    // (2,0) is red: greens at (1,0), (3,0) and (2,1), so (1000 + 1) / 3.
    assert_eq!(at(2, 0), [0, scaled(333), 0]);
    // (1,0) itself keeps its green; its reds (0,0), (2,0) and blue (1,1)
    // are zero.
    assert_eq!(at(1, 0), [0, scaled(1000), 0]);
    // (1,1) is blue: greens at (0,1), (2,1), (1,0), (1,2): 1000 / 4.
    assert_eq!(at(1, 1), [0, scaled(250), 0]);
    // (0,1) is green: its reds are (0,0), (0,2), its blues (1,1): none of
    // them the bright site, and its own green is zero.
    assert_eq!(at(0, 1), [0, 0, 0]);
    // Two rows down nothing sees it.
    assert_eq!(at(1, 2), [0, 0, 0]);
    assert_eq!(at(3, 3), [0, 0, 0]);
}

#[test]
fn bilinear_of_a_region_is_that_window_of_the_whole_and_reads_the_crop_origin() {
    // A varying frame under an offset crop, so the region's window and the
    // absolute CFA phase both show.
    let (w, h) = (12usize, 10usize);
    let samples: Vec<u16> = (0..w * h)
        .map(|i| ((i as u32 * 7919 + 13) % 2000) as u16)
        .collect();
    let decoded = frame(w, h, samples);
    let crop = Crop {
        left: 1,
        top: 1,
        width: 10,
        height: 8,
    };
    let source = Source {
        decoded: &decoded,
        cfa: Cfa::RGGB,
        crop,
        black: 10,
        white: 2010,
    };
    let whole = bilinear(&source, region(0, 0, 10, 8), 4).unwrap();
    let window = bilinear(&source, region(2, 3, 5, 4), 1).unwrap();
    assert_eq!((window.width, window.height), (5, 4));
    for y in 0..4 {
        let from = ((y + 3) * 10 + 2) * 3;
        assert_eq!(
            &window.rgb[y * 15..(y + 1) * 15],
            &whole.rgb[from..from + 15],
            "row {y}"
        );
    }
    // The crop's own origin is a blue site of the RGGB grid (1, 1), so the
    // whole's first pixel keeps its sample as blue, not red: the phase is
    // the sensor's, not the crop's.
    let b = u32::from(decoded.samples[w + 1]) - 10;
    assert_eq!(whole.rgb[2], (b * 65535 / 2000) as u16);
    // The whole on one thread and on many is the same.
    let one = bilinear(&source, region(0, 0, 10, 8), 1).unwrap();
    assert_eq!(one, whole);
}

#[test]
fn bilinear_refuses_bad_regions_levels_and_buffers() {
    let decoded = by_channel(8, 8, Cfa::RGGB, [1, 2, 3]);
    let source = Source {
        decoded: &decoded,
        cfa: Cfa::RGGB,
        crop: full(8, 8),
        black: 0,
        white: 100,
    };
    for (bad, why) in [
        (region(0, 0, 9, 8), "past the right"),
        (region(0, 1, 8, 8), "past the bottom"),
        (region(0, 0, 0, 8), "no width"),
        (region(0, 0, 8, 0), "no height"),
        (region(usize::MAX, 0, 1, 1), "overflowing left"),
    ] {
        assert_eq!(
            bilinear(&source, bad, 1).unwrap_err(),
            develop::Error::Crop,
            "{why}"
        );
    }
    let levels = Source {
        black: 100,
        ..source
    };
    assert_eq!(
        bilinear(&levels, region(0, 0, 8, 8), 1).unwrap_err(),
        develop::Error::Levels
    );
    let short = frame(8, 8, vec![0; 63]);
    let short = Source {
        decoded: &short,
        ..source
    };
    assert_eq!(
        bilinear(&short, region(0, 0, 8, 8), 1).unwrap_err(),
        develop::Error::Size
    );
    let crop = Source {
        crop: full(9, 8),
        ..source
    };
    assert_eq!(
        bilinear(&crop, region(0, 0, 8, 8), 1).unwrap_err(),
        develop::Error::Crop
    );
}

#[test]
fn export_geometry_maps_the_crop_through_the_orientation() {
    let decoded = by_channel(8, 6, Cfa::RGGB, [1, 2, 3]);
    let source = Source {
        decoded: &decoded,
        cfa: Cfa::RGGB,
        crop: full(8, 6),
        black: 0,
        white: 100,
    };
    assert_eq!(
        export_geometry(&source, None, 1).unwrap(),
        Export {
            region: region(0, 0, 8, 6),
            width: 8,
            height: 6,
            orientation: 1,
        }
    );
    assert_eq!(
        export_geometry(&source, None, 6).unwrap(),
        Export {
            region: region(0, 0, 8, 6),
            width: 6,
            height: 8,
            orientation: 6,
        }
    );
    let fractions = [0.25f32, 0.5, 0.5, 0.5];
    // Upright: x 2, y 3, 4 by 3.
    assert_eq!(
        export_geometry(&source, Some(fractions), 1).unwrap(),
        Export {
            region: region(2, 3, 4, 3),
            width: 4,
            height: 3,
            orientation: 1,
        }
    );
    // A quarter turn clockwise: the oriented axes are 6 by 8, so the crop
    // is x 2, y 4, 3 by 4 there, and lands at (4, 1), 4 by 3 in the sensor,
    // as level 2 maps it; the output is the crop's oriented 3 by 4.
    assert_eq!(
        export_geometry(&source, Some(fractions), 6).unwrap(),
        Export {
            region: region(4, 1, 4, 3),
            width: 3,
            height: 4,
            orientation: 6,
        }
    );
    // Half way round: the crop's far corner from the sensor's, 4 by 3 at
    // (2, 0).
    assert_eq!(
        export_geometry(&source, Some(fractions), 3).unwrap(),
        Export {
            region: region(2, 0, 4, 3),
            width: 4,
            height: 3,
            orientation: 3,
        }
    );
    // A quarter turn anticlockwise: the oriented crop x 2, y 4, 3 by 4
    // lands at (0, 2), 4 by 3.
    assert_eq!(
        export_geometry(&source, Some(fractions), 8).unwrap(),
        Export {
            region: region(0, 2, 4, 3),
            width: 3,
            height: 4,
            orientation: 8,
        }
    );
    assert_eq!(
        export_geometry(&source, Some([1.0, 0.0, 0.5, 0.5]), 1).unwrap_err(),
        develop::Error::Crop
    );
}

#[test]
fn export_bands_concatenate_to_the_whole_at_every_orientation() {
    // A frame varying on both axes, so a band taken from the wrong place or
    // turned the wrong way shows; the whole in one band is the oracle for
    // every band size, and the upright export turned by `orient` is the
    // oracle for each orientation, since the per-pixel pipeline commutes
    // with the turn.
    let (w, h) = (14usize, 10usize);
    let samples: Vec<u16> = (0..w * h)
        .map(|i| {
            let (x, y) = (i % w, i / w);
            1100 + 40 * x as u16 + 60 * y as u16 + ((i * 31) % 17) as u16
        })
        .collect();
    let decoded = frame(w, h, samples);
    let source = Source {
        decoded: &decoded,
        cfa: Cfa::RGGB,
        crop: Crop {
            left: 1,
            top: 1,
            width: 12,
            height: 8,
        },
        black: 1008,
        white: 15892,
    };
    let color = camera_color(&z8().xyz_to_cam).unwrap();
    let transfer = Transfer::srgb();
    let params = Params {
        exposure: 2.5,
        threads: 3,
        look: None,
    };
    let whole = |crop: Option<[f32; 4]>, orientation: u16| -> Rgb8 {
        let export = export_geometry(&source, crop, orientation).unwrap();
        export_band(
            &source,
            &export,
            0,
            export.height,
            color.daylight,
            &color,
            &transfer,
            &params,
        )
        .unwrap()
    };
    for crop in [None, Some([0.25f32, 0.125, 0.5, 0.75])] {
        let upright = whole(crop, 1);
        for orientation in [1u16, 3, 6, 8] {
            let export = export_geometry(&source, crop, orientation).unwrap();
            let expected = whole(crop, orientation);
            assert_eq!(
                (expected.width, expected.height),
                (export.width, export.height)
            );
            // Against the turned upright export, when the crop is turned
            // with it: the same region of the sensor either way only for
            // no crop, since a crop is fractions of the oriented image.
            if crop.is_none() {
                assert_eq!(
                    orient(upright.clone(), orientation),
                    expected,
                    "{orientation}"
                );
            }
            for rows in [1usize, 3, 8, 64] {
                let mut joined = Rgb8::new(export.width, export.height).unwrap();
                joined.data.clear();
                let mut first = 0;
                while first < export.height {
                    let band = export_band(
                        &source,
                        &export,
                        first,
                        rows,
                        color.daylight,
                        &color,
                        &transfer,
                        &params,
                    )
                    .unwrap();
                    assert_eq!(band.width, export.width);
                    assert_eq!(band.height, rows.min(export.height - first));
                    joined.data.extend_from_slice(&band.data);
                    first += band.height;
                }
                assert_eq!(joined, expected, "orientation {orientation} rows {rows}");
            }
            // Past the end, and no rows, are refused.
            assert_eq!(
                export_band(
                    &source,
                    &export,
                    export.height,
                    1,
                    color.daylight,
                    &color,
                    &transfer,
                    &params
                )
                .unwrap_err(),
                develop::Error::Size
            );
            assert_eq!(
                export_band(
                    &source,
                    &export,
                    0,
                    0,
                    color.daylight,
                    &color,
                    &transfer,
                    &params
                )
                .unwrap_err(),
                develop::Error::Size
            );
        }
    }
    // An export whose axes disagree with its region, or whose region
    // leaves the crop, is refused before a band is selected: the fields
    // are public.
    let good = export_geometry(&source, None, 6).unwrap();
    let band = |export: &Export| {
        export_band(
            &source,
            export,
            0,
            2,
            color.daylight,
            &color,
            &transfer,
            &params,
        )
    };
    assert!(band(&good).is_ok());
    let mut lying = good;
    lying.height += 1;
    assert_eq!(band(&lying).unwrap_err(), develop::Error::Size);
    let mut swapped = good;
    swapped.orientation = 1;
    assert_eq!(band(&swapped).unwrap_err(), develop::Error::Size);
    let mut outside = good;
    outside.region.left = 9;
    assert_eq!(band(&outside).unwrap_err(), develop::Error::Crop);
    // The export is not flat: the ramp shows across it.
    let upright = whole(None, 1);
    assert!(upright.data.iter().any(|&v| v > 0));
    assert_ne!(upright.pixel(0, 0), upright.pixel(11, 7));
}

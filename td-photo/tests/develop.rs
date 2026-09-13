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
use td_photo::develop::{self, fit, orient, resample, superpixel, Level1, Params};
use td_photo::image::Rgb8;
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
        2,
        1,
        wb,
        &color,
        &transfer,
        &Params {
            exposure: 0.0,
            threads: 2,
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
        2,
        1,
        wb,
        &color,
        &transfer,
        &Params {
            exposure: 1.0,
            threads: 1,
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
        1,
        1,
        [2.0, 1.0, 1.5],
        &color,
        &transfer,
        &Params {
            exposure: 0.0,
            threads: 1,
        },
    )
    .unwrap();
    assert_eq!(out.pixel(0, 0), Some([255, 255, 255]));
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
    };
    let small = develop::render(&level1, 3, 1, color.daylight, &color, &transfer, &params).unwrap();
    assert_eq!((small.width, small.height), (3, 2));
    let turned =
        develop::render(&level1, 3, 6, color.daylight, &color, &transfer, &params).unwrap();
    assert_eq!((turned.width, turned.height), (2, 3));
    let bad = Level1 {
        width: 6,
        height: 4,
        rgb: vec![0; 5],
    };
    assert_eq!(
        develop::render(&bad, 3, 1, color.daylight, &color, &transfer, &params).unwrap_err(),
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
    };
    for (width, height) in [(usize::MAX, 1), (1, usize::MAX), (16385, 1), (0, 4)] {
        let bad = Level1 {
            width,
            height,
            rgb: vec![],
        };
        assert_eq!(
            develop::render(&bad, 3, 1, color.daylight, &color, &transfer, &params).unwrap_err(),
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
        };
        develop::render(&one, 17, 8, wb, &color, &transfer, &params).unwrap()
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

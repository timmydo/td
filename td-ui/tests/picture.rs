#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

//! Pictures: their bounds, the size they are fitted to, the area average
//! over premultiplied samples, and the raster's blend of a fitted one.

use td_ui::picture::{Error, Fitted, Picture, MAX_PICTURE_AXIS, MAX_PICTURE_PIXELS};
use td_ui::raster::{Raster, Rect, Scale, Surface};

fn solid(width: usize, height: usize, rgba: [u8; 4]) -> Picture {
    Picture::new(width, height, rgba.repeat(width * height)).unwrap()
}

#[test]
fn a_picture_is_bounded_and_its_pixels_are_its_size() {
    assert_eq!(Picture::new(0, 1, vec![]).unwrap_err(), Error::Size);
    assert_eq!(Picture::new(1, 0, vec![]).unwrap_err(), Error::Size);
    assert_eq!(
        Picture::new(MAX_PICTURE_AXIS + 1, 1, vec![0; (MAX_PICTURE_AXIS + 1) * 4]).unwrap_err(),
        Error::Size
    );
    // Within the axes, past the pixel budget: refused before the length.
    assert_eq!(
        Picture::new(
            MAX_PICTURE_AXIS,
            MAX_PICTURE_PIXELS / MAX_PICTURE_AXIS + 1,
            vec![]
        )
        .unwrap_err(),
        Error::Size
    );
    assert_eq!(Picture::new(2, 2, vec![0; 15]).unwrap_err(), Error::Length);
    let picture = solid(3, 2, [1, 2, 3, 4]);
    assert_eq!(
        (picture.width(), picture.height(), picture.bytes()),
        (3, 2, 24)
    );
}

#[test]
fn a_fit_keeps_the_aspect_and_never_enlarges() {
    let picture = solid(400, 100, [0; 4]);
    assert_eq!(picture.fit_size(1000, 1000), (400, 100), "fits as it is");
    assert_eq!(picture.fit_size(200, 1000), (200, 50), "width-bound");
    assert_eq!(picture.fit_size(1000, 20), (80, 20), "height-bound");
    assert_eq!(picture.fit_size(0, 0), (1, 1), "an axis is at least one");
    let tall = solid(1, 1000, [0; 4]);
    assert_eq!(tall.fit_size(100, 10), (1, 10));
}

#[test]
fn a_fit_averages_premultiplied_samples_into_the_rasters_order() {
    // Red opaque, blue opaque, a transparent green that adds no colour,
    // and white at half alpha.
    let rgba = [
        255, 0, 0, 255, 0, 0, 255, 255, //
        0, 255, 0, 0, 255, 255, 255, 128,
    ];
    let picture = Picture::new(2, 2, rgba.to_vec()).unwrap();
    let whole = picture.fit(2, 2).unwrap();
    // Blue, green, red, alpha; premultiplied, so the transparent green is
    // black and the half white is half grey.
    assert_eq!(whole.pixel(0, 0), Some([0, 0, 255, 255]));
    assert_eq!(whole.pixel(1, 0), Some([255, 0, 0, 255]));
    assert_eq!(whole.pixel(0, 1), Some([0, 0, 0, 0]));
    assert_eq!(whole.pixel(1, 1), Some([128, 128, 128, 128]));
    assert_eq!(whole.pixel(2, 0), None);
    let one = picture.fit(1, 1).unwrap();
    // Sums over four: blue 255 + 128, green 128, red 255 + 128; alpha
    // 255 + 255 + 0 + 128.
    assert_eq!(one.pixel(0, 0), Some([96, 32, 96, 160]));
    // A larger ask is its own size.
    assert_eq!(
        (
            picture.fit(9, 9).unwrap().width,
            picture.fit(9, 9).unwrap().height
        ),
        (2, 2)
    );
}

#[test]
fn a_fractional_shrink_weights_each_pixel_by_its_overlap() {
    // Black, white, black into two: each output pixel a pixel and a half
    // of black and half a pixel of white.
    let rgba = [0, 0, 0, 255, 255, 255, 255, 255, 0, 0, 0, 255].to_vec();
    let fitted = Picture::new(3, 1, rgba.clone()).unwrap().fit(2, 1).unwrap();
    assert_eq!(fitted.pixel(0, 0), Some([85, 85, 85, 255]));
    assert_eq!(fitted.pixel(1, 0), Some([85, 85, 85, 255]));
    // The same down a column.
    let fitted = Picture::new(1, 3, rgba).unwrap().fit(1, 2).unwrap();
    assert_eq!(fitted.pixel(0, 0), Some([85, 85, 85, 255]));
    assert_eq!(fitted.pixel(0, 1), Some([85, 85, 85, 255]));
}

#[test]
fn shrinking_a_flat_picture_keeps_its_colour() {
    let picture = solid(37, 23, [10, 200, 30, 255]);
    let fitted = picture.fit(5, 3).unwrap();
    assert_eq!((fitted.width, fitted.height), (5, 3));
    assert!(fitted
        .bgra
        .as_chunks::<4>()
        .0
        .iter()
        .all(|p| *p == [30, 200, 10, 255]));
}

#[test]
fn the_raster_blends_a_fitted_picture_over_its_buffer_inside_the_clip() {
    let font = td_ui::font::pinned().unwrap();
    let surface = Surface::new(16, 16, Scale::new(1).unwrap()).unwrap();
    let mut buffer = vec![0u8; 16 * 16 * 4];
    // A grey ground: blue, green, red 100.
    for pixel in buffer.as_chunks_mut::<4>().0 {
        pixel.copy_from_slice(&[100, 100, 100, 255]);
    }
    let fitted = Fitted {
        width: 4,
        height: 2,
        // Opaque red then half-alpha premultiplied white, by row.
        bgra: [[0, 0, 255, 255]; 4]
            .into_iter()
            .chain([[128, 128, 128, 128]; 4])
            .flatten()
            .collect(),
    };
    let rect = Rect {
        x: 2,
        y: 3,
        width: 4,
        height: 2,
    };
    // The clip leaves the first column out.
    let clip = Rect {
        x: 3,
        y: 0,
        width: 16,
        height: 16,
    };
    Raster::new(&mut buffer, &font, surface, 16 * 4)
        .unwrap()
        .image(clip, rect, &fitted);
    let at = |x: usize, y: usize| buffer[(y * 16 + x) * 4..(y * 16 + x) * 4 + 4].to_vec();
    assert_eq!(at(2, 3), vec![100, 100, 100, 255], "clipped");
    assert_eq!(at(3, 3), vec![0, 0, 255, 255], "opaque");
    // 128 + 100 * 127 / 255, rounded.
    assert_eq!(at(3, 4), vec![178, 178, 178, 255], "blended");
    assert_eq!(at(6, 3), vec![100, 100, 100, 255], "past the picture");
    // A rectangle larger than the picture draws only the picture.
    let mut wide = vec![0u8; 16 * 16 * 4];
    Raster::new(&mut wide, &font, surface, 16 * 4)
        .unwrap()
        .image(
            surface.bounds(),
            Rect {
                width: 16,
                height: 16,
                ..rect
            },
            &fitted,
        );
    let lit = wide
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|p| p[3] == 255)
        .count();
    assert_eq!(lit, 8);
}

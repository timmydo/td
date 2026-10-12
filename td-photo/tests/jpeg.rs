#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

//! td-photo's JPEG: the thumbnail rule over td-jpeg's decoder, the PPM
//! cache format, and td-jpeg's encoder over the export's pool (`Bands`).
//! The decoder's and the encoder's own contracts are td-jpeg's
//! (`td-jpeg/tests`).

use std::path::Path;

use td_jpeg::{self as decoder, Scale};
use td_photo::image::Rgb8;
use td_photo::jpeg;

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// td-jpeg's Z 8 thumbnail fixture, which the thumbnail rule decodes.
fn thumb() -> Vec<u8> {
    std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../td-jpeg/tests/fixtures/z8-thumb.jpg"),
    )
    .unwrap()
}

#[test]
fn the_thumbnail_rule_picks_the_scale_and_resamples_to_the_edge() {
    let data = thumb();
    // An edge the full frame does not exceed decodes at full scale, as is.
    let full = jpeg::thumbnail(&data, 400, 1).unwrap();
    assert_eq!((full.width, full.height), (160, 120));
    assert_eq!(full.data, decoder::decode(&data, Scale::Full).unwrap().data);
    assert_eq!(fnv1a64(&full.data), 0x15cf1df123ee575d);
    // An edge that a coarser scale meets exactly is that scale, untouched.
    let quarter = jpeg::thumbnail(&data, 40, 1).unwrap();
    assert_eq!((quarter.width, quarter.height), (40, 30));
    assert_eq!(
        quarter.data,
        decoder::decode(&data, Scale::Quarter).unwrap().data
    );
    assert_eq!(fnv1a64(&quarter.data), 0x7c9a6f6b601504ab);
    // An edge between scales decodes at the coarsest covering scale and is
    // area-resampled down to exactly the edge, the same on any thread count.
    let fifty = jpeg::thumbnail(&data, 50, 1).unwrap();
    assert_eq!((fifty.width, fifty.height), (50, 38));
    assert_eq!(jpeg::thumbnail(&data, 50, 4).unwrap().data, fifty.data);
    let small = jpeg::thumbnail(&data, 16, 1).unwrap();
    assert_eq!((small.width, small.height), (16, 12));
    // The rule is decode-then-resample, so the half decode is the source.
    let half = decoder::decode(&data, Scale::Half).unwrap();
    let mean = |img: &Rgb8| {
        img.data.iter().map(|v| u64::from(*v)).sum::<u64>() as f64 / img.data.len() as f64
    };
    assert!((mean(&half) - mean(&fifty)).abs() < 2.0);
    // A refusal from the stream is the stream's.
    assert_eq!(
        jpeg::thumbnail(b"P6", 40, 1).unwrap_err(),
        jpeg::Error::NotJpeg
    );
}

#[test]
fn ppm_round_trips_through_read_and_write_only_in_the_written_shape() {
    use td_photo::image::{read_ppm, write_ppm};
    let image = decoder::decode(&thumb(), Scale::Eighth).unwrap();
    let mut bytes = Vec::new();
    write_ppm(&image, &mut bytes).unwrap();
    assert!(bytes.starts_with(b"P6\n20 15\n255\n"));
    let back = read_ppm(&bytes).unwrap();
    assert_eq!((back.width, back.height), (20, 15));
    assert_eq!(back.data, image.data);
    // Anything else is a miss, never a panic or a partial image.
    assert!(read_ppm(b"").is_none());
    assert!(read_ppm(b"P6").is_none());
    assert!(read_ppm(b"P5\n20 15\n255\n").is_none());
    assert!(read_ppm(b"P6\n20 15\n255\n").is_none(), "short payload");
    let mut long = bytes.clone();
    long.push(0);
    assert!(read_ppm(&long).is_none(), "long payload");
    let mut short = bytes.clone();
    short.pop();
    assert!(read_ppm(&short).is_none());
    assert!(read_ppm(b"P6\n20  15\n255\n").is_none(), "double space");
    assert!(read_ppm(b"P6\n# c\n20 15\n255\n").is_none(), "comment");
    assert!(read_ppm(b"P6\n20 15\n65535\n").is_none(), "wide samples");
    assert!(read_ppm(b"P6\n0 15\n255\n").is_none(), "zero axis");
    assert!(read_ppm(b"P6\n-20 15\n255\n").is_none());
    assert!(read_ppm(b"P6\n20 15 3\n255\n").is_none());
    // Sizes past the ceilings are refused before any allocation.
    assert!(read_ppm(b"P6\n16385 1\n255\n").is_none());
    assert!(read_ppm(b"P6\n16384 16384\n255\n").is_none());
    assert!(read_ppm(b"P6\n99999999999999999999 1\n255\n").is_none());
}

// ------------------------------------------------------------------ encoder

fn ramp(width: usize, height: usize) -> Rgb8 {
    let mut image = Rgb8::new(width, height).unwrap();
    for y in 0..height {
        for x in 0..width {
            let i = (y * width + x) * 3;
            image.data[i] = (x * 255 / width.max(2).saturating_sub(1).max(1)).min(255) as u8;
            image.data[i + 1] = (y * 255 / height.max(2).saturating_sub(1).max(1)).min(255) as u8;
            image.data[i + 2] = ((x + y) * 127 / (width + height)) as u8;
        }
    }
    image
}

#[test]
fn the_encoder_streams_bands_identically_on_any_thread_count() {
    // Held to the stream td-jpeg codes on the caller's thread alone.
    let image = ramp(50, 45);
    let mut alone = decoder::Encoder::new(50, 45, jpeg::QUALITY).unwrap();
    alone.encode_rows(&image.data, &decoder::InOrder).unwrap();
    let whole = alone.finish(&decoder::InOrder).unwrap();
    for (rows, threads) in [(1usize, 1usize), (7, 4), (8, 2), (64, 16), (45, 3)] {
        let bands = jpeg::Bands(threads);
        let mut encoder = decoder::Encoder::new(50, 45, jpeg::QUALITY).unwrap();
        let mut bytes = encoder.take();
        assert!(bytes.starts_with(&[0xff, 0xd8]));
        for band in image.data.chunks(rows * 50 * 3) {
            encoder.encode_rows(band, &bands).unwrap();
            bytes.extend(encoder.take());
        }
        bytes.extend(encoder.finish(&bands).unwrap());
        assert_eq!(bytes, whole, "rows {rows} threads {threads}");
    }
}

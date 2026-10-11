#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

//! td-photo's JPEG: the thumbnail rule over td-jpeg's decoder, the PPM
//! cache format, and the export encoder round-tripping through the
//! decoder. The decoder's own contract is td-jpeg's (`td-jpeg/tests`).

use std::path::Path;

use td_jpeg::{self as decoder, Scale};
use td_photo::image::{Rgb8, MAX_AXIS};
use td_photo::jpeg;

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn lcg(seed: &mut u64) -> u32 {
    *seed = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    (*seed >> 33) as u32
}

fn assert_close(decoded: &Rgb8, expected: &Rgb8, tolerance: i32, what: &str) {
    assert_eq!(
        (decoded.width, decoded.height),
        (expected.width, expected.height),
        "{what}"
    );
    let mut worst = 0;
    for (i, (a, b)) in decoded.data.iter().zip(expected.data.iter()).enumerate() {
        let d = (i32::from(*a) - i32::from(*b)).abs();
        assert!(d <= tolerance, "{what}: byte {i} decoded {a} expected {b}");
        worst = worst.max(d);
    }
    assert!(worst <= tolerance);
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

fn noise(width: usize, height: usize, seed: u64) -> Rgb8 {
    let mut image = Rgb8::new(width, height).unwrap();
    let mut s = seed;
    for v in image.data.iter_mut() {
        *v = (lcg(&mut s) >> 8) as u8;
    }
    image
}

fn encode(image: &Rgb8, quality: u8, threads: usize) -> Vec<u8> {
    let mut encoder = jpeg::Encoder::new(image.width, image.height, quality, threads).unwrap();
    encoder.encode_rows(&image.data).unwrap();
    encoder.finish().unwrap()
}

#[test]
fn the_encoder_round_trips_through_the_decoder() {
    for (w, h) in [
        (1usize, 1usize),
        (8, 8),
        (9, 17),
        (37, 29),
        (64, 16),
        (100, 3),
    ] {
        let image = ramp(w, h);
        let bytes = encode(&image, jpeg::QUALITY, 3);
        let head = decoder::header(&bytes).unwrap();
        assert_eq!((head.width, head.height), (w, h));
        let decoded = decoder::decode(&bytes, Scale::Full).unwrap();
        assert_close(&decoded, &image, 12, &format!("ramp {w}x{h}"));
        // At the top quality the tables are all ones and the round trip is
        // within the colour transform's rounding, noise included.
        let image = noise(w, h, 0x5eed + w as u64);
        let bytes = encode(&image, 100, 2);
        let decoded = decoder::decode(&bytes, Scale::Full).unwrap();
        assert_close(&decoded, &image, 4, &format!("noise {w}x{h}"));
    }
    // It compresses: a smooth frame at the export quality is a small
    // fraction of its raw bytes.
    let image = ramp(256, 256);
    let bytes = encode(&image, jpeg::QUALITY, 4);
    assert!(bytes.len() * 8 < image.data.len(), "{} bytes", bytes.len());
    assert!(bytes.starts_with(&[0xff, 0xd8, 0xff, 0xe0]) && bytes.ends_with(&[0xff, 0xd9]));
    // A grey frame stays exactly grey: its chroma is the level and codes
    // to nothing.
    let mut grey = ramp(24, 24);
    for px in grey.data.as_chunks_mut::<3>().0 {
        *px = [px[0]; 3];
    }
    let decoded = decoder::decode(&encode(&grey, jpeg::QUALITY, 1), Scale::Full).unwrap();
    assert!(decoded
        .data
        .as_chunks::<3>()
        .0
        .iter()
        .all(|p| p[0] == p[1] && p[1] == p[2]));
    assert_close(&decoded, &grey, 4, "grey");
    // A black frame at quality 100: every luma DC is -1024 at a quantiser
    // of 1, the largest baseline DC category (11) through the coder and
    // the decoder both, and the frame decodes to exactly black. (The
    // clamp itself is pinned by `forward`'s unit test; the transform lands
    // on -1024 here without it.)
    let black = Rgb8::new(16, 16).unwrap();
    let decoded = decoder::decode(&encode(&black, 100, 1), Scale::Full).unwrap();
    assert_eq!(decoded, black);
}

#[test]
fn the_encoder_streams_bands_identically_on_any_thread_count() {
    let image = ramp(50, 45);
    let whole = encode(&image, jpeg::QUALITY, 1);
    for (rows, threads) in [(1usize, 1usize), (7, 4), (8, 2), (64, 16), (45, 3)] {
        let mut encoder = jpeg::Encoder::new(50, 45, jpeg::QUALITY, threads).unwrap();
        let mut bytes = encoder.take();
        assert!(bytes.starts_with(&[0xff, 0xd8]));
        for band in image.data.chunks(rows * 50 * 3) {
            encoder.encode_rows(band).unwrap();
            bytes.extend(encoder.take());
        }
        bytes.extend(encoder.finish().unwrap());
        assert_eq!(bytes, whole, "rows {rows} threads {threads}");
    }
}

#[test]
fn the_encoder_refuses_bad_axes_and_rows() {
    let axis = |w: usize, h: usize| jpeg::Encoder::new(w, h, 92, 1).err().unwrap();
    assert!(matches!(axis(0, 8), jpeg::EncodeError::Axis { .. }));
    assert!(matches!(axis(8, 0), jpeg::EncodeError::Axis { .. }));
    assert!(matches!(
        axis(MAX_AXIS + 1, 8),
        jpeg::EncodeError::Axis { .. }
    ));
    assert!(matches!(
        axis(8, MAX_AXIS + 1),
        jpeg::EncodeError::Axis { .. }
    ));
    assert!(jpeg::Encoder::new(MAX_AXIS, 1, 92, 1).is_ok());
    let mut encoder = jpeg::Encoder::new(8, 4, 92, 1).unwrap();
    assert_eq!(
        encoder.encode_rows(&[0; 23]).unwrap_err(),
        jpeg::EncodeError::Rows
    );
    assert_eq!(
        encoder.encode_rows(&[0; 8 * 3 * 5]).unwrap_err(),
        jpeg::EncodeError::Rows
    );
    encoder.encode_rows(&[0; 8 * 3 * 3]).unwrap();
    assert_eq!(
        encoder.encode_rows(&[0; 8 * 3 * 2]).unwrap_err(),
        jpeg::EncodeError::Rows
    );
    // Short of the height, it will not finish.
    assert_eq!(encoder.finish().unwrap_err(), jpeg::EncodeError::Rows);
    let mut encoder = jpeg::Encoder::new(8, 4, 92, 1).unwrap();
    encoder.encode_rows(&[0; 8 * 3 * 4]).unwrap();
    assert!(encoder.finish().is_ok());
    assert_eq!(jpeg::QUALITY, 92);
}

#[test]
fn the_encoder_writes_the_standard_headers() {
    // The segments before the scan, by their literal bytes: what a decoder
    // that is not this crate's reads first.
    let bytes = encode(&ramp(9, 5), jpeg::QUALITY, 1);
    let mut at = 2;
    let mut segments = Vec::new();
    while at < bytes.len() {
        assert_eq!(bytes[at], 0xff);
        let marker = bytes[at + 1];
        let len = usize::from(u16::from_be_bytes([bytes[at + 2], bytes[at + 3]]));
        segments.push((marker, bytes[at + 4..at + 2 + len].to_vec()));
        at += 2 + len;
        if marker == 0xda {
            break;
        }
    }
    let markers: Vec<u8> = segments.iter().map(|(m, _)| *m).collect();
    assert_eq!(markers, [0xe0, 0xdb, 0xc0, 0xc4, 0xda]);
    assert_eq!(segments[0].1, b"JFIF\0\x01\x01\x00\x00\x01\x00\x01\x00\x00");
    // Two 8-bit quantisers in zigzag order: the luma's first entries are
    // Annex K's 16, 11, 12, 14, 12, 10, 16, 14 at 92 (scale 16), and the
    // chroma's last is 99 at 16.
    let dqt = &segments[1].1;
    assert_eq!(dqt.len(), 130);
    assert_eq!(dqt[0], 0);
    assert_eq!(&dqt[1..9], &[3, 2, 2, 2, 2, 2, 3, 2]);
    assert_eq!(dqt[65], 1);
    assert_eq!(dqt[66], 3);
    assert_eq!(dqt[129], 16);
    assert_eq!(
        segments[2].1,
        [8, 0, 5, 0, 9, 3, 1, 0x11, 0, 2, 0x11, 1, 3, 0x11, 1]
    );
    // Four Huffman tables with Annex K.3's counts.
    let dht = &segments[3].1;
    assert_eq!(dht.len(), 4 * 17 + 2 * 12 + 2 * 162);
    let mut at = 0;
    for (class_id, bits, values) in [
        (
            0x00u8,
            [0u8, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0],
            12usize,
        ),
        (
            0x10,
            [0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7d],
            162,
        ),
        (0x01, [0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0], 12),
        (
            0x11,
            [0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 0x77],
            162,
        ),
    ] {
        assert_eq!(dht[at], class_id);
        assert_eq!(&dht[at + 1..at + 17], &bits);
        at += 17 + values;
    }
    assert_eq!(at, dht.len());
    assert_eq!(segments[4].1, [3, 1, 0x00, 2, 0x11, 3, 0x11, 0, 63, 0]);
}

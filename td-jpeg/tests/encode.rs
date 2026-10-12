#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

//! The baseline encoder: round trips through the decoder, the same stream
//! however its pieces run, its refusals, and the headers by their bytes.

use std::sync::Mutex;

use td_jpeg::{decode, header, EncodeError, Encoder, InOrder, Rgb8, Runner, Scale, MAX_AXIS};

/// The quality the round trips are held at, td-photo's export's.
const QUALITY: u8 = 92;

/// The pieces taken from one queue by `threads` threads, as a caller's
/// pool would run them.
struct Pool(usize);

impl Runner for Pool {
    fn run<T: Send>(&self, items: Vec<T>, work: &(dyn Fn(T) + Sync)) {
        let queue = Mutex::new(items);
        let next = || queue.lock().unwrap().pop();
        std::thread::scope(|scope| {
            for _ in 1..self.0 {
                scope.spawn(|| {
                    while let Some(item) = next() {
                        work(item);
                    }
                });
            }
            while let Some(item) = next() {
                work(item);
            }
        });
    }
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
    let mut encoder = Encoder::new(image.width, image.height, quality).unwrap();
    if threads == 1 {
        encoder.encode_rows(&image.data, &InOrder).unwrap();
        encoder.finish(&InOrder).unwrap()
    } else {
        let pool = Pool(threads);
        encoder.encode_rows(&image.data, &pool).unwrap();
        encoder.finish(&pool).unwrap()
    }
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
        let bytes = encode(&image, QUALITY, 3);
        let head = header(&bytes).unwrap();
        assert_eq!((head.width, head.height), (w, h));
        let decoded = decode(&bytes, Scale::Full).unwrap();
        assert_close(&decoded, &image, 12, &format!("ramp {w}x{h}"));
        // At the top quality the tables are all ones and the round trip is
        // within the colour transform's rounding, noise included.
        let image = noise(w, h, 0x5eed + w as u64);
        let bytes = encode(&image, 100, 2);
        let decoded = decode(&bytes, Scale::Full).unwrap();
        assert_close(&decoded, &image, 4, &format!("noise {w}x{h}"));
    }
    // It compresses: a smooth frame at the export quality is a small
    // fraction of its raw bytes.
    let image = ramp(256, 256);
    let bytes = encode(&image, QUALITY, 4);
    assert!(bytes.len() * 8 < image.data.len(), "{} bytes", bytes.len());
    assert!(bytes.starts_with(&[0xff, 0xd8, 0xff, 0xe0]) && bytes.ends_with(&[0xff, 0xd9]));
    // A grey frame stays exactly grey: its chroma is the level and codes
    // to nothing.
    let mut grey = ramp(24, 24);
    for px in grey.data.as_chunks_mut::<3>().0 {
        *px = [px[0]; 3];
    }
    let decoded = decode(&encode(&grey, QUALITY, 1), Scale::Full).unwrap();
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
    let decoded = decode(&encode(&black, 100, 1), Scale::Full).unwrap();
    assert_eq!(decoded, black);
}

#[test]
fn the_encoder_streams_bands_identically_on_any_thread_count() {
    let image = ramp(50, 45);
    let whole = encode(&image, QUALITY, 1);
    for (rows, threads) in [(1usize, 1usize), (7, 4), (8, 2), (64, 16), (45, 3)] {
        let pool = Pool(threads);
        let mut encoder = Encoder::new(50, 45, QUALITY).unwrap();
        let mut bytes = encoder.take();
        assert!(bytes.starts_with(&[0xff, 0xd8]));
        for band in image.data.chunks(rows * 50 * 3) {
            encoder.encode_rows(band, &pool).unwrap();
            bytes.extend(encoder.take());
        }
        bytes.extend(encoder.finish(&pool).unwrap());
        assert_eq!(bytes, whole, "rows {rows} threads {threads}");
    }
}

#[test]
fn the_encoder_refuses_bad_axes_and_rows() {
    let axis = |w: usize, h: usize| Encoder::new(w, h, 92).err().unwrap();
    assert!(matches!(axis(0, 8), EncodeError::Axis { .. }));
    assert!(matches!(axis(8, 0), EncodeError::Axis { .. }));
    assert!(matches!(axis(MAX_AXIS + 1, 8), EncodeError::Axis { .. }));
    assert!(matches!(axis(8, MAX_AXIS + 1), EncodeError::Axis { .. }));
    assert!(Encoder::new(MAX_AXIS, 1, 92).is_ok());
    let mut encoder = Encoder::new(8, 4, 92).unwrap();
    assert_eq!(
        encoder.encode_rows(&[0; 23], &InOrder).unwrap_err(),
        EncodeError::Rows
    );
    assert_eq!(
        encoder.encode_rows(&[0; 8 * 3 * 5], &InOrder).unwrap_err(),
        EncodeError::Rows
    );
    encoder.encode_rows(&[0; 8 * 3 * 3], &InOrder).unwrap();
    assert_eq!(
        encoder.encode_rows(&[0; 8 * 3 * 2], &InOrder).unwrap_err(),
        EncodeError::Rows
    );
    // Short of the height, it will not finish.
    assert_eq!(encoder.finish(&InOrder).unwrap_err(), EncodeError::Rows);
    let mut encoder = Encoder::new(8, 4, 92).unwrap();
    encoder.encode_rows(&[0; 8 * 3 * 4], &InOrder).unwrap();
    assert!(encoder.finish(&InOrder).is_ok());
}

#[test]
fn the_encoder_writes_the_standard_headers() {
    // The segments before the scan, by their literal bytes: what a decoder
    // that is not this crate's reads first.
    let bytes = encode(&ramp(9, 5), QUALITY, 1);
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

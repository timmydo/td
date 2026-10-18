#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

//! `encode_rgb8` held to the decoder: every stream it writes is one the
//! strict reader accepts (CRCs, zlib header, Adler-32, exact length,
//! filter bytes) and gives back the same pixels, opaque.

fn noise(len: usize, seed: u32) -> Vec<u8> {
    let mut x = seed;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            (x & 0xff) as u8
        })
        .collect()
}

fn roundtrip(width: usize, height: usize, rgb: &[u8]) -> Vec<u8> {
    let png = td_png::encode_rgb8(width, height, rgb).unwrap();
    let header = td_png::header(&png).unwrap();
    assert_eq!((header.width, header.height), (width, height));
    assert_eq!(header.depth, 8);
    assert_eq!(header.colour, td_png::Colour::Rgb);
    assert!(!header.interlaced);
    let image = td_png::decode(&png).unwrap();
    assert_eq!((image.width, image.height), (width, height));
    let rgba: Vec<u8> = rgb
        .as_chunks::<3>()
        .0
        .iter()
        .flat_map(|&[r, g, b]| [r, g, b, 255])
        .collect();
    assert_eq!(image.data, rgba, "{width}x{height}");
    png
}

/// Each chunk's type and length, in stream order.
fn chunks(png: &[u8]) -> Vec<([u8; 4], usize)> {
    let mut out = Vec::new();
    let mut at = 8;
    while at < png.len() {
        let len = u32::from_be_bytes(png[at..at + 4].try_into().unwrap()) as usize;
        let kind: [u8; 4] = png[at + 4..at + 8].try_into().unwrap();
        out.push((kind, len));
        at += 12 + len;
    }
    out
}

#[test]
fn pictures_of_every_shape_come_back_exactly() {
    roundtrip(1, 1, &[1, 2, 3]);
    roundtrip(3, 1, &noise(9, 7));
    roundtrip(1, 5, &noise(15, 9));
    roundtrip(17, 13, &noise(17 * 13 * 3, 11));
    // A gradient (Sub and Up predict it), a checkerboard (Paeth and
    // Average), flat fields and noise, in one frame.
    let (w, h) = (257, 131);
    let mut rgb = Vec::with_capacity(w * h * 3);
    let grain = noise(w * h, 3);
    for y in 0..h {
        for x in 0..w {
            let px = match y / 33 {
                0 => [x as u8, y as u8, (x + y) as u8],
                1 => {
                    let on = (x / 4 + y / 4) % 2 == 0;
                    [if on { 200 } else { 30 }, 90, if on { 10 } else { 250 }]
                }
                2 => [0x2e, 0x34, 0x40],
                _ => {
                    let g = grain[y * w + x];
                    [g, g.rotate_left(3), !g]
                }
            };
            rgb.extend_from_slice(&px);
        }
    }
    roundtrip(w, h, &rgb);
}

#[test]
fn flat_screens_compress_and_noise_spills_into_several_idats() {
    // A desktop: one colour with a few bars of another.
    let (w, h) = (1024, 768);
    let mut rgb = vec![0u8; w * h * 3];
    for (i, px) in rgb.as_chunks_mut::<3>().0.iter_mut().enumerate() {
        let (x, y) = (i % w, i / w);
        let c = if y < 24 || (300..340).contains(&x) {
            [0xee, 0xee, 0xec]
        } else {
            [0x1e, 0x1e, 0x2e]
        };
        *px = c;
    }
    let png = roundtrip(w, h, &rgb);
    assert!(png.len() < rgb.len() / 50, "{} bytes", png.len());

    // Incompressible pixels fall back to stored blocks: past a mebibyte
    // of image data, so the stream is cut into consecutive IDATs.
    let (w, h) = (700, 700);
    let png = roundtrip(w, h, &noise(w * h * 3, 5));
    let kinds = chunks(&png);
    assert_eq!(kinds.first().unwrap().0, *b"IHDR");
    assert_eq!(kinds.last().unwrap(), &(*b"IEND", 0));
    let idats: Vec<usize> = kinds
        .iter()
        .filter(|(k, _)| k == b"IDAT")
        .map(|(_, l)| *l)
        .collect();
    assert_eq!(idats.len(), 2);
    assert_eq!(idats[0], 1 << 20);
    assert_eq!(kinds.len(), 4);
}

/// One small stream byte for byte, as `png_ref.py` and Python's zlib read
/// it when it was pinned: a change to the encoder or td-util's DEFLATE
/// encoder changes every screenshot's bytes, and this says so.
#[test]
fn a_pinned_stream_is_reproduced_exactly() {
    let rgb: Vec<u8> = (0..4 * 3 * 3).map(|i| (i * 37 % 251) as u8).collect();
    let png = td_png::encode_rgb8(4, 3, &rgb).unwrap();
    assert_eq!(png, PINNED);
}

#[test]
fn the_same_pixels_give_the_same_bytes() {
    let rgb = noise(64 * 48 * 3, 13);
    assert_eq!(
        td_png::encode_rgb8(64, 48, &rgb).unwrap(),
        td_png::encode_rgb8(64, 48, &rgb).unwrap()
    );
}

#[test]
fn refusals_are_named() {
    assert_eq!(
        td_png::encode_rgb8(0, 4, &[]),
        Err(td_png::EncodeError::Axis {
            width: 0,
            height: 4
        })
    );
    assert_eq!(
        td_png::encode_rgb8(2, 1, &[0; 7]),
        Err(td_png::EncodeError::Length {
            expected: 6,
            actual: 7
        })
    );
    assert_eq!(
        td_png::encode_rgb8(2, 1, &[0; 7]).unwrap_err().to_string(),
        "expected 6 RGB bytes, got 7"
    );
}

/// `a_pinned_stream_is_reproduced_exactly`'s stream; png_ref.py read it as
/// 4x3 rgba 0xd9dea7668ed8b117.
const PINNED: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x03, 0x08, 0x02, 0x00, 0x00, 0x00, 0x3b, 0x96, 0x39,
    0x91, 0x00, 0x00, 0x00, 0x23, 0x49, 0x44, 0x41, 0x54, 0x78, 0x01, 0x63, 0x60, 0x50, 0xf5, 0xca,
    0x9f, 0xb2, 0xf3, 0x1e, 0x87, 0x6e, 0x50, 0xf9, 0x1c, 0xa6, 0x83, 0x07, 0x8f, 0x81, 0x00, 0x98,
    0x62, 0x82, 0xd2, 0x40, 0xea, 0xd8, 0x31, 0x00, 0x82, 0xf1, 0x17, 0x15, 0x1f, 0xb2, 0xf1, 0xa7,
    0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

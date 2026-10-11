#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

//! The decoder contract: every colour type and depth, plain and
//! interlaced, held to the hashes the independent oracle
//! (`tests/fixtures/png_ref.py`) printed; a libpng-written file held the
//! same way; every refusal DESIGN.md names, from streams built here; and
//! no prefix or corruption of a real stream panicking.

use std::path::Path;

use td_png::{decode, header, Colour, Error, Rgba8, MAX_AXIS, MAX_CHUNKS};

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name),
    )
    .unwrap()
}

/// What `png_ref.py` printed over the suite (`tests/fixtures/README.md`).
const SUITE: &[(&str, usize, usize, u64)] = &[
    ("g1-3x1-i", 3, 1, 0x0f63d359e34d1ec0),
    ("g16-i", 13, 11, 0xbaa7fd6ca336771a),
    ("g16", 13, 11, 0xf4db75743e072490),
    ("g16-t", 13, 11, 0xde55094d917863b0),
    ("g1-i", 13, 11, 0xacfb746e1e3f2f79),
    ("g1", 13, 11, 0x667e6ed4564e11d8),
    ("g2-i", 13, 11, 0xa6fb485718f799bb),
    ("g2", 13, 11, 0xd0a0ad74cc17ccbb),
    ("g2-t", 13, 11, 0xe9483b4cb09ef57c),
    ("g4-i", 13, 11, 0x6a476c8adcdac168),
    ("g4", 13, 11, 0x8173bdf7a63063a0),
    ("g8-i", 13, 11, 0x630a193d9f4376e6),
    ("g8", 13, 11, 0xb0ca025a21f51a1b),
    ("g8-t", 13, 11, 0x6e20a2c1a266ca3c),
    ("g8-tm", 13, 11, 0x4447438a8346ec19),
    ("ga16-i", 13, 11, 0x2d913132dae1811e),
    ("ga16", 13, 11, 0xc5cc40834584b65b),
    ("ga8-i", 13, 11, 0xc290906cebfb1206),
    ("ga8", 13, 11, 0x14dcd8533ed4db4f),
    ("p1-i", 13, 11, 0x45fa5bae96bff8bc),
    ("p1", 13, 11, 0x413909af0c7231c4),
    ("p2-i", 13, 11, 0xce85fac0b8ee5796),
    ("p2", 13, 11, 0x4f21043b95cc4193),
    ("p4-i", 13, 11, 0x571680601b041342),
    ("p4", 13, 11, 0x7669b6680d9d68a1),
    ("p4-t", 13, 11, 0x121112ee79dbfb5f),
    ("p8-i", 13, 11, 0xd0c91cc7016419c0),
    ("p8", 13, 11, 0x5bfdffb77d22e1d7),
    ("rgb16-i", 13, 11, 0x0931181b5b758a40),
    ("rgb16", 13, 11, 0x67c9e2a5e22d75fa),
    ("rgb16-t", 13, 11, 0x12d284de7419c52f),
    ("rgb8-i", 13, 11, 0x74a97c14abedfd28),
    ("rgb8", 13, 11, 0x8165ec73a575cc12),
    ("rgb8-t", 13, 11, 0xb202f15e1c2765c5),
    ("rgba16-i", 13, 11, 0xfe1925732d7e4c33),
    ("rgba16", 13, 11, 0x7d724b3cc444eeb2),
    ("rgba8-1x1-i", 1, 1, 0x6e80c2eb779cab86),
    ("rgba8-i", 13, 11, 0xd680ae4db097ed46),
    ("rgba8", 13, 11, 0x2f38a191ca7f7a0f),
];

#[test]
fn every_colour_type_and_depth_decodes_to_the_oracles_hash() {
    let names: Vec<String> =
        std::fs::read_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/suite"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
    assert_eq!(names.len(), SUITE.len(), "one hash per suite file");
    for &(name, width, height, hash) in SUITE {
        let data = fixture(&format!("suite/{name}.png"));
        let head = header(&data).unwrap();
        assert_eq!((head.width, head.height), (width, height), "{name}");
        let interlaced = name.ends_with("-i") || matches!(name, "g16-t" | "rgb16-t");
        assert_eq!(head.interlaced, interlaced, "{name}");
        let image = decode(&data).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!((image.width, image.height), (width, height), "{name}");
        assert_eq!(image.data.len(), width * height * 4, "{name}");
        assert_eq!(fnv1a64(&image.data), hash, "{name}");
    }
}

#[test]
fn transparency_keys_clear_exactly_their_pixels() {
    // The generator put the key at (0, 0), (5, 3) and (12, 10).
    for name in ["g8-t", "g8-tm", "g16-t", "rgb8-t", "rgb16-t"] {
        let image = decode(&fixture(&format!("suite/{name}.png"))).unwrap();
        for (x, y) in [(0, 0), (5, 3), (12, 10)] {
            assert_eq!(image.pixel(x, y).unwrap()[3], 0, "{name} ({x}, {y})");
        }
        if name.contains("16") {
            // The near miss beside the key: the same eight-bit value.
            assert_eq!(image.pixel(1, 0).unwrap()[3], 255, "{name} near miss");
        }
        let opaque = image
            .data
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[3] == 255)
            .count();
        let clear = image
            .data
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[3] == 0)
            .count();
        assert_eq!(opaque + clear, 13 * 11, "{name}: alpha is all or nothing");
    }
    // A palette's tRNS alphas, the entries past it opaque.
    let image = decode(&fixture("suite/p4-t.png")).unwrap();
    let alphas: std::collections::BTreeSet<u8> =
        image.data.as_chunks::<4>().0.iter().map(|p| p[3]).collect();
    assert!(alphas.iter().all(|a| [0, 64, 128, 192, 255, 7].contains(a)));
    assert!(alphas.contains(&255));
}

#[test]
fn a_libpng_file_decodes_to_the_oracles_hash() {
    let data = fixture("shapes.png");
    let head = header(&data).unwrap();
    assert_eq!(
        (
            head.width,
            head.height,
            head.depth,
            head.colour,
            head.interlaced
        ),
        (96, 64, 8, Colour::Rgba, false)
    );
    let image = decode(&data).unwrap();
    assert_eq!(fnv1a64(&image.data), 0xaf1c86b7bf39b47b);
    // Outside the rounded rectangle is clear; inside the gradient is not.
    assert_eq!(image.pixel(0, 0).unwrap()[3], 0);
    assert_eq!(image.pixel(48, 58).unwrap()[3], 255);
    assert_eq!(image.pixel(96, 0), None);
}

#[test]
fn the_pixel_accessor_checks_the_public_fields() {
    let image = Rgba8 {
        width: 2,
        height: 1,
        data: vec![1, 2, 3, 4, 5, 6, 7],
    };
    assert_eq!(image.pixel(0, 0), Some([1, 2, 3, 4]));
    assert_eq!(image.pixel(1, 0), None, "short data");
    let huge = Rgba8 {
        width: usize::MAX,
        height: usize::MAX,
        data: vec![],
    };
    assert_eq!(huge.pixel(usize::MAX - 1, usize::MAX - 1), None);
}

// ------------------------------------------------------------- the writer

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for b in bytes {
        crc ^= u32::from(*b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn adler32(bytes: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for x in bytes {
        a = (a + u32::from(*x)) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut out = (data.len() as u32).to_be_bytes().to_vec();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut summed = kind.to_vec();
    summed.extend_from_slice(data);
    out.extend_from_slice(&crc32(&summed).to_be_bytes());
    out
}

fn ihdr(width: u32, height: u32, depth: u8, colour: u8, interlace: u8) -> Vec<u8> {
    let mut data = width.to_be_bytes().to_vec();
    data.extend_from_slice(&height.to_be_bytes());
    data.extend_from_slice(&[depth, colour, 0, 0, interlace]);
    chunk(b"IHDR", &data)
}

/// `raw` as a zlib stream of stored blocks, its Adler-32 after.
fn zlib(raw: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    let mut blocks = raw.chunks(65535).peekable();
    if blocks.peek().is_none() {
        out.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
    }
    while let Some(block) = blocks.next() {
        out.push(u8::from(blocks.peek().is_none()));
        let len = block.len() as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(block);
    }
    out.extend_from_slice(&adler32(raw).to_be_bytes());
    out
}

fn png(parts: &[Vec<u8>]) -> Vec<u8> {
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    for part in parts {
        out.extend_from_slice(part);
    }
    out
}

/// A 2x2 eight-bit RGB frame, filter None on each row.
fn rgb_raw() -> Vec<u8> {
    vec![0, 1, 2, 3, 4, 5, 6, 0, 7, 8, 9, 10, 11, 12]
}

fn good() -> Vec<u8> {
    png(&[
        ihdr(2, 2, 8, 2, 0),
        chunk(b"IDAT", &zlib(&rgb_raw())),
        chunk(b"IEND", b""),
    ])
}

#[test]
fn the_writer_here_is_one_the_decoder_reads() {
    let image = decode(&good()).unwrap();
    assert_eq!(
        image.data,
        vec![1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255, 10, 11, 12, 255]
    );
    // What follows IEND is not read.
    let mut tail = good();
    tail.extend_from_slice(b"anything");
    assert_eq!(decode(&tail).unwrap(), image);
    // Unknown ancillary chunks are skipped wherever they are allowed.
    let skipped = png(&[
        ihdr(2, 2, 8, 2, 0),
        chunk(b"zzZz", b"x"),
        chunk(b"IDAT", &zlib(&rgb_raw())),
        chunk(b"tIME", b"1234567"),
        chunk(b"IEND", b""),
    ]);
    assert_eq!(decode(&skipped).unwrap(), image);
    // A suggested palette for an RGB image is read past.
    let suggested = png(&[
        ihdr(2, 2, 8, 2, 0),
        chunk(b"PLTE", &[1, 2, 3]),
        chunk(b"IDAT", &zlib(&rgb_raw())),
        chunk(b"IEND", b""),
    ]);
    assert_eq!(decode(&suggested).unwrap(), image);
    // Image data split anywhere over consecutive IDATs.
    let stream = zlib(&rgb_raw());
    let split = png(&[
        ihdr(2, 2, 8, 2, 0),
        chunk(b"IDAT", &stream[..1]),
        chunk(b"IDAT", &[]),
        chunk(b"IDAT", &stream[1..]),
        chunk(b"IEND", b""),
    ]);
    assert_eq!(decode(&split).unwrap(), image);
}

fn refused(data: &[u8]) -> Error {
    decode(data).unwrap_err()
}

#[test]
fn malformed_framing_is_refused_by_name() {
    assert_eq!(refused(b"GIF89a"), Error::NotPng);
    assert_eq!(header(b"\x89PNG\r\n\x1a"), Err(Error::NotPng));
    let whole = good();
    assert_eq!(
        refused(&whole[..whole.len() - 12]),
        Error::Truncated,
        "no IEND"
    );
    assert_eq!(refused(&whole[..20]), Error::Truncated);
    // A flipped byte in the image data fails that chunk's CRC.
    let mut flipped = whole.clone();
    let at = whole.windows(4).position(|w| w == b"IDAT").unwrap() + 6;
    flipped[at] ^= 1;
    assert_eq!(refused(&flipped), Error::Crc(*b"IDAT"));
    // A type that is not four letters, a length past 2^31 - 1.
    let digit = png(&[
        ihdr(2, 2, 8, 2, 0),
        chunk(b"ID4T", b""),
        chunk(b"IEND", b""),
    ]);
    assert_eq!(refused(&digit), Error::Chunk);
    let mut long = png(&[ihdr(2, 2, 8, 2, 0)]);
    long.extend_from_slice(&0x8000_0000u32.to_be_bytes());
    long.extend_from_slice(b"IDAT");
    assert_eq!(refused(&long), Error::Chunk);
    // Past the chunk budget.
    let mut many = vec![ihdr(2, 2, 8, 2, 0)];
    let note = chunk(b"tEXt", b"");
    many.extend(std::iter::repeat_n(note, MAX_CHUNKS));
    assert_eq!(refused(&png(&many)), Error::Chunks);
    // An unknown critical chunk.
    let critical = png(&[
        ihdr(2, 2, 8, 2, 0),
        chunk(b"ABCD", b""),
        chunk(b"IDAT", &zlib(&rgb_raw())),
        chunk(b"IEND", b""),
    ]);
    assert_eq!(refused(&critical), Error::Critical(*b"ABCD"));
}

#[test]
fn chunks_out_of_place_are_refused_by_name() {
    let idat = || chunk(b"IDAT", &zlib(&rgb_raw()));
    let iend = || chunk(b"IEND", b"");
    let first = png(&[chunk(b"tEXt", b""), ihdr(2, 2, 8, 2, 0), idat(), iend()]);
    assert_eq!(refused(&first), Error::Order(*b"tEXt"));
    let twice = png(&[ihdr(2, 2, 8, 2, 0), ihdr(2, 2, 8, 2, 0), idat(), iend()]);
    assert_eq!(refused(&twice), Error::Order(*b"IHDR"));
    let split = png(&[
        ihdr(2, 2, 8, 2, 0),
        idat(),
        chunk(b"tEXt", b""),
        idat(),
        iend(),
    ]);
    assert_eq!(refused(&split), Error::Order(*b"IDAT"));
    // An indexed image's palette after its data, or twice.
    let indexed = || chunk(b"IDAT", &zlib(&[0, 0, 0, 0, 0, 0]));
    let late_palette = png(&[
        ihdr(2, 2, 8, 3, 0),
        chunk(b"PLTE", &[0; 3]),
        indexed(),
        chunk(b"PLTE", &[0; 3]),
        iend(),
    ]);
    assert_eq!(refused(&late_palette), Error::Order(*b"PLTE"));
    let two_palettes = png(&[
        ihdr(2, 2, 8, 3, 0),
        chunk(b"PLTE", &[0; 3]),
        chunk(b"PLTE", &[0; 3]),
        indexed(),
        iend(),
    ]);
    assert_eq!(refused(&two_palettes), Error::Order(*b"PLTE"));
}

#[test]
fn malformed_chunks_and_formats_are_refused_by_name() {
    let idat = || chunk(b"IDAT", &zlib(&rgb_raw()));
    let iend = || chunk(b"IEND", b"");
    let short = png(&[chunk(b"IHDR", &[0; 12]), idat(), iend()]);
    assert_eq!(refused(&short), Error::Malformed(*b"IHDR"));
    let full_end = png(&[ihdr(2, 2, 8, 2, 0), idat(), chunk(b"IEND", b"x")]);
    assert_eq!(refused(&full_end), Error::Malformed(*b"IEND"));
    let ragged = png(&[ihdr(2, 2, 8, 3, 0), chunk(b"PLTE", &[0; 4]), idat(), iend()]);
    assert_eq!(refused(&ragged), Error::Malformed(*b"PLTE"));
    let empty = png(&[ihdr(2, 2, 8, 3, 0), chunk(b"PLTE", &[]), idat(), iend()]);
    assert_eq!(refused(&empty), Error::Malformed(*b"PLTE"));
    for (depth, colour, interlace) in [
        (3, 0, 0),
        (8, 1, 0),
        (16, 3, 0),
        (4, 2, 0),
        (1, 4, 0),
        (2, 6, 0),
        (8, 2, 2),
    ] {
        let bad = png(&[ihdr(2, 2, depth, colour, interlace), idat(), iend()]);
        assert_eq!(refused(&bad), Error::Format, "{depth} {colour} {interlace}");
    }
    // The compression and filter method bytes, each with its CRC redone.
    for at in [10, 11] {
        let mut data = ihdr(2, 2, 8, 2, 0)[8..21].to_vec();
        data[at] = 1;
        let bad = png(&[chunk(b"IHDR", &data), idat(), iend()]);
        assert_eq!(refused(&bad), Error::Format, "method byte {at}");
        assert_eq!(header(&bad), Err(Error::Format));
    }
}

#[test]
fn axes_and_budgets_are_refused_by_name() {
    let idat = || chunk(b"IDAT", &zlib(&rgb_raw()));
    let iend = || chunk(b"IEND", b"");
    let max = MAX_AXIS as u32;
    for (w, h) in [(0, 2), (2, 0), (max + 1, 1), (1, max + 1), (max, max)] {
        let bad = png(&[ihdr(w, h, 8, 2, 0), idat(), iend()]);
        let axis = Error::Axis {
            width: w as usize,
            height: h as usize,
        };
        assert_eq!(refused(&bad), axis);
        assert_eq!(header(&bad), Err(axis));
    }
    // A full-page screenshot's height, inside the pixel budget.
    let tall = png(&[ihdr(1280, 30000, 8, 2, 0), idat(), iend()]);
    assert!(header(&tall).is_ok());
    // Within the pixel budget, past the inflated one: 16-bit RGBA, which
    // the header refuses so a consumer can budget before decoding.
    let deep = png(&[ihdr(8192, 8192, 16, 6, 0), idat(), iend()]);
    let axis = Error::Axis {
        width: 8192,
        height: 8192,
    };
    assert_eq!(header(&deep), Err(axis.clone()));
    assert_eq!(refused(&deep), axis);
    // Compressed data past what its inflated size can need: a 1x1 grey
    // frame inflates to 2 bytes, so 4 + 64 KiB of data is the most.
    let small = |len: usize| png(&[ihdr(1, 1, 8, 0, 0), chunk(b"IDAT", &vec![0; len]), iend()]);
    assert_eq!(refused(&small(65541)), Error::Budget);
    assert_ne!(refused(&small(65540)), Error::Budget);
}

/// `raw` as one fixed-Huffman block of literals, the stream an encoder
/// without a stored fallback writes for rows it cannot compress: every
/// byte past 143 costs nine bits.
fn fixed_literals(raw: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    let (mut acc, mut bits) = (0u64, 0u32);
    let mut put = |out: &mut Vec<u8>, value: u64, count: u32| {
        acc |= value << bits;
        bits += count;
        while bits >= 8 {
            out.push(acc as u8);
            acc >>= 8;
            bits -= 8;
        }
    };
    // Huffman codes go most significant bit first.
    let code = |value: u32, length: u32| u64::from(value.reverse_bits() >> (32 - length));
    put(&mut out, 0b011, 3);
    for &x in raw {
        if x < 144 {
            put(&mut out, code(0x30 + u32::from(x), 8), 8);
        } else {
            put(&mut out, code(0x190 + u32::from(x) - 144, 9), 9);
        }
    }
    put(&mut out, 0, 7);
    put(&mut out, 0, 7);
    out.extend_from_slice(&adler32(raw).to_be_bytes());
    out
}

#[test]
fn a_stream_larger_than_its_rows_is_within_budget() {
    // Large enough that a budget of a 1024th plus 1 KiB is overrun.
    let (width, height) = (256usize, 256usize);
    let mut seed = 7u64;
    // Rows of filter None, then noise.
    let mut raw = vec![0u8; height * (1 + width * 3)];
    for row in raw.chunks_mut(1 + width * 3) {
        for byte in &mut row[1..] {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            *byte = (seed >> 33) as u8;
        }
    }
    let stream = fixed_literals(&raw);
    assert!(
        stream.len() > raw.len() + raw.len() / 32,
        "{}",
        stream.len()
    );
    let data = png(&[
        ihdr(width as u32, height as u32, 8, 2, 0),
        chunk(b"IDAT", &stream),
        chunk(b"IEND", b""),
    ]);
    let image = decode(&data).unwrap();
    assert_eq!(image.pixel(0, 0).unwrap()[..3], raw[1..4]);
}

#[test]
fn what_cannot_damage_the_pixels_is_read_as_libpng_reads_it() {
    let idat = || chunk(b"IDAT", &zlib(&rgb_raw()));
    let iend = || chunk(b"IEND", b"");
    let rgb = decode(&good()).unwrap();
    let with = |extra: Vec<Vec<u8>>, colour: u8| {
        let mut parts = vec![ihdr(2, 2, 8, colour, 0)];
        parts.extend(extra);
        parts.push(idat());
        parts.push(iend());
        decode(&png(&parts))
    };
    // An ancillary chunk with a bad CRC is skipped.
    let mut bad_text = chunk(b"tEXt", b"note");
    *bad_text.last_mut().unwrap() ^= 1;
    assert_eq!(with(vec![bad_text], 2).unwrap(), rgb);
    // A tRNS of the wrong length for RGB, in an image with alpha, or after
    // the data, is skipped; a second is skipped after the first.
    assert_eq!(with(vec![chunk(b"tRNS", &[0; 2])], 2).unwrap(), rgb);
    let late = png(&[
        ihdr(2, 2, 8, 2, 0),
        idat(),
        chunk(b"tRNS", &[0, 1, 0, 2, 0, 3]),
        iend(),
    ]);
    assert_eq!(decode(&late).unwrap(), rgb);
    let keyed = with(vec![chunk(b"tRNS", &[0, 1, 0, 2, 0, 3])], 2).unwrap();
    assert_eq!(keyed.pixel(0, 0), Some([1, 2, 3, 0]));
    let second = with(
        vec![
            chunk(b"tRNS", &[0, 1, 0, 2, 0, 3]),
            chunk(b"tRNS", &[0, 4, 0, 5, 0, 6]),
        ],
        2,
    )
    .unwrap();
    assert_eq!(second, keyed);
    // An RGBA frame's rows for the alpha case: 2x2, filter None.
    let rgba_raw = [0, 1, 2, 3, 4, 5, 6, 7, 8, 0, 9, 10, 11, 12, 13, 14, 15, 16];
    let rgba = |extra: Vec<Vec<u8>>| {
        let mut parts = vec![ihdr(2, 2, 8, 6, 0)];
        parts.extend(extra);
        parts.push(chunk(b"IDAT", &zlib(&rgba_raw)));
        parts.push(iend());
        decode(&png(&parts)).unwrap()
    };
    assert_eq!(rgba(vec![chunk(b"tRNS", &[0; 2])]), rgba(vec![]));
    // A palette in a grey image, and a suggested palette of any length in
    // an RGB one, are skipped.
    let grey_raw = [0, 7, 9, 0, 11, 13];
    let grey = |extra: Vec<Vec<u8>>| {
        let mut parts = vec![ihdr(2, 2, 8, 0, 0)];
        parts.extend(extra);
        parts.push(chunk(b"IDAT", &zlib(&grey_raw)));
        parts.push(iend());
        decode(&png(&parts)).unwrap()
    };
    assert_eq!(grey(vec![chunk(b"PLTE", &[0; 3])]), grey(vec![]));
    assert_eq!(with(vec![chunk(b"PLTE", &[0; 3 * 257])], 2).unwrap(), rgb);
    assert_eq!(with(vec![chunk(b"PLTE", &[0; 4])], 2).unwrap(), rgb);
    // An indexed image: a palette longer than its depth is cut to it; a
    // tRNS that is empty, longer than the palette, or before it, is
    // skipped.
    let indexed = |extra: Vec<Vec<u8>>| {
        let mut parts = vec![ihdr(2, 2, 1, 3, 0)];
        parts.extend(extra);
        // Rows of indices 0 1 and 1 0, packed into the high bits.
        parts.push(chunk(b"IDAT", &zlib(&[0, 0b0100_0000, 0, 0b1000_0000])));
        parts.push(iend());
        decode(&png(&parts))
    };
    let mut entries: Vec<u8> = (0..=255u8).flat_map(|i| [i, i, i]).collect();
    let cut = indexed(vec![chunk(b"PLTE", &entries)]).unwrap();
    assert_eq!(cut.pixel(1, 0), Some([1, 1, 1, 255]));
    entries.truncate(6);
    let plain = indexed(vec![chunk(b"PLTE", &entries)]).unwrap();
    assert_eq!(plain, cut);
    for trns in [vec![], vec![9; 3]] {
        let skipped = indexed(vec![chunk(b"PLTE", &entries), chunk(b"tRNS", &trns)]);
        assert_eq!(skipped.unwrap(), plain, "{trns:?}");
    }
    let early = indexed(vec![chunk(b"tRNS", &[9]), chunk(b"PLTE", &entries)]);
    assert_eq!(early.unwrap(), plain);
    let alpha = indexed(vec![chunk(b"PLTE", &entries), chunk(b"tRNS", &[9])]).unwrap();
    assert_eq!(alpha.pixel(0, 0), Some([0, 0, 0, 9]));
    assert_eq!(alpha.pixel(1, 0), Some([1, 1, 1, 255]));
}

#[test]
fn image_data_problems_are_refused_by_name() {
    let iend = || chunk(b"IEND", b"");
    let with = |stream: &[u8]| png(&[ihdr(2, 2, 8, 2, 0), chunk(b"IDAT", stream), iend()]);
    assert_eq!(refused(&png(&[ihdr(2, 2, 8, 2, 0), iend()])), Error::NoData);
    let palette_first = png(&[ihdr(2, 2, 8, 3, 0), chunk(b"IDAT", &zlib(&[0; 6])), iend()]);
    assert_eq!(refused(&palette_first), Error::Palette);
    // Index 1 of a one-entry palette.
    let past = png(&[
        ihdr(2, 2, 8, 3, 0),
        chunk(b"PLTE", &[9, 9, 9]),
        chunk(b"IDAT", &zlib(&[0, 0, 0, 0, 0, 1])),
        iend(),
    ]);
    assert_eq!(refused(&past), Error::Palette);
    // The zlib header: not DEFLATE, too wide a window, a bad check, a
    // preset dictionary.
    let stream = zlib(&rgb_raw());
    for head in [[0x77, 0x01], [0x88, 0x1c], [0x78, 0x02], [0x78, 0x20]] {
        let mut bad = stream.clone();
        bad[..2].copy_from_slice(&head);
        assert_eq!(refused(&with(&bad)), Error::Zlib, "{head:x?}");
    }
    // A reserved block type.
    assert!(matches!(
        refused(&with(&[0x78, 0x01, 0x07, 0, 0, 0, 0])),
        Error::Inflate(_)
    ));
    // Short of the size, past it, and bytes after the Adler-32.
    let mut short_raw = rgb_raw();
    short_raw.pop();
    assert_eq!(refused(&with(&zlib(&short_raw))), Error::Length);
    let mut long_raw = rgb_raw();
    long_raw.push(0);
    assert_eq!(refused(&with(&zlib(&long_raw))), Error::Length);
    let mut trailing = stream.clone();
    trailing.push(0);
    assert_eq!(refused(&with(&trailing)), Error::Length);
    let cut = &stream[..stream.len() - 2];
    assert_eq!(refused(&with(cut)), Error::Truncated);
    let mut adler = stream.clone();
    *adler.last_mut().unwrap() ^= 1;
    assert_eq!(refused(&with(&adler)), Error::Adler);
    let mut filter = rgb_raw();
    filter[7] = 5;
    assert_eq!(refused(&with(&zlib(&filter))), Error::Filter(5));
}

#[test]
fn errors_name_themselves() {
    for error in [
        Error::NotPng,
        Error::Crc(*b"IDAT"),
        Error::Critical(*b"ABCD"),
        Error::Order(*b"PLTE"),
        Error::Malformed(*b"tRNS"),
        Error::Axis {
            width: 0,
            height: 1,
        },
        Error::Inflate("why".into()),
        Error::Filter(9),
    ] {
        let text = error.to_string();
        assert!(
            text.starts_with("PNG") || text.starts_with("not a PNG"),
            "{text}"
        );
    }
    assert!(Error::Crc(*b"IDAT").to_string().contains("IDAT"));
    assert!(Error::Inflate("why".into()).to_string().contains("why"));
}

#[test]
fn no_prefix_or_corruption_panics() {
    let data = fixture("suite/g4-i.png");
    for end in 0..data.len() {
        assert!(decode(&data[..end]).is_err(), "prefix of {end}");
    }
    // Every byte of the image data flipped, the CRC redone so the change
    // reaches the inflater, the filters and the expansion.
    for name in ["shapes.png", "suite/p2-i.png", "suite/rgba16.png"] {
        let data = fixture(name);
        let mut at = 8;
        while at < data.len() {
            let length = u32::from_be_bytes(data[at..at + 4].try_into().unwrap()) as usize;
            let kind: [u8; 4] = data[at + 4..at + 8].try_into().unwrap();
            if &kind == b"IDAT" {
                for i in 0..length.min(600) {
                    let mut body = data[at + 8..at + 8 + length].to_vec();
                    body[i] ^= 0x5a;
                    let mut bad = data[..at].to_vec();
                    bad.extend_from_slice(&chunk(b"IDAT", &body));
                    bad.extend_from_slice(&data[at + 12 + length..]);
                    if let Ok(image) = decode(&bad) {
                        assert_eq!(image.data.len(), image.width * image.height * 4);
                    }
                }
            }
            at += 12 + length;
        }
    }
}

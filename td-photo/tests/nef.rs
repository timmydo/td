#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

//! The container and codec contract: a synthetic NEF writer and a Nikon
//! Huffman encoder for every tree drive round trips, the real-frame
//! oracle pins the 14-bit lossless tree against an independent decoder,
//! and every refusal DESIGN.md names is exercised.

use std::path::Path;

use td_photo::nef::{
    self, Cfa, Channel, Crop, Decoded, Error, HuffmanParams, COMPRESSION_NIKON, COMPRESSION_NONE,
    MAX_AXIS, TREES,
};
use td_photo::tiff::{self, kind, tag, Endian};

// ---------------------------------------------------------------- TIFF writer

#[derive(Clone, Debug)]
enum Value {
    Short(Vec<u16>),
    Long(Vec<u32>),
    Rational(Vec<(u32, u32)>),
    Ascii(String),
    Byte(Vec<u8>),
    Undefined(Vec<u8>),
    /// An entry written as given: kind, count and the four field bytes, for
    /// the hostile shapes the typed values cannot spell.
    Raw {
        kind: u16,
        count: u32,
        field: [u8; 4],
    },
}

fn u16_bytes(v: u16, big: bool) -> [u8; 2] {
    if big {
        v.to_be_bytes()
    } else {
        v.to_le_bytes()
    }
}

fn u32_bytes(v: u32, big: bool) -> [u8; 4] {
    if big {
        v.to_be_bytes()
    } else {
        v.to_le_bytes()
    }
}

fn encode_value(value: &Value, big: bool) -> (u16, u32, Vec<u8>) {
    match value {
        Value::Short(v) => (
            kind::SHORT,
            v.len() as u32,
            v.iter().flat_map(|x| u16_bytes(*x, big)).collect(),
        ),
        Value::Long(v) => (
            kind::LONG,
            v.len() as u32,
            v.iter().flat_map(|x| u32_bytes(*x, big)).collect(),
        ),
        Value::Rational(v) => (
            kind::RATIONAL,
            v.len() as u32,
            v.iter()
                .flat_map(|(n, d)| [u32_bytes(*n, big), u32_bytes(*d, big)].concat())
                .collect(),
        ),
        Value::Ascii(s) => {
            let mut bytes = s.as_bytes().to_vec();
            bytes.push(0);
            (kind::ASCII, bytes.len() as u32, bytes)
        }
        Value::Byte(v) => (kind::BYTE, v.len() as u32, v.clone()),
        Value::Undefined(v) => (kind::UNDEFINED, v.len() as u32, v.clone()),
        Value::Raw { .. } => panic!("raw entries are written by the builder itself"),
    }
}

/// A TIFF under construction in either byte order: blobs and IFDs appended
/// in order, the first-IFD pointer patched at the end.
struct Builder {
    data: Vec<u8>,
    big: bool,
}

impl Builder {
    fn new() -> Self {
        Self {
            data: b"II\x2a\x00\x00\x00\x00\x00".to_vec(),
            big: false,
        }
    }

    fn big() -> Self {
        Self {
            data: b"MM\x00\x2a\x00\x00\x00\x00".to_vec(),
            big: true,
        }
    }

    fn with_order(big: bool) -> Self {
        if big {
            Self::big()
        } else {
            Self::new()
        }
    }

    fn blob(&mut self, bytes: &[u8]) -> u32 {
        if self.data.len() % 2 == 1 {
            self.data.push(0);
        }
        let offset = self.data.len() as u32;
        self.data.extend_from_slice(bytes);
        offset
    }

    fn ifd_with_next(&mut self, entries: &[(u16, Value)], next: u32) -> u32 {
        let big = self.big;
        let mut fields = Vec::new();
        for (tag, value) in entries {
            if let Value::Raw { kind, count, field } = value {
                fields.push((*tag, *kind, *count, *field));
                continue;
            }
            let (kind, count, bytes) = encode_value(value, big);
            let field = if bytes.len() <= 4 {
                let mut inline = [0u8; 4];
                inline[..bytes.len()].copy_from_slice(&bytes);
                inline
            } else {
                u32_bytes(self.blob(&bytes), big)
            };
            fields.push((*tag, kind, count, field));
        }
        let mut table = Vec::new();
        table.extend_from_slice(&u16_bytes(fields.len() as u16, big));
        for (tag, kind, count, field) in &fields {
            table.extend_from_slice(&u16_bytes(*tag, big));
            table.extend_from_slice(&u16_bytes(*kind, big));
            table.extend_from_slice(&u32_bytes(*count, big));
            table.extend_from_slice(field);
        }
        table.extend_from_slice(&u32_bytes(next, big));
        self.blob(&table)
    }

    fn ifd(&mut self, entries: &[(u16, Value)]) -> u32 {
        self.ifd_with_next(entries, 0)
    }

    fn set_first(&mut self, offset: u32) {
        let big = self.big;
        self.data[4..8].copy_from_slice(&u32_bytes(offset, big));
    }

    fn finish(self) -> Vec<u8> {
        self.data
    }
}

// ------------------------------------------------------------ Nikon encoder

#[derive(Default)]
struct BitWriter {
    out: Vec<u8>,
    acc: u64,
    n: u32,
}

impl BitWriter {
    fn put(&mut self, value: u32, bits: u32) {
        if bits == 0 {
            return;
        }
        self.acc = (self.acc << bits) | u64::from(value & ((1u32 << bits) - 1));
        self.n += bits;
        while self.n >= 8 {
            self.out.push((self.acc >> (self.n - 8)) as u8);
            self.n -= 8;
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.n > 0 {
            self.out.push((self.acc << (8 - self.n)) as u8);
        }
        self.out
    }
}

/// Canonical codes of one tree in assignment order: (symbol, code, length).
fn codes(tree: usize) -> Vec<(u8, u32, u32)> {
    let spec = TREES[tree];
    let mut out = Vec::new();
    let mut code = 0u32;
    let mut next_symbol = 16;
    for len in 1..=16u32 {
        for _ in 0..spec[len as usize - 1] {
            out.push((spec[next_symbol], code, len));
            next_symbol += 1;
            code += 1;
        }
        code <<= 1;
    }
    out
}

/// Encodes `samples` with plain (unshifted) symbols only, switching to
/// `tree + 1` at `split`; panics when a tree lacks the needed length so a
/// test's data stays within what its tree can carry.
fn encode(
    samples: &[u16],
    width: usize,
    tree: usize,
    vpred: [[u16; 2]; 2],
    split: usize,
) -> Vec<u8> {
    let mut writer = BitWriter::default();
    let mut current = tree;
    let mut table = codes(tree);
    let mut vp = vpred;
    let mut hp = [0u16; 2];
    for (row, line) in samples.chunks_exact(width).enumerate() {
        if split != 0 && row == split {
            current = tree + 1;
            table = codes(current);
        }
        for (col, &sample) in line.iter().enumerate() {
            let parity = col & 1;
            let pred = if col < 2 {
                vp[row & 1][parity]
            } else {
                hp[parity]
            };
            let diff = i32::from(sample.wrapping_sub(pred) as i16);
            if col < 2 {
                vp[row & 1][parity] = sample;
            }
            hp[parity] = sample;
            let len = 32 - diff.unsigned_abs().leading_zeros();
            let (_, code, code_len) = table
                .iter()
                .find(|(sym, _, _)| u32::from(*sym) == len)
                .unwrap_or_else(|| panic!("tree {current} has no plain symbol of length {len}"));
            writer.put(*code, *code_len);
            if len > 0 {
                let bits = if diff >= 0 {
                    diff as u32
                } else {
                    (diff + (1 << len) - 1) as u32
                };
                writer.put(bits, len);
            }
        }
    }
    writer.finish()
}

fn lcg(seed: &mut u64) -> u32 {
    *seed = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    (*seed >> 33) as u32
}

fn random_frame(width: usize, height: usize, bits: u32, seed: u64) -> Vec<u16> {
    let mut s = seed;
    (0..width * height)
        .map(|_| (lcg(&mut s) & ((1 << bits) - 1)) as u16)
        .collect()
}

/// A frame whose neighbouring differences stay within `spread`, for the
/// lossy trees whose plain symbols stop short.
fn wandering_frame(width: usize, height: usize, bits: u32, spread: u32, seed: u64) -> Vec<u16> {
    let mut s = seed;
    let max = (1u32 << bits) - 1;
    let mut out = Vec::with_capacity(width * height);
    for _ in 0..height {
        // Each row restarts near the middle so the vertical predictors of
        // the first two columns stay within the same spread as the rest.
        let mut value = [max / 2; 2];
        for col in 0..width {
            let delta = (lcg(&mut s) % (2 * spread + 1)) as i64 - spread as i64;
            let next = (i64::from(value[col & 1]) + delta).clamp(0, i64::from(max)) as u32;
            value[col & 1] = next;
            out.push(next as u16);
        }
    }
    out
}

fn lossless_params(bits: u8) -> HuffmanParams {
    // The Z 8 writes 2048 at both depths; the synthetic writer's table
    // carries the same value.
    HuffmanParams {
        tree: if bits == 14 { 5 } else { 2 },
        vpred: [[2048; 2]; 2],
        curve: None,
        max: 1 << bits,
        split: 0,
    }
}

fn round_trip(samples: &[u16], width: usize, height: usize, params: &HuffmanParams) -> Decoded {
    let stream = encode(samples, width, params.tree, params.vpred, params.split);
    nef::decode_huffman(params, &stream, width, height).unwrap()
}

// ------------------------------------------------------------ synthetic NEF

struct Synth {
    width: usize,
    height: usize,
    bits: u16,
    compression: u16,
    cfa: Vec<u8>,
    make: &'static str,
    model: &'static str,
    orientation: u16,
    wb: Option<[(u32, u32); 4]>,
    black: Option<u16>,
    crop: Option<[u16; 4]>,
    table: Option<Vec<u8>>,
    /// Whether the maker note's own TIFF is big-endian (the file stays
    /// little-endian); the table must then be built in that order too.
    note_big: bool,
    strip: Vec<u8>,
    strip_count: usize,
    /// Added to every strip offset the file declares, to point past it.
    strip_shift: u32,
    previews: bool,
    /// The bytes of the good preview: a bare SOI/EOI pair unless a test
    /// wants a decodable one.
    preview_bytes: Vec<u8>,
    exif: bool,
}

/// The Z 8's lossless 14-bit table: version `0x46 0x30`, four predictors
/// of 2048, a curve length of 34 and 34 bytes nothing reads.
fn lossless_table(big: bool) -> Vec<u8> {
    let mut table = vec![0x46, 0x30];
    for v in [2048u16; 4] {
        table.extend_from_slice(&u16_bytes(v, big));
    }
    table.extend_from_slice(&u16_bytes(34, big));
    table.extend_from_slice(&[0u8; 34]);
    table
}

impl Synth {
    fn lossless(width: usize, height: usize, samples: &[u16]) -> Self {
        let params = lossless_params(14);
        Self {
            width,
            height,
            bits: 14,
            compression: COMPRESSION_NIKON,
            cfa: vec![0, 1, 1, 2],
            make: "NIKON CORPORATION",
            model: "NIKON Z 8",
            orientation: 1,
            wb: Some([(1019, 512), (793, 512), (512, 512), (512, 512)]),
            black: Some(1008),
            crop: Some([2, 2, width as u16 - 4, height as u16 - 4]),
            table: Some(lossless_table(false)),
            note_big: false,
            strip: encode(samples, width, params.tree, params.vpred, 0),
            strip_count: 1,
            strip_shift: 0,
            previews: true,
            preview_bytes: vec![0xff, 0xd8, 0xff, 0xd9],
            exif: true,
        }
    }

    fn build(&self) -> Vec<u8> {
        let mut b = Builder::new();
        let strip = b.blob(&self.strip);
        let preview = b.blob(&self.preview_bytes);
        let bad_preview = b.blob(&[0x00, 0x00, 0x00, 0x00]);
        // The maker note: its own little TIFF behind the ten-byte prefix, in
        // its own byte order.
        let mut inner = Builder::with_order(self.note_big);
        let mut note_entries = Vec::new();
        if let Some(wb) = self.wb {
            note_entries.push((tag::NIKON_WB_RB_LEVELS, Value::Rational(wb.to_vec())));
        }
        if let Some(black) = self.black {
            note_entries.push((tag::NIKON_BLACK_LEVEL, Value::Short(vec![black; 4])));
        }
        if let Some(crop) = self.crop {
            note_entries.push((tag::NIKON_CROP_AREA, Value::Short(crop.to_vec())));
        }
        if let Some(table) = &self.table {
            note_entries.push((tag::NIKON_LINEARIZATION, Value::Undefined(table.clone())));
        }
        let note_ifd = inner.ifd(&note_entries);
        inner.set_first(note_ifd);
        let mut note = b"Nikon\0\x02\x11\0\0".to_vec();
        note.extend_from_slice(&inner.finish());
        let exif = b.ifd(&[
            (tag::EXPOSURE_TIME, Value::Rational(vec![(10, 20000)])),
            (tag::F_NUMBER, Value::Rational(vec![(560, 100)])),
            (tag::ISO, Value::Short(vec![2800])),
            (
                tag::DATE_TIME_ORIGINAL,
                Value::Ascii("2026:09:12 17:18:09".to_string()),
            ),
            (tag::FOCAL_LENGTH, Value::Rational(vec![(1800, 10)])),
            (tag::MAKER_NOTE, Value::Undefined(note)),
            (
                tag::LENS_MODEL,
                Value::Ascii("NIKKOR Z 180-600mm f/5.6-6.3 VR".to_string()),
            ),
        ]);
        let strip_offsets: Vec<u32> = (0..self.strip_count as u32)
            .map(|i| strip + i * 4 + self.strip_shift)
            .collect();
        let strip_counts: Vec<u32> = (0..self.strip_count as u32)
            .map(|_| self.strip.len() as u32)
            .collect();
        let raw = b.ifd(&[
            (tag::NEW_SUBFILE_TYPE, Value::Long(vec![0])),
            (tag::IMAGE_WIDTH, Value::Long(vec![self.width as u32])),
            (tag::IMAGE_LENGTH, Value::Long(vec![self.height as u32])),
            (tag::BITS_PER_SAMPLE, Value::Short(vec![self.bits])),
            (tag::COMPRESSION, Value::Short(vec![self.compression])),
            (tag::PHOTOMETRIC, Value::Short(vec![32803])),
            (tag::STRIP_OFFSETS, Value::Long(strip_offsets)),
            (tag::SAMPLES_PER_PIXEL, Value::Short(vec![1])),
            (tag::ROWS_PER_STRIP, Value::Long(vec![self.height as u32])),
            (tag::STRIP_BYTE_COUNTS, Value::Long(strip_counts)),
            (tag::CFA_REPEAT_DIM, Value::Short(vec![2, 2])),
            (tag::CFA_PATTERN, Value::Byte(self.cfa.clone())),
        ]);
        let mut small_entries = vec![(tag::NEW_SUBFILE_TYPE, Value::Long(vec![1]))];
        if self.previews {
            small_entries.push((tag::JPEG_OFFSET, Value::Long(vec![preview])));
            small_entries.push((
                tag::JPEG_LENGTH,
                Value::Long(vec![self.preview_bytes.len() as u32]),
            ));
        }
        let small = b.ifd(&small_entries);
        let mut ifd0 = vec![
            (tag::NEW_SUBFILE_TYPE, Value::Long(vec![1])),
            (tag::MAKE, Value::Ascii(self.make.to_string())),
            (tag::MODEL, Value::Ascii(self.model.to_string())),
            (tag::ORIENTATION, Value::Short(vec![self.orientation])),
            (tag::SUB_IFDS, Value::Long(vec![small, raw])),
        ];
        if self.previews {
            // A preview that does not begin with SOI is skipped, not fatal.
            ifd0.push((tag::JPEG_OFFSET, Value::Long(vec![bad_preview])));
            ifd0.push((tag::JPEG_LENGTH, Value::Long(vec![4])));
        }
        if self.exif {
            ifd0.push((tag::EXIF_IFD, Value::Long(vec![exif])));
        }
        let first = b.ifd(&ifd0);
        b.set_first(first);
        b.finish()
    }
}

// ------------------------------------------------------------------- trees

#[test]
fn every_tree_is_a_complete_prefix_code() {
    for (index, spec) in TREES.iter().enumerate() {
        let counts = &spec[..16];
        let total: usize = counts.iter().map(|c| *c as usize).sum();
        let kraft: f64 = counts
            .iter()
            .enumerate()
            .map(|(i, c)| f64::from(*c) / 2f64.powi(i as i32 + 1))
            .sum();
        assert!(
            (kraft - 1.0).abs() < 1e-12,
            "tree {index} Kraft sum {kraft}"
        );
        assert!(total <= 16, "tree {index} symbol count {total}");
        // Every plain difference length the lossless trees need is present.
        if index == 2 || index == 5 {
            let top = if index == 5 { 14 } else { 12 };
            for len in 0..=top {
                assert!(
                    spec[16..16 + total].contains(&len),
                    "tree {index} lacks length {len}"
                );
            }
        }
    }
}

// ------------------------------------------------------------- round trips

#[test]
fn lossless_14_bit_round_trips_a_random_frame() {
    let (w, h) = (64, 32);
    let samples = random_frame(w, h, 14, 7);
    let decoded = round_trip(&samples, w, h, &lossless_params(14));
    assert_eq!(decoded.samples, samples);
    assert_eq!(decoded.corrupt, 0);
    assert_eq!((decoded.width, decoded.height), (w, h));
}

#[test]
fn lossless_12_bit_round_trips_a_random_frame() {
    let (w, h) = (40, 20);
    let samples = random_frame(w, h, 12, 11);
    let decoded = round_trip(&samples, w, h, &lossless_params(12));
    assert_eq!(decoded.samples, samples);
    assert_eq!(decoded.corrupt, 0);
}

#[test]
fn edge_values_round_trip() {
    let (w, h) = (16, 4);
    let params = lossless_params(14);
    for samples in [
        vec![0u16; w * h],
        vec![16383u16; w * h],
        (0..w * h)
            .map(|i| if i % 2 == 0 { 0 } else { 16383 })
            .collect::<Vec<_>>(),
        (0..w * h)
            .map(|i| if (i / w) % 2 == 0 { 16383 } else { 0 })
            .collect::<Vec<_>>(),
    ] {
        let decoded = round_trip(&samples, w, h, &params);
        assert_eq!(decoded.samples, samples);
        assert_eq!(decoded.corrupt, 0);
    }
}

#[test]
fn odd_width_keeps_the_column_parity_predictors_apart() {
    let (w, h) = (7, 6);
    let samples = random_frame(w, h, 14, 3);
    let decoded = round_trip(&samples, w, h, &lossless_params(14));
    assert_eq!(decoded.samples, samples);
}

#[test]
fn lossy_trees_round_trip_small_differences_across_the_split() {
    // 12-bit: tree 0 then 1; tree 1's plain lengths stop at 5.
    let (w, h) = (32, 12);
    let samples = wandering_frame(w, h, 12, 15, 5);
    let params = HuffmanParams {
        tree: 0,
        vpred: [[2048; 2]; 2],
        curve: None,
        max: 4096,
        split: 6,
    };
    let decoded = round_trip(&samples, w, h, &params);
    assert_eq!(decoded.samples, samples);
    assert_eq!(decoded.corrupt, 0);
    // 14-bit: tree 3 then 4; tree 4's plain lengths stop at 8.
    let samples = wandering_frame(w, h, 14, 120, 9);
    let params = HuffmanParams {
        tree: 3,
        vpred: [[8192; 2]; 2],
        curve: None,
        max: 16384,
        split: 5,
    };
    let decoded = round_trip(&samples, w, h, &params);
    assert_eq!(decoded.samples, samples);
    assert_eq!(decoded.corrupt, 0);
}

#[test]
fn a_curve_maps_every_decoded_sample() {
    let (w, h) = (24, 8);
    let samples = random_frame(w, h, 12, 21);
    let curve: Vec<u16> = (0..4096u32).map(|i| (i * 4) as u16).collect();
    let mut params = lossless_params(12);
    params.curve = Some(curve.clone());
    let decoded = round_trip(&samples, w, h, &params);
    let expected: Vec<u16> = samples.iter().map(|s| curve[*s as usize]).collect();
    assert_eq!(decoded.samples, expected);
    assert_eq!(decoded.corrupt, 0);
}

#[test]
fn samples_at_or_past_max_are_counted_and_clamped() {
    let (w, h) = (8, 2);
    // Predictors start high and the samples run past the valid range.
    let samples: Vec<u16> = (0..w * h).map(|i| 16380 + (i % 8) as u16).collect();
    let params = lossless_params(14);
    let decoded = round_trip(&samples, w, h, &params);
    let over = samples.iter().filter(|s| **s >= 16384).count();
    assert!(over > 0);
    assert_eq!(decoded.corrupt, over);
    for (out, sample) in decoded.samples.iter().zip(samples.iter()) {
        assert_eq!(*out, (*sample).min(0x3fff));
    }
}

#[test]
fn a_truncated_stream_reports_the_rows_completed() {
    let (w, h) = (64, 8);
    let samples = random_frame(w, h, 14, 13);
    let params = lossless_params(14);
    let stream = encode(&samples, w, params.tree, params.vpred, 0);
    let cut = &stream[..stream.len() / 2];
    match nef::decode_huffman(&params, cut, w, h) {
        Err(Error::Truncated { rows }) => assert!((1..h).contains(&rows), "rows {rows}"),
        other => panic!("expected truncation, got {other:?}"),
    }
    // The whole stream decodes, and so does one with trailing padding.
    let mut padded = stream.clone();
    padded.extend_from_slice(&[0xaa; 16]);
    assert_eq!(
        nef::decode_huffman(&params, &padded, w, h).unwrap().samples,
        samples
    );
}

#[test]
fn an_unknown_tree_is_refused() {
    let params = HuffmanParams {
        tree: 6,
        vpred: [[0; 2]; 2],
        curve: None,
        max: 16384,
        split: 0,
    };
    assert_eq!(
        nef::decode_huffman(&params, &[0; 16], 4, 2).unwrap_err(),
        Error::Tree(6)
    );
    assert!(matches!(
        nef::decode_huffman(&lossless_params(14), &[0; 16], MAX_AXIS + 1, 2),
        Err(Error::Axis { .. })
    ));
}

// ------------------------------------------------------------- the oracle

/// FNV-1a over the samples as little-endian `u16` bytes, as the reference
/// script computes it.
fn fnv1a64(samples: &[u16]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for s in samples {
        for b in s.to_le_bytes() {
            hash ^= u64::from(b);
            hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    hash
}

#[test]
fn real_z8_rows_match_the_independent_reference() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/z8-rows.bin");
    let stream = std::fs::read(path).unwrap();
    assert_eq!(stream.len(), 18192);
    let params = HuffmanParams {
        tree: 5,
        vpred: [[2048; 2]; 2],
        curve: None,
        max: 16384,
        split: 0,
    };
    let decoded = nef::decode_huffman(&params, &stream, 8280, 2).unwrap();
    assert_eq!(decoded.corrupt, 0);
    assert_eq!(
        &decoded.samples[..8],
        &[1041, 1136, 1041, 1141, 1040, 1131, 1046, 1143]
    );
    assert_eq!(
        &decoded.samples[8280..8288],
        &[1147, 1063, 1188, 1043, 1098, 1115, 1198, 1105]
    );
    assert_eq!(fnv1a64(&decoded.samples), 0x44df96cc9a8684a0);
}

// ------------------------------------------------------------- the table

#[test]
fn the_z8_linearization_header_selects_the_lossless_14_bit_tree() {
    let mut table = vec![0x46u8, 0x30];
    for v in [2048u16; 4] {
        table.extend_from_slice(&v.to_le_bytes());
    }
    table.extend_from_slice(&34u16.to_le_bytes());
    table.extend_from_slice(&[0x11; 34]);
    let params = HuffmanParams::parse(&table, 14, Endian::Little).unwrap();
    assert_eq!(
        params,
        HuffmanParams {
            tree: 5,
            vpred: [[2048; 2]; 2],
            curve: None,
            max: 16384,
            split: 0,
        }
    );
    // The same header at 12 bits is the 12-bit lossless tree.
    assert_eq!(
        HuffmanParams::parse(&table, 12, Endian::Little)
            .unwrap()
            .tree,
        2
    );
    // Big-endian shorts read in the file's order.
    let mut big = vec![0x46u8, 0x30];
    for v in [2048u16; 4] {
        big.extend_from_slice(&v.to_be_bytes());
    }
    big.extend_from_slice(&34u16.to_be_bytes());
    big.extend_from_slice(&[0; 34]);
    assert_eq!(
        HuffmanParams::parse(&big, 14, Endian::Big).unwrap().vpred,
        [[2048; 2]; 2]
    );
    assert_eq!(
        HuffmanParams::parse(&table[..6], 14, Endian::Little).unwrap_err(),
        Error::Linearization("table too short")
    );
    assert_eq!(
        HuffmanParams::parse(&table, 16, Endian::Little).unwrap_err(),
        Error::Bits(16)
    );
}

#[test]
fn a_lossy_type_2_table_carries_a_sampled_curve_and_a_split() {
    let mut table = vec![0x44u8, 0x20];
    for v in [2048u16; 4] {
        table.extend_from_slice(&v.to_le_bytes());
    }
    table.extend_from_slice(&5u16.to_le_bytes());
    for v in [0u16, 1000, 2000, 3000, 4000] {
        table.extend_from_slice(&v.to_le_bytes());
    }
    table.resize(562, 0);
    table.extend_from_slice(&3u16.to_le_bytes());
    let params = HuffmanParams::parse(&table, 12, Endian::Little).unwrap();
    assert_eq!(params.tree, 0);
    assert_eq!(params.split, 3);
    assert_eq!(params.max, 4096);
    let curve = params.curve.as_ref().unwrap();
    // One past `max` is kept: dcraw leaves the last knot there, and the
    // widened range after the split can index it.
    assert_eq!(curve.len(), 4097);
    assert_eq!(curve[4096], 4000);
    assert_eq!(curve[0], 0);
    assert_eq!(curve[512], 500);
    assert_eq!(curve[1024], 1000);
    assert_eq!(curve[4095], 3999);
    // Round trip through trees 0 and 1 with that curve applied.
    let (w, h) = (16, 6);
    let samples = wandering_frame(w, h, 12, 15, 17);
    let decoded = round_trip(&samples, w, h, &params);
    let expected: Vec<u16> = samples.iter().map(|s| curve[*s as usize]).collect();
    assert_eq!(decoded.samples, expected);
}

#[test]
fn a_plain_curve_trims_repeated_top_values_from_the_range() {
    let mut table = vec![0x44u8, 0x10];
    for v in [2048u16; 4] {
        table.extend_from_slice(&v.to_le_bytes());
    }
    table.extend_from_slice(&4096u16.to_le_bytes());
    for i in 0..4096u32 {
        let v = (i.min(4093) * 2) as u16;
        table.extend_from_slice(&v.to_le_bytes());
    }
    let params = HuffmanParams::parse(&table, 12, Endian::Little).unwrap();
    assert_eq!(params.tree, 0);
    assert_eq!(params.max, 4094);
    assert_eq!(params.curve.as_ref().unwrap().len(), 4096);
}

// ---------------------------------------------------------------- the file

#[test]
fn parse_reads_the_synthetic_layout_and_decode_recovers_the_frame() {
    let (w, h) = (32, 16);
    let samples = random_frame(w, h, 14, 99);
    let synth = Synth::lossless(w, h, &samples);
    let file = synth.build();
    let nef = nef::parse(&file).unwrap();
    assert_eq!(nef.make, "NIKON CORPORATION");
    assert_eq!(nef.model, "NIKON Z 8");
    assert_eq!(nef.orientation, 1);
    assert_eq!(nef.endian, Endian::Little);
    assert_eq!((nef.raw.width, nef.raw.height, nef.raw.bits), (w, h, 14));
    assert_eq!(nef.raw.compression, COMPRESSION_NIKON);
    assert_eq!(nef.raw.cfa, Cfa::RGGB);
    assert_eq!(nef.raw.strip.1, synth.strip.len());
    assert_eq!(nef.maker.black, Some(1008));
    let (r, b) = nef.maker.wb.unwrap();
    assert!((r - 1019.0 / 512.0).abs() < 1e-6 && (b - 793.0 / 512.0).abs() < 1e-6);
    assert_eq!(
        nef.crop(),
        Crop {
            left: 2,
            top: 2,
            width: w - 4,
            height: h - 4
        }
    );
    assert_eq!(nef.maker.linearization.map(|(_, len)| len), Some(46));
    assert_eq!(nef.previews.len(), 1, "the non-SOI preview is skipped");
    assert_eq!(nef.exposure.time, Some((10, 20000)));
    assert_eq!(nef.exposure.aperture, Some((560, 100)));
    assert_eq!(nef.exposure.iso, Some(2800));
    assert_eq!(nef.exposure.focal_length, Some((1800, 10)));
    assert_eq!(nef.exposure.taken.as_deref(), Some("2026:09:12 17:18:09"));
    assert_eq!(
        nef.exposure.lens.as_deref(),
        Some("NIKKOR Z 180-600mm f/5.6-6.3 VR")
    );
    let decoded = nef::decode(&nef, &file).unwrap();
    assert_eq!(decoded.samples, samples);
    assert_eq!(decoded.corrupt, 0);
}

#[test]
fn an_uncompressed_strip_decodes_in_the_file_order() {
    let (w, h) = (6, 4);
    let samples: Vec<u16> = (0..w * h).map(|i| (i * 500) as u16).collect();
    let mut synth = Synth::lossless(w, h, &samples);
    synth.compression = COMPRESSION_NONE;
    synth.table = None;
    synth.strip = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
    let file = synth.build();
    let nef = nef::parse(&file).unwrap();
    assert_eq!(nef::decode(&nef, &file).unwrap().samples, samples);
    // A strip shorter than the frame is refused.
    synth.strip.truncate(10);
    let file = synth.build();
    let nef = nef::parse(&file).unwrap();
    assert_eq!(nef::decode(&nef, &file).unwrap_err(), Error::Samples);
}

#[test]
fn missing_maker_note_facts_fall_back() {
    let (w, h) = (8, 8);
    let samples = random_frame(w, h, 14, 1);
    let mut synth = Synth::lossless(w, h, &samples);
    synth.wb = None;
    synth.black = None;
    synth.crop = None;
    let file = synth.build();
    let nef = nef::parse(&file).unwrap();
    assert_eq!(nef.maker.wb, None);
    assert_eq!(nef.maker.black, None);
    assert_eq!(
        nef.crop(),
        Crop {
            left: 0,
            top: 0,
            width: w,
            height: h
        }
    );
    // A crop outside the frame is ignored, not applied.
    synth.crop = Some([4, 4, 8, 8]);
    let nef = nef::parse(&synth.build()).unwrap();
    assert_eq!(nef.maker.crop.unwrap().width, 8);
    assert_eq!(nef.crop().left, 0);
    // No Exif at all is still a readable file.
    synth.exif = false;
    let nef = nef::parse(&synth.build()).unwrap();
    assert_eq!(nef.exposure, nef::Exposure::default());
    assert_eq!(nef.maker.linearization, None);
    assert_eq!(
        nef::decode(&nef, &synth.build()).unwrap_err(),
        Error::NoLinearization
    );
}

#[test]
fn parse_refuses_what_the_design_names() {
    let (w, h) = (8, 8);
    let samples = random_frame(w, h, 14, 2);

    let mut synth = Synth::lossless(w, h, &samples);
    synth.compression = 99;
    let file = synth.build();
    let nef = nef::parse(&file).unwrap();
    assert_eq!(
        nef::decode(&nef, &file).unwrap_err(),
        Error::Compression(99)
    );

    let mut synth = Synth::lossless(w, h, &samples);
    synth.cfa = vec![0, 0, 1, 2];
    assert_eq!(nef::parse(&synth.build()).unwrap_err(), Error::Pattern);

    let mut synth = Synth::lossless(w, h, &samples);
    synth.bits = 16;
    assert_eq!(nef::parse(&synth.build()).unwrap_err(), Error::Bits(16));

    let mut synth = Synth::lossless(w, h, &samples);
    synth.strip_count = 2;
    assert_eq!(nef::parse(&synth.build()).unwrap_err(), Error::Strip);

    let mut synth = Synth::lossless(w, h, &samples);
    synth.width = MAX_AXIS + 1;
    assert!(matches!(
        nef::parse(&synth.build()).unwrap_err(),
        Error::Axis { .. }
    ));

    // Not a TIFF at all.
    assert_eq!(
        nef::parse(b"P6 1 1 255 ").unwrap_err(),
        Error::Container(tiff::Error::NotTiff)
    );

    // A strip that points past the file is refused when decoded.
    let mut synth = Synth::lossless(w, h, &samples);
    synth.strip_shift = 1 << 30;
    let file = synth.build();
    let nef = nef::parse(&file).unwrap();
    assert!(nef.raw.strip.0 > file.len());
    assert!(matches!(
        nef::decode(&nef, &file).unwrap_err(),
        Error::Container(tiff::Error::OutOfFile { .. })
    ));
}

#[test]
fn no_cfa_sub_image_is_no_raw_image() {
    let mut b = Builder::new();
    let preview = b.blob(&[0xff, 0xd8, 0xff, 0xd9]);
    let small = b.ifd(&[
        (tag::JPEG_OFFSET, Value::Long(vec![preview])),
        (tag::JPEG_LENGTH, Value::Long(vec![4])),
    ]);
    let first = b.ifd(&[
        (tag::MAKE, Value::Ascii("X".to_string())),
        (tag::SUB_IFDS, Value::Long(vec![small])),
    ]);
    b.set_first(first);
    assert_eq!(nef::parse(&b.finish()).unwrap_err(), Error::NoRawImage);
}

#[test]
fn container_cycles_and_oversize_tables_are_refused() {
    // IFD0's next pointer back to itself.
    let mut b = Builder::new();
    let first = b.ifd_with_next(&[(tag::MAKE, Value::Ascii("X".to_string()))], 8);
    b.set_first(first);
    let mut file = b.finish();
    let next_at = file.len() - 4;
    file[next_at..].copy_from_slice(&first.to_le_bytes());
    assert_eq!(
        nef::parse(&file).unwrap_err(),
        Error::Container(tiff::Error::Cycle {
            offset: first as usize
        })
    );

    // A sub-IFD pointing back at IFD0.
    let mut b = Builder::new();
    let first = b.ifd(&[(tag::SUB_IFDS, Value::Long(vec![0]))]);
    b.set_first(first);
    let mut file = b.finish();
    // Patch the SubIFDs inline value to IFD0's own offset.
    let entry_value = first as usize + 2 + 8;
    file[entry_value..entry_value + 4].copy_from_slice(&first.to_le_bytes());
    assert_eq!(
        nef::parse(&file).unwrap_err(),
        Error::Container(tiff::Error::Cycle {
            offset: first as usize
        })
    );

    // An entry count past the ceiling.
    let mut file = b"II\x2a\x00\x08\x00\x00\x00".to_vec();
    file.extend_from_slice(&5000u16.to_le_bytes());
    file.resize(8 + 2 + 12 * 5000 + 4, 0);
    assert_eq!(
        nef::parse(&file).unwrap_err(),
        Error::Container(tiff::Error::TooManyEntries {
            offset: 8,
            count: 5000
        })
    );

    // An IFD whose table runs past the file.
    let mut file = b"II\x2a\x00\x08\x00\x00\x00".to_vec();
    file.extend_from_slice(&3u16.to_le_bytes());
    assert!(matches!(
        nef::parse(&file).unwrap_err(),
        Error::Container(tiff::Error::OutOfFile { .. })
    ));

    // A chain longer than the ceiling.
    let mut b = Builder::new();
    let mut offsets = Vec::new();
    for _ in 0..(tiff::MAX_CHAIN + 1) {
        offsets.push(b.ifd_with_next(&[(tag::MAKE, Value::Ascii("X".to_string()))], 0));
    }
    let mut file = b.finish();
    for pair in offsets.windows(2) {
        let (this, next) = (pair[0] as usize, pair[1]);
        let count = u16::from_le_bytes([file[this], file[this + 1]]) as usize;
        let at = this + 2 + 12 * count;
        file[at..at + 4].copy_from_slice(&next.to_le_bytes());
    }
    file[4..8].copy_from_slice(&offsets[0].to_le_bytes());
    assert_eq!(
        nef::parse(&file).unwrap_err(),
        Error::Container(tiff::Error::TooManyIfds)
    );
}

#[test]
fn oversize_input_is_refused_before_parsing() {
    // The reader refuses a slice over the ceiling without reading it; a
    // Vec that large is not allocated here, the check is by length alone.
    let header = b"II\x2a\x00\x08\x00\x00\x00";
    let (reader, first) = tiff::Reader::new(header, 0).unwrap();
    assert_eq!(first, 8);
    assert_eq!(reader.endian(), Endian::Little);
    assert_eq!(tiff::MAX_FILE_BYTES, 512 << 20);
}

// ---------------------------------------------------------------- the CFA

#[test]
fn cfa_patterns_are_the_four_bayer_arrangements() {
    let rggb = Cfa::from_codes(&[0, 1, 1, 2]).unwrap();
    assert_eq!(rggb, Cfa::RGGB);
    assert_eq!(rggb.name(), "RGGB");
    assert_eq!(rggb.at(0, 0), Channel::Red);
    assert_eq!(rggb.at(1, 0), Channel::Green);
    assert_eq!(rggb.at(0, 1), Channel::Green);
    assert_eq!(rggb.at(1, 1), Channel::Blue);
    assert_eq!(rggb.at(2, 3), Channel::Green);
    assert_eq!(rggb.at(3, 3), Channel::Blue);
    let bggr = Cfa::from_codes(&[2, 1, 1, 0]).unwrap();
    assert_eq!(bggr.name(), "BGGR");
    assert_eq!(bggr.at(0, 0), Channel::Blue);
    assert_eq!(Cfa::from_codes(&[1, 0, 2, 1]).unwrap().name(), "GRBG");
    assert_eq!(Cfa::from_codes(&[1, 2, 0, 1]).unwrap().name(), "GBRG");
    for bad in [
        &[0u8, 1, 2, 1][..],
        &[0, 1, 1, 1],
        &[0, 1, 1],
        &[0, 1, 1, 2, 0],
        &[3, 1, 1, 2],
    ] {
        assert_eq!(Cfa::from_codes(bad).unwrap_err(), Error::Pattern, "{bad:?}");
    }
}

#[test]
fn crop_fits_only_inside_the_frame() {
    let crop = Crop {
        left: 12,
        top: 8,
        width: 8256,
        height: 5504,
    };
    assert!(crop.fits(8280, 5520));
    assert!(!crop.fits(8267, 5520));
    assert!(!Crop {
        left: 0,
        top: 0,
        width: 1,
        height: 4
    }
    .fits(8, 8));
    assert!(!Crop {
        left: usize::MAX,
        top: 0,
        width: 2,
        height: 2
    }
    .fits(8, 8));
}

// ------------------------------------------------------- more of the codec

/// dcraw's difference rule for one symbol and its raw payload bits.
fn dcraw_diff(sym: u8, raw: u32) -> i32 {
    let len = u32::from(sym & 15);
    let shl = u32::from(sym >> 4);
    if len == 0 {
        return 0;
    }
    let mut d = (((raw as i32) << 1) + 1) << shl >> 1;
    if (d & (1 << (len - 1))) == 0 {
        d -= (1 << len) - i32::from(shl == 0);
    }
    d
}

#[test]
fn shifted_symbols_decode_as_dcraw_computes_them() {
    // Hand-built by the Opus reviewer: four of tree 4's 0x5c (code 010,
    // length 12, shift 5, seven payload bits), two with a full payload
    // (+4080) and two with a zero payload, which takes the negative branch
    // of the sign extension (-4080).
    let stream = [0x5fu8, 0xd7, 0xf4, 0x01, 0x00];
    let params = HuffmanParams {
        tree: 4,
        vpred: [[8192; 2]; 2],
        curve: None,
        max: 16384,
        split: 0,
    };
    let decoded = nef::decode_huffman(&params, &stream, 4, 1).unwrap();
    assert_eq!(decoded.samples, vec![12272, 12272, 8192, 8192]);
    // Every shifted symbol of the two after-split trees over every payload,
    // as the first sample of a two-sample row followed by a zero symbol.
    let mut shifted = 0;
    for (tree, bits) in [(1usize, 12u8), (4, 14)] {
        let table = codes(tree);
        let zero = table.iter().find(|(sym, _, _)| *sym == 0).unwrap();
        for (sym, code, code_len) in table.iter().filter(|(sym, _, _)| sym >> 4 != 0) {
            let payload_bits = u32::from(sym & 15) - u32::from(sym >> 4);
            for raw in 0..(1u32 << payload_bits) {
                let mut w = BitWriter::default();
                w.put(*code, *code_len);
                w.put(raw, payload_bits);
                w.put(zero.1, zero.2);
                let base = 1u16 << (bits - 1);
                let params = HuffmanParams {
                    tree,
                    vpred: [[base; 2]; 2],
                    curve: None,
                    max: 1 << bits,
                    split: 0,
                };
                let decoded = nef::decode_huffman(&params, &w.finish(), 2, 1).unwrap();
                let expected = base.wrapping_add(dcraw_diff(*sym, raw) as u16);
                assert_eq!(
                    decoded.samples,
                    vec![expected, base],
                    "tree {tree} symbol {sym:#x} raw {raw}"
                );
                assert_eq!(decoded.corrupt, 0);
                shifted += 1;
            }
        }
    }
    assert!(shifted > 200, "{shifted} shifted cases");
}

#[test]
fn a_post_split_sample_at_max_takes_the_curves_last_knot() {
    let mut table = vec![0x44u8, 0x20];
    for v in [2048u16; 4] {
        table.extend_from_slice(&v.to_le_bytes());
    }
    table.extend_from_slice(&5u16.to_le_bytes());
    for v in [0u16, 1000, 2000, 3000, 4000] {
        table.extend_from_slice(&v.to_le_bytes());
    }
    table.resize(562, 0);
    table.extend_from_slice(&1u16.to_le_bytes());
    let params = HuffmanParams::parse(&table, 12, Endian::Little).unwrap();
    assert_eq!(params.split, 1);
    // Row 1 is decoded with tree 1 and the widened range, so a predicted
    // 4096 is admitted and reads the knot dcraw leaves at curve[4096].
    let samples = [2048u16, 2048, 4096, 2048];
    let decoded = round_trip(&samples, 2, 2, &params);
    assert_eq!(decoded.corrupt, 0);
    assert_eq!(decoded.samples, vec![2000, 2000, 4000, 2000]);
    // Knots that do not divide the range: the last segment interpolates
    // toward dcraw's identity-initialised table, and entry `max` is identity.
    let mut table = vec![0x44u8, 0x20];
    for v in [2048u16; 4] {
        table.extend_from_slice(&v.to_le_bytes());
    }
    table.extend_from_slice(&7u16.to_le_bytes());
    for i in 0..7u16 {
        table.extend_from_slice(&(i * 1000).to_le_bytes());
    }
    table.resize(562, 0);
    table.extend_from_slice(&0u16.to_le_bytes());
    let params = HuffmanParams::parse(&table, 12, Endian::Little).unwrap();
    let curve = params.curve.as_ref().unwrap();
    // step = 4096 / 6 = 682; knots at 0, 682, ..., 4092.
    assert_eq!(curve[4092], 6000);
    assert_eq!(u32::from(curve[4095]), (6000 * 679 + 4774 * 3) / 682);
    assert_eq!(curve[4096], 4096);
}

#[test]
fn the_maker_notes_own_byte_order_reads_its_table() {
    let (w, h) = (16, 8);
    let samples = random_frame(w, h, 14, 31);
    let mut synth = Synth::lossless(w, h, &samples);
    synth.note_big = true;
    synth.table = Some(lossless_table(true));
    let file = synth.build();
    let nef = nef::parse(&file).unwrap();
    assert_eq!(nef.endian, Endian::Little);
    assert_eq!(nef.maker.endian, Endian::Big);
    assert_eq!(nef.maker.black, Some(1008));
    assert_eq!(nef.maker.crop.map(|c| c.width), Some(w - 4));
    let (r, _) = nef.maker.wb.unwrap();
    assert!((r - 1019.0 / 512.0).abs() < 1e-6);
    assert_eq!(nef::decode(&nef, &file).unwrap().samples, samples);
    // Read in the file's order instead, the predictors come out byte-swapped
    // and every sample would be wrong with nothing counted corrupt.
    let (offset, len) = nef.maker.linearization.unwrap();
    let wrong = HuffmanParams::parse(&file[offset..offset + len], 14, Endian::Little).unwrap();
    assert_eq!(wrong.vpred, [[8; 2]; 2]);
}

#[test]
fn a_field_whose_declared_extent_leaves_the_file_is_refused() {
    let mut b = Builder::new();
    let raw = b.ifd(&[
        (
            tag::IMAGE_WIDTH,
            Value::Raw {
                kind: kind::LONG,
                count: 0xffff_ffff,
                field: 8u32.to_le_bytes(),
            },
        ),
        (tag::IMAGE_LENGTH, Value::Long(vec![8])),
        (tag::PHOTOMETRIC, Value::Short(vec![32803])),
    ]);
    let first = b.ifd(&[(tag::SUB_IFDS, Value::Long(vec![raw]))]);
    b.set_first(first);
    assert!(matches!(
        nef::parse(&b.finish()).unwrap_err(),
        Error::Container(tiff::Error::OutOfFile { .. })
    ));
    // A compression value past u16 is named as what it is.
    let (w, h) = (8, 8);
    let samples = random_frame(w, h, 14, 5);
    let mut b = Builder::new();
    let strip = b.blob(&encode(&samples, w, 5, [[2048; 2]; 2], 0));
    let raw = b.ifd(&[
        (tag::IMAGE_WIDTH, Value::Long(vec![w as u32])),
        (tag::IMAGE_LENGTH, Value::Long(vec![h as u32])),
        (tag::BITS_PER_SAMPLE, Value::Short(vec![14])),
        (tag::COMPRESSION, Value::Long(vec![70000])),
        (tag::PHOTOMETRIC, Value::Short(vec![32803])),
        (tag::STRIP_OFFSETS, Value::Long(vec![strip])),
        (tag::STRIP_BYTE_COUNTS, Value::Long(vec![16])),
        (tag::CFA_PATTERN, Value::Byte(vec![0, 1, 1, 2])),
    ]);
    let first = b.ifd(&[(tag::SUB_IFDS, Value::Long(vec![raw]))]);
    b.set_first(first);
    assert_eq!(
        nef::parse(&b.finish()).unwrap_err(),
        Error::Compression(70000)
    );
}

#[test]
fn previews_are_collected_along_ifd0s_chain() {
    let (w, h) = (8, 8);
    let samples = random_frame(w, h, 14, 4);
    let mut file = Synth::lossless(w, h, &samples).build();
    // Append an IFD1 naming a preview and chain IFD0 to it.
    let jpeg_at = file.len() as u32;
    file.extend_from_slice(&[0xff, 0xd8, 0xff, 0xd9]);
    let ifd1_at = file.len() as u32;
    let mut table = Vec::new();
    table.extend_from_slice(&2u16.to_le_bytes());
    for (t, v) in [(tag::JPEG_OFFSET, jpeg_at), (tag::JPEG_LENGTH, 4)] {
        table.extend_from_slice(&t.to_le_bytes());
        table.extend_from_slice(&kind::LONG.to_le_bytes());
        table.extend_from_slice(&1u32.to_le_bytes());
        table.extend_from_slice(&v.to_le_bytes());
    }
    table.extend_from_slice(&0u32.to_le_bytes());
    file.extend_from_slice(&table);
    let ifd0 = u32::from_le_bytes(file[4..8].try_into().unwrap()) as usize;
    let count = u16::from_le_bytes([file[ifd0], file[ifd0 + 1]]) as usize;
    let next_at = ifd0 + 2 + 12 * count;
    file[next_at..next_at + 4].copy_from_slice(&ifd1_at.to_le_bytes());
    let nef = nef::parse(&file).unwrap();
    // The chain is walked before the sub-IFDs, so IFD1's preview comes
    // first and the small sub-IFD's second; IFD0's non-JPEG one is skipped.
    assert_eq!(nef.previews.len(), 2);
    assert_eq!(
        nef.previews[0],
        nef::Preview {
            offset: jpeg_at as usize,
            len: 4
        }
    );
    assert!(nef.previews[1].offset < jpeg_at as usize);
}

#[test]
fn a_maker_note_is_confined_to_the_extent_exif_declares() {
    let (w, h) = (8, 8);
    let samples = random_frame(w, h, 14, 9);
    // `count` overrides the maker note field's declared byte count; the
    // note itself is always written whole.
    let build = |count: Option<u32>| -> Vec<u8> {
        let mut b = Builder::new();
        let stream = encode(&samples, w, 5, [[2048; 2]; 2], 0);
        let strip_len = stream.len() as u32;
        let strip = b.blob(&stream);
        let mut inner = Builder::new();
        let note_ifd = inner.ifd(&[
            (tag::NIKON_BLACK_LEVEL, Value::Short(vec![1008; 4])),
            (
                tag::NIKON_LINEARIZATION,
                Value::Undefined(lossless_table(false)),
            ),
        ]);
        inner.set_first(note_ifd);
        let mut note = b"Nikon\0\x02\x11\0\0".to_vec();
        note.extend_from_slice(&inner.finish());
        let declared = count.unwrap_or(note.len() as u32);
        let note_at = b.blob(&note);
        let exif = b.ifd(&[(
            tag::MAKER_NOTE,
            Value::Raw {
                kind: kind::UNDEFINED,
                count: declared,
                field: note_at.to_le_bytes(),
            },
        )]);
        let raw = b.ifd(&[
            (tag::IMAGE_WIDTH, Value::Long(vec![w as u32])),
            (tag::IMAGE_LENGTH, Value::Long(vec![h as u32])),
            (tag::BITS_PER_SAMPLE, Value::Short(vec![14])),
            (tag::COMPRESSION, Value::Short(vec![COMPRESSION_NIKON])),
            (tag::PHOTOMETRIC, Value::Short(vec![32803])),
            (tag::STRIP_OFFSETS, Value::Long(vec![strip])),
            (tag::STRIP_BYTE_COUNTS, Value::Long(vec![strip_len])),
            (tag::CFA_PATTERN, Value::Byte(vec![0, 1, 1, 2])),
        ]);
        let first = b.ifd(&[
            (tag::MAKE, Value::Ascii("NIKON CORPORATION".to_string())),
            (tag::MODEL, Value::Ascii("NIKON Z 8".to_string())),
            (tag::SUB_IFDS, Value::Long(vec![raw])),
            (tag::EXIF_IFD, Value::Long(vec![exif])),
        ]);
        b.set_first(first);
        b.finish()
    };
    let whole = build(None);
    let nef = nef::parse(&whole).unwrap();
    assert_eq!(nef.maker.black, Some(1008));
    assert!(nef.maker.linearization.is_some());
    assert_eq!(nef::decode(&nef, &whole).unwrap().samples, samples);
    // A count past the file is refused before the note is read at all
    // (on a 32-bit host the extent overflows first, which is also refused).
    assert!(matches!(
        nef::parse(&build(Some(0xffff_ffff))).unwrap_err(),
        Error::Container(tiff::Error::OutOfFile { .. } | tiff::Error::Overflow)
    ));
    // A count too short for the `Nikon\0` prefix is no maker note.
    let bare = nef::parse(&build(Some(6))).unwrap();
    assert_eq!(bare.maker.black, None);
    assert_eq!(bare.maker.linearization, None);
    // A count that admits the header but cuts the IFD: the note's own
    // reader ends where the field ends, so the entries are out of file.
    assert!(matches!(
        nef::parse(&build(Some(20))).unwrap_err(),
        Error::Container(tiff::Error::OutOfFile { .. })
    ));
}

#[test]
fn hand_built_parameters_outside_the_sample_range_are_refused() {
    for max in [0, 1, nef::MAX_RANGE + 1, u32::MAX] {
        let params = HuffmanParams {
            tree: 0,
            vpred: [[0; 2]; 2],
            curve: None,
            max,
            split: 1,
        };
        assert!(
            matches!(
                nef::decode_huffman(&params, &[0; 16], 2, 2).unwrap_err(),
                Error::Linearization(_)
            ),
            "max {max}"
        );
    }
    // At the ceiling the split's widening still fits.
    let params = HuffmanParams {
        tree: 0,
        vpred: [[0; 2]; 2],
        curve: None,
        max: nef::MAX_RANGE,
        split: 1,
    };
    assert!(nef::decode_huffman(&params, &[0; 16], 2, 2).is_ok());
}

// ------------------------------------------------------------ the command

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("td-photo-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn run(args: &[&std::ffi::OsStr]) -> (bool, String, String) {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_td-photo"))
        .args(args)
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn the_command_line_probes_and_develops_without_overwriting() {
    use std::ffi::OsStr;
    let dir = scratch("cli");
    let (w, h) = (64, 32);
    let samples = random_frame(w, h, 14, 77);
    let file = dir.join("DSC_0001.NEF");
    let mut synth = Synth::lossless(w, h, &samples);
    // An orientation the pipeline does not turn is reported as such.
    synth.orientation = 5;
    std::fs::write(&file, synth.build()).unwrap();
    let original = std::fs::read(&file).unwrap();

    let (ok, stdout, stderr) = run(&[OsStr::new("probe"), file.as_os_str()]);
    assert!(ok, "{stderr}");
    assert!(stdout.contains("model: NIKON Z 8"), "{stdout}");
    assert!(
        stdout.contains("raw: 64x32 14-bit compression 34713 cfa RGGB"),
        "{stdout}"
    );
    assert!(stdout.contains("crop: 2,2 60x28\n"), "{stdout}");
    assert!(
        stdout.contains("orientation: 5 (treated as 1)\n"),
        "{stdout}"
    );
    // The plain form for an orientation the pipeline applies.
    let upright = dir.join("DSC_0002.NEF");
    std::fs::write(&upright, Synth::lossless(w, h, &samples).build()).unwrap();
    let (ok, up_stdout, _) = run(&[OsStr::new("probe"), upright.as_os_str()]);
    assert!(ok);
    assert!(up_stdout.contains("orientation: 1\n"), "{up_stdout}");
    assert!(
        stdout.contains(
            "huffman: tree 5 vpred [[2048, 2048], [2048, 2048]] max 16384 split 0 curve 0"
        ),
        "{stdout}"
    );
    let (ok, stdout, _) = run(&[
        OsStr::new("probe"),
        file.as_os_str(),
        OsStr::new("--decode"),
    ]);
    assert!(ok);
    assert!(
        stdout.contains("decode: 2048 samples, 0 corrupt"),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!("fnv1a64: {:#018x}", fnv1a64(&samples))),
        "{stdout}"
    );

    let out = dir.join("out.ppm");
    let (ok, stdout, stderr) = run(&[OsStr::new("develop"), file.as_os_str(), out.as_os_str()]);
    assert!(ok, "{stderr}");
    assert!(stdout.contains("developed 64x32 -> 30x14"), "{stdout}");
    let ppm = std::fs::read(&out).unwrap();
    assert!(ppm.starts_with(b"P6\n30 14\n255\n"));
    assert_eq!(ppm.len(), "P6\n30 14\n255\n".len() + 30 * 14 * 3);
    assert!(!dir.join("out.ppm.tmp").exists());

    // Never overwrites: an existing output and the input itself are refused
    // and left as they were.
    let (ok, _, stderr) = run(&[OsStr::new("develop"), file.as_os_str(), out.as_os_str()]);
    assert!(!ok);
    assert!(stderr.contains("already exists"), "{stderr}");
    assert_eq!(std::fs::read(&out).unwrap(), ppm);
    let (ok, _, stderr) = run(&[OsStr::new("develop"), file.as_os_str(), file.as_os_str()]);
    assert!(!ok);
    assert!(stderr.contains("already exists"), "{stderr}");
    assert_eq!(std::fs::read(&file).unwrap(), original);
    // A planted temporary is neither followed nor removed.
    let planted = dir.join("second.ppm.tmp");
    std::fs::write(&planted, b"mine").unwrap();
    let second = dir.join("second.ppm");
    let (ok, _, stderr) = run(&[OsStr::new("develop"), file.as_os_str(), second.as_os_str()]);
    assert!(!ok, "{stderr}");
    assert_eq!(std::fs::read(&planted).unwrap(), b"mine");
    assert!(!second.exists());

    // Options.
    let third = dir.join("third.ppm");
    let (ok, _, stderr) = run(&[
        OsStr::new("develop"),
        file.as_os_str(),
        third.as_os_str(),
        OsStr::new("--long-edge"),
        OsStr::new("16"),
        OsStr::new("--exposure"),
        OsStr::new("1"),
    ]);
    assert!(ok, "{stderr}");
    assert!(std::fs::read(&third)
        .unwrap()
        .starts_with(b"P6\n16 7\n255\n"));
    // A look: `--look` takes the built-in set, and `mono` collapses
    // every pixel to one value through the whole pipeline.
    let mono = dir.join("mono.ppm");
    let (ok, _, stderr) = run(&[
        OsStr::new("develop"),
        file.as_os_str(),
        mono.as_os_str(),
        OsStr::new("--long-edge"),
        OsStr::new("16"),
        OsStr::new("--look"),
        OsStr::new("mono"),
    ]);
    assert!(ok, "{stderr}");
    let ppm = std::fs::read(&mono).unwrap();
    let header = "P6\n16 7\n255\n";
    assert!(ppm.starts_with(header.as_bytes()));
    let pixels = &ppm[header.len()..];
    assert_eq!(pixels.len(), 16 * 7 * 3);
    assert!(pixels.chunks(3).all(|p| p[0] == p[1] && p[1] == p[2]));
    // A crop develops a sub-region: fewer pixels on each axis than the whole
    // frame at the same long edge, so the crop is applied (5(e)). The exact
    // oriented-region mapping is pinned in tests/develop.rs.
    let dims = |ppm: &[u8]| -> (usize, usize) {
        let nl = |from: usize| from + ppm[from..].iter().position(|&b| b == b'\n').unwrap();
        let first = nl(0);
        assert_eq!(&ppm[..first], b"P6");
        let second = nl(first + 1);
        let mut wh = std::str::from_utf8(&ppm[first + 1..second])
            .unwrap()
            .split(' ');
        let mut next = || wh.next().unwrap().parse().unwrap();
        (next(), next())
    };
    let whole = dir.join("whole.ppm");
    let (ok, _, stderr) = run(&[
        OsStr::new("develop"),
        file.as_os_str(),
        whole.as_os_str(),
        OsStr::new("--long-edge"),
        OsStr::new("40"),
    ]);
    assert!(ok, "{stderr}");
    let (fw, fh) = dims(&std::fs::read(&whole).unwrap());
    let cropped = dir.join("cropped.ppm");
    let (ok, _, stderr) = run(&[
        OsStr::new("develop"),
        file.as_os_str(),
        cropped.as_os_str(),
        OsStr::new("--long-edge"),
        OsStr::new("40"),
        OsStr::new("--crop"),
        OsStr::new("0.2000 0.3000 0.5000 0.4000"),
    ]);
    assert!(ok, "{stderr}");
    let (cw, ch) = dims(&std::fs::read(&cropped).unwrap());
    assert!(cw < fw && ch < fh, "crop {cw}x{ch} vs whole {fw}x{fh}");
    // A malformed crop is refused by name and writes nothing.
    let bad_crop = dir.join("bad-crop.ppm");
    let (ok, _, stderr) = run(&[
        OsStr::new("develop"),
        file.as_os_str(),
        bad_crop.as_os_str(),
        OsStr::new("--crop"),
        OsStr::new("not a crop"),
    ]);
    assert!(!ok);
    assert!(stderr.contains("is not X Y W H"), "{stderr}");
    assert!(!bad_crop.exists());
    let x = dir.join("x.ppm");
    let (ok, _, stderr) = run(&[
        OsStr::new("develop"),
        file.as_os_str(),
        x.as_os_str(),
        OsStr::new("--exposure"),
        OsStr::new("9"),
    ]);
    assert!(!ok);
    assert!(stderr.contains("-5..=5"), "{stderr}");
    let (ok, _, stderr) = run(&[
        OsStr::new("develop"),
        file.as_os_str(),
        x.as_os_str(),
        OsStr::new("--bogus"),
    ]);
    assert!(!ok);
    assert!(stderr.contains("unrecognized argument"), "{stderr}");
    assert!(!x.exists());
    let (ok, _, stderr) = run(&[OsStr::new("probe")]);
    assert!(!ok);
    assert!(stderr.contains("probe needs FILE"), "{stderr}");
    let (ok, _, stderr) = run(&[OsStr::new("develop"), file.as_os_str()]);
    assert!(!ok);
    assert!(
        stderr.contains("develop needs FILE and OUT.ppm"),
        "{stderr}"
    );
    let (ok, stdout, _) = run(&[OsStr::new("probe"), OsStr::new("--help")]);
    assert!(ok);
    assert!(stdout.starts_with("td-photo probe FILE"));
    // Not a regular file, and not a TIFF, are refused by name.
    let (ok, _, stderr) = run(&[OsStr::new("probe"), dir.as_os_str()]);
    assert!(!ok);
    assert!(stderr.contains("not a regular file"), "{stderr}");
    let (ok, _, stderr) = run(&[OsStr::new("probe"), out.as_os_str()]);
    assert!(!ok);
    assert!(stderr.contains("not a TIFF"), "{stderr}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// FNV-1a over bytes as they lie, for PPM payloads.
fn fnv1a64_bytes(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn run_env(args: &[&std::ffi::OsStr], env: &[(&str, &std::ffi::OsStr)]) -> (bool, String, String) {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_td-photo"));
    command.args(args);
    for (k, v) in env {
        command.env(k, v);
    }
    let out = command.output().unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn the_thumb_verb_and_the_cache_follow_the_rules() {
    use std::ffi::OsStr;
    let dir = scratch("thumb");
    let (w, h) = (16, 8);
    let samples = random_frame(w, h, 14, 88);
    let jpeg =
        std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/z8-thumb.jpg"))
            .unwrap();
    let mut synth = Synth::lossless(w, h, &samples);
    synth.preview_bytes = jpeg.clone();
    let file = dir.join("DSC_0007.NEF");
    std::fs::write(&file, synth.build()).unwrap();

    // Probe names the preview's geometry, and its hash under --decode.
    let (ok, stdout, _) = run(&[OsStr::new("probe"), file.as_os_str()]);
    assert!(ok);
    assert!(
        stdout.contains(" 160x120 3 component(s) sampling 2x1\n"),
        "{stdout}"
    );
    let (ok, stdout, _) = run(&[
        OsStr::new("probe"),
        file.as_os_str(),
        OsStr::new("--decode"),
    ]);
    assert!(ok);
    assert!(
        stdout.contains("preview-decode: 0 160x120 fnv1a64 0x15cf1df123ee575d"),
        "{stdout}"
    );

    // The default long edge is 400; a 160x120 preview is never enlarged.
    let out = dir.join("t400.ppm");
    let (ok, stdout, stderr) = run(&[OsStr::new("thumb"), file.as_os_str(), out.as_os_str()]);
    assert!(ok, "{stderr}");
    assert!(
        stdout.contains("thumbnail 160x120 from preview 0 (160x120)"),
        "{stdout}"
    );
    let full = std::fs::read(&out).unwrap();
    assert!(full.starts_with(b"P6\n160 120\n255\n"));
    assert_eq!(
        fnv1a64_bytes(&full["P6\n160 120\n255\n".len()..]),
        0x15cf1df123ee575d
    );
    // 40 is exactly the quarter scale; 50 is the half scale resampled.
    let out40 = dir.join("t40.ppm");
    let (ok, stdout, _) = run(&[
        OsStr::new("thumb"),
        file.as_os_str(),
        out40.as_os_str(),
        OsStr::new("--long-edge"),
        OsStr::new("40"),
    ]);
    assert!(ok);
    assert!(
        stdout.contains("thumbnail 40x30 from preview 0"),
        "{stdout}"
    );
    let quarter = std::fs::read(&out40).unwrap();
    assert_eq!(
        fnv1a64_bytes(&quarter["P6\n40 30\n255\n".len()..]),
        0x7c9a6f6b601504ab
    );
    let out50 = dir.join("t50.ppm");
    let (ok, stdout, _) = run(&[
        OsStr::new("thumb"),
        file.as_os_str(),
        out50.as_os_str(),
        OsStr::new("--long-edge"),
        OsStr::new("50"),
    ]);
    assert!(ok);
    assert!(
        stdout.contains("thumbnail 50x38 from preview 0"),
        "{stdout}"
    );

    // The cache: a miss fills it, a hit answers from it byte for byte,
    // and clear removes exactly the entries.
    let cache = dir.join("cache");
    let env = [("XDG_CACHE_HOME", cache.as_os_str())];
    let c1 = dir.join("c1.ppm");
    let (ok, stdout, stderr) = run_env(
        &[
            OsStr::new("thumb"),
            file.as_os_str(),
            c1.as_os_str(),
            OsStr::new("--cache"),
        ],
        &env,
    );
    assert!(ok, "{stderr}");
    assert!(stdout.contains("from preview 0"), "{stdout}");
    let thumbs = cache.join("td-photo").join("thumbs");
    let entries: Vec<String> = std::fs::read_dir(&thumbs)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert!(
        entries[0].ends_with("-400.ppm") && entries[0].len() == 16 + 8,
        "{entries:?}"
    );
    let c2 = dir.join("c2.ppm");
    let (ok, stdout, _) = run_env(
        &[
            OsStr::new("thumb"),
            file.as_os_str(),
            c2.as_os_str(),
            OsStr::new("--cache"),
        ],
        &env,
    );
    assert!(ok);
    assert!(stdout.contains("(cached;"), "{stdout}");
    assert_eq!(std::fs::read(&c1).unwrap(), std::fs::read(&c2).unwrap());
    assert_eq!(std::fs::read(&c1).unwrap(), full);
    // A malformed entry, or a well-formed one past the asked edge, is a
    // miss served from the file, never a failure, and the entry is
    // unlinked and refilled; a stale temporary blocks nothing.
    let mut oversize = b"P6\n401 1\n255\n".to_vec();
    oversize.resize(oversize.len() + 401 * 3, 7);
    let stale = thumbs.join(format!("{}.12345.tmp", entries[0]));
    std::fs::write(&stale, b"left by a killed process").unwrap();
    for (n, corrupt) in [&b"P6\n1 1\n255\nxy"[..], &oversize[..]].iter().enumerate() {
        std::fs::write(thumbs.join(&entries[0]), corrupt).unwrap();
        let c3 = dir.join(format!("c3-{n}.ppm"));
        let (ok, stdout, _) = run_env(
            &[
                OsStr::new("thumb"),
                file.as_os_str(),
                c3.as_os_str(),
                OsStr::new("--cache"),
            ],
            &env,
        );
        assert!(ok);
        assert!(stdout.contains("from preview 0"), "{stdout}");
        assert_eq!(std::fs::read(thumbs.join(&entries[0])).unwrap(), full);
        assert_eq!(std::fs::read(&c3).unwrap(), full);
    }
    assert!(stale.exists());
    // A stray file in the directory is not ours to remove.
    std::fs::write(thumbs.join("notes.txt"), b"keep").unwrap();
    let (ok, stdout, _) = run_env(&[OsStr::new("cache"), OsStr::new("path")], &env);
    assert!(ok);
    assert_eq!(stdout.trim(), cache.join("td-photo").to_string_lossy());
    let (ok, stdout, _) = run_env(&[OsStr::new("cache"), OsStr::new("clear")], &env);
    assert!(ok);
    assert!(stdout.starts_with("removed 2 thumbnails from"), "{stdout}");
    assert!(thumbs.join("notes.txt").exists());
    assert!(!thumbs.join(&entries[0]).exists());
    assert!(!stale.exists());
    let (ok, stdout, _) = run_env(&[OsStr::new("cache"), OsStr::new("clear")], &env);
    assert!(ok);
    assert!(stdout.starts_with("removed 0 thumbnails"), "{stdout}");
    // Without a cache directory at all, clear is a no-op that says so.
    let empty = dir.join("nowhere");
    let (ok, stdout, _) = run_env(
        &[OsStr::new("cache"), OsStr::new("clear")],
        &[("XDG_CACHE_HOME", empty.as_os_str())],
    );
    assert!(ok);
    assert!(stdout.starts_with("removed 0 thumbnails"), "{stdout}");
    // A relative XDG_CACHE_HOME is ignored in favour of HOME.
    let (ok, stdout, _) = run_env(
        &[OsStr::new("cache"), OsStr::new("path")],
        &[
            ("XDG_CACHE_HOME", OsStr::new("relative/dir")),
            ("HOME", dir.as_os_str()),
        ],
    );
    assert!(ok);
    assert_eq!(
        stdout.trim(),
        dir.join(".cache").join("td-photo").to_string_lossy()
    );

    // The cache's own directories must be real directories: a symlink
    // in their place is refused by `clear` (nothing behind it is touched)
    // and the thumbnail is still written, with a note, by `thumb`.
    let elsewhere = dir.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let theirs = elsewhere.join("0123456789abcdef-400.ppm");
    std::fs::write(&theirs, b"not ours").unwrap();
    std::fs::remove_dir_all(&thumbs).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &thumbs).unwrap();
    let (ok, _, stderr) = run_env(&[OsStr::new("cache"), OsStr::new("clear")], &env);
    assert!(!ok);
    assert!(
        stderr.contains("not a directory of the cache's own"),
        "{stderr}"
    );
    assert_eq!(std::fs::read(&theirs).unwrap(), b"not ours");
    let c4 = dir.join("c4.ppm");
    let (ok, stdout, stderr) = run_env(
        &[
            OsStr::new("thumb"),
            file.as_os_str(),
            c4.as_os_str(),
            OsStr::new("--cache"),
        ],
        &env,
    );
    assert!(ok, "{stderr}");
    assert!(stdout.contains("from preview 0"), "{stdout}");
    assert!(
        stderr.contains("td-photo: cache:") && stderr.contains("written without it"),
        "{stderr}"
    );
    assert_eq!(std::fs::read(&c4).unwrap(), full);
    assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 1);

    // The thumbnail is turned the way the camera was held.
    let mut turned = Synth::lossless(w, h, &samples);
    turned.preview_bytes = jpeg.clone();
    turned.orientation = 6;
    let portrait = dir.join("DSC_0009.NEF");
    std::fs::write(&portrait, turned.build()).unwrap();
    let out6 = dir.join("t6.ppm");
    let (ok, stdout, stderr) = run(&[OsStr::new("thumb"), portrait.as_os_str(), out6.as_os_str()]);
    assert!(ok, "{stderr}");
    assert!(
        stdout.contains("thumbnail 120x160 from preview 0 (160x120)"),
        "{stdout}"
    );
    let expected = td_photo::develop::orient(
        td_photo::jpeg::decode(&jpeg, td_photo::jpeg::Scale::Full).unwrap(),
        6,
    );
    let got = std::fs::read(&out6).unwrap();
    assert_eq!(&got["P6\n120 160\n255\n".len()..], &expected.data[..]);

    // Refusals.
    let (ok, _, stderr) = run(&[OsStr::new("thumb"), file.as_os_str(), out.as_os_str()]);
    assert!(!ok);
    assert!(stderr.contains("already exists"), "{stderr}");
    let (ok, _, stderr) = run(&[OsStr::new("thumb"), file.as_os_str()]);
    assert!(!ok);
    assert!(stderr.contains("thumb needs FILE and OUT.ppm"), "{stderr}");
    let (ok, _, stderr) = run(&[OsStr::new("cache")]);
    assert!(!ok);
    assert!(stderr.contains("cache needs path or clear"), "{stderr}");
    let (ok, _, stderr) = run(&[
        OsStr::new("thumb"),
        file.as_os_str(),
        dir.join("x.ppm").as_os_str(),
        OsStr::new("--long-edge"),
        OsStr::new("8"),
    ]);
    assert!(!ok);
    assert!(stderr.contains("16..=16384"), "{stderr}");
    // A file whose previews are not baseline JPEG has no thumbnail.
    let plain = dir.join("DSC_0008.NEF");
    std::fs::write(&plain, Synth::lossless(w, h, &samples).build()).unwrap();
    let (ok, _, stderr) = run(&[
        OsStr::new("thumb"),
        plain.as_os_str(),
        dir.join("p.ppm").as_os_str(),
    ]);
    assert!(!ok);
    assert!(stderr.contains("no baseline JPEG preview"), "{stderr}");
    let _ = std::fs::remove_dir_all(&dir);
}

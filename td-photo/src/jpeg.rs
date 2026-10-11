//! JPEG for td-photo: the thumbnail rule over td-jpeg's decoder, and a
//! baseline encoder for export (`Encoder`): YCbCr 4:4:4 over the standard
//! tables, fed rows and drained bytes a band at a time, its transform
//! td-jpeg's basis (`td_jpeg::T8`). Nothing here reads a file, the
//! environment or a clock.

use std::fmt;

use crate::image::{Rgb8, MAX_AXIS};
use td_jpeg::{decode, header, Scale, T8, ZIGZAG};

/// A thumbnail's refusal is the decoder's.
pub use td_jpeg::Error;

/// What the encoder refuses, each by name.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EncodeError {
    /// An axis of zero or past `MAX_AXIS`.
    Axis { width: usize, height: usize },
    /// Rows that are not the frame's width, or more or fewer of them than
    /// its height.
    Rows,
    /// A quantisation table missing from the encoder's own set.
    Table,
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Axis { width, height } => {
                write!(f, "JPEG encoder axes {width}x{height} out of range")
            }
            Self::Rows => f.write_str("JPEG encoder given rows that are not its width or height"),
            Self::Table => f.write_str("JPEG encoder table missing"),
        }
    }
}

impl std::error::Error for EncodeError {}

/// The stream decoded at the coarsest scale that still covers `long_edge`,
/// then area-resampled (in the encoded domain, never enlarged) to exactly
/// that long edge: the thumbnail rule.
pub fn thumbnail(data: &[u8], long_edge: usize, threads: usize) -> Result<Rgb8, Error> {
    let head = header(data)?;
    let scale = Scale::covering(head.width, head.height, long_edge);
    let image = decode(data, scale)?;
    let (dw, dh) = crate::develop::fit(image.width, image.height, long_edge);
    if (dw, dh) == (image.width, image.height) {
        return Ok(image);
    }
    let refused = Error::Axis {
        width: image.width,
        height: image.height,
    };
    let linear: Vec<f32> = image.data.iter().map(|v| f32::from(*v) / 255.0).collect();
    let small = crate::develop::resample(&linear, image.width, image.height, dw, dh, threads)
        .map_err(|_| refused.clone())?;
    let mut out = Rgb8::new(dw, dh).ok_or(refused)?;
    for (dst, src) in out.data.iter_mut().zip(small) {
        *dst = (src * 255.0 + 0.5).floor().clamp(0.0, 255.0) as u8;
    }
    Ok(out)
}

// ----------------------------------------------------------------- encode

/// The quality export writes at: the standard tables scaled the way every
/// encoder since IJG scales them (`scale = 200 - 2q` from 50 up), 4:4:4.
pub const QUALITY: u8 = 92;

/// T.81 Annex K.1, natural order.
const LUMA_QUANT: [u16; 64] = [
    16, 11, 10, 16, 24, 40, 51, 61, //
    12, 12, 14, 19, 26, 58, 60, 55, //
    14, 13, 16, 24, 40, 57, 69, 56, //
    14, 17, 22, 29, 51, 87, 80, 62, //
    18, 22, 37, 56, 68, 109, 103, 77, //
    24, 35, 55, 64, 81, 104, 113, 92, //
    49, 64, 78, 87, 103, 121, 120, 101, //
    72, 92, 95, 98, 112, 100, 103, 99,
];
const CHROMA_QUANT: [u16; 64] = [
    17, 18, 24, 47, 99, 99, 99, 99, //
    18, 21, 26, 66, 99, 99, 99, 99, //
    24, 26, 56, 99, 99, 99, 99, 99, //
    47, 66, 99, 99, 99, 99, 99, 99, //
    99, 99, 99, 99, 99, 99, 99, 99, //
    99, 99, 99, 99, 99, 99, 99, 99, //
    99, 99, 99, 99, 99, 99, 99, 99, //
    99, 99, 99, 99, 99, 99, 99, 99,
];

/// T.81 Annex K.3: the code counts per length and the symbols in code
/// order, as the DHT segment carries them.
const DC_LUMA_BITS: [u8; 16] = [0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0];
const DC_CHROMA_BITS: [u8; 16] = [0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0];
const DC_VALUES: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
const AC_LUMA_BITS: [u8; 16] = [0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7d];
const AC_LUMA_VALUES: [u8; 162] = [
    0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61, 0x07,
    0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xa1, 0x08, 0x23, 0x42, 0xb1, 0xc1, 0x15, 0x52, 0xd1, 0xf0,
    0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0a, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x25, 0x26, 0x27, 0x28,
    0x29, 0x2a, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49,
    0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69,
    0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89,
    0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7,
    0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5,
    0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe1, 0xe2,
    0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8,
    0xf9, 0xfa,
];
const AC_CHROMA_BITS: [u8; 16] = [0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 0x77];
const AC_CHROMA_VALUES: [u8; 162] = [
    0x00, 0x01, 0x02, 0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07, 0x61, 0x71,
    0x13, 0x22, 0x32, 0x81, 0x08, 0x14, 0x42, 0x91, 0xa1, 0xb1, 0xc1, 0x09, 0x23, 0x33, 0x52, 0xf0,
    0x15, 0x62, 0x72, 0xd1, 0x0a, 0x16, 0x24, 0x34, 0xe1, 0x25, 0xf1, 0x17, 0x18, 0x19, 0x1a, 0x26,
    0x27, 0x28, 0x29, 0x2a, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48,
    0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68,
    0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87,
    0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5,
    0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3,
    0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda,
    0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8,
    0xf9, 0xfa,
];

/// A quantisation table at `quality`, IJG's scaling of the standard one:
/// `5000 / q` below 50, `200 - 2q` from 50 up, each entry held to 1..=255
/// so the table stays 8-bit.
fn quant_table(base: &[u16; 64], quality: u8) -> [u16; 64] {
    let q = u32::from(quality.clamp(1, 100));
    let scale = if q < 50 { 5000 / q } else { 200 - 2 * q };
    base.map(|b| ((u32::from(b) * scale + 50) / 100).clamp(1, 255) as u16)
}

/// A symbol's code and length, by symbol; length 0 is a symbol the table
/// does not carry.
struct Codes {
    code: [u16; 256],
    len: [u8; 256],
}

/// Canonical codes from the counts per length (T.81 C.2), in symbol order.
fn codes(bits: &[u8; 16], values: &[u8]) -> Codes {
    let mut out = Codes {
        code: [0; 256],
        len: [0; 256],
    };
    let mut code = 0u32;
    let mut symbols = values.iter();
    for (i, count) in bits.iter().enumerate() {
        let len = i as u8 + 1;
        for _ in 0..*count {
            if let Some(&symbol) = symbols.next() {
                if let (Some(c), Some(l)) = (
                    out.code.get_mut(usize::from(symbol)),
                    out.len.get_mut(usize::from(symbol)),
                ) {
                    *c = code as u16;
                    *l = len;
                }
            }
            code += 1;
        }
        code <<= 1;
    }
    out
}

/// A value's category (the bits its magnitude takes) and the bits that
/// follow the category's code: the value itself, or one less than a
/// negative value in that many low bits (T.81 F.1.2.1).
fn magnitude(value: i32) -> (u8, u32) {
    let size = 32 - value.unsigned_abs().leading_zeros();
    let bits = if value < 0 {
        (value - 1) as u32 & ((1u32 << size) - 1)
    } else {
        value as u32
    };
    (size as u8, bits)
}

/// A baseline encoder the caller feeds interleaved 8-bit RGB rows, in any
/// number at a time, and drains bytes from as they are made, so a frame
/// of any size is coded a band at a time: JFIF, YCbCr 4:4:4, the standard
/// quantisation and Huffman tables, no restart intervals. The right edge
/// and the bottom are padded by replicating the last column and row. The
/// colour transform, transform and quantisation of each band run across
/// threads through `develop::bands`; the entropy coding is sequential.
pub struct Encoder {
    width: usize,
    height: usize,
    /// Rows received so far.
    taken: usize,
    /// Received rows not yet coded: fewer than eight between calls.
    pending: Vec<u8>,
    /// Quantisers by component (luma, chroma), natural order.
    quant: [[u16; 64]; 2],
    /// DC and AC codes by component (luma, chroma).
    dc: [Codes; 2],
    ac: [Codes; 2],
    /// The transform's basis in `f32`, `T8` as the decoder holds it.
    basis: [[f32; 8]; 8],
    pred: [i32; 3],
    acc: u64,
    acc_bits: u32,
    bytes: Vec<u8>,
    threads: usize,
    /// One band's quantised coefficients, reused: MCU order, then
    /// component, then natural order.
    coefficients: Vec<i16>,
}

impl Encoder {
    /// Begins a `width` by `height` stream at `quality` (1..=100, see
    /// `quant_table`): the headers are written into the pending bytes at
    /// once. Axes are `1..=MAX_AXIS`.
    pub fn new(
        width: usize,
        height: usize,
        quality: u8,
        threads: usize,
    ) -> Result<Self, EncodeError> {
        if width == 0 || height == 0 || width > MAX_AXIS || height > MAX_AXIS {
            return Err(EncodeError::Axis { width, height });
        }
        let quant = [
            quant_table(&LUMA_QUANT, quality),
            quant_table(&CHROMA_QUANT, quality),
        ];
        let mut basis = [[0.0f32; 8]; 8];
        for (row, src) in basis.iter_mut().zip(T8.iter()) {
            for (cell, value) in row.iter_mut().zip(src.iter()) {
                *cell = *value as f32;
            }
        }
        let mut encoder = Encoder {
            width,
            height,
            taken: 0,
            pending: Vec::new(),
            quant,
            dc: [
                codes(&DC_LUMA_BITS, &DC_VALUES),
                codes(&DC_CHROMA_BITS, &DC_VALUES),
            ],
            ac: [
                codes(&AC_LUMA_BITS, &AC_LUMA_VALUES),
                codes(&AC_CHROMA_BITS, &AC_CHROMA_VALUES),
            ],
            basis,
            pred: [0; 3],
            acc: 0,
            acc_bits: 0,
            bytes: Vec::new(),
            threads,
            coefficients: Vec::new(),
        };
        encoder.headers();
        Ok(encoder)
    }

    fn segment(&mut self, marker: u8, payload: &[u8]) {
        self.bytes.extend_from_slice(&[0xff, marker]);
        let len = (payload.len() + 2) as u16;
        self.bytes.extend_from_slice(&len.to_be_bytes());
        self.bytes.extend_from_slice(payload);
    }

    /// SOI, the JFIF APP0, both quantisers, SOF0, the four Huffman tables
    /// and SOS; the scan follows.
    fn headers(&mut self) {
        self.bytes.extend_from_slice(&[0xff, 0xd8]);
        self.segment(0xe0, b"JFIF\0\x01\x01\x00\x00\x01\x00\x01\x00\x00");
        let mut dqt = Vec::with_capacity(130);
        for (id, table) in self.quant.iter().enumerate() {
            dqt.push(id as u8);
            for &k in ZIGZAG.iter() {
                dqt.push(table.get(usize::from(k)).copied().unwrap_or(1) as u8);
            }
        }
        self.segment(0xdb, &dqt);
        let mut sof = vec![8u8];
        sof.extend_from_slice(&(self.height as u16).to_be_bytes());
        sof.extend_from_slice(&(self.width as u16).to_be_bytes());
        sof.extend_from_slice(&[3, 1, 0x11, 0, 2, 0x11, 1, 3, 0x11, 1]);
        self.segment(0xc0, &sof);
        let mut dht = Vec::with_capacity(4 * 17 + 2 * 12 + 2 * 162);
        for (class_id, bits, values) in [
            (0x00u8, &DC_LUMA_BITS, &DC_VALUES[..]),
            (0x10, &AC_LUMA_BITS, &AC_LUMA_VALUES[..]),
            (0x01, &DC_CHROMA_BITS, &DC_VALUES[..]),
            (0x11, &AC_CHROMA_BITS, &AC_CHROMA_VALUES[..]),
        ] {
            dht.push(class_id);
            dht.extend_from_slice(bits);
            dht.extend_from_slice(values);
        }
        self.segment(0xc4, &dht);
        self.segment(0xda, &[3, 1, 0x00, 2, 0x11, 3, 0x11, 0, 63, 0]);
    }

    /// The bytes made so far, handed over; the stream continues after them.
    pub fn take(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.bytes)
    }

    /// Feeds whole rows of interleaved RGB: their length must be a multiple
    /// of the row's, and the rows so far must not pass the height.
    pub fn encode_rows(&mut self, rows: &[u8]) -> Result<(), EncodeError> {
        let row_bytes = self.width * 3;
        if !rows.len().is_multiple_of(row_bytes) {
            return Err(EncodeError::Rows);
        }
        let count = rows.len() / row_bytes;
        if self.taken.saturating_add(count) > self.height {
            return Err(EncodeError::Rows);
        }
        self.taken += count;
        self.pending.extend_from_slice(rows);
        let groups = self.pending.len() / (row_bytes * 8);
        if groups > 0 {
            self.code_groups(groups)?;
            self.pending.drain(..groups * row_bytes * 8);
        }
        Ok(())
    }

    /// Codes the bottom rows (padded to a block by the last row), pads the
    /// bits and closes the stream; the rows fed must be exactly the height.
    pub fn finish(mut self) -> Result<Vec<u8>, EncodeError> {
        if self.taken != self.height {
            return Err(EncodeError::Rows);
        }
        let row_bytes = self.width * 3;
        let rows = self.pending.len() / row_bytes;
        if rows > 0 {
            let last = self
                .pending
                .get((rows - 1) * row_bytes..rows * row_bytes)
                .map(<[u8]>::to_vec);
            if let Some(last) = last {
                for _ in rows..8 {
                    self.pending.extend_from_slice(&last);
                }
            }
            self.code_groups(1)?;
            self.pending.clear();
        }
        // The last byte is padded with ones (T.81 F.1.2.3).
        if self.acc_bits > 0 {
            let pad = 8 - self.acc_bits;
            self.put(u32::MAX >> (32 - pad), pad);
        }
        self.bytes.extend_from_slice(&[0xff, 0xd9]);
        Ok(std::mem::take(&mut self.bytes))
    }

    fn put(&mut self, bits: u32, len: u32) {
        if len == 0 {
            return;
        }
        self.acc = (self.acc << len) | u64::from(bits & (u32::MAX >> (32 - len)));
        self.acc_bits += len;
        while self.acc_bits >= 8 {
            self.acc_bits -= 8;
            let byte = (self.acc >> self.acc_bits) as u8;
            self.bytes.push(byte);
            if byte == 0xff {
                self.bytes.push(0);
            }
        }
        self.acc &= (1u64 << self.acc_bits) - 1;
    }

    /// Transforms and quantises the first `groups` block rows of the
    /// pending rows across threads, then entropy-codes them in order. A
    /// symbol the tables do not carry (none is reachable: the coefficient
    /// clamp keeps every category within them) is `Table`, never a stream
    /// with an unprefixed run of bits.
    fn code_groups(&mut self, groups: usize) -> Result<(), EncodeError> {
        let width = self.width;
        let blocks_x = width.div_ceil(8);
        let per_group = blocks_x * 3 * 64;
        self.coefficients.clear();
        self.coefficients.resize(groups * per_group, 0);
        // Column runs of blocks, so a band of a few block rows still
        // spreads over the pool.
        const RUN: usize = 32;
        let runs_x = blocks_x.div_ceil(RUN);
        let mut items: Vec<(usize, usize, &mut [i16])> = Vec::with_capacity(groups * runs_x);
        let mut rest = self.coefficients.as_mut_slice();
        for group in 0..groups {
            for run in 0..runs_x {
                let blocks = RUN.min(blocks_x - run * RUN);
                let (chunk, tail) = rest.split_at_mut(blocks * 3 * 64);
                items.push((group, run * RUN, chunk));
                rest = tail;
            }
        }
        let pending = &self.pending;
        let quant = &self.quant;
        let basis = &self.basis;
        crate::develop::bands(items, self.threads, |(group, first_block, chunk)| {
            for (b, block3) in chunk.as_chunks_mut::<{ 3 * 64 }>().0.iter_mut().enumerate() {
                let bx = first_block + b;
                let mut ycc = [[[0.0f32; 8]; 8]; 3];
                for y in 0..8 {
                    let row = (group * 8 + y) * width * 3;
                    for x in 0..8 {
                        let px = (bx * 8 + x).min(width - 1);
                        let at = row + px * 3;
                        let [r, g, b] = pending
                            .get(at..at + 3)
                            .and_then(|s| s.as_chunks::<3>().0.first())
                            .copied()
                            .unwrap_or([0, 0, 0]);
                        let (r, g, b) = (f32::from(r), f32::from(g), f32::from(b));
                        let luma = 0.299 * r + 0.587 * g + 0.114 * b - 128.0;
                        let cb = -0.168_736 * r - 0.331_264 * g + 0.5 * b;
                        let cr = 0.5 * r - 0.418_688 * g - 0.081_312 * b;
                        for (plane, value) in ycc.iter_mut().zip([luma, cb, cr]) {
                            if let Some(slot) = plane.get_mut(y).and_then(|row| row.get_mut(x)) {
                                *slot = value;
                            }
                        }
                    }
                }
                for (component, (samples, out)) in ycc
                    .iter()
                    .zip(block3.as_chunks_mut::<64>().0.iter_mut())
                    .enumerate()
                {
                    if let Some(table) = quant.get(component.min(1)) {
                        forward(basis, samples, table, out);
                    }
                }
            }
        });
        // Taken out of `self` for the walk, since coding borrows the
        // encoder mutably, and put back to be reused.
        let coefficients = std::mem::take(&mut self.coefficients);
        let mut coded = Ok(());
        for (block_index, block) in coefficients.as_chunks::<64>().0.iter().enumerate() {
            coded = self.code_block(block_index % 3, block);
            if coded.is_err() {
                break;
            }
        }
        self.coefficients = coefficients;
        coded
    }

    /// One block's DC difference and AC run/size pairs.
    fn code_block(&mut self, component: usize, block: &[i16; 64]) -> Result<(), EncodeError> {
        let tables = component.min(1);
        let dc = i32::from(block.first().copied().unwrap_or(0));
        let pred = self.pred.get(component).copied().unwrap_or(0);
        if let Some(slot) = self.pred.get_mut(component) {
            *slot = dc;
        }
        let (size, bits) = magnitude(dc - pred);
        let (code, len) = self.lookup(&self.dc, tables, size)?;
        self.put(code, len);
        self.put(bits, u32::from(size));
        let mut run = 0u32;
        for &k in ZIGZAG.iter().skip(1) {
            let value = i32::from(block.get(usize::from(k)).copied().unwrap_or(0));
            if value == 0 {
                run += 1;
                continue;
            }
            while run > 15 {
                let (code, len) = self.lookup(&self.ac, tables, 0xf0)?;
                self.put(code, len);
                run -= 16;
            }
            let (size, bits) = magnitude(value);
            let (code, len) = self.lookup(&self.ac, tables, (run as u8) << 4 | size)?;
            self.put(code, len);
            self.put(bits, u32::from(size));
            run = 0;
        }
        if run > 0 {
            let (code, len) = self.lookup(&self.ac, tables, 0x00)?;
            self.put(code, len);
        }
        Ok(())
    }

    /// A symbol's code and length; a symbol the table does not carry is
    /// `Table`.
    fn lookup(
        &self,
        tables: &[Codes; 2],
        which: usize,
        symbol: u8,
    ) -> Result<(u32, u32), EncodeError> {
        let table = tables.get(which).ok_or(EncodeError::Table)?;
        let len = table.len.get(usize::from(symbol)).copied().unwrap_or(0);
        if len == 0 {
            return Err(EncodeError::Table);
        }
        let code = table.code.get(usize::from(symbol)).copied().unwrap_or(0);
        Ok((u32::from(code), u32::from(len)))
    }
}

/// The forward transform of one level-shifted block, quantised by `table`
/// into `out` (64 coefficients, natural order): `basis` is `T8`, so
/// `F[v][u] = sum_y T8[y][v] sum_x T8[x][u] f[y][x]`, the inverse of the
/// decoder's sums. Coefficients are held to baseline's range: the DC to
/// -1024..=1023 (so a difference fits category 11) and each AC to
/// -1023..=1023 (size 10), the categories the standard tables carry.
fn forward(basis: &[[f32; 8]; 8], samples: &[[f32; 8]; 8], table: &[u16; 64], out: &mut [i16]) {
    // tmp[v][x]: the column transform of column x.
    let mut tmp = [[0.0f32; 8]; 8];
    for (v, tmp_v) in tmp.iter_mut().enumerate() {
        for (x, slot) in tmp_v.iter_mut().enumerate() {
            let mut sum = 0.0f32;
            for (row, ty) in samples.iter().zip(basis.iter()) {
                sum += ty.get(v).copied().unwrap_or(0.0) * row.get(x).copied().unwrap_or(0.0);
            }
            *slot = sum;
        }
    }
    for (v, tmp_v) in tmp.iter().enumerate() {
        for u in 0..8 {
            let mut sum = 0.0f32;
            for (tx, value) in basis.iter().zip(tmp_v.iter()) {
                sum += tx.get(u).copied().unwrap_or(0.0) * value;
            }
            let q = f32::from(table.get(v * 8 + u).copied().unwrap_or(1));
            let floor = if v == 0 && u == 0 { -1024.0 } else { -1023.0 };
            let level = (sum / q).round().clamp(floor, 1023.0) as i16;
            if let Some(slot) = out.get_mut(v * 8 + u) {
                *slot = level;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use super::*;

    /// Every baseline AC symbol once (EOB, ZRL and run 0..16 by size
    /// 1..=10), every DC category once, the counts summing to the values,
    /// and no code of all ones, which is what the decoder refuses.
    #[test]
    fn the_standard_tables_are_complete_and_prefix_free() {
        for (bits, values, count) in [
            (&AC_LUMA_BITS, &AC_LUMA_VALUES[..], 162usize),
            (&AC_CHROMA_BITS, &AC_CHROMA_VALUES[..], 162),
            (&DC_LUMA_BITS, &DC_VALUES[..], 12),
            (&DC_CHROMA_BITS, &DC_VALUES[..], 12),
        ] {
            assert_eq!(bits.iter().map(|b| usize::from(*b)).sum::<usize>(), count);
            assert_eq!(values.len(), count);
            let mut seen = std::collections::BTreeSet::new();
            for v in values {
                assert!(seen.insert(*v), "{v:#04x} twice");
            }
            let expected: std::collections::BTreeSet<u8> = if count == 162 {
                [0x00u8, 0xf0]
                    .into_iter()
                    .chain((0..16u8).flat_map(|run| (1..=10u8).map(move |size| run << 4 | size)))
                    .collect()
            } else {
                (0..12u8).collect()
            };
            assert_eq!(seen, expected);
            let table = codes(bits, values);
            for v in values {
                let (code, len) = (table.code[usize::from(*v)], table.len[usize::from(*v)]);
                assert!(len > 0 && len <= 16);
                assert_ne!(u32::from(code), (1u32 << len) - 1, "all ones");
            }
        }
    }

    #[test]
    fn quality_scales_the_standard_tables_like_ijg() {
        assert_eq!(quant_table(&LUMA_QUANT, 50)[0], 16);
        assert_eq!(quant_table(&LUMA_QUANT, 100)[0], 1);
        assert_eq!(quant_table(&LUMA_QUANT, 92)[0], 3);
        assert_eq!(quant_table(&CHROMA_QUANT, 92)[63], 16);
        assert_eq!(quant_table(&LUMA_QUANT, 1)[63], 255);
        assert_eq!(quant_table(&LUMA_QUANT, 0), quant_table(&LUMA_QUANT, 1));
        assert_eq!(quant_table(&LUMA_QUANT, 200), quant_table(&LUMA_QUANT, 100));
    }

    /// The clamp in `forward`, reached with samples past the level shift's
    /// range: the DC floor is -1024 and the AC bound 1023 either way, so
    /// every coefficient has a category the standard tables carry.
    #[test]
    fn the_transform_holds_coefficients_to_the_tables_categories() {
        let mut basis = [[0.0f32; 8]; 8];
        for (row, src) in basis.iter_mut().zip(T8.iter()) {
            for (cell, value) in row.iter_mut().zip(src.iter()) {
                *cell = *value as f32;
            }
        }
        let ones = [1u16; 64];
        let mut out = [0i16; 64];
        forward(&basis, &[[-1000.0f32; 8]; 8], &ones, &mut out);
        assert_eq!(out[0], -1024);
        assert!(out[1..].iter().all(|&v| v == 0));
        forward(&basis, &[[1000.0f32; 8]; 8], &ones, &mut out);
        assert_eq!(out[0], 1023);
        // A checkerboard: the odd frequencies saturate at the AC bound, all
        // of them positive; its negative saturates at the floor.
        let mut board = [[0.0f32; 8]; 8];
        for (y, row) in board.iter_mut().enumerate() {
            for (x, v) in row.iter_mut().enumerate() {
                *v = if (x + y) % 2 == 0 { 1000.0 } else { -1000.0 };
            }
        }
        for sign in [1.0f32, -1.0] {
            let signed = board.map(|row| row.map(|v| v * sign));
            forward(&basis, &signed, &ones, &mut out);
            assert_eq!(out[0], 0);
            assert!(out.iter().any(|&v| v == 1023 * sign as i16));
            assert!(out.iter().all(|&v| (-1023..=1023).contains(&v)));
            for v in out {
                assert!(magnitude(i32::from(v)).0 <= 10);
            }
        }
        assert_eq!(magnitude(-1024 - 1023).0, 11);
    }

    #[test]
    fn magnitudes_follow_f_1_2_1() {
        assert_eq!(magnitude(0), (0, 0));
        assert_eq!(magnitude(1), (1, 1));
        assert_eq!(magnitude(-1), (1, 0));
        assert_eq!(magnitude(2), (2, 2));
        assert_eq!(magnitude(-2), (2, 1));
        assert_eq!(magnitude(-3), (2, 0));
        assert_eq!(magnitude(1023), (10, 1023));
        assert_eq!(magnitude(-1023), (10, 0));
    }
}

//! An AV1 still-picture encoder for the AVIF export: one key frame,
//! 8-bit 4:2:0, intra prediction only, written the way the AV1 bitstream
//! specification (version 1.0.0 with errata) reads it back, so any
//! decoder reconstructs exactly the picture the encoder reconstructs for
//! itself. The stream is a sequence header OBU and a frame OBU; `avif`
//! wraps them. Fed rows like `jpeg::Encoder`, a superblock row at a time.
//!
//! What the encoder uses of AV1: 64x64 superblocks split down to square
//! blocks of 32, 16 and 8 pixels by rate-distortion choice; every intra
//! mode but CfL, palette and filter-intra, with the mode's default
//! transform type from the reduced set; one transform per plane per
//! block; the multi-symbol arithmetic coder with adapting CDFs from the
//! defaults; uniform tile columns by the frame's size alone, which the
//! threads spread over. Loop filter, CDEF, restoration, superres, film grain and
//! screen-content tools stay off, so the decoder side is the smallest AV1
//! has. `transform` holds the transforms, quantizers and scans, `cdf` the
//! default CDFs.

use std::fmt;

use crate::cdf;
use crate::transform::{self, Size, TxType};

/// The longest axis: what `image::MAX_AXIS` allows, which the level
/// rules and the 16-bit frame size fields also hold.
pub const MAX_AXIS: usize = crate::image::MAX_AXIS;

/// Superblock side in luma pixels.
const SB: usize = 64;
/// A mode-info unit: the spec's `MI_SIZE`.
const MI: usize = 4;
/// Superblock side in mode-info units.
const SB_MI: usize = SB / MI;
/// The spec's `MAX_TILE_WIDTH`, `MAX_TILE_AREA`, `MAX_TILE_COLS/ROWS`.
const MAX_TILE_WIDTH: usize = 4096;
const MAX_TILE_AREA: usize = 4096 * 2304;
const MAX_TILE_COLS: usize = 64;
const MAX_TILE_ROWS: usize = 64;

/// Why the encoder refuses.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// An axis of zero, or past `MAX_AXIS`.
    Axis { width: usize, height: usize },
    /// Rows fed that are not whole, or more than the height, or fewer
    /// at the finish.
    Rows,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Axis { width, height } => write!(f, "AV1 refuses a {width}x{height} frame"),
            Self::Rows => f.write_str("rows fed to the AV1 encoder do not fit its frame"),
        }
    }
}

impl std::error::Error for Error {}

// ------------------------------------------------------------ modes

/// The spec's intra modes by number.
const DC_PRED: u8 = 0;
const V_PRED: u8 = 1;
const H_PRED: u8 = 2;
const D45_PRED: u8 = 3;
const D135_PRED: u8 = 4;
const D113_PRED: u8 = 5;
const D157_PRED: u8 = 6;
const D203_PRED: u8 = 7;
const D67_PRED: u8 = 8;
const SMOOTH_PRED: u8 = 9;
const SMOOTH_V_PRED: u8 = 10;
const SMOOTH_H_PRED: u8 = 11;
const PAETH_PRED: u8 = 12;
/// How many of the screened modes are coded in full, luma and chroma.
const LUMA_TRIALS: usize = 3;
const CHROMA_TRIALS: usize = 2;
/// Every mode the encoder tries, luma and chroma alike.
const MODES: [u8; 13] = [
    DC_PRED,
    V_PRED,
    H_PRED,
    D45_PRED,
    D135_PRED,
    D113_PRED,
    D157_PRED,
    D203_PRED,
    D67_PRED,
    SMOOTH_PRED,
    SMOOTH_V_PRED,
    SMOOTH_H_PRED,
    PAETH_PRED,
];

/// The spec's `Mode_To_Angle`.
fn mode_angle(mode: u8) -> i32 {
    match mode {
        V_PRED => 90,
        H_PRED => 180,
        D45_PRED => 45,
        D135_PRED => 135,
        D113_PRED => 113,
        D157_PRED => 157,
        D203_PRED => 203,
        D67_PRED => 67,
        _ => 0,
    }
}

fn is_directional(mode: u8) -> bool {
    (V_PRED..=D67_PRED).contains(&mode)
}

/// The spec's `Intra_Mode_Context`.
fn mode_context(mode: u8) -> usize {
    match mode {
        DC_PRED | SMOOTH_PRED | PAETH_PRED => 0,
        V_PRED | SMOOTH_V_PRED => 1,
        H_PRED | SMOOTH_H_PRED => 2,
        D45_PRED | D67_PRED => 3,
        _ => 4,
    }
}

/// The spec's `Mode_To_Txfm`, the transform a mode's residual gets
/// when the size admits one other than DCT.
fn mode_tx_type(mode: u8, size: Size) -> TxType {
    if size == Size::S32 {
        return TxType::DctDct;
    }
    match mode {
        V_PRED | D113_PRED | D67_PRED | SMOOTH_V_PRED => TxType::AdstDct,
        H_PRED | D157_PRED | D203_PRED | SMOOTH_H_PRED => TxType::DctAdst,
        D135_PRED | SMOOTH_PRED | PAETH_PRED => TxType::AdstAdst,
        _ => TxType::DctDct,
    }
}

/// The spec's `Dr_Intra_Derivative` by angle.
#[rustfmt::skip]
const DR_INTRA_DERIVATIVE: [i32; 90] = [
    0, 0, 0, 1023, 0, 0, 547, 0, 0, 372, 0, 0, 0, 0, 273, 0, 0, 215, 0, 0,
    178, 0, 0, 151, 0, 0, 132, 0, 0, 116, 0, 0, 102, 0, 0, 0, 90, 0, 0, 80,
    0, 0, 71, 0, 0, 64, 0, 0, 57, 0, 0, 51, 0, 0, 45, 0, 0, 0, 40, 0, 0, 35,
    0, 0, 31, 0, 0, 27, 0, 0, 23, 0, 0, 19, 0, 0, 15, 0, 0, 0, 0, 11, 0, 0,
    7, 0, 0, 3, 0, 0,
];

fn derivative(angle: i32) -> i32 {
    usize::try_from(angle)
        .ok()
        .and_then(|a| DR_INTRA_DERIVATIVE.get(a))
        .copied()
        .unwrap_or(1)
}

/// The spec's `Sm_Weights` by block side.
#[rustfmt::skip]
const SM_WEIGHTS: [u8; 60] = [
    255, 149, 85, 64,
    255, 197, 146, 105, 73, 50, 37, 32,
    255, 225, 196, 170, 145, 123, 102, 84, 68, 54, 43, 33, 26, 20, 17, 16,
    255, 240, 225, 210, 196, 182, 169, 157, 145, 133, 122, 111, 101, 92, 83,
    74, 66, 59, 52, 45, 39, 34, 29, 25, 21, 17, 14, 12, 10, 9, 8, 8,
];

fn sm_weights(n: usize) -> &'static [u8] {
    let start = match n {
        4 => 0,
        8 => 4,
        16 => 12,
        _ => 28,
    };
    SM_WEIGHTS.get(start..start + n).unwrap_or(&[])
}

// -------------------------------------------------------------- CDFs

/// One cumulative distribution in the spec's form: `v[i]` is 32768
/// times the probability of a symbol up to `i`, `v[n - 1]` 32768, and
/// `v[n]` the adaptation count.
#[derive(Clone, Copy)]
struct Cdf {
    n: usize,
    v: [u16; 17],
}

impl Cdf {
    fn new<const M: usize>(values: &[u16; M]) -> Cdf {
        let mut v = [0u16; 17];
        for (slot, value) in v.iter_mut().zip(values.iter()) {
            *slot = *value;
        }
        if let Some(top) = v.get_mut(M) {
            *top = 32768;
        }
        Cdf { n: M + 1, v }
    }

    fn at(&self, i: usize) -> u32 {
        u32::from(self.v.get(i).copied().unwrap_or(32768))
    }

    /// The spec's symbol adaptation process.
    fn adapt(&mut self, symbol: usize) {
        let n = self.n;
        let count = self.at(n);
        let rate = 3 + u32::from(count > 15) + u32::from(count > 31) + (n.ilog2()).min(2);
        let mut tmp = 0u32;
        for i in 0..n - 1 {
            if i == symbol {
                tmp = 32768;
            }
            let Some(slot) = self.v.get_mut(i) else {
                break;
            };
            let value = u32::from(*slot);
            *slot = if tmp < value {
                (value - ((value - tmp) >> rate)) as u16
            } else {
                (value + ((tmp - value) >> rate)) as u16
            };
        }
        if let Some(slot) = self.v.get_mut(n) {
            *slot += u16::from(count < 32);
        }
    }

    /// The bits a symbol costs, in 1/512 bit, from libaom's table.
    fn cost(&self, symbol: usize) -> u32 {
        let low = if symbol == 0 { 0 } else { self.at(symbol - 1) };
        let p = self.at(symbol).saturating_sub(low).clamp(1, 32767);
        let shift = 14 - p.ilog2();
        let prob = (p << shift) >> 7;
        PROB_COST
            .get((prob as usize).saturating_sub(128))
            .map_or(0, |c| u32::from(*c))
            + shift * 512
    }
}

/// `round(-log2(i / 256) * 512)` for `i` of 128 to 255.
#[rustfmt::skip]
const PROB_COST: [u16; 128] = [
    512, 506, 501, 495, 489, 484, 478, 473, 467, 462, 456, 451, 446, 441, 435,
    430, 425, 420, 415, 410, 405, 400, 395, 390, 385, 380, 375, 371, 366, 361,
    356, 352, 347, 343, 338, 333, 329, 324, 320, 316, 311, 307, 302, 298, 294,
    289, 285, 281, 277, 273, 268, 264, 260, 256, 252, 248, 244, 240, 236, 232,
    228, 224, 220, 216, 212, 209, 205, 201, 197, 194, 190, 186, 182, 179, 175,
    171, 168, 164, 161, 157, 153, 150, 146, 143, 139, 136, 132, 129, 125, 122,
    119, 115, 112, 109, 105, 102, 99, 95, 92, 89, 86, 82, 79, 76, 73,
    70, 66, 63, 60, 57, 54, 51, 48, 45, 42, 38, 35, 32, 29, 26,
    23, 20, 18, 15, 12, 9, 6, 3,
];

/// Every CDF a tile adapts, from the defaults of the frame's quantizer
/// band.
struct Cdfs {
    y_mode: [[Cdf; 5]; 5],
    uv_mode: [Cdf; 13],
    angle_delta: [Cdf; 8],
    partition_8: [Cdf; 4],
    partition: [[Cdf; 4]; 3],
    skip: [Cdf; 3],
    tx_type: [[Cdf; 13]; 4],
    txb_skip: [[Cdf; 13]; 5],
    eob_extra: [[[Cdf; 9]; 2]; 5],
    dc_sign: [[Cdf; 3]; 2],
    base_eob: [[[Cdf; 4]; 2]; 5],
    base: [[[Cdf; 42]; 2]; 5],
    br: [[[Cdf; 21]; 2]; 5],
    /// By class (16, 64, 256, 1024) then plane type.
    eob_pt: [[Cdf; 2]; 4],
}

fn table<const M: usize, const K: usize>(source: &[[u16; M]; K]) -> [Cdf; K] {
    let mut out = [Cdf::new(&[0u16; M]); K];
    for (o, s) in out.iter_mut().zip(source.iter()) {
        *o = Cdf::new(s);
    }
    out
}

fn table2<const M: usize, const K: usize, const J: usize>(
    source: &[[[u16; M]; K]; J],
) -> [[Cdf; K]; J] {
    let mut out = [[Cdf::new(&[0u16; M]); K]; J];
    for (o, s) in out.iter_mut().zip(source.iter()) {
        *o = table(s);
    }
    out
}

fn table3<const M: usize, const K: usize, const J: usize, const I: usize>(
    source: &[[[[u16; M]; K]; J]; I],
) -> [[[Cdf; K]; J]; I] {
    let mut out = [[[Cdf::new(&[0u16; M]); K]; J]; I];
    for (o, s) in out.iter_mut().zip(source.iter()) {
        *o = table2(s);
    }
    out
}

impl Cdfs {
    /// The defaults for a `base_q_idx` (spec `get_q_ctx`).
    fn new(qindex: u8) -> Cdfs {
        let band = match qindex {
            0..=20 => 0,
            21..=60 => 1,
            61..=120 => 2,
            _ => 3,
        };
        let uv: [Cdf; 13] = table(&cdf::UV_MODE_CFL);
        Cdfs {
            y_mode: table2(&cdf::KF_Y_MODE),
            uv_mode: uv,
            angle_delta: table(&cdf::ANGLE_DELTA),
            partition_8: table(&cdf::PARTITION_8),
            partition: table2(&cdf::PARTITION),
            skip: table(&cdf::SKIP),
            tx_type: table2(&cdf::INTRA_TX_SET2),
            txb_skip: table2(cdf::TXB_SKIP.get(band).unwrap_or(&cdf::TXB_SKIP[0])),
            eob_extra: table3(cdf::EOB_EXTRA.get(band).unwrap_or(&cdf::EOB_EXTRA[0])),
            dc_sign: table2(cdf::DC_SIGN.get(band).unwrap_or(&cdf::DC_SIGN[0])),
            base_eob: table3(
                cdf::COEFF_BASE_EOB
                    .get(band)
                    .unwrap_or(&cdf::COEFF_BASE_EOB[0]),
            ),
            base: table3(cdf::COEFF_BASE.get(band).unwrap_or(&cdf::COEFF_BASE[0])),
            br: table3(cdf::COEFF_BR.get(band).unwrap_or(&cdf::COEFF_BR[0])),
            eob_pt: [
                eob_class(cdf::EOB_PT_16.get(band).unwrap_or(&cdf::EOB_PT_16[0])),
                eob_class(cdf::EOB_PT_64.get(band).unwrap_or(&cdf::EOB_PT_64[0])),
                eob_class(cdf::EOB_PT_256.get(band).unwrap_or(&cdf::EOB_PT_256[0])),
                eob_class(cdf::EOB_PT_1024.get(band).unwrap_or(&cdf::EOB_PT_1024[0])),
            ],
        }
    }
}

/// The 2D-class entry of an `eob_pt` table, per plane type.
fn eob_class<const M: usize>(source: &[[[u16; M]; 2]; 2]) -> [Cdf; 2] {
    [Cdf::new(&source[0][0]), Cdf::new(&source[1][0])]
}

// ------------------------------------------------------------- coder

/// The spec's multi-symbol arithmetic coder, written from libaom's
/// encoder: a 16-bit range, a carry-propagated byte queue and the
/// terminating bit `exit_symbol` requires.
struct Coder {
    low: u64,
    rng: u32,
    cnt: i32,
    precarry: Vec<u16>,
}

impl Coder {
    fn new() -> Coder {
        Coder {
            low: 0,
            rng: 0x8000,
            cnt: -9,
            precarry: Vec::new(),
        }
    }

    /// One symbol of `nsyms` with the inverse cumulative bounds `fl`
    /// (32768 for the first symbol) and `fh`.
    fn encode(&mut self, fl: u32, fh: u32, s: usize, nsyms: usize) {
        let mut l = self.low;
        let r = self.rng;
        let n = (nsyms - 1) as u32;
        let s = s as u32;
        let scaled = |f: u32| ((r >> 8) * (f >> 6)) >> 1;
        let r = if fl < 32768 {
            let u = scaled(fl) + 4 * (n + 1 - s);
            let v = scaled(fh) + 4 * (n - s);
            l += u64::from(r - u);
            u - v
        } else {
            r - (scaled(fh) + 4 * (n - s))
        };
        self.normalize(l, r);
    }

    fn normalize(&mut self, mut low: u64, rng: u32) {
        let d = 16 - (32 - rng.leading_zeros()) as i32;
        let mut c = self.cnt;
        let mut s = c + d;
        if s >= 0 {
            c += 16;
            let mut m = (1u64 << c) - 1;
            if s >= 8 {
                self.precarry.push((low >> c) as u16);
                low &= m;
                c -= 8;
                m >>= 8;
            }
            self.precarry.push((low >> c) as u16);
            s = c + d - 24;
            low &= m;
        }
        self.low = low << d;
        self.rng = rng << d;
        self.cnt = s;
    }

    /// One symbol under an adapting CDF.
    fn symbol(&mut self, cdf: &mut Cdf, s: usize) {
        let fl = if s == 0 { 32768 } else { 32768 - cdf.at(s - 1) };
        let fh = 32768 - cdf.at(s);
        self.encode(fl, fh, s, cdf.n);
        cdf.adapt(s);
    }

    /// One equiprobable bit (spec `L(1)`).
    fn bit(&mut self, bit: bool) {
        if bit {
            self.encode(16384, 0, 1, 2);
        } else {
            self.encode(32768, 16384, 0, 2);
        }
    }

    /// The bytes, with the terminating one bit and zero padding.
    fn finish(mut self) -> Vec<u8> {
        let mut c = self.cnt;
        let m = 0x3FFFu64;
        let mut e = ((self.low + m) & !m) | (m + 1);
        let mut s = 10 + c;
        if s > 0 {
            let mut n = (1u64 << (c + 16)) - 1;
            loop {
                self.precarry.push((e >> (c + 16)) as u16);
                e &= n;
                s -= 8;
                c -= 8;
                n >>= 8;
                if s <= 0 {
                    break;
                }
            }
        }
        let mut out = vec![0u8; self.precarry.len()];
        let mut carry = 0u32;
        for (o, p) in out.iter_mut().zip(self.precarry.iter()).rev() {
            let v = u32::from(*p) + carry;
            *o = v as u8;
            carry = v >> 8;
        }
        out
    }
}

/// Where symbols go: the coder, or a counter that prices them.
trait Sink {
    fn symbol(&mut self, cdf: &mut Cdf, s: usize);
    fn bit(&mut self, bit: bool);
}

impl Sink for Coder {
    fn symbol(&mut self, cdf: &mut Cdf, s: usize) {
        Coder::symbol(self, cdf, s);
    }

    fn bit(&mut self, bit: bool) {
        Coder::bit(self, bit);
    }
}

/// The rate of what passes, in 1/512 bit, the CDFs left as they are.
struct Counter(u64);

impl Sink for Counter {
    fn symbol(&mut self, cdf: &mut Cdf, s: usize) {
        self.0 += u64::from(cdf.cost(s));
    }

    fn bit(&mut self, _bit: bool) {
        self.0 += 512;
    }
}

// --------------------------------------------------------- bit writer

/// Header bits, most significant first (spec `f(n)`).
struct Bits {
    bytes: Vec<u8>,
    used: u32,
}

impl Bits {
    fn new() -> Bits {
        Bits {
            bytes: Vec::new(),
            used: 0,
        }
    }

    fn put(&mut self, n: u32, value: u32) {
        for i in (0..n).rev() {
            let bit = ((value >> i) & 1) as u8;
            if self.used == 0 {
                self.bytes.push(0);
            }
            if let Some(last) = self.bytes.last_mut() {
                *last |= bit << (7 - self.used);
            }
            self.used = (self.used + 1) % 8;
        }
    }

    fn flag(&mut self, on: bool) {
        self.put(1, u32::from(on));
    }

    /// `byte_alignment()`: zero bits to the boundary.
    fn align(&mut self) {
        while self.used != 0 {
            self.put(1, 0);
        }
    }

    /// `trailing_bits()`: a one then zeros to the boundary.
    fn trailing(&mut self) {
        self.put(1, 1);
        self.align();
    }
}

fn leb128(out: &mut Vec<u8>, mut value: usize) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// One OBU with a size field: the header byte, `leb128` size, payload.
fn obu(out: &mut Vec<u8>, kind: u8, payload: &[u8]) {
    out.push((kind << 3) | 2);
    leb128(out, payload.len());
    out.extend_from_slice(payload);
}

const OBU_SEQUENCE_HEADER: u8 = 1;
const OBU_FRAME: u8 = 6;

// ---------------------------------------------------------- geometry

/// The frame's grid and its tiling.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Geometry {
    width: usize,
    height: usize,
    /// Mode-info columns and rows: the frame rounded up to 8 pixels.
    mi_cols: usize,
    mi_rows: usize,
    sb_cols: usize,
    sb_rows: usize,
    tile_cols_log2: u32,
    tile_rows_log2: u32,
    /// Tile edges in superblocks, the last entry the frame's extent.
    tile_col_starts: Vec<usize>,
    tile_row_starts: Vec<usize>,
}

/// A level's limits (spec Annex A.3): its index, the picture's pixels,
/// width and height, and the tiles and tile columns.
struct Level {
    idx: u32,
    max_pixels: usize,
    max_width: usize,
    max_height: usize,
    max_tiles: usize,
    max_cols: usize,
}

impl Level {
    fn admits(&self, width: usize, height: usize) -> bool {
        width * height <= self.max_pixels && width <= self.max_width && height <= self.max_height
    }
}

const fn level(
    idx: u32,
    max_pixels: usize,
    max_width: usize,
    max_height: usize,
    max_tiles: usize,
    max_cols: usize,
) -> Level {
    Level {
        idx,
        max_pixels,
        max_width,
        max_height,
        max_tiles,
        max_cols,
    }
}

/// Levels 2.0 to 6.0, the ones a still picture is measured by.
const LEVELS: [Level; 7] = [
    level(0, 147456, 2048, 1152, 8, 4),
    level(1, 278784, 2816, 1584, 8, 4),
    level(4, 665856, 4352, 2448, 16, 6),
    level(5, 1065024, 5504, 3096, 16, 6),
    level(8, 2359296, 6144, 3456, 32, 8),
    level(12, 8912896, 8192, 4352, 64, 8),
    level(16, 35651584, 16384, 8704, 128, 16),
];
/// The `seq_level_idx` past every level's limits.
const UNCONSTRAINED: u32 = 31;

/// The tile and tile column limits of the least level the picture size
/// fits, or of level 6 past it.
fn level_limits(width: usize, height: usize) -> (usize, usize) {
    LEVELS
        .iter()
        .find(|level| level.admits(width, height))
        .or(LEVELS.last())
        .map_or((MAX_TILE_COLS * MAX_TILE_ROWS, MAX_TILE_COLS), |level| {
            (level.max_tiles, level.max_cols)
        })
}

/// `tile_log2(blk, target)`: the least `k` with `blk << k >= target`.
fn tile_log2(blk: usize, target: usize) -> u32 {
    let mut k = 0;
    while (blk << k) < target {
        k += 1;
    }
    k
}

impl Geometry {
    /// The frame's grid, tiled uniformly: as many tile columns as the
    /// width allows while the uniform column stays four superblocks
    /// wide (the last, the remainder, may be narrower), within the
    /// tile columns the level the picture size sets allows (spec
    /// `tile_info`, Annex A.3), and the fewest rows the tile area
    /// needs. The grid is the frame's alone, so the bytes are the same
    /// on any host; threads spread over the columns.
    pub fn new(width: usize, height: usize) -> Result<Geometry, Error> {
        Self::tiled(width, height, 0)
    }

    /// `new`, with at least `rows_log2` tile rows where the limits
    /// allow them: the way to exercise tile rows on a small frame.
    pub fn tiled(width: usize, height: usize, rows_log2: u32) -> Result<Geometry, Error> {
        if width == 0 || height == 0 || width > MAX_AXIS || height > MAX_AXIS {
            return Err(Error::Axis { width, height });
        }
        let mi_cols = 2 * width.div_ceil(8);
        let mi_rows = 2 * height.div_ceil(8);
        let sb_cols = mi_cols.div_ceil(SB_MI);
        let sb_rows = mi_rows.div_ceil(SB_MI);
        let sb_shift = 4;
        let max_tile_width_sb = MAX_TILE_WIDTH >> (sb_shift + 2);
        let max_tile_area_sb = MAX_TILE_AREA >> (2 * (sb_shift + 2));
        let min_log2_cols = tile_log2(max_tile_width_sb, sb_cols);
        let max_log2_cols = tile_log2(1, sb_cols.min(MAX_TILE_COLS));
        let max_log2_rows = tile_log2(1, sb_rows.min(MAX_TILE_ROWS));
        let min_log2_tiles = min_log2_cols.max(tile_log2(max_tile_area_sb, sb_rows * sb_cols));
        let (_, max_level_cols) = level_limits(width, height);
        let mut tile_cols_log2 = min_log2_cols;
        while tile_cols_log2 < max_log2_cols
            && (sb_cols >> (tile_cols_log2 + 1)) >= 4
            && (1 << (tile_cols_log2 + 1)) <= max_level_cols
        {
            tile_cols_log2 += 1;
        }
        let tile_width_sb = sb_cols.div_ceil(1 << tile_cols_log2);
        let mut tile_col_starts: Vec<usize> = (0..sb_cols).step_by(tile_width_sb).collect();
        tile_col_starts.push(sb_cols);
        let min_log2_rows = min_log2_tiles.saturating_sub(tile_cols_log2);
        let mut tile_rows_log2 = min_log2_rows.max(rows_log2).min(max_log2_rows);
        // The count was chosen for the frame's area; a column the level
        // capped wide and rounded up to whole superblocks can pass the
        // tile area, so the rows split again (past level 6 alone).
        while tile_rows_log2 < max_log2_rows
            && tile_width_sb * sb_rows.div_ceil(1 << tile_rows_log2) > max_tile_area_sb
        {
            tile_rows_log2 += 1;
        }
        let tile_height_sb = sb_rows.div_ceil(1 << tile_rows_log2);
        let mut tile_row_starts: Vec<usize> = (0..sb_rows).step_by(tile_height_sb).collect();
        tile_row_starts.push(sb_rows);
        Ok(Geometry {
            width,
            height,
            mi_cols,
            mi_rows,
            sb_cols,
            sb_rows,
            tile_cols_log2,
            tile_rows_log2,
            tile_col_starts,
            tile_row_starts,
        })
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    /// The mode-info grid: the frame rounded up to 8 pixels, in 4s.
    pub fn mi_grid(&self) -> (usize, usize) {
        (self.mi_cols, self.mi_rows)
    }

    /// The superblock grid.
    pub fn sb_grid(&self) -> (usize, usize) {
        (self.sb_cols, self.sb_rows)
    }

    pub fn tile_cols(&self) -> usize {
        self.tile_col_starts.len() - 1
    }

    pub fn tile_rows(&self) -> usize {
        self.tile_row_starts.len() - 1
    }

    /// Where each tile column starts, in superblocks, then the frame's
    /// extent.
    pub fn tile_col_starts(&self) -> &[usize] {
        &self.tile_col_starts
    }

    /// Where each tile row starts, in superblocks, then the frame's
    /// extent.
    pub fn tile_row_starts(&self) -> &[usize] {
        &self.tile_row_starts
    }

    /// The tile row an SB row begins, if it begins one.
    fn tile_row_starting(&self, sb_row: usize) -> Option<usize> {
        self.tile_row_starts
            .iter()
            .take(self.tile_rows())
            .position(|&start| start == sb_row)
    }

    /// The least `seq_level_idx` whose picture size and tile limits
    /// admit the frame and its grid (spec Annex A.3), or 31
    /// (unconstrained) past level 6.
    pub fn level(&self) -> u32 {
        let tiles = self.tile_cols() * self.tile_rows();
        LEVELS
            .iter()
            .find(|level| {
                level.admits(self.width, self.height)
                    && tiles <= level.max_tiles
                    && self.tile_cols() <= level.max_cols
            })
            .map_or(UNCONSTRAINED, |level| level.idx)
    }

    /// The bits of `tile_info()` after the uniform spacing flag.
    fn write_tile_info(&self, bits: &mut Bits) {
        let sb_cols = self.sb_cols;
        let sb_rows = self.sb_rows;
        let min_log2_cols = tile_log2(MAX_TILE_WIDTH >> 6, sb_cols);
        let max_log2_cols = tile_log2(1, sb_cols.min(MAX_TILE_COLS));
        let max_log2_rows = tile_log2(1, sb_rows.min(MAX_TILE_ROWS));
        bits.flag(true);
        let mut log2 = min_log2_cols;
        while log2 < max_log2_cols {
            let more = log2 < self.tile_cols_log2;
            bits.flag(more);
            if !more {
                break;
            }
            log2 += 1;
        }
        let min_log2_tiles = min_log2_cols.max(tile_log2(MAX_TILE_AREA >> 12, sb_rows * sb_cols));
        let mut log2 = min_log2_tiles.saturating_sub(self.tile_cols_log2);
        while log2 < max_log2_rows {
            let more = log2 < self.tile_rows_log2;
            bits.flag(more);
            if !more {
                break;
            }
            log2 += 1;
        }
        if self.tile_cols_log2 > 0 || self.tile_rows_log2 > 0 {
            bits.put(self.tile_rows_log2 + self.tile_cols_log2, 0);
            bits.put(2, 3);
        }
    }
}

/// The `base_q_idx` a quality asks for: 100 the finest step AV1 codes
/// short of lossless, 1 the coarsest.
pub fn qindex(quality: u8) -> u8 {
    let quality = u32::from(quality.clamp(1, 100));
    (255 - ((quality - 1) * 254 + 49) / 99) as u8
}

// ------------------------------------------------------------ headers

/// `seq_profile`: main, 8-bit 4:2:0.
const PROFILE: u32 = 0;
/// The colour the stream declares: BT.709 primaries, the sRGB transfer
/// and the BT.601 matrix `code_band` applies, full range.
pub const COLOUR_PRIMARIES: u8 = 1;
pub const TRANSFER_CHARACTERISTICS: u8 = 13;
pub const MATRIX_COEFFICIENTS: u8 = 6;
pub const FULL_RANGE: bool = true;

/// The `AV1CodecConfigurationRecord` bytes the AVIF container repeats
/// from the sequence header: marker and version, the profile with the
/// level, the tier, depth and subsampling, and no presentation delay.
/// The configuration OBUs are left out; the item's data carries the
/// sequence header.
pub fn av1c(geometry: &Geometry) -> [u8; 4] {
    [
        0x81,
        ((PROFILE << 5) | (geometry.level() & 0x1f)) as u8,
        // seq_tier 0, high_bitdepth 0, twelve_bit 0, mono_chrome 0,
        // subsampling 1 and 1, chroma_sample_position 0.
        0b0000_1100,
        0,
    ]
}

/// The sequence header OBU's payload for a frame.
fn sequence_header(geometry: &Geometry) -> Vec<u8> {
    let mut b = Bits::new();
    b.put(3, PROFILE);
    b.flag(true);
    b.flag(true);
    b.put(5, geometry.level());
    b.put(4, 15);
    b.put(4, 15);
    b.put(16, (geometry.width - 1) as u32);
    b.put(16, (geometry.height - 1) as u32);
    b.flag(false);
    b.flag(false);
    b.flag(false);
    b.flag(false);
    b.flag(false);
    b.flag(false);
    // color_config: 8-bit, colour, the declared colour, chroma position
    // unknown, one chroma delta q.
    b.flag(false);
    b.flag(false);
    b.flag(true);
    b.put(8, u32::from(COLOUR_PRIMARIES));
    b.put(8, u32::from(TRANSFER_CHARACTERISTICS));
    b.put(8, u32::from(MATRIX_COEFFICIENTS));
    b.flag(FULL_RANGE);
    b.put(2, 0);
    b.flag(false);
    b.flag(false);
    b.trailing();
    b.bytes
}

/// The frame OBU's header bits, to the byte alignment before the tile
/// group.
fn frame_header(geometry: &Geometry, qindex: u8) -> Bits {
    let mut b = Bits::new();
    b.flag(false);
    b.flag(false);
    b.flag(false);
    geometry.write_tile_info(&mut b);
    b.put(8, u32::from(qindex));
    b.flag(false);
    b.flag(false);
    b.flag(false);
    b.flag(false);
    b.flag(false);
    b.flag(false);
    b.put(6, 0);
    b.put(6, 0);
    b.put(3, 0);
    b.flag(false);
    b.flag(false);
    b.flag(true);
    b.align();
    b
}

// ------------------------------------------------------------ planes

/// One plane's band: `stride` wide, the row above the band first.
struct Band {
    stride: usize,
    rows: usize,
    data: Vec<u8>,
}

impl Band {
    fn new(stride: usize, rows: usize) -> Band {
        Band {
            stride,
            rows,
            data: vec![0; stride * rows],
        }
    }

    fn at(&self, x: usize, y: usize) -> u8 {
        self.data.get(y * self.stride + x).copied().unwrap_or(128)
    }

    fn row(&self, y: usize) -> &[u8] {
        self.data
            .get(y * self.stride..(y + 1) * self.stride)
            .unwrap_or(&[])
    }

    fn row_mut(&mut self, y: usize) -> &mut [u8] {
        let stride = self.stride;
        self.data
            .get_mut(y * stride..(y + 1) * stride)
            .unwrap_or(&mut [])
    }

    /// Copies the last row into the first: the band above's floor.
    fn carry_last_row(&mut self) {
        let stride = self.stride;
        let last = (self.rows - 1) * stride;
        let (top, rest) = self.data.split_at_mut(stride);
        if let Some(bottom) = rest.get(last - stride..last) {
            top.copy_from_slice(bottom);
        }
    }
}

// --------------------------------------------------------- contexts

/// The above and left context arrays of one tile: level and DC sign
/// contexts per plane in 4-pixel units, the skip flags, modes and block
/// sizes of the mode-info neighbours.
#[derive(Clone)]
struct Contexts {
    above_level: [Vec<u8>; 3],
    above_dc: [Vec<u8>; 3],
    left_level: [Vec<u8>; 3],
    left_dc: [Vec<u8>; 3],
    above_skip: Vec<u8>,
    left_skip: Vec<u8>,
    above_mode: Vec<u8>,
    left_mode: Vec<u8>,
    above_width_log2: Vec<u8>,
    left_height_log2: Vec<u8>,
    /// Per plane, the superblock's decoded 4x4 units with a border of
    /// one: `(SB_MI + 3)` square, offset by one.
    decoded: [[bool; DECODED * DECODED]; 3],
}

const DECODED: usize = SB_MI + 3;

impl Contexts {
    fn new(mi_width: usize) -> Contexts {
        let cols = || vec![0u8; mi_width];
        let rows = || vec![0u8; SB_MI];
        Contexts {
            above_level: [cols(), cols(), cols()],
            above_dc: [cols(), cols(), cols()],
            left_level: [rows(), rows(), rows()],
            left_dc: [rows(), rows(), rows()],
            above_skip: cols(),
            left_skip: rows(),
            above_mode: cols(),
            left_mode: rows(),
            above_width_log2: cols(),
            left_height_log2: rows(),
            decoded: [[false; DECODED * DECODED]; 3],
        }
    }

    /// `clear_left_context()` at a superblock row's start.
    fn clear_left(&mut self) {
        for plane in 0..3 {
            if let Some(v) = self.left_level.get_mut(plane) {
                v.fill(0);
            }
            if let Some(v) = self.left_dc.get_mut(plane) {
                v.fill(0);
            }
        }
    }

    /// `clear_block_decoded_flags()` for a superblock with `mi_width_left`
    /// and `mi_height_left` mode-info units of the tile from its corner:
    /// the row above and the column left count as decoded that far, the
    /// corner too, the unit below the superblock's left column never.
    fn clear_decoded(&mut self, mi_width_left: usize, mi_height_left: usize) {
        for (plane, decoded) in self.decoded.iter_mut().enumerate() {
            let sub = usize::from(plane > 0);
            let sb_width4 = mi_width_left >> sub;
            let sb_height4 = mi_height_left >> sub;
            let size4 = SB_MI >> sub;
            for y in 0..DECODED {
                for x in 0..DECODED {
                    let on = if y == 0 {
                        x <= sb_width4
                    } else if x == 0 {
                        y <= sb_height4
                    } else {
                        false
                    };
                    if let Some(slot) = decoded.get_mut(y * DECODED + x) {
                        *slot = on;
                    }
                }
            }
            if let Some(slot) = decoded.get_mut((size4 + 1) * DECODED) {
                *slot = false;
            }
        }
    }

    /// A plane's level and DC contexts: above level, above DC, left
    /// level, left DC.
    fn plane(&self, plane: usize) -> (&[u8], &[u8], &[u8], &[u8]) {
        fn get(v: &[Vec<u8>; 3], plane: usize) -> &[u8] {
            v.get(plane).map_or(&[], Vec::as_slice)
        }
        (
            get(&self.above_level, plane),
            get(&self.above_dc, plane),
            get(&self.left_level, plane),
            get(&self.left_dc, plane),
        )
    }

    fn plane_mut(&mut self, plane: usize) -> (&mut [u8], &mut [u8], &mut [u8], &mut [u8]) {
        fn get(v: &mut [Vec<u8>; 3], plane: usize) -> &mut [u8] {
            v.get_mut(plane).map_or(&mut [], Vec::as_mut_slice)
        }
        let Contexts {
            above_level,
            above_dc,
            left_level,
            left_dc,
            ..
        } = self;
        (
            get(above_level, plane),
            get(above_dc, plane),
            get(left_level, plane),
            get(left_dc, plane),
        )
    }

    fn is_decoded(&self, plane: usize, x4: isize, y4: isize) -> bool {
        let (x, y) = (x4 + 1, y4 + 1);
        if x < 0 || y < 0 || x >= DECODED as isize || y >= DECODED as isize {
            return false;
        }
        self.decoded
            .get(plane)
            .and_then(|d| d.get(y as usize * DECODED + x as usize))
            .copied()
            .unwrap_or(false)
    }

    fn mark_decoded(&mut self, plane: usize, x4: usize, y4: usize, units: usize) {
        if let Some(d) = self.decoded.get_mut(plane) {
            for y in y4..y4 + units {
                for x in x4..x4 + units {
                    if let Some(slot) = d.get_mut((y + 1) * DECODED + x + 1) {
                        *slot = true;
                    }
                }
            }
        }
    }
}

// -------------------------------------------------------------- tile

/// A block's place: its mode-info position in the tile and the luma
/// side's log2 (3 to 6).
#[derive(Clone, Copy)]
struct At {
    mi_row: usize,
    mi_col: usize,
    log2: u32,
}

impl At {
    fn side(self) -> usize {
        1 << self.log2
    }

    /// Mode-info units along a side.
    fn units(self) -> usize {
        self.side() / MI
    }

    /// The position's row within its superblock, in mode-info units.
    fn sb_row(self) -> usize {
        self.mi_row % SB_MI
    }

    /// A quadrant of a split.
    fn quarter(self, dr: usize, dc: usize) -> At {
        At {
            mi_row: self.mi_row + dr,
            mi_col: self.mi_col + dc,
            log2: self.log2 - 1,
        }
    }
}

/// One plane of a block, coded under a mode: the rate-distortion cost,
/// the transform type, the levels, the end of block and the
/// reconstructed pixels.
struct Coded {
    cost: u64,
    tx: TxType,
    levels: Vec<i32>,
    eob: usize,
    recon: Vec<u8>,
}

/// One plane's quantized levels in the transform's row-major layout,
/// and their end of block in scan order (0 for none).
#[derive(Clone, Copy)]
struct Levels<'a> {
    levels: &'a [i32],
    eob: usize,
}

/// A coded block: what the symbols say and the levels they carry.
struct Leaf {
    at: At,
    skip: bool,
    y_mode: u8,
    uv_mode: u8,
    tx_type: TxType,
    /// Per plane: quantized levels in the transform's row-major layout
    /// and the end of block in scan order (0 for none).
    levels: [Vec<i32>; 3],
    eob: [usize; 3],
}

/// A superblock's decided tree in coding order.
enum Node {
    Split,
    Block(Leaf),
}

/// The state saved before a trial: the context columns and rows a block
/// touches, the decoded flags, and optionally its reconstruction.
struct Saved {
    at: At,
    /// The context segments, in `Contexts::segments` order, end to end.
    segments: Vec<u8>,
    decoded: [[bool; DECODED * DECODED]; 3],
    /// The three planes' pixels, end to end.
    pixels: Option<Vec<u8>>,
}

impl Contexts {
    /// Every context slice a block at the position writes.
    fn segments(&mut self, at: At) -> Vec<&mut [u8]> {
        let units = at.units();
        let sb_row = at.sb_row();
        let mi_col = at.mi_col;
        let Contexts {
            above_level,
            above_dc,
            left_level,
            left_dc,
            above_skip,
            left_skip,
            above_mode,
            left_mode,
            above_width_log2,
            left_height_log2,
            ..
        } = self;
        let mut out: Vec<&mut [u8]> = Vec::with_capacity(18);
        for (plane, (above, left)) in above_level
            .iter_mut()
            .chain(above_dc.iter_mut())
            .zip(left_level.iter_mut().chain(left_dc.iter_mut()))
            .enumerate()
        {
            let sub = usize::from(plane % 3 > 0);
            let (x4, y4, u) = (mi_col >> sub, sb_row >> sub, units >> sub);
            out.push(above.get_mut(x4..x4 + u).unwrap_or(&mut []));
            out.push(left.get_mut(y4..y4 + u).unwrap_or(&mut []));
        }
        for above in [above_skip, above_mode, above_width_log2] {
            out.push(above.get_mut(mi_col..mi_col + units).unwrap_or(&mut []));
        }
        for left in [left_skip, left_mode, left_height_log2] {
            out.push(left.get_mut(sb_row..sb_row + units).unwrap_or(&mut []));
        }
        out
    }
}

/// One tile's encoder: its share of the source and reconstruction bands,
/// its contexts, CDFs and coder.
struct Tile {
    /// The tile's first pixel column and its width, luma.
    x0: usize,
    width: usize,
    /// The tile's first and last-plus-one mode-info rows, and its
    /// last-plus-one mode-info column in tile units: the frame's edge
    /// where the padded width passes it.
    mi_row_start: usize,
    mi_row_end: usize,
    mi_col_end: usize,
    /// The frame's mode-info extent, for the clamps.
    mi_cols: usize,
    mi_rows: usize,
    /// The band's first mode-info row.
    band_mi_row: usize,
    source: [Band; 3],
    recon: [Band; 3],
    contexts: Contexts,
    cdfs: Cdfs,
    coder: Coder,
    qindex: u8,
    rdmult: u64,
}

fn plane_size(log2: u32, plane: usize) -> Size {
    match (log2, plane > 0) {
        (3, false) | (4, true) => Size::S8,
        (4, false) | (5, true) => Size::S16,
        (5, false) => Size::S32,
        _ => Size::S4,
    }
}

impl Tile {
    fn new(
        x0: usize,
        width: usize,
        mi_row_start: usize,
        mi_row_end: usize,
        geometry: &Geometry,
        qindex: u8,
    ) -> Tile {
        let q = u64::try_from(transform::dc_q(qindex)).unwrap_or(4);
        let rdmult = (q * q * 11 / 3).max(1);
        Tile {
            x0,
            width,
            mi_row_start,
            mi_row_end,
            mi_col_end: (width / MI).min(geometry.mi_cols.saturating_sub(x0 / MI)),
            mi_cols: geometry.mi_cols,
            mi_rows: geometry.mi_rows,
            band_mi_row: 0,
            source: [
                Band::new(width, SB),
                Band::new(width / 2, SB / 2),
                Band::new(width / 2, SB / 2),
            ],
            recon: [
                Band::new(width, SB + 1),
                Band::new(width / 2, SB / 2 + 1),
                Band::new(width / 2, SB / 2 + 1),
            ],
            contexts: Contexts::new(width / MI),
            cdfs: Cdfs::new(qindex),
            coder: Coder::new(),
            qindex,
            rdmult,
        }
    }

    /// Fills the source bands from the frame band's converted planes.
    fn take_source(&mut self, y: &Band, u: &Band, v: &Band) {
        for (plane, src) in [y, u, v].into_iter().enumerate() {
            let sub = usize::from(plane > 0);
            let x0 = self.x0 >> sub;
            if let Some(band) = self.source.get_mut(plane) {
                for row in 0..band.rows {
                    let to = band.row_mut(row);
                    if let Some(from) = src.row(row).get(x0..x0 + to.len()) {
                        to.copy_from_slice(from);
                    }
                }
            }
        }
    }

    /// Codes one superblock row of the tile: decisions, then symbols.
    fn encode_sb_row(&mut self, band_mi_row: usize) {
        self.band_mi_row = band_mi_row;
        if band_mi_row != self.mi_row_start {
            for plane in 0..3 {
                if let Some(band) = self.recon.get_mut(plane) {
                    band.carry_last_row();
                }
            }
        }
        self.contexts.clear_left();
        let sb_count = self.width / SB;
        for sb in 0..sb_count {
            let mi_col = sb * SB_MI;
            let mi_width_left = self.mi_col_end.saturating_sub(mi_col);
            let mi_height_left = self.mi_row_end - band_mi_row;
            self.contexts.clear_decoded(mi_width_left, mi_height_left);
            let at = At {
                mi_row: band_mi_row,
                mi_col,
                log2: 6,
            };
            let before = self.save(at, false);
            let mut nodes = Vec::new();
            self.decide(at, &mut nodes);
            // The decisions left the contexts where the superblock ends;
            // coding walks them forward again from where it began.
            self.restore(before);
            self.contexts.clear_decoded(mi_width_left, mi_height_left);
            let mut nodes = nodes.into_iter();
            self.emit(at, &mut nodes);
        }
    }

    /// The frame's mode-info column of a tile-local one.
    fn frame_mi_col(&self, mi_col: usize) -> usize {
        self.x0 / MI + mi_col
    }

    fn avail_up(&self, mi_row: usize) -> bool {
        mi_row > self.mi_row_start
    }

    fn avail_left(&self, mi_col: usize) -> bool {
        mi_col > 0
    }

    // ------------------------------------------------------ decisions

    /// Whether a block at the position is inside the frame at all, and
    /// whether its lower and right halves begin inside it (spec
    /// `decode_partition`'s `hasRows` and `hasCols`).
    fn edges(&self, at: At) -> Option<(bool, bool)> {
        if at.mi_row >= self.mi_row_end || self.frame_mi_col(at.mi_col) >= self.mi_cols {
            return None;
        }
        let half = at.units() / 2;
        Some((
            at.mi_row + half < self.mi_rows,
            self.frame_mi_col(at.mi_col) + half < self.mi_cols,
        ))
    }

    /// Decides the partition under the position, leaving reconstruction
    /// and contexts as if coded, and returns the rate-distortion cost.
    fn decide(&mut self, at: At, out: &mut Vec<Node>) -> u64 {
        let Some((has_rows, has_cols)) = self.edges(at) else {
            return 0;
        };
        let half = at.units() / 2;
        if at.log2 == 3 {
            let rate = self.partition_rate(at, false);
            let (leaf, cost) = self.best_leaf(at);
            out.push(Node::Block(leaf));
            return cost + self.rd(0, rate);
        }
        let split_rate = self.partition_rate(at, true);
        if at.log2 == 6 || !(has_rows && has_cols) {
            out.push(Node::Split);
            let mut cost = self.rd(0, split_rate);
            for (dr, dc) in [(0, 0), (0, half), (half, 0), (half, half)] {
                cost += self.decide(at.quarter(dr, dc), out);
            }
            return cost;
        }
        let before = self.save(at, true);
        let none_rate = self.partition_rate(at, false);
        let (leaf, leaf_cost) = self.best_leaf(at);
        let none_cost = leaf_cost + self.rd(0, none_rate);
        let after_none = self.save(at, true);
        self.restore(before);
        let mut children = vec![Node::Split];
        let mut split_cost = self.rd(0, split_rate);
        for (dr, dc) in [(0, 0), (0, half), (half, 0), (half, half)] {
            if split_cost >= none_cost {
                break;
            }
            split_cost += self.decide(at.quarter(dr, dc), &mut children);
        }
        if none_cost <= split_cost {
            self.restore(after_none);
            out.push(Node::Block(leaf));
            none_cost
        } else {
            out.append(&mut children);
            split_cost
        }
    }

    fn rd(&self, distortion: u64, rate: u64) -> u64 {
        (distortion << 7) + ((rate * self.rdmult) >> 9)
    }

    /// The rate of the partition symbol a node needs, none or split.
    fn partition_rate(&mut self, at: At, split: bool) -> u64 {
        let mut counter = Counter(0);
        self.partition_symbol(&mut counter, at, split);
        counter.0
    }

    /// The partition symbol of a node (spec `decode_partition`).
    fn partition_symbol(&mut self, sink: &mut impl Sink, at: At, split: bool) {
        let Some((has_rows, has_cols)) = self.edges(at) else {
            return;
        };
        let (mi_row, mi_col) = (at.mi_row, at.mi_col);
        let bsl = at.log2 - 2;
        let above = self.avail_up(mi_row)
            && self
                .contexts
                .above_width_log2
                .get(mi_col)
                .is_some_and(|&w| u32::from(w) < bsl);
        let left = self.avail_left(mi_col)
            && self
                .contexts
                .left_height_log2
                .get(mi_row % SB_MI)
                .is_some_and(|&h| u32::from(h) < bsl);
        let ctx = usize::from(left) * 2 + usize::from(above);
        let cdf = if bsl == 1 {
            self.cdfs.partition_8.get_mut(ctx)
        } else {
            self.cdfs
                .partition
                .get_mut(bsl as usize - 2)
                .and_then(|row| row.get_mut(ctx))
        };
        let Some(cdf) = cdf else {
            return;
        };
        if has_rows && has_cols {
            sink.symbol(cdf, if split { 3 } else { 0 });
        } else if has_cols || has_rows {
            // split_or_horz / split_or_vert: a bool from the kinds that
            // split the half inside the frame the other way (the top
            // half vertically, the left half horizontally); never
            // adapted.
            let kinds: &[usize] = if has_cols {
                &[2, 3, 4, 6, 7, 9]
            } else {
                &[1, 3, 4, 5, 6, 8]
            };
            let mut psum = 0u32;
            for &k in kinds {
                if k < cdf.n {
                    let low = if k == 0 { 0 } else { cdf.at(k - 1) };
                    psum += cdf.at(k) - low;
                }
            }
            let mut derived = Cdf { n: 2, v: [0; 17] };
            derived.v[0] = (32768 - psum.min(32767)) as u16;
            derived.v[1] = 32768;
            sink.symbol(&mut derived, usize::from(split));
        }
    }

    /// Saves what a trial at the block may change.
    fn save(&mut self, at: At, pixels: bool) -> Saved {
        let side = at.side();
        let pixels = pixels.then(|| {
            let mut copy = Vec::with_capacity(side * side * 3 / 2);
            for (plane, band) in self.recon.iter().enumerate() {
                let sub = usize::from(plane > 0);
                let n = side >> sub;
                let x = (at.mi_col * MI) >> sub;
                let y = ((at.mi_row - self.band_mi_row) * MI) >> sub;
                for r in 0..n {
                    copy.extend_from_slice(band.row(y + 1 + r).get(x..x + n).unwrap_or(&[]));
                }
            }
            copy
        });
        let mut segments = Vec::with_capacity(300);
        for segment in self.contexts.segments(at) {
            segments.extend_from_slice(segment);
        }
        Saved {
            at,
            segments,
            decoded: self.contexts.decoded,
            pixels,
        }
    }

    fn restore(&mut self, saved: Saved) {
        let mut rest = saved.segments.as_slice();
        for slot in self.contexts.segments(saved.at) {
            let Some((copy, tail)) = rest.split_at_checked(slot.len()) else {
                break;
            };
            slot.copy_from_slice(copy);
            rest = tail;
        }
        self.contexts.decoded = saved.decoded;
        let Some(pixels) = saved.pixels else {
            return;
        };
        let side = saved.at.side();
        let mut rest = pixels.as_slice();
        for (plane, band) in self.recon.iter_mut().enumerate() {
            let sub = usize::from(plane > 0);
            let n = side >> sub;
            let x = (saved.at.mi_col * MI) >> sub;
            let y = ((saved.at.mi_row - self.band_mi_row) * MI) >> sub;
            for r in 0..n {
                let Some((src, tail)) = rest.split_at_checked(n) else {
                    return;
                };
                rest = tail;
                if let Some(dst) = band.row_mut(y + 1 + r).get_mut(x..x + n) {
                    dst.copy_from_slice(src);
                }
            }
        }
    }

    // ---------------------------------------------------------- leaves

    /// The best-coded block at the position: modes chosen by
    /// rate-distortion cost, reconstruction and contexts applied.
    fn best_leaf(&mut self, at: At) -> (Leaf, u64) {
        let side = at.side();
        let mut leaf = Leaf {
            at,
            skip: false,
            y_mode: DC_PRED,
            uv_mode: DC_PRED,
            tx_type: TxType::DctDct,
            levels: [Vec::new(), Vec::new(), Vec::new()],
            eob: [0; 3],
        };
        // Luma: every mode screened by its residual's magnitude, the
        // closest few coded in full.
        let edges = self.pred_edges(0, at);
        let mut screened: Vec<(u64, u8, Vec<u8>)> = MODES
            .iter()
            .map(|&mode| {
                let mut pred = vec![0u8; side * side];
                predict_block(mode, &edges, &mut pred);
                (self.sad(0, at, &pred), mode, pred)
            })
            .collect();
        screened.sort_unstable_by_key(|(sad, mode, _)| (*sad, *mode));
        let mut best: Option<(u8, Coded)> = None;
        for (_, mode, pred) in screened.iter().take(LUMA_TRIALS) {
            let coded = self.code_plane(0, at, *mode, pred);
            if best.as_ref().is_none_or(|b| coded.cost < b.1.cost) {
                best = Some((*mode, coded));
            }
        }
        let Some((y_mode, luma)) = best else {
            return (leaf, 0);
        };
        let y_cost = luma.cost;
        leaf.y_mode = y_mode;
        leaf.tx_type = luma.tx;
        self.write_recon(0, at, &luma.recon);
        leaf.levels[0] = luma.levels;
        leaf.eob[0] = luma.eob;
        // Chroma: both planes share a mode.
        let n = side / 2;
        let (edges_u, edges_v) = (self.pred_edges(1, at), self.pred_edges(2, at));
        let mut screened: Vec<(u64, u8, Vec<u8>)> = MODES
            .iter()
            .map(|&mode| {
                let mut pred = vec![0u8; n * n * 2];
                let (u, v) = pred.split_at_mut(n * n);
                predict_block(mode, &edges_u, u);
                predict_block(mode, &edges_v, v);
                let total = self.sad(1, at, u) + self.sad(2, at, v);
                (total, mode, pred)
            })
            .collect();
        screened.sort_unstable_by_key(|(sad, mode, _)| (*sad, *mode));
        let mut best: Option<(u8, Coded, Coded)> = None;
        for (_, mode, pred) in screened.iter().take(CHROMA_TRIALS) {
            let (pu, pv) = pred.split_at(n * n);
            let u = self.code_plane(1, at, *mode, pu);
            let v = self.code_plane(2, at, *mode, pv);
            if best
                .as_ref()
                .is_none_or(|b| u.cost + v.cost < b.1.cost + b.2.cost)
            {
                best = Some((*mode, u, v));
            }
        }
        let Some((uv_mode, u, v)) = best else {
            return (leaf, y_cost);
        };
        let uv_cost = u.cost + v.cost;
        leaf.uv_mode = uv_mode;
        self.write_recon(1, at, &u.recon);
        self.write_recon(2, at, &v.recon);
        leaf.levels[1] = u.levels;
        leaf.levels[2] = v.levels;
        leaf.eob[1] = u.eob;
        leaf.eob[2] = v.eob;
        leaf.skip = leaf.eob.iter().all(|&e| e == 0);
        // The mode symbols' rate, then the block is applied.
        let mut counter = Counter(0);
        self.mode_symbols(&mut counter, &leaf);
        let cost = y_cost + uv_cost + self.rd(0, counter.0);
        self.apply_contexts(&leaf);
        (leaf, cost)
    }

    /// The sum of absolute differences of a prediction against the source.
    fn sad(&self, plane: usize, at: At, pred: &[u8]) -> u64 {
        let sub = usize::from(plane > 0);
        let n = at.side() >> sub;
        let x = (at.mi_col * MI) >> sub;
        let y = ((at.mi_row - self.band_mi_row) * MI) >> sub;
        let Some(source) = self.source.get(plane) else {
            return 0;
        };
        let mut total = 0u64;
        for (r, prow) in pred.chunks_exact(n).enumerate() {
            let srow = source.row(y + r).get(x..x + n).unwrap_or(&[]);
            for (p, s) in prow.iter().zip(srow) {
                total += u64::from(p.abs_diff(*s));
            }
        }
        total
    }

    /// Codes one plane of a block under a mode from its prediction:
    /// transform, quantization, reconstruction; nothing is written to
    /// the tile.
    fn code_plane(&mut self, plane: usize, at: At, mode: u8, pred: &[u8]) -> Coded {
        let sub = usize::from(plane > 0);
        let n = at.side() >> sub;
        let size = plane_size(at.log2, plane);
        let tx = mode_tx_type(mode, size);
        let x = (at.mi_col * MI) >> sub;
        let y = ((at.mi_row - self.band_mi_row) * MI) >> sub;
        let mut residual = [0i32; SB * SB / 4];
        let mut coeffs = [0i32; SB * SB / 4];
        let (Some(residual), Some(coeffs)) = (residual.get_mut(..n * n), coeffs.get_mut(..n * n))
        else {
            return Coded {
                cost: u64::MAX,
                tx,
                levels: Vec::new(),
                eob: 0,
                recon: pred.to_vec(),
            };
        };
        if let Some(source) = self.source.get(plane) {
            for (r, (res, prow)) in residual
                .chunks_exact_mut(n)
                .zip(pred.chunks_exact(n))
                .enumerate()
            {
                let srow = source.row(y + r).get(x..x + n).unwrap_or(&[]);
                for ((d, p), s) in res.iter_mut().zip(prow).zip(srow) {
                    *d = i32::from(*s) - i32::from(*p);
                }
            }
        }
        transform::forward(size, tx, residual, coeffs);
        let (levels, eob) = self.quantize(size, coeffs);
        let mut recon = pred.to_vec();
        if eob > 0 {
            let dequant = self.dequantize(size, &levels);
            let back = residual;
            transform::inverse(size, tx, &dequant, back);
            for (rec, b) in recon.iter_mut().zip(back.iter()) {
                *rec = (i32::from(*rec) + b).clamp(0, 255) as u8;
            }
        }
        let mut distortion = 0u64;
        if let Some(source) = self.source.get(plane) {
            for (r, rrow) in recon.chunks_exact(n).enumerate() {
                let srow = source.row(y + r).get(x..x + n).unwrap_or(&[]);
                for (rec, s) in rrow.iter().zip(srow) {
                    let d = i64::from(*rec) - i64::from(*s);
                    distortion += (d * d) as u64;
                }
            }
        }
        let mut counter = Counter(0);
        let coded = Levels {
            levels: &levels,
            eob,
        };
        self.coefficient_symbols(&mut counter, plane, at, mode, tx, coded);
        Coded {
            cost: self.rd(distortion, counter.0),
            tx,
            levels,
            eob,
            recon,
        }
    }

    /// Levels by a dead-zone quantizer, and the end of block.
    fn quantize(&self, size: Size, coeffs: &[i32]) -> (Vec<i32>, usize) {
        let dc = i64::from(transform::dc_q(self.qindex));
        let ac = i64::from(transform::ac_q(self.qindex));
        let shift = size.dequant_shift();
        let mut levels = vec![0i32; coeffs.len()];
        let mut eob = 0;
        for (c, &pos) in size.scan().iter().enumerate() {
            let pos = usize::from(pos);
            let Some(&coeff) = coeffs.get(pos) else {
                continue;
            };
            let q = if pos == 0 { dc } else { ac };
            let round = if pos == 0 { q / 2 } else { q * 3 / 8 };
            let magnitude = (i64::from(coeff.unsigned_abs()) << shift) + round;
            // Most fall under a step: no division for a zero level.
            if magnitude >= q {
                let level = (magnitude / q).min(0xFFFF) as i32;
                eob = c + 1;
                if let Some(slot) = levels.get_mut(pos) {
                    *slot = if coeff < 0 { -level } else { level };
                }
            }
        }
        (levels, eob)
    }

    /// The spec's dequantization of levels (7.12.3).
    fn dequantize(&self, size: Size, levels: &[i32]) -> Vec<i32> {
        let dc = transform::dc_q(self.qindex);
        let ac = transform::ac_q(self.qindex);
        let shift = size.dequant_shift();
        levels
            .iter()
            .enumerate()
            .map(|(pos, &level)| {
                let q = if pos == 0 { dc } else { ac };
                let dq = ((level.unsigned_abs() * q as u32) & 0xFFFFFF) >> shift;
                let dq = dq.min(0x8000) as i32;
                if level < 0 {
                    -dq
                } else {
                    dq.min(0x7FFF)
                }
            })
            .collect()
    }

    fn write_recon(&mut self, plane: usize, at: At, pixels: &[u8]) {
        let sub = usize::from(plane > 0);
        let n = at.side() >> sub;
        let x = (at.mi_col * MI) >> sub;
        let y = ((at.mi_row - self.band_mi_row) * MI) >> sub;
        let Some(band) = self.recon.get_mut(plane) else {
            return;
        };
        for (r, src) in pixels.chunks_exact(n).enumerate() {
            if let Some(dst) = band.row_mut(y + 1 + r).get_mut(x..x + n) {
                dst.copy_from_slice(src);
            }
        }
    }

    // ------------------------------------------------------ prediction

    /// The edges the spec's intra prediction (7.11.2) of a plane of the
    /// block reads from the reconstruction so far, which every mode
    /// shares.
    fn pred_edges(&self, plane: usize, at: At) -> Edges {
        let sub = usize::from(plane > 0);
        let n = at.side() >> sub;
        let units = n / MI;
        let (mi_row, mi_col) = (at.mi_row, at.mi_col);
        let x = (mi_col * MI) >> sub;
        let y = ((mi_row - self.band_mi_row) * MI) >> sub;
        let have_left = self.avail_left(mi_col);
        let have_above = self.avail_up(mi_row);
        let mut edges = Edges {
            n,
            have_above,
            have_left,
            above: [128; 65],
            left: [128; 65],
        };
        let Some(band) = self.recon.get(plane) else {
            return edges;
        };
        let sb_x4 = ((mi_col % SB_MI) >> sub) as isize;
        let sb_y4 = ((mi_row % SB_MI) >> sub) as isize;
        let have_above_right = self
            .contexts
            .is_decoded(plane, sb_x4 + units as isize, sb_y4 - 1);
        let have_below_left = self
            .contexts
            .is_decoded(plane, sb_x4 - 1, sb_y4 + units as isize);
        let max_x = ((self.mi_cols * MI) >> sub)
            .saturating_sub(self.x0 >> sub)
            .min(self.width >> sub)
            - 1;
        let max_y = ((self.mi_rows * MI) >> sub) - 1 - ((self.band_mi_row * MI) >> sub);
        // Band rows are the plane's rows plus one: row 0 is the row above.
        let px = |xx: usize, band_row: usize| band.at(xx, band_row);
        let Edges { above, left, .. } = &mut edges;
        let base = 1 << (8 - 1);
        if !have_above && have_left {
            above.fill(px(x - 1, y + 1));
        } else if !have_above && !have_left {
            above.fill(base - 1);
        } else {
            let limit = max_x.min(x + if have_above_right { 2 * n } else { n } - 1);
            for (i, slot) in above.iter_mut().enumerate().skip(1) {
                *slot = px(limit.min(x + i - 1), y);
            }
        }
        if !have_left && have_above {
            left.fill(px(x, y));
        } else if !have_left && !have_above {
            left.fill(base + 1);
        } else {
            let limit = max_y.min(y + if have_below_left { 2 * n } else { n } - 1);
            for (i, slot) in left.iter_mut().enumerate().skip(1) {
                *slot = px(x - 1, limit.min(y + i - 1) + 1);
            }
        }
        let corner = if have_above && have_left {
            px(x - 1, y)
        } else if have_above {
            px(x, y)
        } else if have_left {
            px(x - 1, y + 1)
        } else {
            base
        };
        above[0] = corner;
        left[0] = corner;
        edges
    }

    // --------------------------------------------------------- symbols

    /// The mode-info symbols of a block (spec `intra_frame_mode_info`).
    fn mode_symbols(&mut self, sink: &mut impl Sink, leaf: &Leaf) {
        let mi_row = leaf.at.mi_row;
        let mi_col = leaf.at.mi_col;
        let above_skip = if self.avail_up(mi_row) {
            self.contexts.above_skip.get(mi_col).copied().unwrap_or(0)
        } else {
            0
        };
        let left_skip = if self.avail_left(mi_col) {
            self.contexts
                .left_skip
                .get(mi_row % SB_MI)
                .copied()
                .unwrap_or(0)
        } else {
            0
        };
        if let Some(cdf) = self.cdfs.skip.get_mut(usize::from(above_skip + left_skip)) {
            sink.symbol(cdf, usize::from(leaf.skip));
        }
        let above_mode = if self.avail_up(mi_row) {
            self.contexts
                .above_mode
                .get(mi_col)
                .copied()
                .unwrap_or(DC_PRED)
        } else {
            DC_PRED
        };
        let left_mode = if self.avail_left(mi_col) {
            self.contexts
                .left_mode
                .get(mi_row % SB_MI)
                .copied()
                .unwrap_or(DC_PRED)
        } else {
            DC_PRED
        };
        if let Some(cdf) = self
            .cdfs
            .y_mode
            .get_mut(mode_context(above_mode))
            .and_then(|row| row.get_mut(mode_context(left_mode)))
        {
            sink.symbol(cdf, usize::from(leaf.y_mode));
        }
        if is_directional(leaf.y_mode) {
            if let Some(cdf) = self
                .cdfs
                .angle_delta
                .get_mut(usize::from(leaf.y_mode - V_PRED))
            {
                sink.symbol(cdf, 3);
            }
        }
        if let Some(cdf) = self.cdfs.uv_mode.get_mut(usize::from(leaf.y_mode)) {
            sink.symbol(cdf, usize::from(leaf.uv_mode));
        }
        if is_directional(leaf.uv_mode) {
            if let Some(cdf) = self
                .cdfs
                .angle_delta
                .get_mut(usize::from(leaf.uv_mode - V_PRED))
            {
                sink.symbol(cdf, 3);
            }
        }
    }

    /// The coefficient symbols of one plane of a block (spec `coeffs`),
    /// from the contexts as they stand.
    fn coefficient_symbols(
        &mut self,
        sink: &mut impl Sink,
        plane: usize,
        at: At,
        y_mode: u8,
        tx: TxType,
        coded: Levels<'_>,
    ) {
        let Levels { levels, eob } = coded;
        let sub = usize::from(plane > 0);
        let size = plane_size(at.log2, plane);
        let n = size.points();
        let units = n / MI;
        let x4 = at.mi_col >> sub;
        let y4 = at.sb_row() >> sub;
        let ptype = usize::from(plane > 0);
        let size_ctx = size.index();
        let max_x4 = (self.mi_cols >> sub).saturating_sub((self.x0 / MI) >> sub);
        let max_y4 = (self.mi_rows >> sub).saturating_sub(self.band_mi_row >> sub);
        let (above_level, above_dc, left_level, left_dc) = self.contexts.plane(plane);
        // all_zero
        let ctx = if plane == 0 {
            0
        } else {
            let mut above = 0u8;
            let mut left = 0u8;
            for k in 0..units {
                if x4 + k < max_x4 {
                    above |= above_level.get(x4 + k).copied().unwrap_or(0);
                    above |= above_dc.get(x4 + k).copied().unwrap_or(0);
                }
                if y4 + k < max_y4 {
                    left |= left_level.get(y4 + k).copied().unwrap_or(0);
                    left |= left_dc.get(y4 + k).copied().unwrap_or(0);
                }
            }
            7 + usize::from(above != 0) + usize::from(left != 0)
        };
        if let Some(cdf) = self
            .cdfs
            .txb_skip
            .get_mut(size_ctx)
            .and_then(|row| row.get_mut(ctx))
        {
            sink.symbol(cdf, usize::from(eob == 0));
        }
        if eob == 0 {
            return;
        }
        if plane == 0 && size != Size::S32 {
            if let Some(cdf) = self
                .cdfs
                .tx_type
                .get_mut(size_ctx)
                .and_then(|row| row.get_mut(usize::from(y_mode)))
            {
                sink.symbol(cdf, tx.symbol());
            }
        }
        // eob_pt and its extra bits
        let (eob_pt, extra_bits) = eob_position(eob);
        let class = (size.points().ilog2() - 2) as usize;
        if let Some(cdf) = self
            .cdfs
            .eob_pt
            .get_mut(class)
            .and_then(|row| row.get_mut(ptype))
        {
            sink.symbol(cdf, eob_pt - 1);
        }
        if extra_bits > 0 {
            let extra = eob - eob_group_start(eob_pt);
            let top = (extra >> (extra_bits - 1)) & 1 == 1;
            if let Some(cdf) = self
                .cdfs
                .eob_extra
                .get_mut(size_ctx)
                .and_then(|row| row.get_mut(ptype))
                .and_then(|row| row.get_mut(eob_pt - 3))
            {
                sink.symbol(cdf, usize::from(top));
            }
            for i in 1..extra_bits {
                let shift = extra_bits - 1 - i;
                sink.bit((extra >> shift) & 1 == 1);
            }
        }
        // Levels in reverse scan order.
        let scan = size.scan();
        let (row_shift, col_mask) = (n.ilog2(), n - 1);
        let level_at = |row: usize, col: usize| -> i32 {
            if row >= n || col >= n {
                return 0;
            }
            levels.get(row * n + col).map_or(0, |l| l.abs())
        };
        for c in (0..eob).rev() {
            let pos = usize::from(scan.get(c).copied().unwrap_or(0));
            let (row, col) = (pos >> row_shift, pos & col_mask);
            let level = level_at(row, col);
            if c == eob - 1 {
                let ctx = if c == 0 {
                    0
                } else if c <= n * n / 8 {
                    1
                } else if c <= n * n / 4 {
                    2
                } else {
                    3
                };
                if let Some(cdf) = self
                    .cdfs
                    .base_eob
                    .get_mut(size_ctx)
                    .and_then(|r| r.get_mut(ptype))
                    .and_then(|r| r.get_mut(ctx))
                {
                    sink.symbol(cdf, level.min(3) as usize - 1);
                }
            } else {
                let ctx = if pos == 0 {
                    0
                } else {
                    let mag = level_at(row, col + 1).min(3)
                        + level_at(row + 1, col).min(3)
                        + level_at(row + 1, col + 1).min(3)
                        + level_at(row, col + 2).min(3)
                        + level_at(row + 2, col).min(3);
                    let ctx = ((mag + 1) >> 1).min(4) as usize;
                    ctx + if row + col < 2 {
                        1
                    } else if row + col < 4 {
                        6
                    } else {
                        21
                    }
                };
                if let Some(cdf) = self
                    .cdfs
                    .base
                    .get_mut(size_ctx)
                    .and_then(|r| r.get_mut(ptype))
                    .and_then(|r| r.get_mut(ctx))
                {
                    sink.symbol(cdf, level.min(3) as usize);
                }
            }
            if level > 2 {
                let mag = level_at(row, col + 1).min(15)
                    + level_at(row + 1, col).min(15)
                    + level_at(row + 1, col + 1).min(15);
                let mag = ((mag + 1) >> 1).min(6) as usize;
                let ctx = if pos == 0 {
                    mag
                } else if row < 2 && col < 2 {
                    mag + 7
                } else {
                    mag + 14
                };
                let base_range = (level - 3) as usize;
                for idx in (0..12).step_by(3) {
                    let k = (base_range - idx).min(3);
                    if let Some(cdf) = self
                        .cdfs
                        .br
                        .get_mut(size_ctx.min(3))
                        .and_then(|r| r.get_mut(ptype))
                        .and_then(|r| r.get_mut(ctx))
                    {
                        sink.symbol(cdf, k);
                    }
                    if k < 3 {
                        break;
                    }
                }
            }
        }
        // Signs and the Golomb remainders in scan order.
        for c in 0..eob {
            let pos = usize::from(scan.get(c).copied().unwrap_or(0));
            let Some(&level) = levels.get(pos) else {
                continue;
            };
            if level == 0 {
                continue;
            }
            let negative = level < 0;
            if c == 0 {
                let mut dc_sign = 0i32;
                let lean = |category: Option<&u8>| match category {
                    Some(1) => -1,
                    Some(2) => 1,
                    _ => 0,
                };
                for k in 0..units {
                    if x4 + k < max_x4 {
                        dc_sign += lean(above_dc.get(x4 + k));
                    }
                    if y4 + k < max_y4 {
                        dc_sign += lean(left_dc.get(y4 + k));
                    }
                }
                let ctx = match dc_sign.signum() {
                    -1 => 1,
                    1 => 2,
                    _ => 0,
                };
                if let Some(cdf) = self
                    .cdfs
                    .dc_sign
                    .get_mut(ptype)
                    .and_then(|r| r.get_mut(ctx))
                {
                    sink.symbol(cdf, usize::from(negative));
                }
            } else {
                sink.bit(negative);
            }
            let magnitude = level.unsigned_abs();
            if magnitude > 14 {
                let x = magnitude - 14;
                let length = 32 - x.leading_zeros();
                for _ in 0..length - 1 {
                    sink.bit(false);
                }
                for i in (0..length).rev() {
                    sink.bit((x >> i) & 1 == 1);
                }
            }
        }
    }

    /// Records a coded block in the contexts, as the decoder would.
    fn apply_contexts(&mut self, leaf: &Leaf) {
        let at = leaf.at;
        let units = at.units();
        let sb_row = at.sb_row();
        for (plane, (levels, &eob)) in leaf.levels.iter().zip(leaf.eob.iter()).enumerate() {
            let sub = usize::from(plane > 0);
            let x4 = at.mi_col >> sub;
            let y4 = sb_row >> sub;
            let units = units >> sub;
            let (cul, dc) = if leaf.skip || eob == 0 {
                (0, 0)
            } else {
                let cul = levels.iter().map(|l| l.unsigned_abs()).sum::<u32>().min(63) as u8;
                let dc = match levels.first().copied().unwrap_or(0).signum() {
                    -1 => 1,
                    1 => 2,
                    _ => 0,
                };
                (cul, dc)
            };
            let (above_level, above_dc, left_level, left_dc) = self.contexts.plane_mut(plane);
            for k in 0..units {
                if let Some(s) = above_level.get_mut(x4 + k) {
                    *s = cul;
                }
                if let Some(s) = above_dc.get_mut(x4 + k) {
                    *s = dc;
                }
                if let Some(s) = left_level.get_mut(y4 + k) {
                    *s = cul;
                }
                if let Some(s) = left_dc.get_mut(y4 + k) {
                    *s = dc;
                }
            }
            self.contexts
                .mark_decoded(plane, (at.mi_col % SB_MI) >> sub, y4, units);
        }
        let mi_col = at.mi_col;
        for k in 0..units {
            if let Some(s) = self.contexts.above_skip.get_mut(mi_col + k) {
                *s = u8::from(leaf.skip);
            }
            if let Some(s) = self.contexts.above_mode.get_mut(mi_col + k) {
                *s = leaf.y_mode;
            }
            if let Some(s) = self.contexts.above_width_log2.get_mut(mi_col + k) {
                *s = (at.log2 - 2) as u8;
            }
            if let Some(s) = self.contexts.left_skip.get_mut(sb_row + k) {
                *s = u8::from(leaf.skip);
            }
            if let Some(s) = self.contexts.left_mode.get_mut(sb_row + k) {
                *s = leaf.y_mode;
            }
            if let Some(s) = self.contexts.left_height_log2.get_mut(sb_row + k) {
                *s = (at.log2 - 2) as u8;
            }
        }
    }

    /// Writes a decided tree's symbols in coding order.
    fn emit(&mut self, at: At, nodes: &mut impl Iterator<Item = Node>) {
        if self.edges(at).is_none() {
            return;
        }
        let half = at.units() / 2;
        let Some(node) = nodes.next() else {
            return;
        };
        let mut coder = std::mem::replace(&mut self.coder, Coder::new());
        match node {
            Node::Split => {
                self.partition_symbol(&mut coder, at, true);
                self.coder = coder;
                for (dr, dc) in [(0, 0), (0, half), (half, 0), (half, half)] {
                    self.emit(at.quarter(dr, dc), nodes);
                }
            }
            Node::Block(leaf) => {
                self.partition_symbol(&mut coder, at, false);
                self.mode_symbols(&mut coder, &leaf);
                if !leaf.skip {
                    for (plane, (levels, &eob)) in
                        leaf.levels.iter().zip(leaf.eob.iter()).enumerate()
                    {
                        self.coefficient_symbols(
                            &mut coder,
                            plane,
                            leaf.at,
                            leaf.y_mode,
                            leaf.tx_type,
                            Levels { levels, eob },
                        );
                    }
                }
                self.coder = coder;
                self.apply_contexts(&leaf);
            }
        }
    }
}

/// libaom's `av1_get_eob_pos_token`: the class of an end of block and
/// the count of extra bits it carries.
fn eob_position(eob: usize) -> (usize, usize) {
    let t = match eob {
        0 => 0,
        1 => 1,
        2 => 2,
        3..=4 => 3,
        5..=8 => 4,
        9..=16 => 5,
        17..=32 => 6,
        33..=64 => 7,
        65..=128 => 8,
        129..=256 => 9,
        257..=512 => 10,
        _ => 11,
    };
    (t, t.saturating_sub(2))
}

/// The first end of block of a class.
fn eob_group_start(t: usize) -> usize {
    match t {
        0 => 0,
        1 => 1,
        2 => 2,
        _ => (1 << (t - 2)) + 1,
    }
}

// --------------------------------------------------- prediction kernels

/// A plane's prediction edges for an `n` square: `above[1 + i]` is the
/// spec's `AboveRow[i]`, `above[0]` its `AboveRow[-1]`, the corner; the
/// same for the left column.
struct Edges {
    n: usize,
    have_above: bool,
    have_left: bool,
    above: [u8; 65],
    left: [u8; 65],
}

/// Fills an `n` square row by row with `value(i, j)`, clamped to a
/// pixel.
fn fill(out: &mut [u8], n: usize, value: impl Fn(usize, usize) -> i32) {
    for (i, row) in out.chunks_exact_mut(n).enumerate() {
        for (j, slot) in row.iter_mut().enumerate() {
            *slot = value(i, j).clamp(0, 255) as u8;
        }
    }
}

/// The spec's prediction of an `n` square from the edges.
fn predict_block(mode: u8, edges: &Edges, out: &mut [u8]) {
    let Edges {
        n,
        have_above,
        have_left,
        ref above,
        ref left,
    } = *edges;
    let a = |i: isize| i32::from(above.get((i + 1) as usize).copied().unwrap_or(128));
    let l = |i: isize| i32::from(left.get((i + 1) as usize).copied().unwrap_or(128));
    match mode {
        DC_PRED => {
            let v = if have_above && have_left {
                let sum: i32 = (0..n as isize).map(|i| a(i) + l(i)).sum();
                (sum + n as i32) / (2 * n as i32)
            } else if have_left {
                let sum: i32 = (0..n as isize).map(l).sum();
                (sum + n as i32 / 2) / n as i32
            } else if have_above {
                let sum: i32 = (0..n as isize).map(a).sum();
                (sum + n as i32 / 2) / n as i32
            } else {
                128
            };
            out.fill(v.clamp(0, 255) as u8);
        }
        SMOOTH_PRED | SMOOTH_V_PRED | SMOOTH_H_PRED => {
            let w = sm_weights(n);
            let weight = |i: usize| i32::from(w.get(i).copied().unwrap_or(0));
            let below = l(n as isize - 1);
            let right = a(n as isize - 1);
            match mode {
                SMOOTH_PRED => fill(out, n, |i, j| {
                    let s = weight(i) * a(j as isize)
                        + (256 - weight(i)) * below
                        + weight(j) * l(i as isize)
                        + (256 - weight(j)) * right;
                    (s + 256) >> 9
                }),
                SMOOTH_V_PRED => fill(out, n, |i, j| {
                    let s = weight(i) * a(j as isize) + (256 - weight(i)) * below;
                    (s + 128) >> 8
                }),
                _ => fill(out, n, |i, j| {
                    let s = weight(j) * l(i as isize) + (256 - weight(j)) * right;
                    (s + 128) >> 8
                }),
            }
        }
        PAETH_PRED => {
            let corner = a(-1);
            fill(out, n, |i, j| {
                let (top, side) = (a(j as isize), l(i as isize));
                let base = top + side - corner;
                let (p_left, p_top, p_corner) = (
                    (base - side).abs(),
                    (base - top).abs(),
                    (base - corner).abs(),
                );
                if p_left <= p_top && p_left <= p_corner {
                    side
                } else if p_top <= p_corner {
                    top
                } else {
                    corner
                }
            });
        }
        _ => {
            let angle = mode_angle(mode);
            let round5 = |v: i32| (v + 16) >> 5;
            if angle == 90 {
                if let Some(top) = above.get(1..=n) {
                    for row in out.chunks_exact_mut(n) {
                        row.copy_from_slice(top);
                    }
                }
            } else if angle == 180 {
                for (row, &side) in out.chunks_exact_mut(n).zip(left.iter().skip(1)) {
                    row.fill(side);
                }
            } else if angle < 90 {
                let dx = derivative(angle);
                let max_base = 2 * n as isize - 1;
                fill(out, n, |i, j| {
                    let idx = (i as i32 + 1) * dx;
                    let shift = (idx >> 1) & 0x1F;
                    let base = (idx >> 6) as isize + j as isize;
                    if base < max_base {
                        round5(a(base) * (32 - shift) + a(base + 1) * shift)
                    } else {
                        a(max_base)
                    }
                });
            } else if angle < 180 {
                let dx = derivative(180 - angle);
                let dy = derivative(angle - 90);
                fill(out, n, |i, j| {
                    let idx = ((j as i32) << 6) - (i as i32 + 1) * dx;
                    let base = idx >> 6;
                    if base >= -1 {
                        let shift = (idx >> 1) & 0x1F;
                        let b = base as isize;
                        round5(a(b) * (32 - shift) + a(b + 1) * shift)
                    } else {
                        let idx = ((i as i32) << 6) - (j as i32 + 1) * dy;
                        let base = (idx >> 6) as isize;
                        let shift = (idx >> 1) & 0x1F;
                        round5(l(base) * (32 - shift) + l(base + 1) * shift)
                    }
                });
            } else {
                let dy = derivative(270 - angle);
                let max_base = 2 * n as isize - 1;
                fill(out, n, |i, j| {
                    let idx = (j as i32 + 1) * dy;
                    let shift = (idx >> 1) & 0x1F;
                    let base = (idx >> 6) as isize + i as isize;
                    if base < max_base {
                        round5(l(base) * (32 - shift) + l(base + 1) * shift)
                    } else {
                        l(max_base)
                    }
                });
            }
        }
    }
}

// ------------------------------------------------------------ encoder

/// The frame as the encoder reconstructed it, which is what every
/// decoder shows: 4:2:0 planes, the chroma ones half the size rounded
/// up.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Reconstruction {
    pub width: usize,
    pub height: usize,
    pub planes: [Vec<u8>; 3],
}

/// A streaming encoder: fed interleaved 8-bit RGB rows in any number at
/// a time, it converts and codes a superblock row whenever one is
/// complete, and `finish` returns the OBUs.
pub struct Encoder {
    geometry: Geometry,
    qindex: u8,
    threads: usize,
    taken: usize,
    /// Rows received and not yet coded, at most a superblock row.
    pending: Vec<u8>,
    /// The converted planes of one superblock row, frame-wide padded.
    planes: [Band; 3],
    /// The tiles of the current tile row, by column.
    tiles: Vec<Tile>,
    /// Coded tiles' bytes in tile order.
    coded: Vec<Vec<u8>>,
    sb_row: usize,
    /// The reconstruction, gathered when asked for.
    reconstruction: Option<Reconstruction>,
}

impl Encoder {
    /// Begins a `width` by `height` frame at `quality` (1..=100).
    pub fn new(width: usize, height: usize, quality: u8, threads: usize) -> Result<Encoder, Error> {
        Self::with_geometry(Geometry::new(width, height)?, quality, threads)
    }

    /// `new` over a grid `Geometry` chose for the caller.
    pub fn with_geometry(
        geometry: Geometry,
        quality: u8,
        threads: usize,
    ) -> Result<Encoder, Error> {
        let stride = geometry.sb_cols * SB;
        Ok(Encoder {
            geometry,
            qindex: qindex(quality),
            threads: threads.max(1),
            taken: 0,
            pending: Vec::new(),
            planes: [
                Band::new(stride, SB),
                Band::new(stride / 2, SB / 2),
                Band::new(stride / 2, SB / 2),
            ],
            tiles: Vec::new(),
            coded: Vec::new(),
            sb_row: 0,
            reconstruction: None,
        })
    }

    /// Asks for the reconstruction to be kept for `finish_with`.
    pub fn keep_reconstruction(&mut self) {
        let (w, h) = (self.geometry.width, self.geometry.height);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        self.reconstruction = Some(Reconstruction {
            width: w,
            height: h,
            planes: [vec![0; w * h], vec![0; cw * ch], vec![0; cw * ch]],
        });
    }

    pub fn geometry(&self) -> &Geometry {
        &self.geometry
    }

    /// Feeds whole rows of interleaved RGB: their length must be a
    /// multiple of the row's, and the rows so far must not pass the
    /// height.
    pub fn encode_rows(&mut self, rows: &[u8]) -> Result<(), Error> {
        let row_bytes = self.geometry.width * 3;
        if row_bytes == 0 || !rows.len().is_multiple_of(row_bytes) {
            return Err(Error::Rows);
        }
        let count = rows.len() / row_bytes;
        if self.taken.saturating_add(count) > self.geometry.height {
            return Err(Error::Rows);
        }
        self.taken += count;
        self.pending.extend_from_slice(rows);
        // The whole bands go, and what is left of a band is copied once
        // per call, not once per band.
        let pending = std::mem::take(&mut self.pending);
        let mut bands = pending.chunks_exact(row_bytes * SB);
        for band in &mut bands {
            self.code_band(band, SB);
        }
        self.pending = bands.remainder().to_vec();
        Ok(())
    }

    /// Codes the bottom rows and closes the stream: the sequence header
    /// OBU and the frame OBU.
    pub fn finish(self) -> Result<Vec<u8>, Error> {
        self.finish_with().map(|(obus, _)| obus)
    }

    /// `finish`, with the reconstruction if it was kept.
    pub fn finish_with(mut self) -> Result<(Vec<u8>, Option<Reconstruction>), Error> {
        if self.taken != self.geometry.height {
            return Err(Error::Rows);
        }
        let row_bytes = self.geometry.width * 3;
        let rows = self.pending.len() / row_bytes;
        if rows > 0 {
            let band = std::mem::take(&mut self.pending);
            self.code_band(&band, rows);
        }
        self.close_tile_row();
        let mut out = Vec::new();
        obu(
            &mut out,
            OBU_SEQUENCE_HEADER,
            &sequence_header(&self.geometry),
        );
        let mut frame = frame_header(&self.geometry, self.qindex).bytes;
        let tiles = self.geometry.tile_cols() * self.geometry.tile_rows();
        if tiles > 1 {
            // tile_start_and_end_present_flag, then the alignment.
            frame.push(0);
        }
        for (index, data) in self.coded.iter().enumerate() {
            if index + 1 < tiles {
                let size = (data.len() - 1) as u32;
                frame.extend_from_slice(&size.to_le_bytes());
            }
            frame.extend_from_slice(data);
        }
        obu(&mut out, OBU_FRAME, &frame);
        Ok((out, self.reconstruction.take()))
    }

    /// Converts a band of `rows` RGB rows into the planes, padded, and
    /// codes it as one superblock row.
    fn code_band(&mut self, band: &[u8], rows: usize) {
        let width = self.geometry.width;
        let stride = self.planes[0].stride;
        let rgb_at = |x: usize, y: usize| -> (i32, i32, i32) {
            let x = x.min(width - 1);
            let y = y.min(rows - 1);
            let at = (y * width + x) * 3;
            let px = band.get(at..at + 3).unwrap_or(&[0, 0, 0]);
            (
                i32::from(px.first().copied().unwrap_or(0)),
                i32::from(px.get(1).copied().unwrap_or(0)),
                i32::from(px.get(2).copied().unwrap_or(0)),
            )
        };
        for y in 0..SB {
            for x in 0..stride {
                let (r, g, b) = rgb_at(x, y);
                let luma = (77 * r + 150 * g + 29 * b + 128) >> 8;
                if let Some(slot) = self.planes[0].data.get_mut(y * stride + x) {
                    *slot = luma.clamp(0, 255) as u8;
                }
            }
        }
        for y in 0..SB / 2 {
            for x in 0..stride / 2 {
                let mut cb = 0;
                let mut cr = 0;
                for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                    let (r, g, b) = rgb_at(2 * x + dx, 2 * y + dy);
                    cb += -43 * r - 85 * g + 128 * b;
                    cr += 128 * r - 107 * g - 21 * b;
                }
                let cb = 128 + ((cb + 512) >> 10);
                let cr = 128 + ((cr + 512) >> 10);
                if let Some(slot) = self.planes[1].data.get_mut(y * (stride / 2) + x) {
                    *slot = cb.clamp(0, 255) as u8;
                }
                if let Some(slot) = self.planes[2].data.get_mut(y * (stride / 2) + x) {
                    *slot = cr.clamp(0, 255) as u8;
                }
            }
        }
        if let Some(tile_row) = self.geometry.tile_row_starting(self.sb_row) {
            self.close_tile_row();
            self.open_tile_row(tile_row);
        }
        let band_mi_row = self.sb_row * SB_MI;
        let [y, u, v] = &self.planes;
        for tile in self.tiles.iter_mut() {
            tile.take_source(y, u, v);
        }
        let items: Vec<&mut Tile> = self.tiles.iter_mut().collect();
        crate::develop::bands(items, self.threads, |tile| tile.encode_sb_row(band_mi_row));
        if let Some(reconstruction) = self.reconstruction.as_mut() {
            let y0 = self.sb_row * SB;
            for tile in &self.tiles {
                for (plane, band) in tile.recon.iter().enumerate() {
                    let sub = usize::from(plane > 0);
                    let (w, h) = (
                        reconstruction.width.div_ceil(1 << sub),
                        reconstruction.height.div_ceil(1 << sub),
                    );
                    let x0 = tile.x0 >> sub;
                    let Some(out) = reconstruction.planes.get_mut(plane) else {
                        continue;
                    };
                    for r in 0..(SB >> sub) {
                        let y = (y0 >> sub) + r;
                        if y >= h {
                            break;
                        }
                        let width = (tile.width >> sub).min(w.saturating_sub(x0));
                        if let (Some(src), Some(dst)) = (
                            band.row(r + 1).get(..width),
                            out.get_mut(y * w + x0..y * w + x0 + width),
                        ) {
                            dst.copy_from_slice(src);
                        }
                    }
                }
            }
        }
        self.sb_row += 1;
    }

    fn open_tile_row(&mut self, tile_row: usize) {
        let g = &self.geometry;
        let mi_row_start = g.tile_row_starts.get(tile_row).copied().unwrap_or(0) * SB_MI;
        let mi_row_end = g
            .tile_row_starts
            .get(tile_row + 1)
            .copied()
            .unwrap_or(g.sb_rows)
            * SB_MI;
        let mi_row_end = mi_row_end.min(g.mi_rows);
        self.tiles = (0..g.tile_cols())
            .map(|col| {
                let start = g.tile_col_starts.get(col).copied().unwrap_or(0);
                let end = g.tile_col_starts.get(col + 1).copied().unwrap_or(g.sb_cols);
                Tile::new(
                    start * SB,
                    (end - start) * SB,
                    mi_row_start,
                    mi_row_end,
                    g,
                    self.qindex,
                )
            })
            .collect();
    }

    fn close_tile_row(&mut self) {
        for tile in self.tiles.drain(..) {
            self.coded.push(tile.coder.finish());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u32 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 33) as u32
        }
    }

    /// The spec's symbol decoder (8.2), written from the text: what any
    /// decoder does with the coder's bytes.
    struct Decoder<'a> {
        data: &'a [u8],
        bit: usize,
        value: u32,
        range: u32,
        max_bits: i64,
    }

    impl<'a> Decoder<'a> {
        fn new(data: &'a [u8]) -> Decoder<'a> {
            let mut d = Decoder {
                data,
                bit: 0,
                value: 0,
                range: 1 << 15,
                max_bits: 8 * data.len() as i64 - 15,
            };
            let num_bits = (data.len() * 8).min(15);
            let buf = d.f(num_bits);
            let padded = buf << (15 - num_bits);
            d.value = ((1 << 15) - 1) ^ padded;
            d
        }

        fn f(&mut self, n: usize) -> u32 {
            let mut v = 0;
            for _ in 0..n {
                let byte = self.data.get(self.bit / 8).copied().unwrap_or(0);
                let bit = (byte >> (7 - self.bit % 8)) & 1;
                v = (v << 1) | u32::from(bit);
                self.bit += 1;
            }
            v
        }

        fn read(&mut self, cdf: &mut Cdf, adapt: bool) -> usize {
            let n = cdf.n;
            let mut cur = self.range;
            let mut symbol = 0;
            loop {
                let prev = cur;
                let f = (1 << 15) - cdf.at(symbol);
                cur = (((self.range >> 8) * (f >> 6)) >> 1) + 4 * (n as u32 - symbol as u32 - 1);
                if self.value >= cur {
                    self.range = prev - cur;
                    self.value -= cur;
                    break;
                }
                symbol += 1;
            }
            let bits = 15 - (31 - self.range.leading_zeros());
            self.range <<= bits;
            let num_bits = (bits as i64).min(self.max_bits.max(0)) as usize;
            let new_data = self.f(num_bits);
            let padded = new_data << (bits as usize - num_bits);
            self.value = padded ^ (((self.value + 1) << bits) - 1);
            self.max_bits -= i64::from(bits);
            if adapt {
                cdf.adapt(symbol);
            }
            symbol
        }

        fn bool(&mut self) -> bool {
            let mut cdf = Cdf::new(&[16384]);
            self.read(&mut cdf, false) == 1
        }
    }

    #[test]
    fn the_coder_writes_what_the_specs_decoder_reads() {
        let mut rng = Lcg(3);
        for round in 0..20 {
            let sizes = [2usize, 3, 4, 5, 7, 10, 13, 14, 16];
            let mut encoder_cdfs: Vec<Cdf> = sizes
                .iter()
                .map(|&n| {
                    let mut values = [0u16; 16];
                    for (i, v) in values.iter_mut().enumerate().take(n - 1) {
                        *v = ((i + 1) * 32768 / n) as u16;
                    }
                    let mut cdf = Cdf::new(&values);
                    cdf.n = n;
                    for slot in cdf.v.iter_mut().skip(n - 1) {
                        *slot = 32768;
                    }
                    cdf.v[n] = 0;
                    cdf
                })
                .collect();
            let mut decoder_cdfs = encoder_cdfs.clone();
            let count = 1 + round * 50;
            let mut symbols = Vec::new();
            let mut coder = Coder::new();
            for _ in 0..count {
                let which = rng.next() as usize % sizes.len();
                match rng.next() % 3 {
                    0 => {
                        let bit = rng.next() & 1 == 1;
                        coder.bit(bit);
                        symbols.push((usize::MAX, usize::from(bit)));
                    }
                    _ => {
                        let n = encoder_cdfs[which].n;
                        let s = if rng.next().is_multiple_of(4) {
                            rng.next() as usize % n
                        } else {
                            (rng.next() as usize % n).min(1)
                        };
                        Sink::symbol(&mut coder, &mut encoder_cdfs[which], s);
                        symbols.push((which, s));
                    }
                }
            }
            let bytes = coder.finish();
            let mut decoder = Decoder::new(&bytes);
            for (i, &(which, s)) in symbols.iter().enumerate() {
                let got = if which == usize::MAX {
                    usize::from(decoder.bool())
                } else {
                    decoder.read(&mut decoder_cdfs[which], true)
                };
                assert_eq!(got, s, "round {round} symbol {i}");
            }
            // Spec 8.2.4: the bit at `trailingBitPosition` is one and
            // every bit after it zero (libaom's decoder checks).
            let at = decoder.bit as i64 - (15i64).min(decoder.max_bits + 15);
            let bit_at = |i: i64| (bytes[i as usize / 8] >> (7 - i as usize % 8)) & 1;
            assert!(at >= 0, "round {round}: trailing bit before the data");
            assert_eq!(bit_at(at), 1, "round {round}: {bytes:?}");
            for i in at + 1..8 * bytes.len() as i64 {
                assert_eq!(bit_at(i), 0, "round {round}: bit {i} of {bytes:?}");
            }
        }
        // An empty coder still terminates.
        assert_eq!(Coder::new().finish(), vec![0x80]);
    }

    #[test]
    fn the_adaptation_is_the_specs() {
        let mut cdf = Cdf::new(&[8192, 16384, 24576]);
        assert_eq!(cdf.n, 4);
        cdf.adapt(0);
        // rate 3 + 0 + 0 + min(2, 2) = 5: entries at and past the symbol
        // move towards 32768, earlier ones towards 0.
        assert_eq!(
            &cdf.v[..4],
            &[
                8192 + (24576 >> 5),
                16384 + (16384 >> 5),
                24576 + (8192 >> 5),
                32768
            ]
        );
        assert_eq!(cdf.v[4], 1);
        let mut cdf = Cdf::new(&[8192, 16384, 24576]);
        cdf.adapt(3);
        assert_eq!(
            &cdf.v[..4],
            &[
                8192 - (8192 >> 5),
                16384 - (16384 >> 5),
                24576 - (24576 >> 5),
                32768
            ]
        );
        // The count saturates at 32 and the rate rises with it.
        for _ in 0..40 {
            cdf.adapt(1);
        }
        assert_eq!(cdf.v[4], 32);
        // Costs: an even split of two symbols is one bit each.
        let even = Cdf::new(&[16384]);
        assert_eq!(even.cost(0), 512);
        assert_eq!(even.cost(1), 512);
        let skewed = Cdf::new(&[32000]);
        assert!(skewed.cost(0) < 64);
        assert!(skewed.cost(1) > 5 * 512);
    }

    #[test]
    fn the_default_cdfs_have_the_documented_shapes() {
        let cdfs = Cdfs::new(0);
        assert_eq!(cdfs.y_mode[0][0].n, 13);
        assert_eq!(cdfs.uv_mode[0].n, 14);
        assert_eq!(cdfs.angle_delta[0].n, 7);
        assert_eq!(cdfs.partition_8[0].n, 4);
        assert_eq!(cdfs.partition[2][3].n, 10);
        assert_eq!(cdfs.skip[0].n, 2);
        assert_eq!(cdfs.tx_type[2][12].n, 5);
        assert_eq!(cdfs.txb_skip[4][12].n, 2);
        assert_eq!(cdfs.eob_extra[4][1][8].n, 2);
        assert_eq!(cdfs.dc_sign[1][2].n, 2);
        assert_eq!(cdfs.base_eob[4][1][3].n, 3);
        assert_eq!(cdfs.base[4][1][41].n, 4);
        assert_eq!(cdfs.br[4][1][20].n, 4);
        assert_eq!(cdfs.eob_pt[0][0].n, 5);
        assert_eq!(cdfs.eob_pt[3][1].n, 11);
        // Every CDF is monotone up to 32768 with a zero count.
        for cdf in [
            &cdfs.y_mode[4][4],
            &cdfs.uv_mode[12],
            &cdfs.base[0][0][0],
            &cdfs.br[3][1][7],
        ] {
            for i in 1..cdf.n {
                assert!(cdf.at(i) >= cdf.at(i - 1));
            }
            assert_eq!(cdf.at(cdf.n - 1), 32768);
            assert_eq!(cdf.at(cdf.n), 0);
        }
        // The bands differ.
        assert_ne!(Cdfs::new(0).base[1][0][5].v, Cdfs::new(200).base[1][0][5].v);
    }

    #[test]
    fn the_bit_writer_and_leb128_are_the_specs() {
        let mut b = Bits::new();
        b.put(3, 5);
        b.flag(true);
        b.put(6, 0);
        b.trailing();
        assert_eq!(b.bytes, vec![0b1011_0000, 0b0010_0000]);
        let mut b = Bits::new();
        b.put(8, 0xAB);
        b.align();
        assert_eq!(b.bytes, vec![0xAB]);
        let mut out = Vec::new();
        leb128(&mut out, 0);
        leb128(&mut out, 127);
        leb128(&mut out, 128);
        leb128(&mut out, 300);
        assert_eq!(out, vec![0, 127, 0x80, 0x01, 0xAC, 0x02]);
        let mut out = Vec::new();
        obu(&mut out, OBU_SEQUENCE_HEADER, &[1, 2, 3]);
        assert_eq!(out, vec![0x0A, 3, 1, 2, 3]);
    }

    #[test]
    fn the_eob_classes_partition_the_ends_of_block() {
        for eob in 1..=1024usize {
            let (t, bits) = eob_position(eob);
            let start = eob_group_start(t);
            assert!(start <= eob, "{eob}");
            assert!(eob - start < (1 << bits), "{eob}");
            if t < 11 {
                assert!(eob < eob_group_start(t + 1), "{eob}");
            }
        }
        assert_eq!(eob_position(1), (1, 0));
        assert_eq!(eob_position(3), (3, 1));
        assert_eq!(eob_position(1024), (11, 9));
    }

    #[test]
    fn the_predictors_follow_the_edges() {
        let mut above = [0u8; 65];
        let mut left = [0u8; 65];
        for i in 0..65 {
            above[i] = (10 + i * 3) as u8;
            left[i] = (200 - i * 2) as u8;
        }
        above[0] = 77;
        left[0] = 77;
        let edges = |have_above, have_left| Edges {
            n: 4,
            have_above,
            have_left,
            above,
            left,
        };
        let mut out = [0u8; 16];
        predict_block(V_PRED, &edges(true, true), &mut out);
        assert_eq!(&out[..4], &above[1..5]);
        assert_eq!(&out[12..], &above[1..5]);
        predict_block(H_PRED, &edges(true, true), &mut out);
        assert_eq!(out[0], left[1]);
        assert_eq!(out[15], left[4]);
        predict_block(DC_PRED, &edges(true, true), &mut out);
        let sum: u32 = above[1..5]
            .iter()
            .chain(&left[1..5])
            .map(|&v| u32::from(v))
            .sum();
        assert!(out.iter().all(|&v| u32::from(v) == (sum + 4) / 8));
        predict_block(DC_PRED, &edges(false, false), &mut out);
        assert!(out.iter().all(|&v| v == 128));
        predict_block(DC_PRED, &edges(false, true), &mut out);
        let sum: u32 = left[1..5].iter().map(|&v| u32::from(v)).sum();
        assert!(out.iter().all(|&v| u32::from(v) == (sum + 2) / 4));
        // D45 walks up and right: row i, column j is AboveRow[i + j + 1].
        predict_block(D45_PRED, &edges(true, true), &mut out);
        for i in 0..4 {
            for j in 0..4 {
                assert_eq!(out[i * 4 + j], above[i + j + 2], "{i} {j}");
            }
        }
        // D135 walks down and right from the corner.
        predict_block(D135_PRED, &edges(true, true), &mut out);
        for i in 0..4 {
            for j in 0..4 {
                let want = if j >= i { above[j - i] } else { left[i - j] };
                assert_eq!(out[i * 4 + j], want, "{i} {j}");
            }
        }
        // Paeth picks the neighbour closest to the gradient guess.
        predict_block(PAETH_PRED, &edges(true, true), &mut out);
        let (top, side, corner) = (i32::from(above[1]), i32::from(left[1]), 77);
        let base = top + side - corner;
        let want = if (base - side).abs() <= (base - top).abs()
            && (base - side).abs() <= (base - corner).abs()
        {
            side
        } else if (base - top).abs() <= (base - corner).abs() {
            top
        } else {
            corner
        };
        assert_eq!(i32::from(out[0]), want);
        // Smooth blends towards the far edges.
        predict_block(SMOOTH_PRED, &edges(true, true), &mut out);
        // The weight at the first of four is 255; the rest of 256 goes
        // to the far edge.
        let want = (255 * i32::from(above[1])
            + i32::from(left[4])
            + 255 * i32::from(left[1])
            + i32::from(above[4])
            + 256)
            >> 9;
        assert_eq!(i32::from(out[0]), want);
        assert_eq!(mode_tx_type(V_PRED, Size::S8), TxType::AdstDct);
        assert_eq!(mode_tx_type(H_PRED, Size::S16), TxType::DctAdst);
        assert_eq!(mode_tx_type(V_PRED, Size::S32), TxType::DctDct);
        assert_eq!(mode_tx_type(D45_PRED, Size::S4), TxType::DctDct);
    }

    #[test]
    fn the_headers_are_the_documented_bits() {
        let g = Geometry::new(64, 48).unwrap();
        let seq = sequence_header(&g);
        assert_eq!(seq.len(), 12);
        let header = frame_header(&g, 21);
        // Nine flags and a byte of quantizer, six-bit filter levels and
        // the rest: the header is byte-aligned at a known length.
        assert_eq!(header.used, 0);
        assert_eq!(header.bytes.len(), 5);
        assert_eq!(qindex(92), 22);
    }
}

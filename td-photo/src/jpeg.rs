//! Baseline JPEG decoder for the camera's embedded previews: 8-bit, one or
//! three components with sampling factors 1 or 2, Huffman DC and AC
//! tables, restart intervals, one interleaved scan. Progressive,
//! arithmetic, lossless, hierarchical, 12-bit and multi-scan streams are
//! refused by name. It decodes at 1/1, 1/2, 1/4 or 1/8 scale with a
//! reduced inverse DCT over the first N coefficients of each axis.
//!
//! The arithmetic is a contract shared with `tests/fixtures/jpeg_ref.py`,
//! so a decode is held to the oracle's hash and not to a tolerance: the
//! transform is separable `f64` over literal constants, rows then columns,
//! sums in ascending frequency; a sample is `floor(v + 128 + 0.5)` clamped
//! to 0..=255; chroma is replicated to the luma grid; and the JFIF colour
//! constants are applied with the same rounding. Nothing here reads a
//! file, the environment or a clock.

use std::fmt;

use crate::image::{Rgb8, MAX_AXIS, MAX_IMAGE_PIXELS};

/// The most samples a preview's component planes may hold together,
/// block padding included: the decoder's working set for one frame.
pub const MAX_PREVIEW_SAMPLES: usize = 128 << 20;
/// The most quantisation and Huffman table definitions one stream may
/// carry, so a stream of table segments costs bounded work.
pub const MAX_TABLE_DEFINITIONS: usize = 32;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// No SOI marker, or a byte where a marker should be.
    NotJpeg,
    /// The data ends inside a segment or inside the entropy-coded scan.
    Truncated,
    /// A frame type this decoder does not implement, by its SOF marker.
    Unsupported(u8),
    /// Sample precision other than 8.
    Precision(u8),
    /// A component count other than 1 or 3, or a repeated component id.
    Components(u8),
    /// A sampling factor other than 1 or 2.
    Sampling,
    /// A quantisation or Huffman table is missing or malformed.
    Table,
    /// A code that is not in its table, or a category past baseline's.
    Huffman,
    /// A segment shorter than its own header.
    Segment,
    /// A zero axis, one past the ceiling, or a frame past the budget.
    Axis { width: usize, height: usize },
    /// Scan parameters this decoder does not implement.
    Scan,
    /// A restart marker was due and something else was found.
    Restart,
    /// A coefficient run past the end of its block.
    Run,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotJpeg => f.write_str("not a JPEG stream"),
            Self::Truncated => f.write_str("JPEG data ends early"),
            Self::Unsupported(m) => write!(f, "JPEG frame type {m:#04x} is not baseline"),
            Self::Precision(p) => write!(f, "JPEG precision {p} is not 8"),
            Self::Components(n) => write!(f, "JPEG component layout {n} is not 1 or 3 distinct"),
            Self::Sampling => f.write_str("JPEG sampling factor is not 1 or 2"),
            Self::Table => f.write_str("JPEG table missing or malformed"),
            Self::Huffman => f.write_str("JPEG code not in its table"),
            Self::Segment => f.write_str("JPEG segment malformed"),
            Self::Axis { width, height } => write!(f, "JPEG frame {width}x{height} refused"),
            Self::Scan => f.write_str("JPEG scan is not one interleaved baseline scan"),
            Self::Restart => f.write_str("JPEG restart marker missing"),
            Self::Run => f.write_str("JPEG coefficient run past its block"),
        }
    }
}

impl std::error::Error for Error {}

/// The reduction the inverse DCT applies: output samples per block axis.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scale {
    Full,
    Half,
    Quarter,
    Eighth,
}

impl Scale {
    /// Output samples per block axis: 8, 4, 2 or 1.
    pub const fn n(self) -> usize {
        match self {
            Self::Full => 8,
            Self::Half => 4,
            Self::Quarter => 2,
            Self::Eighth => 1,
        }
    }

    /// The coarsest scale whose long edge still covers `long_edge`.
    pub fn covering(width: usize, height: usize, long_edge: usize) -> Self {
        let long = width.max(height);
        for scale in [Self::Eighth, Self::Quarter, Self::Half] {
            if scaled(long, scale) >= long_edge {
                return scale;
            }
        }
        Self::Full
    }
}

/// An axis after `scale`: `ceil(axis * n / 8)`.
pub fn scaled(axis: usize, scale: Scale) -> usize {
    (axis.min(MAX_AXIS) * scale.n()).div_ceil(8)
}

/// What the frame header says, without decoding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    pub width: usize,
    pub height: usize,
    pub components: u8,
    /// Sampling factors (horizontal, vertical) of the first component.
    pub sampling: (u8, u8),
}

/// Reads the frame header of a baseline stream.
pub fn header(data: &[u8]) -> Result<Header, Error> {
    let parsed = parse(data, true)?;
    let frame = parsed.frame;
    let first = frame.components.first().ok_or(Error::Components(0))?;
    Ok(Header {
        width: frame.width,
        height: frame.height,
        components: frame.components.len() as u8,
        sampling: (first.h, first.v),
    })
}

/// Decodes the stream to RGB at `scale`.
pub fn decode(data: &[u8], scale: Scale) -> Result<Rgb8, Error> {
    decode_with(data, scale, &mut |_| {})
}

// --------------------------------------------------------------- segments

#[derive(Clone, Copy, Debug)]
struct Component {
    id: u8,
    h: u8,
    v: u8,
    tq: u8,
}

#[derive(Clone, Debug)]
struct Frame {
    width: usize,
    height: usize,
    components: Vec<Component>,
}

#[derive(Clone, Copy)]
struct ScanComponent {
    index: usize,
    dc: u8,
    ac: u8,
}

struct Tables {
    quant: [Option<[u16; 64]>; 4],
    dc: [Option<Lut>; 4],
    ac: [Option<Lut>; 4],
}

struct Parsed {
    frame: Frame,
    tables: Tables,
    restart: usize,
    scan: Vec<ScanComponent>,
    /// Where the entropy-coded data begins.
    data_start: usize,
}

fn u16_at(data: &[u8], at: usize) -> Result<u16, Error> {
    let hi = *data.get(at).ok_or(Error::Truncated)?;
    let lo = *data
        .get(at.checked_add(1).ok_or(Error::Truncated)?)
        .ok_or(Error::Truncated)?;
    Ok(u16::from_be_bytes([hi, lo]))
}

/// Walks the marker segments up to the frame header (`header_only`) or the
/// start of scan.
fn parse(data: &[u8], header_only: bool) -> Result<Parsed, Error> {
    if !data.starts_with(&[0xFF, 0xD8]) {
        return Err(Error::NotJpeg);
    }
    let mut pos = 2usize;
    let mut frame: Option<Frame> = None;
    let mut tables = Tables {
        quant: [None; 4],
        dc: [const { None }; 4],
        ac: [const { None }; 4],
    };
    let mut restart = 0usize;
    let mut definitions = 0usize;
    loop {
        if *data.get(pos).ok_or(Error::Truncated)? != 0xFF {
            return Err(Error::NotJpeg);
        }
        pos += 1;
        let mut marker = *data.get(pos).ok_or(Error::Truncated)?;
        pos += 1;
        // Fill bytes before a marker are allowed.
        while marker == 0xFF {
            marker = *data.get(pos).ok_or(Error::Truncated)?;
            pos += 1;
        }
        match marker {
            0xD8 | 0xD0..=0xD7 | 0x01 => continue,
            // EOI before any frame: nothing to decode.
            0xD9 => return Err(Error::Segment),
            _ => {}
        }
        let len = usize::from(u16_at(data, pos)?);
        if len < 2 {
            return Err(Error::Segment);
        }
        let end = pos.checked_add(len).ok_or(Error::Truncated)?;
        let segment = data.get(pos + 2..end).ok_or(Error::Truncated)?;
        match marker {
            0xDB => parse_quant(segment, &mut tables, &mut definitions)?,
            0xC4 => parse_huffman(segment, &mut tables, &mut definitions)?,
            0xC0 | 0xC1 => {
                if frame.is_some() {
                    return Err(Error::Segment);
                }
                let parsed = parse_frame(segment)?;
                if header_only {
                    return Ok(Parsed {
                        frame: parsed,
                        tables,
                        restart,
                        scan: Vec::new(),
                        data_start: end,
                    });
                }
                frame = Some(parsed);
            }
            // Other frame types, arithmetic conditioning (DAC) and the
            // hierarchical markers (DHP, EXP) name a stream this decoder
            // does not implement, whatever frame follows them.
            0xC2 | 0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCC..=0xCF | 0xDE | 0xDF => {
                return Err(Error::Unsupported(marker));
            }
            0xDD => restart = usize::from(u16_at(segment, 0)?),
            0xDA => {
                let frame = frame.ok_or(Error::Segment)?;
                let scan = parse_scan(segment, &frame, &tables)?;
                return Ok(Parsed {
                    frame,
                    tables,
                    restart,
                    scan,
                    data_start: end,
                });
            }
            // APPn, COM, DNL and the rest carry nothing this decoder needs.
            _ => {}
        }
        pos = end;
    }
}

fn count_definition(definitions: &mut usize) -> Result<(), Error> {
    *definitions += 1;
    if *definitions > MAX_TABLE_DEFINITIONS {
        return Err(Error::Table);
    }
    Ok(())
}

fn parse_quant(segment: &[u8], tables: &mut Tables, definitions: &mut usize) -> Result<(), Error> {
    let mut at = 0usize;
    while at < segment.len() {
        count_definition(definitions)?;
        let pq_tq = *segment.get(at).ok_or(Error::Segment)?;
        let (precision, id) = (pq_tq >> 4, usize::from(pq_tq & 15));
        at += 1;
        let mut table = [0u16; 64];
        match precision {
            0 => {
                let bytes = segment.get(at..at + 64).ok_or(Error::Segment)?;
                for (slot, b) in table.iter_mut().zip(bytes) {
                    *slot = u16::from(*b);
                }
                at += 64;
            }
            1 => {
                let bytes = segment.get(at..at + 128).ok_or(Error::Segment)?;
                for (slot, pair) in table.iter_mut().zip(bytes.as_chunks::<2>().0) {
                    *slot = u16::from_be_bytes(*pair);
                }
                at += 128;
            }
            _ => return Err(Error::Table),
        }
        if table.contains(&0) {
            return Err(Error::Table);
        }
        *tables.quant.get_mut(id).ok_or(Error::Table)? = Some(table);
    }
    Ok(())
}

fn parse_huffman(
    segment: &[u8],
    tables: &mut Tables,
    definitions: &mut usize,
) -> Result<(), Error> {
    let mut at = 0usize;
    while at < segment.len() {
        count_definition(definitions)?;
        let tc_th = *segment.get(at).ok_or(Error::Segment)?;
        let (class, id) = (tc_th >> 4, usize::from(tc_th & 15));
        let counts: &[u8; 16] = segment
            .get(at + 1..at + 17)
            .and_then(|s| s.try_into().ok())
            .ok_or(Error::Segment)?;
        let total: usize = counts.iter().map(|c| usize::from(*c)).sum();
        let symbols = segment
            .get(at + 17..at + 17 + total)
            .ok_or(Error::Segment)?;
        at += 17 + total;
        let lut = Lut::build(counts, symbols)?;
        let slot = match class {
            0 => tables.dc.get_mut(id),
            1 => tables.ac.get_mut(id),
            _ => None,
        };
        *slot.ok_or(Error::Table)? = Some(lut);
    }
    Ok(())
}

fn parse_frame(segment: &[u8]) -> Result<Frame, Error> {
    let precision = *segment.first().ok_or(Error::Segment)?;
    if precision != 8 {
        return Err(Error::Precision(precision));
    }
    let height = usize::from(u16_at(segment, 1)?);
    let width = usize::from(u16_at(segment, 3)?);
    let count = *segment.get(5).ok_or(Error::Segment)?;
    if count != 1 && count != 3 {
        return Err(Error::Components(count));
    }
    let refused = Error::Axis { width, height };
    if width == 0 || height == 0 || width > MAX_AXIS || height > MAX_AXIS {
        return Err(refused);
    }
    // The full-scale output is one image buffer, so its ceiling applies
    // here too, ahead of the plane budget below that a 4:2:0 frame can
    // meet with more pixels than the buffer may hold.
    if width
        .checked_mul(height)
        .is_none_or(|n| n > MAX_IMAGE_PIXELS)
    {
        return Err(refused);
    }
    let mut components = Vec::with_capacity(usize::from(count));
    for k in 0..usize::from(count) {
        let id = *segment.get(6 + 3 * k).ok_or(Error::Segment)?;
        let hv = *segment.get(7 + 3 * k).ok_or(Error::Segment)?;
        let tq = *segment.get(8 + 3 * k).ok_or(Error::Segment)?;
        let (h, v) = (hv >> 4, hv & 15);
        if !(1..=2).contains(&h) || !(1..=2).contains(&v) {
            return Err(Error::Sampling);
        }
        if tq > 3 {
            return Err(Error::Table);
        }
        if components.iter().any(|c: &Component| c.id == id) {
            return Err(Error::Components(count));
        }
        components.push(Component { id, h, v, tq });
    }
    let frame = Frame {
        width,
        height,
        components,
    };
    // The planes are whole MCUs of every component: that, not the frame's
    // pixel count, is what a decode allocates. Axes are at most MAX_AXIS,
    // so each product fits; the sum is checked all the same.
    let (mcus_x, mcus_y) = frame.mcu_grid();
    let mut total = 0usize;
    for comp in &frame.components {
        let (bh, bv) = frame.blocks(comp);
        let plane = (mcus_x * bh * 8)
            .checked_mul(mcus_y * bv * 8)
            .ok_or(refused.clone())?;
        total = total.checked_add(plane).ok_or(refused.clone())?;
    }
    if total > MAX_PREVIEW_SAMPLES {
        return Err(refused);
    }
    Ok(frame)
}

impl Frame {
    /// MCUs across and down. A lone component is coded non-interleaved,
    /// one block per MCU whatever its sampling factors say (T.81 A.2.2,
    /// and how libjpeg reads such files).
    fn mcu_grid(&self) -> (usize, usize) {
        if self.components.len() == 1 {
            return (self.width.div_ceil(8), self.height.div_ceil(8));
        }
        let (hmax, vmax) = self.max_sampling();
        (
            self.width.div_ceil(8 * hmax),
            self.height.div_ceil(8 * vmax),
        )
    }

    /// Blocks across and down one MCU of `comp`.
    fn blocks(&self, comp: &Component) -> (usize, usize) {
        if self.components.len() == 1 {
            (1, 1)
        } else {
            (usize::from(comp.h), usize::from(comp.v))
        }
    }

    fn max_sampling(&self) -> (usize, usize) {
        let hmax = self.components.iter().map(|c| c.h).max().unwrap_or(1);
        let vmax = self.components.iter().map(|c| c.v).max().unwrap_or(1);
        (usize::from(hmax), usize::from(vmax))
    }
}

fn parse_scan(segment: &[u8], frame: &Frame, tables: &Tables) -> Result<Vec<ScanComponent>, Error> {
    let count = usize::from(*segment.first().ok_or(Error::Segment)?);
    if count != frame.components.len() {
        return Err(Error::Scan);
    }
    let mut scan = Vec::with_capacity(count);
    for k in 0..count {
        let id = *segment.get(1 + 2 * k).ok_or(Error::Segment)?;
        let td_ta = *segment.get(2 + 2 * k).ok_or(Error::Segment)?;
        let (dc, ac) = (td_ta >> 4, td_ta & 15);
        let index = frame
            .components
            .iter()
            .position(|c| c.id == id)
            .ok_or(Error::Scan)?;
        // T.81 B.2.3: the scan's components come in the frame's order,
        // and the colour step relies on it.
        if index != k {
            return Err(Error::Scan);
        }
        if tables.dc.get(usize::from(dc)).is_none_or(Option::is_none)
            || tables.ac.get(usize::from(ac)).is_none_or(Option::is_none)
        {
            return Err(Error::Table);
        }
        scan.push(ScanComponent { index, dc, ac });
    }
    let at = 1 + 2 * count;
    let ss = *segment.get(at).ok_or(Error::Segment)?;
    let se = *segment.get(at + 1).ok_or(Error::Segment)?;
    let ah_al = *segment.get(at + 2).ok_or(Error::Segment)?;
    if ss != 0 || se != 63 || ah_al != 0 {
        return Err(Error::Scan);
    }
    Ok(scan)
}

// ---------------------------------------------------------------- entropy

/// A canonical Huffman table as one 16-bit lookup: `length << 8 | symbol`,
/// zero for a bit pattern no code begins.
struct Lut {
    entries: Vec<u16>,
}

impl Lut {
    fn build(counts: &[u8; 16], symbols: &[u8]) -> Result<Self, Error> {
        let mut entries = vec![0u16; 1 << 16];
        let mut code: u32 = 0;
        let mut next = 0usize;
        for (i, count) in counts.iter().enumerate() {
            let len = i as u32 + 1;
            for _ in 0..*count {
                let symbol = *symbols.get(next).ok_or(Error::Table)?;
                next += 1;
                if code >= 1 << len {
                    return Err(Error::Table);
                }
                let start = (code << (16 - len)) as usize;
                let end = ((code + 1) << (16 - len)) as usize;
                for entry in entries.get_mut(start..end).ok_or(Error::Table)? {
                    *entry = (len as u16) << 8 | u16::from(symbol);
                }
                code += 1;
            }
            // T.81 C.2 reserves the all-ones word of every length, which is
            // what lets padding be told from a symbol; the code after the
            // last of this length must therefore still fit the length.
            if *count > 0 && code >= 1 << len {
                return Err(Error::Table);
            }
            code <<= 1;
        }
        Ok(Self { entries })
    }

    fn decode(&self, bits: &mut Bits<'_>) -> Result<u8, Error> {
        let entry = self
            .entries
            .get(usize::from(bits.peek16()))
            .copied()
            .unwrap_or(0);
        if entry == 0 {
            return Err(Error::Huffman);
        }
        bits.consume(u32::from(entry >> 8));
        Ok((entry & 0xFF) as u8)
    }
}

/// MSB-first reader over entropy-coded data: `FF 00` is one `FF` byte, any
/// other marker stops the bytes and zeros follow, so a stream that ends
/// early decodes to the end and is then reported by `overrun`.
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
    acc: u64,
    count: u32,
    stopped: bool,
    /// Bits delivered from real bytes and bits consumed, since the last
    /// restart.
    delivered: u64,
    consumed: u64,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8], pos: usize) -> Self {
        Self {
            data,
            pos,
            acc: 0,
            count: 0,
            stopped: false,
            delivered: 0,
            consumed: 0,
        }
    }

    fn next_byte(&mut self) -> u8 {
        if self.stopped {
            return 0;
        }
        match self.data.get(self.pos) {
            None => {
                self.stopped = true;
                0
            }
            Some(&0xFF) => match self.data.get(self.pos + 1) {
                Some(&0x00) => {
                    self.pos += 2;
                    self.delivered += 8;
                    0xFF
                }
                _ => {
                    self.stopped = true;
                    0
                }
            },
            Some(&b) => {
                self.pos += 1;
                self.delivered += 8;
                b
            }
        }
    }

    fn refill(&mut self) {
        while self.count <= 56 {
            let b = self.next_byte();
            self.acc |= u64::from(b) << (56 - self.count);
            self.count += 8;
        }
    }

    fn peek16(&mut self) -> u16 {
        if self.count < 16 {
            self.refill();
        }
        (self.acc >> 48) as u16
    }

    fn consume(&mut self, n: u32) {
        self.acc <<= n;
        self.count = self.count.saturating_sub(n);
        self.consumed += u64::from(n);
    }

    fn get(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        if self.count < n {
            self.refill();
        }
        let value = (self.acc >> (64 - n)) as u32;
        self.consume(n);
        value
    }

    fn overrun(&self) -> bool {
        self.consumed > self.delivered
    }

    /// Whether a whole delivered byte went unconsumed: more entropy-coded
    /// data before the marker than the MCUs took, which padding (under
    /// eight bits) never is.
    fn excess(&self) -> bool {
        self.delivered.saturating_sub(self.consumed) >= 8
    }

    /// The marker at the reader's position, past any fill bytes, with the
    /// position just after it; `None` when the data ends or the next byte
    /// is not a marker.
    fn marker(&self) -> Option<(u8, usize)> {
        let mut at = self.pos;
        while self.data.get(at) == Some(&0xFF) && self.data.get(at + 1) == Some(&0xFF) {
            at += 1;
        }
        match (self.data.get(at), self.data.get(at + 1)) {
            (Some(&0xFF), Some(&m)) if m != 0x00 => Some((m, at + 2)),
            _ => None,
        }
    }

    /// At a restart point: the rest of the byte is padding, the `RSTn`
    /// marker with the expected `n` follows (after any fill bytes), and the
    /// predictors start over.
    fn restart(&mut self, expected: u8) -> Result<(), Error> {
        if self.overrun() {
            return Err(Error::Truncated);
        }
        if self.excess() {
            return Err(Error::Restart);
        }
        // Every real byte before the marker was delivered, so the marker is
        // the next thing in the data whether or not the bytes have stopped.
        match self.marker() {
            Some((m, after)) if m == 0xD0 + expected => self.pos = after,
            _ => return Err(Error::Restart),
        }
        self.acc = 0;
        self.count = 0;
        self.stopped = false;
        self.delivered = 0;
        self.consumed = 0;
        Ok(())
    }

    /// After the last MCU: the scan is complete, only padding remains, and
    /// EOI follows. A second scan or anything else in its place is refused;
    /// what follows EOI is not read.
    fn finish(&mut self) -> Result<(), Error> {
        if self.overrun() {
            return Err(Error::Truncated);
        }
        if self.excess() {
            return Err(Error::Scan);
        }
        match self.marker() {
            Some((0xD9, _)) => Ok(()),
            Some(_) => Err(Error::Scan),
            None if self.data.len().saturating_sub(self.pos) < 2 => Err(Error::Truncated),
            None => Err(Error::Scan),
        }
    }
}

/// JPEG lossless sign extension of a `t`-bit magnitude category.
fn extend(value: u32, t: u8) -> i32 {
    if t == 0 {
        0
    } else if value >= 1 << (t - 1) {
        value as i32
    } else {
        value as i32 - (1 << t) + 1
    }
}

/// Natural-order index of each zigzag position.
const ZIGZAG: [u8; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// One block's dequantised coefficients in natural order.
fn decode_block(
    bits: &mut Bits<'_>,
    dc: &Lut,
    ac: &Lut,
    quant: &[u16; 64],
    pred: &mut i32,
    block: &mut [i32; 64],
) -> Result<(), Error> {
    block.fill(0);
    let t = dc.decode(bits)?;
    // Baseline categories: DC 0..=11, AC 1..=10, so a coefficient times a
    // 16-bit quantiser fits i32.
    if t > 11 {
        return Err(Error::Huffman);
    }
    let diff = extend(bits.get(u32::from(t)), t);
    // A quantised DC of 8-bit samples has 11 bits; a predictor past 16 is
    // a corrupt stream, and the bound keeps the product in i32.
    *pred = pred
        .checked_add(diff)
        .filter(|p| (i32::from(i16::MIN)..=i32::from(i16::MAX)).contains(p))
        .ok_or(Error::Huffman)?;
    block[0] = pred.saturating_mul(i32::from(quant[0]));
    let mut k = 1usize;
    while k < 64 {
        let rs = ac.decode(bits)?;
        let (run, size) = (usize::from(rs >> 4), rs & 15);
        if size == 0 {
            // Only EOB (0x00) and ZRL (0xF0) have no magnitude; ZRL is
            // sixteen zeros that must fit the block.
            match run {
                0 => break,
                15 if k <= 48 => {
                    k += 16;
                    continue;
                }
                15 => return Err(Error::Run),
                _ => return Err(Error::Huffman),
            }
        }
        if size > 10 {
            return Err(Error::Huffman);
        }
        k += run;
        if k > 63 {
            return Err(Error::Run);
        }
        let value = extend(bits.get(u32::from(size)), size);
        let natural = usize::from(*ZIGZAG.get(k).ok_or(Error::Run)?);
        let q = i32::from(*quant.get(k).ok_or(Error::Run)?);
        if let Some(slot) = block.get_mut(natural) {
            *slot = value * q;
        }
        k += 1;
    }
    Ok(())
}

// ------------------------------------------------------------------- idct

// TN[x][u] = (c(u) / 2) cos((2x + 1) u pi / (2N)) as `jpeg_ref.py` prints
// them; the transpose so the inner sums zip over u.
const T8: [[f64; 8]; 8] = [
    [
        0.35355339059327373,
        0.4903926402016152,
        0.46193976625564337,
        0.4157348061512726,
        0.3535533905932738,
        0.27778511650980114,
        0.19134171618254492,
        0.09754516100806417,
    ],
    [
        0.35355339059327373,
        0.4157348061512726,
        0.19134171618254492,
        -0.0975451610080641,
        -0.35355339059327373,
        -0.4903926402016152,
        -0.4619397662556434,
        -0.2777851165098011,
    ],
    [
        0.35355339059327373,
        0.27778511650980114,
        -0.19134171618254486,
        -0.4903926402016152,
        -0.35355339059327384,
        0.09754516100806415,
        0.46193976625564326,
        0.41573480615127273,
    ],
    [
        0.35355339059327373,
        0.09754516100806417,
        -0.46193976625564337,
        -0.2777851165098011,
        0.3535533905932737,
        0.41573480615127273,
        -0.19134171618254495,
        -0.4903926402016153,
    ],
    [
        0.35355339059327373,
        -0.0975451610080641,
        -0.4619397662556434,
        0.2777851165098009,
        0.35355339059327384,
        -0.41573480615127256,
        -0.19134171618254528,
        0.4903926402016152,
    ],
    [
        0.35355339059327373,
        -0.277785116509801,
        -0.19134171618254517,
        0.4903926402016152,
        -0.35355339059327334,
        -0.09754516100806401,
        0.46193976625564337,
        -0.4157348061512725,
    ],
    [
        0.35355339059327373,
        -0.4157348061512727,
        0.191341716182545,
        0.09754516100806439,
        -0.35355339059327356,
        0.4903926402016153,
        -0.4619397662556432,
        0.27778511650980076,
    ],
    [
        0.35355339059327373,
        -0.4903926402016152,
        0.46193976625564326,
        -0.41573480615127256,
        0.3535533905932733,
        -0.27778511650980076,
        0.19134171618254478,
        -0.09754516100806429,
    ],
];
const T4: [[f64; 4]; 4] = [
    [
        0.35355339059327373,
        0.46193976625564337,
        0.3535533905932738,
        0.19134171618254492,
    ],
    [
        0.35355339059327373,
        0.19134171618254492,
        -0.35355339059327373,
        -0.4619397662556434,
    ],
    [
        0.35355339059327373,
        -0.19134171618254486,
        -0.35355339059327384,
        0.46193976625564326,
    ],
    [
        0.35355339059327373,
        -0.46193976625564337,
        0.3535533905932737,
        -0.19134171618254495,
    ],
];
const T2: [[f64; 2]; 2] = [
    [0.35355339059327373, 0.3535533905932738],
    [0.35355339059327373, -0.35355339059327373],
];
const T1: [[f64; 1]; 1] = [[0.35355339059327373]];

/// The N-point reduced inverse DCT of the block's first N x N
/// coefficients into `out` (N * N samples, row-major).
fn transform<const N: usize>(coef: &[i32; 64], table: &[[f64; N]; N], out: &mut [u8]) {
    // tmp[x][v]: the row transform of coefficient row v.
    let mut tmp = [[0.0f64; N]; N];
    for (v, row) in coef.as_chunks::<8>().0.iter().take(N).enumerate() {
        for (tx, tmp_x) in table.iter().zip(tmp.iter_mut()) {
            let mut sum = 0.0f64;
            for (t, c) in tx.iter().zip(row.iter()) {
                sum += t * f64::from(*c);
            }
            if let Some(slot) = tmp_x.get_mut(v) {
                *slot = sum;
            }
        }
    }
    for (y, ty) in table.iter().enumerate() {
        for (x, tmp_x) in tmp.iter().enumerate() {
            let mut sum = 0.0f64;
            for (t, m) in ty.iter().zip(tmp_x.iter()) {
                sum += t * m;
            }
            let value = (sum + 128.0 + 0.5).floor().clamp(0.0, 255.0);
            if let Some(slot) = out.get_mut(y * N + x) {
                *slot = value as u8;
            }
        }
    }
}

fn idct(coef: &[i32; 64], scale: Scale, out: &mut [u8]) {
    match scale {
        Scale::Full => transform(coef, &T8, out),
        Scale::Half => transform(coef, &T4, out),
        Scale::Quarter => transform(coef, &T2, out),
        Scale::Eighth => transform(coef, &T1, out),
    }
}

// ----------------------------------------------------------------- decode

struct Plane<'t> {
    width: usize,
    height: usize,
    samples: Vec<u8>,
    /// Blocks per MCU along each axis.
    bh: usize,
    bv: usize,
    /// Replication shifts from this plane to the output grid.
    xshift: u32,
    yshift: u32,
    quant: [u16; 64],
    dc: &'t Lut,
    ac: &'t Lut,
    pred: i32,
}

/// Decodes the stream to RGB at `scale`, handing every block's dequantised
/// coefficients (natural order, decode order) to `sink` on the way; the
/// oracle tests hash them.
pub fn decode_with(
    data: &[u8],
    scale: Scale,
    sink: &mut dyn FnMut(&[i32; 64]),
) -> Result<Rgb8, Error> {
    let parsed = parse(data, false)?;
    let frame = &parsed.frame;
    let n = scale.n();
    let single = frame.components.len() == 1;
    let (hmax, vmax) = frame.max_sampling();
    let (mcus_x, mcus_y) = frame.mcu_grid();
    let mut planes: Vec<Plane<'_>> = Vec::with_capacity(parsed.scan.len());
    for sc in &parsed.scan {
        let comp = frame.components.get(sc.index).ok_or(Error::Scan)?;
        let (bh, bv) = frame.blocks(comp);
        let width = mcus_x * bh * n;
        let height = mcus_y * bv * n;
        let quant = tables_quant(&parsed.tables, comp.tq)?;
        let dc = parsed
            .tables
            .dc
            .get(usize::from(sc.dc))
            .and_then(Option::as_ref)
            .ok_or(Error::Table)?;
        let ac = parsed
            .tables
            .ac
            .get(usize::from(sc.ac))
            .and_then(Option::as_ref)
            .ok_or(Error::Table)?;
        planes.push(Plane {
            width,
            height,
            samples: vec![0u8; width * height],
            bh,
            bv,
            xshift: if single {
                0
            } else {
                (hmax / usize::from(comp.h)) as u32 - 1
            },
            yshift: if single {
                0
            } else {
                (vmax / usize::from(comp.v)) as u32 - 1
            },
            quant,
            dc,
            ac,
            pred: 0,
        });
    }
    let mut bits = Bits::new(data, parsed.data_start);
    let mut block = [0i32; 64];
    let mut out_block = [0u8; 64];
    let mut mcu = 0usize;
    let mut intervals = 0u8;
    for my in 0..mcus_y {
        // A scan that ran out is reported after the row it ran out in, not
        // after every row a hostile frame declared.
        if bits.overrun() {
            return Err(Error::Truncated);
        }
        for mx in 0..mcus_x {
            if parsed.restart != 0 && mcu != 0 && mcu.is_multiple_of(parsed.restart) {
                bits.restart(intervals % 8)?;
                intervals = intervals.wrapping_add(1);
                for plane in &mut planes {
                    plane.pred = 0;
                }
            }
            mcu += 1;
            for plane in &mut planes {
                for by in 0..plane.bv {
                    for bx in 0..plane.bh {
                        decode_block(
                            &mut bits,
                            plane.dc,
                            plane.ac,
                            &plane.quant,
                            &mut plane.pred,
                            &mut block,
                        )?;
                        sink(&block);
                        idct(&block, scale, &mut out_block);
                        let ox = (mx * plane.bh + bx) * n;
                        let oy = (my * plane.bv + by) * n;
                        for (yy, src) in out_block.chunks_exact(n).take(n).enumerate() {
                            let start = (oy + yy) * plane.width + ox;
                            if let Some(dst) = plane.samples.get_mut(start..start + n) {
                                dst.copy_from_slice(src);
                            }
                        }
                    }
                }
            }
        }
    }
    bits.finish()?;
    let (ow, oh) = (scaled(frame.width, scale), scaled(frame.height, scale));
    let mut image = Rgb8::new(ow, oh).ok_or(Error::Axis {
        width: frame.width,
        height: frame.height,
    })?;
    let sample = |plane: &Plane<'_>, x: usize, y: usize| -> u8 {
        let px = (x >> plane.xshift).min(plane.width.saturating_sub(1));
        let py = (y >> plane.yshift).min(plane.height.saturating_sub(1));
        plane
            .samples
            .get(py * plane.width + px)
            .copied()
            .unwrap_or(0)
    };
    match planes.as_slice() {
        [grey] => {
            for (y, row) in image.data.chunks_exact_mut(ow * 3).enumerate() {
                for (x, px) in row.as_chunks_mut::<3>().0.iter_mut().enumerate() {
                    let v = sample(grey, x, y);
                    *px = [v, v, v];
                }
            }
        }
        [py, pb, pr] => {
            for (y, row) in image.data.chunks_exact_mut(ow * 3).enumerate() {
                for (x, px) in row.as_chunks_mut::<3>().0.iter_mut().enumerate() {
                    let yy = f64::from(sample(py, x, y));
                    let cb = f64::from(sample(pb, x, y)) - 128.0;
                    let cr = f64::from(sample(pr, x, y)) - 128.0;
                    let r = (yy + 1.402 * cr + 0.5).floor().clamp(0.0, 255.0);
                    let g = (yy - 0.344136 * cb - 0.714136 * cr + 0.5)
                        .floor()
                        .clamp(0.0, 255.0);
                    let b = (yy + 1.772 * cb + 0.5).floor().clamp(0.0, 255.0);
                    *px = [r as u8, g as u8, b as u8];
                }
            }
        }
        _ => return Err(Error::Components(planes.len() as u8)),
    }
    Ok(image)
}

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

fn tables_quant(tables: &Tables, id: u8) -> Result<[u16; 64], Error> {
    tables
        .quant
        .get(usize::from(id))
        .copied()
        .flatten()
        .ok_or(Error::Table)
}

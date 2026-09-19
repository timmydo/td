//! Baseline JPEG decoder for the camera's embedded previews: 8-bit, one or
//! three components with sampling factors 1 or 2, Huffman DC and AC
//! tables, restart intervals, one interleaved scan. Progressive,
//! arithmetic, lossless, hierarchical, 12-bit and multi-scan streams are
//! refused by name. It decodes at 1/1, 1/2, 1/4 or 1/8 scale with a
//! reduced inverse DCT over the first N coefficients of each axis. And a
//! baseline encoder for export (`Encoder`, at the end): YCbCr 4:4:4 over
//! the standard tables, fed rows and drained bytes a band at a time.
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
    /// The encoder was given rows that are not its width, or more rows
    /// than its height, or finished short of it.
    Rows,
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
            Self::Rows => f.write_str("JPEG encoder given rows that are not its width or height"),
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
    pub fn new(width: usize, height: usize, quality: u8, threads: usize) -> Result<Self, Error> {
        if width == 0 || height == 0 || width > MAX_AXIS || height > MAX_AXIS {
            return Err(Error::Axis { width, height });
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
    pub fn encode_rows(&mut self, rows: &[u8]) -> Result<(), Error> {
        let row_bytes = self.width * 3;
        if !rows.len().is_multiple_of(row_bytes) {
            return Err(Error::Rows);
        }
        let count = rows.len() / row_bytes;
        if self.taken.saturating_add(count) > self.height {
            return Err(Error::Rows);
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
    pub fn finish(mut self) -> Result<Vec<u8>, Error> {
        if self.taken != self.height {
            return Err(Error::Rows);
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
    fn code_groups(&mut self, groups: usize) -> Result<(), Error> {
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
    fn code_block(&mut self, component: usize, block: &[i16; 64]) -> Result<(), Error> {
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
    fn lookup(&self, tables: &[Codes; 2], which: usize, symbol: u8) -> Result<(u32, u32), Error> {
        let table = tables.get(which).ok_or(Error::Table)?;
        let len = table.len.get(usize::from(symbol)).copied().unwrap_or(0);
        if len == 0 {
            return Err(Error::Table);
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

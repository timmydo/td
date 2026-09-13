//! The Nikon NEF layout over the TIFF container and the Nikon Huffman codec:
//! the raw sub-image, the embedded previews, the exposure facts and the
//! maker note's white balance, black level, sensor crop and linearization
//! table, then the decoder dcraw's `nikon_load_raw` describes. Every count,
//! offset and axis is checked against a ceiling before it sizes an
//! allocation or indexes a buffer. Nothing here reads a file, the
//! environment or a clock.

use std::fmt;

use crate::tiff::{self, kind, tag, Endian, Entry, Ifd, Reader, Visited};

/// The longest raw axis accepted.
pub const MAX_AXIS: usize = 16384;
/// The most raw samples accepted (a 256 MiB `u16` buffer).
pub const MAX_RAW_SAMPLES: usize = 128 << 20;
/// The widest sample range a decoder parameter set may claim: a 15-bit
/// value, above every range `HuffmanParams::parse` can produce.
pub const MAX_RANGE: u32 = 1 << 15;
/// The most sub-IFDs read from IFD0's `SubIFDs`.
pub const MAX_SUB_IFDS: usize = 16;
/// TIFF `Compression` values the decoder handles.
pub const COMPRESSION_NONE: u16 = 1;
pub const COMPRESSION_NIKON: u16 = 34713;
/// `PhotometricInterpretation` of a colour filter array.
pub const PHOTOMETRIC_CFA: u32 = 32803;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    Container(tiff::Error),
    /// No sub-image is a one-strip Bayer CFA.
    NoRawImage,
    Axis {
        width: usize,
        height: usize,
    },
    Bits(u32),
    /// The TIFF `Compression` value, as declared, when not one of the two.
    Compression(u32),
    /// The CFA pattern is not a 2x2 arrangement of R, G, G, B.
    Pattern,
    /// The raw image is not stored as exactly one strip.
    Strip,
    /// The strip holds fewer samples than the frame.
    Samples,
    /// Nikon compression without a maker-note linearization table.
    NoLinearization,
    /// The linearization table is malformed; names what.
    Linearization(&'static str),
    /// No such Huffman tree.
    Tree(usize),
    /// The bitstream ended after this many complete rows.
    Truncated {
        rows: usize,
    },
}

impl From<tiff::Error> for Error {
    fn from(e: tiff::Error) -> Self {
        Self::Container(e)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Container(e) => write!(f, "container: {e}"),
            Self::NoRawImage => f.write_str("no raw CFA sub-image"),
            Self::Axis { width, height } => write!(f, "raw axes {width}x{height} refused"),
            Self::Bits(bits) => write!(f, "{bits} bits per sample unsupported"),
            Self::Compression(c) => write!(f, "compression {c} unsupported"),
            Self::Pattern => f.write_str("CFA pattern is not 2x2 Bayer"),
            Self::Strip => f.write_str("raw image is not one strip"),
            Self::Samples => f.write_str("raw strip is shorter than the frame"),
            Self::NoLinearization => f.write_str("Nikon compression without a linearization table"),
            Self::Linearization(what) => write!(f, "linearization table: {what}"),
            Self::Tree(t) => write!(f, "no Huffman tree {t}"),
            Self::Truncated { rows } => write!(f, "bitstream ended after {rows} rows"),
        }
    }
}

impl std::error::Error for Error {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    Red,
    Green,
    Blue,
}

/// A 2x2 Bayer arrangement, row-major from the sensor's top-left sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cfa {
    pattern: [Channel; 4],
}

impl Cfa {
    pub const RGGB: Self = Self {
        pattern: [Channel::Red, Channel::Green, Channel::Green, Channel::Blue],
    };

    /// From TIFF `CFAPattern` codes (0 red, 1 green, 2 blue): one red, one
    /// blue and two greens on a diagonal.
    pub fn from_codes(codes: &[u8]) -> Result<Self, Error> {
        let code = |i: usize| -> Result<Channel, Error> {
            match codes.get(i) {
                Some(0) => Ok(Channel::Red),
                Some(1) => Ok(Channel::Green),
                Some(2) => Ok(Channel::Blue),
                _ => Err(Error::Pattern),
            }
        };
        if codes.len() != 4 {
            return Err(Error::Pattern);
        }
        let pattern = [code(0)?, code(1)?, code(2)?, code(3)?];
        let greens_diagonal = (pattern[0] == Channel::Green && pattern[3] == Channel::Green)
            || (pattern[1] == Channel::Green && pattern[2] == Channel::Green);
        let reds = pattern.iter().filter(|c| **c == Channel::Red).count();
        let blues = pattern.iter().filter(|c| **c == Channel::Blue).count();
        if !greens_diagonal || reds != 1 || blues != 1 {
            return Err(Error::Pattern);
        }
        Ok(Self { pattern })
    }

    /// The channel of the sample at sensor column `x`, row `y`.
    #[inline]
    pub fn at(self, x: usize, y: usize) -> Channel {
        match (y & 1, x & 1) {
            (0, 0) => self.pattern[0],
            (0, _) => self.pattern[1],
            (_, 0) => self.pattern[2],
            (_, _) => self.pattern[3],
        }
    }

    pub fn pattern(self) -> [Channel; 4] {
        self.pattern
    }

    /// The pattern's letters, `RGGB` and the like.
    pub fn name(self) -> String {
        self.pattern
            .iter()
            .map(|c| match c {
                Channel::Red => 'R',
                Channel::Green => 'G',
                Channel::Blue => 'B',
            })
            .collect()
    }
}

/// A sensor rectangle in sample units.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Crop {
    pub left: usize,
    pub top: usize,
    pub width: usize,
    pub height: usize,
}

impl Crop {
    /// Whether the rectangle lies inside a `width` by `height` frame and is
    /// at least one quad in each axis.
    pub fn fits(self, width: usize, height: usize) -> bool {
        self.width >= 2
            && self.height >= 2
            && self
                .left
                .checked_add(self.width)
                .is_some_and(|right| right <= width)
            && self
                .top
                .checked_add(self.height)
                .is_some_and(|bottom| bottom <= height)
    }
}

/// The raw sub-image as the container describes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Raw {
    pub width: usize,
    pub height: usize,
    pub bits: u8,
    pub compression: u16,
    /// Absolute offset and length of the one strip.
    pub strip: (usize, usize),
    pub cfa: Cfa,
}

/// One embedded JPEG: absolute offset and length, beginning with SOI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Preview {
    pub offset: usize,
    pub len: usize,
}

/// The Exif facts shown beside a photo; each optional.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Exposure {
    pub time: Option<(u32, u32)>,
    pub aperture: Option<(u32, u32)>,
    pub iso: Option<u32>,
    pub focal_length: Option<(u32, u32)>,
    pub taken: Option<String>,
    pub lens: Option<String>,
}

/// What the Nikon maker note supplies; each optional.
#[derive(Clone, Debug, PartialEq)]
pub struct MakerNote {
    /// The note's own byte order, which its linearization table is read
    /// in; it need not be the file's.
    pub endian: Endian,
    /// Red and blue multipliers over green, the as-shot balance.
    pub wb: Option<(f32, f32)>,
    pub black: Option<u16>,
    pub crop: Option<Crop>,
    /// Absolute offset and length of the linearization table.
    pub linearization: Option<(usize, usize)>,
}

impl Default for MakerNote {
    fn default() -> Self {
        Self {
            endian: Endian::Little,
            wb: None,
            black: None,
            crop: None,
            linearization: None,
        }
    }
}

/// A parsed file: everything but the sample data.
#[derive(Clone, Debug, PartialEq)]
pub struct Nef {
    pub endian: Endian,
    pub make: String,
    pub model: String,
    /// TIFF orientation; 1, 3, 6 and 8 are honoured downstream.
    pub orientation: u16,
    pub raw: Raw,
    pub previews: Vec<Preview>,
    pub exposure: Exposure,
    pub maker: MakerNote,
}

impl Nef {
    /// The sensor crop the maker note names, or the full frame, held to
    /// the frame either way.
    pub fn crop(&self) -> Crop {
        let full = Crop {
            left: 0,
            top: 0,
            width: self.raw.width,
            height: self.raw.height,
        };
        match self.maker.crop {
            Some(crop) if crop.fits(self.raw.width, self.raw.height) => crop,
            _ => full,
        }
    }
}

/// Parses the container and the Nikon layout over it.
pub fn parse(data: &[u8]) -> Result<Nef, Error> {
    let (reader, first) = Reader::new(data, 0)?;
    let mut visited = Visited::new();
    let chain = reader.chain(first, &mut visited)?;
    let ifd0 = chain.first().ok_or(Error::NoRawImage)?;
    let make = ascii(&reader, ifd0, tag::MAKE);
    let model = ascii(&reader, ifd0, tag::MODEL);
    let orientation = ifd0
        .find(tag::ORIENTATION)
        .and_then(|e| reader.integer(e, 0).ok())
        .and_then(|v| u16::try_from(v).ok())
        .unwrap_or(1);
    let mut previews = Vec::new();
    // IFD0 and whatever follows it on its chain (a thumbnail IFD1 on some
    // bodies) may each name a preview.
    for ifd in &chain {
        push_preview(&reader, ifd, &mut previews);
    }
    let mut raw: Option<Raw> = None;
    if let Some(entry) = ifd0.find(tag::SUB_IFDS) {
        let count = (entry.count as usize).min(MAX_SUB_IFDS);
        for index in 0..count {
            let offset = reader.integer(entry, index)?;
            let sub = reader.ifd(offset)?;
            visited.enter(sub.offset)?;
            push_preview(&reader, &sub, &mut previews);
            if let Some(candidate) = read_raw(&reader, &sub)? {
                let larger = raw
                    .as_ref()
                    .is_none_or(|r| candidate.width * candidate.height > r.width * r.height);
                if larger {
                    raw = Some(candidate);
                }
            }
        }
    }
    let mut exposure = Exposure::default();
    let mut maker = MakerNote::default();
    if let Some(entry) = ifd0.find(tag::EXIF_IFD) {
        let exif = reader.ifd(reader.integer(entry, 0)?)?;
        visited.enter(exif.offset)?;
        exposure = read_exposure(&reader, &exif);
        if let Some(note) = exif.find(tag::MAKER_NOTE) {
            maker = read_maker_note(data, note, &mut visited)?;
        }
    }
    let raw = raw.ok_or(Error::NoRawImage)?;
    Ok(Nef {
        endian: reader.endian(),
        make,
        model,
        orientation,
        raw,
        previews,
        exposure,
        maker,
    })
}

fn ascii(reader: &Reader<'_>, ifd: &Ifd, tag: u16) -> String {
    ifd.find(tag)
        .and_then(|e| reader.ascii(e).ok())
        .unwrap_or_default()
        .to_string()
}

fn push_preview(reader: &Reader<'_>, ifd: &Ifd, out: &mut Vec<Preview>) {
    let Some(offset) = ifd
        .find(tag::JPEG_OFFSET)
        .and_then(|e| reader.integer(e, 0).ok())
    else {
        return;
    };
    let Some(len) = ifd
        .find(tag::JPEG_LENGTH)
        .and_then(|e| reader.integer(e, 0).ok())
    else {
        return;
    };
    let (offset, len) = (offset as usize, len as usize);
    let Some(end) = offset.checked_add(len) else {
        return;
    };
    if let Some([0xff, 0xd8, ..]) = reader.data().get(offset..end) {
        out.push(Preview { offset, len });
    }
}

/// The sub-IFD as a raw image when it is a CFA, else `None`.
fn read_raw(reader: &Reader<'_>, ifd: &Ifd) -> Result<Option<Raw>, Error> {
    let photometric = ifd
        .find(tag::PHOTOMETRIC)
        .map(|e| reader.integer(e, 0))
        .transpose()?;
    if photometric != Some(PHOTOMETRIC_CFA) {
        return Ok(None);
    }
    let integer = |tag: u16| -> Result<u32, Error> {
        let entry = ifd.find(tag).ok_or(Error::NoRawImage)?;
        Ok(reader.integer(entry, 0)?)
    };
    let width = integer(tag::IMAGE_WIDTH)? as usize;
    let height = integer(tag::IMAGE_LENGTH)? as usize;
    check_axes(width, height)?;
    let bits = integer(tag::BITS_PER_SAMPLE)?;
    if bits != 12 && bits != 14 {
        return Err(Error::Bits(bits));
    }
    if ifd
        .find(tag::SAMPLES_PER_PIXEL)
        .map(|e| reader.integer(e, 0))
        .transpose()?
        .is_some_and(|s| s != 1)
    {
        return Err(Error::Pattern);
    }
    let declared = integer(tag::COMPRESSION)?;
    let compression = u16::try_from(declared).map_err(|_| Error::Compression(declared))?;
    let offsets = ifd.find(tag::STRIP_OFFSETS).ok_or(Error::Strip)?;
    let counts = ifd.find(tag::STRIP_BYTE_COUNTS).ok_or(Error::Strip)?;
    if offsets.count != 1 || counts.count != 1 {
        return Err(Error::Strip);
    }
    let strip = (
        reader.integer(offsets, 0)? as usize,
        reader.integer(counts, 0)? as usize,
    );
    if let Some(dim) = ifd.find(tag::CFA_REPEAT_DIM) {
        if reader.integer(dim, 0)? != 2 || reader.integer(dim, 1)? != 2 {
            return Err(Error::Pattern);
        }
    }
    let pattern = ifd.find(tag::CFA_PATTERN).ok_or(Error::Pattern)?;
    let cfa = Cfa::from_codes(reader.bytes(pattern)?)?;
    Ok(Some(Raw {
        width,
        height,
        bits: bits as u8,
        compression,
        strip,
        cfa,
    }))
}

fn check_axes(width: usize, height: usize) -> Result<(), Error> {
    let refused = Error::Axis { width, height };
    if width == 0 || height == 0 || width > MAX_AXIS || height > MAX_AXIS {
        return Err(refused);
    }
    match width.checked_mul(height) {
        Some(n) if n <= MAX_RAW_SAMPLES => Ok(()),
        _ => Err(refused),
    }
}

fn read_exposure(reader: &Reader<'_>, exif: &Ifd) -> Exposure {
    let rational = |tag: u16| exif.find(tag).and_then(|e| reader.rational(e, 0).ok());
    Exposure {
        time: rational(tag::EXPOSURE_TIME),
        aperture: rational(tag::F_NUMBER),
        iso: exif.find(tag::ISO).and_then(|e| reader.integer(e, 0).ok()),
        focal_length: rational(tag::FOCAL_LENGTH),
        taken: exif
            .find(tag::DATE_TIME_ORIGINAL)
            .and_then(|e| reader.ascii(e).ok())
            .map(str::to_string),
        lens: exif
            .find(tag::LENS_MODEL)
            .and_then(|e| reader.ascii(e).ok())
            .map(str::to_string),
    }
}

/// The `Nikon\0` type-2 maker note: a ten-byte prefix, then a TIFF header
/// whose offsets are relative to that header.
fn read_maker_note(data: &[u8], note: &Entry, visited: &mut Visited) -> Result<MakerNote, Error> {
    let mut out = MakerNote::default();
    // The note's reader sees the file only up to the field's declared end,
    // so the note's own offsets cannot reach past what Exif says it holds.
    let end = note
        .offset
        .checked_add(note.len)
        .ok_or(tiff::Error::Overflow)?;
    let field = data.get(..end).ok_or(tiff::Error::OutOfFile {
        offset: note.offset,
        len: note.len,
    })?;
    let prefix_end = note.offset.checked_add(10).ok_or(tiff::Error::Overflow)?;
    let Some(prefix) = field.get(note.offset..prefix_end) else {
        return Ok(out);
    };
    if !prefix.starts_with(b"Nikon\0") {
        return Ok(out);
    }
    let Ok((reader, first)) = Reader::new(field, prefix_end) else {
        return Ok(out);
    };
    out.endian = reader.endian();
    let ifd = reader.ifd(first)?;
    visited.enter(ifd.offset)?;
    if let Some(entry) = ifd.find(tag::NIKON_WB_RB_LEVELS) {
        if entry.kind == kind::RATIONAL && entry.count >= 2 {
            let (rn, rd) = reader.rational(entry, 0)?;
            let (bn, bd) = reader.rational(entry, 1)?;
            if rd != 0 && bd != 0 && rn != 0 && bn != 0 {
                out.wb = Some((rn as f32 / rd as f32, bn as f32 / bd as f32));
            }
        }
    }
    if let Some(entry) = ifd.find(tag::NIKON_BLACK_LEVEL) {
        if entry.kind == kind::SHORT && entry.count >= 1 {
            out.black = u16::try_from(reader.integer(entry, 0)?).ok();
        }
    }
    if let Some(entry) = ifd.find(tag::NIKON_CROP_AREA) {
        if entry.kind == kind::SHORT && entry.count >= 4 {
            out.crop = Some(Crop {
                left: reader.integer(entry, 0)? as usize,
                top: reader.integer(entry, 1)? as usize,
                width: reader.integer(entry, 2)? as usize,
                height: reader.integer(entry, 3)? as usize,
            });
        }
    }
    if let Some(entry) = ifd.find(tag::NIKON_LINEARIZATION) {
        if entry.kind == kind::UNDEFINED {
            reader.bytes(entry)?;
            out.linearization = Some((entry.offset, entry.len));
        }
    }
    Ok(out)
}

/// A decoded frame: `width * height` samples in sensor order, and how many
/// predicted samples fell outside the range and were clamped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decoded {
    pub width: usize,
    pub height: usize,
    pub samples: Vec<u16>,
    pub corrupt: usize,
}

/// dcraw's six Nikon trees: sixteen code-length counts, then the symbols in
/// canonical order, zero-padded. A symbol's low nibble is the difference
/// length and its high nibble the shift.
pub const TREES: [[u8; 32]; 6] = [
    // 12-bit lossy
    [
        0, 1, 5, 1, 1, 1, 1, 1, 1, 2, 0, 0, 0, 0, 0, 0, 5, 4, 3, 6, 2, 7, 1, 0, 8, 9, 11, 10, 12,
        0, 0, 0,
    ],
    // 12-bit lossy after the split
    [
        0, 1, 5, 1, 1, 1, 1, 1, 1, 2, 0, 0, 0, 0, 0, 0, 0x39, 0x5a, 0x38, 0x27, 0x16, 5, 4, 3, 2,
        1, 0, 11, 12, 12, 0, 0,
    ],
    // 12-bit lossless
    [
        0, 1, 4, 2, 3, 1, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 5, 4, 6, 3, 7, 2, 8, 1, 9, 0, 10, 11, 12,
        0, 0, 0,
    ],
    // 14-bit lossy
    [
        0, 1, 4, 3, 1, 1, 1, 1, 1, 2, 0, 0, 0, 0, 0, 0, 5, 6, 4, 7, 8, 3, 9, 2, 1, 0, 10, 11, 12,
        13, 14, 0,
    ],
    // 14-bit lossy after the split
    [
        0, 1, 5, 1, 1, 1, 1, 1, 1, 1, 2, 0, 0, 0, 0, 0, 8, 0x5c, 0x4b, 0x3a, 0x29, 7, 6, 5, 4, 3,
        2, 1, 0, 13, 14, 0,
    ],
    // 14-bit lossless
    [
        0, 1, 4, 2, 2, 3, 1, 2, 0, 0, 0, 0, 0, 0, 0, 0, 7, 6, 8, 5, 9, 4, 10, 3, 11, 12, 2, 0, 1,
        13, 14, 0,
    ],
];

/// What the linearization table says about a compressed strip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HuffmanParams {
    /// Index into `TREES` for the first rows.
    pub tree: usize,
    /// Initial vertical predictors, `[row parity][column parity]`.
    pub vpred: [[u16; 2]; 2],
    /// The output curve, or `None` for identity.
    pub curve: Option<Vec<u16>>,
    /// One past the largest valid predicted sample.
    pub max: u32,
    /// The row at which the tree changes to `tree + 1`, or 0 for never.
    pub split: usize,
}

impl HuffmanParams {
    /// Reads the maker note's table for a `bits`-per-sample strip stored in
    /// the file's byte order.
    pub fn parse(table: &[u8], bits: u8, endian: Endian) -> Result<Self, Error> {
        let short = |at: usize| -> Result<u16, Error> {
            let bytes = table
                .get(at..at + 2)
                .ok_or(Error::Linearization("table too short"))?;
            let array: [u8; 2] = bytes
                .try_into()
                .map_err(|_| Error::Linearization("table too short"))?;
            Ok(match endian {
                Endian::Little => u16::from_le_bytes(array),
                Endian::Big => u16::from_be_bytes(array),
            })
        };
        let ver0 = *table.first().ok_or(Error::Linearization("empty"))?;
        let ver1 = *table.get(1).ok_or(Error::Linearization("empty"))?;
        let mut at = 2;
        if ver0 == 0x49 || ver1 == 0x58 {
            at += 2110;
        }
        let mut tree = 0;
        if ver0 == 0x46 {
            tree = 2;
        }
        if bits == 14 {
            tree += 3;
        } else if bits != 12 {
            return Err(Error::Bits(u32::from(bits)));
        }
        let vpred = [
            [short(at)?, short(at + 2)?],
            [short(at + 4)?, short(at + 6)?],
        ];
        at += 8;
        let mut max = (1usize << bits) & 0x7fff;
        let csize = usize::from(short(at)?);
        at += 2;
        let step = if csize > 1 { max / (csize - 1) } else { 0 };
        let mut curve: Option<Vec<u16>> = None;
        let mut split = 0;
        if ver0 == 0x44 && ver1 == 0x20 && step > 0 {
            // A sampled curve, linearly interpolated between its points over
            // dcraw's identity-initialised table: a segment past the last
            // knot interpolates toward identity, and entry `max` keeps the
            // knot (or identity) dcraw leaves there, which the widened range
            // after a split can index.
            let mut points: Vec<u16> = (0..=max + step)
                .map(|i| u16::try_from(i).unwrap_or(u16::MAX))
                .collect();
            for i in 0..csize {
                let value = short(at + 2 * i)?;
                if let Some(slot) = points.get_mut(i * step) {
                    *slot = value;
                }
            }
            let mut full = vec![0u16; max + 1];
            for (i, slot) in full.iter_mut().enumerate().take(max) {
                let base = i - i % step;
                let frac = i % step;
                let low = u32::from(points.get(base).copied().unwrap_or(0));
                let high = u32::from(points.get(base + step).copied().unwrap_or(0));
                let value = (low * (step - frac) as u32 + high * frac as u32) / step as u32;
                *slot = u16::try_from(value).unwrap_or(u16::MAX);
            }
            if let (Some(slot), Some(knot)) = (full.get_mut(max), points.get(max)) {
                *slot = *knot;
            }
            curve = Some(full);
            split = usize::from(short(562)?);
        } else if ver0 != 0x46 && csize <= 0x4001 {
            let mut full = Vec::with_capacity(csize);
            for i in 0..csize {
                full.push(short(at + 2 * i)?);
            }
            max = csize;
            curve = Some(full);
        }
        if let Some(c) = &curve {
            // Trailing repeats of the top value shrink the valid range.
            while max >= 2 && c.get(max - 2).is_some_and(|a| Some(a) == c.get(max - 1)) {
                max -= 1;
            }
        }
        if max < 2 {
            return Err(Error::Linearization("curve range"));
        }
        Ok(Self {
            tree,
            vpred,
            curve,
            max: max as u32,
            split,
        })
    }
}

/// MSB-first bit reader with zero padding past the end; `overrun` says
/// whether any consumed bit lay beyond the data.
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
    acc: u64,
    count: u32,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            acc: 0,
            count: 0,
        }
    }

    #[inline]
    fn refill(&mut self) {
        while self.count <= 56 {
            let byte = self.data.get(self.pos).copied().unwrap_or(0);
            self.pos += 1;
            self.acc = (self.acc << 8) | u64::from(byte);
            self.count += 8;
        }
    }

    /// The next `n` bits (1 through 16) without consuming them.
    #[inline]
    fn peek(&mut self, n: u32) -> u32 {
        if self.count < n {
            self.refill();
        }
        ((self.acc >> (self.count - n)) & ((1u64 << n) - 1)) as u32
    }

    #[inline]
    fn consume(&mut self, n: u32) {
        self.count = self.count.saturating_sub(n);
    }

    #[inline]
    fn get(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        let value = self.peek(n);
        self.consume(n);
        value
    }

    /// Whether any consumed bit lay beyond the data. In `u64`: a 512 MiB
    /// slice's bit count overflows a 32-bit `usize`.
    fn overrun(&self) -> bool {
        (self.pos as u64) * 8 - u64::from(self.count) > (self.data.len() as u64) * 8
    }
}

/// One tree as a `1 << longest` lookup: code length and symbol per prefix.
struct Lut {
    bits: u32,
    table: Vec<(u8, u8)>,
}

impl Lut {
    fn build(tree: usize) -> Result<Self, Error> {
        let spec = TREES.get(tree).ok_or(Error::Tree(tree))?;
        let counts = spec.get(..16).ok_or(Error::Tree(tree))?;
        let symbols = spec.get(16..).ok_or(Error::Tree(tree))?;
        let longest = counts
            .iter()
            .rposition(|c| *c != 0)
            .map(|i| i + 1)
            .ok_or(Error::Tree(tree))?;
        let mut table = vec![(0u8, 0u8); 1 << longest];
        let mut next = 0usize;
        let mut symbol = symbols.iter();
        for (len, count) in counts.iter().enumerate().map(|(i, c)| (i + 1, *c)) {
            for _ in 0..count {
                let sym = *symbol.next().ok_or(Error::Tree(tree))?;
                let span = 1usize << (longest - len);
                for entry in table.iter_mut().skip(next).take(span) {
                    *entry = (len as u8, sym);
                }
                next += span;
            }
        }
        Ok(Self {
            bits: longest as u32,
            table,
        })
    }

    #[inline]
    fn symbol(&self, bits: &mut Bits<'_>) -> u8 {
        let prefix = bits.peek(self.bits) as usize;
        let (len, sym) = self.table.get(prefix).copied().unwrap_or((0, 0));
        // An unassigned prefix cannot occur in a complete tree; consuming the
        // whole prefix keeps the stream moving if one ever did.
        bits.consume(if len == 0 { self.bits } else { u32::from(len) });
        sym
    }
}

/// Decodes a compressed strip of `width * height` samples.
pub fn decode_huffman(
    params: &HuffmanParams,
    stream: &[u8],
    width: usize,
    height: usize,
) -> Result<Decoded, Error> {
    check_axes(width, height)?;
    // `parse` never yields a range outside this; hand-built parameters
    // must not either, or the split's widening below would overflow.
    if params.max < 2 || params.max > MAX_RANGE {
        return Err(Error::Linearization("curve range"));
    }
    let mut samples = vec![0u16; width * height];
    let mut lut = Lut::build(params.tree)?;
    let mut bits = Bits::new(stream);
    let mut vpred = params.vpred;
    let mut hpred = [0u16; 2];
    let mut min: u16 = 0;
    let mut max = params.max;
    let mut corrupt = 0usize;
    let identity = params.curve.is_none();
    let curve: &[u16] = params.curve.as_deref().unwrap_or(&[]);
    for (row, out_row) in samples.chunks_exact_mut(width).enumerate() {
        if params.split != 0 && row == params.split {
            lut = Lut::build(params.tree + 1)?;
            min = 16;
            max = max.saturating_add(32);
        }
        let vp = if row & 1 == 0 {
            &mut vpred[0]
        } else {
            &mut vpred[1]
        };
        for (col, out) in out_row.iter_mut().enumerate() {
            let sym = lut.symbol(&mut bits);
            let len = u32::from(sym & 15);
            let shl = u32::from(sym >> 4);
            let diff: i32 = if len == 0 {
                0
            } else {
                let raw = bits.get(len.saturating_sub(shl)) as i32;
                let mut d = (((raw << 1) + 1) << shl) >> 1;
                if d & (1 << (len - 1)) == 0 {
                    d -= (1 << len) - i32::from(shl == 0);
                }
                d
            };
            let even = col & 1 == 0;
            let pred = if col < 2 {
                let v = if even { &mut vp[0] } else { &mut vp[1] };
                *v = v.wrapping_add(diff as u16);
                let h = if even { &mut hpred[0] } else { &mut hpred[1] };
                *h = *v;
                *v
            } else {
                let h = if even { &mut hpred[0] } else { &mut hpred[1] };
                *h = h.wrapping_add(diff as u16);
                *h
            };
            // dcraw's range test: `(ushort)(hpred + min) >= max`, with `max`
            // widened by `2 * min` at the split, so after it a sample may sit
            // `min` below zero or above the old range; `+ min` is not a typo
            // for `- min`.
            if u32::from(pred.wrapping_add(min)) >= max {
                corrupt += 1;
            }
            let index = if pred >= 0x8000 { 0 } else { pred.min(0x3fff) };
            *out = if identity {
                index
            } else {
                curve.get(usize::from(index)).copied().unwrap_or(index)
            };
        }
        if bits.overrun() {
            return Err(Error::Truncated { rows: row });
        }
    }
    Ok(Decoded {
        width,
        height,
        samples,
        corrupt,
    })
}

/// Decodes an uncompressed strip of 16-bit samples in the file's order.
pub fn decode_uncompressed(
    strip: &[u8],
    width: usize,
    height: usize,
    endian: Endian,
) -> Result<Decoded, Error> {
    check_axes(width, height)?;
    let count = width * height;
    if strip.len() / 2 < count {
        return Err(Error::Samples);
    }
    let samples = strip
        .as_chunks::<2>()
        .0
        .iter()
        .take(count)
        .map(|pair| match endian {
            Endian::Little => u16::from_le_bytes(*pair),
            Endian::Big => u16::from_be_bytes(*pair),
        })
        .collect();
    Ok(Decoded {
        width,
        height,
        samples,
        corrupt: 0,
    })
}

/// Decodes the raw sub-image of a parsed file from the same bytes.
pub fn decode(nef: &Nef, data: &[u8]) -> Result<Decoded, Error> {
    let (offset, len) = nef.raw.strip;
    let end = offset.checked_add(len).ok_or(tiff::Error::Overflow)?;
    let strip = data
        .get(offset..end)
        .ok_or(tiff::Error::OutOfFile { offset, len })?;
    match nef.raw.compression {
        COMPRESSION_NONE => decode_uncompressed(strip, nef.raw.width, nef.raw.height, nef.endian),
        COMPRESSION_NIKON => {
            let (offset, len) = nef.maker.linearization.ok_or(Error::NoLinearization)?;
            let end = offset.checked_add(len).ok_or(tiff::Error::Overflow)?;
            let table = data
                .get(offset..end)
                .ok_or(tiff::Error::OutOfFile { offset, len })?;
            let params = HuffmanParams::parse(table, nef.raw.bits, nef.maker.endian)?;
            decode_huffman(&params, strip, nef.raw.width, nef.raw.height)
        }
        other => Err(Error::Compression(u32::from(other))),
    }
}

#![forbid(unsafe_code)]
//! PNG decoder: every colour type and bit depth the specification allows,
//! the five filters, Adam7 interlacing and `tRNS` transparency, into an
//! owned eight-bit RGBA buffer with straight alpha (`Rgba8`). Chunk CRCs,
//! the zlib header and its Adler-32 are checked, and the inflated size is
//! exactly the one the header implies, so damaged image data is an error
//! naming what was refused rather than a picture of noise. What cannot
//! damage the pixels is read as libpng reads it: an ancillary chunk with a
//! bad CRC, an invalid `tRNS` and a palette in an image without one are
//! skipped, and a palette longer than its depth is cut to it. Colour
//! management chunks (`gAMA`, `cHRM`, `sRGB`, `iCCP`) and every other
//! ancillary chunk are skipped; an animated PNG decodes to its default
//! image. Nothing here reads a file, the environment or a clock
//! (DESIGN.md).
//!
//! The inflater and the CRC are the engine's, mounted as source rather
//! than copied.

#[path = "../../engine/src/crc32.rs"]
#[allow(dead_code)]
// The engine file's own tests, held to the engine's lints rather than these.
#[cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic
    )
)]
mod crc32;
#[path = "../../engine/src/gzip.rs"]
#[allow(dead_code)]
#[cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic
    )
)]
mod gzip;

use std::fmt;

/// The longest axis a decoded image may have: long enough for a
/// full-page screenshot, the pixel budget bounding the rest.
pub const MAX_AXIS: usize = 1 << 16;
/// The most pixels one decoded image holds: 64 Mi, 256 MiB as RGBA.
pub const MAX_IMAGE_PIXELS: usize = 64 << 20;
/// The most bytes the image data may inflate to, filter bytes included:
/// 256 MiB, so a 16-bit RGBA frame past 32 Mi pixels is refused. The
/// compressed data may be at most twice its inflated size plus 64 KiB.
pub const MAX_INFLATED_BYTES: usize = 256 << 20;
/// The most chunks one stream may carry before its end.
pub const MAX_CHUNKS: usize = 1 << 16;

const SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

/// Adam7's passes: column and row of the first pixel, then the steps.
const ADAM7: &[(usize, usize, usize, usize)] = &[
    (0, 0, 8, 8),
    (4, 0, 8, 8),
    (0, 4, 4, 8),
    (2, 0, 4, 4),
    (0, 2, 2, 4),
    (1, 0, 2, 2),
    (0, 1, 1, 2),
];

/// Eight-bit RGBA, rows top to bottom, no padding, alpha straight (not
/// premultiplied).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Rgba8 {
    pub width: usize,
    pub height: usize,
    pub data: Vec<u8>,
}

impl Rgba8 {
    /// The pixel at `x`, `y`, or `None` outside the image or past a buffer
    /// whose fields disagree.
    pub fn pixel(&self, x: usize, y: usize) -> Option<[u8; 4]> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let at = y.checked_mul(self.width)?.checked_add(x)?.checked_mul(4)?;
        let bytes = self.data.get(at..at.checked_add(4)?)?;
        Some([
            *bytes.first()?,
            *bytes.get(1)?,
            *bytes.get(2)?,
            *bytes.get(3)?,
        ])
    }
}

/// The colour types the specification defines.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Colour {
    Grey,
    Rgb,
    Palette,
    GreyAlpha,
    Rgba,
}

impl Colour {
    fn channels(self) -> usize {
        match self {
            Self::Grey | Self::Palette => 1,
            Self::GreyAlpha => 2,
            Self::Rgb => 3,
            Self::Rgba => 4,
        }
    }
}

/// What `IHDR` says, read without decoding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Header {
    pub width: usize,
    pub height: usize,
    pub depth: u8,
    pub colour: Colour,
    pub interlaced: bool,
}

/// Each refusal, by name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Error {
    /// Not the PNG signature.
    NotPng,
    /// The data ends inside a chunk, or before `IEND`.
    Truncated,
    /// A critical chunk whose CRC does not match its type and data.
    Crc([u8; 4]),
    /// A chunk type that is not four ASCII letters, or a chunk longer than
    /// 2^31 - 1 bytes.
    Chunk,
    /// More chunks than `MAX_CHUNKS`.
    Chunks,
    /// A critical chunk this decoder does not know.
    Critical([u8; 4]),
    /// `IHDR` not first or twice, image data not in consecutive `IDAT`s,
    /// or an indexed image's `PLTE` twice or after its data.
    Order([u8; 4]),
    /// `IHDR`, `IEND` or an indexed image's `PLTE` of a length the
    /// specification does not allow.
    Malformed([u8; 4]),
    /// A bit depth and colour type the specification does not pair, or a
    /// compression, filter or interlace method it does not define.
    Format,
    /// An axis of zero, one past `MAX_AXIS`, a frame past
    /// `MAX_IMAGE_PIXELS`, or one whose rows inflate past
    /// `MAX_INFLATED_BYTES`.
    Axis { width: usize, height: usize },
    /// Compressed image data past what its inflated size can need.
    Budget,
    /// A palette image without `PLTE`, or an index past the palette.
    Palette,
    /// No `IDAT`.
    NoData,
    /// A zlib header this decoder does not accept (not DEFLATE, a window
    /// past 32 KiB, a bad check or a preset dictionary).
    Zlib,
    /// The DEFLATE stream is malformed; the inflater's words.
    Inflate(String),
    /// The image data inflates to other than the size the header implies,
    /// or is followed by anything but its Adler-32.
    Length,
    /// The Adler-32 does not match the inflated data.
    Adler,
    /// A row filter type past 4.
    Filter(u8),
}

fn name(kind: &[u8; 4]) -> String {
    kind.iter().map(|b| char::from(*b)).collect()
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotPng => f.write_str("not a PNG stream"),
            Self::Truncated => f.write_str("PNG data ends early"),
            Self::Crc(kind) => write!(f, "PNG chunk {} fails its CRC", name(kind)),
            Self::Chunk => f.write_str("PNG chunk type or length malformed"),
            Self::Chunks => write!(f, "PNG has more than {MAX_CHUNKS} chunks"),
            Self::Critical(kind) => write!(f, "PNG critical chunk {} not supported", name(kind)),
            Self::Order(kind) => write!(f, "PNG chunk {} out of place", name(kind)),
            Self::Malformed(kind) => write!(f, "PNG chunk {} malformed", name(kind)),
            Self::Format => f.write_str("PNG bit depth, colour type or method not defined"),
            Self::Axis { width, height } => write!(f, "PNG frame {width}x{height} refused"),
            Self::Budget => f.write_str("PNG image data past its budget"),
            Self::Palette => f.write_str("PNG palette missing or index past it"),
            Self::NoData => f.write_str("PNG has no image data"),
            Self::Zlib => f.write_str("PNG zlib header refused"),
            Self::Inflate(why) => write!(f, "PNG image data: {why}"),
            Self::Length => f.write_str("PNG image data is not the size its header implies"),
            Self::Adler => f.write_str("PNG image data fails its Adler-32"),
            Self::Filter(kind) => write!(f, "PNG row filter {kind} not defined"),
        }
    }
}

impl std::error::Error for Error {}

/// One chunk: its type, its data and whether its CRC matched.
struct Chunk<'a> {
    kind: [u8; 4],
    data: &'a [u8],
    sound: bool,
}

impl Chunk<'_> {
    /// An upper-case first letter is critical.
    fn critical(&self) -> bool {
        self.kind.first().is_some_and(u8::is_ascii_uppercase)
    }

    /// The chunk, or its refusal when it is critical and fails its CRC.
    fn checked(self) -> Result<Self, Error> {
        if !self.sound && self.critical() {
            return Err(Error::Crc(self.kind));
        }
        Ok(self)
    }
}

/// Walks the chunks after the signature.
struct Chunks<'a> {
    rest: &'a [u8],
    count: usize,
}

impl<'a> Chunks<'a> {
    fn new(data: &'a [u8]) -> Result<Self, Error> {
        let rest = data.strip_prefix(SIGNATURE).ok_or(Error::NotPng)?;
        Ok(Self { rest, count: 0 })
    }

    fn next_chunk(&mut self) -> Result<Chunk<'a>, Error> {
        self.count += 1;
        if self.count > MAX_CHUNKS {
            return Err(Error::Chunks);
        }
        let (head, rest) = self.rest.split_at_checked(8).ok_or(Error::Truncated)?;
        let length = u32::from_be_bytes(four(head, 0)?);
        if length > 0x7fff_ffff {
            return Err(Error::Chunk);
        }
        let kind = four(head, 4)?;
        if !kind.iter().all(u8::is_ascii_alphabetic) {
            return Err(Error::Chunk);
        }
        let length = usize::try_from(length).map_err(|_| Error::Chunk)?;
        let (data, rest) = rest.split_at_checked(length).ok_or(Error::Truncated)?;
        let (crc, rest) = rest.split_at_checked(4).ok_or(Error::Truncated)?;
        let mut sum = crc32::Crc32::new();
        sum.update(&kind);
        sum.update(data);
        let sound = sum.finalize() == u32::from_be_bytes(four(crc, 0)?);
        self.rest = rest;
        Ok(Chunk { kind, data, sound })
    }
}

fn four(bytes: &[u8], at: usize) -> Result<[u8; 4], Error> {
    bytes
        .get(at..at.checked_add(4).ok_or(Error::Truncated)?)
        .and_then(|b| b.try_into().ok())
        .ok_or(Error::Truncated)
}

fn read_header(chunk: &Chunk<'_>) -> Result<Header, Error> {
    if &chunk.kind != b"IHDR" {
        return Err(Error::Order(chunk.kind));
    }
    let data = chunk.data;
    if data.len() != 13 {
        return Err(Error::Malformed(chunk.kind));
    }
    let width = u32::from_be_bytes(four(data, 0)?);
    let height = u32::from_be_bytes(four(data, 4)?);
    let field = |at: usize| data.get(at).copied().ok_or(Error::Truncated);
    let depth = field(8)?;
    let colour = match (field(9)?, depth) {
        (0, 1 | 2 | 4 | 8 | 16) => Colour::Grey,
        (2, 8 | 16) => Colour::Rgb,
        (3, 1 | 2 | 4 | 8) => Colour::Palette,
        (4, 8 | 16) => Colour::GreyAlpha,
        (6, 8 | 16) => Colour::Rgba,
        _ => return Err(Error::Format),
    };
    if field(10)? != 0 || field(11)? != 0 {
        return Err(Error::Format);
    }
    let interlaced = match field(12)? {
        0 => false,
        1 => true,
        _ => return Err(Error::Format),
    };
    let width = usize::try_from(width).map_err(|_| Error::Format)?;
    let height = usize::try_from(height).map_err(|_| Error::Format)?;
    let refused = Error::Axis { width, height };
    if width == 0 || height == 0 || width > MAX_AXIS || height > MAX_AXIS {
        return Err(refused);
    }
    if width
        .checked_mul(height)
        .is_none_or(|n| n > MAX_IMAGE_PIXELS)
    {
        return Err(refused);
    }
    let head = Header {
        width,
        height,
        depth,
        colour,
        interlaced,
    };
    if inflated_size(&head).is_none_or(|n| n > MAX_INFLATED_BYTES) {
        return Err(refused);
    }
    Ok(head)
}

/// The stream's `IHDR`, checked, without decoding: everything `decode`
/// would refuse from the header alone is refused here, so a consumer can
/// budget before it decodes.
pub fn header(data: &[u8]) -> Result<Header, Error> {
    let mut chunks = Chunks::new(data)?;
    read_header(&chunks.next_chunk()?.checked()?)
}

/// Bytes in one filtered row of `width` pixels, filter byte excluded.
fn row_bytes(head: &Header, width: usize) -> Option<usize> {
    let bits = width
        .checked_mul(head.colour.channels())?
        .checked_mul(usize::from(head.depth))?;
    Some(bits.checked_add(7)? / 8)
}

/// The passes as (first column, first row, column step, row step, width,
/// height): one whole-frame pass, or Adam7's seven, empty ones included.
fn passes(head: &Header) -> Vec<(usize, usize, usize, usize, usize, usize)> {
    if !head.interlaced {
        return vec![(0, 0, 1, 1, head.width, head.height)];
    }
    ADAM7
        .iter()
        .map(|&(x0, y0, dx, dy)| {
            let w = head.width.saturating_sub(x0).div_ceil(dx);
            let h = head.height.saturating_sub(y0).div_ceil(dy);
            (x0, y0, dx, dy, w, h)
        })
        .collect()
}

/// What the image data inflates to: each non-empty pass's rows, a filter
/// byte before each.
fn inflated_size(head: &Header) -> Option<usize> {
    let mut total = 0usize;
    for &(_, _, _, _, w, h) in &passes(head) {
        if w == 0 || h == 0 {
            continue;
        }
        let row = row_bytes(head, w)?.checked_add(1)?;
        total = total.checked_add(row.checked_mul(h)?)?;
    }
    Some(total)
}

/// What `tRNS` says, by colour type.
enum Transparency {
    None,
    /// A palette's alphas, already written into it.
    Palette,
    /// The grey sample, at the image's depth, that is transparent.
    Grey(u16),
    /// The RGB samples, at the image's depth, that are transparent.
    Rgb([u16; 3]),
}

/// Decodes the stream to eight-bit RGBA.
pub fn decode(data: &[u8]) -> Result<Rgba8, Error> {
    let mut chunks = Chunks::new(data)?;
    let head = read_header(&chunks.next_chunk()?.checked()?)?;
    let expected = inflated_size(&head).ok_or(Error::Axis {
        width: head.width,
        height: head.height,
    })?;
    // Twice the rows plus 64 KiB: a fixed-Huffman block of literals runs
    // up to an eighth past its rows, and dynamic blocks resend their
    // tables, so the bound only stops a stream no encoder writes.
    let budget = expected.saturating_mul(2).saturating_add(64 << 10);
    let indexed = head.colour == Colour::Palette;
    let mut palette: Option<Vec<[u8; 4]>> = None;
    let mut transparency = Transparency::None;
    let mut image: Vec<u8> = Vec::new();
    // 0 before the first IDAT, 1 among them, 2 after.
    let mut idat = 0u8;
    loop {
        let chunk = chunks.next_chunk()?.checked()?;
        let kind = chunk.kind;
        if &kind == b"IDAT" {
            if idat == 2 {
                return Err(Error::Order(kind));
            }
            if indexed && palette.is_none() {
                return Err(Error::Palette);
            }
            idat = 1;
            if image.len().saturating_add(chunk.data.len()) > budget {
                return Err(Error::Budget);
            }
            image.extend_from_slice(chunk.data);
            continue;
        }
        if idat == 1 {
            idat = 2;
        }
        if !chunk.sound {
            // An ancillary chunk with a bad CRC: skipped, as libpng does.
            continue;
        }
        match &kind {
            b"IEND" => {
                if !chunk.data.is_empty() {
                    return Err(Error::Malformed(kind));
                }
                break;
            }
            b"IHDR" => return Err(Error::Order(kind)),
            b"PLTE" if indexed => {
                if idat != 0 || palette.is_some() {
                    return Err(Error::Order(kind));
                }
                if chunk.data.len() % 3 != 0 || chunk.data.is_empty() {
                    return Err(Error::Malformed(kind));
                }
                // Only indices below 2^depth can occur, so a longer
                // palette is cut to them, as libpng cuts it.
                let (triples, _) = chunk.data.as_chunks::<3>();
                let most = 1usize << head.depth;
                palette = Some(
                    triples
                        .iter()
                        .take(most)
                        .map(|&[r, g, b]| [r, g, b, 255])
                        .collect(),
                );
            }
            b"tRNS" => {
                // Only a valid one before the image data, once, is read;
                // any other is skipped, as libpng skips it.
                if idat == 0 && matches!(transparency, Transparency::None) {
                    transparency = read_transparency(&head, chunk.data, palette.as_mut());
                }
            }
            _ => {
                // A suggested palette in an RGB image, a palette in a
                // grey one, and every other ancillary chunk are skipped.
                if chunk.critical() && &kind != b"PLTE" {
                    return Err(Error::Critical(kind));
                }
            }
        }
    }
    if idat == 0 {
        return Err(Error::NoData);
    }
    let raw = inflate(&image, expected)?;
    drop(image);
    expand(&head, &raw, palette.as_deref(), &transparency)
}

/// What a `tRNS` chunk says for `head`'s colour type, a key's bits above
/// the image depth masked off as the specification asks of a decoder;
/// `Transparency::None` for one of the wrong length or colour type, or a
/// palette's before its `PLTE` or longer than it. A palette's alphas are
/// written into the palette, and the chunk is then marked read.
fn read_transparency(
    head: &Header,
    data: &[u8],
    palette: Option<&mut Vec<[u8; 4]>>,
) -> Transparency {
    let mask = if head.depth == 16 {
        u16::MAX
    } else {
        (1u16 << head.depth) - 1
    };
    let key = |at: usize| -> u16 {
        let high = data.get(at).copied().unwrap_or(0);
        let low = data.get(at + 1).copied().unwrap_or(0);
        u16::from_be_bytes([high, low]) & mask
    };
    match head.colour {
        Colour::Grey if data.len() == 2 => Transparency::Grey(key(0)),
        Colour::Rgb if data.len() == 6 => Transparency::Rgb([key(0), key(2), key(4)]),
        Colour::Palette => match palette {
            Some(entries) if !data.is_empty() && data.len() <= entries.len() => {
                for (entry, alpha) in entries.iter_mut().zip(data) {
                    if let Some(slot) = entry.get_mut(3) {
                        *slot = *alpha;
                    }
                }
                Transparency::Palette
            }
            _ => Transparency::None,
        },
        _ => Transparency::None,
    }
}

/// The zlib stream: header checked, DEFLATE inflated to exactly `expected`
/// bytes, then the Adler-32 and nothing after it.
fn inflate(stream: &[u8], expected: usize) -> Result<Vec<u8>, Error> {
    let cmf = *stream.first().ok_or(Error::Truncated)?;
    let flg = *stream.get(1).ok_or(Error::Truncated)?;
    if cmf & 0x0f != 8 || cmf >> 4 > 7 || (u16::from(cmf) * 256 + u16::from(flg)) % 31 != 0 {
        return Err(Error::Zlib);
    }
    if flg & 0x20 != 0 {
        return Err(Error::Zlib);
    }
    let body = stream.get(2..).ok_or(Error::Truncated)?;
    // The inflater refuses output past its bound, so a stream that runs
    // long is refused there, by name.
    let (raw, consumed) = gzip::decompress_raw_prefix(body, expected).map_err(|why| {
        if why.starts_with("DEFLATE output exceeds") {
            Error::Length
        } else {
            Error::Inflate(why)
        }
    })?;
    if raw.len() != expected {
        return Err(Error::Length);
    }
    let trailer = body.get(consumed..).ok_or(Error::Truncated)?;
    if trailer.len() < 4 {
        return Err(Error::Truncated);
    }
    if trailer.len() > 4 {
        return Err(Error::Length);
    }
    if u32::from_be_bytes(four(trailer, 0)?) != adler32(&raw) {
        return Err(Error::Adler);
    }
    Ok(raw)
}

/// Adler-32 (RFC 1950 §9), summed in runs short enough that neither sum
/// can overflow before its reduction.
fn adler32(data: &[u8]) -> u32 {
    const MOD: u32 = 65521;
    let (mut a, mut b) = (1u32, 0u32);
    for run in data.chunks(5552) {
        for byte in run {
            a += u32::from(*byte);
            b += a;
        }
        a %= MOD;
        b %= MOD;
    }
    (b << 16) | a
}

/// Paeth's predictor (PNG §9.4).
fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let p = i16::from(a) + i16::from(b) - i16::from(c);
    let pa = (p - i16::from(a)).abs();
    let pb = (p - i16::from(b)).abs();
    let pc = (p - i16::from(c)).abs();
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

/// Undoes one row's filter in place, `prior` the unfiltered row above
/// (zeros for a pass's first row), `bpp` the bytes per complete pixel
/// (at least one).
fn unfilter(kind: u8, row: &mut [u8], prior: &[u8], bpp: usize) -> Result<(), Error> {
    match kind {
        0 => {}
        1 => {
            for i in bpp..row.len() {
                let left = row.get(i - bpp).copied().unwrap_or(0);
                if let Some(x) = row.get_mut(i) {
                    *x = x.wrapping_add(left);
                }
            }
        }
        2 => {
            for (x, up) in row.iter_mut().zip(prior) {
                *x = x.wrapping_add(*up);
            }
        }
        3 => {
            for i in 0..row.len() {
                let left = if i >= bpp {
                    row.get(i - bpp).copied().unwrap_or(0)
                } else {
                    0
                };
                let up = prior.get(i).copied().unwrap_or(0);
                let mean = ((u16::from(left) + u16::from(up)) / 2) as u8;
                if let Some(x) = row.get_mut(i) {
                    *x = x.wrapping_add(mean);
                }
            }
        }
        4 => {
            for i in 0..row.len() {
                let (left, corner) = if i >= bpp {
                    (
                        row.get(i - bpp).copied().unwrap_or(0),
                        prior.get(i - bpp).copied().unwrap_or(0),
                    )
                } else {
                    (0, 0)
                };
                let up = prior.get(i).copied().unwrap_or(0);
                let predicted = paeth(left, up, corner);
                if let Some(x) = row.get_mut(i) {
                    *x = x.wrapping_add(predicted);
                }
            }
        }
        _ => return Err(Error::Filter(kind)),
    }
    Ok(())
}

/// The `index`th sample of an unfiltered row at `depth` bits.
fn sample(row: &[u8], index: usize, depth: u8) -> u16 {
    match depth {
        16 => {
            let at = index * 2;
            u16::from_be_bytes([
                row.get(at).copied().unwrap_or(0),
                row.get(at + 1).copied().unwrap_or(0),
            ])
        }
        8 => u16::from(row.get(index).copied().unwrap_or(0)),
        _ => {
            let depth = usize::from(depth);
            let bit = index * depth;
            let byte = row.get(bit / 8).copied().unwrap_or(0);
            let shift = 8 - depth - bit % 8;
            u16::from((byte >> shift) & ((1u8 << depth) - 1))
        }
    }
}

/// A sample at `depth` bits scaled to eight, rounding to nearest.
fn to8(value: u16, depth: u8) -> u8 {
    match depth {
        16 => ((u32::from(value) * 255 + 32767) / 65535) as u8,
        8 => value as u8,
        _ => {
            let most = (1u32 << depth) - 1;
            (u32::from(value) * 255 / most) as u8
        }
    }
}

/// Unfilters every pass and writes each pixel into the RGBA frame.
fn expand(
    head: &Header,
    raw: &[u8],
    palette: Option<&[[u8; 4]]>,
    transparency: &Transparency,
) -> Result<Rgba8, Error> {
    let size = head
        .width
        .checked_mul(head.height)
        .and_then(|n| n.checked_mul(4))
        .ok_or(Error::Axis {
            width: head.width,
            height: head.height,
        })?;
    let mut out = Rgba8 {
        width: head.width,
        height: head.height,
        data: vec![0; size],
    };
    let channels = head.colour.channels();
    let bpp = (channels * usize::from(head.depth)).div_ceil(8).max(1);
    let mut at = 0usize;
    let mut row = Vec::new();
    let mut prior = Vec::new();
    for (x0, y0, dx, dy, w, h) in passes(head) {
        if w == 0 || h == 0 {
            continue;
        }
        let len = row_bytes(head, w).ok_or(Error::Length)?;
        prior.clear();
        prior.resize(len, 0);
        for r in 0..h {
            let kind = *raw.get(at).ok_or(Error::Length)?;
            let bytes = raw.get(at + 1..at + 1 + len).ok_or(Error::Length)?;
            at += 1 + len;
            row.clear();
            row.extend_from_slice(bytes);
            unfilter(kind, &mut row, &prior, bpp)?;
            let y = y0 + r * dy;
            for c in 0..w {
                let x = x0 + c * dx;
                let pixel = rgba(head, &row, c * channels, palette, transparency)?;
                let to = (y * head.width + x) * 4;
                out.data
                    .get_mut(to..to + 4)
                    .ok_or(Error::Length)?
                    .copy_from_slice(&pixel);
            }
            std::mem::swap(&mut row, &mut prior);
        }
    }
    Ok(out)
}

/// The pixel whose first sample is the row's `first`th.
fn rgba(
    head: &Header,
    row: &[u8],
    first: usize,
    palette: Option<&[[u8; 4]]>,
    transparency: &Transparency,
) -> Result<[u8; 4], Error> {
    let depth = head.depth;
    let s = |i: usize| sample(row, first + i, depth);
    let v = |i: usize| to8(s(i), depth);
    Ok(match head.colour {
        Colour::Grey => {
            let grey = v(0);
            let alpha = match transparency {
                Transparency::Grey(key) if *key == s(0) => 0,
                _ => 255,
            };
            [grey, grey, grey, alpha]
        }
        Colour::Rgb => {
            let alpha = match transparency {
                Transparency::Rgb(key) if *key == [s(0), s(1), s(2)] => 0,
                _ => 255,
            };
            [v(0), v(1), v(2), alpha]
        }
        Colour::Palette => *palette
            .and_then(|p| p.get(usize::from(s(0))))
            .ok_or(Error::Palette)?,
        Colour::GreyAlpha => {
            let grey = v(0);
            [grey, grey, grey, v(1)]
        }
        Colour::Rgba => [v(0), v(1), v(2), v(3)],
    })
}

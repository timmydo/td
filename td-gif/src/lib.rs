#![forbid(unsafe_code)]
//! GIF87a/89a first-image decoder, into a logical-screen RGBA canvas.
//! Pure, bounded byte-slice API: no animation, files, threads or ambient
//! state. See DESIGN.md for transparency, skipped extensions and limits.

use std::fmt;

pub const MAX_AXIS: usize = 16384;
pub const MAX_IMAGE_PIXELS: usize = 16 << 20;
/// Bytes examined up to the end of the first image (later frames ignored).
pub const MAX_INPUT_BYTES: usize = 64 << 20;
/// Blocks plus data sub-blocks examined before returning the first image.
pub const MAX_BLOCKS: usize = 1 << 20;
/// LZW code budget, including clear codes that produce no pixels.
pub const MAX_CODES: usize = 32 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub width: usize,
    pub height: usize,
}

/// Eight-bit straight-alpha RGBA, rows top to bottom, no padding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rgba8 {
    pub width: usize,
    pub height: usize,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    NotGif,
    Truncated,
    Axis,
    Limit,
    Block,
    Extension,
    PlainText,
    Palette,
    Frame,
    Lzw,
    Pixels,
    NoImage,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotGif => "not a GIF87a or GIF89a stream",
            Self::Truncated => "GIF data ends early",
            Self::Axis => "GIF canvas axes exceed the pixel budget",
            Self::Limit => "GIF decode budget exceeded",
            Self::Block => "GIF block malformed",
            Self::Extension => "GIF extension malformed",
            Self::PlainText => "GIF plain-text rendering unsupported",
            Self::Palette => "GIF palette missing or index out of range",
            Self::Frame => "GIF image rectangle outside the canvas",
            Self::Lzw => "GIF LZW code stream malformed",
            Self::Pixels => "GIF image pixel count does not match its rectangle",
            Self::NoImage => "GIF trailer before any image",
        })
    }
}
impl std::error::Error for Error {}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
    blocks: usize,
}
impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            blocks: 0,
        }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        let end = self.pos.checked_add(n).ok_or(Error::Limit)?;
        if end > MAX_INPUT_BYTES {
            return Err(Error::Limit);
        }
        let slice = self.data.get(self.pos..end).ok_or(Error::Truncated)?;
        self.pos = end;
        Ok(slice)
    }
    fn byte(&mut self) -> Result<u8, Error> {
        self.take(1)?.first().copied().ok_or(Error::Truncated)
    }
    fn word(&mut self) -> Result<usize, Error> {
        let bytes: [u8; 2] = self.take(2)?.try_into().map_err(|_| Error::Truncated)?;
        Ok(usize::from(u16::from_le_bytes(bytes)))
    }
    fn block(&mut self) -> Result<(), Error> {
        self.blocks += 1;
        if self.blocks > MAX_BLOCKS {
            return Err(Error::Limit);
        }
        Ok(())
    }
    fn sub_block(&mut self) -> Result<&'a [u8], Error> {
        self.block()?;
        let len = usize::from(self.byte()?);
        self.take(len)
    }
    fn skip_sub_blocks(&mut self) -> Result<(), Error> {
        while !self.sub_block()?.is_empty() {}
        Ok(())
    }
}

struct Screen {
    header: Header,
    flags: u8,
    background: u8,
}
fn screen(reader: &mut Reader<'_>) -> Result<Screen, Error> {
    match reader.take(6)? {
        b"GIF87a" | b"GIF89a" => {}
        _ => return Err(Error::NotGif),
    }
    let width = reader.word()?;
    let height = reader.word()?;
    if width == 0
        || height == 0
        || width > MAX_AXIS
        || height > MAX_AXIS
        || width
            .checked_mul(height)
            .is_none_or(|v| v > MAX_IMAGE_PIXELS)
    {
        return Err(Error::Axis);
    }
    let flags = reader.byte()?;
    let background = reader.byte()?;
    let _aspect = reader.byte()?;
    Ok(Screen {
        header: Header { width, height },
        flags,
        background,
    })
}

/// Read the bounded logical-screen dimensions, without parsing image data.
pub fn header(bytes: &[u8]) -> Result<Header, Error> {
    Ok(screen(&mut Reader::new(bytes))?.header)
}

fn palette<'a>(reader: &mut Reader<'a>, flags: u8) -> Result<&'a [u8], Error> {
    if flags & 0x80 == 0 {
        return Ok(&[]);
    }
    reader.take(3 * (2usize << (flags & 7)))
}
fn colour(palette: &[u8], index: u8) -> Result<[u8; 4], Error> {
    let at = usize::from(index) * 3;
    let rgb: [u8; 3] = palette
        .get(at..at + 3)
        .ok_or(Error::Palette)?
        .try_into()
        .map_err(|_| Error::Palette)?;
    let [r, g, b] = rgb;
    Ok([r, g, b, 255])
}

/// Decode only the first image descriptor and its complete LZW data stream.
/// Later frames and the trailer are not read or required. Transparency
/// clears the canvas; otherwise the global background colour fills it.
pub fn decode(bytes: &[u8]) -> Result<Rgba8, Error> {
    let mut reader = Reader::new(bytes);
    let screen = screen(&mut reader)?;
    let global = palette(&mut reader, screen.flags)?;
    let mut transparent = None;
    let mut control = false;
    loop {
        reader.block()?;
        match reader.byte()? {
            0x3b => return Err(Error::NoImage),
            0x21 => match reader.byte()? {
                0xf9 => {
                    if control || reader.byte()? != 4 {
                        return Err(Error::Extension);
                    }
                    let flags = reader.byte()?;
                    if flags & 0xe0 != 0 || (flags >> 2) & 7 > 3 {
                        return Err(Error::Extension);
                    }
                    let _delay = reader.word()?;
                    let index = reader.byte()?;
                    if reader.byte()? != 0 {
                        return Err(Error::Extension);
                    }
                    transparent = if flags & 1 != 0 { Some(index) } else { None };
                    control = true;
                }
                0x01 => return Err(Error::PlainText),
                0xff => {
                    if reader.sub_block()?.len() != 11 {
                        return Err(Error::Extension);
                    }
                    reader.skip_sub_blocks()?;
                }
                // Comments and unrecognised extension labels carry sub-blocks.
                _ => reader.skip_sub_blocks()?,
            },
            0x2c => return image(&mut reader, screen, global, transparent),
            _ => return Err(Error::Block),
        }
    }
}

fn image(
    reader: &mut Reader<'_>,
    screen: Screen,
    global: &[u8],
    transparent: Option<u8>,
) -> Result<Rgba8, Error> {
    let left = reader.word()?;
    let top = reader.word()?;
    let width = reader.word()?;
    let height = reader.word()?;
    if width == 0
        || height == 0
        || left + width > screen.header.width
        || top + height > screen.header.height
    {
        return Err(Error::Frame);
    }
    let flags = reader.byte()?;
    if flags & 0x18 != 0 {
        return Err(Error::Frame);
    }
    let local = palette(reader, flags)?;
    let table = if local.is_empty() { global } else { local };
    if table.is_empty() {
        return Err(Error::Palette);
    }
    if let Some(index) = transparent {
        let _ = colour(table, index)?;
    }
    let background = if transparent.is_some() || global.is_empty() {
        [0; 4]
    } else {
        colour(global, screen.background)?
    };
    let min = reader.byte()?;
    if !(2..=8).contains(&min) {
        return Err(Error::Lzw);
    }
    let mut image = Rgba8 {
        width: screen.header.width,
        height: screen.header.height,
        data: td_alloc::filled(screen.header.width * screen.header.height * 4, 0)
            .map_err(|_| Error::Limit)?,
    };
    for pixel in image.data.as_chunks_mut::<4>().0 {
        *pixel = background;
    }
    let mut x = 0usize;
    let mut y = 0usize;
    let mut pass = 0usize;
    let interlaced = flags & 0x40 != 0;
    const PASSES: &[(usize, usize)] = &[(0, 8), (4, 8), (2, 4), (1, 2)];
    lzw(reader, min, width * height, |index| {
        let value = if Some(index) == transparent {
            [0; 4]
        } else {
            colour(table, index)?
        };
        let at = ((top + y) * image.width + left + x) * 4;
        image
            .data
            .get_mut(at..at + 4)
            .ok_or(Error::Pixels)?
            .copy_from_slice(&value);
        x += 1;
        if x == width {
            x = 0;
            if interlaced {
                y += PASSES.get(pass).ok_or(Error::Pixels)?.1;
                while y >= height && pass < 3 {
                    pass += 1;
                    y = PASSES.get(pass).ok_or(Error::Pixels)?.0;
                }
            } else {
                y += 1;
            }
        }
        Ok(())
    })?;
    Ok(image)
}

struct Codes<'a, 'b> {
    reader: &'b mut Reader<'a>,
    remaining: usize,
    acc: u32,
    bits: u8,
    count: usize,
}
impl Codes<'_, '_> {
    fn code(&mut self, width: u8) -> Result<u16, Error> {
        self.count += 1;
        if self.count > MAX_CODES {
            return Err(Error::Limit);
        }
        while self.bits < width {
            if self.remaining == 0 {
                self.reader.block()?;
                self.remaining = usize::from(self.reader.byte()?);
                if self.remaining == 0 {
                    return Err(Error::Lzw);
                }
            }
            self.acc |= u32::from(self.reader.byte()?) << self.bits;
            self.bits += 8;
            self.remaining -= 1;
        }
        let value = (self.acc & ((1 << width) - 1)) as u16;
        self.acc >>= width;
        self.bits -= width;
        Ok(value)
    }
    fn finish(&mut self) -> Result<(), Error> {
        // Only the last partial byte may follow EOI, then the terminator.
        if self.remaining != 0 {
            return Err(Error::Lzw);
        }
        self.reader.block()?;
        if self.reader.byte()? != 0 {
            return Err(Error::Lzw);
        }
        Ok(())
    }
}

fn lzw(
    reader: &mut Reader<'_>,
    min: u8,
    pixels: usize,
    mut emit: impl FnMut(u8) -> Result<(), Error>,
) -> Result<(), Error> {
    let clear = 1u16 << min;
    let end = clear + 1;
    let mut codes = Codes {
        reader,
        remaining: 0,
        acc: 0,
        bits: 0,
        count: 0,
    };
    let mut width = min + 1;
    let mut next = clear + 2;
    let mut previous: Option<u16> = None;
    let mut prefix = [0u16; 4096];
    let mut suffix = [0u8; 4096];
    let mut stack = [0u8; 4096];
    let mut produced = 0usize;
    if codes.code(width)? != clear {
        return Err(Error::Lzw);
    }
    loop {
        let code = codes.code(width)?;
        if code == clear {
            width = min + 1;
            next = clear + 2;
            previous = None;
            continue;
        }
        if code == end {
            if produced != pixels {
                return Err(Error::Pixels);
            }
            return codes.finish();
        }
        let special = code == next && previous.is_some();
        if code > next || (code == next && !special) {
            return Err(Error::Lzw);
        }
        let mut current = if special {
            previous.ok_or(Error::Lzw)?
        } else {
            code
        };
        let mut used = 0usize;
        while current >= clear {
            if current < clear + 2 || current >= next {
                return Err(Error::Lzw);
            }
            let at = usize::from(current);
            *stack.get_mut(used).ok_or(Error::Lzw)? = *suffix.get(at).ok_or(Error::Lzw)?;
            used += 1;
            let parent = *prefix.get(at).ok_or(Error::Lzw)?;
            // Every dictionary entry names an earlier string: no cycles.
            if parent >= current {
                return Err(Error::Lzw);
            }
            current = parent;
        }
        let first = current as u8;
        *stack.get_mut(used).ok_or(Error::Lzw)? = first;
        used += 1;
        let count = used + usize::from(special);
        if count > pixels.saturating_sub(produced) {
            return Err(Error::Pixels);
        }
        for &index in stack.get(..used).ok_or(Error::Lzw)?.iter().rev() {
            emit(index)?;
        }
        if special {
            emit(first)?;
        }
        produced += count;
        if let Some(old) = previous {
            if next < 4096 {
                *prefix.get_mut(usize::from(next)).ok_or(Error::Lzw)? = old;
                *suffix.get_mut(usize::from(next)).ok_or(Error::Lzw)? = first;
                next += 1;
                if next == 1 << width && width < 12 {
                    width += 1;
                }
            }
        }
        previous = Some(code);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn budgets_reject_before_reading_or_producing_more_data() {
        let mut reader = Reader::new(&[]);
        reader.pos = MAX_INPUT_BYTES;
        assert_eq!(reader.byte(), Err(Error::Limit));
        reader.blocks = MAX_BLOCKS - 1;
        assert_eq!(reader.block(), Ok(()));
        assert_eq!(reader.block(), Err(Error::Limit));
        // Two three-bit codes in one byte, at the final permitted code.
        let mut reader = Reader::new(&[1, 0b0010_1100, 0]);
        let mut codes = Codes {
            reader: &mut reader,
            remaining: 0,
            acc: 0,
            bits: 0,
            count: MAX_CODES - 1,
        };
        assert_eq!(codes.code(3), Ok(4));
        assert_eq!(codes.code(3), Err(Error::Limit));
    }
}

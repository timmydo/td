#![forbid(unsafe_code)]
//! Bounded WebP decoding into straight-alpha RGBA.

use std::fmt;
mod lossless;

pub const MAX_AXIS: usize = 16384;
pub const MAX_IMAGE_PIXELS: usize = 16 << 20;
pub const MAX_INPUT_BYTES: usize = 64 << 20;
pub const MAX_CHUNKS: usize = 4096;
pub const MAX_PREFIX_GROUPS: usize = 256;
pub const MAX_WORK: usize = 256 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub width: usize,
    pub height: usize,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rgba8 {
    pub width: usize,
    pub height: usize,
    pub data: Vec<u8>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    NotWebp,
    Truncated,
    Container,
    Axis,
    Limit,
    Unsupported,
    Bitstream,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotWebp => "not a WebP stream",
            Self::Truncated => "WebP data ends early",
            Self::Container => "malformed WebP container",
            Self::Axis => "WebP dimensions exceed the pixel budget",
            Self::Limit => "WebP decode budget exceeded",
            Self::Unsupported => "unsupported WebP encoding",
            Self::Bitstream => "malformed WebP bitstream",
        })
    }
}
impl std::error::Error for Error {}

fn dimensions(width: usize, height: usize) -> Result<Header, Error> {
    if width == 0
        || height == 0
        || width > MAX_AXIS
        || height > MAX_AXIS
        || width
            .checked_mul(height)
            .is_none_or(|n| n > MAX_IMAGE_PIXELS)
    {
        return Err(Error::Axis);
    }
    Ok(Header { width, height })
}
fn le(bytes: &[u8]) -> usize {
    bytes
        .iter()
        .rev()
        .fold(0, |v, b| (v << 8) | usize::from(*b))
}
fn take(bytes: &[u8], start: usize, len: usize) -> Result<&[u8], Error> {
    bytes
        .get(start..start.checked_add(len).ok_or(Error::Limit)?)
        .ok_or(Error::Truncated)
}
type Chunk<'a> = (&'a [u8], &'a [u8]);

struct Chunks<'a> {
    data: &'a [u8],
    pos: usize,
    count: usize,
}
impl<'a> Chunks<'a> {
    fn next(&mut self) -> Result<Option<Chunk<'a>>, Error> {
        if self.pos == self.data.len() {
            return Ok(None);
        }
        self.count += 1;
        if self.count > MAX_CHUNKS {
            return Err(Error::Limit);
        }
        let id = take(self.data, self.pos, 4)?;
        let n = le(take(self.data, self.pos + 4, 4)?);
        let data = take(self.data, self.pos + 8, n)?;
        self.pos = self.pos.checked_add(8 + n).ok_or(Error::Limit)?;
        if n & 1 != 0 {
            if take(self.data, self.pos, 1)? != [0] {
                return Err(Error::Container);
            }
            self.pos += 1;
        }
        Ok(Some((id, data)))
    }
}
fn chunks(bytes: &[u8]) -> Result<Chunks<'_>, Error> {
    if bytes.len() > MAX_INPUT_BYTES {
        return Err(Error::Limit);
    }
    if take(bytes, 0, 4)? != b"RIFF" || take(bytes, 8, 4)? != b"WEBP" {
        return Err(Error::NotWebp);
    }
    let n = le(take(bytes, 4, 4)?);
    if n < 4 || n & 1 != 0 {
        return Err(Error::Container);
    }
    let data = take(bytes, 12, n - 4)?;
    Ok(Chunks {
        data,
        pos: 0,
        count: 0,
    })
}
fn coded_header(id: &[u8], data: &[u8]) -> Result<Header, Error> {
    match id {
        b"VP8L" => {
            if data.first() != Some(&0x2f) {
                return Err(Error::Bitstream);
            }
            let v = le(take(data, 1, 4)?);
            if v >> 29 != 0 {
                return Err(Error::Unsupported);
            }
            dimensions((v & 0x3fff) + 1, ((v >> 14) & 0x3fff) + 1)
        }
        b"VP8 " => {
            let tag = le(take(data, 0, 3)?);
            if tag & 1 != 0
                || tag & 16 == 0
                || (tag >> 1) & 7 > 3
                || take(data, 3, 3)? != [0x9d, 1, 0x2a]
            {
                return Err(Error::Bitstream);
            }
            dimensions(
                le(take(data, 6, 2)?) & 0x3fff,
                le(take(data, 8, 2)?) & 0x3fff,
            )
        }
        _ => Err(Error::Unsupported),
    }
}
pub fn header(bytes: &[u8]) -> Result<Header, Error> {
    let (id, data) = chunks(bytes)?.next()?.ok_or(Error::Container)?;
    if id == b"VP8X" {
        if data.len() != 10 {
            return Err(Error::Container);
        }
        dimensions(le(take(data, 4, 3)?) + 1, le(take(data, 7, 3)?) + 1)
    } else {
        coded_header(id, data)
    }
}
pub fn decode(bytes: &[u8]) -> Result<Rgba8, Error> {
    let expected = header(bytes)?;
    let mut chunks = chunks(bytes)?;
    let mut output = None;
    while let Some((id, data)) = chunks.next()? {
        match id {
            b"VP8X" if chunks.count == 1 => {
                if data.first().is_some_and(|v| v & 2 != 0) {
                    return Err(Error::Unsupported);
                }
            }
            b"VP8L" if output.is_none() => {
                if coded_header(id, data)? != expected {
                    return Err(Error::Container);
                }
                let pixels = lossless::decode(
                    take(data, 5, data.len().saturating_sub(5))?,
                    expected.width,
                    expected.height,
                )?;
                let mut rgba = Vec::new();
                rgba.try_reserve_exact(pixels.len() * 4)
                    .map_err(|_| Error::Limit)?;
                for p in pixels {
                    rgba.extend_from_slice(&[
                        (p >> 16) as u8,
                        (p >> 8) as u8,
                        p as u8,
                        (p >> 24) as u8,
                    ]);
                }
                output = Some(Rgba8 {
                    width: expected.width,
                    height: expected.height,
                    data: rgba,
                });
            }
            b"VP8 " | b"ALPH" | b"ANIM" | b"ANMF" => return Err(Error::Unsupported),
            b"VP8X" | b"VP8L" => return Err(Error::Container),
            _ => {}
        }
    }
    output.ok_or(Error::Container)
}

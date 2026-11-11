#![forbid(unsafe_code)]
//! Bounded primary-image AVIF decoding into straight-alpha RGBA.

use std::fmt;
mod container;
#[cfg(test)]
mod tests;

pub const MAX_INPUT_BYTES: usize = 64 << 20;
pub const MAX_AXIS: usize = 16384;
pub const MAX_IMAGE_PIXELS: usize = 16 << 20;
pub const MAX_BOXES: usize = 4096;
pub const MAX_ITEMS: usize = 256;
pub const MAX_PROPERTIES: usize = 256;
pub const MAX_EXTENTS: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub width: usize,
    pub height: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    NotAvif,
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
            Self::NotAvif => "not an AVIF file",
            Self::Truncated => "AVIF data ends early",
            Self::Container => "malformed AVIF container",
            Self::Axis => "AVIF dimensions exceed the pixel budget",
            Self::Limit => "AVIF parsing budget exceeded",
            Self::Unsupported => "unsupported AVIF feature",
            Self::Bitstream => "malformed AV1 image data",
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

/// The primary coded image, before AV1 pixel decoding. An image item can
/// occupy several extents; extraction joins only its admitted bytes.
pub struct ImageItem {
    pub header: Header,
    pub av1_config: [u8; 4],
    pub data: Vec<u8>,
    pub colour: Option<Colour>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Colour {
    pub primaries: u16,
    pub transfer: u16,
    pub matrix: u16,
    pub full_range: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rgba8 {
    pub width: usize,
    pub height: usize,
    pub data: Vec<u8>,
}

/// Inspect only the first file-type box; this does not validate the image.
pub fn is_avif(bytes: &[u8]) -> bool {
    container::is_avif(bytes)
}

pub fn header(bytes: &[u8]) -> Result<Header, Error> {
    Ok(container::parse(bytes)?.header)
}

pub fn primary_image(bytes: &[u8]) -> Result<ImageItem, Error> {
    let item = container::parse(bytes)?;
    let mut data = Vec::new();
    data.try_reserve_exact(item.length)
        .map_err(|_| Error::Limit)?;
    for extent in item.extents {
        data.extend_from_slice(extent);
    }
    Ok(ImageItem {
        header: item.header,
        av1_config: item.config,
        data,
        colour: item.colour,
    })
}

fn codec_error(error: td_av1::Error) -> Error {
    match error {
        td_av1::Error::Truncated => Error::Truncated,
        td_av1::Error::Bitstream => Error::Bitstream,
        td_av1::Error::Unsupported => Error::Unsupported,
        td_av1::Error::Limit => Error::Limit,
    }
}

/// Decode the supported eight-bit 4:2:0 still-picture subset. Other AV1
/// tools, alpha, animation, transforms and HDR return `Unsupported`.
pub fn decode(bytes: &[u8]) -> Result<Rgba8, Error> {
    let item = primary_image(bytes)?;
    let header = td_av1::header(&item.data).map_err(codec_error)?;
    if (header.width, header.height) != (item.header.width, item.header.height)
        || header.config != item.av1_config
    {
        return Err(Error::Container);
    }
    let colour = Colour {
        primaries: u16::from(header.primaries),
        transfer: u16::from(header.transfer),
        matrix: u16::from(header.matrix),
        full_range: header.full_range,
    };
    if item.colour.is_some_and(|c| c != colour) {
        return Err(Error::Container);
    }
    if ![1, 2].contains(&colour.primaries)
        || ![1, 2, 13].contains(&colour.transfer)
        || ![1, 2, 5, 6].contains(&colour.matrix)
    {
        return Err(Error::Unsupported);
    }
    let image = td_av1::decode(&item.data).map_err(codec_error)?;
    drop(item.data);
    let mut rgba = Vec::new();
    rgba.try_reserve_exact(header.width * header.height * 4)
        .map_err(|_| Error::Limit)?;
    let [y, u, v] = &image.planes;
    let chroma_width = header.width.div_ceil(2);
    for row in 0..header.height {
        for col in 0..header.width {
            let at = row * header.width + col;
            let chroma = (row / 2) * chroma_width + col / 2;
            let samples = [
                *y.get(at).ok_or(Error::Bitstream)?,
                *u.get(chroma).ok_or(Error::Bitstream)?,
                *v.get(chroma).ok_or(Error::Bitstream)?,
            ];
            rgba.extend_from_slice(&rgb(samples, colour));
            rgba.push(255);
        }
    }
    Ok(Rgba8 {
        width: header.width,
        height: header.height,
        data: rgba,
    })
}

fn rgb([y, cb, cr]: [u8; 3], colour: Colour) -> [u8; 3] {
    // BT.601/709 inverse matrices at sixteen fractional bits; unspecified
    // matrix uses BT.601. Chroma uses nearest 4:2:0 samples.
    let (scale, offset, red, gu, gv, blue) = match (colour.full_range, colour.matrix == 1) {
        (true, false) => (65536, 0, 91881, 22554, 46802, 116130),
        (true, true) => (65536, 0, 103206, 12276, 30679, 121609),
        (false, false) => (76309, 16, 104597, 25675, 53279, 132201),
        (false, true) => (76309, 16, 117489, 13975, 34925, 138438),
    };
    let y = (i32::from(y) - offset) * scale;
    let (cb, cr) = (i32::from(cb) - 128, i32::from(cr) - 128);
    [y + red * cr, y - gu * cb - gv * cr, y + blue * cb]
        .map(|v| ((v + 32768) >> 16).clamp(0, 255) as u8)
}

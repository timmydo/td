#![forbid(unsafe_code)]
//! Bounded primary-image extraction from AVIF containers.

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
    })
}

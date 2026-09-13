//! Owned RGB image buffers and the binary PPM writer. The writer takes any
//! `Write`; it opens nothing itself. The fields are public so a pipeline
//! can fill a buffer in place, which is why every accessor checks the axes
//! against the buffer rather than trusting them.

use std::io::{self, Write};

/// The longest axis any RGB buffer here may have; the raw reader's ceiling
/// is the same number, and a test holds them together.
pub const MAX_AXIS: usize = 16384;
/// The most pixels one image buffer holds: 64 Mi, so the largest `f32`
/// RGB buffer the pipeline makes (12 bytes a pixel) is 768 MiB, under the
/// allocation limit of a 32-bit target.
pub const MAX_IMAGE_PIXELS: usize = 64 << 20;

/// Eight-bit interleaved RGB, rows top to bottom, no padding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rgb8 {
    pub width: usize,
    pub height: usize,
    pub data: Vec<u8>,
}

impl Rgb8 {
    /// A black image, or `None` for a zero axis, one past `MAX_AXIS`, or
    /// more than `MAX_IMAGE_PIXELS`.
    pub fn new(width: usize, height: usize) -> Option<Self> {
        if width == 0
            || height == 0
            || width > MAX_AXIS
            || height > MAX_AXIS
            || width * height > MAX_IMAGE_PIXELS
        {
            return None;
        }
        Some(Self {
            width,
            height,
            data: vec![0; width * height * 3],
        })
    }

    /// Whether the axes are nonzero, within `MAX_AXIS`, the pixel count
    /// within `MAX_IMAGE_PIXELS`, and the buffer exactly
    /// `width * height * 3` bytes.
    pub fn is_consistent(&self) -> bool {
        self.width != 0
            && self.height != 0
            && self.width <= MAX_AXIS
            && self.height <= MAX_AXIS
            && self.width * self.height <= MAX_IMAGE_PIXELS
            && self.width * self.height * 3 == self.data.len()
    }

    /// The pixel at (x, y), or `None` outside the image or its buffer.
    pub fn pixel(&self, x: usize, y: usize) -> Option<[u8; 3]> {
        if x >= self.width || y >= self.height {
            return None;
        }
        // Checked: the fields are public, so the axes need not agree with
        // the buffer.
        let at = y.checked_mul(self.width)?.checked_add(x)?.checked_mul(3)?;
        let bytes = self.data.get(at..at.checked_add(3)?)?;
        Some([*bytes.first()?, *bytes.get(1)?, *bytes.get(2)?])
    }
}

/// Writes a binary `P6` PPM with a 255 maximum.
pub fn write_ppm(image: &Rgb8, out: &mut dyn Write) -> io::Result<()> {
    if !image.is_consistent() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "image buffer does not match its axes",
        ));
    }
    write!(out, "P6\n{} {}\n255\n", image.width, image.height)?;
    out.write_all(&image.data)?;
    out.flush()
}

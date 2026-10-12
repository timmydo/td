//! JPEG for td-photo: the thumbnail rule over td-jpeg's decoder, the
//! quality export writes at, and the pool export's JPEG encoder
//! (`td_jpeg::Encoder`) spreads its transform over (`Bands`). Nothing
//! here reads a file, the environment or a clock.

use crate::image::Rgb8;
use td_jpeg::{decode, header, Scale};

/// A thumbnail's refusal is the decoder's.
pub use td_jpeg::Error;

/// The quality export writes at: the standard tables scaled the way every
/// encoder since IJG scales them (`scale = 200 - 2q` from 50 up), 4:4:4.
pub const QUALITY: u8 = 92;

/// td-jpeg's encoder's pieces over `develop`'s bands, on at most this
/// many threads.
pub struct Bands(pub usize);

impl td_jpeg::Runner for Bands {
    fn run<T: Send>(&self, items: Vec<T>, work: &(dyn Fn(T) + Sync)) {
        crate::develop::bands(items, self.0, work);
    }
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

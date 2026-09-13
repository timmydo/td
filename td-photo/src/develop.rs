//! Development: the superpixel demosaic that makes level 1 from a decoded
//! frame, the separable area/bilinear resampler, the per-pixel pipeline
//! (white balance and clip, exposure, camera matrix, transfer) and the
//! orientation step. Row bands are pulled from one shared queue by the
//! caller and the scoped threads it could start, so a thread the system
//! refuses costs parallelism and never the result; buffers are sized once
//! per call under `image::MAX_AXIS` and `image::MAX_IMAGE_PIXELS`. Nothing
//! here reads a file, the environment or a clock.

use std::fmt;
use std::sync::Mutex;

use crate::color::{apply, CameraColor, Matrix, Transfer};
use crate::image::{Rgb8, MAX_AXIS, MAX_IMAGE_PIXELS};
use crate::nef::{Cfa, Channel, Crop, Decoded, MAX_RAW_SAMPLES};

/// The most threads any step spreads over.
pub const MAX_THREADS: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The crop does not lie inside the frame.
    Crop,
    /// A buffer does not match its axes, or an axis is zero or past the
    /// ceiling.
    Size,
    /// White is not above black.
    Levels,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Crop => "crop lies outside the frame",
            Self::Size => "image buffer does not match its axes or exceeds the ceiling",
            Self::Levels => "white level is not above black",
        })
    }
}

impl std::error::Error for Error {}

/// Level 1: camera-native linear RGB, black-subtracted and scaled so the
/// white level is 65535, one pixel per 2x2 quad of the crop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Level1 {
    pub width: usize,
    pub height: usize,
    pub rgb: Vec<u16>,
}

fn thread_count(requested: usize, rows: usize) -> usize {
    requested.clamp(1, MAX_THREADS).min(rows.max(1))
}

/// Rows per band: a few bands per thread so a slow one is shared out.
fn band_rows(rows: usize, threads: usize) -> usize {
    rows.div_ceil(threads.saturating_mul(4).max(1)).max(1)
}

/// Runs `work` over every band exactly once, on up to `threads - 1` scoped
/// threads beside the caller, all pulling from one queue. A thread the
/// system refuses to create is simply absent: the caller drains what is
/// left, so the result never depends on how many threads started.
fn bands<T: Send>(items: Vec<T>, threads: usize, work: impl Fn(T) + Sync) {
    let queue = Mutex::new(items);
    let next = || queue.lock().unwrap_or_else(|e| e.into_inner()).pop();
    std::thread::scope(|scope| {
        for _ in 1..threads {
            let _ = std::thread::Builder::new().spawn_scoped(scope, || {
                while let Some(item) = next() {
                    work(item);
                }
            });
        }
        while let Some(item) = next() {
            work(item);
        }
    });
}

/// Each 2x2 quad of `crop` becomes one pixel: the red and blue samples as
/// they are, the two greens averaged, all through the black/white scale.
pub fn superpixel(
    decoded: &Decoded,
    cfa: Cfa,
    crop: Crop,
    black: u16,
    white: u16,
    threads: usize,
) -> Result<Level1, Error> {
    // Checked: `Decoded`'s fields are public, so the raw ceilings `nef`
    // applies to a parsed frame are applied here again.
    if !axis_ok(decoded.width)
        || !axis_ok(decoded.height)
        || decoded
            .width
            .checked_mul(decoded.height)
            .is_none_or(|n| n != decoded.samples.len() || n > MAX_RAW_SAMPLES)
    {
        return Err(Error::Size);
    }
    if !crop.fits(decoded.width, decoded.height) {
        return Err(Error::Crop);
    }
    if white <= black {
        return Err(Error::Levels);
    }
    let range = u32::from(white - black);
    let scale: Vec<u16> = (0..=u16::MAX)
        .map(|s| {
            let v = u32::from(s.saturating_sub(black)) * 65535 / range;
            v.min(65535) as u16
        })
        .collect();
    let out_w = crop.width / 2;
    let out_h = crop.height / 2;
    let mut rgb = vec![0u16; out_w * out_h * 3];
    let threads = thread_count(threads, out_h);
    let band = band_rows(out_h, threads);
    let width = decoded.width;
    let samples = &decoded.samples;
    let scale = &scale;
    let items: Vec<(usize, &mut [u16])> = rgb.chunks_mut(band * out_w * 3).enumerate().collect();
    bands(items, threads, |(band_index, chunk)| {
        let first_row = band_index * band;
        for (r, out_row) in chunk.chunks_exact_mut(out_w * 3).enumerate() {
            let y0 = crop.top + 2 * (first_row + r);
            let y1 = y0 + 1;
            let start0 = y0 * width + crop.left;
            let start1 = y1 * width + crop.left;
            let (Some(row0), Some(row1)) = (
                samples.get(start0..start0 + crop.width),
                samples.get(start1..start1 + crop.width),
            ) else {
                continue;
            };
            let channels = [
                cfa.at(crop.left, y0),
                cfa.at(crop.left + 1, y0),
                cfa.at(crop.left, y1),
                cfa.at(crop.left + 1, y1),
            ];
            for ((out_px, &[t0, t1]), &[b0, b1]) in out_row
                .as_chunks_mut::<3>()
                .0
                .iter_mut()
                .zip(row0.as_chunks::<2>().0.iter())
                .zip(row1.as_chunks::<2>().0.iter())
            {
                let mut red = 0u32;
                let mut green = 0u32;
                let mut blue = 0u32;
                for (channel, value) in channels.iter().zip([t0, t1, b0, b1]) {
                    match channel {
                        Channel::Red => red = u32::from(value),
                        Channel::Green => green += u32::from(value),
                        Channel::Blue => blue = u32::from(value),
                    }
                }
                let look = |v: u32| scale.get(v as usize).copied().unwrap_or(u16::MAX);
                *out_px = [look(red), look(green / 2), look(blue)];
            }
        }
    });
    Ok(Level1 {
        width: out_w,
        height: out_h,
        rgb,
    })
}

/// One destination index's source contributions along one axis.
#[derive(Clone, Debug, PartialEq)]
struct Span {
    first: usize,
    weights: Vec<f32>,
}

/// Area averaging when reducing, bilinear when enlarging.
fn spans(src: usize, dst: usize) -> Vec<Span> {
    let scale = src as f32 / dst as f32;
    (0..dst)
        .map(|d| {
            if scale >= 1.0 {
                let start = d as f32 * scale;
                let end = (start + scale).min(src as f32);
                let first = (start.floor() as usize).min(src.saturating_sub(1));
                let last = (end.ceil() as usize).min(src).max(first + 1);
                let weights = (first..last)
                    .map(|s| {
                        let lo = (s as f32).max(start);
                        let hi = ((s + 1) as f32).min(end);
                        ((hi - lo) / scale).max(0.0)
                    })
                    .collect();
                Span { first, weights }
            } else {
                let center = ((d as f32 + 0.5) * scale - 0.5).max(0.0);
                let i0 = (center.floor() as usize).min(src.saturating_sub(1));
                let i1 = (i0 + 1).min(src.saturating_sub(1));
                let t = center - i0 as f32;
                if i0 == i1 {
                    Span {
                        first: i0,
                        weights: vec![1.0],
                    }
                } else {
                    Span {
                        first: i0,
                        weights: vec![1.0 - t, t],
                    }
                }
            }
        })
        .collect()
}

fn axis_ok(n: usize) -> bool {
    n != 0 && n <= MAX_AXIS
}

/// Both axes within `MAX_AXIS` (so the product cannot overflow) and the
/// pixel count within `MAX_IMAGE_PIXELS`.
fn pixels_ok(width: usize, height: usize) -> bool {
    axis_ok(width) && axis_ok(height) && width * height <= MAX_IMAGE_PIXELS
}

/// Resamples interleaved three-channel `f32` rows from `sw`x`sh` to
/// `dw`x`dh`, horizontal pass then vertical, rows split across threads.
/// Every axis is nonzero and at most `image::MAX_AXIS`.
pub fn resample(
    src: &[f32],
    sw: usize,
    sh: usize,
    dw: usize,
    dh: usize,
    threads: usize,
) -> Result<Vec<f32>, Error> {
    // Source, destination and the middle (destination width by source
    // height) buffers all stay under the pixel budget.
    if !pixels_ok(sw, sh) || !pixels_ok(dw, dh) || !pixels_ok(dw, sh) || src.len() != sw * sh * 3 {
        return Err(Error::Size);
    }
    let xs = spans(sw, dw);
    let ys = spans(sh, dh);
    let mut middle = vec![0.0f32; dw * sh * 3];
    let threads_h = thread_count(threads, sh);
    let band_h = band_rows(sh, threads_h);
    let xs_ref = &xs;
    let items: Vec<(usize, &mut [f32])> = middle.chunks_mut(band_h * dw * 3).enumerate().collect();
    bands(items, threads_h, |(band_index, chunk)| {
        for (r, out_row) in chunk.chunks_exact_mut(dw * 3).enumerate() {
            let y = band_index * band_h + r;
            let Some(in_row) = src.get(y * sw * 3..(y + 1) * sw * 3) else {
                continue;
            };
            let in_px = in_row.as_chunks::<3>().0;
            for (out_px, span) in out_row.as_chunks_mut::<3>().0.iter_mut().zip(xs_ref.iter()) {
                let mut acc = [0.0f32; 3];
                for (k, w) in span.weights.iter().enumerate() {
                    if let Some([r, g, b]) = in_px.get(span.first + k) {
                        acc[0] += r * w;
                        acc[1] += g * w;
                        acc[2] += b * w;
                    }
                }
                *out_px = acc;
            }
        }
    });
    let mut out = vec![0.0f32; dw * dh * 3];
    let threads_v = thread_count(threads, dh);
    let band_v = band_rows(dh, threads_v);
    let middle_ref = &middle;
    let ys_ref = &ys;
    let items: Vec<(usize, &mut [f32])> = out.chunks_mut(band_v * dw * 3).enumerate().collect();
    bands(items, threads_v, |(band_index, chunk)| {
        for (r, out_row) in chunk.chunks_exact_mut(dw * 3).enumerate() {
            let y = band_index * band_v + r;
            let Some(span) = ys_ref.get(y) else {
                continue;
            };
            for (k, w) in span.weights.iter().enumerate() {
                let sy = span.first + k;
                let Some(in_row) = middle_ref.get(sy * dw * 3..(sy + 1) * dw * 3) else {
                    continue;
                };
                for (slot, value) in out_row.iter_mut().zip(in_row.iter()) {
                    *slot += value * w;
                }
            }
        }
    });
    Ok(out)
}

/// What the pipeline takes beyond the camera's facts.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Params {
    /// Exposure in stops.
    pub exposure: f32,
    pub threads: usize,
}

/// The axes that fit `long_edge` without enlarging, keeping aspect.
pub fn fit(width: usize, height: usize, long_edge: usize) -> (usize, usize) {
    // Clamped to the axis ceiling so the products below cannot overflow
    // for any caller.
    let width = width.clamp(1, MAX_AXIS);
    let height = height.clamp(1, MAX_AXIS);
    let long = width.max(height);
    let target = long_edge.clamp(1, long);
    let short = |s: usize| ((s * target + long / 2) / long).max(1);
    if width >= height {
        (target, short(height))
    } else {
        (short(width), target)
    }
}

/// Develops level 1 to 8-bit sRGB at most `long_edge` on its long side:
/// resample, then per pixel white balance and clip at the camera white,
/// exposure, the camera matrix, the transfer; then orientation.
pub fn render(
    level1: &Level1,
    long_edge: usize,
    orientation: u16,
    wb: [f32; 3],
    color: &CameraColor,
    transfer: &Transfer,
    params: &Params,
) -> Result<Rgb8, Error> {
    let (w, h) = (level1.width, level1.height);
    if !pixels_ok(w, h) || level1.rgb.len() != w * h * 3 {
        return Err(Error::Size);
    }
    let (dw, dh) = fit(w, h, long_edge);
    let linear: Vec<f32> = level1.rgb.iter().map(|v| f32::from(*v) / 65535.0).collect();
    let small = resample(&linear, w, h, dw, dh, params.threads)?;
    let gain = 2.0f32.powf(params.exposure);
    let mut matrix: Matrix = color.rgb_cam;
    for row in matrix.iter_mut() {
        for cell in row.iter_mut() {
            *cell *= gain;
        }
    }
    let mut out = vec![0u8; dw * dh * 3];
    let threads = thread_count(params.threads, dh);
    let band = band_rows(dh, threads);
    let small_ref = &small;
    let matrix_ref = &matrix;
    let items: Vec<(usize, &mut [u8])> = out.chunks_mut(band * dw * 3).enumerate().collect();
    bands(items, threads, |(band_index, chunk)| {
        let start = band_index * band * dw * 3;
        let Some(input) = small_ref.get(start..start + chunk.len()) else {
            return;
        };
        for (out_px, &[r, g, b]) in chunk
            .as_chunks_mut::<3>()
            .0
            .iter_mut()
            .zip(input.as_chunks::<3>().0.iter())
        {
            let cam = [
                (r * wb[0]).min(1.0),
                (g * wb[1]).min(1.0),
                (b * wb[2]).min(1.0),
            ];
            *out_px = apply(matrix_ref, cam).map(|v| transfer.encode(v));
        }
    });
    Ok(orient(
        Rgb8 {
            width: dw,
            height: dh,
            data: out,
        },
        orientation,
    ))
}

/// Applies a TIFF orientation: 3 turns the image half way round, 6 a
/// quarter turn clockwise, 8 a quarter turn anticlockwise; anything else,
/// and an inconsistent buffer, is returned as it came.
pub fn orient(image: Rgb8, orientation: u16) -> Rgb8 {
    if !matches!(orientation, 3 | 6 | 8) || !image.is_consistent() {
        return image;
    }
    let (w, h) = (image.width, image.height);
    let (ow, oh) = if orientation == 3 { (w, h) } else { (h, w) };
    let mut data = vec![0u8; image.data.len()];
    for (oy, out_row) in data.chunks_exact_mut(ow * 3).enumerate() {
        for (ox, out_px) in out_row.as_chunks_mut::<3>().0.iter_mut().enumerate() {
            let (x, y) = match orientation {
                3 => (w - 1 - ox, h - 1 - oy),
                6 => (oy, h - 1 - ox),
                _ => (w - 1 - oy, ox),
            };
            if let Some(px) = image.pixel(x, y) {
                *out_px = px;
            }
        }
    }
    Rgb8 {
        width: ow,
        height: oh,
        data,
    }
}

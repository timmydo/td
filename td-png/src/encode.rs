//! PNG encoder: eight-bit RGB, not interlaced, each row under the filter
//! whose bytes sum smallest as signed values (libpng's heuristic), the
//! filtered rows in one zlib stream of td-util's DEFLATE encoder. The
//! output depends only on the pixels. Nothing here reads a file, the
//! environment or a clock.
//!
//! The DEFLATE encoder and the CRC are the crate root's `deflate` and
//! `crc32` mounts, so a consumer that mounts only this file beside them
//! carries no decoder.

/// The image data per `IDAT` chunk.
const IDAT_BYTES: usize = 1 << 20;
/// The largest axis PNG can name (2^31 - 1).
const PNG_AXIS: usize = (1 << 31) - 1;
/// td-util's levels trade chain length; screenshots and other flat
/// pictures match within the shortest chains.
const LEVEL: u8 = 1;

/// What `encode_rgb8` refuses, by name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EncodeError {
    /// An axis of zero or past PNG's 2^31 - 1, or a frame whose filtered
    /// rows do not fit in memory's addressable size.
    Axis { width: usize, height: usize },
    /// Pixels other than three bytes for each of `width * height`.
    Length { expected: usize, actual: usize },
}

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Axis { width, height } => write!(f, "cannot encode a {width}x{height} PNG"),
            Self::Length { expected, actual } => {
                write!(f, "expected {expected} RGB bytes, got {actual}")
            }
        }
    }
}

impl std::error::Error for EncodeError {}

/// `rgb`, rows top to bottom with no padding, three bytes a pixel, as a
/// complete PNG stream.
pub fn encode_rgb8(width: usize, height: usize, rgb: &[u8]) -> Result<Vec<u8>, EncodeError> {
    let axis = || EncodeError::Axis { width, height };
    if width == 0 || height == 0 || width > PNG_AXIS || height > PNG_AXIS {
        return Err(axis());
    }
    let row = width.checked_mul(3).ok_or_else(axis)?;
    let expected = row.checked_mul(height).ok_or_else(axis)?;
    let filtered = row
        .checked_add(1)
        .and_then(|r| r.checked_mul(height))
        .ok_or_else(axis)?;
    if rgb.len() != expected {
        return Err(EncodeError::Length {
            expected,
            actual: rgb.len(),
        });
    }

    let mut raw = Vec::with_capacity(filtered);
    let mut candidate = vec![0u8; row];
    let mut best = vec![0u8; row];
    let zero = vec![0u8; row];
    let mut above: &[u8] = &zero;
    for line in rgb.chunks_exact(row) {
        let mut best_kind = 0u8;
        let mut best_cost = u64::MAX;
        for kind in 0..5u8 {
            filter(kind, line, above, &mut candidate);
            let cost: u64 = candidate
                .iter()
                .map(|&b| u64::from((b as i8).unsigned_abs()))
                .sum();
            if cost < best_cost {
                best_cost = cost;
                best_kind = kind;
                std::mem::swap(&mut best, &mut candidate);
            }
        }
        raw.push(best_kind);
        raw.extend_from_slice(&best);
        above = line;
    }

    let mut zlib = Vec::with_capacity(raw.len() / 2 + 16);
    // DEFLATE with a 32 KiB window, the fastest-level hint, no dictionary:
    // 0x7801 is a multiple of 31.
    zlib.extend_from_slice(&[0x78, 0x01]);
    zlib.extend_from_slice(&crate::deflate::compress(&raw, LEVEL));
    zlib.extend_from_slice(&adler32(&raw).to_be_bytes());
    drop(raw);

    let mut out = Vec::with_capacity(zlib.len() + 64);
    out.extend_from_slice(b"\x89PNG\r\n\x1a\n");
    let mut header = Vec::with_capacity(13);
    // Both axes are at most 2^31 - 1, checked above.
    header.extend_from_slice(&(width as u32).to_be_bytes());
    header.extend_from_slice(&(height as u32).to_be_bytes());
    // Depth 8, colour type 2 (RGB), compression, filter and interlace 0.
    header.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &header);
    for data in zlib.chunks(IDAT_BYTES) {
        chunk(&mut out, b"IDAT", data);
    }
    chunk(&mut out, b"IEND", &[]);
    Ok(out)
}

/// `line` under filter `kind` against the row `above` it, into `out`; all
/// three are one row long.
pub(crate) fn filter(kind: u8, line: &[u8], above: &[u8], out: &mut [u8]) {
    for (i, (slot, (&x, &b))) in out.iter_mut().zip(line.iter().zip(above)).enumerate() {
        let a = i
            .checked_sub(3)
            .and_then(|j| line.get(j))
            .copied()
            .unwrap_or(0);
        let c = i
            .checked_sub(3)
            .and_then(|j| above.get(j))
            .copied()
            .unwrap_or(0);
        *slot = x.wrapping_sub(match kind {
            0 => 0,
            1 => a,
            2 => b,
            3 => ((u16::from(a) + u16::from(b)) / 2) as u8,
            _ => paeth(a, b, c),
        });
    }
}

/// Paeth's predictor (PNG §9.4), the decoder's too.
pub(crate) fn paeth(a: u8, b: u8, c: u8) -> u8 {
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

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    // A chunk is at most IDAT_BYTES or a 13-byte header.
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut sum = crate::crc32::Crc32::new();
    sum.update(kind);
    sum.update(data);
    out.extend_from_slice(&sum.finalize().to_be_bytes());
}

/// Adler-32 (RFC 1950 §9), summed in runs short enough that neither sum
/// can overflow before its reduction; the decoder's too.
pub(crate) fn adler32(data: &[u8]) -> u32 {
    const MOD: u32 = 65521;
    // The most bytes summed before s2 could pass u32 (zlib's NMAX).
    const RUN: usize = 5552;
    let (mut s1, mut s2) = (1u32, 0u32);
    for run in data.chunks(RUN) {
        for &b in run {
            s1 += u32::from(b);
            s2 += s1;
        }
        s1 %= MOD;
        s2 %= MOD;
    }
    (s2 << 16) | s1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adler32_matches_the_rfc_1950_value() {
        assert_eq!(adler32(b""), 1);
        assert_eq!(adler32(b"Wikipedia"), 0x11e6_0398);
        let long = vec![0xffu8; 100_000];
        // s1 and s2 reduced the slow way, a byte at a time.
        let (mut s1, mut s2) = (1u64, 0u64);
        for &b in &long {
            s1 = (s1 + u64::from(b)) % 65521;
            s2 = (s2 + s1) % 65521;
        }
        assert_eq!(u64::from(adler32(&long)), (s2 << 16) | s1);
    }

    #[test]
    fn paeth_breaks_ties_as_the_specification_orders_them() {
        assert_eq!(paeth(10, 10, 10), 10);
        assert_eq!(paeth(1, 2, 3), 1);
        assert_eq!(paeth(3, 2, 1), 3);
        assert_eq!(paeth(0, 9, 0), 9);
        assert_eq!(paeth(9, 0, 0), 9);
        // The two ties whose order shows: a over c, then b over c.
        assert_eq!(paeth(6, 0, 2), 6);
        assert_eq!(paeth(0, 6, 2), 6);
    }

    #[test]
    fn refusals_are_named() {
        assert_eq!(
            encode_rgb8(0, 1, &[]),
            Err(EncodeError::Axis {
                width: 0,
                height: 1
            })
        );
        assert_eq!(
            encode_rgb8(1, 0, &[]),
            Err(EncodeError::Axis {
                width: 1,
                height: 0
            })
        );
        assert_eq!(
            encode_rgb8(PNG_AXIS + 1, 1, &[]),
            Err(EncodeError::Axis {
                width: PNG_AXIS + 1,
                height: 1
            })
        );
        assert_eq!(
            encode_rgb8(2, 2, &[0; 11]),
            Err(EncodeError::Length {
                expected: 12,
                actual: 11
            })
        );
    }
}

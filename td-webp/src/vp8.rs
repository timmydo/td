//! VP8 key-frame decoding, as specified by RFC 6386.
use super::vp8_tables::{AC_Q, BMODE_PROBS, COEFF_PROBS, DC_Q, UPDATE_PROBS};
use crate::{Error, Header, Rgba8};

fn at<T: Copy>(s: &[T], i: usize) -> Result<T, Error> {
    s.get(i).copied().ok_or(Error::Bitstream)
}
fn put<T>(s: &mut [T], i: usize, v: T) -> Result<(), Error> {
    *s.get_mut(i).ok_or(Error::Bitstream)? = v;
    Ok(())
}
fn zeros<T: Clone>(n: usize, v: T) -> Result<Vec<T>, Error> {
    let mut out = Vec::new();
    out.try_reserve_exact(n).map_err(|_| Error::Limit)?;
    out.resize(n, v);
    Ok(out)
}

struct Bool<'a> {
    bytes: &'a [u8],
    pos: usize,
    value: u32,
    range: u32,
    shifts: u32,
    initialized: bool,
    work: usize,
    limit: usize,
}
impl<'a> Bool<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            pos: 2,
            value: 0,
            range: 255,
            shifts: 0,
            initialized: false,
            work: 0,
            limit: crate::MAX_WORK,
        }
    }
    fn bit(&mut self, probability: u8) -> Result<bool, Error> {
        if !self.initialized {
            self.value = (u32::from(at(self.bytes, 0)?) << 8) | u32::from(at(self.bytes, 1)?);
            self.initialized = true;
        }
        if self.work >= self.limit {
            return Err(Error::Limit);
        }
        self.work += 1;
        let split = 1 + (((self.range - 1) * u32::from(probability)) >> 8);
        let one = self.value >= split << 8;
        if one {
            self.value -= split << 8;
            self.range -= split;
        } else {
            self.range = split;
        }
        while self.range < 128 {
            self.range <<= 1;
            self.value <<= 1;
            self.shifts += 1;
            if self.shifts == 8 {
                self.shifts = 0;
                self.value |= u32::from(at(self.bytes, self.pos)?);
                self.pos += 1;
            }
        }
        Ok(one)
    }
    fn literal(&mut self, n: usize) -> Result<i32, Error> {
        let mut value = 0;
        for _ in 0..n {
            value = (value << 1) | i32::from(self.bit(128)?);
        }
        Ok(value)
    }
    fn signed(&mut self, n: usize) -> Result<i32, Error> {
        let v = self.literal(n)?;
        Ok(if self.bit(128)? { -v } else { v })
    }
    fn optional(&mut self, n: usize) -> Result<i32, Error> {
        if self.bit(128)? {
            self.signed(n)
        } else {
            Ok(0)
        }
    }
    fn tree(&mut self, tree: &[i8], probs: &[u8], start: usize) -> Result<usize, Error> {
        let mut p = start;
        for _ in 0..12 {
            let branch = usize::from(self.bit(at(probs, p / 2)?)?);
            let next = at(tree, p + branch)?;
            if next <= 0 {
                return Ok((-i32::from(next)) as usize);
            }
            p = next as usize;
        }
        Err(Error::Bitstream)
    }
}
struct Params {
    segment_map: bool,
    segment_absolute: bool,
    segment_quant: [i32; 4],
    segment_filter: [i32; 4],
    segment_probs: [u8; 3],
    segmented: bool,
    simple: bool,
    filter: i32,
    sharpness: i32,
    ref_delta: i32,
    mode_delta: i32,
    quant: i32,
    deltas: [i32; 5],
    probs: [u8; 1056],
    skip: Option<u8>,
    partitions: usize,
}
impl Params {
    fn read(bits: &mut Bool<'_>) -> Result<Self, Error> {
        if bits.bit(128)? {
            return Err(Error::Unsupported);
        }
        let _clamp = bits.bit(128)?;
        let mut p = Self {
            segment_map: false,
            segment_absolute: false,
            segment_quant: [0; 4],
            segment_filter: [0; 4],
            segment_probs: [255; 3],
            segmented: bits.bit(128)?,
            simple: false,
            filter: 0,
            sharpness: 0,
            ref_delta: 0,
            mode_delta: 0,
            quant: 0,
            deltas: [0; 5],
            probs: COEFF_PROBS,
            skip: None,
            partitions: 1,
        };
        if p.segmented {
            p.segment_map = bits.bit(128)?;
            if bits.bit(128)? {
                p.segment_absolute = bits.bit(128)?;
                for q in &mut p.segment_quant {
                    *q = bits.optional(7)?;
                }
                for f in &mut p.segment_filter {
                    *f = bits.optional(6)?;
                }
            }
            if p.segment_map {
                for prob in &mut p.segment_probs {
                    if bits.bit(128)? {
                        *prob = bits.literal(8)? as u8;
                    }
                }
            }
        }
        p.simple = bits.bit(128)?;
        p.filter = bits.literal(6)?;
        p.sharpness = bits.literal(3)?;
        if bits.bit(128)? && bits.bit(128)? {
            for i in 0..4 {
                let d = bits.optional(6)?;
                if i == 0 {
                    p.ref_delta = d;
                }
            }
            for i in 0..4 {
                let d = bits.optional(6)?;
                if i == 0 {
                    p.mode_delta = d;
                }
            }
        }
        p.partitions = 1 << bits.literal(2)?;
        p.quant = bits.literal(7)?;
        for delta in &mut p.deltas {
            *delta = bits.optional(4)?;
        }
        let _refresh_entropy = bits.bit(128)?;
        for (prob, update) in p.probs.iter_mut().zip(UPDATE_PROBS) {
            if bits.bit(update)? {
                *prob = bits.literal(8)? as u8;
            }
        }
        if bits.bit(128)? {
            p.skip = Some(bits.literal(8)? as u8);
        }
        Ok(p)
    }
    fn quantizers(&self, segment: usize) -> Result<[[i32; 2]; 3], Error> {
        let mut q = self.quant;
        if self.segmented {
            q = at(&self.segment_quant, segment)? + if self.segment_absolute { 0 } else { q };
        }
        q = q.clamp(0, 127);
        let dc = |delta: i32| at(&DC_Q, (q + delta).clamp(0, 127) as usize);
        let ac = |delta: i32| at(&AC_Q, (q + delta).clamp(0, 127) as usize);
        Ok([
            [dc(at(&self.deltas, 0)?)?, ac(0)?],
            [
                2 * dc(at(&self.deltas, 1)?)?,
                (ac(at(&self.deltas, 2)?)? * 155 / 100).max(8),
            ],
            [
                dc(at(&self.deltas, 3)?)?.min(132),
                ac(at(&self.deltas, 4)?)?,
            ],
        ])
    }
    fn strength(&self, segment: usize, small: bool) -> Result<i32, Error> {
        let mut f = self.filter;
        if self.segmented {
            f = at(&self.segment_filter, segment)? + if self.segment_absolute { 0 } else { f };
        }
        Ok(
            (f.clamp(0, 63) + self.ref_delta + if small { self.mode_delta } else { 0 })
                .clamp(0, 63),
        )
    }
}
#[derive(Clone)]
struct Macroblock {
    segment: usize,
    skip: bool,
    y: usize,
    uv: usize,
    modes: [usize; 16],
    nonzero: bool,
}
fn modes(
    bits: &mut Bool<'_>,
    params: &Params,
    mw: usize,
    mh: usize,
) -> Result<Vec<Macroblock>, Error> {
    let mut above = zeros(mw * 4, 0usize)?;
    let mut output = Vec::new();
    output
        .try_reserve_exact(mw * mh)
        .map_err(|_| Error::Limit)?;
    for _y in 0..mh {
        let mut left = [0usize; 4];
        for x in 0..mw {
            let segment = if params.segment_map {
                bits.tree(&[2, 4, 0, -1, -2, -3], &params.segment_probs, 0)?
            } else {
                0
            };
            let skip = if let Some(prob) = params.skip {
                bits.bit(prob)?
            } else {
                false
            };
            let y = bits.tree(&[-4, 2, 4, 6, 0, -1, -2, -3], &[145, 156, 163, 128], 0)?;
            let mut modes = [0; 16];
            for by in 0..4 {
                for bx in 0..4 {
                    let mode = if y == 4 {
                        let context = (at(&above, x * 4 + bx)? * 10 + at(&left, by)?) * 9;
                        bits.tree(
                            &[
                                0, 2, -1, 4, -2, 6, 8, 12, -3, 10, -5, -6, -4, 14, -7, 16, -8, -9,
                            ],
                            BMODE_PROBS
                                .get(context..context + 9)
                                .ok_or(Error::Bitstream)?,
                            0,
                        )?
                    } else {
                        at(&[0, 2, 3, 1], y)?
                    };
                    put(&mut modes, by * 4 + bx, mode)?;
                    put(&mut above, x * 4 + bx, mode)?;
                    put(&mut left, by, mode)?;
                }
            }
            let uv = bits.tree(&[0, 2, -1, 4, -2, -3], &[142, 114, 183], 0)?;
            output.push(Macroblock {
                segment,
                skip,
                y,
                uv,
                modes,
                nonzero: false,
            });
        }
    }
    Ok(output)
}
fn coefficient_block(
    bits: &mut Bool<'_>,
    probs: &[u8],
    plane: usize,
    context: usize,
    quant: [i32; 2],
) -> Result<([i32; 16], bool), Error> {
    const TREE: [i8; 22] = [
        -11, 2, 0, 4, -1, 6, 8, 12, -2, 10, -3, -4, 14, 16, -5, -6, 18, 20, -7, -8, -9, -10,
    ];
    const BANDS: [usize; 16] = [0, 1, 2, 3, 6, 4, 5, 6, 6, 6, 6, 6, 6, 6, 6, 7];
    const ZIG: [usize; 16] = [0, 1, 4, 8, 5, 2, 3, 6, 9, 12, 13, 10, 7, 11, 14, 15];
    const CATS: [&[u8]; 6] = [
        &[159],
        &[165, 145],
        &[173, 148, 140],
        &[176, 155, 140, 135],
        &[180, 157, 141, 134, 130],
        &[254, 254, 243, 230, 196, 177, 153, 140, 133, 130, 129],
    ];
    let mut out = [0i32; 16];
    let mut context = context;
    let mut zero = false;
    let mut nonzero = false;
    for pos in usize::from(plane == 0)..16 {
        let atp = ((plane * 8 + at(&BANDS, pos)?) * 3 + context) * 11;
        let p = probs.get(atp..atp + 11).ok_or(Error::Bitstream)?;
        let token = bits.tree(&TREE, p, if zero { 2 } else { 0 })?;
        if token == 11 {
            break;
        }
        let mut value = token as i32;
        if token >= 5 {
            let cat = token - 5;
            let mut extra = 0;
            for &prob in at(&CATS, cat)? {
                extra = (extra << 1) | i32::from(bits.bit(prob)?);
            }
            value = at(&[5, 7, 11, 19, 35, 67], cat)? + extra;
        }
        zero = value == 0;
        nonzero |= !zero;
        context = value.min(2) as usize;
        if !zero && bits.bit(128)? {
            value = -value;
        }
        let dequant = value * at(&quant, usize::from(pos != 0))?;
        put(&mut out, at(&ZIG, pos)?, i32::from(dequant as i16))?;
    }
    Ok((out, nonzero))
}
fn transform(input: [i32; 16], walsh: bool) -> Result<[i32; 16], Error> {
    fn one([a, b, c, d]: [i32; 4], walsh: bool) -> [i32; 4] {
        if walsh {
            [a + d + b + c, b - c + a - d, a + d - b - c, a - d - b + c]
        } else {
            let p = ((b * 35468) >> 16) - d - ((d * 20091) >> 16);
            let q = b + ((b * 20091) >> 16) + ((d * 35468) >> 16);
            [a + c + q, a - c + p, a - c - p, a + c - q]
        }
    }
    let mut tmp = [0; 16];
    let mut out = [0; 16];
    for x in 0..4 {
        let v = one(
            [
                at(&input, x)?,
                at(&input, x + 4)?,
                at(&input, x + 8)?,
                at(&input, x + 12)?,
            ],
            walsh,
        );
        for (y, p) in v.into_iter().enumerate() {
            put(&mut tmp, y * 4 + x, i32::from(p as i16))?;
        }
    }
    for y in 0..4 {
        let pos = y * 4;
        let v = one(
            [
                at(&tmp, pos)?,
                at(&tmp, pos + 1)?,
                at(&tmp, pos + 2)?,
                at(&tmp, pos + 3)?,
            ],
            walsh,
        );
        for (x, p) in v.into_iter().enumerate() {
            put(&mut out, pos + x, (p + if walsh { 3 } else { 4 }) >> 3)?;
        }
    }
    Ok(out)
}
struct Plane {
    width: usize,
    height: usize,
    data: Vec<u8>,
}
impl Plane {
    fn new(width: usize, height: usize) -> Result<Self, Error> {
        Ok(Self {
            width,
            height,
            data: zeros(width * height, 0)?,
        })
    }
    fn get(&self, x: isize, y: isize) -> Result<i32, Error> {
        if y < 0 {
            return Ok(127);
        }
        if x < 0 {
            return Ok(129);
        }
        if y as usize >= self.height {
            return Err(Error::Bitstream);
        }
        Ok(i32::from(at(
            &self.data,
            y as usize * self.width + (x as usize).min(self.width - 1),
        )?))
    }
    fn set(&mut self, x: usize, y: usize, v: i32) -> Result<(), Error> {
        put(&mut self.data, y * self.width + x, v.clamp(0, 255) as u8)
    }
    fn predict(&mut self, x: usize, y: usize, size: usize, mode: usize) -> Result<(), Error> {
        let mut sum = 0;
        let mut n = 0;
        if x != 0 {
            for i in 0..size {
                sum += self.get(x as isize - 1, (y + i) as isize)?;
            }
            n += size as i32;
        }
        if y != 0 {
            for i in 0..size {
                sum += self.get((x + i) as isize, y as isize - 1)?;
            }
            n += size as i32;
        }
        let dc = if n == 0 { 128 } else { (sum + n / 2) / n };
        for by in 0..size {
            for bx in 0..size {
                let top = self.get((x + bx) as isize, y as isize - 1)?;
                let left = self.get(x as isize - 1, (y + by) as isize)?;
                let v = match mode {
                    0 => dc,
                    1 => top,
                    2 => left,
                    3 => top + left - self.get(x as isize - 1, y as isize - 1)?,
                    _ => return Err(Error::Bitstream),
                };
                self.set(x + bx, y + by, v)?;
            }
        }
        Ok(())
    }
    fn add(&mut self, x: usize, y: usize, coeffs: [i32; 16]) -> Result<(), Error> {
        let residual = transform(coeffs, false)?;
        for (i, p) in residual.into_iter().enumerate() {
            let x = x + i % 4;
            let y = y + i / 4;
            self.set(x, y, self.get(x as isize, y as isize)? + p)?;
        }
        Ok(())
    }
}

fn avg2(a: i32, b: i32) -> i32 {
    (a + b + 1) >> 1
}
fn avg3(a: i32, b: i32, c: i32) -> i32 {
    (a + 2 * b + c + 2) >> 2
}
fn corner_prediction(edge: &[i32; 9], x: usize, y: usize) -> Result<i32, Error> {
    let d = 2 * x as i32 - y as i32;
    if d >= 0 && d % 2 == 0 {
        let k = 4 + d as usize / 2;
        Ok(avg2(at(edge, k)?, at(edge, k + 1)?))
    } else {
        let k = if d < 0 {
            (5 + d) as usize
        } else {
            5 + d as usize / 2
        };
        Ok(avg3(at(edge, k - 1)?, at(edge, k)?, at(edge, k + 1)?))
    }
}
impl Plane {
    fn small_prediction(&mut self, x: usize, y: usize, mode: usize) -> Result<(), Error> {
        let mut top = [0i32; 8];
        let mut left = [0i32; 4];
        for (i, v) in top.iter_mut().enumerate() {
            let py = if i >= 4 && x % 16 == 12 {
                y / 16 * 16
            } else {
                y
            };
            *v = self.get((x + i) as isize, py as isize - 1)?;
        }
        for (i, v) in left.iter_mut().enumerate() {
            *v = self.get(x as isize - 1, (y + i) as isize)?;
        }
        let corner = self.get(x as isize - 1, y as isize - 1)?;
        let mut edge = [corner; 9];
        let mut transpose = [corner; 9];
        for i in 0..4 {
            put(&mut edge, 3 - i, at(&left, i)?)?;
            put(&mut edge, 5 + i, at(&top, i)?)?;
            put(&mut transpose, 3 - i, at(&top, i)?)?;
            put(&mut transpose, 5 + i, at(&left, i)?)?;
        }
        let dc = (top.iter().take(4).sum::<i32>() + left.iter().sum::<i32>() + 4) >> 3;
        for by in 0..4 {
            for bx in 0..4 {
                let v = match mode {
                    0 => dc,
                    1 => at(&top, bx)? + at(&left, by)? - corner,
                    2 => avg3(
                        if bx == 0 { corner } else { at(&top, bx - 1)? },
                        at(&top, bx)?,
                        at(&top, bx + 1)?,
                    ),
                    3 => avg3(
                        if by == 0 { corner } else { at(&left, by - 1)? },
                        at(&left, by)?,
                        at(&left, (by + 1).min(3))?,
                    ),
                    4 => {
                        let k = bx + by;
                        avg3(at(&top, k)?, at(&top, k + 1)?, at(&top, (k + 2).min(7))?)
                    }
                    5 => {
                        let k = 4 + bx - by;
                        avg3(at(&edge, k - 1)?, at(&edge, k)?, at(&edge, k + 1)?)
                    }
                    6 => corner_prediction(&edge, bx, by)?,
                    7 => {
                        let d = 2 * bx + by;
                        if d >= 8 {
                            avg3(at(&top, d - 4)?, at(&top, d - 3)?, at(&top, d - 2)?)
                        } else if d % 2 == 0 {
                            avg2(at(&top, d / 2)?, at(&top, d / 2 + 1)?)
                        } else {
                            avg3(at(&top, d / 2)?, at(&top, d / 2 + 1)?, at(&top, d / 2 + 2)?)
                        }
                    }
                    8 => corner_prediction(&transpose, by, bx)?,
                    9 => {
                        let d = 2 * by + bx;
                        let a = at(&left, (d / 2).min(3))?;
                        let b = at(&left, (d / 2 + 1).min(3))?;
                        if d % 2 == 0 {
                            avg2(a, b)
                        } else {
                            avg3(a, b, at(&left, (d / 2 + 2).min(3))?)
                        }
                    }
                    _ => return Err(Error::Bitstream),
                };
                self.set(x + bx, y + by, v)?;
            }
        }
        Ok(())
    }
}
fn coefficients(
    bits: &mut Bool<'_>,
    params: &Params,
    mb: &mut Macroblock,
    above: &mut [u8; 9],
    left: &mut [u8; 9],
) -> Result<[[i32; 16]; 24], Error> {
    let mut blocks = [[0i32; 16]; 24];
    if mb.skip {
        above.get_mut(..8).ok_or(Error::Bitstream)?.fill(0);
        left.get_mut(..8).ok_or(Error::Bitstream)?.fill(0);
        if mb.y != 4 {
            put(above, 8, 0)?;
            put(left, 8, 0)?;
        }
        return Ok(blocks);
    }
    let q = params.quantizers(mb.segment)?;
    if mb.y != 4 {
        let context = usize::from(at(above, 8)? + at(left, 8)?);
        let (coeffs, nz) = coefficient_block(bits, &params.probs, 1, context, at(&q, 1)?)?;
        mb.nonzero |= nz;
        put(above, 8, u8::from(nz))?;
        put(left, 8, u8::from(nz))?;
        let dc = transform(coeffs, true)?;
        for (block, value) in blocks.iter_mut().take(16).zip(dc) {
            put(block, 0, value)?;
        }
    }
    for (i, block) in blocks.iter_mut().enumerate() {
        let (plane, ax, ly, qi) = if i < 16 {
            (if mb.y == 4 { 3 } else { 0 }, i % 4, i / 4, 0)
        } else {
            let base = 4 + (i - 16) / 4 * 2;
            (2, base + i % 2, base + (i % 4) / 2, 2)
        };
        let context = usize::from(at(above, ax)? + at(left, ly)?);
        let (mut coeffs, nz) = coefficient_block(bits, &params.probs, plane, context, at(&q, qi)?)?;
        if plane == 0 {
            put(&mut coeffs, 0, at(block, 0)?)?;
        }
        *block = coeffs;
        mb.nonzero |= nz;
        put(above, ax, u8::from(nz))?;
        put(left, ly, u8::from(nz))?;
    }
    Ok(blocks)
}
fn filter_segment(
    mut p: [i32; 8],
    simple: bool,
    macroblock: bool,
    level: i32,
    sharpness: i32,
) -> [i32; 8] {
    let [p3, p2, p1, p0, q0, q1, q2, q3] = p;
    let mut interior = level;
    if sharpness > 0 {
        interior = (interior >> if sharpness > 4 { 2 } else { 1 }).min(9 - sharpness);
    }
    interior = interior.max(1);
    let edge = 2 * (level + if macroblock { 2 } else { 0 }) + interior;
    if 2 * (p0 - q0).abs() + (p1 - q1).abs() / 2 > edge {
        return p;
    }
    if !simple
        && [
            (p3 - p2),
            (p2 - p1),
            (p1 - p0),
            (q1 - q0),
            (q2 - q1),
            (q3 - q2),
        ]
        .iter()
        .any(|d| d.abs() > interior)
    {
        return p;
    }
    let threshold = if level >= 40 {
        2
    } else if level >= 15 {
        1
    } else {
        0
    };
    let hev = (p1 - p0).abs() > threshold || (q1 - q0).abs() > threshold;
    let clip = |v: i32| v.clamp(-128, 127);
    if !simple && macroblock && !hev {
        let w = clip(clip(p1 - q1) + 3 * (q0 - p0));
        let a = (27 * w + 63) >> 7;
        let b = (18 * w + 63) >> 7;
        let c = (9 * w + 63) >> 7;
        p = [
            p3,
            (p2 + c).clamp(0, 255),
            (p1 + b).clamp(0, 255),
            (p0 + a).clamp(0, 255),
            (q0 - a).clamp(0, 255),
            (q1 - b).clamp(0, 255),
            (q2 - c).clamp(0, 255),
            q3,
        ];
    } else {
        let a = clip((if simple || hev { clip(p1 - q1) } else { 0 }) + 3 * (q0 - p0));
        let down = clip(a + 4) >> 3;
        let up = clip(a + 3) >> 3;
        let outer = if !simple && !hev { (down + 1) >> 1 } else { 0 };
        p = [
            p3,
            p2,
            (p1 + outer).clamp(0, 255),
            (p0 + up).clamp(0, 255),
            (q0 - down).clamp(0, 255),
            (q1 - outer).clamp(0, 255),
            q2,
            q3,
        ];
    }
    p
}
impl Plane {
    fn filter_edge(
        &mut self,
        origin: (usize, usize),
        size: usize,
        vertical: bool,
        macroblock: bool,
        params: &Params,
        level: i32,
    ) -> Result<(), Error> {
        let (x, y) = origin;
        for i in 0..size {
            let mut segment = [0; 8];
            for (j, v) in segment.iter_mut().enumerate() {
                let px = x as isize + if vertical { j as isize - 4 } else { i as isize };
                let py = y as isize + if vertical { i as isize } else { j as isize - 4 };
                *v = self.get(px, py)?;
            }
            let out = filter_segment(segment, params.simple, macroblock, level, params.sharpness);
            for (j, v) in out.into_iter().enumerate().take(7).skip(1) {
                let px = if vertical { x + j - 4 } else { x + i };
                let py = if vertical { y + i } else { y + j - 4 };
                self.set(px, py, v)?;
            }
        }
        Ok(())
    }
    fn filter(
        &mut self,
        mx: usize,
        my: usize,
        size: usize,
        mb: &Macroblock,
        params: &Params,
    ) -> Result<(), Error> {
        let level = params.strength(mb.segment, mb.y == 4)?;
        if level == 0 {
            return Ok(());
        }
        let (x, y) = (mx * size, my * size);
        if mx != 0 {
            self.filter_edge((x, y), size, true, true, params, level)?;
        }
        if mb.y == 4 || mb.nonzero {
            for dx in (4..size).step_by(4) {
                self.filter_edge((x + dx, y), size, true, false, params, level)?;
            }
        }
        if my != 0 {
            self.filter_edge((x, y), size, false, true, params, level)?;
        }
        if mb.y == 4 || mb.nonzero {
            for dy in (4..size).step_by(4) {
                self.filter_edge((x, y + dy), size, false, false, params, level)?;
            }
        }
        Ok(())
    }
}
fn reconstruct(data: &[u8], header: Header) -> Result<(Plane, Plane, Plane), Error> {
    let tag = crate::le(crate::take(data, 0, 3)?);
    let first_size = tag >> 5;
    let mut first = Bool::new(crate::take(data, 10, first_size)?);
    let params = Params::read(&mut first)?;
    let (mw, mh) = (header.width.div_ceil(16), header.height.div_ceil(16));
    let mut macroblocks = modes(&mut first, &params, mw, mh)?;
    let sizes_at = 10 + first_size;
    let mut pos = sizes_at + 3 * (params.partitions - 1);
    let mut partitions = Vec::new();
    partitions
        .try_reserve_exact(params.partitions)
        .map_err(|_| Error::Limit)?;
    for i in 0..params.partitions {
        let n = if i + 1 == params.partitions {
            data.len().checked_sub(pos).ok_or(Error::Truncated)?
        } else {
            crate::le(crate::take(data, sizes_at + 3 * i, 3)?)
        };
        partitions.push(Bool::new(crate::take(data, pos, n)?));
        pos += n;
    }
    let mut y = Plane::new(mw * 16, mh * 16)?;
    let mut u = Plane::new(mw * 8, mh * 8)?;
    let mut v = Plane::new(mw * 8, mh * 8)?;
    let mut above = zeros(mw, [0u8; 9])?;
    let mut work = first.work;
    for my in 0..mh {
        let mut left = [0u8; 9];
        let bits = partitions
            .get_mut(my % params.partitions)
            .ok_or(Error::Bitstream)?;
        for mx in 0..mw {
            let mb = macroblocks.get_mut(my * mw + mx).ok_or(Error::Bitstream)?;
            let before = bits.work;
            bits.limit = before + crate::MAX_WORK.checked_sub(work).ok_or(Error::Limit)?;
            let coeffs = coefficients(
                bits,
                &params,
                mb,
                above.get_mut(mx).ok_or(Error::Bitstream)?,
                &mut left,
            )?;
            work += bits.work - before;
            if mb.y != 4 {
                y.predict(mx * 16, my * 16, 16, mb.y)?;
            }
            for i in 0..16 {
                let (x, py) = (mx * 16 + (i % 4) * 4, my * 16 + (i / 4) * 4);
                if mb.y == 4 {
                    y.small_prediction(x, py, at(&mb.modes, i)?)?;
                }
                y.add(x, py, at(&coeffs, i)?)?;
            }
            for (plane, offset) in [(&mut u, 16), (&mut v, 20)] {
                plane.predict(mx * 8, my * 8, 8, mb.uv)?;
                for i in 0..4 {
                    plane.add(
                        mx * 8 + (i % 2) * 4,
                        my * 8 + (i / 2) * 4,
                        at(&coeffs, offset + i)?,
                    )?;
                }
            }
        }
    }
    for my in 0..mh {
        for mx in 0..mw {
            let mb = macroblocks.get(my * mw + mx).ok_or(Error::Bitstream)?;
            y.filter(mx, my, 16, mb, &params)?;
            if !params.simple {
                u.filter(mx, my, 8, mb, &params)?;
                v.filter(mx, my, 8, mb, &params)?;
            }
        }
    }
    Ok((y, u, v))
}
pub(super) fn decode(data: &[u8], header: Header) -> Result<Rgba8, Error> {
    let (y, u, v) = reconstruct(data, header)?;
    let mut rgba = Vec::new();
    rgba.try_reserve_exact(header.width * header.height * 4)
        .map_err(|_| Error::Limit)?;
    for py in 0..header.height {
        for px in 0..header.width {
            let l = y.get(px as isize, py as isize)? - 16;
            let cb = u.get((px / 2) as isize, (py / 2) as isize)? - 128;
            let cr = v.get((px / 2) as isize, (py / 2) as isize)? - 128;
            rgba.extend_from_slice(&[
                ((298 * l + 409 * cr + 128) >> 8).clamp(0, 255) as u8,
                ((298 * l - 100 * cb - 208 * cr + 128) >> 8).clamp(0, 255) as u8,
                ((298 * l + 516 * cb + 128) >> 8).clamp(0, 255) as u8,
                255,
            ]);
        }
    }
    Ok(Rgba8 {
        width: header.width,
        height: header.height,
        data: rgba,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    #[test]
    fn independent_libwebp_planes() {
        for (data, expected) in [
            (
                &include_bytes!("../tests/fixtures/single-lossy.webp")[..],
                &include_bytes!("../tests/fixtures/single-lossy.yuv")[..],
            ),
            (
                &include_bytes!("../tests/fixtures/palette-lossy.webp")[..],
                &include_bytes!("../tests/fixtures/palette-lossy.yuv")[..],
            ),
            (
                &include_bytes!("../tests/fixtures/gradient-lossy.webp")[..],
                &include_bytes!("../tests/fixtures/gradient-lossy.yuv")[..],
            ),
            (
                &include_bytes!("../tests/fixtures/noise-lossy.webp")[..],
                &include_bytes!("../tests/fixtures/noise-lossy.yuv")[..],
            ),
            (
                &include_bytes!("../tests/fixtures/vertical-lossy.webp")[..],
                &include_bytes!("../tests/fixtures/vertical-lossy.yuv")[..],
            ),
            (
                &include_bytes!("../tests/fixtures/horizontal-lossy.webp")[..],
                &include_bytes!("../tests/fixtures/horizontal-lossy.yuv")[..],
            ),
        ] {
            let header = crate::header(data).unwrap();
            let (y, u, v) = reconstruct(&data[20..], header).unwrap();
            let mut actual = Vec::new();
            for (plane, w, h) in [
                (y, header.width, header.height),
                (u, header.width.div_ceil(2), header.height.div_ceil(2)),
                (v, header.width.div_ceil(2), header.height.div_ceil(2)),
            ] {
                for row in 0..h {
                    actual.extend_from_slice(&plane.data[row * plane.width..row * plane.width + w]);
                }
            }
            if actual != expected {
                let mismatch = actual
                    .iter()
                    .zip(expected)
                    .position(|(a, b)| a != b)
                    .unwrap();
                assert_eq!(
                    actual[mismatch], expected[mismatch],
                    "{}x{} offset {}",
                    header.width, header.height, mismatch
                );
            }
        }
    }
    #[test]
    fn truncated_and_mutated_partitions_are_bounded() {
        let data = include_bytes!("../tests/fixtures/gradient-lossy.webp");
        let header = crate::header(data).unwrap();
        let body = &data[20..];
        for n in 0..body.len() {
            let _ = decode(&body[..n], header);
        }
        let mut seed = 0xb331c1a5u32;
        for _ in 0..3000 {
            let mut bytes = body.to_vec();
            for _ in 0..3 {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                let at = seed as usize % bytes.len();
                bytes[at] ^= (seed >> 16) as u8;
            }
            if let Ok(image) = decode(&bytes, header) {
                assert_eq!(image.data.len(), header.width * header.height * 4);
            }
        }
    }
}

//! VP8L, from the WebP lossless bitstream specification.
use crate::{Error, MAX_PREFIX_GROUPS, MAX_WORK};

struct Bits<'a> {
    bytes: &'a [u8],
    pos: usize,
    work: usize,
}
impl Bits<'_> {
    fn read(&mut self, n: usize) -> Result<u32, Error> {
        if n > 24 {
            return Err(Error::Bitstream);
        }
        let end = self.pos.checked_add(n).ok_or(Error::Limit)?;
        if end > self.bytes.len() * 8 {
            return Err(Error::Truncated);
        }
        let mut value = 0;
        for i in 0..n {
            let p = self.pos + i;
            value |=
                u32::from((self.bytes.get(p / 8).copied().ok_or(Error::Truncated)? >> (p % 8)) & 1)
                    << i;
        }
        self.pos = end;
        Ok(value)
    }
    fn charge(&mut self, n: usize) -> Result<(), Error> {
        self.work = self.work.checked_add(n).ok_or(Error::Limit)?;
        if self.work > MAX_WORK {
            return Err(Error::Limit);
        }
        Ok(())
    }
}
fn buffer<T: Clone>(n: usize, value: T) -> Result<Vec<T>, Error> {
    let mut v = Vec::new();
    td_alloc::reserve_exact(&mut v, n).map_err(|_| Error::Limit)?;
    v.resize(n, value);
    Ok(v)
}
// Canonical codes use a compact sorted-symbol table, not exponential lookup tables.
struct Huffman {
    counts: [usize; 16],
    symbols: Vec<u16>,
}
impl Huffman {
    fn build(lengths: &[u8]) -> Result<Self, Error> {
        let mut counts = [0usize; 16];
        for &n in lengths {
            if n != 0 {
                *counts.get_mut(usize::from(n)).ok_or(Error::Bitstream)? += 1;
            }
        }
        let used: usize = counts.iter().sum();
        if used == 0 {
            return Err(Error::Bitstream);
        }
        if used == 1 && counts.get(1) != Some(&1) {
            return Err(Error::Bitstream);
        }
        let mut left = 1isize;
        for &n in counts.iter().skip(1) {
            left = left * 2 - n as isize;
            if left < 0 {
                return Err(Error::Bitstream);
            }
        }
        if used != 1 && left != 0 {
            return Err(Error::Bitstream);
        }
        let mut symbols = Vec::new();
        td_alloc::reserve_exact(&mut symbols, used).map_err(|_| Error::Limit)?;
        for len in 1..16 {
            for (symbol, &n) in lengths.iter().enumerate() {
                if usize::from(n) == len {
                    symbols.push(symbol as u16);
                }
            }
        }
        Ok(Self { counts, symbols })
    }
    fn symbol(&self, bits: &mut Bits<'_>) -> Result<usize, Error> {
        bits.charge(1)?;
        if self.symbols.len() == 1 {
            return self
                .symbols
                .first()
                .copied()
                .map(usize::from)
                .ok_or(Error::Bitstream);
        }
        let mut code = 0usize;
        let mut first = 0;
        let mut offset = 0;
        for &count in self.counts.iter().skip(1) {
            code = (code << 1) | bits.read(1)? as usize;
            if code >= first && code - first < count {
                return self
                    .symbols
                    .get(offset + code - first)
                    .copied()
                    .map(usize::from)
                    .ok_or(Error::Bitstream);
            }
            offset += count;
            first = (first + count) << 1;
        }
        Err(Error::Bitstream)
    }
    fn read(bits: &mut Bits<'_>, alphabet: usize) -> Result<Self, Error> {
        bits.charge(alphabet * 16)?;
        let mut lengths = buffer(alphabet, 0u8)?;
        if bits.read(1)? != 0 {
            let count = bits.read(1)? + 1;
            let n = 1 + 7 * bits.read(1)?;
            let first = bits.read(n as usize)? as usize;
            *lengths.get_mut(first).ok_or(Error::Bitstream)? = 1;
            if count == 2 {
                let second = bits.read(8)? as usize;
                *lengths.get_mut(second).ok_or(Error::Bitstream)? = 1;
            }
        } else {
            const ORDER: [usize; 19] = [
                17, 18, 0, 1, 2, 3, 4, 5, 16, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15,
            ];
            let n = 4 + bits.read(4)? as usize;
            let mut code_lengths = [0u8; 19];
            for &i in ORDER.iter().take(n) {
                *code_lengths.get_mut(i).ok_or(Error::Bitstream)? = bits.read(3)? as u8;
            }
            let tree = Self::build(&code_lengths)?;
            let max = if bits.read(1)? == 0 {
                alphabet
            } else {
                let n = 2 + 2 * bits.read(3)? as usize;
                2 + bits.read(n)? as usize
            };
            if max > alphabet {
                return Err(Error::Bitstream);
            }
            let mut pos = 0;
            let mut previous = 8;
            for _ in 0..max {
                if pos == alphabet {
                    break;
                }
                let symbol = tree.symbol(bits)?;
                let (value, count) = match symbol {
                    0..=15 => {
                        if symbol != 0 {
                            previous = symbol as u8;
                        }
                        (symbol as u8, 1)
                    }
                    16 => (previous, 3 + bits.read(2)? as usize),
                    17 => (0, 3 + bits.read(3)? as usize),
                    18 => (0, 11 + bits.read(7)? as usize),
                    _ => return Err(Error::Bitstream),
                };
                lengths
                    .get_mut(pos..pos + count)
                    .ok_or(Error::Bitstream)?
                    .fill(value);
                pos += count;
            }
        }
        Self::build(&lengths)
    }
}
struct Group {
    green: Huffman,
    red: Huffman,
    blue: Huffman,
    alpha: Huffman,
    distance: Huffman,
}
fn prefix(bits: &mut Bits<'_>, symbol: usize) -> Result<usize, Error> {
    if symbol < 4 {
        return Ok(symbol + 1);
    }
    let n = (symbol - 2) >> 1;
    if n > 18 {
        return Err(Error::Bitstream);
    }
    Ok(((2 + (symbol & 1)) << n) + bits.read(n)? as usize + 1)
}
fn distance(code: usize, width: usize) -> Result<usize, Error> {
    const MAP: [(i32, i32); 120] = [
        (0, 1),
        (1, 0),
        (1, 1),
        (-1, 1),
        (0, 2),
        (2, 0),
        (1, 2),
        (-1, 2),
        (2, 1),
        (-2, 1),
        (2, 2),
        (-2, 2),
        (0, 3),
        (3, 0),
        (1, 3),
        (-1, 3),
        (3, 1),
        (-3, 1),
        (2, 3),
        (-2, 3),
        (3, 2),
        (-3, 2),
        (0, 4),
        (4, 0),
        (1, 4),
        (-1, 4),
        (4, 1),
        (-4, 1),
        (3, 3),
        (-3, 3),
        (2, 4),
        (-2, 4),
        (4, 2),
        (-4, 2),
        (0, 5),
        (3, 4),
        (-3, 4),
        (4, 3),
        (-4, 3),
        (5, 0),
        (1, 5),
        (-1, 5),
        (5, 1),
        (-5, 1),
        (2, 5),
        (-2, 5),
        (5, 2),
        (-5, 2),
        (4, 4),
        (-4, 4),
        (3, 5),
        (-3, 5),
        (5, 3),
        (-5, 3),
        (0, 6),
        (6, 0),
        (1, 6),
        (-1, 6),
        (6, 1),
        (-6, 1),
        (2, 6),
        (-2, 6),
        (6, 2),
        (-6, 2),
        (4, 5),
        (-4, 5),
        (5, 4),
        (-5, 4),
        (3, 6),
        (-3, 6),
        (6, 3),
        (-6, 3),
        (0, 7),
        (7, 0),
        (1, 7),
        (-1, 7),
        (5, 5),
        (-5, 5),
        (7, 1),
        (-7, 1),
        (4, 6),
        (-4, 6),
        (6, 4),
        (-6, 4),
        (2, 7),
        (-2, 7),
        (7, 2),
        (-7, 2),
        (3, 7),
        (-3, 7),
        (7, 3),
        (-7, 3),
        (5, 6),
        (-5, 6),
        (6, 5),
        (-6, 5),
        (8, 0),
        (4, 7),
        (-4, 7),
        (7, 4),
        (-7, 4),
        (8, 1),
        (8, 2),
        (6, 6),
        (-6, 6),
        (8, 3),
        (5, 7),
        (-5, 7),
        (7, 5),
        (-7, 5),
        (8, 4),
        (6, 7),
        (-6, 7),
        (7, 6),
        (-7, 6),
        (8, 5),
        (7, 7),
        (-7, 7),
        (8, 6),
        (8, 7),
    ];
    if code > 120 {
        return Ok(code - 120);
    }
    let &(x, y) = MAP
        .get(code.checked_sub(1).ok_or(Error::Bitstream)?)
        .ok_or(Error::Bitstream)?;
    Ok((x + y * width as i32).max(1) as usize)
}
fn image(bits: &mut Bits<'_>, width: usize, height: usize, meta: bool) -> Result<Vec<u32>, Error> {
    let cache_bits = if bits.read(1)? == 0 {
        0
    } else {
        let n = bits.read(4)? as usize;
        if !(1..=11).contains(&n) {
            return Err(Error::Bitstream);
        }
        n
    };
    let mut cache = buffer(if cache_bits == 0 { 0 } else { 1 << cache_bits }, 0u32)?;
    let mut entropy = Vec::new();
    let mut entropy_bits = 0;
    let mut entropy_width = 0;
    let mut count = 1;
    if meta && bits.read(1)? != 0 {
        entropy_bits = bits.read(3)? as usize + 2;
        entropy_width = width.div_ceil(1 << entropy_bits);
        entropy = image(
            bits,
            entropy_width,
            height.div_ceil(1 << entropy_bits),
            false,
        )?;
        count = entropy
            .iter()
            .map(|p| ((p >> 8) & 65535) as usize)
            .max()
            .ok_or(Error::Bitstream)?
            + 1;
    }
    if count > MAX_PREFIX_GROUPS {
        return Err(Error::Limit);
    }
    let mut groups = Vec::new();
    td_alloc::reserve_exact(&mut groups, count).map_err(|_| Error::Limit)?;
    for _ in 0..count {
        groups.push(Group {
            green: Huffman::read(bits, 280 + cache.len())?,
            red: Huffman::read(bits, 256)?,
            blue: Huffman::read(bits, 256)?,
            alpha: Huffman::read(bits, 256)?,
            distance: Huffman::read(bits, 40)?,
        });
    }
    let size = width.checked_mul(height).ok_or(Error::Limit)?;
    let mut output = Vec::new();
    td_alloc::reserve_exact(&mut output, size).map_err(|_| Error::Limit)?;
    while output.len() < size {
        let pos = output.len();
        let group = if entropy.is_empty() {
            0
        } else {
            let at =
                ((pos / width) >> entropy_bits) * entropy_width + ((pos % width) >> entropy_bits);
            ((entropy.get(at).copied().ok_or(Error::Bitstream)? >> 8) & 65535) as usize
        };
        let group = groups.get(group).ok_or(Error::Bitstream)?;
        let symbol = group.green.symbol(bits)?;
        match symbol {
            0..=255 => {
                let r = group.red.symbol(bits)? as u32;
                let b = group.blue.symbol(bits)? as u32;
                let a = group.alpha.symbol(bits)? as u32;
                output.push((a << 24) | (r << 16) | ((symbol as u32) << 8) | b);
            }
            256..=279 => {
                let len = prefix(bits, symbol - 256)?;
                let code = group.distance.symbol(bits)?;
                let distance = distance(prefix(bits, code)?, width)?;
                if distance > pos || len > size - pos {
                    return Err(Error::Bitstream);
                }
                bits.charge(len)?;
                for _ in 0..len {
                    let p = output
                        .get(output.len() - distance)
                        .copied()
                        .ok_or(Error::Bitstream)?;
                    output.push(p);
                }
            }
            _ => output.push(*cache.get(symbol - 280).ok_or(Error::Bitstream)?),
        }
        if cache_bits != 0 {
            for &p in output.get(pos..).ok_or(Error::Bitstream)? {
                let hash = p.wrapping_mul(0x1e35a7bd) >> (32 - cache_bits);
                *cache.get_mut(hash as usize).ok_or(Error::Bitstream)? = p;
            }
        }
    }
    Ok(output)
}
struct Transform {
    kind: u32,
    width: usize,
    bits: usize,
    data: Vec<u32>,
}
fn add(a: u32, b: u32) -> u32 {
    let mut output = 0;
    for shift in [0, 8, 16, 24] {
        output |= (((a >> shift) as u8).wrapping_add((b >> shift) as u8) as u32) << shift;
    }
    output
}
fn average(a: u32, b: u32) -> u32 {
    (a & b) + (((a ^ b) & 0xfefefefe) >> 1)
}
fn predict(mode: u32, l: u32, t: u32, tl: u32, tr: u32) -> Result<u32, Error> {
    Ok(match mode {
        0 => 0xff000000,
        1 => l,
        2 => t,
        3 => tr,
        4 => tl,
        5 => average(average(l, tr), t),
        6 => average(l, tl),
        7 => average(l, t),
        8 => average(tl, t),
        9 => average(t, tr),
        10 => average(average(l, tl), average(t, tr)),
        11 => {
            let mut dl = 0i32;
            let mut dt = 0i32;
            for shift in [0, 8, 16, 24] {
                let a = ((l >> shift) & 255) as i32;
                let b = ((t >> shift) & 255) as i32;
                let c = ((tl >> shift) & 255) as i32;
                dl += (b - c).abs();
                dt += (a - c).abs();
            }
            if dl < dt {
                l
            } else {
                t
            }
        }
        12 | 13 => {
            let mut out = 0;
            for shift in [0, 8, 16, 24] {
                let a = ((l >> shift) & 255) as i32;
                let b = ((t >> shift) & 255) as i32;
                let c = ((tl >> shift) & 255) as i32;
                let v = if mode == 12 {
                    a + b - c
                } else {
                    let avg = (a + b) / 2;
                    avg + (avg - c) / 2
                };
                out |= (v.clamp(0, 255) as u32) << shift;
            }
            out
        }
        _ => return Err(Error::Bitstream),
    })
}
fn pixel(pixels: &[u32], pos: usize) -> Result<u32, Error> {
    pixels.get(pos).copied().ok_or(Error::Bitstream)
}
fn delta(a: u8, b: u8) -> u8 {
    ((i32::from(a as i8) * i32::from(b as i8)) >> 5) as u8
}
fn inverse(transform: Transform, pixels: &mut Vec<u32>, height: usize) -> Result<(), Error> {
    let width = transform.width;
    if transform.kind == 3 {
        let packed_width = width.div_ceil(1 << transform.bits);
        let mut output = buffer(width * height, 0u32)?;
        let n = 8 >> transform.bits;
        for (pos, p) in output.iter_mut().enumerate() {
            let (x, y) = (pos % width, pos / width);
            let packed = (pixel(pixels, y * packed_width + (x >> transform.bits))? >> 8) & 255;
            let index =
                ((packed >> ((x & ((1 << transform.bits) - 1)) * n)) & ((1 << n) - 1)) as usize;
            *p = transform.data.get(index).copied().unwrap_or(0);
        }
        *pixels = output;
        return Ok(());
    }
    let block_width = width.div_ceil(1 << transform.bits);
    for pos in 0..pixels.len() {
        let (x, y) = (pos % width, pos / width);
        let p = pixel(pixels, pos)?;
        let result = match transform.kind {
            0 => {
                let mode = (pixel(
                    &transform.data,
                    (y >> transform.bits) * block_width + (x >> transform.bits),
                )? >> 8)
                    & 255;
                if mode > 13 {
                    return Err(Error::Bitstream);
                }
                let pred = if pos == 0 {
                    0xff000000
                } else if y == 0 {
                    pixel(pixels, pos - 1)?
                } else if x == 0 {
                    pixel(pixels, pos - width)?
                } else {
                    predict(
                        mode,
                        pixel(pixels, pos - 1)?,
                        pixel(pixels, pos - width)?,
                        pixel(pixels, pos - width - 1)?,
                        pixel(pixels, pos - width + 1)?,
                    )?
                };
                add(p, pred)
            }
            1 => {
                let c = pixel(
                    &transform.data,
                    (y >> transform.bits) * block_width + (x >> transform.bits),
                )?;
                let g = (p >> 8) as u8;
                let r = ((p >> 16) as u8).wrapping_add(delta(c as u8, g));
                let b = (p as u8)
                    .wrapping_add(delta((c >> 8) as u8, g))
                    .wrapping_add(delta((c >> 16) as u8, r));
                (p & 0xff00ff00) | (u32::from(r) << 16) | u32::from(b)
            }
            2 => {
                let g = (p >> 8) as u8;
                (p & 0xff00ff00)
                    | (u32::from(((p >> 16) as u8).wrapping_add(g)) << 16)
                    | u32::from((p as u8).wrapping_add(g))
            }
            _ => return Err(Error::Bitstream),
        };
        *pixels.get_mut(pos).ok_or(Error::Bitstream)? = result;
    }
    Ok(())
}
pub(crate) fn decode(bytes: &[u8], width: usize, height: usize) -> Result<Vec<u32>, Error> {
    let mut bits = Bits {
        bytes,
        pos: 0,
        work: 0,
    };
    let mut transforms = Vec::new();
    td_alloc::reserve_exact(&mut transforms, 4).map_err(|_| Error::Limit)?;
    let mut seen = 0u32;
    let mut coded_width = width;
    while bits.read(1)? != 0 {
        let kind = bits.read(2)?;
        if seen & (1 << kind) != 0 {
            return Err(Error::Bitstream);
        }
        seen |= 1 << kind;
        let mut transform = Transform {
            kind,
            width: coded_width,
            bits: 0,
            data: Vec::new(),
        };
        match kind {
            0 | 1 => {
                transform.bits = bits.read(3)? as usize + 2;
                transform.data = image(
                    &mut bits,
                    coded_width.div_ceil(1 << transform.bits),
                    height.div_ceil(1 << transform.bits),
                    false,
                )?;
            }
            3 => {
                let n = bits.read(8)? as usize + 1;
                transform.data = image(&mut bits, n, 1, false)?;
                let mut previous = 0;
                for p in &mut transform.data {
                    *p = add(*p, previous);
                    previous = *p;
                }
                transform.bits = match n {
                    1..=2 => 3,
                    3..=4 => 2,
                    5..=16 => 1,
                    _ => 0,
                };
                coded_width = coded_width.div_ceil(1 << transform.bits);
            }
            2 => {}
            _ => return Err(Error::Bitstream),
        }
        transforms.push(transform);
    }
    let mut pixels = image(&mut bits, coded_width, height, true)?;
    for transform in transforms.into_iter().rev() {
        bits.charge(transform.width * height)?;
        inverse(transform, &mut pixels, height)?;
    }
    Ok(pixels)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn truncated_and_mutated_entropy_stays_bounded() {
        let encoded = include_bytes!("../tests/fixtures/gradient.webp");
        let stream = &encoded[25..];
        let expected = decode(stream, 65, 47).unwrap();
        for n in 0..stream.len() {
            if let Ok(pixels) = decode(&stream[..n], 65, 47) {
                assert_eq!(pixels, expected);
            }
        }
        let mut seed = 0x7d31_4c21u32;
        for _ in 0..2000 {
            let mut bytes = stream.to_vec();
            for _ in 0..3 {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                let at = seed as usize % bytes.len();
                bytes[at] ^= (seed >> 16) as u8;
            }
            if let Ok(pixels) = decode(&bytes, 65, 47) {
                assert_eq!(pixels.len(), 65 * 47);
            }
        }
    }

    #[test]
    fn prefix_tree_and_work_admission() {
        assert!(Huffman::build(&[]).is_err());
        assert!(Huffman::build(&[1, 1, 1]).is_err());
        assert!(Huffman::build(&[2, 2]).is_err());
        assert!(Huffman::build(&[0, 2]).is_err());
        assert!(Huffman::build(&[16]).is_err());
        let singleton = Huffman::build(&[0, 1]).unwrap();
        let mut bits = Bits {
            bytes: &[],
            pos: 0,
            work: 0,
        };
        assert_eq!(singleton.symbol(&mut bits), Ok(1));
        bits.work = MAX_WORK;
        assert_eq!(singleton.symbol(&mut bits), Err(Error::Limit));
    }
}

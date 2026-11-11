//! Bounded reduced-header, eight-bit 4:2:0 intra decoding.
use super::*;

pub const MAX_INPUT_BYTES: usize = 64 << 20;
pub const MAX_IMAGE_PIXELS: usize = 16 << 20;
pub const MAX_SYMBOLS: usize = 64 << 20;
pub const MAX_OBUS: usize = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    Truncated,
    Bitstream,
    Unsupported,
    Limit,
}
impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Truncated => "AV1 data ends early",
            Self::Bitstream => "malformed AV1 stream",
            Self::Unsupported => "unsupported AV1 coding tool",
            Self::Limit => "AV1 decode budget exceeded",
        })
    }
}
impl std::error::Error for DecodeError {}
type Result<T> = std::result::Result<T, DecodeError>;

struct ReadBits<'a> {
    data: &'a [u8],
    bit: usize,
}
impl<'a> ReadBits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, bit: 0 }
    }
    fn f(&mut self, n: usize) -> Result<u32> {
        if n > 32 {
            return Err(DecodeError::Bitstream);
        }
        let mut value = 0;
        for _ in 0..n {
            let byte = self.data.get(self.bit / 8).ok_or(DecodeError::Truncated)?;
            value = (value << 1) | u32::from((byte >> (7 - self.bit % 8)) & 1);
            self.bit += 1;
        }
        Ok(value)
    }
    fn flag(&mut self) -> Result<bool> {
        Ok(self.f(1)? != 0)
    }
    fn off(&mut self) -> Result<()> {
        if self.flag()? {
            Err(DecodeError::Unsupported)
        } else {
            Ok(())
        }
    }
    fn align(&mut self) -> Result<()> {
        while !self.bit.is_multiple_of(8) {
            if self.flag()? {
                return Err(DecodeError::Bitstream);
            }
        }
        Ok(())
    }
    fn trailing(&mut self) -> Result<()> {
        if !self.flag()? {
            return Err(DecodeError::Bitstream);
        }
        self.align()?;
        if self
            .data
            .get(self.bit / 8..)
            .ok_or(DecodeError::Truncated)?
            .iter()
            .any(|byte| *byte != 0)
        {
            return Err(DecodeError::Bitstream);
        }
        self.bit = self.data.len() * 8;
        Ok(())
    }
}

struct Symbols<'a, 'b> {
    bits: ReadBits<'a>,
    value: u32,
    range: u32,
    left: i64,
    budget: &'b mut usize,
    adapt: bool,
    filter_use: [Cdf; 10],
    filter_mode: Cdf,
}
impl<'a, 'b> Symbols<'a, 'b> {
    fn new(data: &'a [u8], budget: &'b mut usize, adapt: bool) -> Result<Self> {
        if data.is_empty() {
            return Err(DecodeError::Truncated);
        }
        let mut bits = ReadBits::new(data);
        let n = (data.len() * 8).min(15);
        let value = 32767 ^ (bits.f(n)? << (15 - n));
        Ok(Self {
            bits,
            value,
            range: 32768,
            left: data.len() as i64 * 8 - 15,
            budget,
            adapt,
            filter_use: [
                4621, 6743, 5893, 7866, 12551, 9394, 12408, 14301, 12756, 22343,
            ]
            .map(|v| Cdf::new(&[v])),
            filter_mode: Cdf::new(&[8949, 12776, 17211, 29558]),
        })
    }
    fn read(&mut self, cdf: &mut Cdf) -> Result<usize> {
        *self.budget = self.budget.checked_sub(1).ok_or(DecodeError::Limit)?;
        let mut cur = self.range;
        let mut symbol = None;
        for s in 0..cdf.n {
            let prev = cur;
            let f = 32768 - cdf.at(s);
            cur = (((self.range >> 8) * (f >> 6)) >> 1) + 4 * (cdf.n as u32 - s as u32 - 1);
            if self.value >= cur {
                self.range = prev
                    .checked_sub(cur)
                    .filter(|v| *v != 0)
                    .ok_or(DecodeError::Bitstream)?;
                self.value -= cur;
                symbol = Some(s);
                break;
            }
        }
        let symbol = symbol.ok_or(DecodeError::Bitstream)?;
        let n = 15 - self.range.ilog2();
        self.range <<= n;
        let available = i64::from(n).min(self.left.max(0)) as usize;
        let padded = self.bits.f(available)? << (n as usize - available);
        self.value = padded ^ (((self.value + 1) << n) - 1);
        self.left -= i64::from(n);
        if self.left < -14 {
            return Err(DecodeError::Truncated);
        }
        if self.adapt {
            cdf.adapt(symbol);
        }
        Ok(symbol)
    }
    fn flag(&mut self) -> Result<bool> {
        Ok(self.read(&mut Cdf::new(&[16384]))? != 0)
    }
    fn literal(&mut self, n: usize) -> Result<usize> {
        let mut out = 0;
        for _ in 0..n {
            out = (out << 1) | usize::from(self.flag()?);
        }
        Ok(out)
    }
    fn finish(mut self) -> Result<()> {
        let rewind = 15i64.min(self.left + 15) as usize;
        self.bits.bit = self
            .bits
            .bit
            .checked_sub(rewind)
            .ok_or(DecodeError::Bitstream)?;
        self.bits.trailing()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecodeHeader {
    pub width: usize,
    pub height: usize,
    pub config: [u8; 4],
    pub primaries: u8,
    pub transfer: u8,
    pub matrix: u8,
    pub full_range: bool,
}
struct Sequence {
    header: DecodeHeader,
    edge_filter: bool,
    filter_intra: bool,
    cdef: bool,
}
fn sequence(data: &[u8]) -> Result<Sequence> {
    let mut b = ReadBits::new(data);
    if b.f(3)? != 0 || !b.flag()? || !b.flag()? {
        return Err(DecodeError::Unsupported);
    }
    let level = b.f(5)? as u8;
    let wb = b.f(4)? as usize + 1;
    let hb = b.f(4)? as usize + 1;
    let width = b.f(wb)? as usize + 1;
    let height = b.f(hb)? as usize + 1;
    if width > MAX_AXIS || height > MAX_AXIS || width * height > MAX_IMAGE_PIXELS {
        return Err(DecodeError::Limit);
    }
    b.off()?; // 128x128 superblocks
    let filter_intra = b.flag()?;
    let edge_filter = b.flag()?;
    b.off()?; // superresolution
    let cdef = b.flag()?;
    b.off()?; // restoration
    b.off()?; // high bit depth
    b.off()?; // monochrome
    let (primaries, transfer, matrix) = if b.flag()? {
        (b.f(8)? as u8, b.f(8)? as u8, b.f(8)? as u8)
    } else {
        (2, 2, 2)
    };
    if matrix == 0 {
        return Err(DecodeError::Unsupported);
    }
    let full_range = b.flag()?;
    let chroma_position = b.f(2)? as u8;
    if chroma_position == 3 {
        return Err(DecodeError::Bitstream);
    }
    b.off()?; // separate chroma delta Q
    b.off()?; // film grain
    b.trailing()?;
    Ok(Sequence {
        header: DecodeHeader {
            width,
            height,
            config: [0x81, level, 12 | chroma_position, 0],
            primaries,
            transfer,
            matrix,
            full_range,
        },
        edge_filter,
        filter_intra,
        cdef,
    })
}

struct Obus<'a> {
    data: &'a [u8],
    pos: usize,
    count: usize,
}
impl<'a> Obus<'a> {
    fn new(data: &'a [u8]) -> Result<Self> {
        if data.len() > MAX_INPUT_BYTES {
            return Err(DecodeError::Limit);
        }
        Ok(Self {
            data,
            pos: 0,
            count: 0,
        })
    }
    fn byte(&mut self) -> Result<u8> {
        let value = *self.data.get(self.pos).ok_or(DecodeError::Truncated)?;
        self.pos += 1;
        Ok(value)
    }
    fn next(&mut self) -> Result<Option<(u8, &'a [u8])>> {
        if self.pos == self.data.len() {
            return Ok(None);
        }
        self.count += 1;
        if self.count > MAX_OBUS {
            return Err(DecodeError::Limit);
        }
        let header = self.byte()?;
        if header & 0x81 != 0 || header & 2 == 0 {
            return Err(DecodeError::Bitstream);
        }
        if header & 4 != 0 && self.byte()? != 0 {
            return Err(DecodeError::Unsupported);
        }
        let mut length = 0usize;
        let mut done = false;
        for shift in (0..56).step_by(7) {
            let b = self.byte()?;
            let value = usize::from(b & 127)
                .checked_shl(shift)
                .ok_or(DecodeError::Limit)?;
            length = length.checked_add(value).ok_or(DecodeError::Limit)?;
            if b & 128 == 0 {
                done = true;
                break;
            }
        }
        if !done {
            return Err(DecodeError::Bitstream);
        }
        let end = self.pos.checked_add(length).ok_or(DecodeError::Limit)?;
        let data = self.data.get(self.pos..end).ok_or(DecodeError::Truncated)?;
        self.pos = end;
        Ok(Some(((header >> 3) & 15, data)))
    }
}

pub fn header(data: &[u8]) -> Result<DecodeHeader> {
    let mut obus = Obus::new(data)?;
    while let Some((kind, data)) = obus.next()? {
        match kind {
            1 => return Ok(sequence(data)?.header),
            2 if data.is_empty() => {}
            15 => {}
            _ => return Err(DecodeError::Bitstream),
        }
    }
    Err(DecodeError::Truncated)
}

struct Frame {
    geometry: Geometry,
    qindex: u8,
    levels: deblock::Levels,
    strengths: cdef::Strengths,
    tile_size_bytes: usize,
    adapt: bool,
}
fn frame_header(b: &mut ReadBits<'_>, s: &Sequence) -> Result<Frame> {
    let adapt = !b.flag()?;
    b.off()?; // screen content tools
    if b.flag()? {
        b.f(16)?;
        b.f(16)?;
    } // render size is not the coded extent
    if !b.flag()? {
        return Err(DecodeError::Unsupported);
    } // uniform tile spacing
    let (width, height) = (s.header.width, s.header.height);
    let sb_cols = width.div_ceil(64);
    let sb_rows = height.div_ceil(64);
    let min_cols = tile_log2(MAX_TILE_WIDTH >> 6, sb_cols);
    let max_cols = tile_log2(1, sb_cols.min(MAX_TILE_COLS));
    let max_rows = tile_log2(1, sb_rows.min(MAX_TILE_ROWS));
    let min_tiles = min_cols.max(tile_log2(MAX_TILE_AREA >> 12, sb_cols * sb_rows));
    let mut cols = min_cols;
    while cols < max_cols && b.flag()? {
        cols += 1;
    }
    let mut rows = min_tiles.saturating_sub(cols);
    while rows < max_rows && b.flag()? {
        rows += 1;
    }
    let geometry = Geometry::tiled(width, height, cols, rows).map_err(|_| DecodeError::Limit)?;
    if geometry.tile_cols_log2 != cols || geometry.tile_rows_log2 != rows {
        return Err(DecodeError::Bitstream);
    }
    let tile_size_bytes = if cols + rows > 0 {
        let update_id = b.f((cols + rows) as usize)? as usize;
        if update_id >= geometry.tile_cols() * geometry.tile_rows() {
            return Err(DecodeError::Bitstream);
        }
        b.f(2)? as usize + 1
    } else {
        0
    };
    let qindex = b.f(8)? as u8;
    if qindex == 0 {
        return Err(DecodeError::Unsupported);
    }
    for _ in 0..6 {
        b.off()?;
    } // delta quantizers, matrix, segmentation, delta_q
    let luma = [b.f(6)? as u8, b.f(6)? as u8];
    let (u, v) = if luma == [0, 0] {
        (0, 0)
    } else {
        (b.f(6)? as u8, b.f(6)? as u8)
    };
    if b.f(3)? != 0 {
        return Err(DecodeError::Unsupported);
    }
    let reference_delta = if b.flag()? {
        let mut intra_delta = 1i32;
        if b.flag()? {
            for reference in 0..8 {
                if b.flag()? {
                    let raw = b.f(7)? as i32;
                    let signed = if raw & 64 != 0 { raw - 128 } else { raw };
                    if reference == 0 {
                        intra_delta = signed;
                    }
                }
            }
            for _ in 0..2 {
                if b.flag()? {
                    b.f(7)?;
                }
            }
        }
        intra_delta
    } else {
        0
    };
    let filtering_enabled = luma != [0, 0];
    let adjusted = |level: u8| {
        if !filtering_enabled {
            0
        } else {
            (i32::from(level) + reference_delta * (1 << (level >> 5))).clamp(0, 63) as u8
        }
    };
    let luma = luma.map(adjusted);
    // AV1 7.14 admits chroma planes only when their base level is nonzero;
    // luma directions are admitted together by the frame-level gate.
    let (u, v) = (
        if u == 0 { 0 } else { adjusted(u) },
        if v == 0 { 0 } else { adjusted(v) },
    );
    let mut strengths = cdef::Strengths {
        damping: 3,
        y_pri: 0,
        y_sec: 0,
        uv_pri: 0,
        uv_sec: 0,
    };
    if s.cdef {
        strengths.damping = b.f(2)? as u8 + 3;
        if b.f(2)? != 0 {
            return Err(DecodeError::Unsupported);
        }
        strengths.y_pri = b.f(4)? as u8;
        strengths.y_sec = b.f(2)? as u8;
        strengths.uv_pri = b.f(4)? as u8;
        strengths.uv_sec = b.f(2)? as u8;
    }
    b.off()?; // TX_MODE_SELECT
    if !b.flag()? {
        return Err(DecodeError::Unsupported);
    } // reduced transform set
    b.align()?;
    Ok(Frame {
        geometry,
        qindex,
        levels: deblock::Levels { luma, u, v },
        strengths,
        tile_size_bytes,
        adapt,
    })
}

pub struct Decoded {
    pub header: DecodeHeader,
    pub planes: [Vec<u8>; 3],
}
pub fn decode(data: &[u8]) -> Result<Decoded> {
    let mut obus = Obus::new(data)?;
    let mut seq = None;
    let mut picture = None;
    while let Some((kind, data)) = obus.next()? {
        match kind {
            1 if seq.is_none() && picture.is_none() => seq = Some(sequence(data)?),
            2 if data.is_empty() && picture.is_none() => {}
            6 if picture.is_none() => {
                let s = seq.as_ref().ok_or(DecodeError::Bitstream)?;
                let mut b = ReadBits::new(data);
                let frame = frame_header(&mut b, s)?;
                picture = Some(decode_frame(
                    data.get(b.bit / 8..).ok_or(DecodeError::Truncated)?,
                    frame,
                    s,
                )?);
            }
            15 => {}
            _ => return Err(DecodeError::Unsupported),
        }
    }
    picture.ok_or(DecodeError::Truncated)
}

fn zeroes<T: Default + Clone>(len: usize) -> Result<Vec<T>> {
    let mut out = Vec::new();
    out.try_reserve_exact(len).map_err(|_| DecodeError::Limit)?;
    out.resize(len, T::default());
    Ok(out)
}

fn decode_frame(data: &[u8], frame: Frame, seq: &Sequence) -> Result<Decoded> {
    let g = &frame.geometry;
    let (stride, rows) = (g.sb_cols * SB, g.sb_rows * SB);
    let area = stride
        .checked_mul(rows)
        .filter(|n| *n <= 20 << 20)
        .ok_or(DecodeError::Limit)?;
    let mut kept = Kept {
        planes: [zeroes(area)?, zeroes(area / 4)?, zeroes(area / 4)?],
        sizes: zeroes(area / 16)?,
        skips: zeroes(area / 16)?,
    };
    let mut bits = ReadBits::new(data);
    let tiles = g.tile_cols() * g.tile_rows();
    if tiles > 1 {
        bits.off()?;
        bits.align()?;
    }
    let mut pos = bits.bit / 8;
    let mut work = MAX_SYMBOLS;
    for ty in 0..g.tile_rows() {
        for tx in 0..g.tile_cols() {
            let index = ty * g.tile_cols() + tx;
            let length = if index + 1 < tiles {
                let mut n = 0usize;
                for shift in 0..frame.tile_size_bytes {
                    n |= usize::from(*data.get(pos).ok_or(DecodeError::Truncated)?) << (8 * shift);
                    pos += 1;
                }
                n.checked_add(1).ok_or(DecodeError::Limit)?
            } else {
                data.len().checked_sub(pos).ok_or(DecodeError::Truncated)?
            };
            let end = pos.checked_add(length).ok_or(DecodeError::Limit)?;
            let tile_data = data.get(pos..end).ok_or(DecodeError::Truncated)?;
            pos = end;
            let x0 = *g.tile_col_starts.get(tx).ok_or(DecodeError::Bitstream)? * SB;
            let width = *g
                .tile_col_starts
                .get(tx + 1)
                .ok_or(DecodeError::Bitstream)?
                * SB
                - x0;
            let start = *g.tile_row_starts.get(ty).ok_or(DecodeError::Bitstream)? * SB_MI;
            let end = (*g
                .tile_row_starts
                .get(ty + 1)
                .ok_or(DecodeError::Bitstream)?
                * SB_MI)
                .min(g.mi_rows);
            // Tile scratch is at most 4096 columns by 65 rows, independent of image height.
            let mut tile = Tile::new(x0, width, start, end, g, frame.qindex);
            let mut symbols = Symbols::new(tile_data, &mut work, frame.adapt)?;
            for row in (start..end).step_by(SB_MI) {
                tile.band_mi_row = row;
                tile.contexts.clear_left();
                for sb in 0..width / SB {
                    let col = sb * SB_MI;
                    tile.contexts
                        .clear_decoded(tile.mi_col_end.saturating_sub(col), end - row);
                    decode_partition(&mut tile, &mut symbols, At::square(row, col, 6), seq)?;
                }
                kept.take(&tile, g);
                for band in &mut tile.recon {
                    let last = band.rows - 1;
                    let (head, tail) = band
                        .data
                        .split_at_mut_checked(band.stride)
                        .ok_or(DecodeError::Bitstream)?;
                    let start = (last - 1) * band.stride;
                    let source = tail
                        .get(start..start + band.stride)
                        .ok_or(DecodeError::Bitstream)?;
                    head.copy_from_slice(source);
                }
            }
            symbols.finish()?;
        }
    }
    // Share the identical deblocking/CDEF/crop reconstruction path with the encoder.
    let mut encoder =
        Encoder::<InOrder>::with_geometry(frame.geometry, 50, 1).map_err(|_| DecodeError::Limit)?;
    encoder.levels = frame.levels;
    encoder.strengths = frame.strengths;
    let image = encoder.reconstruct(kept);
    Ok(Decoded {
        header: seq.header,
        planes: image.planes,
    })
}

fn decode_partition(
    tile: &mut Tile,
    symbols: &mut Symbols<'_, '_>,
    at: At,
    seq: &Sequence,
) -> Result<()> {
    let Some((has_rows, has_cols)) = tile.edges(at) else {
        return Ok(());
    };
    let part = if at.w_log2 < 3 {
        0
    } else {
        let bsl = at.w_log2 - 2;
        let above = tile.avail_up(at.mi_row)
            && tile
                .contexts
                .above_width_log2
                .get(at.mi_col)
                .is_some_and(|v| u32::from(*v) < bsl);
        let left = tile.avail_left(at.mi_col)
            && tile
                .contexts
                .left_height_log2
                .get(at.sb_row())
                .is_some_and(|v| u32::from(*v) < bsl);
        let ctx = usize::from(left) * 2 + usize::from(above);
        let cdf = if bsl == 1 {
            tile.cdfs.partition_8.get_mut(ctx)
        } else {
            tile.cdfs
                .partition
                .get_mut(bsl as usize - 2)
                .and_then(|r| r.get_mut(ctx))
        }
        .ok_or(DecodeError::Bitstream)?;
        if has_rows && has_cols {
            symbols.read(cdf)?
        } else if has_rows || has_cols {
            let kinds: &[usize] = if has_cols {
                &[2, 3, 4, 6, 7, 9]
            } else {
                &[1, 3, 4, 5, 6, 8]
            };
            let mut probability = 0;
            for &k in kinds {
                if k < cdf.n {
                    probability += cdf.at(k) - if k == 0 { 0 } else { cdf.at(k - 1) };
                }
            }
            let mut derived = Cdf::new(&[(32768 - probability.min(32767)) as u16]);
            if symbols.read(&mut derived)? != 0 {
                3
            } else if has_cols {
                1
            } else {
                2
            }
        } else {
            3
        }
    };
    match part {
        0 => decode_block(tile, symbols, at, seq),
        1 | 2 if at.w_log2 < 6 => {
            for block in at.halves(part == 2) {
                if tile.edges(block).is_some() {
                    decode_block(tile, symbols, block, seq)?;
                }
            }
            Ok(())
        }
        3 => {
            let half = at.w_units() / 2;
            for (dr, dc) in [(0, 0), (0, half), (half, 0), (half, half)] {
                decode_partition(tile, symbols, at.quarter(dr, dc), seq)?;
            }
            Ok(())
        }
        _ => Err(DecodeError::Unsupported),
    }
}

fn delta(tile: &mut Tile, symbols: &mut Symbols<'_, '_>, at: At, mode: u8) -> Result<i8> {
    if at.has_angle_deltas() && is_directional(mode) {
        let cdf = tile
            .cdfs
            .angle_delta
            .get_mut(usize::from(mode - V_PRED))
            .ok_or(DecodeError::Bitstream)?;
        Ok(symbols.read(cdf)? as i8 - 3)
    } else {
        Ok(0)
    }
}

fn decode_block(
    tile: &mut Tile,
    symbols: &mut Symbols<'_, '_>,
    at: At,
    seq: &Sequence,
) -> Result<()> {
    let up = tile.avail_up(at.mi_row);
    let left = tile.avail_left(at.mi_col);
    let above_skip = if up {
        tile.contexts
            .above_skip
            .get(at.mi_col)
            .copied()
            .unwrap_or(0)
    } else {
        0
    };
    let left_skip = if left {
        tile.contexts
            .left_skip
            .get(at.sb_row())
            .copied()
            .unwrap_or(0)
    } else {
        0
    };
    let skip = symbols.read(
        tile.cdfs
            .skip
            .get_mut(usize::from(above_skip + left_skip))
            .ok_or(DecodeError::Bitstream)?,
    )? != 0;
    let above_mode = if up {
        tile.contexts
            .above_mode
            .get(at.mi_col)
            .copied()
            .unwrap_or(0)
    } else {
        0
    };
    let left_mode = if left {
        tile.contexts
            .left_mode
            .get(at.sb_row())
            .copied()
            .unwrap_or(0)
    } else {
        0
    };
    let cdf = tile
        .cdfs
        .y_mode
        .get_mut(mode_context(above_mode))
        .and_then(|r| r.get_mut(mode_context(left_mode)))
        .ok_or(DecodeError::Bitstream)?;
    let y_mode = symbols.read(cdf)? as u8;
    let y_delta = delta(tile, symbols, at, y_mode)?;
    let mut uv_mode = 0;
    let mut cfl = [0i8; 2];
    let mut uv_delta = 0;
    if at.chroma().is_some() {
        let table = if at.cfl_allowed() {
            &mut tile.cdfs.uv_mode
        } else {
            &mut tile.cdfs.uv_mode_no_cfl
        };
        uv_mode = symbols.read(
            table
                .get_mut(usize::from(y_mode))
                .ok_or(DecodeError::Bitstream)?,
        )? as u8;
        if uv_mode == UV_CFL_PRED {
            let signs = symbols.read(&mut tile.cdfs.cfl_sign)? + 1;
            let (u, v) = (signs / 3, signs % 3);
            for (slot, (own, other)) in cfl.iter_mut().zip([(u, v), (v, u)]) {
                if own != 0 {
                    let value = symbols.read(
                        tile.cfl_alpha_cdf(own, other)
                            .ok_or(DecodeError::Bitstream)?,
                    )? as i8
                        + 1;
                    *slot = if own == 1 { -value } else { value };
                }
            }
        }
        uv_delta = delta(tile, symbols, at, uv_mode)?;
    }
    let filter_mode = if seq.filter_intra && y_mode == DC_PRED && at.width().max(at.height()) <= 32
    {
        let index = match (at.width(), at.height()) {
            (4, 4) => 0,
            (4, 8) => 1,
            (8, 4) => 2,
            (8, 8) => 3,
            (8, 16) => 4,
            (16, 8) => 5,
            (16, 16) => 6,
            (16, 32) => 7,
            (32, 16) => 8,
            (32, 32) => 9,
            _ => return Err(DecodeError::Unsupported),
        };
        let mut cdf = *symbols
            .filter_use
            .get(index)
            .ok_or(DecodeError::Bitstream)?;
        let on = symbols.read(&mut cdf)? != 0;
        *symbols
            .filter_use
            .get_mut(index)
            .ok_or(DecodeError::Bitstream)? = cdf;
        if on {
            let mut cdf = symbols.filter_mode;
            let mode = symbols.read(&mut cdf)?;
            symbols.filter_mode = cdf;
            Some(mode)
        } else {
            None
        }
    } else {
        None
    };
    let transform_mode = if let Some(mode) = filter_mode {
        *[DC_PRED, V_PRED, H_PRED, D157_PRED, DC_PRED]
            .get(mode)
            .ok_or(DecodeError::Bitstream)?
    } else {
        y_mode
    };
    let mut scratch = std::mem::take(&mut tile.scratch);
    let mut leaf = Leaf {
        at,
        skip,
        y_mode,
        uv_mode,
        deltas: [y_delta, uv_delta],
        cfl,
        tx_type: TxType::DctDct,
        levels: [Vec::new(), Vec::new(), Vec::new()],
        eob: [0; 3],
    };
    for (levels, trial) in leaf.levels.iter_mut().zip(&mut scratch.trial) {
        std::mem::swap(levels, &mut trial.levels);
    }
    for plane in 0..3 {
        let Some(pat) = plane_at(at, plane) else {
            continue;
        };
        let size = plane_size(pat, plane);
        let levels = leaf.levels.get_mut(plane).ok_or(DecodeError::Bitstream)?;
        clear_buffer(levels, size.coded().area())?;
        let (mut tx, mut eob) = (
            if plane == 0 {
                TxType::DctDct
            } else {
                mode_tx_type(uv_mode, size)
            },
            0,
        );
        if !skip {
            (tx, eob) = coefficients(tile, symbols, plane, pat, transform_mode, tx, levels)?;
        }
        if plane == 0 {
            leaf.tx_type = tx;
        }
        let pred = &mut scratch.preds;
        clear_buffer(pred, size.area())?;
        let mode = if plane == 0 {
            y_mode
        } else if uv_mode == UV_CFL_PRED {
            DC_PRED
        } else {
            uv_mode
        };
        let d = if plane == 0 { y_delta } else { uv_delta };
        let edges = tile.pred_edges(plane, pat);
        if let Some(mode) = filter_mode.filter(|_| plane == 0) {
            filter_prediction(mode, &edges, pred)?;
        } else {
            predict_block(mode, d, &edges, seq.edge_filter, pred);
        }
        if plane != 0 && uv_mode == UV_CFL_PRED {
            let ac = &mut scratch.work.ac;
            clear_buffer(ac, size.area())?;
            tile.cfl_ac(pat, ac);
            let cfl_pred = &mut scratch
                .best
                .first_mut()
                .ok_or(DecodeError::Bitstream)?
                .recon;
            clear_buffer(cfl_pred, size.area())?;
            cfl_predict(
                pred,
                ac,
                *cfl.get(plane - 1).ok_or(DecodeError::Bitstream)?,
                cfl_pred,
            );
            std::mem::swap(pred, cfl_pred);
        }
        if eob != 0 {
            let dequant = &mut scratch.work.dequant;
            clear_buffer(dequant, size.coded().area())?;
            tile.dequantize(size, levels, dequant);
            let residual = &mut scratch.work.residual;
            clear_buffer(residual, size.area())?;
            transform::inverse(size, tx, dequant, residual);
            for (p, &r) in pred.iter_mut().zip(residual.iter()) {
                *p = (i32::from(*p) + r).clamp(0, 255) as u8;
            }
        }
        tile.write_recon(plane, pat, pred);
        *leaf.eob.get_mut(plane).ok_or(DecodeError::Bitstream)? = eob;
    }
    tile.apply_contexts(&leaf);
    tile.record_block(at, skip);
    for (levels, trial) in leaf.levels.iter_mut().zip(&mut scratch.trial) {
        std::mem::swap(levels, &mut trial.levels);
    }
    tile.scratch = scratch;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn coefficients(
    tile: &mut Tile,
    symbols: &mut Symbols<'_, '_>,
    plane: usize,
    at: At,
    y_mode: u8,
    mut tx: TxType,
    levels: &mut [i32],
) -> Result<(TxType, usize)> {
    let sub = usize::from(plane > 0);
    let size = plane_size(at, plane);
    let coded = size.coded();
    let size_ctx = size_ctx(size);
    let (x4, y4) = (at.mi_col >> sub, at.sb_row() >> sub);
    let max_x4 = (tile.mi_cols >> sub).saturating_sub((tile.x0 / MI) >> sub);
    let max_y4 = (tile.mi_rows >> sub).saturating_sub(tile.band_mi_row >> sub);
    let (al, ad, ll, ld) = tile.contexts.plane(plane);
    let ctx = if plane == 0 {
        0
    } else {
        let (mut above, mut left) = (0, 0);
        for k in (0..size.width() / MI).filter(|k| x4 + k < max_x4) {
            above |= al.get(x4 + k).copied().unwrap_or(0) | ad.get(x4 + k).copied().unwrap_or(0);
        }
        for k in (0..size.height() / MI).filter(|k| y4 + k < max_y4) {
            left |= ll.get(y4 + k).copied().unwrap_or(0) | ld.get(y4 + k).copied().unwrap_or(0);
        }
        7 + usize::from(above != 0) + usize::from(left != 0)
    };
    let cdf = tile
        .cdfs
        .txb_skip
        .get_mut(size_ctx)
        .and_then(|r| r.get_mut(ctx))
        .ok_or(DecodeError::Bitstream)?;
    if symbols.read(cdf)? != 0 {
        return Ok((tx, 0));
    }
    if plane == 0 && !dct_only(size) {
        let cdf = tile
            .cdfs
            .tx_type
            .get_mut(size_sqr(size))
            .and_then(|r| r.get_mut(usize::from(y_mode)))
            .ok_or(DecodeError::Bitstream)?;
        tx = match symbols.read(cdf)? {
            1 => TxType::DctDct,
            2 => TxType::AdstAdst,
            3 => TxType::AdstDct,
            4 => TxType::DctAdst,
            _ => return Err(DecodeError::Unsupported),
        };
    }
    let class = (coded.area().ilog2() - 4) as usize;
    let cdf = tile
        .cdfs
        .eob_pt
        .get_mut(class)
        .and_then(|r| r.get_mut(sub))
        .ok_or(DecodeError::Bitstream)?;
    let token = symbols.read(cdf)? + 1;
    let extra = token.saturating_sub(2);
    let mut eob = eob_group_start(token);
    if extra > 0 {
        let cdf = tile
            .cdfs
            .eob_extra
            .get_mut(size_ctx)
            .and_then(|r| r.get_mut(sub))
            .and_then(|r| r.get_mut(token - 3))
            .ok_or(DecodeError::Bitstream)?;
        eob += symbols.read(cdf)? << (extra - 1);
        eob += symbols.literal(extra - 1)?;
    }
    if eob > coded.area() {
        return Err(DecodeError::Bitstream);
    }
    let mut mags = std::mem::take(&mut tile.mags);
    mags.fill(levels, size);
    for c in (0..eob).rev() {
        let pos = usize::from(*size.scan().get(c).ok_or(DecodeError::Bitstream)?);
        let (row, col) = (pos / coded.width(), pos % coded.width());
        let mut level = if c + 1 == eob {
            let ctx = base_eob_ctx(c, coded.area());
            let cdf = tile
                .cdfs
                .base_eob
                .get_mut(size_ctx)
                .and_then(|r| r.get_mut(sub))
                .and_then(|r| r.get_mut(ctx))
                .ok_or(DecodeError::Bitstream)?;
            symbols.read(cdf)? + 1
        } else {
            let ctx = mags.base_ctx(row, col);
            let cdf = tile
                .cdfs
                .base
                .get_mut(size_ctx)
                .and_then(|r| r.get_mut(sub))
                .and_then(|r| r.get_mut(ctx))
                .ok_or(DecodeError::Bitstream)?;
            symbols.read(cdf)?
        };
        if level > 2 {
            let ctx = mags.br_ctx(row, col);
            let cdf = tile
                .cdfs
                .br
                .get_mut(size_ctx.min(3))
                .and_then(|r| r.get_mut(sub))
                .and_then(|r| r.get_mut(ctx))
                .ok_or(DecodeError::Bitstream)?;
            for _ in 0..4 {
                let v = symbols.read(cdf)?;
                level += v;
                if v < 3 {
                    break;
                }
            }
        }
        *levels.get_mut(pos).ok_or(DecodeError::Bitstream)? = level as i32;
        mags.set(row, col, level as u32);
    }
    let dc_ctx = tile.dc_sign_ctx(plane, at);
    for c in 0..eob {
        let pos = usize::from(*size.scan().get(c).ok_or(DecodeError::Bitstream)?);
        let level = levels.get_mut(pos).ok_or(DecodeError::Bitstream)?;
        if *level == 0 {
            continue;
        }
        let negative = if c == 0 {
            let cdf = tile
                .cdfs
                .dc_sign
                .get_mut(sub)
                .and_then(|r| r.get_mut(dc_ctx))
                .ok_or(DecodeError::Bitstream)?;
            symbols.read(cdf)? != 0
        } else {
            symbols.flag()?
        };
        if *level == 15 {
            let mut leading = 0;
            while !symbols.flag()? {
                leading += 1;
                if leading > 19 {
                    return Err(DecodeError::Bitstream);
                }
            }
            *level = 14 + ((1 << leading) + symbols.literal(leading)?) as i32;
        }
        if negative {
            *level = -*level;
        }
    }
    tile.mags = mags;
    Ok((tx, eob))
}

// AV1 specification Intra_Filter_Taps (7.11.2.3), scale 16.
const FILTER_TAPS: [[[i32; 7]; 8]; 5] = [
    [
        [-6, 10, 0, 0, 0, 12, 0],
        [-5, 2, 10, 0, 0, 9, 0],
        [-3, 1, 1, 10, 0, 7, 0],
        [-3, 1, 1, 2, 10, 5, 0],
        [-4, 6, 0, 0, 0, 2, 12],
        [-3, 2, 6, 0, 0, 2, 9],
        [-3, 2, 2, 6, 0, 2, 7],
        [-3, 1, 2, 2, 6, 3, 5],
    ],
    [
        [-10, 16, 0, 0, 0, 10, 0],
        [-6, 0, 16, 0, 0, 6, 0],
        [-4, 0, 0, 16, 0, 4, 0],
        [-2, 0, 0, 0, 16, 2, 0],
        [-10, 16, 0, 0, 0, 0, 10],
        [-6, 0, 16, 0, 0, 0, 6],
        [-4, 0, 0, 16, 0, 0, 4],
        [-2, 0, 0, 0, 16, 0, 2],
    ],
    [
        [-8, 8, 0, 0, 0, 16, 0],
        [-8, 0, 8, 0, 0, 16, 0],
        [-8, 0, 0, 8, 0, 16, 0],
        [-8, 0, 0, 0, 8, 16, 0],
        [-4, 4, 0, 0, 0, 0, 16],
        [-4, 0, 4, 0, 0, 0, 16],
        [-4, 0, 0, 4, 0, 0, 16],
        [-4, 0, 0, 0, 4, 0, 16],
    ],
    [
        [-2, 8, 0, 0, 0, 10, 0],
        [-1, 3, 8, 0, 0, 6, 0],
        [-1, 2, 3, 8, 0, 4, 0],
        [0, 1, 2, 3, 8, 2, 0],
        [-1, 4, 0, 0, 0, 3, 10],
        [-1, 3, 4, 0, 0, 4, 6],
        [-1, 2, 3, 4, 0, 4, 4],
        [-1, 2, 2, 3, 4, 3, 3],
    ],
    [
        [-12, 14, 0, 0, 0, 14, 0],
        [-10, 0, 14, 0, 0, 12, 0],
        [-9, 0, 0, 14, 0, 11, 0],
        [-8, 0, 0, 0, 14, 10, 0],
        [-10, 12, 0, 0, 0, 0, 14],
        [-9, 1, 12, 0, 0, 0, 12],
        [-8, 0, 0, 12, 0, 1, 11],
        [-7, 0, 0, 1, 12, 1, 9],
    ],
];

fn filter_prediction(mode: usize, edges: &Edges, out: &mut [u8]) -> Result<()> {
    let taps = FILTER_TAPS.get(mode).ok_or(DecodeError::Bitstream)?;
    let (w, h) = (edges.w, edges.h);
    for y in (0..h).step_by(2) {
        for x in (0..w).step_by(4) {
            let mut neighbours = [0i32; 7];
            for (i, slot) in neighbours.iter_mut().enumerate() {
                let value = if i < 5 {
                    if y == 0 {
                        edges.above.get(x + i)
                    } else if x == 0 && i == 0 {
                        edges.left.get(y)
                    } else {
                        out.get((y - 1) * w + x + i - 1)
                    }
                } else if x == 0 {
                    edges.left.get(y + i - 4)
                } else {
                    out.get((y + i - 5) * w + x - 1)
                };
                *slot = i32::from(*value.ok_or(DecodeError::Bitstream)?);
            }
            for (i, coeffs) in taps.iter().enumerate() {
                let sum: i32 = coeffs.iter().zip(neighbours).map(|(c, p)| c * p).sum();
                let rounded = sum.signum() * ((sum.abs() + 8) >> 4);
                *out.get_mut((y + i / 4) * w + x + i % 4)
                    .ok_or(DecodeError::Bitstream)? = rounded.clamp(0, 255) as u8;
            }
        }
    }
    Ok(())
}

fn clear_buffer<T: Default + Clone>(buffer: &mut Vec<T>, len: usize) -> Result<()> {
    buffer
        .try_reserve_exact(len.saturating_sub(buffer.len()))
        .map_err(|_| DecodeError::Limit)?;
    buffer.resize(len, T::default());
    buffer.fill(T::default());
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
    use super::*;

    fn encoded(
        width: usize,
        height: usize,
        quality: u8,
        kind: usize,
        tiles: (u32, u32),
    ) -> (Vec<u8>, Reconstruction) {
        let mut pixels = Vec::new();
        let mut seed = 37u64;
        for y in 0..height {
            for x in 0..width {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                let rgb = match kind {
                    0 => [55, 123, 232],
                    1 => [
                        (x * 255 / width) as u8,
                        (y * 255 / height) as u8,
                        ((x + y) * 255 / (width + height)) as u8,
                    ],
                    2 => [(seed >> 16) as u8, (seed >> 24) as u8, (seed >> 32) as u8],
                    _ => [if (x / 3 + y / 5) % 2 == 0 { 0 } else { 255 }; 3],
                };
                pixels.extend(rgb);
            }
        }
        let g = Geometry::tiled(width, height, tiles.0, tiles.1).unwrap();
        let mut e = Encoder::<InOrder>::with_geometry(g, quality, 1).unwrap();
        e.keep_reconstruction();
        e.encode_rows(&pixels).unwrap();
        let (bytes, image) = e.finish_with().unwrap();
        (bytes, image.unwrap())
    }

    #[test]
    fn decoded_planes_match_the_encoder_reconstruction() {
        for (width, height, quality, kind, tiles) in [
            (1, 1, 75, 0, (0, 0)),
            (35, 27, 92, 1, (0, 0)),
            (64, 64, 30, 0, (0, 0)),
            (127, 71, 75, 2, (0, 0)),
            (65, 129, 97, 3, (0, 0)),
            (143, 137, 65, 1, (1, 1)),
            (1, 127, 50, 2, (0, 0)),
            (127, 1, 95, 3, (0, 0)),
        ] {
            let (bytes, expected) = encoded(width, height, quality, kind, tiles);
            let result = decode(&bytes)
                .unwrap_or_else(|error| panic!("{width}x{height} q{quality}: {error:?}"));
            assert_eq!((result.header.width, result.header.height), (width, height));
            for (plane, (actual, expected)) in
                result.planes.iter().zip(&expected.planes).enumerate()
            {
                assert_eq!(
                    actual, expected,
                    "{width}x{height} q{quality} plane {plane}"
                );
            }
        }
    }

    #[test]
    fn decoded_planes_match_dav1d_fixtures() {
        for (name, bytes, expected) in [
            (
                "zero-chroma-delta",
                include_bytes!("../../tests/fixtures/zero-chroma-delta.obu").as_slice(),
                include_bytes!("../../tests/fixtures/zero-chroma-delta.yuv").as_slice(),
            ),
            (
                "direction-delta",
                include_bytes!("../../tests/fixtures/direction-delta.obu").as_slice(),
                include_bytes!("../../tests/fixtures/direction-delta.yuv").as_slice(),
            ),
            (
                "disabled-delta",
                include_bytes!("../../tests/fixtures/disabled-delta.obu").as_slice(),
                include_bytes!("../../tests/fixtures/disabled-delta.yuv").as_slice(),
            ),
            (
                "chroma-delta",
                include_bytes!("../../tests/fixtures/chroma-delta.obu").as_slice(),
                include_bytes!("../../tests/fixtures/chroma-delta.yuv").as_slice(),
            ),
            (
                "libaom-noise-1",
                include_bytes!("../../tests/fixtures/libaom-noise-1.obu").as_slice(),
                include_bytes!("../../tests/fixtures/libaom-noise-1.yuv").as_slice(),
            ),
            (
                "libaom-noise-2",
                include_bytes!("../../tests/fixtures/libaom-noise-2.obu").as_slice(),
                include_bytes!("../../tests/fixtures/libaom-noise-2.yuv").as_slice(),
            ),
            (
                "libaom-noise-4",
                include_bytes!("../../tests/fixtures/libaom-noise-4.obu").as_slice(),
                include_bytes!("../../tests/fixtures/libaom-noise-4.yuv").as_slice(),
            ),
            (
                "libaom-noise-5",
                include_bytes!("../../tests/fixtures/libaom-noise-5.obu").as_slice(),
                include_bytes!("../../tests/fixtures/libaom-noise-5.yuv").as_slice(),
            ),
            (
                "libaom",
                include_bytes!("../../tests/fixtures/libaom.obu").as_slice(),
                include_bytes!("../../tests/fixtures/libaom.yuv").as_slice(),
            ),
            (
                "single",
                include_bytes!("../../tests/fixtures/single.obu").as_slice(),
                include_bytes!("../../tests/fixtures/single.yuv").as_slice(),
            ),
            (
                "ramp",
                include_bytes!("../../tests/fixtures/ramp.obu").as_slice(),
                include_bytes!("../../tests/fixtures/ramp.yuv").as_slice(),
            ),
            (
                "solid64",
                include_bytes!("../../tests/fixtures/solid64.obu").as_slice(),
                include_bytes!("../../tests/fixtures/solid64.yuv").as_slice(),
            ),
            (
                "noise",
                include_bytes!("../../tests/fixtures/noise.obu").as_slice(),
                include_bytes!("../../tests/fixtures/noise.yuv").as_slice(),
            ),
            (
                "edges",
                include_bytes!("../../tests/fixtures/edges.obu").as_slice(),
                include_bytes!("../../tests/fixtures/edges.yuv").as_slice(),
            ),
            (
                "tiles",
                include_bytes!("../../tests/fixtures/tiles.obu").as_slice(),
                include_bytes!("../../tests/fixtures/tiles.yuv").as_slice(),
            ),
            (
                "vertical",
                include_bytes!("../../tests/fixtures/vertical.obu").as_slice(),
                include_bytes!("../../tests/fixtures/vertical.yuv").as_slice(),
            ),
            (
                "horizontal",
                include_bytes!("../../tests/fixtures/horizontal.obu").as_slice(),
                include_bytes!("../../tests/fixtures/horizontal.yuv").as_slice(),
            ),
        ] {
            let image = decode(bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(image.planes.concat(), expected, "{name}");
        }
    }

    #[test]
    fn malformed_streams_and_work_admission_are_bounded() {
        let (bytes, _) = encoded(1, 1, 75, 0, (0, 0));
        for n in 0..bytes.len() {
            assert!(decode(&bytes[..n]).is_err(), "prefix {n}");
        }
        let mut seed = 191u64;
        for _ in 0..1500 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let mut mutated = bytes.clone();
            let at = (seed >> 17) as usize % mutated.len();
            mutated[at] ^= (seed >> 32) as u8;
            if let Ok(image) = decode(&mutated) {
                assert_eq!(
                    image.planes[0].len(),
                    image.header.width * image.header.height
                );
                assert!(image.planes[0].len() <= MAX_IMAGE_PIXELS);
            }
        }
        let seq = sequence_header(&Geometry::new(16384, 16384).unwrap());
        assert!(matches!(sequence(&seq), Err(DecodeError::Limit)));
        let mut budget = 0;
        let mut symbols = Symbols::new(&[0x80], &mut budget, true).unwrap();
        assert_eq!(symbols.flag(), Err(DecodeError::Limit));
    }
}

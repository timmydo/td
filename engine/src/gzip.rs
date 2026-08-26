//! Minimal gzip and raw-DEFLATE reader shared by td's control plane.
//!
//! Kept in-tree and std-only for the same reason as `tar.rs`: source seed
//! preparation should not require host unpackers or a Rust crate dependency.

use crate::crc32::crc32;
use std::fs::File;
use std::io::Read;
use std::path::Path;

const MAX_BITS: usize = 15;
const MAX_GZIP_INPUT_BYTES: u64 = 257 * 1024 * 1024;
const MAX_GZIP_OUTPUT_BYTES: usize = 256 * 1024 * 1024;
// Normal source archives are one member; concatenation stays finite too.
const MAX_GZIP_MEMBERS: usize = 4_096;
// Leaves more than 9x headroom over 256 MiB of gzip --rsyncable output.
const MAX_DEFLATE_BLOCKS: usize = 1 << 20;

const LENGTH_BASE: [usize; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LENGTH_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [usize; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];
const CODE_LENGTH_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

pub fn decompress_file(path: &Path) -> Result<Vec<u8>, String> {
    let file = File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let len = file
        .metadata()
        .map_err(|e| format!("stat {}: {e}", path.display()))?
        .len();
    if len > MAX_GZIP_INPUT_BYTES {
        return Err(format!(
            "gzip input {} is too large: {len} bytes exceeds {} byte limit",
            path.display(),
            MAX_GZIP_INPUT_BYTES
        ));
    }
    let cap =
        usize::try_from(len).map_err(|_| "gzip input length did not fit usize".to_string())?;
    let mut input = Vec::with_capacity(cap);
    let mut limited = file.take(
        MAX_GZIP_INPUT_BYTES
            .checked_add(1)
            .ok_or_else(|| "gzip input limit overflow".to_string())?,
    );
    limited
        .read_to_end(&mut input)
        .map_err(|e| format!("read {}: {e}", path.display()))?;
    let read_len =
        u64::try_from(input.len()).map_err(|_| "gzip input length did not fit u64".to_string())?;
    if read_len > MAX_GZIP_INPUT_BYTES {
        return Err(format!(
            "gzip input {} grew past {} byte limit while reading",
            path.display(),
            MAX_GZIP_INPUT_BYTES
        ));
    }
    decompress_bytes(&input).map_err(|e| format!("decompress {}: {e}", path.display()))
}

pub fn decompress_bytes(input: &[u8]) -> Result<Vec<u8>, String> {
    decompress_bytes_with_limit(input, MAX_GZIP_OUTPUT_BYTES)
}

fn decompress_bytes_with_limit(input: &[u8], max_output_bytes: usize) -> Result<Vec<u8>, String> {
    require_input_size(input.len(), "gzip")?;
    if input.is_empty() {
        return Err("empty gzip stream".to_string());
    }
    let mut pos = 0usize;
    let mut out = Vec::new();
    let mut members = 0usize;
    let mut blocks = 0usize;
    while pos < input.len() {
        members = members
            .checked_add(1)
            .ok_or_else(|| "gzip member count overflow".to_string())?;
        if members > MAX_GZIP_MEMBERS {
            return Err(format!(
                "gzip member count exceeds {MAX_GZIP_MEMBERS} member limit"
            ));
        }
        let payload_start = parse_gzip_header(input, pos)?;
        let payload = range_from(input, payload_start)?;
        let remaining = max_output_bytes
            .checked_sub(out.len())
            .ok_or_else(|| "gzip output exceeded configured limit".to_string())?;
        let remaining_blocks = MAX_DEFLATE_BLOCKS
            .checked_sub(blocks)
            .ok_or_else(|| "DEFLATE block budget underflow".to_string())?;
        let (member, consumed, member_blocks) =
            decompress_raw_prefix_bounded(payload, remaining, remaining_blocks)?;
        blocks = blocks
            .checked_add(member_blocks)
            .ok_or_else(|| "DEFLATE block count overflow".to_string())?;
        let trailer_pos = payload_start
            .checked_add(consumed)
            .ok_or_else(|| "gzip member offset overflow".to_string())?;
        let trailer = range(input, trailer_pos, 8)?;
        let want_crc = u32_le(trailer, 0)?;
        let want_size = u32_le(trailer, 4)?;
        let got_crc = crc32(&member);
        if got_crc != want_crc {
            return Err(format!(
                "gzip CRC mismatch: got {got_crc:08x}, want {want_crc:08x}"
            ));
        }
        let got_size = u32::try_from(member.len())
            .map_err(|_| "gzip member size did not fit u32".to_string())?;
        if got_size != want_size {
            return Err(format!(
                "gzip size mismatch: got {got_size}, want {want_size}"
            ));
        }
        let new_len = out
            .len()
            .checked_add(member.len())
            .ok_or_else(|| "gzip output length overflow".to_string())?;
        if new_len > max_output_bytes {
            return Err(format!("gzip output exceeds {max_output_bytes} byte limit"));
        }
        out.extend_from_slice(&member);
        pos = trailer_pos
            .checked_add(8)
            .ok_or_else(|| "gzip trailer offset overflow".to_string())?;
    }
    Ok(out)
}

/// Inflate one raw DEFLATE stream at the beginning of `input`.
///
/// The returned byte count stops after the byte containing the final block,
/// allowing a framing format such as gzip to consume its own trailer.
pub fn decompress_raw_prefix(
    input: &[u8],
    max_output_bytes: usize,
) -> Result<(Vec<u8>, usize), String> {
    let (output, consumed, _) =
        decompress_raw_prefix_bounded(input, max_output_bytes, MAX_DEFLATE_BLOCKS)?;
    Ok((output, consumed))
}

fn decompress_raw_prefix_bounded(
    input: &[u8],
    max_output_bytes: usize,
    max_blocks: usize,
) -> Result<(Vec<u8>, usize, usize), String> {
    require_input_size(input.len(), "raw DEFLATE")?;
    if input.is_empty() {
        return Err("empty raw DEFLATE stream".to_string());
    }
    let mut bits = BitReader::new(input);
    let mut output = Vec::new();
    let blocks = inflate(&mut bits, &mut output, max_output_bytes, max_blocks)?;
    Ok((output, bits.byte_position(), blocks))
}

/// Inflate exactly one raw DEFLATE stream with one exact decoded size.
pub fn decompress_raw_exact(
    input: &[u8],
    expected_output_bytes: usize,
    max_output_bytes: usize,
) -> Result<Vec<u8>, String> {
    if expected_output_bytes > max_output_bytes {
        return Err(format!(
            "raw DEFLATE output declares {expected_output_bytes} bytes; limit is {max_output_bytes}"
        ));
    }
    let (output, consumed) = match decompress_raw_prefix(input, expected_output_bytes) {
        Err(error) if error == output_limit_error(expected_output_bytes) => {
            return Err(format!(
                "raw DEFLATE output exceeds declared {expected_output_bytes} bytes"
            ));
        }
        result => result?,
    };
    if consumed != input.len() {
        return Err(format!(
            "raw DEFLATE stream consumed {consumed} of {} bytes",
            input.len()
        ));
    }
    if output.len() != expected_output_bytes {
        return Err(format!(
            "raw DEFLATE output is {} bytes; expected {expected_output_bytes}",
            output.len()
        ));
    }
    Ok(output)
}

fn require_input_size(len: usize, what: &str) -> Result<(), String> {
    let len = u64::try_from(len).map_err(|_| format!("{what} input length did not fit u64"))?;
    if len > MAX_GZIP_INPUT_BYTES {
        return Err(format!(
            "{what} input is too large: {len} bytes exceeds {MAX_GZIP_INPUT_BYTES} byte limit"
        ));
    }
    Ok(())
}

fn parse_gzip_header(input: &[u8], start: usize) -> Result<usize, String> {
    let fixed = range(input, start, 10)?;
    if byte(fixed, 0)? != 0x1f || byte(fixed, 1)? != 0x8b {
        return Err("bad gzip magic".to_string());
    }
    if byte(fixed, 2)? != 8 {
        return Err("gzip member is not DEFLATE-compressed".to_string());
    }
    let flags = byte(fixed, 3)?;
    if flags & 0xe0 != 0 {
        return Err(format!("gzip reserved flag bits set: 0x{flags:02x}"));
    }
    let mut pos = start
        .checked_add(10)
        .ok_or_else(|| "gzip header offset overflow".to_string())?;
    if flags & 0x04 != 0 {
        let xlen = usize::from(u16_le(input, pos)?);
        pos = pos
            .checked_add(2)
            .and_then(|p| p.checked_add(xlen))
            .ok_or_else(|| "gzip extra field offset overflow".to_string())?;
        let _ = range(input, start, pos.saturating_sub(start))?;
    }
    if flags & 0x08 != 0 {
        pos = skip_zero_terminated(input, pos, "gzip original filename")?;
    }
    if flags & 0x10 != 0 {
        pos = skip_zero_terminated(input, pos, "gzip comment")?;
    }
    if flags & 0x02 != 0 {
        let got = u16_le(input, pos)?;
        let header = range(input, start, pos.saturating_sub(start))?;
        let want = u16::try_from(crc32(header) & 0xffff)
            .map_err(|_| "gzip header CRC did not fit u16".to_string())?;
        if got != want {
            return Err(format!(
                "gzip header CRC mismatch: got {got:04x}, want {want:04x}"
            ));
        }
        pos = pos
            .checked_add(2)
            .ok_or_else(|| "gzip header CRC offset overflow".to_string())?;
    }
    Ok(pos)
}

fn inflate(
    bits: &mut BitReader<'_>,
    out: &mut Vec<u8>,
    max_output_bytes: usize,
    max_blocks: usize,
) -> Result<usize, String> {
    let (fixed_lit, fixed_dist) = fixed_trees()?;
    let mut blocks = 0usize;
    loop {
        blocks = blocks
            .checked_add(1)
            .ok_or_else(|| "DEFLATE block count overflow".to_string())?;
        if blocks > max_blocks {
            return Err(format!(
                "DEFLATE block count exceeds {max_blocks} block limit"
            ));
        }
        let final_block = bits.read_bits(1)? != 0;
        match bits.read_bits(2)? {
            0 => inflate_stored(bits, out, max_output_bytes)?,
            1 => {
                inflate_huffman(bits, out, &fixed_lit, &fixed_dist, max_output_bytes)?;
            }
            2 => {
                let (lit, dist) = dynamic_trees(bits)?;
                inflate_huffman(bits, out, &lit, &dist, max_output_bytes)?;
            }
            _ => return Err("reserved DEFLATE block type".to_string()),
        }
        if final_block {
            break;
        }
    }
    Ok(blocks)
}

fn inflate_stored(
    bits: &mut BitReader<'_>,
    out: &mut Vec<u8>,
    max_output_bytes: usize,
) -> Result<(), String> {
    bits.align_to_byte();
    let len = bits.read_aligned_u16()?;
    let nlen = bits.read_aligned_u16()?;
    if len != !nlen {
        return Err("stored DEFLATE block length check failed".to_string());
    }
    for _ in 0..usize::from(len) {
        let b = bits.read_aligned_u8()?;
        push_output(out, b, max_output_bytes)?;
    }
    Ok(())
}

fn inflate_huffman(
    bits: &mut BitReader<'_>,
    out: &mut Vec<u8>,
    lit: &Huffman,
    dist: &Huffman,
    max_output_bytes: usize,
) -> Result<(), String> {
    loop {
        let sym = lit.decode(bits)?;
        match sym {
            0..=255 => {
                let byte = u8::try_from(sym).map_err(|_| "literal did not fit u8".to_string())?;
                push_output(out, byte, max_output_bytes)?;
            }
            256 => break,
            257..=285 => {
                let len = decode_length(sym, bits)?;
                let dist_sym = dist.decode(bits)?;
                let distance = decode_distance(dist_sym, bits)?;
                copy_match(out, distance, len, max_output_bytes)?;
            }
            _ => return Err(format!("invalid DEFLATE literal/length symbol {sym}")),
        }
    }
    Ok(())
}

fn fixed_trees() -> Result<(Huffman, Huffman), String> {
    let mut lit_lengths = Vec::with_capacity(288);
    for symbol in 0..288 {
        let len = match symbol {
            0..=143 => 8,
            144..=255 => 9,
            256..=279 => 7,
            _ => 8,
        };
        lit_lengths.push(len);
    }
    let dist_lengths = vec![5u8; 32];
    Ok((
        Huffman::from_lengths(&lit_lengths)?,
        Huffman::from_lengths(&dist_lengths)?,
    ))
}

fn dynamic_trees(bits: &mut BitReader<'_>) -> Result<(Huffman, Huffman), String> {
    let hlit = usize::from(bits.read_bits(5)?)
        .checked_add(257)
        .ok_or_else(|| "HLIT overflow".to_string())?;
    if hlit > 286 {
        return Err(format!("reserved DEFLATE HLIT value {hlit}"));
    }
    let hdist = usize::from(bits.read_bits(5)?)
        .checked_add(1)
        .ok_or_else(|| "HDIST overflow".to_string())?;
    if hdist > 30 {
        return Err(format!("reserved DEFLATE HDIST value {hdist}"));
    }
    let hclen = usize::from(bits.read_bits(4)?)
        .checked_add(4)
        .ok_or_else(|| "HCLEN overflow".to_string())?;

    let mut code_lengths = vec![0u8; 19];
    for index in CODE_LENGTH_ORDER.iter().take(hclen) {
        let len = u8::try_from(bits.read_bits(3)?)
            .map_err(|_| "code-length length did not fit u8".to_string())?;
        let slot = code_lengths
            .get_mut(*index)
            .ok_or_else(|| "code-length order index out of bounds".to_string())?;
        *slot = len;
    }
    let code_tree = Huffman::from_lengths(&code_lengths)?;
    let total = hlit
        .checked_add(hdist)
        .ok_or_else(|| "dynamic Huffman length count overflow".to_string())?;
    let mut lengths = Vec::with_capacity(total);
    while lengths.len() < total {
        let sym = code_tree.decode(bits)?;
        match sym {
            0..=15 => {
                let len = u8::try_from(sym)
                    .map_err(|_| "decoded code length did not fit u8".to_string())?;
                lengths.push(len);
            }
            16 => {
                let prev = *lengths
                    .last()
                    .ok_or_else(|| "repeat code length without previous length".to_string())?;
                let count = usize::from(bits.read_bits(2)?)
                    .checked_add(3)
                    .ok_or_else(|| "repeat length overflow".to_string())?;
                push_repeated(&mut lengths, prev, count, total)?;
            }
            17 => {
                let count = usize::from(bits.read_bits(3)?)
                    .checked_add(3)
                    .ok_or_else(|| "zero repeat length overflow".to_string())?;
                push_repeated(&mut lengths, 0, count, total)?;
            }
            18 => {
                let count = usize::from(bits.read_bits(7)?)
                    .checked_add(11)
                    .ok_or_else(|| "long zero repeat length overflow".to_string())?;
                push_repeated(&mut lengths, 0, count, total)?;
            }
            _ => return Err(format!("invalid code-length symbol {sym}")),
        }
    }
    let lit_lengths = lengths
        .get(..hlit)
        .ok_or_else(|| "literal length slice out of bounds".to_string())?;
    if lit_lengths.get(256).copied().unwrap_or(0) == 0 {
        return Err("dynamic Huffman literal tree lacks end-of-block code".to_string());
    }
    let dist_lengths = lengths
        .get(hlit..)
        .ok_or_else(|| "distance length slice out of bounds".to_string())?;
    Ok((
        Huffman::from_lengths(lit_lengths)?,
        Huffman::from_lengths(dist_lengths)?,
    ))
}

fn push_repeated(
    lengths: &mut Vec<u8>,
    value: u8,
    count: usize,
    total: usize,
) -> Result<(), String> {
    let new_len = lengths
        .len()
        .checked_add(count)
        .ok_or_else(|| "repeat count overflow".to_string())?;
    if new_len > total {
        return Err("repeat code length overran dynamic Huffman table".to_string());
    }
    for _ in 0..count {
        lengths.push(value);
    }
    Ok(())
}

fn decode_length(sym: u16, bits: &mut BitReader<'_>) -> Result<usize, String> {
    let idx = usize::from(
        sym.checked_sub(257)
            .ok_or_else(|| "length symbol underflow".to_string())?,
    );
    let base = *LENGTH_BASE
        .get(idx)
        .ok_or_else(|| format!("invalid length symbol {sym}"))?;
    let extra = *LENGTH_EXTRA
        .get(idx)
        .ok_or_else(|| format!("invalid length symbol {sym}"))?;
    Ok(base + usize::from(bits.read_bits(extra)?))
}

fn decode_distance(sym: u16, bits: &mut BitReader<'_>) -> Result<usize, String> {
    let idx = usize::from(sym);
    let base = *DIST_BASE
        .get(idx)
        .ok_or_else(|| format!("invalid distance symbol {sym}"))?;
    let extra = *DIST_EXTRA
        .get(idx)
        .ok_or_else(|| format!("invalid distance symbol {sym}"))?;
    Ok(base + usize::from(bits.read_bits(extra)?))
}

fn push_output(out: &mut Vec<u8>, byte: u8, max_output_bytes: usize) -> Result<(), String> {
    if out.len() >= max_output_bytes {
        return Err(output_limit_error(max_output_bytes));
    }
    out.push(byte);
    Ok(())
}

fn copy_match(
    out: &mut Vec<u8>,
    distance: usize,
    len: usize,
    max_output_bytes: usize,
) -> Result<(), String> {
    if distance == 0 || distance > out.len() {
        return Err(format!("invalid DEFLATE distance {distance}"));
    }
    let new_len = out
        .len()
        .checked_add(len)
        .ok_or_else(|| "DEFLATE output length overflow".to_string())?;
    if new_len > max_output_bytes {
        return Err(output_limit_error(max_output_bytes));
    }
    for _ in 0..len {
        let src = out
            .len()
            .checked_sub(distance)
            .ok_or_else(|| "DEFLATE copy distance underflow".to_string())?;
        let b = *out
            .get(src)
            .ok_or_else(|| "DEFLATE copy source out of bounds".to_string())?;
        out.push(b);
    }
    Ok(())
}

fn output_limit_error(max_output_bytes: usize) -> String {
    format!("DEFLATE output exceeds {max_output_bytes} byte limit")
}

struct BitReader<'a> {
    input: &'a [u8],
    pos: usize,
    bits: u32,
    bit_count: u8,
}

impl<'a> BitReader<'a> {
    fn new(input: &'a [u8]) -> BitReader<'a> {
        BitReader {
            input,
            pos: 0,
            bits: 0,
            bit_count: 0,
        }
    }

    fn byte_position(&self) -> usize {
        self.pos
    }

    fn read_bits(&mut self, mut n: u8) -> Result<u16, String> {
        if n > 16 {
            return Err(format!("cannot read {n} bits at once"));
        }
        let mut out = 0u32;
        let mut shift = 0u8;
        while n > 0 {
            if self.bit_count == 0 {
                self.bits = u32::from(
                    *self
                        .input
                        .get(self.pos)
                        .ok_or_else(|| "truncated DEFLATE stream".to_string())?,
                );
                self.pos = self
                    .pos
                    .checked_add(1)
                    .ok_or_else(|| "DEFLATE byte position overflow".to_string())?;
                self.bit_count = 8;
            }
            let take = n.min(self.bit_count);
            let mask = (1u32 << take) - 1;
            out |= (self.bits & mask) << shift;
            self.bits >>= take;
            self.bit_count -= take;
            n -= take;
            shift = shift
                .checked_add(take)
                .ok_or_else(|| "bit shift overflow".to_string())?;
        }
        u16::try_from(out).map_err(|_| "bit value did not fit u16".to_string())
    }

    fn align_to_byte(&mut self) {
        self.bits = 0;
        self.bit_count = 0;
    }

    fn read_aligned_u8(&mut self) -> Result<u8, String> {
        if self.bit_count != 0 {
            return Err("internal error: aligned read with pending bits".to_string());
        }
        let b = *self
            .input
            .get(self.pos)
            .ok_or_else(|| "truncated DEFLATE stored block".to_string())?;
        self.pos = self
            .pos
            .checked_add(1)
            .ok_or_else(|| "DEFLATE byte position overflow".to_string())?;
        Ok(b)
    }

    fn read_aligned_u16(&mut self) -> Result<u16, String> {
        let lo = u16::from(self.read_aligned_u8()?);
        let hi = u16::from(self.read_aligned_u8()?);
        Ok(lo | (hi << 8))
    }
}

struct Huffman {
    counts: [u16; MAX_BITS + 1],
    symbols: Vec<u16>,
}

impl Huffman {
    fn from_lengths(lengths: &[u8]) -> Result<Huffman, String> {
        let mut counts = [0u16; MAX_BITS + 1];
        for len in lengths {
            if usize::from(*len) > MAX_BITS {
                return Err(format!("Huffman code length {len} exceeds {MAX_BITS}"));
            }
            if *len != 0 {
                let slot = counts
                    .get_mut(usize::from(*len))
                    .ok_or_else(|| "Huffman length count out of bounds".to_string())?;
                *slot = slot
                    .checked_add(1)
                    .ok_or_else(|| "Huffman length count overflow".to_string())?;
            }
        }

        let mut left = 1i32;
        for bits in 1..=MAX_BITS {
            left <<= 1;
            left -= i32::from(
                *counts
                    .get(bits)
                    .ok_or_else(|| "Huffman count index out of bounds".to_string())?,
            );
            if left < 0 {
                return Err("oversubscribed Huffman tree".to_string());
            }
        }

        let symbol_count = counts.iter().try_fold(0usize, |total, count| {
            total
                .checked_add(usize::from(*count))
                .ok_or_else(|| "Huffman symbol count overflow".to_string())
        })?;
        let mut offsets = [0usize; MAX_BITS + 1];
        for len in 1..MAX_BITS {
            let previous = *offsets
                .get(len)
                .ok_or_else(|| "Huffman offset index out of bounds".to_string())?;
            let count = usize::from(
                *counts
                    .get(len)
                    .ok_or_else(|| "Huffman count index out of bounds".to_string())?,
            );
            let next = previous
                .checked_add(count)
                .ok_or_else(|| "Huffman symbol offset overflow".to_string())?;
            let slot = offsets
                .get_mut(len + 1)
                .ok_or_else(|| "Huffman offset index out of bounds".to_string())?;
            *slot = next;
        }

        let mut symbols = vec![0u16; symbol_count];
        for (symbol, len) in lengths.iter().enumerate() {
            if *len == 0 {
                continue;
            }
            let idx = usize::from(*len);
            let offset = offsets
                .get_mut(idx)
                .ok_or_else(|| "Huffman symbol offset out of bounds".to_string())?;
            let at = *offset;
            *offset = offset
                .checked_add(1)
                .ok_or_else(|| "Huffman symbol offset overflow".to_string())?;
            let symbol =
                u16::try_from(symbol).map_err(|_| "Huffman symbol did not fit u16".to_string())?;
            let slot = symbols
                .get_mut(at)
                .ok_or_else(|| "Huffman symbol table out of bounds".to_string())?;
            *slot = symbol;
        }
        Ok(Huffman { counts, symbols })
    }

    fn decode(&self, bits: &mut BitReader<'_>) -> Result<u16, String> {
        self.decode_with_steps(bits).map(|(symbol, _)| symbol)
    }

    fn decode_with_steps(&self, bits: &mut BitReader<'_>) -> Result<(u16, usize), String> {
        let mut code = 0u32;
        let mut first = 0u32;
        let mut symbol_base = 0usize;
        for len in 1..=MAX_BITS {
            code |= u32::from(bits.read_bits(1)?);
            let count = u32::from(
                *self
                    .counts
                    .get(len)
                    .ok_or_else(|| "Huffman count index out of bounds".to_string())?,
            );
            let end = first
                .checked_add(count)
                .ok_or_else(|| "Huffman decode range overflow".to_string())?;
            if code < end {
                let within = usize::try_from(
                    code.checked_sub(first)
                        .ok_or_else(|| "Huffman decode index underflow".to_string())?,
                )
                .map_err(|_| "Huffman decode index did not fit usize".to_string())?;
                let at = symbol_base
                    .checked_add(within)
                    .ok_or_else(|| "Huffman symbol index overflow".to_string())?;
                let symbol = *self
                    .symbols
                    .get(at)
                    .ok_or_else(|| "Huffman symbol index out of bounds".to_string())?;
                return Ok((symbol, len));
            }
            symbol_base = symbol_base
                .checked_add(
                    usize::try_from(count)
                        .map_err(|_| "Huffman length count did not fit usize".to_string())?,
                )
                .ok_or_else(|| "Huffman symbol base overflow".to_string())?;
            first = end
                .checked_shl(1)
                .ok_or_else(|| "Huffman first-code shift overflow".to_string())?;
            code = code
                .checked_shl(1)
                .ok_or_else(|| "Huffman decode shift overflow".to_string())?;
        }
        Err("invalid Huffman code".to_string())
    }
}

fn byte(input: &[u8], pos: usize) -> Result<u8, String> {
    input
        .get(pos)
        .copied()
        .ok_or_else(|| "unexpected EOF".to_string())
}

fn range(input: &[u8], start: usize, len: usize) -> Result<&[u8], String> {
    let end = start
        .checked_add(len)
        .ok_or_else(|| "range offset overflow".to_string())?;
    input
        .get(start..end)
        .ok_or_else(|| "unexpected EOF".to_string())
}

fn range_from(input: &[u8], start: usize) -> Result<&[u8], String> {
    input
        .get(start..)
        .ok_or_else(|| "range start out of bounds".to_string())
}

fn u16_le(input: &[u8], pos: usize) -> Result<u16, String> {
    let bytes = range(input, pos, 2)?;
    Ok(u16::from(byte(bytes, 0)?) | (u16::from(byte(bytes, 1)?) << 8))
}

fn u32_le(input: &[u8], pos: usize) -> Result<u32, String> {
    let bytes = range(input, pos, 4)?;
    Ok(u32::from(byte(bytes, 0)?)
        | (u32::from(byte(bytes, 1)?) << 8)
        | (u32::from(byte(bytes, 2)?) << 16)
        | (u32::from(byte(bytes, 3)?) << 24))
}

fn skip_zero_terminated(input: &[u8], pos: usize, what: &str) -> Result<usize, String> {
    let tail = range_from(input, pos)?;
    for (offset, b) in tail.iter().enumerate() {
        if *b == 0 {
            return pos
                .checked_add(offset)
                .and_then(|p| p.checked_add(1))
                .ok_or_else(|| format!("{what} offset overflow"));
        }
    }
    Err(format!("{what} is not nul-terminated"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decompresses_stored_gzip_member() {
        let gz = [
            0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0x01, 0x06, 0x00, 0xf9,
            0xff, b'h', b'e', b'l', b'l', b'o', b'\n', 0x20, 0x30, 0x3a, 0x36, 0x06, 0x00, 0x00,
            0x00,
        ];
        assert_eq!(decompress_bytes(&gz).unwrap(), b"hello\n");
    }

    #[test]
    fn decompresses_fixed_huffman_gzip_member() {
        let gz = hex_bytes("1f8b0800000000000203cb48cdc9c957c84027b9000088590b18000000");
        assert_eq!(decompress_bytes(&gz).unwrap(), b"hello hello hello hello\n");
    }

    #[test]
    fn decompresses_dynamic_huffman_gzip_member() {
        let gz = hex_bytes(
            "1f8b0800000000000203edcac90182401000b0bf554c6b22878a32b0ec7a55af7d987752cf436ced729aa32bf95c62cc575cdb7ddd231f4389fae3dbf1f38e3ea743755dd7755dd7755dd7755dd7755dd7755dd7755dd7755dd775ffee7e01de757bf560220000",
        );
        let want = ("the quick brown fox jumps over the lazy dog\n").repeat(200);
        assert_eq!(decompress_bytes(&gz).unwrap(), want.as_bytes());
    }

    #[test]
    fn raw_stream_reports_its_exact_boundary() {
        let raw = hex_bytes("cb48cdc9c957c84027b900");
        let mut framed = raw.clone();
        framed.extend_from_slice(b"trailer");

        let (decoded, consumed) = decompress_raw_prefix(&framed, 64).unwrap();

        assert_eq!(decoded, b"hello hello hello hello\n");
        assert_eq!(consumed, raw.len());
    }

    #[test]
    fn exact_raw_stream_refuses_wrong_sizes_limits_and_trailing_bytes() {
        let raw = hex_bytes("cb48cdc9c957c84027b900");
        assert_eq!(
            decompress_raw_exact(&raw, 24, 24).unwrap(),
            b"hello hello hello hello\n"
        );

        let under_declared = decompress_raw_exact(&raw, 23, 24).unwrap_err();
        assert!(
            under_declared.contains("exceeds declared 23 bytes"),
            "got: {under_declared}"
        );

        let over_declared = decompress_raw_exact(&raw, 25, 25).unwrap_err();
        assert!(
            over_declared.contains("output is 24 bytes; expected 25"),
            "got: {over_declared}"
        );

        let over_limit = decompress_raw_exact(&raw, 24, 23).unwrap_err();
        assert!(over_limit.contains("limit is 23"), "got: {over_limit}");

        let mut trailing = raw;
        trailing.push(0);
        let trailing_error = decompress_raw_exact(&trailing, 24, 24).unwrap_err();
        assert!(
            trailing_error.contains("consumed 11 of 12"),
            "got: {trailing_error}"
        );
    }

    #[test]
    fn raw_stream_refuses_reserved_hlit_before_building_trees() {
        let error = decompress_raw_exact(&[0xf5], 0, 0).unwrap_err();

        assert!(error.contains("reserved DEFLATE HLIT value 287"));
    }

    #[test]
    fn raw_stream_refuses_reserved_hdist_before_building_trees() {
        let error = decompress_raw_exact(&[0x05, 0x1e], 0, 0).unwrap_err();

        assert!(error.contains("reserved DEFLATE HDIST value 31"));
    }

    #[test]
    fn wide_huffman_bucket_lookup_is_bounded_by_code_length() {
        let tree = Huffman::from_lengths(&vec![8; 256]).unwrap();
        let mut bits = BitReader::new(&[0xff]);

        let (symbol, steps) = tree.decode_with_steps(&mut bits).unwrap();

        assert_eq!(symbol, 255);
        assert_eq!(steps, 8);
        assert!(steps <= MAX_BITS);
    }

    #[test]
    fn raw_stream_has_one_global_block_work_budget() {
        let mut raw = Vec::new();
        let mut bit_len = 0usize;
        for _ in 0..=MAX_DEFLATE_BLOCKS {
            push_test_bits(&mut raw, &mut bit_len, 0, 1);
            push_test_bits(&mut raw, &mut bit_len, 1, 2);
            push_test_bits(&mut raw, &mut bit_len, 0, 7);
        }

        let error = decompress_raw_prefix(&raw, 0).unwrap_err();

        assert!(error.contains("exceeds 1048576 block limit"));
    }

    #[test]
    fn concatenated_gzip_member_count_is_bounded() {
        let empty = hex_bytes("1f8b080000000000000303000000000000000000");
        let mut gzip = Vec::new();
        for _ in 0..=MAX_GZIP_MEMBERS {
            gzip.extend_from_slice(&empty);
        }

        let error = decompress_bytes(&gzip).unwrap_err();

        assert!(error.contains("exceeds 4096 member limit"));
    }

    #[test]
    fn concatenated_gzip_members_share_one_block_budget() {
        const BLOCKS_PER_MEMBER: usize = 1_024;
        let raw = empty_fixed_raw(BLOCKS_PER_MEMBER);
        let mut member = hex_bytes("1f8b0800000000000003");
        member.extend_from_slice(&raw);
        member.extend_from_slice(&[0; 8]);
        let mut gzip = Vec::new();
        for _ in 0..=(MAX_DEFLATE_BLOCKS / BLOCKS_PER_MEMBER) {
            gzip.extend_from_slice(&member);
        }

        let error = decompress_bytes(&gzip).unwrap_err();

        assert!(error.contains("DEFLATE block count exceeds"));
    }

    #[test]
    fn in_memory_inputs_are_bounded_before_parsing() {
        let limit = usize::try_from(MAX_GZIP_INPUT_BYTES).unwrap();

        assert!(require_input_size(limit, "test").is_ok());
        assert!(require_input_size(limit + 1, "test")
            .unwrap_err()
            .contains("input is too large"));
    }

    #[test]
    fn crc_mismatch_errors() {
        let mut gz = [
            0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0x01, 0x06, 0x00, 0xf9,
            0xff, b'h', b'e', b'l', b'l', b'o', b'\n', 0x20, 0x30, 0x3a, 0x36, 0x06, 0x00, 0x00,
            0x00,
        ];
        gz[21] ^= 0xff;
        let err = decompress_bytes(&gz).unwrap_err();
        assert!(err.contains("CRC mismatch"), "got: {err}");
    }

    #[test]
    fn output_limit_errors_before_inflating_past_bound() {
        let gz = [
            0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0x01, 0x06, 0x00, 0xf9,
            0xff, b'h', b'e', b'l', b'l', b'o', b'\n', 0x20, 0x30, 0x3a, 0x36, 0x06, 0x00, 0x00,
            0x00,
        ];

        let err = decompress_bytes_with_limit(&gz, 5).unwrap_err();

        assert!(
            err.contains("DEFLATE output exceeds 5 byte limit"),
            "got: {err}"
        );
    }

    fn hex_bytes(hex: &str) -> Vec<u8> {
        let mut out = Vec::new();
        let mut chars = hex.as_bytes().chunks(2);
        while let Some(pair) = chars.next() {
            let s = std::str::from_utf8(pair).unwrap();
            out.push(u8::from_str_radix(s, 16).unwrap());
        }
        out
    }

    fn push_test_bits(output: &mut Vec<u8>, bit_len: &mut usize, value: u16, count: u8) {
        for shift in 0..count {
            if *bit_len % 8 == 0 {
                output.push(0);
            }
            if value & (1u16 << shift) != 0 {
                let byte = output.last_mut().unwrap();
                *byte |= 1u8 << (*bit_len % 8);
            }
            *bit_len += 1;
        }
    }

    fn empty_fixed_raw(blocks: usize) -> Vec<u8> {
        let mut raw = Vec::new();
        let mut bit_len = 0usize;
        for index in 0..blocks {
            push_test_bits(&mut raw, &mut bit_len, u16::from(index + 1 == blocks), 1);
            push_test_bits(&mut raw, &mut bit_len, 1, 2);
            push_test_bits(&mut raw, &mut bit_len, 0, 7);
        }
        raw
    }
}

//! Minimal gzip/DEFLATE reader for td-builder.
//!
//! Kept in-tree and std-only for the same reason as `tar.rs`: source seed
//! preparation should not require host unpackers or a Rust crate dependency.

use std::fs::File;
use std::io::Read;
use std::path::Path;

const MAX_BITS: usize = 15;
const MAX_GZIP_INPUT_BYTES: u64 = 257 * 1024 * 1024;
const MAX_GZIP_OUTPUT_BYTES: usize = 256 * 1024 * 1024;

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
    if input.is_empty() {
        return Err("empty gzip stream".to_string());
    }
    let mut pos = 0usize;
    let mut out = Vec::new();
    while pos < input.len() {
        let payload_start = parse_gzip_header(input, pos)?;
        let payload = range_from(input, payload_start)?;
        let mut bits = BitReader::new(payload);
        let mut member = Vec::new();
        let remaining = max_output_bytes
            .checked_sub(out.len())
            .ok_or_else(|| "gzip output exceeded configured limit".to_string())?;
        inflate(&mut bits, &mut member, remaining)?;
        let trailer_pos = payload_start
            .checked_add(bits.byte_position())
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
) -> Result<(), String> {
    loop {
        let final_block = bits.read_bits(1)? != 0;
        match bits.read_bits(2)? {
            0 => inflate_stored(bits, out, max_output_bytes)?,
            1 => {
                let (lit, dist) = fixed_trees()?;
                inflate_huffman(bits, out, &lit, &dist, max_output_bytes)?;
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
    Ok(())
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
    let hdist = usize::from(bits.read_bits(5)?)
        .checked_add(1)
        .ok_or_else(|| "HDIST overflow".to_string())?;
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
        return Err(format!("gzip output exceeds {max_output_bytes} byte limit"));
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
        return Err(format!("gzip output exceeds {max_output_bytes} byte limit"));
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
    by_len: Vec<Vec<(u16, u16)>>,
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

        let mut next_code = [0u16; MAX_BITS + 1];
        let mut code = 0u16;
        for bits in 1..=MAX_BITS {
            let prev = *counts
                .get(bits - 1)
                .ok_or_else(|| "Huffman count index out of bounds".to_string())?;
            code = code
                .checked_add(prev)
                .ok_or_else(|| "Huffman code overflow".to_string())?
                .checked_shl(1)
                .ok_or_else(|| "Huffman code shift overflow".to_string())?;
            let slot = next_code
                .get_mut(bits)
                .ok_or_else(|| "Huffman next-code index out of bounds".to_string())?;
            *slot = code;
        }

        let mut by_len = vec![Vec::new(); MAX_BITS + 1];
        for (symbol, len) in lengths.iter().enumerate() {
            if *len == 0 {
                continue;
            }
            let idx = usize::from(*len);
            let code_slot = next_code
                .get_mut(idx)
                .ok_or_else(|| "Huffman code length out of bounds".to_string())?;
            let raw_code = *code_slot;
            *code_slot = code_slot
                .checked_add(1)
                .ok_or_else(|| "Huffman next code overflow".to_string())?;
            let symbol =
                u16::try_from(symbol).map_err(|_| "Huffman symbol did not fit u16".to_string())?;
            let reversed = reverse_bits(raw_code, *len);
            by_len
                .get_mut(idx)
                .ok_or_else(|| "Huffman code bucket out of bounds".to_string())?
                .push((reversed, symbol));
        }
        Ok(Huffman { by_len })
    }

    fn decode(&self, bits: &mut BitReader<'_>) -> Result<u16, String> {
        let mut code = 0u16;
        for len in 1..=MAX_BITS {
            let bit = bits.read_bits(1)?;
            let shift = u32::try_from(len - 1)
                .map_err(|_| "Huffman decode shift did not fit u32".to_string())?;
            code |= bit
                .checked_shl(shift)
                .ok_or_else(|| "Huffman decode shift overflow".to_string())?;
            if let Some(entries) = self.by_len.get(len) {
                for (candidate, symbol) in entries {
                    if *candidate == code {
                        return Ok(*symbol);
                    }
                }
            }
        }
        Err("invalid Huffman code".to_string())
    }
}

fn reverse_bits(mut code: u16, len: u8) -> u16 {
    let mut out = 0u16;
    for _ in 0..len {
        out = (out << 1) | (code & 1);
        code >>= 1;
    }
    out
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

fn crc32(input: &[u8]) -> u32 {
    let table = crc32_table();
    let mut crc = 0xffff_ffffu32;
    for b in input {
        let idx = ((crc ^ u32::from(*b)) & 0xff) as usize;
        let table_value = table.get(idx).copied().unwrap_or(0);
        crc = (crc >> 8) ^ table_value;
    }
    !crc
}

fn crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    for i in 0..256usize {
        let mut crc = u32::try_from(i).unwrap_or(0);
        for _ in 0..8 {
            if crc & 1 == 0 {
                crc >>= 1;
            } else {
                crc = (crc >> 1) ^ 0xedb8_8320;
            }
        }
        if let Some(slot) = table.get_mut(i) {
            *slot = crc;
        }
    }
    table
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
            err.contains("gzip output exceeds 5 byte limit"),
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
}

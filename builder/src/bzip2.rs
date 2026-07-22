//! Minimal bzip2 reader for td-builder.
//!
//! Kept in-tree and std-only for the same reason as `tar.rs` and `gzip.rs`:
//! the bootstrap engine unpacks pinned `.tar.bz2` sources natively, so recipes
//! need no host tar/bzip2 and no Rust crate dependency (re #469). Decode-only:
//! stream/block headers, the 2..6 selector-switched Huffman tables, MTF+RLE2
//! (RUNA/RUNB), the inverse BWT (single-pass T-vector), RLE1, and both CRC
//! layers (per-block and combined stream). Concatenated streams decode like
//! `bzcat`. The deprecated randomization bit is rejected — no real tarball
//! uses it.

use std::fs::File;
use std::io::Read;
use std::path::Path;

const MAX_BZIP2_INPUT_BYTES: u64 = 257 * 1024 * 1024;
// Sized by the largest pinned .tar.bz2 source: gcc-4.9.4.tar.bz2 expands to
// ~554 MiB, so the gzip reader's 256 MiB cap would reject a real seed tarball.
const MAX_BZIP2_OUTPUT_BYTES: usize = 1024 * 1024 * 1024;

/// 48-bit compressed-block magic (BCD digits of pi).
const BLOCK_MAGIC: u64 = 0x3141_5926_5359;
/// 48-bit end-of-stream magic (BCD digits of sqrt(pi)).
const EOS_MAGIC: u64 = 0x1772_4538_5090;
/// Symbols decoded per Huffman-table selector.
const GROUP_RUN: u32 = 50;
/// RLE2 run digits: a run of the MTF-front byte in bijective base 2.
const RUNA: u16 = 0;
const RUNB: u16 = 1;

pub fn decompress_file(path: &Path) -> Result<Vec<u8>, String> {
    let file = File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let len = file
        .metadata()
        .map_err(|e| format!("stat {}: {e}", path.display()))?
        .len();
    if len > MAX_BZIP2_INPUT_BYTES {
        return Err(format!(
            "bzip2 input {} is too large: {len} bytes exceeds {} byte limit",
            path.display(),
            MAX_BZIP2_INPUT_BYTES
        ));
    }
    let cap =
        usize::try_from(len).map_err(|_| "bzip2 input length did not fit usize".to_string())?;
    let mut input = Vec::with_capacity(cap);
    let mut limited = file.take(
        MAX_BZIP2_INPUT_BYTES
            .checked_add(1)
            .ok_or_else(|| "bzip2 input limit overflow".to_string())?,
    );
    limited
        .read_to_end(&mut input)
        .map_err(|e| format!("read {}: {e}", path.display()))?;
    let read_len =
        u64::try_from(input.len()).map_err(|_| "bzip2 input length did not fit u64".to_string())?;
    if read_len > MAX_BZIP2_INPUT_BYTES {
        return Err(format!(
            "bzip2 input {} grew past {} byte limit while reading",
            path.display(),
            MAX_BZIP2_INPUT_BYTES
        ));
    }
    decompress(&input).map_err(|e| format!("decompress {}: {e}", path.display()))
}

pub fn decompress(input: &[u8]) -> Result<Vec<u8>, String> {
    decompress_with_limit(input, MAX_BZIP2_OUTPUT_BYTES)
}

fn decompress_with_limit(input: &[u8], max_output_bytes: usize) -> Result<Vec<u8>, String> {
    if input.is_empty() {
        return Err("empty bzip2 stream".to_string());
    }
    let crc_table = bz_crc32_table();
    let mut pos = 0usize;
    let mut out = Vec::new();
    while pos < input.len() {
        pos = decompress_stream(input, pos, &crc_table, &mut out, max_output_bytes)?;
    }
    Ok(out)
}

/// Decode one `BZh` stream starting at byte `start`, appending to `out`.
/// Returns the byte offset just past the stream (concatenated streams follow
/// byte-aligned, like `bzcat`).
fn decompress_stream(
    input: &[u8],
    start: usize,
    crc_table: &[u32; 256],
    out: &mut Vec<u8>,
    max_output_bytes: usize,
) -> Result<usize, String> {
    let header = range(input, start, 4)?;
    if byte(header, 0)? != b'B' || byte(header, 1)? != b'Z' || byte(header, 2)? != b'h' {
        return Err("bad bzip2 magic".to_string());
    }
    let level = byte(header, 3)?;
    if !(b'1'..=b'9').contains(&level) {
        return Err(format!("bad bzip2 block-size level byte 0x{level:02x}"));
    }
    let block_limit = 100_000usize
        .checked_mul(usize::from(level - b'0'))
        .ok_or_else(|| "bzip2 block size overflow".to_string())?;
    let data_start = start
        .checked_add(4)
        .ok_or_else(|| "bzip2 header offset overflow".to_string())?;
    let mut bits = BitReader::new(input, data_start);
    // Per-stream scratch, reused across blocks so the hot loop does not
    // reallocate: the BWT string and its T-vector.
    let mut bwt: Vec<u8> = Vec::with_capacity(block_limit);
    let mut tt: Vec<u32> = Vec::new();
    let mut combined_crc = 0u32;
    loop {
        let magic = bits.read_magic48()?;
        if magic == BLOCK_MAGIC {
            let block_crc = decode_block(
                &mut bits,
                block_limit,
                crc_table,
                &mut bwt,
                &mut tt,
                out,
                max_output_bytes,
            )?;
            combined_crc = combined_crc.rotate_left(1) ^ block_crc;
        } else if magic == EOS_MAGIC {
            let stored = bits.read_bits(32)?;
            if stored != combined_crc {
                return Err(format!(
                    "bzip2 stream CRC mismatch: got {combined_crc:08x}, want {stored:08x}"
                ));
            }
            return bits.aligned_byte_position();
        } else {
            return Err(format!("bad bzip2 block magic 0x{magic:012x}"));
        }
    }
}

/// Decode one compressed block (its 48-bit magic already consumed), append the
/// plain bytes to `out`, and return the verified block CRC.
fn decode_block(
    bits: &mut BitReader<'_>,
    block_limit: usize,
    crc_table: &[u32; 256],
    bwt: &mut Vec<u8>,
    tt: &mut Vec<u32>,
    out: &mut Vec<u8>,
    max_output_bytes: usize,
) -> Result<u32, String> {
    let want_crc = bits.read_bits(32)?;
    if bits.read_bit()? != 0 {
        return Err("randomized bzip2 blocks are not supported".to_string());
    }
    let orig_ptr = usize::try_from(bits.read_bits(24)?)
        .map_err(|_| "bzip2 BWT origin pointer did not fit usize".to_string())?;
    let seq_to_unseq = read_symbol_map(bits)?;
    // The RLE2 alphabet: RUNA, RUNB, one symbol per used byte minus one
    // (MTF position 0 is expressed through the runs), and EOB.
    let alpha_size = seq_to_unseq.len() + 2;
    let (tables, selectors) = read_huffman_tables(bits, alpha_size)?;
    decode_mtf_rle2(bits, &tables, &selectors, &seq_to_unseq, block_limit, bwt)?;
    let got_crc = inverse_bwt_rle1(bwt, tt, orig_ptr, crc_table, out, max_output_bytes)?;
    if got_crc != want_crc {
        return Err(format!(
            "bzip2 block CRC mismatch: got {got_crc:08x}, want {want_crc:08x}"
        ));
    }
    Ok(got_crc)
}

/// The two-level 16+16 bitmap of byte values used in this block, in
/// ascending order (bzip2's `seqToUnseq`).
fn read_symbol_map(bits: &mut BitReader<'_>) -> Result<Vec<u8>, String> {
    let used_rows = bits.read_bits(16)?;
    let mut seq_to_unseq: Vec<u8> = Vec::with_capacity(256);
    for row in 0..16u32 {
        if used_rows & (0x8000 >> row) == 0 {
            continue;
        }
        let cells = bits.read_bits(16)?;
        for cell in 0..16u32 {
            if cells & (0x8000 >> cell) != 0 {
                let value = u8::try_from(row * 16 + cell)
                    .map_err(|_| "bzip2 symbol value did not fit u8".to_string())?;
                seq_to_unseq.push(value);
            }
        }
    }
    if seq_to_unseq.is_empty() {
        return Err("bzip2 block uses no byte values".to_string());
    }
    Ok(seq_to_unseq)
}

/// Group count, MTF-coded selector list, and the delta-coded canonical code
/// lengths for each group's Huffman table.
fn read_huffman_tables(
    bits: &mut BitReader<'_>,
    alpha_size: usize,
) -> Result<(Vec<HuffTable>, Vec<u8>), String> {
    let n_groups = usize::try_from(bits.read_bits(3)?)
        .map_err(|_| "bzip2 group count did not fit usize".to_string())?;
    if !(2..=6).contains(&n_groups) {
        return Err(format!("bzip2 Huffman group count {n_groups} out of range"));
    }
    let n_selectors = usize::try_from(bits.read_bits(15)?)
        .map_err(|_| "bzip2 selector count did not fit usize".to_string())?;
    if n_selectors == 0 {
        return Err("bzip2 block declares zero selectors".to_string());
    }
    let mut group_mtf: Vec<u8> = Vec::with_capacity(n_groups);
    for group in 0..n_groups {
        group_mtf
            .push(u8::try_from(group).map_err(|_| "bzip2 group id did not fit u8".to_string())?);
    }
    let mut selectors = Vec::with_capacity(n_selectors);
    for _ in 0..n_selectors {
        let mut j = 0usize;
        while bits.read_bit()? == 1 {
            j += 1;
            if j >= n_groups {
                return Err("bzip2 selector out of range".to_string());
            }
        }
        let prefix = group_mtf
            .get_mut(..=j)
            .ok_or_else(|| "bzip2 selector MTF index out of range".to_string())?;
        prefix.rotate_right(1);
        selectors.push(
            *group_mtf
                .first()
                .ok_or_else(|| "bzip2 selector MTF list is empty".to_string())?,
        );
    }
    let mut tables = Vec::with_capacity(n_groups);
    let mut lengths: Vec<u8> = Vec::with_capacity(alpha_size);
    for _ in 0..n_groups {
        lengths.clear();
        let mut curr = bits.read_bits(5)?;
        for _ in 0..alpha_size {
            loop {
                if !(1..=20).contains(&curr) {
                    return Err(format!("bzip2 Huffman code length {curr} out of range"));
                }
                if bits.read_bit()? == 0 {
                    break;
                }
                if bits.read_bit()? == 0 {
                    curr = curr
                        .checked_add(1)
                        .ok_or_else(|| "bzip2 code length overflow".to_string())?;
                } else {
                    curr = curr
                        .checked_sub(1)
                        .ok_or_else(|| "bzip2 code length underflow".to_string())?;
                }
            }
            lengths.push(
                u8::try_from(curr).map_err(|_| "bzip2 code length did not fit u8".to_string())?,
            );
        }
        tables.push(HuffTable::from_lengths(&lengths)?);
    }
    Ok((tables, selectors))
}

/// Walks the selector list, yielding one Huffman symbol per call and switching
/// tables every `GROUP_RUN` symbols.
struct GroupCursor {
    remaining: u32,
    next_selector: usize,
    table: usize,
}

impl GroupCursor {
    fn next_sym(
        &mut self,
        bits: &mut BitReader<'_>,
        tables: &[HuffTable],
        selectors: &[u8],
    ) -> Result<u16, String> {
        if self.remaining == 0 {
            let sel = *selectors
                .get(self.next_selector)
                .ok_or_else(|| "bzip2 block ran out of Huffman selectors".to_string())?;
            self.next_selector = self
                .next_selector
                .checked_add(1)
                .ok_or_else(|| "bzip2 selector index overflow".to_string())?;
            self.table = usize::from(sel);
            self.remaining = GROUP_RUN;
        }
        self.remaining -= 1;
        tables
            .get(self.table)
            .ok_or_else(|| "bzip2 selector references a missing table".to_string())?
            .decode(bits)
    }
}

/// Decode the block's MTF+RLE2 symbol stream into the BWT string `bwt`.
fn decode_mtf_rle2(
    bits: &mut BitReader<'_>,
    tables: &[HuffTable],
    selectors: &[u8],
    seq_to_unseq: &[u8],
    block_limit: usize,
    bwt: &mut Vec<u8>,
) -> Result<(), String> {
    bwt.clear();
    let alpha_size = seq_to_unseq.len() + 2;
    let eob = u16::try_from(alpha_size - 1)
        .map_err(|_| "bzip2 EOB symbol did not fit u16".to_string())?;
    let block_limit_u64 =
        u64::try_from(block_limit).map_err(|_| "bzip2 block size did not fit u64".to_string())?;
    let mut mtf: Vec<u8> = seq_to_unseq.to_vec();
    let mut cursor = GroupCursor {
        remaining: 0,
        next_selector: 0,
        table: 0,
    };
    let mut sym = cursor.next_sym(bits, tables, selectors)?;
    loop {
        if sym == eob {
            return Ok(());
        }
        if sym <= RUNB {
            // A run of the current MTF-front byte, RUNA/RUNB digits in
            // bijective base 2: run = sum of (digit+1) << position.
            let mut run: u64 = 0;
            let mut shift: u32 = 0;
            loop {
                let digit: u64 = if sym == RUNA { 1 } else { 2 };
                let add = digit
                    .checked_shl(shift)
                    .ok_or_else(|| "bzip2 RLE2 run overflow".to_string())?;
                run = run
                    .checked_add(add)
                    .ok_or_else(|| "bzip2 RLE2 run overflow".to_string())?;
                if run > block_limit_u64 {
                    return Err("bzip2 RLE2 run exceeds the block size".to_string());
                }
                sym = cursor.next_sym(bits, tables, selectors)?;
                if sym > RUNB {
                    break;
                }
                shift = shift
                    .checked_add(1)
                    .ok_or_else(|| "bzip2 RLE2 shift overflow".to_string())?;
            }
            let value = *mtf
                .first()
                .ok_or_else(|| "bzip2 MTF list is empty".to_string())?;
            let run =
                usize::try_from(run).map_err(|_| "bzip2 RLE2 run did not fit usize".to_string())?;
            let new_len = bwt
                .len()
                .checked_add(run)
                .ok_or_else(|| "bzip2 block length overflow".to_string())?;
            if new_len > block_limit {
                return Err("bzip2 block overruns its declared size".to_string());
            }
            bwt.resize(new_len, value);
            continue;
        }
        // A plain MTF value: move position (sym - 1) to the front, emit it.
        let idx = usize::from(sym)
            .checked_sub(1)
            .ok_or_else(|| "bzip2 MTF symbol underflow".to_string())?;
        let prefix = mtf
            .get_mut(..=idx)
            .ok_or_else(|| "bzip2 MTF index out of range".to_string())?;
        prefix.rotate_right(1);
        let value = *mtf
            .first()
            .ok_or_else(|| "bzip2 MTF list is empty".to_string())?;
        if bwt.len() >= block_limit {
            return Err("bzip2 block overruns its declared size".to_string());
        }
        bwt.push(value);
        sym = cursor.next_sym(bits, tables, selectors)?;
    }
}

/// Invert the BWT with the single-pass T-vector (counting sort + next-array),
/// RLE1-decode the result into `out`, and return the block CRC of the plain
/// bytes.
fn inverse_bwt_rle1(
    bwt: &[u8],
    tt: &mut Vec<u32>,
    orig_ptr: usize,
    crc_table: &[u32; 256],
    out: &mut Vec<u8>,
    max_output_bytes: usize,
) -> Result<u32, String> {
    let nblock = bwt.len();
    if nblock == 0 {
        return Err("bzip2 block decoded to no data".to_string());
    }
    if orig_ptr >= nblock {
        return Err("bzip2 BWT origin pointer out of range".to_string());
    }
    // Counting sort: cftab[b] becomes the first row index whose first column
    // holds byte b; then tt[·] is the next-array over the sorted rotations.
    let mut cftab = [0usize; 256];
    for value in bwt {
        let slot = cftab
            .get_mut(usize::from(*value))
            .ok_or_else(|| "bzip2 byte count out of range".to_string())?;
        *slot = slot
            .checked_add(1)
            .ok_or_else(|| "bzip2 byte count overflow".to_string())?;
    }
    let mut sum = 0usize;
    for slot in cftab.iter_mut() {
        let count = *slot;
        *slot = sum;
        sum = sum
            .checked_add(count)
            .ok_or_else(|| "bzip2 byte count overflow".to_string())?;
    }
    tt.clear();
    tt.resize(nblock, 0u32);
    for (i, value) in bwt.iter().enumerate() {
        let slot = cftab
            .get_mut(usize::from(*value))
            .ok_or_else(|| "bzip2 byte count out of range".to_string())?;
        let dst = tt
            .get_mut(*slot)
            .ok_or_else(|| "bzip2 BWT permutation out of range".to_string())?;
        *dst = u32::try_from(i).map_err(|_| "bzip2 block index did not fit u32".to_string())?;
        *slot = slot
            .checked_add(1)
            .ok_or_else(|| "bzip2 permutation index overflow".to_string())?;
    }
    // Walk the next-array from the original row, un-RLE1 as we go: after four
    // equal bytes the next BWT byte is a repeat count (0..255), never data.
    let mut pos = usize::try_from(
        *tt.get(orig_ptr)
            .ok_or_else(|| "bzip2 BWT origin out of range".to_string())?,
    )
    .map_err(|_| "bzip2 BWT position did not fit usize".to_string())?;
    let mut crc = 0xffff_ffffu32;
    let mut run = 0u32;
    let mut last: u16 = 0x0100; // sentinel outside the byte range
    for _ in 0..nblock {
        let value = *bwt
            .get(pos)
            .ok_or_else(|| "bzip2 BWT walk out of range".to_string())?;
        pos = usize::try_from(
            *tt.get(pos)
                .ok_or_else(|| "bzip2 BWT walk out of range".to_string())?,
        )
        .map_err(|_| "bzip2 BWT position did not fit usize".to_string())?;
        if run == 4 {
            // `value` is the RLE1 repeat count for the just-emitted byte.
            let repeat = u8::try_from(last).map_err(|_| "bzip2 RLE1 state corrupt".to_string())?;
            for _ in 0..value {
                push_output(out, repeat, max_output_bytes)?;
                crc = crc_update(crc_table, crc, repeat);
            }
            run = 0;
            last = 0x0100;
            continue;
        }
        if u16::from(value) == last {
            run += 1;
        } else {
            run = 1;
            last = u16::from(value);
        }
        push_output(out, value, max_output_bytes)?;
        crc = crc_update(crc_table, crc, value);
    }
    if run == 4 {
        // A run of exactly four is always followed by its count byte; a block
        // ending here is corrupt.
        return Err("bzip2 block ended inside an RLE1 run (missing count byte)".to_string());
    }
    Ok(!crc)
}

fn push_output(out: &mut Vec<u8>, byte: u8, max_output_bytes: usize) -> Result<(), String> {
    if out.len() >= max_output_bytes {
        return Err(format!(
            "bzip2 output exceeds {max_output_bytes} byte limit"
        ));
    }
    out.push(byte);
    Ok(())
}

/// One group's canonical Huffman decode table — bzip2's limit/base/perm form.
struct HuffTable {
    min_len: u32,
    /// Indexed by code length: the largest code value of that length.
    limit: Vec<i64>,
    /// Indexed by code length: subtract from a code to index `perm`.
    base: Vec<i64>,
    /// Symbols ordered by (code length, symbol).
    perm: Vec<u16>,
}

impl HuffTable {
    fn from_lengths(lengths: &[u8]) -> Result<HuffTable, String> {
        let min_len = *lengths
            .iter()
            .min()
            .ok_or_else(|| "empty bzip2 Huffman table".to_string())?;
        let max_len = *lengths
            .iter()
            .max()
            .ok_or_else(|| "empty bzip2 Huffman table".to_string())?;
        let max = usize::from(max_len);
        let mut perm = Vec::with_capacity(lengths.len());
        for want in min_len..=max_len {
            for (symbol, len) in lengths.iter().enumerate() {
                if *len == want {
                    perm.push(
                        u16::try_from(symbol)
                            .map_err(|_| "bzip2 symbol did not fit u16".to_string())?,
                    );
                }
            }
        }
        // cum[l] = number of symbols with code length < l.
        let mut cum = vec![0i64; max + 2];
        for len in lengths {
            let slot = cum
                .get_mut(usize::from(*len) + 1)
                .ok_or_else(|| "bzip2 code length out of range".to_string())?;
            *slot += 1;
        }
        let mut running = 0i64;
        for slot in cum.iter_mut() {
            running += *slot;
            *slot = running;
        }
        let mut limit = vec![0i64; max + 1];
        let mut base = vec![0i64; max + 1];
        let mut code = 0i64;
        for len in usize::from(min_len)..=max {
            let this = cum
                .get(len + 1)
                .ok_or_else(|| "bzip2 length count out of range".to_string())?
                - cum
                    .get(len)
                    .ok_or_else(|| "bzip2 length count out of range".to_string())?;
            code += this;
            let slot = limit
                .get_mut(len)
                .ok_or_else(|| "bzip2 limit index out of range".to_string())?;
            *slot = code - 1;
            code <<= 1;
        }
        for len in usize::from(min_len) + 1..=max {
            let prev = *limit
                .get(len - 1)
                .ok_or_else(|| "bzip2 limit index out of range".to_string())?;
            let below = *cum
                .get(len)
                .ok_or_else(|| "bzip2 length count out of range".to_string())?;
            let slot = base
                .get_mut(len)
                .ok_or_else(|| "bzip2 base index out of range".to_string())?;
            *slot = ((prev + 1) << 1) - below;
        }
        Ok(HuffTable {
            min_len: u32::from(min_len),
            limit,
            base,
            perm,
        })
    }

    fn decode(&self, bits: &mut BitReader<'_>) -> Result<u16, String> {
        let mut len = usize::try_from(self.min_len)
            .map_err(|_| "bzip2 Huffman length did not fit usize".to_string())?;
        let mut code = i64::from(bits.read_bits(self.min_len)?);
        loop {
            let lim = *self
                .limit
                .get(len)
                .ok_or_else(|| "invalid bzip2 Huffman code".to_string())?;
            if code <= lim {
                break;
            }
            len = len
                .checked_add(1)
                .ok_or_else(|| "bzip2 Huffman length overflow".to_string())?;
            code = (code << 1) | i64::from(bits.read_bit()?);
        }
        let base = *self
            .base
            .get(len)
            .ok_or_else(|| "invalid bzip2 Huffman code".to_string())?;
        let idx =
            usize::try_from(code - base).map_err(|_| "invalid bzip2 Huffman code".to_string())?;
        self.perm
            .get(idx)
            .copied()
            .ok_or_else(|| "invalid bzip2 Huffman symbol".to_string())
    }
}

/// MSB-first bit reader over the whole input, starting at a byte offset.
struct BitReader<'a> {
    input: &'a [u8],
    /// The next byte to load into the accumulator.
    pos: usize,
    acc: u64,
    /// Valid low bits of `acc` not yet consumed.
    nbits: u32,
}

impl<'a> BitReader<'a> {
    fn new(input: &'a [u8], pos: usize) -> BitReader<'a> {
        BitReader {
            input,
            pos,
            acc: 0,
            nbits: 0,
        }
    }

    fn read_bits(&mut self, n: u32) -> Result<u32, String> {
        if n == 0 || n > 32 {
            return Err(format!("cannot read {n} bits at once"));
        }
        while self.nbits < n {
            let b = *self
                .input
                .get(self.pos)
                .ok_or_else(|| "truncated bzip2 stream".to_string())?;
            self.acc = (self.acc << 8) | u64::from(b);
            self.pos = self
                .pos
                .checked_add(1)
                .ok_or_else(|| "bzip2 byte position overflow".to_string())?;
            self.nbits = self
                .nbits
                .checked_add(8)
                .ok_or_else(|| "bzip2 bit count overflow".to_string())?;
        }
        let shift = self.nbits - n;
        let value = (self.acc >> shift) & ((1u64 << n) - 1);
        self.nbits = shift;
        u32::try_from(value).map_err(|_| "bit value did not fit u32".to_string())
    }

    fn read_bit(&mut self) -> Result<u32, String> {
        self.read_bits(1)
    }

    fn read_magic48(&mut self) -> Result<u64, String> {
        let hi = self.read_bits(24)?;
        let lo = self.read_bits(24)?;
        Ok((u64::from(hi) << 24) | u64::from(lo))
    }

    /// Discard padding bits to the next byte boundary and return that byte
    /// offset in the input (where a concatenated stream would start).
    fn aligned_byte_position(&mut self) -> Result<usize, String> {
        self.nbits -= self.nbits % 8;
        let buffered = usize::try_from(self.nbits / 8)
            .map_err(|_| "bzip2 buffered byte count did not fit usize".to_string())?;
        self.pos
            .checked_sub(buffered)
            .ok_or_else(|| "bzip2 byte position underflow".to_string())
    }
}

/// bzip2's CRC-32: MSB-first, polynomial 0x04c11db7 (bit-reversed relative to
/// the zlib/gzip variant).
fn crc_update(table: &[u32; 256], crc: u32, byte: u8) -> u32 {
    let idx = ((crc >> 24) ^ u32::from(byte)) & 0xff;
    let entry = table.get(idx as usize).copied().unwrap_or(0);
    (crc << 8) ^ entry
}

fn bz_crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    for i in 0..256usize {
        let mut crc = u32::try_from(i).unwrap_or(0) << 24;
        for _ in 0..8 {
            if crc & 0x8000_0000 == 0 {
                crc <<= 1;
            } else {
                crc = (crc << 1) ^ 0x04c1_1db7;
            }
        }
        if let Some(slot) = table.get_mut(i) {
            *slot = crc;
        }
    }
    table
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // `printf 'hello world\n' | bzip2 -9` — one block, level 9.
    const HELLO_BZ2_HEX: &str = "425a68393141592653594eece83600000251800010400006449080200031\
                                 064c4101a7a9a580bb9431f8bb9229c28482776741b0";

    // `sample_data()` compressed once with `bzip2 -1` — one block, level 1;
    // exercises RLE1 count bytes (300-byte runs) and RLE2 runs.
    const SAMPLE_BZ2_HEX: &str = "425a68313141592653596415403900006b7fffcdfdffffffffcffa79b6ff\
        fffff9ffffaee797f7fbcc73bd577ff7fdfdcf9cbfb0015b263640d000069a001a687a800001a34686800\
        00d00d000003434d3400003d4d064d03d4000034f29a66a34c8c410321a681a1ea0000068000d06800680\
        d0d00d1a68341a0327a800003201a7a9ea00d000068000d0c9899a42b548d1a006801a1a680d001a03200\
        1a0064000003400191a0d00000001900062006800000fa964a10aec61d185c91750da5e1a8ef794c13d94\
        5be85e3570cca3d8ca2059257aac0101c74acea43850a4f157f8c025451c08b26209c6252880728408af3\
        2520c4d4543835c4f95492305498f281a3d616d65b88d96bd64047d59539e1582312e27e4d00c104e88d9\
        15ab613d996af96cb309e603baad54c8a54c8da79176fb2f13866cb350e38d61087f55896e01be3ecf878\
        d30123e6e27b1020a397c998f3b4f76526e184451213c2000a4d010006068880037f5d6750400\
        0da2000623e2000b64020007b100073645b90a737c064c1a61383488f4d2c93850d67883ec60e0450f729\
        81c9ca8b8d5d0f54141a3324467a5c990b7042e7d4ac0ba100561d0065040cdaad171c6931471a11f0880\
        54b1341501eb609233d29fcd22582b750776bc814f95f41e1be868878686cd1b73235f30ca6985d9d0ce9\
        a50592a4a03672475c4ee3843f102ca2e2be14f8043dff9681eb17a93780d248ca0fe1951a9b6023cac65\
        1dd32c40e74247983f92a2528412e7d9c82c0d48a234d7a094bb068bb90476d1f968d867843005459fc9f\
        d1e8938dcefcfeaefea9283948600b8e67fc5dc914e14241905500e40";

    // `printf '' | bzip2 -9` — a stream with zero blocks.
    const EMPTY_BZ2_HEX: &str = "425a683917724538509000000000";

    /// The plain bytes SAMPLE_BZ2 was made from: repetitive text, long runs
    /// (RLE1 territory), and LCG pseudo-random bytes.
    fn sample_data() -> Vec<u8> {
        let mut out = Vec::new();
        for _ in 0..30 {
            out.extend_from_slice(b"abcdefgh");
        }
        for byte in [0u8, 0xff, b'A'] {
            for _ in 0..300 {
                out.push(byte);
            }
        }
        let mut x: u32 = 0x2545_f491;
        for _ in 0..400 {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            out.push((x >> 16) as u8);
        }
        out
    }

    fn sources_path(name: &str) -> Option<PathBuf> {
        let path = crate::bootstrap::shared_sources_dir().join(name);
        path.exists().then_some(path)
    }

    fn decompress_tarball(name: &str, min_len: usize) -> Option<usize> {
        let Some(path) = sources_path(name) else {
            eprintln!("skipping: {name} not present in the shared sources cache");
            return None;
        };
        let data = std::fs::read(&path).unwrap();
        let started = std::time::Instant::now();
        let out = decompress(&data).unwrap();
        eprintln!(
            "decompressed {name}: {} -> {} bytes in {:?}",
            data.len(),
            out.len(),
            started.elapsed()
        );
        assert!(out.len() > min_len, "only {} bytes decoded", out.len());
        assert_eq!(
            &out[257..262],
            b"ustar",
            "first tar header lacks ustar magic"
        );
        Some(out.len())
    }

    #[test]
    fn decompresses_hello_world() {
        let bz = hex_bytes(HELLO_BZ2_HEX);
        assert_eq!(decompress(&bz).unwrap(), b"hello world\n");
    }

    #[test]
    fn decompresses_repetitive_and_random_sample() {
        let bz = hex_bytes(SAMPLE_BZ2_HEX);
        assert_eq!(decompress(&bz).unwrap(), sample_data());
    }

    #[test]
    fn decompresses_zero_block_stream() {
        let bz = hex_bytes(EMPTY_BZ2_HEX);
        assert_eq!(decompress(&bz).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn decompresses_concatenated_streams() {
        // Two streams back to back (different levels), like `bzcat file1 file2`.
        let mut bz = hex_bytes(HELLO_BZ2_HEX);
        bz.extend_from_slice(&hex_bytes(SAMPLE_BZ2_HEX));
        let mut want = b"hello world\n".to_vec();
        want.extend_from_slice(&sample_data());
        assert_eq!(decompress(&bz).unwrap(), want);
    }

    #[test]
    fn empty_input_errors() {
        let err = decompress(&[]).unwrap_err();
        assert!(err.contains("empty bzip2 stream"), "got: {err}");
    }

    #[test]
    fn corrupt_byte_errors() {
        let mut bz = hex_bytes(SAMPLE_BZ2_HEX);
        bz[300] ^= 0xff;
        assert!(decompress(&bz).is_err());

        let mut bz = hex_bytes(HELLO_BZ2_HEX);
        bz[25] ^= 0xff;
        assert!(decompress(&bz).is_err());
    }

    #[test]
    fn truncated_stream_errors() {
        let bz = hex_bytes(HELLO_BZ2_HEX);
        assert!(decompress(&bz[..bz.len() - 6]).is_err());
        assert!(decompress(&bz[..20]).is_err());
        assert!(decompress(&bz[..3]).is_err());
    }

    #[test]
    fn trailing_garbage_errors() {
        let mut bz = hex_bytes(HELLO_BZ2_HEX);
        bz.extend_from_slice(b"junk");
        let err = decompress(&bz).unwrap_err();
        assert!(err.contains("bad bzip2 magic"), "got: {err}");
    }

    #[test]
    fn randomized_block_errors() {
        // Bit 80 after the 4-byte header (48-bit magic + 32-bit CRC) is the
        // randomization flag: the first bit of byte 14.
        let mut bz = hex_bytes(HELLO_BZ2_HEX);
        bz[14] |= 0x80;
        let err = decompress(&bz).unwrap_err();
        assert!(
            err.contains("randomized bzip2 blocks are not supported"),
            "got: {err}"
        );
    }

    #[test]
    fn output_limit_errors_before_decoding_past_bound() {
        let bz = hex_bytes(HELLO_BZ2_HEX);
        let err = decompress_with_limit(&bz, 5).unwrap_err();
        assert!(
            err.contains("bzip2 output exceeds 5 byte limit"),
            "got: {err}"
        );
    }

    #[test]
    fn decompresses_tcc_tarball() {
        decompress_tarball("tcc-0.9.27.tar.bz2", 500_000);
    }

    #[test]
    fn decompresses_binutils_tarball() {
        // Larger and multi-block: exercises selector tables and the combined
        // stream CRC across many blocks.
        decompress_tarball("binutils-2.20.1a.tar.bz2", 500_000);
    }

    fn hex_bytes(hex: &str) -> Vec<u8> {
        let compact: String = hex.chars().filter(|c| !c.is_whitespace()).collect();
        let mut out = Vec::new();
        let mut chars = compact.as_bytes().chunks(2);
        while let Some(pair) = chars.next() {
            let s = std::str::from_utf8(pair).unwrap();
            out.push(u8::from_str_radix(s, 16).unwrap());
        }
        out
    }
}

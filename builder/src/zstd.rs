//! zstd.rs — dependency-free Zstandard (RFC 8878) compressor for the EROFS image
//! writer, with an independent decoder that checks every frame it emits.
//!
//! Scope: one frame per call, holding at most `MAX_INPUT` bytes in one block, with the
//! Single_Segment flag set so the frame's window is exactly its content. That is what
//! an EROFS extent needs: the kernel decodes each extent as one self-contained frame
//! whose window must fit the image's configured dictionary size (`erofs.rs`).
//!
//! Encoding: a hash-chain LZ77 matcher with one-step lazy evaluation and the
//! repeat-offset shortcut; Huffman-coded literals (one or four streams, weights
//! direct or FSE-coded); sequences coded with whichever of the predefined or a
//! frame-local FSE table is smaller for each of the three symbol streams. A block
//! that does not shrink is stored raw.
//!
//! Every frame is decoded again before it is returned and compared with its input,
//! so an encoder error fails the build instead of reaching an image. The decoder is
//! also checked against frames produced by the reference implementation (tests).

use std::io;

/// Largest input one frame carries: one block (RFC 8878 Block_Maximum_Size).
pub const MAX_INPUT: usize = 128 * 1024;

const MAGIC: u32 = 0xFD2F_B528;
const MIN_MATCH: usize = 4;
const HASH_BITS: u32 = 15;
const CHAIN_DEPTH: usize = 48;
/// Literal runs shorter than this are stored raw: a Huffman table costs more.
const MIN_HUFFMAN_LITERALS: usize = 64;
const HUF_MAX_BITS: u32 = 11;
const HUF_WEIGHT_LOG: u32 = 6;
const FSE_MIN_LOG: u32 = 5;

const BLOCK_RAW: u32 = 0;
const BLOCK_RLE: u32 = 1;
const BLOCK_COMPRESSED: u32 = 2;

const LIT_RAW: u8 = 0;
const LIT_RLE: u8 = 1;
const LIT_COMPRESSED: u8 = 2;

const MODE_PREDEFINED: u8 = 0;
const MODE_RLE: u8 = 1;
const MODE_FSE: u8 = 2;

const LL_BASE: &[u32] = &[
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 18, 20, 22, 24, 28, 32, 40, 48, 64,
    128, 256, 512, 1024, 2048, 4096, 8192, 16384, 32768, 65536,
];
const LL_BITS: &[u32] = &[
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 3, 3, 4, 6, 7, 8, 9, 10, 11,
    12, 13, 14, 15, 16,
];
const ML_BASE: &[u32] = &[
    3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27,
    28, 29, 30, 31, 32, 33, 34, 35, 37, 39, 41, 43, 47, 51, 59, 67, 83, 99, 131, 259, 515, 1027,
    2051, 4099, 8195, 16387, 32771, 65539,
];
const ML_BITS: &[u32] = &[
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    1, 1, 1, 1, 2, 2, 3, 3, 4, 4, 5, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16,
];
/// Largest offset code a decoder accepts (RFC 8878 §3.1.1.3.2.1.1).
const OF_MAX_CODE: usize = 31;

const LL_DEFAULT: &[i16] = &[
    4, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 2, 1, 1, 1, 1, 1,
    -1, -1, -1, -1,
];
const LL_DEFAULT_LOG: u32 = 6;
const LL_MAX_LOG: u32 = 9;
const ML_DEFAULT: &[i16] = &[
    1, 4, 3, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1, -1, -1,
];
const ML_DEFAULT_LOG: u32 = 6;
const ML_MAX_LOG: u32 = 9;
const OF_DEFAULT: &[i16] = &[
    1, 1, 1, 1, 1, 1, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1,
];
const OF_DEFAULT_LOG: u32 = 5;
const OF_MAX_LOG: u32 = 8;

fn corrupt(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("zstd: {msg}"))
}

fn internal(msg: &str) -> io::Error {
    io::Error::other(format!("zstd encoder: {msg}"))
}

/// Index of the highest set bit; `v` must be nonzero.
fn highbit(v: u32) -> u32 {
    31 - v.leading_zeros()
}

/// Compress `src` (at most `MAX_INPUT` bytes) into one frame and prove the frame
/// decodes back to `src`.
pub fn compress(src: &[u8]) -> io::Result<Vec<u8>> {
    if src.len() > MAX_INPUT {
        return Err(internal("input exceeds one block"));
    }
    let mut out = Vec::with_capacity(src.len() / 2 + 32);
    out.extend_from_slice(&MAGIC.to_le_bytes());
    // Frame_Header_Descriptor: Single_Segment, no checksum, no dictionary, and the
    // Frame_Content_Size field width.
    let size = src.len();
    if size <= 255 {
        out.push(0x20);
        out.push(size as u8);
    } else if size <= 65535 + 256 {
        out.push(0x60);
        out.extend_from_slice(&((size - 256) as u16).to_le_bytes());
    } else {
        out.push(0xA0);
        out.extend_from_slice(&(size as u32).to_le_bytes());
    }
    let (block_type, body) = encode_block(src)?;
    let header = 1 | (block_type << 1) | ((body_size(block_type, &body, size)? as u32) << 3);
    out.extend_from_slice(header.to_le_bytes().get(..3).unwrap_or(&[]));
    out.extend_from_slice(&body);
    match decompress(&out, src.len()) {
        Ok(back) if back == src => Ok(out),
        Ok(_) => Err(internal("frame does not decode to its input")),
        Err(e) => Err(internal(&format!("frame does not decode: {e}"))),
    }
}

/// The Block_Size field: the body length, except an RLE block's regenerated size.
fn body_size(block_type: u32, body: &[u8], size: usize) -> io::Result<usize> {
    let n = if block_type == BLOCK_RLE {
        size
    } else {
        body.len()
    };
    if n > MAX_INPUT {
        return Err(internal("block exceeds the maximum block size"));
    }
    Ok(n)
}

fn encode_block(src: &[u8]) -> io::Result<(u32, Vec<u8>)> {
    if let Some(&first) = src.first() {
        if src.len() > 1 && src.iter().all(|&b| b == first) {
            return Ok((BLOCK_RLE, vec![first]));
        }
    }
    let seqs = find_sequences(src);
    let mut body = Vec::new();
    let mut literals = Vec::with_capacity(src.len());
    let mut pos = 0usize;
    for s in &seqs.list {
        let end = pos + s.lit_len as usize;
        literals.extend_from_slice(src.get(pos..end).ok_or_else(|| internal("literal run"))?);
        pos = end + s.match_len as usize;
    }
    literals.extend_from_slice(src.get(pos..).ok_or_else(|| internal("last literals"))?);
    encode_literals(&literals, &mut body)?;
    encode_sequences(&seqs.list, &mut body)?;
    if body.len() >= src.len() {
        return Ok((BLOCK_RAW, src.to_vec()));
    }
    Ok((BLOCK_COMPRESSED, body))
}

// ---- matching -------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Seq {
    lit_len: u32,
    match_len: u32,
    /// Offset_Value as coded: 1 is the most recent offset, otherwise offset + 3.
    of_value: u32,
}

struct Sequences {
    list: Vec<Seq>,
}

fn hash4(src: &[u8], i: usize) -> usize {
    let v = match src.get(i..i + 4) {
        Some([a, b, c, d]) => u32::from_le_bytes([*a, *b, *c, *d]),
        _ => 0,
    };
    (v.wrapping_mul(0x9E37_79B1) >> (32 - HASH_BITS)) as usize
}

fn match_len(src: &[u8], a: usize, b: usize) -> usize {
    let (Some(x), Some(y)) = (src.get(a..), src.get(b..)) else {
        return 0;
    };
    x.iter().zip(y).take_while(|(p, q)| p == q).count()
}

struct Matcher<'a> {
    src: &'a [u8],
    head: Vec<u32>,
    chain: Vec<u32>,
    next_insert: usize,
}

impl<'a> Matcher<'a> {
    fn new(src: &'a [u8]) -> Self {
        Self {
            src,
            head: vec![0; 1 << HASH_BITS],
            chain: vec![0; src.len()],
            next_insert: 0,
        }
    }

    /// Insert every position below `upto` that can start a match.
    fn insert_upto(&mut self, upto: usize) {
        while self.next_insert < upto && self.next_insert + MIN_MATCH <= self.src.len() {
            let i = self.next_insert;
            let h = hash4(self.src, i);
            if let (Some(slot), Some(link)) = (self.head.get_mut(h), self.chain.get_mut(i)) {
                *link = *slot;
                *slot = (i + 1) as u32;
            }
            self.next_insert += 1;
        }
        self.next_insert = self.next_insert.max(upto);
    }

    /// Longest earlier match for position `i` as `(length, offset)`.
    fn best(&mut self, i: usize) -> (usize, usize) {
        if i + MIN_MATCH > self.src.len() {
            return (0, 0);
        }
        self.insert_upto(i);
        let mut best = (0usize, 0usize);
        let mut cand = self.head.get(hash4(self.src, i)).copied().unwrap_or(0);
        let mut depth = 0;
        while cand != 0 && depth < CHAIN_DEPTH {
            let j = (cand - 1) as usize;
            let len = match_len(self.src, j, i);
            if len > best.0 {
                best = (len, i - j);
                if i + len == self.src.len() {
                    break;
                }
            }
            cand = self.chain.get(j).copied().unwrap_or(0);
            depth += 1;
        }
        if best.0 < MIN_MATCH {
            (0, 0)
        } else {
            best
        }
    }
}

/// Greedy-with-one-step-lazy parse. Offsets never reach before the frame start, so
/// the frame decodes on its own.
fn find_sequences(src: &[u8]) -> Sequences {
    let mut m = Matcher::new(src);
    let mut list = Vec::new();
    let mut anchor = 0usize;
    let mut i = 0usize;
    let mut rep0 = 1usize;
    while i + MIN_MATCH <= src.len() {
        // The repeat offset costs almost nothing to code, so take it when it matches.
        if i > anchor && rep0 <= i {
            let len = match_len(src, i - rep0, i);
            if len >= MIN_MATCH {
                list.push(Seq {
                    lit_len: (i - anchor) as u32,
                    match_len: len as u32,
                    of_value: 1,
                });
                i += len;
                anchor = i;
                continue;
            }
        }
        let (mut len, mut off) = m.best(i);
        if len == 0 {
            i += 1;
            continue;
        }
        let (len1, off1) = m.best(i + 1);
        if len1 > len + 1 {
            i += 1;
            len = len1;
            off = off1;
        }
        // An offset equal to the repeat offset with no literals would be read as the
        // second repeat offset; coding it as a new offset keeps the meaning exact.
        list.push(Seq {
            lit_len: (i - anchor) as u32,
            match_len: len as u32,
            of_value: (off + 3) as u32,
        });
        rep0 = off;
        i += len;
        anchor = i;
    }
    Sequences { list }
}

// ---- bit streams ------------------------------------------------------------------

/// Writes a stream the decoder reads backward: values go in low bits first, and a
/// closing 1 bit marks where reading starts.
struct BitWriter {
    out: Vec<u8>,
    acc: u64,
    n: u32,
}

impl BitWriter {
    fn new() -> Self {
        Self {
            out: Vec::new(),
            acc: 0,
            n: 0,
        }
    }

    fn add(&mut self, value: u64, bits: u32) {
        if bits == 0 {
            return;
        }
        let mask = if bits >= 64 {
            u64::MAX
        } else {
            (1u64 << bits) - 1
        };
        self.acc |= (value & mask) << self.n;
        self.n += bits;
        while self.n >= 8 {
            self.out.push(self.acc as u8);
            self.acc >>= 8;
            self.n -= 8;
        }
    }

    fn close(mut self) -> Vec<u8> {
        self.add(1, 1);
        if self.n > 0 {
            self.out.push(self.acc as u8);
        }
        self.out
    }
}

/// Reads a backward stream from its closing bit toward its first byte. Reads past
/// the first byte yield zeros and are counted, as the reference decoder's are.
struct BitReader<'a> {
    data: &'a [u8],
    /// Bits not yet read; negative once reads ran past the start.
    left: i64,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> io::Result<Self> {
        let last = *data.last().ok_or_else(|| corrupt("empty bit stream"))?;
        if last == 0 {
            return Err(corrupt("bit stream lacks its closing bit"));
        }
        let left = (data.len() as i64 - 1) * 8 + i64::from(highbit(u32::from(last)));
        Ok(Self { data, left })
    }

    /// Bits `[start, start + 56)` of the stream as a little-endian number.
    fn load(&self, start: usize) -> u64 {
        let idx = start / 8;
        let mut word = [0u8; 8];
        if let Some(src) = self.data.get(idx..) {
            let n = src.len().min(8);
            if let (Some(dst), Some(src)) = (word.get_mut(..n), src.get(..n)) {
                dst.copy_from_slice(src);
            }
        }
        u64::from_le_bytes(word) >> (start % 8)
    }

    /// The next `bits` (at most 32) bits, most significant first, unconsumed.
    fn peek(&self, bits: u32) -> u64 {
        if bits == 0 {
            return 0;
        }
        let mask = (1u64 << bits) - 1;
        let start = self.left - i64::from(bits);
        if start >= 0 {
            return self.load(start as usize) & mask;
        }
        let avail = self.left.max(0) as u32;
        if avail == 0 {
            return 0;
        }
        (self.load(0) & ((1u64 << avail) - 1)) << (bits - avail)
    }

    fn read(&mut self, bits: u32) -> u64 {
        let v = self.peek(bits);
        self.left -= i64::from(bits);
        v
    }

    fn overflowed(&self) -> bool {
        self.left < 0
    }

    fn finished(&self) -> bool {
        self.left == 0
    }
}

// ---- FSE ----------------------------------------------------------------------------

/// Spread a normalized distribution over the state table (RFC 8878 §4.1.1).
fn spread(norm: &[i16], log: u32) -> io::Result<Vec<u8>> {
    let size = 1usize << log;
    let mask = size - 1;
    let step = (size >> 1) + (size >> 3) + 3;
    let mut table = vec![0u8; size];
    let mut high = size;
    for (s, &n) in norm.iter().enumerate() {
        if n == -1 {
            high = high
                .checked_sub(1)
                .ok_or_else(|| corrupt("FSE table overfull"))?;
            *table.get_mut(high).ok_or_else(|| corrupt("FSE spread"))? = s as u8;
        }
    }
    let mut pos = 0usize;
    for (s, &n) in norm.iter().enumerate() {
        for _ in 0..n.max(0) {
            *table.get_mut(pos).ok_or_else(|| corrupt("FSE spread"))? = s as u8;
            pos = (pos + step) & mask;
            while pos >= high {
                pos = (pos + step) & mask;
            }
        }
    }
    if pos != 0 {
        return Err(corrupt("FSE distribution does not fill its table"));
    }
    Ok(table)
}

fn check_norm(norm: &[i16], log: u32) -> io::Result<()> {
    let total: i64 = norm.iter().map(|&n| i64::from(n.unsigned_abs())).sum();
    if total != 1i64 << log || norm.iter().any(|&n| n < -1) {
        return Err(corrupt("FSE distribution does not sum to its table size"));
    }
    Ok(())
}

#[derive(Clone, Copy, Default)]
struct DState {
    symbol: u8,
    bits: u8,
    base: u16,
}

struct DTable {
    log: u32,
    states: Vec<DState>,
}

impl DTable {
    fn build(norm: &[i16], log: u32) -> io::Result<Self> {
        check_norm(norm, log)?;
        let size = 1u32 << log;
        let symbols = spread(norm, log)?;
        let mut next: Vec<u32> = norm
            .iter()
            .map(|&n| if n == -1 { 1 } else { n.max(0) as u32 })
            .collect();
        let mut states = Vec::with_capacity(symbols.len());
        for &s in &symbols {
            let slot = next
                .get_mut(usize::from(s))
                .ok_or_else(|| corrupt("FSE symbol"))?;
            let ns = *slot;
            *slot += 1;
            let bits = log - highbit(ns);
            states.push(DState {
                symbol: s,
                bits: bits as u8,
                base: ((ns << bits) - size) as u16,
            });
        }
        Ok(Self { log, states })
    }

    fn rle(symbol: u8) -> Self {
        Self {
            log: 0,
            states: vec![DState {
                symbol,
                bits: 0,
                base: 0,
            }],
        }
    }

    fn get(&self, state: usize) -> io::Result<DState> {
        self.states
            .get(state)
            .copied()
            .ok_or_else(|| corrupt("FSE state out of range"))
    }
}

struct CTable {
    log: u32,
    states: Vec<u16>,
    /// Per symbol: (deltaFindState, deltaNbBits).
    symbols: Vec<(i32, i64)>,
}

impl CTable {
    fn build(norm: &[i16], log: u32) -> io::Result<Self> {
        check_norm(norm, log)?;
        let size = 1u32 << log;
        let table = spread(norm, log)?;
        let mut cumul = Vec::with_capacity(norm.len());
        let mut acc = 0u32;
        for &n in norm {
            cumul.push(acc);
            acc += if n == -1 { 1 } else { n.max(0) as u32 };
        }
        let mut states = vec![0u16; size as usize];
        for (u, &s) in table.iter().enumerate() {
            let c = cumul
                .get_mut(usize::from(s))
                .ok_or_else(|| internal("FSE cumul"))?;
            *states
                .get_mut(*c as usize)
                .ok_or_else(|| internal("FSE state"))? = (size as usize + u) as u16;
            *c += 1;
        }
        let mut symbols = Vec::with_capacity(norm.len());
        let mut total = 0i32;
        for &n in norm {
            match n {
                0 => symbols.push((0, (i64::from(log) + 1) << 16)),
                -1 | 1 => {
                    symbols.push((total - 1, (i64::from(log) << 16) - i64::from(size)));
                    total += 1;
                }
                _ => {
                    let n = i32::from(n);
                    let max_bits_out = log - highbit((n - 1) as u32);
                    let min_state_plus = i64::from(n) << max_bits_out;
                    symbols.push((total - n, (i64::from(max_bits_out) << 16) - min_state_plus));
                    total += n;
                }
            }
        }
        Ok(Self {
            log,
            states,
            symbols,
        })
    }

    fn sym(&self, s: u8) -> io::Result<(i32, i64)> {
        self.symbols
            .get(usize::from(s))
            .copied()
            .ok_or_else(|| internal("FSE symbol outside its table"))
    }

    fn state_at(&self, index: i64) -> io::Result<u32> {
        usize::try_from(index)
            .ok()
            .and_then(|i| self.states.get(i))
            .map(|&v| u32::from(v))
            .ok_or_else(|| internal("FSE state index"))
    }

    fn init(&self, s: u8) -> io::Result<u32> {
        let (find, nb) = self.sym(s)?;
        let bits_out = (nb + (1 << 15)) >> 16;
        let value = (bits_out << 16) - nb;
        self.state_at((value >> bits_out) + i64::from(find))
    }

    fn encode(&self, w: &mut BitWriter, state: &mut u32, s: u8) -> io::Result<()> {
        let (find, nb) = self.sym(s)?;
        let bits_out = (i64::from(*state) + nb) >> 16;
        let bits_out = u32::try_from(bits_out).map_err(|_| internal("FSE bit count"))?;
        w.add(u64::from(*state), bits_out);
        *state = self.state_at(i64::from(*state >> bits_out) + i64::from(find))?;
        Ok(())
    }

    fn flush(&self, w: &mut BitWriter, state: u32) {
        w.add(u64::from(state), self.log);
    }
}

/// Scale symbol counts to a distribution summing to `1 << log`, every present
/// symbol at least 1 and none above `cap`.
fn normalize(counts: &[u32], log: u32, cap: u32) -> io::Result<Vec<i16>> {
    let size = 1u64 << log;
    let total: u64 = counts.iter().map(|&c| u64::from(c)).sum();
    let present = counts.iter().filter(|&&c| c > 0).count() as u64;
    if total == 0 || present > size || u64::from(cap) * present < size {
        return Err(internal("distribution cannot be normalized"));
    }
    let mut norm: Vec<i64> = counts
        .iter()
        .map(|&c| {
            if c == 0 {
                0
            } else {
                ((u64::from(c) * size / total) as i64).clamp(1, i64::from(cap))
            }
        })
        .collect();
    let mut sum: i64 = norm.iter().sum();
    // Hand out or take back the rounding error one unit at a time, largest counts
    // first, within [1, cap].
    let mut order: Vec<usize> = counts
        .iter()
        .enumerate()
        .filter_map(|(i, &count)| (count > 0).then_some(i))
        .collect();
    order.sort_by(|&a, &b| counts.get(b).cmp(&counts.get(a)).then(a.cmp(&b)));
    while sum != size as i64 {
        let mut moved = false;
        for &i in &order {
            if sum == size as i64 {
                break;
            }
            let Some(v) = norm.get_mut(i) else { continue };
            if sum < size as i64 && *v < i64::from(cap) {
                *v += 1;
                sum += 1;
                moved = true;
            } else if sum > size as i64 && *v > 1 {
                *v -= 1;
                sum -= 1;
                moved = true;
            }
        }
        if !moved {
            return Err(internal("distribution normalization stalled"));
        }
    }
    Ok(norm.into_iter().map(|v| v as i16).collect())
}

/// The table log for `count` symbols drawn from `distinct` values.
fn table_log(count: usize, distinct: usize, max_log: u32) -> u32 {
    let need = highbit(count.max(1) as u32) + 1;
    let floor = highbit(distinct.max(2) as u32 - 1) + 2;
    need.min(max_log).max(floor).max(FSE_MIN_LOG).min(max_log)
}

/// FSE_writeNCount: the compact distribution header (RFC 8878 §4.1.1).
fn write_ncount(norm: &[i16], log: u32, out: &mut Vec<u8>) -> io::Result<()> {
    let size = 1i64 << log;
    let mut remaining = size + 1;
    let mut threshold = size;
    let mut nb_bits = log + 1;
    let mut stream: u64 = u64::from(log - FSE_MIN_LOG);
    let mut count_bits = 4u32;
    let mut previous_zero = false;
    let mut symbol = 0usize;
    let flush = |stream: &mut u64, count_bits: &mut u32, out: &mut Vec<u8>| {
        while *count_bits >= 16 {
            out.push(*stream as u8);
            out.push((*stream >> 8) as u8);
            *stream >>= 16;
            *count_bits -= 16;
        }
    };
    while symbol < norm.len() && remaining > 1 {
        if previous_zero {
            let mut start = symbol;
            while norm.get(symbol) == Some(&0) {
                symbol += 1;
            }
            if symbol == norm.len() {
                return Err(internal("distribution ends in zeros"));
            }
            while symbol >= start + 24 {
                start += 24;
                stream |= 0xFFFFu64 << count_bits;
                count_bits += 16;
                flush(&mut stream, &mut count_bits, out);
            }
            while symbol >= start + 3 {
                start += 3;
                stream |= 3u64 << count_bits;
                count_bits += 2;
            }
            stream |= ((symbol - start) as u64) << count_bits;
            count_bits += 2;
            flush(&mut stream, &mut count_bits, out);
        }
        let count = i64::from(*norm.get(symbol).ok_or_else(|| internal("ncount"))?);
        symbol += 1;
        let max = 2 * threshold - 1 - remaining;
        remaining -= count.abs();
        let mut value = count + 1;
        if value >= threshold {
            value += max;
        }
        stream |= (value as u64) << count_bits;
        count_bits += nb_bits;
        if value < max {
            count_bits -= 1;
        }
        previous_zero = value == 1;
        if remaining < 1 {
            return Err(internal("distribution overflows its table"));
        }
        while remaining < threshold {
            nb_bits -= 1;
            threshold >>= 1;
        }
        flush(&mut stream, &mut count_bits, out);
    }
    if remaining != 1 {
        return Err(internal("distribution does not fill its table"));
    }
    while count_bits > 0 {
        out.push(stream as u8);
        stream >>= 8;
        count_bits = count_bits.saturating_sub(8);
    }
    Ok(())
}

/// Forward little-endian bit reader for distribution headers.
struct FwdReader<'a> {
    data: &'a [u8],
    bit: usize,
}

impl FwdReader<'_> {
    fn peek(&self, bits: u32) -> u64 {
        let mut v = 0u64;
        for k in 0..bits as usize {
            let i = self.bit + k;
            let b = self.data.get(i / 8).copied().unwrap_or(0);
            v |= u64::from((b >> (i % 8)) & 1) << k;
        }
        v
    }

    fn skip(&mut self, bits: u32) -> io::Result<()> {
        self.bit += bits as usize;
        if self.bit > self.data.len() * 8 {
            return Err(corrupt("distribution header truncated"));
        }
        Ok(())
    }
}

/// Decode a distribution header; returns the distribution, its log and the bytes read.
fn read_ncount(
    data: &[u8],
    max_symbols: usize,
    max_log: u32,
) -> io::Result<(Vec<i16>, u32, usize)> {
    let mut r = FwdReader { data, bit: 0 };
    let log = r.peek(4) as u32 + FSE_MIN_LOG;
    r.skip(4)?;
    if log > max_log {
        return Err(corrupt("distribution accuracy too high"));
    }
    let size = 1i64 << log;
    let mut remaining = size + 1;
    let mut threshold = size;
    let mut nb_bits = log + 1;
    let mut norm = Vec::new();
    let mut previous_zero = false;
    while remaining > 1 {
        if previous_zero {
            loop {
                let repeat = r.peek(2) as usize;
                r.skip(2)?;
                for _ in 0..repeat {
                    norm.push(0);
                }
                if repeat != 3 {
                    break;
                }
            }
        }
        let max = 2 * threshold - 1 - remaining;
        let low = r.peek(nb_bits - 1) as i64;
        let value = if low < max {
            r.skip(nb_bits - 1)?;
            low
        } else {
            let mut v = r.peek(nb_bits) as i64;
            r.skip(nb_bits)?;
            if v >= threshold {
                v -= max;
            }
            v
        };
        let count = value - 1;
        remaining -= count.abs();
        norm.push(count as i16);
        previous_zero = value == 1;
        if norm.len() > max_symbols {
            return Err(corrupt("distribution has too many symbols"));
        }
        while remaining < threshold && threshold > 1 {
            nb_bits -= 1;
            threshold >>= 1;
        }
    }
    if remaining != 1 {
        return Err(corrupt("distribution does not fill its table"));
    }
    Ok((norm, log, r.bit.div_ceil(8)))
}

// ---- literals -------------------------------------------------------------------------

struct Huffman {
    /// Per symbol: (code, length); length 0 for absent symbols.
    codes: Vec<(u32, u32)>,
    /// Weights of symbols 0..last (the last present symbol's weight is implied).
    weights: Vec<u8>,
}

/// Code lengths for `counts`, at most `HUF_MAX_BITS`, by Huffman's algorithm on
/// counts flattened until the limit holds.
fn huffman_lengths(counts: &[u32]) -> Vec<u32> {
    let mut weights: Vec<u64> = counts.iter().map(|&c| u64::from(c)).collect();
    loop {
        let lengths = huffman_once(&weights);
        if lengths.iter().all(|&l| l <= HUF_MAX_BITS) {
            return lengths;
        }
        for w in weights.iter_mut().filter(|w| **w > 0) {
            *w = (*w).div_ceil(2);
        }
    }
}

fn huffman_once(weights: &[u64]) -> Vec<u32> {
    // Nodes: leaves first, then internal nodes; parent links give depths.
    let mut leaves: Vec<(u64, usize)> = weights
        .iter()
        .enumerate()
        .filter(|(_, &w)| w > 0)
        .map(|(s, &w)| (w, s))
        .collect();
    leaves.sort();
    let n = leaves.len();
    let mut parent = vec![usize::MAX; 2 * n];
    let mut internal_w: Vec<u64> = Vec::with_capacity(n);
    let (mut li, mut ii) = (0usize, 0usize);
    let take = |li: &mut usize, ii: &mut usize, internal_w: &Vec<u64>| -> (u64, usize) {
        let leaf = leaves.get(*li).map(|&(w, _)| w);
        let node = internal_w.get(*ii).copied();
        match (leaf, node) {
            (Some(a), Some(b)) if a <= b => {
                *li += 1;
                (a, *li - 1)
            }
            (Some(a), None) => {
                *li += 1;
                (a, *li - 1)
            }
            (_, Some(b)) => {
                *ii += 1;
                (b, n + *ii - 1)
            }
            (None, None) => (0, usize::MAX),
        }
    };
    for k in 0..n.saturating_sub(1) {
        let (wa, a) = take(&mut li, &mut ii, &internal_w);
        let (wb, b) = take(&mut li, &mut ii, &internal_w);
        internal_w.push(wa + wb);
        if let Some(p) = parent.get_mut(a) {
            *p = n + k;
        }
        if let Some(p) = parent.get_mut(b) {
            *p = n + k;
        }
    }
    let mut lengths = vec![0u32; weights.len()];
    for (idx, &(_, s)) in leaves.iter().enumerate() {
        let mut depth = 0;
        let mut at = idx;
        while let Some(&p) = parent.get(at) {
            if p == usize::MAX {
                break;
            }
            depth += 1;
            at = p;
        }
        if let Some(l) = lengths.get_mut(s) {
            *l = depth;
        }
    }
    lengths
}

impl Huffman {
    fn build(counts: &[u32]) -> Option<Self> {
        if counts.iter().filter(|&&c| c > 0).count() < 2 {
            return None;
        }
        let lengths = huffman_lengths(counts);
        let max_bits = lengths.iter().copied().max().unwrap_or(0);
        let last = lengths.iter().rposition(|&l| l > 0)?;
        let weight = |l: u32| if l == 0 { 0 } else { (max_bits + 1 - l) as u8 };
        let all_weights: Vec<u8> = lengths.iter().map(|&l| weight(l)).collect();
        let codes = assign_codes(&all_weights, max_bits);
        Some(Self {
            codes,
            weights: all_weights.get(..last).unwrap_or(&[]).to_vec(),
        })
    }
}

/// Prefix codes from weights (RFC 8878 §4.2.1.3): longest codes first, symbols in
/// increasing order within a length, each taking `2^(weight-1)` table entries.
fn assign_codes(weights: &[u8], max_bits: u32) -> Vec<(u32, u32)> {
    let mut codes = vec![(0u32, 0u32); weights.len()];
    let mut entry = 0u32;
    for w in 1..=max_bits as u8 {
        for (s, &sw) in weights.iter().enumerate() {
            if sw == w {
                if let Some(c) = codes.get_mut(s) {
                    *c = (entry >> (w - 1), max_bits + 1 - u32::from(w));
                }
                entry += 1 << (w - 1);
            }
        }
    }
    codes
}

/// The Huffman tree description: direct 4-bit weights or FSE-coded weights.
fn write_huffman_tree(h: &Huffman, out: &mut Vec<u8>) -> io::Result<()> {
    let fse = fse_weights(&h.weights);
    let direct_ok = h.weights.len() <= 128;
    match fse {
        Some(body) if !direct_ok || body.len() < h.weights.len().div_ceil(2) => {
            out.push(body.len() as u8);
            out.extend_from_slice(&body);
        }
        _ if direct_ok => {
            out.push(127 + h.weights.len() as u8);
            for pair in h.weights.chunks(2) {
                let hi = pair.first().copied().unwrap_or(0);
                let lo = pair.get(1).copied().unwrap_or(0);
                out.push((hi << 4) | lo);
            }
        }
        _ => return Err(internal("Huffman weights cannot be described")),
    }
    Ok(())
}

/// FSE-coded weights with two interleaved states, or None when that cannot work.
fn fse_weights(weights: &[u8]) -> Option<Vec<u8>> {
    if weights.len() < 2 {
        return None;
    }
    let mut counts = vec![0u32; usize::from(*weights.iter().max()?) + 1];
    for &w in weights {
        *counts.get_mut(usize::from(w))? += 1;
    }
    let log = HUF_WEIGHT_LOG;
    // A symbol owning more than half the table has zero-bit transitions, and one at
    // the end would hide where the stream stops; at most half keeps every read >= 1.
    let norm = normalize(&counts, log, (1 << log) / 2).ok()?;
    let table = CTable::build(&norm, log).ok()?;
    let mut out = Vec::new();
    write_ncount(&norm, log, &mut out).ok()?;
    let mut w = BitWriter::new();
    let mut i = weights.len();
    let mut s1;
    let mut s2;
    if weights.len() % 2 == 1 {
        s1 = table.init(*weights.get(i - 1)?).ok()?;
        s2 = table.init(*weights.get(i - 2)?).ok()?;
        table.encode(&mut w, &mut s1, *weights.get(i - 3)?).ok()?;
        i -= 3;
    } else {
        s2 = table.init(*weights.get(i - 1)?).ok()?;
        s1 = table.init(*weights.get(i - 2)?).ok()?;
        i -= 2;
    }
    while i >= 2 {
        table.encode(&mut w, &mut s2, *weights.get(i - 1)?).ok()?;
        table.encode(&mut w, &mut s1, *weights.get(i - 2)?).ok()?;
        i -= 2;
    }
    table.flush(&mut w, s2);
    table.flush(&mut w, s1);
    out.extend_from_slice(&w.close());
    (out.len() < 128).then_some(out)
}

fn huffman_stream(h: &Huffman, symbols: &[u8]) -> io::Result<Vec<u8>> {
    let mut w = BitWriter::new();
    for &s in symbols.iter().rev() {
        let &(code, len) = h
            .codes
            .get(usize::from(s))
            .ok_or_else(|| internal("literal outside its code"))?;
        if len == 0 {
            return Err(internal("literal without a code"));
        }
        w.add(u64::from(code), len);
    }
    Ok(w.close())
}

fn raw_literals_header(kind: u8, size: usize, out: &mut Vec<u8>) {
    if size <= 31 {
        out.push(kind | ((size as u8) << 3));
    } else if size <= 4095 {
        out.push(kind | (1 << 2) | (((size & 0xF) as u8) << 4));
        out.push((size >> 4) as u8);
    } else {
        out.push(kind | (3 << 2) | (((size & 0xF) as u8) << 4));
        out.push((size >> 4) as u8);
        out.push((size >> 12) as u8);
    }
}

fn encode_literals(lit: &[u8], out: &mut Vec<u8>) -> io::Result<()> {
    if let Some(&first) = lit.first() {
        if lit.len() > 1 && lit.iter().all(|&b| b == first) {
            raw_literals_header(LIT_RLE, lit.len(), out);
            out.push(first);
            return Ok(());
        }
    }
    if lit.len() >= MIN_HUFFMAN_LITERALS {
        if let Some(section) = huffman_literals(lit)? {
            let raw_cost = lit.len() + 3;
            if section.len() < raw_cost {
                out.extend_from_slice(&section);
                return Ok(());
            }
        }
    }
    raw_literals_header(LIT_RAW, lit.len(), out);
    out.extend_from_slice(lit);
    Ok(())
}

fn huffman_literals(lit: &[u8]) -> io::Result<Option<Vec<u8>>> {
    let mut counts = vec![0u32; 256];
    for &b in lit {
        if let Some(c) = counts.get_mut(usize::from(b)) {
            *c += 1;
        }
    }
    let Some(h) = Huffman::build(&counts) else {
        return Ok(None);
    };
    let mut tree = Vec::new();
    if write_huffman_tree(&h, &mut tree).is_err() {
        return Ok(None);
    }
    let four = lit.len() > 1023;
    let mut payload = tree;
    if four {
        let seg = lit.len().div_ceil(4);
        let mut streams = Vec::with_capacity(4);
        for k in 0..4 {
            let start = (k * seg).min(lit.len());
            let end = ((k + 1) * seg).min(lit.len());
            streams.push(huffman_stream(&h, lit.get(start..end).unwrap_or(&[]))?);
        }
        for s in streams.iter().take(3) {
            let n = u16::try_from(s.len()).map_err(|_| internal("literal stream too long"))?;
            payload.extend_from_slice(&n.to_le_bytes());
        }
        for s in &streams {
            payload.extend_from_slice(s);
        }
    } else {
        payload.extend_from_slice(&huffman_stream(&h, lit)?);
    }
    let regen = lit.len();
    let comp = payload.len();
    let (sf, field_bits, header_len) = if !four {
        if comp > 1023 {
            return Ok(None);
        }
        (0u64, 10u32, 3usize)
    } else if regen <= 1023 && comp <= 1023 {
        (1, 10, 3)
    } else if regen <= 16383 && comp <= 16383 {
        (2, 14, 4)
    } else if regen <= 262_143 && comp <= 262_143 {
        (3, 18, 5)
    } else {
        return Ok(None);
    };
    let header = u64::from(LIT_COMPRESSED)
        | (sf << 2)
        | ((regen as u64) << 4)
        | ((comp as u64) << (4 + field_bits));
    let mut out = Vec::with_capacity(header_len + comp);
    out.extend_from_slice(header.to_le_bytes().get(..header_len).unwrap_or(&[]));
    out.extend_from_slice(&payload);
    Ok(Some(out))
}

// ---- sequences ------------------------------------------------------------------------

fn code_for(value: u32, bases: &[u32]) -> usize {
    bases.iter().rposition(|&b| b <= value).unwrap_or(0)
}

struct Coded {
    ll: Vec<u8>,
    ml: Vec<u8>,
    of: Vec<u8>,
}

/// One symbol stream's table choice: its mode byte bits, encoder and description.
struct Choice {
    mode: u8,
    table: CTable,
    description: Vec<u8>,
}

fn choose_table(
    symbols: &[u8],
    default: &[i16],
    default_log: u32,
    max_log: u32,
) -> io::Result<Choice> {
    let alphabet = default.len();
    let mut counts = vec![0u32; alphabet];
    for &s in symbols {
        *counts
            .get_mut(usize::from(s))
            .ok_or_else(|| internal("sequence code outside its alphabet"))? += 1;
    }
    let distinct = counts.iter().filter(|&&c| c > 0).count();
    if distinct == 1 {
        // An RLE table codes every symbol in zero bits; its encoder is never used.
        let s = counts.iter().position(|&c| c > 0).unwrap_or(0) as u8;
        return Ok(Choice {
            mode: MODE_RLE,
            table: CTable {
                log: 0,
                states: Vec::new(),
                symbols: Vec::new(),
            },
            description: vec![s],
        });
    }
    let predefined = CTable::build(default, default_log)?;
    let predefined_cost = cost_bits(&counts, default, default_log);
    let last = counts.iter().rposition(|&c| c > 0).unwrap_or(0);
    let log = table_log(symbols.len(), distinct, max_log);
    if let Ok(norm) = normalize(counts.get(..=last).unwrap_or(&[]), log, (1 << log) - 1) {
        let mut description = Vec::new();
        if write_ncount(&norm, log, &mut description).is_ok() {
            let custom_cost = cost_bits(&counts, &norm, log) + description.len() as u64 * 8;
            if custom_cost < predefined_cost {
                return Ok(Choice {
                    mode: MODE_FSE,
                    table: CTable::build(&norm, log)?,
                    description,
                });
            }
        }
    }
    Ok(Choice {
        mode: MODE_PREDEFINED,
        table: predefined,
        description: Vec::new(),
    })
}

/// Approximate bits to code `counts` under distribution `norm`.
fn cost_bits(counts: &[u32], norm: &[i16], log: u32) -> u64 {
    let mut bits = 0f64;
    let size = f64::from(1u32 << log);
    for (s, &c) in counts.iter().enumerate() {
        if c == 0 {
            continue;
        }
        let p = match norm.get(s) {
            Some(&n) if n != 0 => f64::from(n.unsigned_abs()) / size,
            _ => return u64::MAX / 2,
        };
        bits += f64::from(c) * -p.log2();
    }
    bits as u64
}

fn encode_sequences(seqs: &[Seq], out: &mut Vec<u8>) -> io::Result<()> {
    let n = seqs.len();
    if n < 128 {
        out.push(n as u8);
    } else if n < 0x7F00 {
        out.push(((n >> 8) + 128) as u8);
        out.push(n as u8);
    } else {
        out.push(255);
        out.extend_from_slice(&((n - 0x7F00) as u16).to_le_bytes());
    }
    if n == 0 {
        return Ok(());
    }
    let mut coded = Coded {
        ll: Vec::with_capacity(n),
        ml: Vec::with_capacity(n),
        of: Vec::with_capacity(n),
    };
    for s in seqs {
        coded.ll.push(code_for(s.lit_len, LL_BASE) as u8);
        coded.ml.push(code_for(s.match_len, ML_BASE) as u8);
        coded.of.push(highbit(s.of_value) as u8);
    }
    let ll = choose_table(&coded.ll, LL_DEFAULT, LL_DEFAULT_LOG, LL_MAX_LOG)?;
    let of = choose_table(&coded.of, OF_DEFAULT, OF_DEFAULT_LOG, OF_MAX_LOG)?;
    let ml = choose_table(&coded.ml, ML_DEFAULT, ML_DEFAULT_LOG, ML_MAX_LOG)?;
    out.push((ll.mode << 6) | (of.mode << 4) | (ml.mode << 2));
    out.extend_from_slice(&ll.description);
    out.extend_from_slice(&of.description);
    out.extend_from_slice(&ml.description);

    let mut w = BitWriter::new();
    let last = n - 1;
    let code = |v: &[u8], i: usize| v.get(i).copied().ok_or_else(|| internal("sequence code"));
    let mut st_ml = init_state(&ml, code(&coded.ml, last)?)?;
    let mut st_of = init_state(&of, code(&coded.of, last)?)?;
    let mut st_ll = init_state(&ll, code(&coded.ll, last)?)?;
    let extras = |w: &mut BitWriter, i: usize| -> io::Result<()> {
        let s = seqs.get(i).ok_or_else(|| internal("sequence"))?;
        let llc = usize::from(code(&coded.ll, i)?);
        let mlc = usize::from(code(&coded.ml, i)?);
        let ofc = u32::from(code(&coded.of, i)?);
        let ll_base = LL_BASE.get(llc).copied().unwrap_or(0);
        let ml_base = ML_BASE.get(mlc).copied().unwrap_or(0);
        w.add(
            u64::from(s.lit_len - ll_base),
            LL_BITS.get(llc).copied().unwrap_or(0),
        );
        w.add(
            u64::from(s.match_len - ml_base),
            ML_BITS.get(mlc).copied().unwrap_or(0),
        );
        w.add(u64::from(s.of_value - (1 << ofc)), ofc);
        Ok(())
    };
    extras(&mut w, last)?;
    for i in (0..last).rev() {
        encode_state(&of, &mut w, &mut st_of, code(&coded.of, i)?)?;
        encode_state(&ml, &mut w, &mut st_ml, code(&coded.ml, i)?)?;
        encode_state(&ll, &mut w, &mut st_ll, code(&coded.ll, i)?)?;
        extras(&mut w, i)?;
    }
    flush_state(&ml, &mut w, st_ml);
    flush_state(&of, &mut w, st_of);
    flush_state(&ll, &mut w, st_ll);
    out.extend_from_slice(&w.close());
    Ok(())
}

fn init_state(c: &Choice, s: u8) -> io::Result<u32> {
    if c.mode == MODE_RLE {
        return Ok(0);
    }
    c.table.init(s)
}

fn encode_state(c: &Choice, w: &mut BitWriter, state: &mut u32, s: u8) -> io::Result<()> {
    if c.mode == MODE_RLE {
        return Ok(());
    }
    c.table.encode(w, state, s)
}

fn flush_state(c: &Choice, w: &mut BitWriter, state: u32) {
    if c.mode != MODE_RLE {
        c.table.flush(w, state);
    }
}

// ---- decoder ----------------------------------------------------------------------------

struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        let end = self.pos.checked_add(n).ok_or_else(|| corrupt("length"))?;
        let s = self
            .data
            .get(self.pos..end)
            .ok_or_else(|| corrupt("truncated"))?;
        self.pos = end;
        Ok(s)
    }

    fn byte(&mut self) -> io::Result<u8> {
        Ok(*self.take(1)?.first().ok_or_else(|| corrupt("truncated"))?)
    }

    fn le(&mut self, n: usize) -> io::Result<u64> {
        let b = self.take(n)?;
        Ok(b.iter()
            .rev()
            .fold(0u64, |acc, &x| (acc << 8) | u64::from(x)))
    }
}

/// Decode one frame. Only what a single-frame, dictionary-free stream needs is
/// supported; anything else is refused rather than guessed.
pub fn decompress(frame: &[u8], max_out: usize) -> io::Result<Vec<u8>> {
    let mut c = Cursor {
        data: frame,
        pos: 0,
    };
    if c.le(4)? != u64::from(MAGIC) {
        return Err(corrupt("bad magic"));
    }
    let fhd = c.byte()?;
    let fcs_flag = fhd >> 6;
    let single = fhd & 0x20 != 0;
    if fhd & 0x08 != 0 {
        return Err(corrupt("reserved frame header bit set"));
    }
    let checksum = fhd & 0x04 != 0;
    let dict_bytes = match fhd & 3 {
        0 => 0,
        1 => 1,
        2 => 2,
        _ => 4,
    };
    if !single {
        c.byte()?; // Window_Descriptor; the output bound below is what matters here.
    }
    if c.le(dict_bytes)? != 0 {
        return Err(corrupt("dictionaries are not supported"));
    }
    let content = match (fcs_flag, single) {
        (0, true) => Some(c.le(1)?),
        (0, false) => None,
        (1, _) => Some(c.le(2)? + 256),
        (2, _) => Some(c.le(4)?),
        _ => Some(c.le(8)?),
    };
    let mut out: Vec<u8> = Vec::new();
    let mut rep = [1usize, 4, 8];
    loop {
        let h = c.le(3)? as u32;
        let last = h & 1 != 0;
        let kind = (h >> 1) & 3;
        let size = (h >> 3) as usize;
        if size > MAX_INPUT {
            return Err(corrupt("block too large"));
        }
        match kind {
            BLOCK_RAW => out.extend_from_slice(c.take(size)?),
            BLOCK_RLE => {
                let b = c.byte()?;
                out.resize(out.len() + size, b);
            }
            BLOCK_COMPRESSED => {
                let body = c.take(size)?;
                decode_block(body, &mut out, &mut rep, max_out)?;
            }
            _ => return Err(corrupt("reserved block type")),
        }
        if out.len() > max_out {
            return Err(corrupt("output exceeds its bound"));
        }
        if last {
            break;
        }
    }
    if checksum {
        c.take(4)?;
    }
    if c.pos != frame.len() {
        return Err(corrupt("trailing bytes after the frame"));
    }
    if let Some(n) = content {
        if n != out.len() as u64 {
            return Err(corrupt("content size mismatch"));
        }
    }
    Ok(out)
}

fn decode_block(
    body: &[u8],
    out: &mut Vec<u8>,
    rep: &mut [usize; 3],
    max_out: usize,
) -> io::Result<()> {
    let mut c = Cursor { data: body, pos: 0 };
    let literals = decode_literals(&mut c)?;
    let seq_data = body
        .get(c.pos..)
        .ok_or_else(|| corrupt("sequence section"))?;
    let seqs = decode_sequences(seq_data)?;
    let mut lit_pos = 0usize;
    for (ll, ml, ofv) in seqs {
        let lit_end = lit_pos
            .checked_add(ll)
            .filter(|&e| e <= literals.len())
            .ok_or_else(|| corrupt("literal length past the literals"))?;
        out.extend_from_slice(literals.get(lit_pos..lit_end).unwrap_or(&[]));
        lit_pos = lit_end;
        let offset = if ofv > 3 {
            let o = ofv - 3;
            *rep = [o, rep[0], rep[1]];
            o
        } else {
            let idx = ofv - 1 + usize::from(ll == 0);
            match idx {
                0 => rep[0],
                1 => {
                    *rep = [rep[1], rep[0], rep[2]];
                    rep[0]
                }
                2 => {
                    *rep = [rep[2], rep[0], rep[1]];
                    rep[0]
                }
                _ => {
                    let o = rep[0]
                        .checked_sub(1)
                        .filter(|&o| o > 0)
                        .ok_or_else(|| corrupt("zero offset"))?;
                    *rep = [o, rep[0], rep[1]];
                    o
                }
            }
        };
        if offset == 0 || offset > out.len() {
            return Err(corrupt("offset reaches before the frame"));
        }
        if out.len() + ml > max_out {
            return Err(corrupt("output exceeds its bound"));
        }
        let start = out.len() - offset;
        for k in 0..ml {
            let b = *out.get(start + k).ok_or_else(|| corrupt("match copy"))?;
            out.push(b);
        }
    }
    out.extend_from_slice(literals.get(lit_pos..).unwrap_or(&[]));
    Ok(())
}

fn decode_literals(c: &mut Cursor) -> io::Result<Vec<u8>> {
    let b0 = c.byte()?;
    let kind = b0 & 3;
    let sf = (b0 >> 2) & 3;
    match kind {
        LIT_RAW | LIT_RLE => {
            let size = match sf {
                0 | 2 => usize::from(b0 >> 3),
                1 => usize::from(b0 >> 4) | (usize::from(c.byte()?) << 4),
                _ => {
                    usize::from(b0 >> 4)
                        | (usize::from(c.byte()?) << 4)
                        | (usize::from(c.byte()?) << 12)
                }
            };
            if size > MAX_INPUT {
                return Err(corrupt("literals too large"));
            }
            if kind == LIT_RAW {
                Ok(c.take(size)?.to_vec())
            } else {
                Ok(vec![c.byte()?; size])
            }
        }
        LIT_COMPRESSED => {
            let (header_len, bits) = match sf {
                0 | 1 => (3usize, 10u32),
                2 => (4, 14),
                _ => (5, 18),
            };
            let mut h = u64::from(b0);
            for k in 1..header_len {
                h |= u64::from(c.byte()?) << (8 * k);
            }
            let mask = (1u64 << bits) - 1;
            let regen = ((h >> 4) & mask) as usize;
            let comp = ((h >> (4 + bits)) & mask) as usize;
            if regen > MAX_INPUT {
                return Err(corrupt("literals too large"));
            }
            let data = c.take(comp)?;
            decode_huffman_literals(data, regen, sf != 0)
        }
        _ => Err(corrupt("treeless literals need a previous table")),
    }
}

struct HufTable {
    max_bits: u32,
    entries: Vec<(u8, u8)>,
}

fn read_huffman_tree(data: &[u8]) -> io::Result<(HufTable, usize)> {
    let header = usize::from(*data.first().ok_or_else(|| corrupt("Huffman header"))?);
    let (weights, used) = if header < 128 {
        let body = data
            .get(1..1 + header)
            .ok_or_else(|| corrupt("Huffman weights truncated"))?;
        (decode_fse_weights(body)?, 1 + header)
    } else {
        let n = header - 127;
        let bytes = data
            .get(1..1 + n.div_ceil(2))
            .ok_or_else(|| corrupt("Huffman weights truncated"))?;
        let mut w = Vec::with_capacity(n);
        for k in 0..n {
            let b = bytes.get(k / 2).copied().unwrap_or(0);
            w.push(if k % 2 == 0 { b >> 4 } else { b & 15 });
        }
        (w, 1 + n.div_ceil(2))
    };
    let mut total = 0u32;
    for &w in &weights {
        if w > 12 {
            return Err(corrupt("Huffman weight too large"));
        }
        if w > 0 {
            total += 1 << (w - 1);
        }
    }
    if total == 0 {
        return Err(corrupt("Huffman weights are all zero"));
    }
    let max_bits = highbit(total) + 1;
    if max_bits > HUF_MAX_BITS {
        return Err(corrupt("Huffman code too long"));
    }
    let rest = (1u32 << max_bits) - total;
    if !rest.is_power_of_two() {
        return Err(corrupt("Huffman weights are not complete"));
    }
    let mut all = weights;
    all.push((highbit(rest) + 1) as u8);
    if all.len() > 256 {
        return Err(corrupt("too many Huffman symbols"));
    }
    let mut entries = vec![(0u8, 0u8); 1 << max_bits];
    let mut entry = 0usize;
    for w in 1..=max_bits as u8 {
        for (s, &sw) in all.iter().enumerate() {
            if sw == w {
                let span = 1usize << (w - 1);
                for e in entries
                    .get_mut(entry..entry + span)
                    .ok_or_else(|| corrupt("Huffman table overflow"))?
                {
                    *e = (s as u8, (max_bits + 1 - u32::from(w)) as u8);
                }
                entry += span;
            }
        }
    }
    if entry != entries.len() {
        return Err(corrupt("Huffman table incomplete"));
    }
    Ok((HufTable { max_bits, entries }, used))
}

/// The two-state FSE weight stream, ending exactly as the reference decoder does:
/// when a state update reads past the start, the other state's symbol is the last.
fn decode_fse_weights(data: &[u8]) -> io::Result<Vec<u8>> {
    let (norm, log, used) = read_ncount(data, 13, HUF_WEIGHT_LOG)?;
    let table = DTable::build(&norm, log)?;
    let mut r = BitReader::new(data.get(used..).ok_or_else(|| corrupt("weights"))?)?;
    let mut s1 = r.read(log) as usize;
    let mut s2 = r.read(log) as usize;
    let mut out = Vec::new();
    loop {
        if out.len() > 254 {
            return Err(corrupt("too many Huffman weights"));
        }
        let e = table.get(s1)?;
        out.push(e.symbol);
        s1 = usize::from(e.base) + r.read(u32::from(e.bits)) as usize;
        if r.overflowed() {
            out.push(table.get(s2)?.symbol);
            break;
        }
        let e = table.get(s2)?;
        out.push(e.symbol);
        s2 = usize::from(e.base) + r.read(u32::from(e.bits)) as usize;
        if r.overflowed() {
            out.push(table.get(s1)?.symbol);
            break;
        }
    }
    Ok(out)
}

fn decode_huffman_literals(data: &[u8], regen: usize, four: bool) -> io::Result<Vec<u8>> {
    let (table, used) = read_huffman_tree(data)?;
    let rest = data.get(used..).ok_or_else(|| corrupt("literal streams"))?;
    let mut out = Vec::with_capacity(regen);
    if !four {
        huffman_decode_stream(&table, rest, regen, &mut out)?;
        return Ok(out);
    }
    let mut c = Cursor { data: rest, pos: 0 };
    let sizes = [c.le(2)? as usize, c.le(2)? as usize, c.le(2)? as usize];
    let seg = regen.div_ceil(4);
    let last = regen
        .checked_sub(3 * seg)
        .ok_or_else(|| corrupt("too few literals for four streams"))?;
    for n in sizes {
        huffman_decode_stream(&table, c.take(n)?, seg, &mut out)?;
    }
    let s = rest
        .get(c.pos..)
        .ok_or_else(|| corrupt("fourth literal stream"))?;
    huffman_decode_stream(&table, s, last, &mut out)?;
    Ok(out)
}

fn huffman_decode_stream(t: &HufTable, data: &[u8], n: usize, out: &mut Vec<u8>) -> io::Result<()> {
    let mut r = BitReader::new(data)?;
    for _ in 0..n {
        let (s, bits) = *t
            .entries
            .get(r.peek(t.max_bits) as usize)
            .ok_or_else(|| corrupt("Huffman lookup"))?;
        r.read(u32::from(bits));
        if r.overflowed() {
            return Err(corrupt("literal stream overrun"));
        }
        out.push(s);
    }
    if !r.finished() {
        return Err(corrupt("literal stream not fully consumed"));
    }
    Ok(())
}

fn decode_table(
    mode: u8,
    c: &mut Cursor,
    default: &[i16],
    default_log: u32,
    max_log: u32,
) -> io::Result<DTable> {
    match mode {
        MODE_PREDEFINED => DTable::build(default, default_log),
        MODE_RLE => {
            let s = c.byte()?;
            if usize::from(s) >= default.len() {
                return Err(corrupt("RLE symbol outside its alphabet"));
            }
            Ok(DTable::rle(s))
        }
        MODE_FSE => {
            let rest = c.data.get(c.pos..).ok_or_else(|| corrupt("table"))?;
            let (norm, log, used) = read_ncount(rest, default.len(), max_log)?;
            c.take(used)?;
            DTable::build(&norm, log)
        }
        _ => Err(corrupt("repeat tables need a previous block")),
    }
}

fn decode_sequences(data: &[u8]) -> io::Result<Vec<(usize, usize, usize)>> {
    let mut c = Cursor { data, pos: 0 };
    let b0 = usize::from(c.byte()?);
    let n = if b0 == 0 {
        return Ok(Vec::new());
    } else if b0 < 128 {
        b0
    } else if b0 < 255 {
        ((b0 - 128) << 8) + usize::from(c.byte()?)
    } else {
        c.le(2)? as usize + 0x7F00
    };
    let modes = c.byte()?;
    if modes & 3 != 0 {
        return Err(corrupt("reserved sequence mode bits"));
    }
    let ll = decode_table(modes >> 6, &mut c, LL_DEFAULT, LL_DEFAULT_LOG, LL_MAX_LOG)?;
    let of = decode_table(
        (modes >> 4) & 3,
        &mut c,
        OF_DEFAULT,
        OF_DEFAULT_LOG,
        OF_MAX_LOG,
    )?;
    let ml = decode_table(
        (modes >> 2) & 3,
        &mut c,
        ML_DEFAULT,
        ML_DEFAULT_LOG,
        ML_MAX_LOG,
    )?;
    let mut r = BitReader::new(
        data.get(c.pos..)
            .ok_or_else(|| corrupt("sequence stream"))?,
    )?;
    let mut sll = r.read(ll.log) as usize;
    let mut sof = r.read(of.log) as usize;
    let mut sml = r.read(ml.log) as usize;
    let mut seqs = Vec::with_capacity(n);
    for i in 0..n {
        let el = ll.get(sll)?;
        let eo = of.get(sof)?;
        let em = ml.get(sml)?;
        let ofc = u32::from(eo.symbol);
        if ofc as usize > OF_MAX_CODE {
            return Err(corrupt("offset code too large"));
        }
        let ofv = (1usize << ofc) + r.read(ofc) as usize;
        let mlc = usize::from(em.symbol);
        let ml_len = ML_BASE
            .get(mlc)
            .copied()
            .ok_or_else(|| corrupt("match code"))? as usize
            + r.read(ML_BITS.get(mlc).copied().unwrap_or(0)) as usize;
        let llc = usize::from(el.symbol);
        let ll_len = LL_BASE
            .get(llc)
            .copied()
            .ok_or_else(|| corrupt("literal code"))? as usize
            + r.read(LL_BITS.get(llc).copied().unwrap_or(0)) as usize;
        seqs.push((ll_len, ml_len, ofv));
        if i + 1 < n {
            sll = usize::from(el.base) + r.read(u32::from(el.bits)) as usize;
            sml = usize::from(em.base) + r.read(u32::from(em.bits)) as usize;
            sof = usize::from(eo.base) + r.read(u32::from(eo.bits)) as usize;
        }
        if r.overflowed() {
            return Err(corrupt("sequence stream overrun"));
        }
    }
    if !r.finished() {
        return Err(corrupt("sequence stream not fully consumed"));
    }
    Ok(seqs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lcg_text(n: usize, seed: u64) -> Vec<u8> {
        // Word-like text with repetition: what binaries and configs look like to a
        // matcher, without a fixture file.
        const WORDS: &[&[u8]] = &[
            b"td",
            b"store",
            b"/lib/",
            b"x86_64",
            b"\0\0\0\0",
            b"mount",
            b"erofs",
            b"zstd",
            b"\x7fELF",
            b"\xff\xfe",
            b"recipe",
            b" ",
            b"\n",
            b"build",
            b"0123456789",
        ];
        let mut s = seed;
        let mut out = Vec::with_capacity(n);
        while out.len() < n {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let w = WORDS[(s >> 33) as usize % WORDS.len()];
            out.extend_from_slice(w);
            if (s >> 20) % 7 == 0 {
                out.push((s >> 40) as u8);
            }
        }
        out.truncate(n);
        out
    }

    fn noise(n: usize, seed: u64) -> Vec<u8> {
        let mut s = seed;
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                s as u8
            })
            .collect()
    }

    fn roundtrip(data: &[u8]) -> Vec<u8> {
        let frame = compress(data).unwrap();
        assert_eq!(decompress(&frame, data.len()).unwrap(), data);
        frame
    }

    #[test]
    fn empty_and_tiny_inputs_roundtrip() {
        for n in [0usize, 1, 2, 3, 4, 5, 31, 32, 255, 256, 257] {
            roundtrip(&lcg_text(n, n as u64));
        }
    }

    #[test]
    fn rle_input_is_one_byte_block() {
        let frame = roundtrip(&[7u8; 100_000]);
        assert!(frame.len() < 16, "RLE frame is {} bytes", frame.len());
    }

    #[test]
    fn text_compresses_and_roundtrips_at_every_size_class() {
        for n in [
            1000usize, 1023, 1024, 4096, 16_383, 16_384, 65_535, 65_792, 131_072,
        ] {
            let data = lcg_text(n, 7 + n as u64);
            let frame = roundtrip(&data);
            assert!(
                frame.len() < n / 2,
                "{n} bytes compressed to {}",
                frame.len()
            );
        }
    }

    #[test]
    fn noise_is_stored_raw() {
        let data = noise(50_000, 3);
        let frame = roundtrip(&data);
        assert!(frame.len() <= data.len() + 12);
    }

    #[test]
    fn mixed_noise_and_text_roundtrips() {
        let mut data = noise(20_000, 9);
        data.extend_from_slice(&lcg_text(60_000, 4));
        data.extend_from_slice(&noise(30_000, 11));
        data.extend_from_slice(&data.clone()[..20_000]);
        roundtrip(&data);
    }

    #[test]
    fn oversized_input_is_refused() {
        assert!(compress(&vec![0u8; MAX_INPUT + 1]).is_err());
    }

    #[test]
    fn decoder_refuses_corruption() {
        let data = lcg_text(40_000, 1);
        let frame = compress(&data).unwrap();
        let mut refused = 0;
        for i in (5..frame.len()).step_by(37) {
            let mut bad = frame.clone();
            bad[i] ^= 0x5A;
            if decompress(&bad, data.len()).map_or(true, |o| o != data) {
                refused += 1;
            }
        }
        assert!(refused > 0);
        assert!(decompress(&frame[..frame.len() - 1], data.len()).is_err());
    }

    #[test]
    fn ncount_roundtrips() {
        for (counts, log) in [
            (
                vec![
                    5u32, 0, 0, 0, 1, 9, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                    0, 0, 0, 0, 0, 3,
                ],
                6,
            ),
            (vec![1u32; 36], 6),
            (vec![100u32, 1], 5),
            ((0..53).map(|i| (i * 7 % 11) as u32).collect(), 9),
        ] {
            let norm = normalize(&counts, log, (1 << log) - 1).unwrap();
            let mut buf = Vec::new();
            write_ncount(&norm, log, &mut buf).unwrap();
            let (back, blog, used) = read_ncount(&buf, 64, 9).unwrap();
            assert_eq!(blog, log);
            assert_eq!(used, buf.len());
            let mut trimmed = norm.clone();
            while trimmed.last() == Some(&0) {
                trimmed.pop();
            }
            assert_eq!(back, trimmed);
        }
    }

    /// Frames of `lcg_text(12_000, 42)` made by the reference implementation
    /// (zstd 1.5.7, `zstd -19` and `zstd -1`): a content checksum, four Huffman
    /// literal streams with FSE-coded weights and FSE tables for all three sequence
    /// streams, decoded here independently of this module's encoder.
    const REFERENCE_19: &[&str] = &[
        "28b52ffd64e02d0d650026ada2416068513aeb60d215a9d8a5488fb265c808d1957f359aefe7d00b81b5e174",
        "a75d1d61b204f186e95987ae901e3569bef1b06daff8ea470ec776c66d40ce8d55b0029100920092006b9ce1",
        "434e2a680d367d5aef3d36df8b980969ec73cffce95f7c377340ebd7b9c5a44b483ded8f716e5277eeb93d5f",
        "09adcd73bb9656dd974eebb59cfb31704e2bad3f5748478acf2dfec8436b8f332a0c56431c18aa8a35885842",
        "5acfb54dab1c13bf1246323b863484583c3f10b13ab288397eae108bb8c50f3c0fbb0e3a0e1b3464d0ed66ec",
        "783e4e1cc32cab2f0110c3017dea44d0333cf767415aa1dc9ffaed4772ce8fcd340f5a4debefc6b1e6a7c2ce",
        "e53a3bf1304f3409816b67926e303e0d5dd0a2f5cbc9b94d3e628e4024179f0963f92856ec23c05f08c47728",
        "db0479c9a1b548852ffb60b42a58b2a998149455d9f064964363af91d4a08a0dad47e32300772ef9550b34a6",
        "5fe47ab75202e11d201865dc58cb3dcce836ec3128e79e0107b0759f5f63e1a4ed68b4963d782e9ed3eae4a5",
        "8a75a1f56c9a0cbc8b5e8d14ad429f7ba3f5f6e6486b4d6cc7b9c71560671a49e866925e63323f6d0e287f9b",
        "c20928c9ece49c4c55f8835c39c184eb099058e9261cb5f9cdec67c48cc232508458c43c57c29eabece8fb8a",
        "56a76339d4168f605aa680b06ce4071973e00c710871f9694d5b175e4fe74e205b3a17d1390461e7ce092acf",
        "f520fb7a1ec139ad1eb2e52b5aa56c8b1acecb2803a7140a10e0de2a70bbdf74202ecde7f1248a0a48fdcf16",
        "5b781048c08fd625af498fc9368c20ff84ca82a09f19281b14babf208b10225d1dcc84441e9b1733ccc43a4d",
        "37fc2a2847459d3b6804d2b4de671b7d2e986995f4fbce5a324ab70e2af79f3b4d9c7fbbc724714842fcdd82",
        "435a0f84fca8134a6e474961d9373340205080b8643e27547b7d23e03052923040304c19955492e62d241f89",
        "2f58154c16ac635db9384c17e9ea85b4cdbcd3b5358edc52a3af4f61e27ea76b530060c67dafd3e71ddbaa58",
        "4540c7f5fd559ec03e4da9955daf95623cc3a2c472537e7002383e74728c19c790b3defca450c0d9d146b469",
        "fa158021c035254b91779adebafdb8a3af51e41a3a49f03ebf5a60796f6d3010f159dfc83cfa87c4a4282e23",
        "197a6519f54f252e1480d67542e7829f9a98a18b36d42b967da5a3da04abacefd5a81c23ba10493054c3c951",
        "b558b2b45579a46766d36b039d4f34f3290215b62f315f28b30ba02856031032d00778501131821b3ca7954e",
        "bc0afeebd8338682fd8289becf2552d39fd772193b1e9f06c28b74cfa6b18079c1510fac0805e9eb1da4dfec",
        "b282c387cc7f4f69c79aa908b94e355af4931e0bc9fab3595d85370ebb16b4f9a668e7d22536265e6a938baa",
        "387e8d2e0bc392fd3d84262ab0898660c999c0e982943d951245077936091f1feb1854c6db6792c467c92b3b",
        "ea1270113ae42983a7da5991a2382d499cee694c87236cc4d984d9be07183280468cddc82e259bf75222c024",
        "996ed37cc522035aeb375e565f4e4ef9b58850aec658b10e93fa2d679d72831da070de02bf9e16425cd4a0c0",
        "44fb0174c79ace10d40918a39feda6271f2c9855e1d5ff8be04cb24c7b4c39fbd7a039b58458a551bc9e10e3",
        "5f33c8bcec1337c28039f380bc7892054efcc438f32b327c1ec6664e1270ec098f2bc9ea280ee38f849593f2",
        "d9609d8e018c036893ca4f3d09b9a7018ce35f95812ea6db5afd272b6550944150ed66b7c8f33b94c42fca9e",
        "94c5c4f7c9eb6080fdcb699d1f78ff996d89b75a03f62344fadd731560f659bc93f46484d27b83c717ae68a6",
        "20c679b10c9d1febc80b88a95fb4c933d650d56629f9d6fbbddfb341362cfe4190c822e1cc93774772719571",
        "700b428f8d8591d55735af6333d3f579bfdd02df203d36928fb0f3c698d74c0c98793def5dd47da22656d215",
        "f597d4998ce2d93cb5eebdd8f6a44e33968e7e8f8cad42281e40c4af06e5450ff2ee03700a8b879e490a68ba",
        "32bfe60c830d497f72a505803d718bdd0a1e6ed9fba4c1821637a9464fa54f5cc039c12ebfc533225fa1c764",
        "98ae8913ce818697fa57238eaa9f89163462658c1d7a7787fe7edf2bab1c8e34b9c33740f6537ff0a6351356",
        "a7fb9d1528d3fbcef390016bd7d78e9de80a9695bb690f9d662cfcbe19a3f079289071c9c72ca13de5fd21ab",
        "e6272779c1b2426a129c6db26ddd9938fd09993350f94831ad52f913aa7ed83b4187995f966d0fdac6a0f3d1",
        "7c9d75437e52718e9159e1c7ea01ec0902839d8a1d18b08254c3bdd5164e5bb8b3d2e2d92b99b315d8f53cd6",
        "a9526b483c56ea6bcf19a9c502e222c59dd322dfda4f35bdebf94ed2862a3cdd6572ec1f8176fedbdd0c838b",
        "18e3f7afa2dc842755c86b33309830b839fc44c509558b2554fc9b2ff3078adce42d057035c3713654ad0575",
        "0acc843c97ccec1bbaa0d282727e073471c2bcad9e645eb32b6715b2a891640c6aca7d68c6783db1fae543e5",
        "1ccbf581fced0bf23433cfbffefab66c09aa5435ae45822a8264b68bdc9ea481fc217ea2b87c3761a3800499",
        "7586e4155ee70b060f69f9d5087852646ae25d3e0a3310ff40748156ef2d5c432135614ce82fc0fae1415d24",
        "3ae0c1a2a1166ea1832efae82fe31f5937a0e92f01afa6f58fab0bc6b952efd8b78f82e1e2d089543b400574",
        "366502855e72f07cfaf08b03dfc5c810a08d4b2a8f49588150d56d7641619f4bb0af8ca823beac02b25bb5f0",
        "857f0c0c3ebe2d025b0d0d348a4b12b459ac70934edc002c19ffd49ca17970664e4954186d86a9ed9628cbf0",
        "43c93cd3b8e917e728144ac245a8cd51294d0aecd5aca5aa83a156a7315170d327e396b0e82d57f18e374e2a",
        "5f590da5be33802b0d1afd7ae1ee9bd10dcfa9e404aeb9f713c4a1fb2cdec8389eb60922f141b3563b4c4ee7",
        "64741fda807abc6087be2c74de342746224a7c0993b7762be4ef0c1010b0c2f2ab8f60001ef110821f64a708",
        "500b3545abdb4d9477ca343160cd414d1b8487863facc7130573450166904816f2029de1e626a6a39afadfbd",
        "b7c6476a3de42fd6d52cc7f2190351a69f0a993bad1ea65fae62adec93ccc10f00d060f945a3728012658327",
        "afa306543cf2810c52a45d6b58fb54ab29bf8d8d6948113a53302621ad20cde82243457162e737d110262d83",
        "5a41c884d7183a78860d21f7c9fda60e37e4acbab60834dc5a8aa1ebeecfd855518ca8936b6c4406e8259587",
        "4d127ce1272fe858e4dd351795d26f52e4a8a4855ffec6f08fbdc510620fcc0f5c2deac9701bd2332bab6c0b",
        "d274c2d4ef5aa29f1fa4bc1ac28f96f97b36a30a484acb8512ffd1bc24449ebf5d0e22d5ff73879a6dbcdc66",
        "54fa024adf554771726404dcbd60ec2c975caca8963847b982455ccb69adb19dd852fcdc1c8138b2c45bac55",
        "c8d1a660f3c0fd944821f184799efb9e6bdb57989a2c2131a265aa887f8c1958b5beca6e5a95386ef71000d4",
        "340dee9e7ab6f81d6ffc3be473e5a200e4be5c4861234e7c52396fcf50f2f3a7270f03dc4c3626c6c7698cec",
        "dc0151ffc644f56a14ce2503809ef6b3a50f094c766b97d3c4ed06c6197ede07a6478de59ad5ebd50231679a",
        "23017e850765d3a30e5e72050305bf5047a99e6e117a78a27da32ee1eb7f843eb5523e83f1fec9368e54a63d",
        "166637f837eeaee29bda397cf7415b3af1eb36493f1c2d4171016e7a99ec7f84b903f44b66b0bc1e95768d75",
        "3e185fc6eb665bfc1c43091a4c18c5bcf3606e354610ee20d41d2882f445f7d29857b20c63d78c3df3d47fa7",
        "8c81c27e34f77028af9e0390115f1743a8a88ab1cc297f06e6423081f4590d01c0a40f5d3487b26bc41717bb",
        "542fab1057162286ce272236ad0763774f652507883a2706b58b9e9522aad053a3ccedb4a5d42832d77624d2",
        "0f18479228c1caf632d3f4d54c9658631b7d3318da4752022ed0de3f4fdcd47715135bfcfcd0ed5c54b71501",
        "11e54a16c2624a802892f10be856a11720c729afb15eafa7086163ae4da3d13246d8e7b6cfaedeb614fb5a26",
        "e533e2446f6818028410f527dd13a84b9e69cba670bf6f88e7c4ea843b0799b7537836681833033b6caad060",
        "37b2d938d390e5fa411d7669a1ccd25b3553bcc12e48688e619bd05f879016d4168390070a13e00f5b995322",
        "b0b00888a8cc34a8f227b8e40909ec411d2399292b9ae88c9d1ce8f48fc14da9a5752c581e5e6712aba44bf5",
        "66859fe3c9ec89e4220a2c5085948efb14a34b25a70753f9924013cb04c89a8f6df2083f7d2039553ed5df1e",
        "769120be6016c84d6ab05100520bcc23cf02f14507342ce5e6ae0c95b944f04fa51880a3bf51ed79ead0ab79",
        "99397a94e55fb8112a006455688045737b4e6a7ab7cd0b338f61180f790c79bcbd38da9cf5e56917eed1bf48",
        "42822ec876628d888c372a700421c311a5ae0da8c59ff860a4e9a84cbe8079c1c3a4dab56d117e4e2590013d",
        "285795266d4901c5100f4c88bfd65eb3fa094a784bf25bfd02f9d206003014676e7f86",
    ];
    const REFERENCE_1: &[&str] = &[
        "28b52ffd64e02d657600fa451c0e38606ea5030aad2880eaaa82404280110eaa4fd4b2a9574129f4d90fdf46",
        "00c435f2c720ec08ece44cfacd9848c8d1c8b919f8d884f7a9780ac300d200dc009e1cf847492befeebef18f",
        "6cfc6835d1c1373bf8a647d9a37a77b761f847b9bbebefef1fa57f07dfece09b6c5efefe1d7c938cdb3fcadd",
        "5d27b4c2f9efee7af18fb2836ffaefeed2eab51d7cb37777ab3af86607dfa4b5c5f877f0cd5ed3a7696d7f5a",
        "dbbf836fe604598992fda33c446b9038175d34d0747475f0cd19d146da8f1cadfecc4ceb9f0ebeb9bbcb6677",
        "b7075a8ddc0fb0dddddd5d7eb0bb0be2790049dbd1feb91f691fc3c03a7c71f0daa0210306af2f195f60425c",
        "071a101afd6477378da9d401a6e23826d5fefc1d7c130d3b25ff0b4368e53752fce114b11a7f35335edbdd25",
        "c3fdf6c2cd3fca2a5c3af8e612b3ce11296b314e402b24308ce10fad492af847f9a6e7d0aaa0ca8595bf024b",
        "5a34f847c9e5692b4e07dfbc430a2eca0633fca3cc436b8b521400ab836f7294fec43fcabee11fa58a2845d1",
        "fac6dd628bc99a69f094872fff283bf826ad8e5ef847d96677778fff105a83d0eabfa97130e3a1958affeeae",
        "ff94c15d49eb04ad4ceb195c07df6c5a9b1375828f7277979bcda89920873bf826c3692e5acf94f94719bde3",
        "cc50d3aa55fdedf897a1b5f36415f042bb8e89a535c7fe7d45eb151f5ad56865aa0ebe69a787bf1a8a39fdc6",
        "6bfe92da0d9b7f94773af86620ff281975f04d3d06b8ad99420f88b22d10a12d292c2a74912ebbbb23a06c69",
        "044c7677aa3beea4c7ffca937f947068b5c3243806890f9abadefc25f0164b1d7c53ed066b7d69b1311c828a",
        "d5eeae1bad2520b8f973e222fe51e27120fd48124286d6ee512bc070f94f0854e57f69039a8045832220dadd",
        "45737fffb8335aa974b3686e5a43356f31a295df7c30adb067e3f1876b1674b6eceec68de8e09bfe9c26691f",
        "777739cc3f4a3afe80cedafc3912a24f7d8ef84799a5fb06ad565450d18ae536667685d649b40268005a5908",
        "74f0cdabe6ac361be5a39b64dadd2560a2917f946372b085078f02d00a6975c302ebe09b51c8da0a879235ff",
        "28a7b08bdd5d0bfe5142f287cda0adc1a4e65e10264a241d7cd34b07dfe4e01f251411fe511e0243a7836fa6",
        "0dcacaf133c4fca31c1bf38fb4bb1bc27fad836f7619ad3cd97f0ead4cca3f4a5a9bc36805c2668a0796f66f",
        "54088bd6ca646f2276774119f1ff0a5b50a9836f0ee9f667ff2c5d46d529d85a94255afd038662a8735baa54",
        "28499242db01431080c004c68956caaccc79138080195172a84b254aa5033e845735d0c01a8f60a8443a05df",
        "8d92ee155984903735ed23192de90dcc112303af4c05f43badb6c644fc00a8712e38d4fdfc3ec1019ca323e2",
        "1a5341899c3d95cb219f66d0b0bda286041262034e62216f7e05f4677da0a7939d2678689135fff24494503a",
        "4990f5b52bc78131860adc478c348b7bb9b6fd321a93dd61c4c68e4218521a0c3e03fd85f4e38bcb634f7a8f",
        "7e93496a1e2b4dc4917cc5f4f772d0a17a486591144278bae63f6d8718afc3296da1ea8c3e22db7e94dff1f6",
        "ad09a3fe4612afe49ea488d5232dd7a928b285a96859e247c2cc4e3a950c65a2e898bc9334bb6a61b2e60566",
        "6c19691800811b9b6ad8d321981260b48a68b6dc48e081984c636cf1edc8e2fe688f346fa06dc54969001dd2",
        "c0e48aa9cf781c8fd5538a54bc09429069457d2d63744bcad65f83440409332059c2383753fc24f4dc07bfd3",
        "055ed93a9d22c1963e55e142f74824e5d5dbd336200180eedac17589328b2f20094e9f90514430b486420fcb",
        "c1bb8f41ef0ef616daf66109edcde7e8e211933b55a3723a99d2a34a1093225603540d729f73582620d6bd0c",
        "e0396c460b09accba1a69e9582395312f1ee004c4b6a63ce9ecdc1c2ed63ec9b8447f81829e35ab26a2fa004",
        "5992901e41bc9b01c7e2cf101ed895663d20401d2ccaaba40539685a389261143b21932daf020f54f35b76e4",
        "e5f5f18bda745d31d3d3ca9940855a9172de2c9d1e5604c4c61835155ab8a2428a8b6b7da573d47fe47b2ff3",
        "155bf49482d203676b26aae748d02a51b11ae6c8b14b0ce242a0fac6ff306015beca0cb1610501005f7cbdf0",
        "1ef3510866d9832903c64b1b05629fdd637239811f02bde31fd807a446d82cac0e0f37a5f18e28c9a4522787",
        "6041a341e7b7a7e32d4762da095785bdcb475105d4a211234fdcb15939f53f54d231bbd778da16422a5f05ba",
        "3ba812eb64db52d472e30e693d7d9fd4d17e31a6d6eab13018844273b5ce34f8d725ee34333718a5b7ab336a",
        "37d28d13a38486926cfd5f46071821e6345f123f3538d9c4aa15d4f48207653dedb808d30ee637e002aa067b",
        "7534240924cc0ec9ef7515f0b8388c80936704c765befa08f6e93b74b332b357b32cd3a95168c9321b9fa49d",
        "dc50744891cf0f665dc5b1aaf30c585ed9d469f1a86881f8e9c944dee997746b13f04ac9964b1669663a696d",
        "9615f82f4d4480c01e46f5baa14ab0f6db1a8c54ce19f0a9bcf3f011b2493eda547155f94108e11369e52e08",
        "24e4be490708fafa2b1e68632e86748aeaf81da5d755621f811a9165c446143444626ca514ea7ea27a60ba99",
        "0c930db59909a06b995037aa602e433887b40486f290f14a8977ac90e781e0f06a1a0d9e730b85c4270691ca",
        "20c38f388b8431d570d22094aa1694c1f049c2c52ad3a1c23a4e134e5ee32f18ade1f352ee9e5dfa4c86cb57",
        "925d789402e8643c40ed58c266e1cc14060a02f74b8cb4a2d25167d69fe0689f5a48b3da61da019a13e65ed2",
        "cfc260246ec876425f3f409d4806a0f8cb823e59c48cda3933260fdf356998a3f487c08fccfa12bceec82b08",
        "cc3ea9a09fe377a5c63fe428b79f10f06e2a75f667246334a95d1eab2e549654023fea3fa4612c0f35111496",
        "2491d6c93f8e4a096d84eef72f98fa6770c8db804dee7e03b4fa444b27668b498023df7c5b67209495212d9d",
        "9c8e7117831dc36bb32ea8303b7699ce83353717058de054d66463674c06f93b73a33ed85ce1c40306ccbfc3",
        "ecb18a71ed3c57178d9be7370d7798cb80da2664e75bdf2ae9a6c21348102dbea692eaa0487bc95f487aaf1e",
        "b60c7ef866d3a21a9ac0f189100e1f9bff13b8922da730ceab4494c110e7ca26e3803e457e11e993649640e4",
        "2f4b2b49b86b4ec500447cf6227dd77285c1913096e3212ea32b4ec09c0d1a4c6cb2076d9ba1fc17e29113f4",
        "d9710b52b46e9d7c476848bb65e2877d582e89d6f46f20460721fb73b3aff3251651a9979be518c07764a62a",
        "7e933038b9ec6574c64ccd2fd5b78da0748b8913d1b5ab06d5d8f912276cb2c15d7226f8371813e5e2951048",
        "a38f57abaeeefc6c0ee2f232da8c3a3576c52f41b25ed602194056a9d2c83131abfb06c982b27ff9980c6829",
        "92fb13e80044f41ed580b5128cef9f2d8149c19727527d58d886e04e3ec8881f2df72e217b2b59a0fbc34551",
        "f3b4ed25ca3429ec23f08312cca34d214686688bff3fa3a07ccaeff2372785f492a121f5497677f9f7438fe8",
        "3dd1b8059483b31eeb3503d295c0df4b243b009253ca4fc26d14cb750de0af1f9a8aca025dad82b7138fb43d",
        "1266d10ccc22ac90e336980a5e534bf931467632ef532d56db6aa35b1b4824be0126e774e2cfd98c5da9a64e",
        "011513f7019fefe18c320dc0ef28e708254d56afd3cccc26da7d79b1516f3a1812e966927aab5624e0929742",
        "92d051ce7533806721c907eeacbd6bfb2533e620d27fa2ed8e14c150068cdb1e925fc17c2c04bd56c1f49985",
        "00e83f9d2e89779cd96d2e61243fd9353bb976bf78a9bbd8e125fb6762669983810c8896c5e53e604000298e",
        "4f1f43e558140c0414e5a9d30d95d0303a327e3cab3ab796fceac8b72b33709eb7c2819c8593412177e1056e",
        "0c3a2769407b7e9fef5f7bf79a068286687728ada1a1c50d7289faf10c63c787845b9266fea8f1c355f39008",
        "744aee99427b096c542fb0f1a79c129848991c37d21ef6e52cb89670694da862754f067e5ab55cec1d78222d",
        "f66f2e50a6d4164ea5f73277ad63b1d4c774c02ecaffc852b642fc1e16531163bc0b4dfe970b5a93ba9a3627",
        "8ef3d51e19b853d0b97826da40512512e0b59278947042a6a7972d0731a665fb0323372a91a4c6546038753f",
        "ed9069a1f109ab70c039229e212383ac9a9492a98506029f04bdbc1b449055cd94de0f58cba015367dd312b0",
        "683557b1c0cbeed0c323d95b72d57619f634a725986852eb701a480aaf888dca68eb8993f0f8f51024d648e1",
        "2eb9860b7d7304996ea49528ef065bad839871f7e63350161964b35ad08d20a673d811df0b317d961b144643",
        "c1c9916ab7df70a1c0f126ae6abd905064f99352114791879f8d626221666d13cce835d5958107bd3c28c12d",
        "41d9d7a385bc0291078ad1379cde11c04e08d40333ce5ebb273cb48431cded134b29a41af5199ecc7c0803d6",
        "1f16a11b60d7ffdc45affc378d9b6ecb2dd0509d8f752a0f9e6b948f294e116b49833b9c3fe288e9dfd1459d",
        "ccd2bc38d1cc753d0d877cd0c24b6e76feb7505cc336819230c5d20c4b65a4dcf7d43207518af102dd9321fd",
        "5738a5e2ec76fb03e287b952cc6cb25d1c8ee1b0bfaa32075d5ae69d87e56983c2829d009aa2375b12501030",
        "4ffa7dbdceb48cf85e6ab144aa39807b66b9d58299ee87882111d20d89d17acdae5669b78039778308095665",
        "c015d619aec5e414b5352d8674495de7ce47e10420d5194a368f1b3a9d44c23f344e1c9602e9482e8581aeda",
        "8758b4653a6c5613fb38cd4626e498130a3851625b166290ce2a528485cd9c6499880958f51637bb46edf5cd",
        "a2c183c2e1b9879516f82ab27f54cfac8164a9005991eaf95d236d00b9d42140127224d294e4a2eee2398e01",
        "848436dc920a724da214cc71cfa0e923437af64ba641aed3e1fa0ca29964e8ac7b598ea8240d41a287f92dd1",
        "dd04c10f740010f0208d5a5cccf458c300300fe9c3f28f3fd7f2400cebad81a4717e8dd806ae530a7b8a9159",
        "66b23a172f62e9e1de7b4c0d28920e4d2809a5111576508465ecc8814296839e828e3217b2a6061cfad442e7",
        "ce8ceca293be5241301cdd8a3257382fb21f21be582d14d1f03f1e92a90443015251bd32cb31e24c1df2ff89",
        "c368a2043005124eb9b18d9d9fbe96bfc3ec332f35553e00c0251731912882f427c89d74302e0e146180cbd2",
        "08092812cbea86491eeeb13c3cd39be7b5002e1491f288290c350677ba4c653fca6789c2504aa86e39ef405a",
        "64392fd6b8afb95c7b012112e80a676e7f86",
    ];

    fn unhex(lines: &[&str]) -> Vec<u8> {
        let hex: String = lines.concat();
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn decodes_reference_frames() {
        let want = lcg_text(12_000, 42);
        for frame in [unhex(REFERENCE_19), unhex(REFERENCE_1)] {
            assert_eq!(decompress(&frame, want.len()).unwrap(), want);
        }
    }

    #[test]
    fn deterministic() {
        let data = lcg_text(100_000, 5);
        assert_eq!(compress(&data).unwrap(), compress(&data).unwrap());
    }
}

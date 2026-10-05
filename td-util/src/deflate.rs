//! A DEFLATE (RFC 1951) encoder: LZ77 over hash chains, then each block in
//! the fixed Huffman code or stored, whichever is smaller. No dynamic tables,
//! so it trails `gzip -9`: about a sixth larger on mixed source and binaries,
//! more on skewed data. The output is any inflater's to read and depends only
//! on the input and the level.

const WINDOW: usize = 32 * 1024;
const MIN_MATCH: usize = 3;
const MAX_MATCH: usize = 258;
const HASH_BITS: u32 = 15;
/// Input bytes per block: one stored block's limit.
const BLOCK: usize = 65_535;

const LENGTH_BASE: &[u16] = &[
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LENGTH_EXTRA: &[u8] = &[
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: &[u16] = &[
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: &[u8] = &[
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

#[derive(Clone, Copy)]
enum Token {
    Literal(u8),
    Match { len: u16, dist: u16 },
}

struct Bits {
    out: Vec<u8>,
    acc: u64,
    n: u32,
}

impl Bits {
    fn put(&mut self, value: u32, count: u32) {
        self.acc |= u64::from(value) << self.n;
        self.n += count;
        while self.n >= 8 {
            self.out.push((self.acc & 0xff) as u8);
            self.acc >>= 8;
            self.n -= 8;
        }
    }

    /// Huffman codes are sent most-significant bit first.
    fn put_code(&mut self, code: u32, len: u32) {
        let mut rev = 0u32;
        for i in 0..len {
            rev |= ((code >> i) & 1) << (len - 1 - i);
        }
        self.put(rev, len);
    }

    fn align(&mut self) {
        if self.n > 0 {
            self.put(0, 8 - self.n);
        }
    }
}

/// The fixed literal/length code (RFC 1951 3.2.6): (code, bit length).
fn fixed_litlen(sym: u16) -> (u32, u32) {
    let s = u32::from(sym);
    match sym {
        0..=143 => (0x30 + s, 8),
        144..=255 => (0x190 + s - 144, 9),
        256..=279 => (s - 256, 7),
        _ => (0xc0 + s - 280, 8),
    }
}

fn length_symbol(len: u16) -> (u16, u16, u8) {
    let mut i = LENGTH_BASE.len() - 1;
    while LENGTH_BASE.get(i).is_some_and(|&b| b > len) {
        i -= 1;
    }
    let base = LENGTH_BASE.get(i).copied().unwrap_or(3);
    let extra = LENGTH_EXTRA.get(i).copied().unwrap_or(0);
    (257 + i as u16, len - base, extra)
}

fn dist_symbol(dist: u16) -> (u32, u16, u8) {
    let mut i = DIST_BASE.len() - 1;
    while DIST_BASE.get(i).is_some_and(|&b| b > dist) {
        i -= 1;
    }
    let base = DIST_BASE.get(i).copied().unwrap_or(1);
    let extra = DIST_EXTRA.get(i).copied().unwrap_or(0);
    (i as u32, dist - base, extra)
}

/// LZ77 over `data`, a block at a time: the hash chains carry across blocks,
/// so a match still reaches back a full window, but only one block's tokens
/// are ever held. `chain` bounds how many earlier positions each search may
/// visit, which is what a level trades.
struct Lz<'a> {
    data: &'a [u8],
    chain: usize,
    head: Vec<usize>,
    prev: Vec<usize>,
    /// The next input byte to tokenize.
    at: usize,
}

impl<'a> Lz<'a> {
    fn new(data: &'a [u8], chain: usize) -> Self {
        Lz {
            data,
            chain,
            head: vec![usize::MAX; 1 << HASH_BITS],
            prev: vec![usize::MAX; WINDOW],
            at: 0,
        }
    }

    fn hash(&self, i: usize) -> usize {
        let b = |k: usize| u32::from(self.data.get(i + k).copied().unwrap_or(0));
        let h = (b(0) << 10) ^ (b(1) << 5) ^ b(2);
        (h.wrapping_mul(2_654_435_761) >> (32 - HASH_BITS)) as usize
    }

    fn insert(&mut self, i: usize) {
        if i + MIN_MATCH <= self.data.len() {
            let h = self.hash(i);
            if let (Some(p), Some(hd)) = (self.prev.get_mut(i % WINDOW), self.head.get(h)) {
                *p = *hd;
            }
            if let Some(hd) = self.head.get_mut(h) {
                *hd = i;
            }
        }
    }

    /// Tokens for the next BLOCK input bytes at most, into `out`. A match
    /// stops at the block's end, so each block covers exactly its bytes and
    /// can fall back to one stored block of them.
    fn block(&mut self, out: &mut Vec<Token>) {
        let end = (self.at + BLOCK).min(self.data.len());
        while self.at < end {
            let i = self.at;
            let mut best_len = 0;
            let mut best_dist = 0;
            if i + MIN_MATCH <= end {
                let mut cand = self.head.get(self.hash(i)).copied().unwrap_or(usize::MAX);
                let mut left = self.chain;
                let max = MAX_MATCH.min(end - i);
                while cand != usize::MAX && left > 0 && cand < i && i - cand <= WINDOW {
                    let mut l = 0;
                    while l < max && self.data.get(cand + l) == self.data.get(i + l) {
                        l += 1;
                    }
                    if l > best_len {
                        best_len = l;
                        best_dist = i - cand;
                        if l == max {
                            break;
                        }
                    }
                    let next = self.prev.get(cand % WINDOW).copied().unwrap_or(usize::MAX);
                    // A slot overwritten by a newer position ends the chain.
                    if next != usize::MAX && next >= cand {
                        break;
                    }
                    cand = next;
                    left -= 1;
                }
            }
            if best_len >= MIN_MATCH {
                out.push(Token::Match {
                    len: best_len as u16,
                    dist: best_dist as u16,
                });
                for k in 0..best_len {
                    self.insert(i + k);
                }
                self.at += best_len;
            } else {
                if let Some(&b) = self.data.get(i) {
                    out.push(Token::Literal(b));
                }
                self.insert(i);
                self.at += 1;
            }
        }
    }
}

fn fixed_bits(tokens: &[Token]) -> usize {
    let mut bits = 3 + 7; // header + end of block
    for t in tokens {
        bits += match *t {
            Token::Literal(b) => fixed_litlen(u16::from(b)).1 as usize,
            Token::Match { len, dist } => {
                let (sym, _, le) = length_symbol(len);
                let (_, _, de) = dist_symbol(dist);
                fixed_litlen(sym).1 as usize + usize::from(le) + 5 + usize::from(de)
            }
        };
    }
    bits
}

/// Raw DEFLATE of `data` at `level` 1..=9.
pub fn compress(data: &[u8], level: u8) -> Vec<u8> {
    let chain = match level {
        0 | 1 => 4,
        2..=3 => 16,
        4..=6 => 128,
        _ => 1024,
    };
    let mut w = Bits {
        out: Vec::with_capacity(data.len() / 2 + 16),
        acc: 0,
        n: 0,
    };
    let mut lz = Lz::new(data, chain);
    let mut tokens = Vec::with_capacity(BLOCK);
    loop {
        let from = lz.at;
        tokens.clear();
        lz.block(&mut tokens);
        let last = lz.at >= data.len();
        let raw = data.get(from..lz.at).unwrap_or(&[]);
        emit_block(&mut w, &tokens, raw, last);
        if last {
            break;
        }
    }
    w.align();
    w.out
}

fn emit_block(w: &mut Bits, tokens: &[Token], raw: &[u8], last: bool) {
    let stored_bits = 3 + 7 + 32 + raw.len() * 8;
    if fixed_bits(tokens) > stored_bits {
        w.put(u32::from(last), 1);
        w.put(0, 2);
        w.align();
        let len = raw.len() as u16;
        w.put(u32::from(len), 16);
        w.put(u32::from(!len), 16);
        w.out.extend_from_slice(raw);
        return;
    }
    w.put(u32::from(last), 1);
    w.put(1, 2);
    for t in tokens {
        match *t {
            Token::Literal(b) => {
                let (c, l) = fixed_litlen(u16::from(b));
                w.put_code(c, l);
            }
            Token::Match { len, dist } => {
                let (sym, lx, le) = length_symbol(len);
                let (c, l) = fixed_litlen(sym);
                w.put_code(c, l);
                w.put(u32::from(lx), u32::from(le));
                let (ds, dx, de) = dist_symbol(dist);
                w.put_code(ds, 5);
                w.put(u32::from(dx), u32::from(de));
            }
        }
    }
    let (c, l) = fixed_litlen(256);
    w.put_code(c, l);
}

/// A gzip member: no name, mtime 0, OS unix — the same bytes for the same
/// input on any machine (`gzip -n`).
pub fn gzip(data: &[u8], level: u8) -> Vec<u8> {
    let xfl = match level {
        9 => 2,
        1 => 4,
        _ => 0,
    };
    let mut out = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, xfl, 3];
    out.extend_from_slice(&compress(data, level));
    out.extend_from_slice(&crate::crc32::crc32(data).to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    fn roundtrip(data: &[u8], level: u8) -> usize {
        let gz = gzip(data, level);
        assert_eq!(
            crate::gzip::decompress_bytes(&gz).unwrap(),
            data,
            "level {level}"
        );
        gz.len()
    }

    #[test]
    fn roundtrips_through_the_engine_inflater() {
        let mut text = Vec::new();
        for i in 0..20_000u32 {
            text.extend_from_slice(format!("line {} of the build log\n", i % 977).as_bytes());
        }
        let mut noise = Vec::new();
        let mut x = 0x1234_5678u32;
        for _ in 0..200_000 {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            noise.push((x & 0xff) as u8);
        }
        for level in [1, 6, 9] {
            assert!(roundtrip(&text, level) < text.len() / 4, "text compresses");
            // Incompressible input falls back to stored blocks: small overhead.
            assert!(roundtrip(&noise, level) < noise.len() + noise.len() / 100 + 64);
            roundtrip(b"", level);
            roundtrip(b"a", level);
            roundtrip(&[0u8; 300_000], level);
            roundtrip(
                b"abcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabc",
                level,
            );
        }
    }

    #[test]
    fn output_is_deterministic_and_header_is_reproducible() {
        let data = b"the same input gives the same bytes";
        assert_eq!(gzip(data, 6), gzip(data, 6));
        let gz = gzip(data, 9);
        assert_eq!(&gz[..10], &[0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 2, 3]);
    }

    #[test]
    fn symbols_cover_their_ranges() {
        assert_eq!(length_symbol(3), (257, 0, 0));
        assert_eq!(length_symbol(258), (285, 0, 0));
        assert_eq!(length_symbol(227), (284, 0, 5));
        assert_eq!(length_symbol(257), (284, 30, 5));
        assert_eq!(dist_symbol(1), (0, 0, 0));
        assert_eq!(dist_symbol(32768), (29, 8191, 13));
    }
}

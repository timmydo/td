//! SHA-256, SHA-384, SHA-512 (FIPS 180-4) and SHA-1. SHA-1 exists only because
//! the OpenPGP v4 fingerprint and SSH's hashed `known_hosts` are defined by it: it
//! is not a `Digest`, so the one HMAC on it is `hmac::hmac_sha1` and no signature
//! can be built on it.
//!
//! All of them run in time that depends only on the input's length.

/// A hash, clonable because TLS forks its running transcript.
pub trait Digest: Clone {
    const BLOCK_LEN: usize;
    const OUTPUT_LEN: usize;
    type Output: AsRef<[u8]> + AsMut<[u8]> + Copy;

    fn new() -> Self;
    fn update(&mut self, data: &[u8]);
    fn finalize(self) -> Self::Output;

    fn digest(data: &[u8]) -> Self::Output {
        let mut hash = Self::new();
        hash.update(data);
        hash.finalize()
    }
}

#[derive(Clone)]
struct Blocks<const B: usize> {
    pending: [u8; B],
    used: usize,
    total: u128,
}

impl<const B: usize> Blocks<B> {
    const fn new() -> Self {
        Self {
            pending: [0; B],
            used: 0,
            total: 0,
        }
    }

    fn absorb(&mut self, mut data: &[u8], compress: &mut impl FnMut(&[u8; B])) {
        self.total = self.total.wrapping_add(data.len() as u128);
        if self.used > 0 {
            let take = (B - self.used).min(data.len());
            let (head, rest) = data.split_at(take);
            let (_, free) = self.pending.split_at_mut(self.used);
            free.split_at_mut(take).0.copy_from_slice(head);
            self.used += take;
            data = rest;
            if self.used < B {
                return;
            }
            compress(&self.pending);
            self.used = 0;
        }
        let (blocks, rest) = data.as_chunks::<B>();
        for block in blocks {
            compress(block);
        }
        self.pending
            .split_at_mut(rest.len())
            .0
            .copy_from_slice(rest);
        self.used = rest.len();
    }

    /// 0x80, zeros, then the bit length in its last `length_len` bytes.
    fn pad(mut self, length_len: usize, compress: &mut impl FnMut(&[u8; B])) {
        let bits = self.total.wrapping_mul(8).to_be_bytes();
        let zeros = (2 * B - length_len - 1 - self.used) % B;
        self.absorb(&[0x80], compress);
        for _ in 0..zeros {
            self.absorb(&[0], compress);
        }
        self.absorb(bits.split_at(bits.len() - length_len).1, compress);
    }
}

#[derive(Clone)]
pub struct Sha256 {
    h: [u32; 8],
    blocks: Blocks<64>,
}

impl Digest for Sha256 {
    const BLOCK_LEN: usize = 64;
    const OUTPUT_LEN: usize = 32;
    type Output = [u8; 32];

    fn new() -> Self {
        Self {
            h: IV256,
            blocks: Blocks::new(),
        }
    }

    fn update(&mut self, data: &[u8]) {
        let Self { h, blocks } = self;
        blocks.absorb(data, &mut |block| compress256(h, block));
    }

    fn finalize(self) -> [u8; 32] {
        let Self { mut h, blocks } = self;
        blocks.pad(8, &mut |block| compress256(&mut h, block));
        let mut out = [0; 32];
        for (bytes, word) in out.as_chunks_mut::<4>().0.iter_mut().zip(h) {
            *bytes = word.to_be_bytes();
        }
        out
    }
}

fn compress256(h: &mut [u32; 8], block: &[u8; 64]) {
    #[cfg(target_arch = "x86_64")]
    if let Some(cpu) = crate::accel::ShaNi::detect() {
        cpu.compress256(h, block);
        return;
    }
    compress256_software(h, block);
}

fn compress256_software(h: &mut [u32; 8], block: &[u8; 64]) {
    let mut w = [0u32; 16];
    for (word, bytes) in w.iter_mut().zip(block.as_chunks::<4>().0) {
        *word = u32::from_be_bytes(*bytes);
    }
    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut last] = *h;
    for (round, k) in K256.into_iter().enumerate() {
        let at = |later: usize| w.get((round + later) % 16).copied().unwrap_or_default();
        let (w0, w1, w9, w14) = (at(0), at(1), at(9), at(14));
        let t1 = last
            .wrapping_add(e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25))
            .wrapping_add((e & f) ^ (!e & g))
            .wrapping_add(k)
            .wrapping_add(w0);
        let t2 = (a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22))
            .wrapping_add((a & b) ^ (a & c) ^ (b & c));
        last = g;
        g = f;
        f = e;
        e = d.wrapping_add(t1);
        d = c;
        c = b;
        b = a;
        a = t1.wrapping_add(t2);
        let next = w0
            .wrapping_add(w1.rotate_right(7) ^ w1.rotate_right(18) ^ (w1 >> 3))
            .wrapping_add(w9)
            .wrapping_add(w14.rotate_right(17) ^ w14.rotate_right(19) ^ (w14 >> 10));
        if let Some(word) = w.get_mut(round % 16) {
            *word = next;
        }
    }
    for (word, add) in h.iter_mut().zip([a, b, c, d, e, f, g, last]) {
        *word = word.wrapping_add(add);
    }
}

#[derive(Clone)]
struct Core512 {
    h: [u64; 8],
    blocks: Blocks<128>,
}

impl Core512 {
    fn update(&mut self, data: &[u8]) {
        let Self { h, blocks } = self;
        blocks.absorb(data, &mut |block| compress512(h, block));
    }

    fn finalize(self) -> [u8; 64] {
        let Self { mut h, blocks } = self;
        blocks.pad(16, &mut |block| compress512(&mut h, block));
        let mut out = [0; 64];
        for (bytes, word) in out.as_chunks_mut::<8>().0.iter_mut().zip(h) {
            *bytes = word.to_be_bytes();
        }
        out
    }
}

fn compress512(h: &mut [u64; 8], block: &[u8; 128]) {
    let mut w = [0u64; 16];
    for (word, bytes) in w.iter_mut().zip(block.as_chunks::<8>().0) {
        *word = u64::from_be_bytes(*bytes);
    }
    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut last] = *h;
    for (round, k) in K512.into_iter().enumerate() {
        let at = |later: usize| w.get((round + later) % 16).copied().unwrap_or_default();
        let (w0, w1, w9, w14) = (at(0), at(1), at(9), at(14));
        let t1 = last
            .wrapping_add(e.rotate_right(14) ^ e.rotate_right(18) ^ e.rotate_right(41))
            .wrapping_add((e & f) ^ (!e & g))
            .wrapping_add(k)
            .wrapping_add(w0);
        let t2 = (a.rotate_right(28) ^ a.rotate_right(34) ^ a.rotate_right(39))
            .wrapping_add((a & b) ^ (a & c) ^ (b & c));
        last = g;
        g = f;
        f = e;
        e = d.wrapping_add(t1);
        d = c;
        c = b;
        b = a;
        a = t1.wrapping_add(t2);
        let next = w0
            .wrapping_add(w1.rotate_right(1) ^ w1.rotate_right(8) ^ (w1 >> 7))
            .wrapping_add(w9)
            .wrapping_add(w14.rotate_right(19) ^ w14.rotate_right(61) ^ (w14 >> 6));
        if let Some(word) = w.get_mut(round % 16) {
            *word = next;
        }
    }
    for (word, add) in h.iter_mut().zip([a, b, c, d, e, f, g, last]) {
        *word = word.wrapping_add(add);
    }
}

#[derive(Clone)]
pub struct Sha512(Core512);

impl Digest for Sha512 {
    const BLOCK_LEN: usize = 128;
    const OUTPUT_LEN: usize = 64;
    type Output = [u8; 64];

    fn new() -> Self {
        Self(Core512 {
            h: IV512,
            blocks: Blocks::new(),
        })
    }

    fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }

    fn finalize(self) -> [u8; 64] {
        self.0.finalize()
    }
}

#[derive(Clone)]
pub struct Sha384(Core512);

impl Digest for Sha384 {
    const BLOCK_LEN: usize = 128;
    const OUTPUT_LEN: usize = 48;
    type Output = [u8; 48];

    fn new() -> Self {
        Self(Core512 {
            h: IV384,
            blocks: Blocks::new(),
        })
    }

    fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }

    fn finalize(self) -> [u8; 48] {
        let mut out = [0; 48];
        for (byte, full) in out.iter_mut().zip(self.0.finalize()) {
            *byte = full;
        }
        out
    }
}

/// Legacy: for fingerprints the OpenPGP format fixes, never for a signature.
pub fn sha1(data: &[u8]) -> [u8; 20] {
    sha1_of_parts(&[data])
}

pub(crate) fn sha1_of_parts(parts: &[&[u8]]) -> [u8; 20] {
    let mut h = [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476, 0xc3d2e1f0];
    let mut blocks = Blocks::<64>::new();
    for part in parts {
        blocks.absorb(part, &mut |block| compress1(&mut h, block));
    }
    blocks.pad(8, &mut |block| compress1(&mut h, block));
    let mut out = [0; 20];
    for (bytes, word) in out.as_chunks_mut::<4>().0.iter_mut().zip(h) {
        *bytes = word.to_be_bytes();
    }
    out
}

fn compress1(h: &mut [u32; 5], block: &[u8; 64]) {
    let mut w = [0u32; 16];
    for (word, bytes) in w.iter_mut().zip(block.as_chunks::<4>().0) {
        *word = u32::from_be_bytes(*bytes);
    }
    let [mut a, mut b, mut c, mut d, mut e] = *h;
    for round in 0..80u32 {
        let (f, k) = match round / 20 {
            0 => ((b & c) | (!b & d), 0x5a827999),
            1 => (b ^ c ^ d, 0x6ed9eba1),
            2 => ((b & c) | (b & d) | (c & d), 0x8f1bbcdc),
            _ => (b ^ c ^ d, 0xca62c1d6u32),
        };
        let [w0, w1, w2, w3, w4, w5, w6, w7, w8, w9, w10, w11, w12, w13, w14, w15] = w;
        let t = a
            .rotate_left(5)
            .wrapping_add(f)
            .wrapping_add(e)
            .wrapping_add(k)
            .wrapping_add(w0);
        e = d;
        d = c;
        c = b.rotate_left(30);
        b = a;
        a = t;
        let next = (w13 ^ w8 ^ w2 ^ w0).rotate_left(1);
        w = [
            w1, w2, w3, w4, w5, w6, w7, w8, w9, w10, w11, w12, w13, w14, w15, next,
        ];
    }
    for (word, add) in h.iter_mut().zip([a, b, c, d, e]) {
        *word = word.wrapping_add(add);
    }
}

pub(crate) const K256: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];
const K512: [u64; 80] = [
    0x428a2f98d728ae22,
    0x7137449123ef65cd,
    0xb5c0fbcfec4d3b2f,
    0xe9b5dba58189dbbc,
    0x3956c25bf348b538,
    0x59f111f1b605d019,
    0x923f82a4af194f9b,
    0xab1c5ed5da6d8118,
    0xd807aa98a3030242,
    0x12835b0145706fbe,
    0x243185be4ee4b28c,
    0x550c7dc3d5ffb4e2,
    0x72be5d74f27b896f,
    0x80deb1fe3b1696b1,
    0x9bdc06a725c71235,
    0xc19bf174cf692694,
    0xe49b69c19ef14ad2,
    0xefbe4786384f25e3,
    0x0fc19dc68b8cd5b5,
    0x240ca1cc77ac9c65,
    0x2de92c6f592b0275,
    0x4a7484aa6ea6e483,
    0x5cb0a9dcbd41fbd4,
    0x76f988da831153b5,
    0x983e5152ee66dfab,
    0xa831c66d2db43210,
    0xb00327c898fb213f,
    0xbf597fc7beef0ee4,
    0xc6e00bf33da88fc2,
    0xd5a79147930aa725,
    0x06ca6351e003826f,
    0x142929670a0e6e70,
    0x27b70a8546d22ffc,
    0x2e1b21385c26c926,
    0x4d2c6dfc5ac42aed,
    0x53380d139d95b3df,
    0x650a73548baf63de,
    0x766a0abb3c77b2a8,
    0x81c2c92e47edaee6,
    0x92722c851482353b,
    0xa2bfe8a14cf10364,
    0xa81a664bbc423001,
    0xc24b8b70d0f89791,
    0xc76c51a30654be30,
    0xd192e819d6ef5218,
    0xd69906245565a910,
    0xf40e35855771202a,
    0x106aa07032bbd1b8,
    0x19a4c116b8d2d0c8,
    0x1e376c085141ab53,
    0x2748774cdf8eeb99,
    0x34b0bcb5e19b48a8,
    0x391c0cb3c5c95a63,
    0x4ed8aa4ae3418acb,
    0x5b9cca4f7763e373,
    0x682e6ff3d6b2b8a3,
    0x748f82ee5defb2fc,
    0x78a5636f43172f60,
    0x84c87814a1f0ab72,
    0x8cc702081a6439ec,
    0x90befffa23631e28,
    0xa4506cebde82bde9,
    0xbef9a3f7b2c67915,
    0xc67178f2e372532b,
    0xca273eceea26619c,
    0xd186b8c721c0c207,
    0xeada7dd6cde0eb1e,
    0xf57d4f7fee6ed178,
    0x06f067aa72176fba,
    0x0a637dc5a2c898a6,
    0x113f9804bef90dae,
    0x1b710b35131c471b,
    0x28db77f523047d84,
    0x32caab7b40c72493,
    0x3c9ebe0a15c9bebc,
    0x431d67c49c100d4c,
    0x4cc5d4becb3e42b6,
    0x597f299cfc657e2a,
    0x5fcb6fab3ad6faec,
    0x6c44198c4a475817,
];
const IV256: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];
const IV384: [u64; 8] = [
    0xcbbb9d5dc1059ed8,
    0x629a292a367cd507,
    0x9159015a3070dd17,
    0x152fecd8f70e5939,
    0x67332667ffc00b31,
    0x8eb44a8768581511,
    0xdb0c2e0d64f98fa7,
    0x47b5481dbefa4fa4,
];
const IV512: [u64; 8] = [
    0x6a09e667f3bcc908,
    0xbb67ae8584caa73b,
    0x3c6ef372fe94f82b,
    0xa54ff53a5f1d36f1,
    0x510e527fade682d1,
    0x9b05688c2b3e6c1f,
    0x1f83d9abfb41bd6b,
    0x5be0cd19137e2179,
];

#[cfg(test)]
mod tests {
    #![allow(clippy::indexing_slicing)]
    use super::{sha1, Blocks, Digest, Sha256, Sha384, Sha512};

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    // FIPS 180's "abc" for each; the wider corpus is tests/vectors.rs.
    #[test]
    fn abc() {
        assert_eq!(
            hex(&sha1(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(
            hex(&Sha256::digest(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(&Sha384::digest(b"abc")),
            "cb00753f45a35e8bb5a03d699ac65007272c32ab0eded163\
             1a8b605a43ff5bed8086072ba1e7cc2358baeca134c825a7"
        );
        assert_eq!(
            hex(&Sha512::digest(b"abc")),
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a\
             2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
        );
    }

    // The longest corpus message is far below these; the length field is not.
    fn padded_length<const B: usize>(length_len: usize, total: u128) -> Vec<u8> {
        let mut blocks = Blocks::<B>::new();
        blocks.total = total;
        let mut last = Vec::new();
        blocks.pad(length_len, &mut |block| last = block.to_vec());
        last[B - length_len..].to_vec()
    }

    #[test]
    fn the_length_field_holds_lengths_past_32_and_64_bits() {
        let five_gib = 5u128 << 30;
        assert_eq!(
            padded_length::<64>(8, five_gib),
            (five_gib * 8).to_be_bytes()[8..].to_vec()
        );
        let huge = 1u128 << 70;
        assert_eq!(
            padded_length::<128>(16, huge),
            (huge * 8).to_be_bytes().to_vec()
        );
    }

    fn splits<D: Digest>() {
        let data: Vec<u8> = (0..600u32).map(|i| (i * 7) as u8).collect();
        let whole = D::digest(&data);
        for at in 0..data.len() {
            let mut hash = D::new();
            hash.update(&data[..at]);
            let fork = hash.clone();
            hash.update(&data[at..]);
            assert_eq!(hash.finalize().as_ref(), whole.as_ref(), "split at {at}");
            let mut fork = fork;
            fork.update(&data[at..]);
            assert_eq!(fork.finalize().as_ref(), whole.as_ref(), "fork at {at}");
        }
    }

    #[test]
    fn every_split_and_fork_agrees_with_the_whole() {
        splits::<Sha256>();
        splits::<Sha384>();
        splits::<Sha512>();
    }
}

#[cfg(test)]
mod backend_tests {
    #![allow(clippy::indexing_slicing, clippy::unwrap_used)]
    use super::{compress256_software, Blocks, IV256};
    use crate::test_corpus::{expand, hex};

    type Compress = Box<dyn Fn(&mut [u32; 8], &[u8; 64])>;

    fn digest(compress: &Compress, data: &[u8]) -> Vec<u8> {
        let mut h = IV256;
        let mut blocks = Blocks::<64>::new();
        blocks.absorb(data, &mut |block| compress(&mut h, block));
        blocks.pad(8, &mut |block| compress(&mut h, block));
        h.iter().flat_map(|word| word.to_be_bytes()).collect()
    }

    #[cfg(target_arch = "x86_64")]
    fn hardware() -> Option<Compress> {
        let cpu = crate::accel::ShaNi::detect()?;
        Some(Box::new(move |h, block| cpu.compress256(h, block)))
    }

    #[cfg(not(target_arch = "x86_64"))]
    fn hardware() -> Option<Compress> {
        None
    }

    /// Each backend this machine has, which is always the software one.
    fn backends() -> Vec<Compress> {
        let software: Compress = Box::new(compress256_software);
        let mut all = vec![software];
        all.extend(hardware());
        all
    }

    #[test]
    fn every_backend_answers_python() {
        let mut rows = 0;
        for line in include_str!("../tests/digest_vectors.txt").lines() {
            let fields: Vec<&str> = line.split(' ').collect();
            if let ["sha256", seed, length, want] = fields[..] {
                let data = expand(seed.parse().unwrap(), length.parse().unwrap());
                for compress in &backends() {
                    assert_eq!(digest(compress, &data), hex(want), "{line}");
                }
                rows += 1;
            }
        }
        assert_eq!(rows, 263);
    }
}

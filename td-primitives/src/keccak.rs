//! SHA3-256, SHA3-512, SHAKE128 and SHAKE256 (FIPS 202), which ML-KEM is built from.
//! Lanes are read little-endian. Every step is an XOR, AND or rotation, so the time
//! depends only on how much goes in and comes out.

use crate::ct;

const ROUND_CONSTANTS: [u64; 24] = [
    0x0000_0000_0000_0001,
    0x0000_0000_0000_8082,
    0x8000_0000_0000_808a,
    0x8000_0000_8000_8000,
    0x0000_0000_0000_808b,
    0x0000_0000_8000_0001,
    0x8000_0000_8000_8081,
    0x8000_0000_0000_8009,
    0x0000_0000_0000_008a,
    0x0000_0000_0000_0088,
    0x0000_0000_8000_8009,
    0x0000_0000_8000_000a,
    0x0000_0000_8000_808b,
    0x8000_0000_0000_008b,
    0x8000_0000_0000_8089,
    0x8000_0000_0000_8003,
    0x8000_0000_0000_8002,
    0x8000_0000_0000_0080,
    0x0000_0000_0000_800a,
    0x8000_0000_8000_000a,
    0x8000_0000_8000_8081,
    0x8000_0000_0000_8080,
    0x0000_0000_8000_0001,
    0x8000_0000_8000_8008,
];

/// Keccak-f[1600], FIPS 202 section 3.3. `aN` is lane x + 5y.
fn keccak_f(state: &mut [u64; 25]) {
    let [mut a0, mut a1, mut a2, mut a3, mut a4, mut a5, mut a6, mut a7, mut a8, mut a9, mut a10, mut a11, mut a12, mut a13, mut a14, mut a15, mut a16, mut a17, mut a18, mut a19, mut a20, mut a21, mut a22, mut a23, mut a24] =
        *state;
    for rc in ROUND_CONSTANTS {
        // Theta.
        let c0 = a0 ^ a5 ^ a10 ^ a15 ^ a20;
        let c1 = a1 ^ a6 ^ a11 ^ a16 ^ a21;
        let c2 = a2 ^ a7 ^ a12 ^ a17 ^ a22;
        let c3 = a3 ^ a8 ^ a13 ^ a18 ^ a23;
        let c4 = a4 ^ a9 ^ a14 ^ a19 ^ a24;
        let d0 = c4 ^ c1.rotate_left(1);
        let d1 = c0 ^ c2.rotate_left(1);
        let d2 = c1 ^ c3.rotate_left(1);
        let d3 = c2 ^ c4.rotate_left(1);
        let d4 = c3 ^ c0.rotate_left(1);
        // Rho and pi together: A'[x, y] = A[(x + 3y) mod 5, x], rotated.
        let b0 = a0 ^ d0;
        let b1 = (a6 ^ d1).rotate_left(44);
        let b2 = (a12 ^ d2).rotate_left(43);
        let b3 = (a18 ^ d3).rotate_left(21);
        let b4 = (a24 ^ d4).rotate_left(14);
        let b5 = (a3 ^ d3).rotate_left(28);
        let b6 = (a9 ^ d4).rotate_left(20);
        let b7 = (a10 ^ d0).rotate_left(3);
        let b8 = (a16 ^ d1).rotate_left(45);
        let b9 = (a22 ^ d2).rotate_left(61);
        let b10 = (a1 ^ d1).rotate_left(1);
        let b11 = (a7 ^ d2).rotate_left(6);
        let b12 = (a13 ^ d3).rotate_left(25);
        let b13 = (a19 ^ d4).rotate_left(8);
        let b14 = (a20 ^ d0).rotate_left(18);
        let b15 = (a4 ^ d4).rotate_left(27);
        let b16 = (a5 ^ d0).rotate_left(36);
        let b17 = (a11 ^ d1).rotate_left(10);
        let b18 = (a17 ^ d2).rotate_left(15);
        let b19 = (a23 ^ d3).rotate_left(56);
        let b20 = (a2 ^ d2).rotate_left(62);
        let b21 = (a8 ^ d3).rotate_left(55);
        let b22 = (a14 ^ d4).rotate_left(39);
        let b23 = (a15 ^ d0).rotate_left(41);
        let b24 = (a21 ^ d1).rotate_left(2);
        // Chi and iota.
        a0 = b0 ^ (!b1 & b2) ^ rc;
        a1 = b1 ^ (!b2 & b3);
        a2 = b2 ^ (!b3 & b4);
        a3 = b3 ^ (!b4 & b0);
        a4 = b4 ^ (!b0 & b1);
        a5 = b5 ^ (!b6 & b7);
        a6 = b6 ^ (!b7 & b8);
        a7 = b7 ^ (!b8 & b9);
        a8 = b8 ^ (!b9 & b5);
        a9 = b9 ^ (!b5 & b6);
        a10 = b10 ^ (!b11 & b12);
        a11 = b11 ^ (!b12 & b13);
        a12 = b12 ^ (!b13 & b14);
        a13 = b13 ^ (!b14 & b10);
        a14 = b14 ^ (!b10 & b11);
        a15 = b15 ^ (!b16 & b17);
        a16 = b16 ^ (!b17 & b18);
        a17 = b17 ^ (!b18 & b19);
        a18 = b18 ^ (!b19 & b15);
        a19 = b19 ^ (!b15 & b16);
        a20 = b20 ^ (!b21 & b22);
        a21 = b21 ^ (!b22 & b23);
        a22 = b22 ^ (!b23 & b24);
        a23 = b23 ^ (!b24 & b20);
        a24 = b24 ^ (!b20 & b21);
    }
    *state = [
        a0, a1, a2, a3, a4, a5, a6, a7, a8, a9, a10, a11, a12, a13, a14, a15, a16, a17, a18, a19,
        a20, a21, a22, a23, a24,
    ];
}

/// `pending` buffers one rate block: absorbed input, then squeezed output.
struct Sponge {
    state: [u64; 25],
    pending: [u8; 200],
    at: usize,
    rate: usize,
    suffix: u8,
    squeezing: bool,
}

impl Drop for Sponge {
    // ML-KEM hashes secrets through this.
    fn drop(&mut self) {
        ct::zero_words(&mut self.state);
        ct::zero(&mut self.pending);
    }
}

impl Sponge {
    const fn new(rate: usize, suffix: u8) -> Self {
        Self {
            state: [0; 25],
            pending: [0; 200],
            at: 0,
            rate,
            suffix,
            squeezing: false,
        }
    }

    fn absorb(&mut self, mut data: &[u8]) {
        if self.squeezing {
            return;
        }
        while !data.is_empty() {
            let free = self.pending.get_mut(self.at..self.rate).unwrap_or(&mut []);
            let take = free.len().min(data.len());
            let (head, tail) = data.split_at(take);
            if let Some(dst) = free.get_mut(..take) {
                dst.copy_from_slice(head);
            }
            data = tail;
            self.at += take;
            if self.at == self.rate {
                self.absorb_block();
                self.at = 0;
            }
        }
    }

    fn absorb_block(&mut self) {
        let block = self.pending.get(..self.rate).unwrap_or(&[]);
        for (lane, bytes) in self.state.iter_mut().zip(block.as_chunks::<8>().0) {
            *lane ^= u64::from_le_bytes(*bytes);
        }
        keccak_f(&mut self.state);
    }

    /// FIPS 202 section B.2: the suffix and pad10*1 as whole bytes.
    fn finish_absorb(&mut self) {
        if let Some(free) = self.pending.get_mut(self.at..self.rate) {
            free.fill(0);
            if let Some(first) = free.first_mut() {
                *first = self.suffix;
            }
            if let Some(last) = free.last_mut() {
                *last |= 0x80;
            }
        }
        self.absorb_block();
        self.squeezing = true;
        self.refill();
    }

    fn refill(&mut self) {
        let block = self.pending.get_mut(..self.rate).unwrap_or(&mut []);
        for (bytes, lane) in block.as_chunks_mut::<8>().0.iter_mut().zip(self.state) {
            *bytes = lane.to_le_bytes();
        }
        self.at = 0;
    }

    fn squeeze(&mut self, mut out: &mut [u8]) {
        if !self.squeezing {
            self.finish_absorb();
        }
        while !out.is_empty() {
            if self.at == self.rate {
                keccak_f(&mut self.state);
                self.refill();
            }
            let ready = self.pending.get(self.at..self.rate).unwrap_or(&[]);
            let take = ready.len().min(out.len());
            let (head, tail) = std::mem::take(&mut out).split_at_mut(take);
            if let Some(src) = ready.get(..take) {
                head.copy_from_slice(src);
            }
            out = tail;
            self.at += take;
        }
    }
}

pub fn sha3_256(d: &[u8]) -> [u8; 32] {
    let mut sponge = Sponge::new(136, 0x06);
    sponge.absorb(d);
    let mut out = [0u8; 32];
    sponge.squeeze(&mut out);
    out
}

pub fn sha3_512(d: &[u8]) -> [u8; 64] {
    let mut sponge = Sponge::new(72, 0x06);
    sponge.absorb(d);
    let mut out = [0u8; 64];
    sponge.squeeze(&mut out);
    out
}

/// `absorb` after the first `squeeze` is ignored: the sponge is already padded.
pub struct Shake128(Sponge);

impl Default for Shake128 {
    fn default() -> Self {
        Self::new()
    }
}

impl Shake128 {
    pub const fn new() -> Self {
        Self(Sponge::new(168, 0x1f))
    }

    pub fn absorb(&mut self, d: &[u8]) {
        self.0.absorb(d);
    }

    pub fn squeeze(&mut self, out: &mut [u8]) {
        self.0.squeeze(out);
    }
}

/// `absorb` after the first `squeeze` is ignored: the sponge is already padded.
pub struct Shake256(Sponge);

impl Default for Shake256 {
    fn default() -> Self {
        Self::new()
    }
}

impl Shake256 {
    pub const fn new() -> Self {
        Self(Sponge::new(136, 0x1f))
    }

    pub fn absorb(&mut self, d: &[u8]) {
        self.0.absorb(d);
    }

    pub fn squeeze(&mut self, out: &mut [u8]) {
        self.0.squeeze(out);
    }
}

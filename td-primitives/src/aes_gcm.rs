//! AES-128-GCM and AES-256-GCM (SP 800-38D) with 96-bit nonces and 128-bit
//! tags. AES is bitsliced over four blocks and GHASH multiplies with integer
//! multiplication, so neither indexes memory by a secret. Both are translated
//! from BearSSL's `aes_ct64` and `ghash_ctmul64` (THIRD-PARTY.md); the S-box is
//! Boyar and Peralta's circuit.

use crate::{ct, Refused};

pub const NONCE_LEN: usize = 12;
pub const TAG_LEN: usize = 16;

/// SP 800-38D 5.2.1.1: 2^39 - 256 bits.
const MAX_LEN: u64 = (1 << 36) - 32;

const RCON: [u32; 10] = [0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80, 0x1b, 0x36];

type Bits = [u64; 8];
type FourBlocks = [[u32; 4]; 4];

/// The whole expanded key, in an array so that encryption has no shape it
/// could fall through: AES-128 uses keys 0 to 10, AES-256 keys 0 to 14. Boxed so
/// that moving a cipher copies no round key; H still moves with it.
struct RoundKeys {
    keys: Box<[Bits; 15]>,
    wide: bool,
}

pub struct AesGcm {
    round_keys: RoundKeys,
    hash_key: HashKey,
}

impl AesGcm {
    pub fn new_128(key: &[u8; 16]) -> Self {
        let [.., a, b, c, d] = *key;
        Self::new(key, u32::from_le_bytes([a, b, c, d]), false)
    }

    pub fn new_256(key: &[u8; 32]) -> Self {
        let [.., a, b, c, d] = *key;
        Self::new(key, u32::from_le_bytes([a, b, c, d]), true)
    }

    fn new(key: &[u8], last_word: u32, wide: bool) -> Self {
        let mut schedule = expand_key(key, last_word, wide);
        let mut keys = Box::new([[0u64; 8]; 15]);
        for (slot, words) in keys.iter_mut().zip(schedule.as_chunks::<4>().0) {
            let (a, b) = interleave_in(*words);
            *slot = [a, a, a, a, b, b, b, b];
            ortho(slot);
        }
        schedule.fill(0);
        std::hint::black_box(&schedule);
        let round_keys = RoundKeys { keys, wide };
        let [zero, ..] = encrypt_blocks(&round_keys, [[0; 4]; 4]);
        Self {
            hash_key: HashKey::new(block_bytes(&zero)),
            round_keys,
        }
    }

    /// Encrypt `in_out` where it lies and return the tag. A nonce must never
    /// repeat under one key: GCM then gives up both secrecy and integrity.
    pub fn seal(
        &self,
        nonce: &[u8; NONCE_LEN],
        aad: &[u8],
        in_out: &mut [u8],
    ) -> Result<[u8; TAG_LEN], Refused> {
        check_len(in_out.len())?;
        self.ctr(nonce, in_out);
        Ok(self.tag(nonce, aad, in_out))
    }

    /// Check the tag, then decrypt `in_out` where it lies. A refused input is
    /// left as ciphertext: nothing is decrypted before the tag holds.
    pub fn open(
        &self,
        nonce: &[u8; NONCE_LEN],
        aad: &[u8],
        in_out: &mut [u8],
        tag: &[u8; TAG_LEN],
    ) -> Result<(), Refused> {
        check_len(in_out.len())?;
        if !ct::eq(&self.tag(nonce, aad, in_out), tag) {
            return Err(Refused);
        }
        self.ctr(nonce, in_out);
        Ok(())
    }

    fn ctr(&self, nonce: &[u8; NONCE_LEN], data: &mut [u8]) {
        let mut counter = 2u32;
        for chunk in data.chunks_mut(64) {
            let blocks =
                [0u32, 1, 2, 3].map(|lane| counter_block(nonce, counter.wrapping_add(lane)));
            let stream = encrypt_blocks(&self.round_keys, blocks);
            let mut pad = [0u8; 64];
            for (bytes, word) in pad
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(stream.as_flattened())
            {
                *bytes = word.to_le_bytes();
            }
            for (byte, mask) in chunk.iter_mut().zip(pad) {
                *byte ^= mask;
            }
            counter = counter.wrapping_add(4);
        }
    }

    fn tag(&self, nonce: &[u8; NONCE_LEN], aad: &[u8], ciphertext: &[u8]) -> [u8; TAG_LEN] {
        let mut y = [0u64; 2];
        self.hash_key.padded(&mut y, aad);
        self.hash_key.padded(&mut y, ciphertext);
        let mut lengths = [0u8; 16];
        for (bytes, len) in lengths
            .as_chunks_mut::<8>()
            .0
            .iter_mut()
            .zip([aad.len(), ciphertext.len()])
        {
            *bytes = (len as u64).wrapping_mul(8).to_be_bytes();
        }
        self.hash_key.absorb(&mut y, &lengths);

        let [first, ..] = encrypt_blocks(&self.round_keys, [counter_block(nonce, 1); 4]);
        let mut tag = block_bytes(&first);
        let [high, low] = y;
        for (bytes, word) in tag.as_chunks_mut::<8>().0.iter_mut().zip([high, low]) {
            *bytes = (u64::from_be_bytes(*bytes) ^ word).to_be_bytes();
        }
        tag
    }

    #[cfg(test)]
    fn encrypt_block(&self, block: &[u8; 16]) -> [u8; 16] {
        let mut words = [0u32; 4];
        for (word, bytes) in words.iter_mut().zip(block.as_chunks::<4>().0) {
            *word = u32::from_le_bytes(*bytes);
        }
        let [first, ..] = encrypt_blocks(&self.round_keys, [words; 4]);
        block_bytes(&first)
    }
}

impl Drop for AesGcm {
    fn drop(&mut self) {
        ct::zero_words(self.round_keys.keys.as_flattened_mut());
        self.hash_key.erase();
    }
}

fn check_len(len: usize) -> Result<(), Refused> {
    if u64::try_from(len).map_err(|_| Refused)? > MAX_LEN {
        return Err(Refused);
    }
    Ok(())
}

fn counter_block(nonce: &[u8; NONCE_LEN], counter: u32) -> [u32; 4] {
    let mut words = [0u32; 4];
    for (word, bytes) in words.iter_mut().zip(nonce.as_chunks::<4>().0) {
        *word = u32::from_le_bytes(*bytes);
    }
    let [a, b, c, _] = words;
    [a, b, c, u32::from_le_bytes(counter.to_be_bytes())]
}

fn block_bytes(words: &[u32; 4]) -> [u8; 16] {
    let mut out = [0u8; 16];
    for (bytes, word) in out.as_chunks_mut::<4>().0.iter_mut().zip(words) {
        *bytes = word.to_le_bytes();
    }
    out
}

/// FIPS 197 5.2, whole groups of Nk words, in two buffers sized for all they
/// will hold, so no reallocation leaves a copy of the key behind.
/// AES-128 takes 44 words. AES-256 takes 60 and writes 64; the caller clears all.
fn expand_key(key: &[u8], last_word: u32, wide: bool) -> Vec<u32> {
    let mut group: Vec<u32> = key
        .as_chunks::<4>()
        .0
        .iter()
        .map(|bytes| u32::from_le_bytes(*bytes))
        .collect();
    let mut schedule = Vec::with_capacity(64);
    schedule.extend_from_slice(&group);
    let want = if wide { 60 } else { 44 };
    let mut word = last_word;
    for round_constant in RCON {
        if schedule.len() >= want {
            break;
        }
        for (position, slot) in group.iter_mut().enumerate() {
            if position == 0 {
                word = sub_word(word.rotate_right(8)) ^ round_constant;
            } else if wide && position == 4 {
                word = sub_word(word);
            }
            word ^= *slot;
            *slot = word;
            schedule.push(word);
        }
    }
    group.fill(0);
    std::hint::black_box(&group);
    schedule
}

fn sub_word(word: u32) -> u32 {
    let mut q: Bits = [u64::from(word), 0, 0, 0, 0, 0, 0, 0];
    ortho(&mut q);
    sbox(&mut q);
    ortho(&mut q);
    let [low, ..] = q;
    low as u32
}

fn encrypt_blocks(round_keys: &RoundKeys, blocks: FourBlocks) -> FourBlocks {
    let [a, b, c, d] = blocks;
    let (q0, q4) = interleave_in(a);
    let (q1, q5) = interleave_in(b);
    let (q2, q6) = interleave_in(c);
    let (q3, q7) = interleave_in(d);
    let mut q = [q0, q1, q2, q3, q4, q5, q6, q7];
    ortho(&mut q);
    let keys: &[Bits; 15] = &round_keys.keys;
    let (rounds, last) = if round_keys.wide {
        (14, &keys[14])
    } else {
        (10, &keys[10])
    };
    add_round_key(&mut q, &keys[0]);
    for key in keys.iter().take(rounds).skip(1) {
        sbox(&mut q);
        shift_rows(&mut q);
        mix_columns(&mut q);
        add_round_key(&mut q, key);
    }
    sbox(&mut q);
    shift_rows(&mut q);
    add_round_key(&mut q, last);
    ortho(&mut q);
    let [q0, q1, q2, q3, q4, q5, q6, q7] = q;
    [
        interleave_out(q0, q4),
        interleave_out(q1, q5),
        interleave_out(q2, q6),
        interleave_out(q3, q7),
    ]
}

fn add_round_key(q: &mut Bits, key: &Bits) {
    for (word, round) in q.iter_mut().zip(key) {
        *word ^= round;
    }
}

fn swap_bits(x: &mut u64, y: &mut u64, low: u64, shift: u32) {
    let (a, b) = (*x, *y);
    *x = (a & low) | ((b & low) << shift);
    *y = ((a & !low) >> shift) | (b & !low);
}

fn ortho(q: &mut Bits) {
    const TWO: u64 = 0x5555555555555555;
    const FOUR: u64 = 0x3333333333333333;
    const EIGHT: u64 = 0x0f0f0f0f0f0f0f0f;
    let [q0, q1, q2, q3, q4, q5, q6, q7] = q;
    swap_bits(q0, q1, TWO, 1);
    swap_bits(q2, q3, TWO, 1);
    swap_bits(q4, q5, TWO, 1);
    swap_bits(q6, q7, TWO, 1);

    swap_bits(q0, q2, FOUR, 2);
    swap_bits(q1, q3, FOUR, 2);
    swap_bits(q4, q6, FOUR, 2);
    swap_bits(q5, q7, FOUR, 2);

    swap_bits(q0, q4, EIGHT, 4);
    swap_bits(q1, q5, EIGHT, 4);
    swap_bits(q2, q6, EIGHT, 4);
    swap_bits(q3, q7, EIGHT, 4);
}

fn interleave_in(words: [u32; 4]) -> (u64, u64) {
    let [x0, x1, x2, x3] = words.map(|word| {
        let mut x = u64::from(word);
        x |= x << 16;
        x &= 0x0000ffff0000ffff;
        x |= x << 8;
        x & 0x00ff00ff00ff00ff
    });
    (x0 | (x2 << 8), x1 | (x3 << 8))
}

fn interleave_out(q0: u64, q1: u64) -> [u32; 4] {
    [q0, q1, q0 >> 8, q1 >> 8].map(|word| {
        let mut x = word & 0x00ff00ff00ff00ff;
        x |= x >> 8;
        x &= 0x0000ffff0000ffff;
        (x as u32) | ((x >> 16) as u32)
    })
}

fn shift_rows(q: &mut Bits) {
    for word in q.iter_mut() {
        let x = *word;
        *word = (x & 0x000000000000ffff)
            | ((x & 0x00000000fff00000) >> 4)
            | ((x & 0x00000000000f0000) << 12)
            | ((x & 0x0000ff0000000000) >> 8)
            | ((x & 0x000000ff00000000) << 8)
            | ((x & 0xf000000000000000) >> 12)
            | ((x & 0x0fff000000000000) << 4);
    }
}

fn mix_columns(q: &mut Bits) {
    let [q0, q1, q2, q3, q4, q5, q6, q7] = *q;
    let [r0, r1, r2, r3, r4, r5, r6, r7] = q.map(|x| x.rotate_right(16));
    *q = [
        q7 ^ r7 ^ r0 ^ (q0 ^ r0).rotate_left(32),
        q0 ^ r0 ^ q7 ^ r7 ^ r1 ^ (q1 ^ r1).rotate_left(32),
        q1 ^ r1 ^ r2 ^ (q2 ^ r2).rotate_left(32),
        q2 ^ r2 ^ q7 ^ r7 ^ r3 ^ (q3 ^ r3).rotate_left(32),
        q3 ^ r3 ^ q7 ^ r7 ^ r4 ^ (q4 ^ r4).rotate_left(32),
        q4 ^ r4 ^ r5 ^ (q5 ^ r5).rotate_left(32),
        q5 ^ r5 ^ r6 ^ (q6 ^ r6).rotate_left(32),
        q6 ^ r6 ^ r7 ^ (q7 ^ r7).rotate_left(32),
    ];
}

/// Boyar and Peralta's circuit, whose x0 is the high bit and x7 the low.
fn sbox(q: &mut Bits) {
    let [x7, x6, x5, x4, x3, x2, x1, x0] = *q;

    let y14 = x3 ^ x5;
    let y13 = x0 ^ x6;
    let y9 = x0 ^ x3;
    let y8 = x0 ^ x5;
    let t0 = x1 ^ x2;
    let y1 = t0 ^ x7;
    let y4 = y1 ^ x3;
    let y12 = y13 ^ y14;
    let y2 = y1 ^ x0;
    let y5 = y1 ^ x6;
    let y3 = y5 ^ y8;
    let t1 = x4 ^ y12;
    let y15 = t1 ^ x5;
    let y20 = t1 ^ x1;
    let y6 = y15 ^ x7;
    let y10 = y15 ^ t0;
    let y11 = y20 ^ y9;
    let y7 = x7 ^ y11;
    let y17 = y10 ^ y11;
    let y19 = y10 ^ y8;
    let y16 = t0 ^ y11;
    let y21 = y13 ^ y16;
    let y18 = x0 ^ y16;

    let t2 = y12 & y15;
    let t3 = y3 & y6;
    let t4 = t3 ^ t2;
    let t5 = y4 & x7;
    let t6 = t5 ^ t2;
    let t7 = y13 & y16;
    let t8 = y5 & y1;
    let t9 = t8 ^ t7;
    let t10 = y2 & y7;
    let t11 = t10 ^ t7;
    let t12 = y9 & y11;
    let t13 = y14 & y17;
    let t14 = t13 ^ t12;
    let t15 = y8 & y10;
    let t16 = t15 ^ t12;
    let t17 = t4 ^ t14;
    let t18 = t6 ^ t16;
    let t19 = t9 ^ t14;
    let t20 = t11 ^ t16;
    let t21 = t17 ^ y20;
    let t22 = t18 ^ y19;
    let t23 = t19 ^ y21;
    let t24 = t20 ^ y18;

    let t25 = t21 ^ t22;
    let t26 = t21 & t23;
    let t27 = t24 ^ t26;
    let t28 = t25 & t27;
    let t29 = t28 ^ t22;
    let t30 = t23 ^ t24;
    let t31 = t22 ^ t26;
    let t32 = t31 & t30;
    let t33 = t32 ^ t24;
    let t34 = t23 ^ t33;
    let t35 = t27 ^ t33;
    let t36 = t24 & t35;
    let t37 = t36 ^ t34;
    let t38 = t27 ^ t36;
    let t39 = t29 & t38;
    let t40 = t25 ^ t39;

    let t41 = t40 ^ t37;
    let t42 = t29 ^ t33;
    let t43 = t29 ^ t40;
    let t44 = t33 ^ t37;
    let t45 = t42 ^ t41;
    let z0 = t44 & y15;
    let z1 = t37 & y6;
    let z2 = t33 & x7;
    let z3 = t43 & y16;
    let z4 = t40 & y1;
    let z5 = t29 & y7;
    let z6 = t42 & y11;
    let z7 = t45 & y17;
    let z8 = t41 & y10;
    let z9 = t44 & y12;
    let z10 = t37 & y3;
    let z11 = t33 & y4;
    let z12 = t43 & y13;
    let z13 = t40 & y5;
    let z14 = t29 & y2;
    let z15 = t42 & y9;
    let z16 = t45 & y14;
    let z17 = t41 & y8;

    let t46 = z15 ^ z16;
    let t47 = z10 ^ z11;
    let t48 = z5 ^ z13;
    let t49 = z9 ^ z10;
    let t50 = z2 ^ z12;
    let t51 = z2 ^ z5;
    let t52 = z7 ^ z8;
    let t53 = z0 ^ z3;
    let t54 = z6 ^ z7;
    let t55 = z16 ^ z17;
    let t56 = z12 ^ t48;
    let t57 = t50 ^ t53;
    let t58 = z4 ^ t46;
    let t59 = z3 ^ t54;
    let t60 = t46 ^ t57;
    let t61 = z14 ^ t57;
    let t62 = t52 ^ t58;
    let t63 = t49 ^ t58;
    let t64 = z4 ^ t59;
    let t65 = t61 ^ t62;
    let t66 = z1 ^ t63;
    let s0 = t59 ^ t63;
    let s6 = t56 ^ !t62;
    let s7 = t48 ^ !t60;
    let t67 = t64 ^ t65;
    let s3 = t53 ^ t66;
    let s4 = t51 ^ t66;
    let s5 = t47 ^ t65;
    let s1 = t64 ^ !s3;
    let s2 = t55 ^ !t67;

    *q = [s7, s6, s5, s4, s3, s2, s1, s0];
}

/// H and its bit reversal, in the two 64-bit halves GHASH multiplies.
struct HashKey {
    h0: u64,
    h1: u64,
    h2: u64,
    h0r: u64,
    h1r: u64,
    h2r: u64,
}

impl HashKey {
    fn new(bytes: [u8; 16]) -> Self {
        let whole = u128::from_be_bytes(bytes);
        let h1 = (whole >> 64) as u64;
        let h0 = whole as u64;
        let h0r = h0.reverse_bits();
        let h1r = h1.reverse_bits();
        Self {
            h0,
            h1,
            h2: h0 ^ h1,
            h0r,
            h1r,
            h2r: h0r ^ h1r,
        }
    }

    fn erase(&mut self) {
        let Self {
            h0,
            h1,
            h2,
            h0r,
            h1r,
            h2r,
        } = self;
        for word in [h0, h1, h2, h0r, h1r, h2r] {
            *word = 0;
            std::hint::black_box(word);
        }
    }

    /// `data` in 16-byte blocks, its tail padded with zeros.
    fn padded(&self, y: &mut [u64; 2], data: &[u8]) {
        let (blocks, rest) = data.as_chunks::<16>();
        for block in blocks {
            self.absorb(y, block);
        }
        if !rest.is_empty() {
            let mut last = [0u8; 16];
            for (out, byte) in last.iter_mut().zip(rest) {
                *out = *byte;
            }
            self.absorb(y, &last);
        }
    }

    /// y = (y ^ block) * H in GF(2^128); `y` is [high, low].
    fn absorb(&self, y: &mut [u64; 2], block: &[u8; 16]) {
        let whole = u128::from_be_bytes(*block);
        let [y_high, y_low] = *y;
        let y1 = y_high ^ ((whole >> 64) as u64);
        let y0 = y_low ^ (whole as u64);
        let y0r = y0.reverse_bits();
        let y1r = y1.reverse_bits();
        let y2 = y0 ^ y1;
        let y2r = y0r ^ y1r;

        let z0 = carryless(y0, self.h0);
        let z1 = carryless(y1, self.h1);
        let mut z2 = carryless(y2, self.h2);
        let mut z0h = carryless(y0r, self.h0r);
        let mut z1h = carryless(y1r, self.h1r);
        let mut z2h = carryless(y2r, self.h2r);
        z2 ^= z0 ^ z1;
        z2h ^= z0h ^ z1h;
        z0h = z0h.reverse_bits() >> 1;
        z1h = z1h.reverse_bits() >> 1;
        z2h = z2h.reverse_bits() >> 1;

        let mut v0 = z0;
        let mut v1 = z0h ^ z2;
        let mut v2 = z1 ^ z2h;
        let mut v3 = z1h;

        v3 = (v3 << 1) | (v2 >> 63);
        v2 = (v2 << 1) | (v1 >> 63);
        v1 = (v1 << 1) | (v0 >> 63);
        v0 <<= 1;

        v2 ^= v0 ^ (v0 >> 1) ^ (v0 >> 2) ^ (v0 >> 7);
        v1 ^= (v0 << 63) ^ (v0 << 62) ^ (v0 << 57);
        v3 ^= v1 ^ (v1 >> 1) ^ (v1 >> 2) ^ (v1 >> 7);
        v2 ^= (v1 << 63) ^ (v1 << 62) ^ (v1 << 57);

        *y = [v3, v2];
    }
}

/// The low half of a carryless product, by integer multiplications whose
/// sparse operands leave room for carries to die between the kept bits.
fn carryless(x: u64, y: u64) -> u64 {
    const M0: u64 = 0x1111111111111111;
    const M1: u64 = 0x2222222222222222;
    const M2: u64 = 0x4444444444444444;
    const M3: u64 = 0x8888888888888888;
    let (x0, x1, x2, x3) = (x & M0, x & M1, x & M2, x & M3);
    let (y0, y1, y2, y3) = (y & M0, y & M1, y & M2, y & M3);
    let times = u64::wrapping_mul;
    let z0 = times(x0, y0) ^ times(x1, y3) ^ times(x2, y2) ^ times(x3, y1);
    let z1 = times(x0, y1) ^ times(x1, y0) ^ times(x2, y3) ^ times(x3, y2);
    let z2 = times(x0, y2) ^ times(x1, y1) ^ times(x2, y0) ^ times(x3, y3);
    let z3 = times(x0, y3) ^ times(x1, y2) ^ times(x2, y1) ^ times(x3, y0);
    (z0 & M0) | (z1 & M1) | (z2 & M2) | (z3 & M3)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::indexing_slicing, clippy::unwrap_used)]
    use super::{check_len, interleave_in, interleave_out, ortho, sbox, AesGcm, MAX_LEN};

    fn hex(text: &str) -> Vec<u8> {
        let text: Vec<u8> = text.bytes().filter(u8::is_ascii_hexdigit).collect();
        text.chunks(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }

    /// Multiplicative inverse, from which FIPS 197 5.1.1 defines the S-box.
    fn gf_inverse(x: u8) -> u8 {
        let multiply = |mut a: u8, mut b: u8| {
            let mut product = 0;
            while b != 0 {
                if b & 1 != 0 {
                    product ^= a;
                }
                let carry = a & 0x80 != 0;
                a <<= 1;
                if carry {
                    a ^= 0x1b;
                }
                b >>= 1;
            }
            product
        };
        (0..=255u8).find(|&y| multiply(x, y) == 1).unwrap_or(0)
    }

    fn fips_sbox(x: u8) -> u8 {
        let y = gf_inverse(x);
        y ^ y.rotate_left(1) ^ y.rotate_left(2) ^ y.rotate_left(3) ^ y.rotate_left(4) ^ 0x63
    }

    #[test]
    fn the_circuit_is_the_sbox_for_every_byte_in_every_lane() {
        for value in 0..=255u8 {
            // Every byte position of every lane at once.
            let word = u32::from_le_bytes([value; 4]);
            let (a, b) = interleave_in([word; 4]);
            let mut q = [a, a, a, a, b, b, b, b];
            ortho(&mut q);
            sbox(&mut q);
            ortho(&mut q);
            let want = fips_sbox(value);
            for lane in 0..4 {
                let out = interleave_out(q[lane], q[lane + 4]);
                for word in out {
                    assert_eq!(word.to_le_bytes(), [want; 4], "{value:#x} lane {lane}");
                }
            }
        }
    }

    // FIPS 197 appendix C.1 and C.3.
    #[test]
    fn fips_197_blocks() {
        let plain: [u8; 16] = hex("00112233445566778899aabbccddeeff").try_into().unwrap();
        let key128: [u8; 16] = hex("000102030405060708090a0b0c0d0e0f").try_into().unwrap();
        assert_eq!(
            AesGcm::new_128(&key128).encrypt_block(&plain).to_vec(),
            hex("69c4e0d86a7b0430d8cdb78070b4c55a")
        );
        let key256: [u8; 32] = hex("000102030405060708090a0b0c0d0e0f\
                                    101112131415161718191a1b1c1d1e1f")
        .try_into()
        .unwrap();
        assert_eq!(
            AesGcm::new_256(&key256).encrypt_block(&plain).to_vec(),
            hex("8ea2b7ca516745bfeafc49904b496089")
        );
    }

    #[test]
    fn the_length_bound_is_sp_800_38d() {
        assert_eq!(MAX_LEN, (1u64 << 32) * 16 - 2 * 16);
        assert!(check_len(MAX_LEN as usize).is_ok());
        assert!(check_len(MAX_LEN as usize + 1).is_err());
    }

    #[test]
    fn a_round_trip_refuses_any_changed_byte_and_leaves_it_ciphertext() {
        let aes = AesGcm::new_256(&[0x42; 32]);
        let nonce = [7u8; 12];
        let aad = b"header";
        for len in [0usize, 1, 15, 16, 17, 63, 64, 65, 200] {
            let plain = vec![0x5au8; len];
            let mut data = plain.clone();
            let tag = aes.seal(&nonce, aad, &mut data).unwrap();
            let sealed = data.clone();

            for at in 0..len {
                let mut changed = sealed.clone();
                changed[at] ^= 1;
                let before = changed.clone();
                assert!(aes.open(&nonce, aad, &mut changed, &tag).is_err());
                assert_eq!(changed, before);
            }
            let mut bad_tag = tag;
            bad_tag[0] ^= 1;
            assert!(aes.open(&nonce, aad, &mut data, &bad_tag).is_err());
            assert!(aes.open(&nonce, b"Header", &mut data, &tag).is_err());
            assert!(aes.open(&[8; 12], aad, &mut data, &tag).is_err());
            assert_eq!(data, sealed);

            aes.open(&nonce, aad, &mut data, &tag).unwrap();
            assert_eq!(data, plain);
        }
    }
}

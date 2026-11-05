//! ChaCha20-Poly1305 (RFC 8439). ChaCha20 is additions, XORs and rotations and
//! Poly1305 adds 64-by-64 products, so its time depends only on the lengths,
//! given that those products are constant time. Poly1305 is reachable only
//! through the AEAD: every block it sees is whole, padded by RFC 8439's
//! construction, which is why it has no short-final-block path. Its limbs and
//! finish follow poly1305-donna-64 (THIRD-PARTY.md).

use crate::{ct, Refused};

pub const KEY_LEN: usize = 32;
pub const NONCE_LEN: usize = 12;
pub const TAG_LEN: usize = 16;

/// RFC 8439 2.8: a 32-bit block counter starting at 1 covers this much.
const MAX_LEN: u64 = ((1 << 32) - 1) * 64;

const SIGMA: [u32; 4] = [0x61707865, 0x3320646e, 0x79622d32, 0x6b206574];
const MASK_44: u64 = (1 << 44) - 1;
const MASK_42: u64 = (1 << 42) - 1;

/// Encrypt `in_out` where it lies and return the tag. A nonce must never
/// repeat under one key: that reveals the keystream and the Poly1305 key.
pub fn seal(
    key: &[u8; KEY_LEN],
    nonce: &[u8; NONCE_LEN],
    aad: &[u8],
    in_out: &mut [u8],
) -> Result<[u8; TAG_LEN], Refused> {
    let cipher = Cipher::new(key, nonce, in_out.len())?;
    cipher.crypt(in_out);
    Ok(cipher.tag(aad, in_out))
}

/// Check the tag, then decrypt `in_out` where it lies. A refused input is left
/// as ciphertext: nothing is decrypted before the tag holds.
pub fn open(
    key: &[u8; KEY_LEN],
    nonce: &[u8; NONCE_LEN],
    aad: &[u8],
    in_out: &mut [u8],
    tag: &[u8; TAG_LEN],
) -> Result<(), Refused> {
    let cipher = Cipher::new(key, nonce, in_out.len())?;
    if !ct::eq(&cipher.tag(aad, in_out), tag) {
        return Err(Refused);
    }
    cipher.crypt(in_out);
    Ok(())
}

struct Cipher {
    key: [u32; 8],
    nonce: [u32; 3],
}

impl Cipher {
    fn new(key: &[u8; KEY_LEN], nonce: &[u8; NONCE_LEN], len: usize) -> Result<Self, Refused> {
        if u64::try_from(len).map_err(|_| Refused)? > MAX_LEN {
            return Err(Refused);
        }
        let mut words = [0u32; 8];
        for (word, bytes) in words.iter_mut().zip(key.as_chunks::<4>().0) {
            *word = u32::from_le_bytes(*bytes);
        }
        let mut nonce_words = [0u32; 3];
        for (word, bytes) in nonce_words.iter_mut().zip(nonce.as_chunks::<4>().0) {
            *word = u32::from_le_bytes(*bytes);
        }
        Ok(Self {
            key: words,
            nonce: nonce_words,
        })
    }

    fn crypt(&self, data: &mut [u8]) {
        let mut counter = 1u32;
        for chunk in data.chunks_mut(64) {
            let stream = block(&self.key, counter, &self.nonce);
            for (byte, pad) in chunk.iter_mut().zip(stream) {
                *byte ^= pad;
            }
            counter = counter.wrapping_add(1);
        }
    }

    fn tag(&self, aad: &[u8], ciphertext: &[u8]) -> [u8; TAG_LEN] {
        let mut first = block(&self.key, 0, &self.nonce);
        let mut mac = Poly1305::new(&first);
        ct::zero(&mut first);
        mac.padded(aad);
        mac.padded(ciphertext);
        let mut lengths = [0u8; 16];
        for (bytes, len) in lengths
            .as_chunks_mut::<8>()
            .0
            .iter_mut()
            .zip([aad.len(), ciphertext.len()])
        {
            *bytes = (len as u64).to_le_bytes();
        }
        mac.block(&lengths);
        mac.finish()
    }
}

impl Drop for Cipher {
    fn drop(&mut self) {
        self.key.fill(0);
        std::hint::black_box(&self.key);
    }
}

fn quarter(mut a: u32, mut b: u32, mut c: u32, mut d: u32) -> (u32, u32, u32, u32) {
    a = a.wrapping_add(b);
    d = (d ^ a).rotate_left(16);
    c = c.wrapping_add(d);
    b = (b ^ c).rotate_left(12);
    a = a.wrapping_add(b);
    d = (d ^ a).rotate_left(8);
    c = c.wrapping_add(d);
    b = (b ^ c).rotate_left(7);
    (a, b, c, d)
}

fn block(key: &[u32; 8], counter: u32, nonce: &[u32; 3]) -> [u8; 64] {
    let [k0, k1, k2, k3, k4, k5, k6, k7] = *key;
    let [n0, n1, n2] = *nonce;
    let [s0, s1, s2, s3] = SIGMA;
    let start = [
        s0, s1, s2, s3, k0, k1, k2, k3, k4, k5, k6, k7, counter, n0, n1, n2,
    ];
    let mut x = start;
    for _ in 0..10 {
        (x[0], x[4], x[8], x[12]) = quarter(x[0], x[4], x[8], x[12]);
        (x[1], x[5], x[9], x[13]) = quarter(x[1], x[5], x[9], x[13]);
        (x[2], x[6], x[10], x[14]) = quarter(x[2], x[6], x[10], x[14]);
        (x[3], x[7], x[11], x[15]) = quarter(x[3], x[7], x[11], x[15]);
        (x[0], x[5], x[10], x[15]) = quarter(x[0], x[5], x[10], x[15]);
        (x[1], x[6], x[11], x[12]) = quarter(x[1], x[6], x[11], x[12]);
        (x[2], x[7], x[8], x[13]) = quarter(x[2], x[7], x[8], x[13]);
        (x[3], x[4], x[9], x[14]) = quarter(x[3], x[4], x[9], x[14]);
    }
    let mut out = [0u8; 64];
    for ((bytes, mixed), initial) in out.as_chunks_mut::<4>().0.iter_mut().zip(x).zip(start) {
        *bytes = mixed.wrapping_add(initial).to_le_bytes();
    }
    out
}

/// Poly1305 in 44/44/42-bit limbs over 128-bit products.
struct Poly1305 {
    r: [u64; 3],
    s: [u64; 2],
    h: [u64; 3],
    pad: [u64; 2],
}

fn two_words(bytes: &[u8; 16]) -> (u64, u64) {
    let whole = u128::from_le_bytes(*bytes);
    (whole as u64, (whole >> 64) as u64)
}

impl Poly1305 {
    fn new(key_block: &[u8; 64]) -> Self {
        let mut clamp = [0u8; 16];
        let mut pad = [0u8; 16];
        let mut bytes = key_block.iter();
        for (out, byte) in clamp.iter_mut().chain(pad.iter_mut()).zip(&mut bytes) {
            *out = *byte;
        }
        let (t0, t1) = two_words(&clamp);
        let (p0, p1) = two_words(&pad);
        ct::zero(&mut clamp);
        ct::zero(&mut pad);
        let r0 = t0 & 0xffc0fffffff;
        let r1 = ((t0 >> 44) | (t1 << 20)) & 0xfffffc0ffff;
        let r2 = (t1 >> 24) & 0x00ffffffc0f;
        Self {
            r: [r0, r1, r2],
            s: [r1 * 20, r2 * 20],
            h: [0; 3],
            pad: [p0, p1],
        }
    }

    /// One whole 16-byte block with its 2^128 bit set.
    fn block(&mut self, block: &[u8; 16]) {
        let (t0, t1) = two_words(block);
        let [r0, r1, r2] = self.r;
        let [s1, s2] = self.s;
        let [mut h0, mut h1, mut h2] = self.h;
        h0 += t0 & MASK_44;
        h1 += ((t0 >> 44) | (t1 << 20)) & MASK_44;
        h2 += ((t1 >> 24) & MASK_42) | (1 << 40);
        let wide = |a: u64, b: u64| u128::from(a) * u128::from(b);
        let d0 = wide(h0, r0) + wide(h1, s2) + wide(h2, s1);
        let mut d1 = wide(h0, r1) + wide(h1, r0) + wide(h2, s2);
        let mut d2 = wide(h0, r2) + wide(h1, r1) + wide(h2, r0);
        h0 = (d0 as u64) & MASK_44;
        d1 += d0 >> 44;
        h1 = (d1 as u64) & MASK_44;
        d2 += d1 >> 44;
        h2 = (d2 as u64) & MASK_42;
        h0 += ((d2 >> 42) as u64) * 5;
        h1 += h0 >> 44;
        h0 &= MASK_44;
        self.h = [h0, h1, h2];
    }

    /// `data` in whole blocks, its tail padded with zeros (RFC 8439 2.8).
    fn padded(&mut self, data: &[u8]) {
        let (blocks, rest) = data.as_chunks::<16>();
        for block in blocks {
            self.block(block);
        }
        if !rest.is_empty() {
            let mut last = [0u8; 16];
            for (out, byte) in last.iter_mut().zip(rest) {
                *out = *byte;
            }
            self.block(&last);
        }
    }

    fn finish(self) -> [u8; 16] {
        let [mut h0, mut h1, mut h2] = self.h;
        for _ in 0..2 {
            h2 += h1 >> 44;
            h1 &= MASK_44;
            h0 += (h2 >> 42) * 5;
            h2 &= MASK_42;
            h1 += h0 >> 44;
            h0 &= MASK_44;
        }
        // h - p, kept when it did not borrow.
        let g0 = h0.wrapping_add(5);
        let g1 = h1.wrapping_add(g0 >> 44);
        let g2 = h2.wrapping_add(g1 >> 44).wrapping_sub(1 << 42);
        let keep_g = std::hint::black_box((g2 >> 63).wrapping_sub(1));
        h0 = (h0 & !keep_g) | (g0 & MASK_44 & keep_g);
        h1 = (h1 & !keep_g) | (g1 & MASK_44 & keep_g);
        h2 = (h2 & !keep_g) | (g2 & keep_g);

        let [p0, p1] = self.pad;
        h0 += p0 & MASK_44;
        h1 += (((p0 >> 44) | (p1 << 20)) & MASK_44) + (h0 >> 44);
        h0 &= MASK_44;
        h2 += (p1 >> 24) + (h1 >> 44);
        h1 &= MASK_44;
        h2 &= MASK_42;

        let mut tag = [0u8; 16];
        for (bytes, word) in tag
            .as_chunks_mut::<8>()
            .0
            .iter_mut()
            .zip([h0 | (h1 << 44), (h1 >> 20) | (h2 << 24)])
        {
            *bytes = word.to_le_bytes();
        }
        tag
    }
}

impl Drop for Poly1305 {
    fn drop(&mut self) {
        self.r.fill(0);
        self.s.fill(0);
        self.h.fill(0);
        self.pad.fill(0);
        std::hint::black_box((&self.r, &self.s, &self.h, &self.pad));
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::indexing_slicing, clippy::unwrap_used)]
    use super::{block, open, seal, Cipher, Poly1305, MAX_LEN};

    fn hex(text: &str) -> Vec<u8> {
        let text: Vec<u8> = text.bytes().filter(u8::is_ascii_hexdigit).collect();
        text.chunks(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }

    fn one_time(r: [u8; 16], s: [u8; 16], blocks: &[[u8; 16]]) -> [u8; 16] {
        let mut key = [0u8; 64];
        key[..16].copy_from_slice(&r);
        key[16..32].copy_from_slice(&s);
        let mut mac = Poly1305::new(&key);
        for one in blocks {
            mac.block(one);
        }
        mac.finish()
    }

    fn filled(first: u8, rest: u8) -> [u8; 16] {
        let mut out = [rest; 16];
        out[0] = first;
        out
    }

    // RFC 8439 A.3 #5 to #9, whose accumulators reach the edges of reduction
    // modulo 2^130 - 5 that the AEAD corpora do not.
    #[test]
    fn rfc_8439_reduction_edges() {
        let (one, two, zero, ones) = (filled(1, 0), filled(2, 0), [0; 16], [0xff; 16]);
        assert_eq!(one_time(two, zero, &[ones]), filled(3, 0));
        assert_eq!(one_time(two, ones, &[two]), filled(3, 0));
        assert_eq!(
            one_time(one, zero, &[ones, filled(0xf0, 0xff), filled(0x11, 0)]),
            filled(5, 0)
        );
        assert_eq!(
            one_time(one, zero, &[ones, filled(0xfb, 0xfe), [1; 16]]),
            zero
        );
        assert_eq!(
            one_time(two, zero, &[filled(0xfd, 0xff)]),
            filled(0xfa, 0xff)
        );
        // h is 2^130 + 3 before the last reduction: the one case in which the
        // finishing carry out of the top limb matters.
        let mut seven_f = [0xff; 16];
        seven_f[15] = 0x7f;
        assert_eq!(one_time(two, zero, &[seven_f, one]), filled(8, 0));
    }

    #[test]
    fn the_length_bound_stops_the_counter_short_of_block_zero() {
        assert_eq!(MAX_LEN, ((1u64 << 32) - 1) * 64);
        assert!(Cipher::new(&[0; 32], &[0; 12], MAX_LEN as usize).is_ok());
        assert!(Cipher::new(&[0; 32], &[0; 12], MAX_LEN as usize + 1).is_err());
    }

    // RFC 8439 2.3.2.
    #[test]
    fn rfc_8439_block() {
        let key: Vec<u8> = (0..32).collect();
        let mut words = [0u32; 8];
        for (word, bytes) in words.iter_mut().zip(key.chunks(4)) {
            *word = u32::from_le_bytes(bytes.try_into().unwrap());
        }
        let out = block(&words, 1, &[0x09000000, 0x4a000000, 0]);
        assert_eq!(
            out.to_vec(),
            hex(
                "10f1e7e4d13b5915500fdd1fa32071c4c7d1f4c733c068030422aa9ac3d46c4e\
                 d2826446079faa0914c2d705d98b02a2b5129cd1de164eb9cbd083e8a2503c4e"
            )
        );
    }

    // RFC 8439 2.8.2.
    #[test]
    fn rfc_8439_aead() {
        let key: [u8; 32] = hex("808182838485868788898a8b8c8d8e8f\
                                 909192939495969798999a9b9c9d9e9f")
        .try_into()
        .unwrap();
        let nonce: [u8; 12] = hex("070000004041424344454647").try_into().unwrap();
        let aad = hex("50515253c0c1c2c3c4c5c6c7");
        let plain = b"Ladies and Gentlemen of the class of '99: If I could offer you \
                      only one tip for the future, sunscreen would be it.";
        let mut data = plain.to_vec();
        let tag = seal(&key, &nonce, &aad, &mut data).unwrap();
        assert_eq!(tag.to_vec(), hex("1ae10b594f09e26a7e902ecbd0600691"));
        assert_eq!(
            data,
            hex(
                "d31a8d34648e60db7b86afbc53ef7ec2a4aded51296e08fea9e2b5a736ee62d6\
                 3dbea45e8ca9671282fafb69da92728b1a71de0a9e060b2905d6a5b67ecd3b36\
                 92ddbd7f2d778b8c9803aee328091b58fab324e4fad675945585808b4831d7bc\
                 3ff4def08e4b7a9de576d26586cec64b6116"
            )
        );
        open(&key, &nonce, &aad, &mut data, &tag).unwrap();
        assert_eq!(data, plain);

        // Any changed part is refused, and a refused input stays ciphertext.
        let mut sealed = plain.to_vec();
        let tag = seal(&key, &nonce, &aad, &mut sealed).unwrap();
        let before = sealed.clone();
        let mut bad = tag;
        bad[15] ^= 1;
        assert!(open(&key, &nonce, &aad, &mut sealed, &bad).is_err());
        assert_eq!(sealed, before);
        assert!(open(&key, &nonce, b"other", &mut sealed, &tag).is_err());
        assert_eq!(sealed, before);
        assert!(open(&key, &[9; 12], &aad, &mut sealed, &tag).is_err());
        assert_eq!(sealed, before);
        for at in 0..sealed.len() {
            let mut changed = before.clone();
            changed[at] ^= 1;
            let held = changed.clone();
            assert!(open(&key, &nonce, &aad, &mut changed, &tag).is_err());
            assert_eq!(changed, held);
        }
    }
}

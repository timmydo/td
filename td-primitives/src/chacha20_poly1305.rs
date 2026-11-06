//! ChaCha20-Poly1305 (RFC 8439) and OpenSSH's `chacha20-poly1305@openssh.com`.
//! ChaCha20 is additions, XORs and rotations and Poly1305 adds 64-by-64 products,
//! so the time depends only on the lengths, given that those products are constant
//! time. Poly1305 is reachable only through these two AEADs. RFC 8439's pads every
//! block whole; OpenSSH's MACs its packet bare, whose short last block takes
//! Poly1305's own ending. A block takes four full products and two by the small top
//! limb of h, which stays below 2p between blocks. Its limbs, reduction and finish
//! are modified from the 64-bit path of OpenSSL's `crypto/poly1305/poly1305.c`,
//! Apache-2.0 (THIRD-PARTY.md).

use crate::{ct, Refused};

pub const KEY_LEN: usize = 32;
pub const NONCE_LEN: usize = 12;
pub const TAG_LEN: usize = 16;
pub const OPENSSH_KEY_LEN: usize = 64;

/// RFC 8439 2.8: a 32-bit block counter starting at 1 covers this much.
const MAX_LEN: u64 = ((1 << 32) - 1) * 64;

/// The bytes before a packet's payload in OpenSSH's construction.
const LENGTH_LEN: usize = 4;

pub(crate) const SIGMA: [u32; 4] = [0x61707865, 0x3320646e, 0x79622d32, 0x6b206574];

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

/// `chacha20-poly1305@openssh.com`, OpenSSH's PROTOCOL.chacha20poly1305. Its state is
/// a 64-bit counter then a 64-bit nonce holding the sequence number big-endian, which
/// is RFC 8439's layout under a nonce of four zero bytes then that number, as long as
/// the counter stays below 2^32: past that the two differ, so `seal` and `open` refuse.
/// A sequence number must never repeat under one key.
pub struct OpenSshAead {
    main: [u32; 8],
    length: [u32; 8],
}

impl OpenSshAead {
    /// `key[..32]` is K_2, for the payload and the Poly1305 key; `key[32..]` is K_1,
    /// for the length.
    pub fn new(key: &[u8; OPENSSH_KEY_LEN]) -> Self {
        let (main, length) = key.split_at(KEY_LEN);
        Self {
            main: key_words(main),
            length: key_words(length),
        }
    }

    /// The packet length, which the reader needs before it can have the whole packet
    /// to authenticate. So this one value is released unauthenticated: the protocol
    /// defines it so, and the caller must bound it before believing it.
    pub fn decrypt_length(&self, seq: u32, encrypted: &[u8; LENGTH_LEN]) -> [u8; LENGTH_LEN] {
        let mut pad = block(&self.length, 0, &openssh_nonce(seq));
        let mut out = *encrypted;
        for (byte, mask) in out.iter_mut().zip(&pad) {
            *byte ^= *mask;
        }
        ct::zero(&mut pad);
        out
    }

    /// `packet` is the 4 length bytes then the payload: encrypt it where it lies and
    /// return the tag over all of it. Refused, and left alone, when too short to hold
    /// a length or too long for the counter.
    pub fn seal(&self, seq: u32, packet: &mut [u8]) -> Result<[u8; TAG_LEN], Refused> {
        let cipher = self.cipher(seq, packet.len())?;
        let (length, payload) = packet
            .split_first_chunk_mut::<LENGTH_LEN>()
            .ok_or(Refused)?;
        *length = self.decrypt_length(seq, length);
        cipher.crypt(payload);
        Ok(cipher.bare_tag(packet))
    }

    /// Check the tag over the encrypted length and payload, then decrypt the payload
    /// where it lies. The 4 length bytes stay as they arrived, `decrypt_length` having
    /// read them. A refused packet is left untouched: nothing of the payload is
    /// decrypted before the tag holds.
    pub fn open(&self, seq: u32, packet: &mut [u8], tag: &[u8; TAG_LEN]) -> Result<(), Refused> {
        let cipher = self.cipher(seq, packet.len())?;
        if !ct::eq(&cipher.bare_tag(packet), tag) {
            return Err(Refused);
        }
        let (_, payload) = packet
            .split_first_chunk_mut::<LENGTH_LEN>()
            .ok_or(Refused)?;
        cipher.crypt(payload);
        Ok(())
    }

    fn cipher(&self, seq: u32, packet_len: usize) -> Result<Cipher, Refused> {
        check_len(packet_len.checked_sub(LENGTH_LEN).ok_or(Refused)?)?;
        Ok(Cipher {
            key: self.main,
            nonce: openssh_nonce(seq),
        })
    }
}

impl Drop for OpenSshAead {
    fn drop(&mut self) {
        self.main.fill(0);
        self.length.fill(0);
        std::hint::black_box((&self.main, &self.length));
    }
}

/// The sequence number's big-endian bytes, read as ChaCha20's little-endian word.
const fn openssh_nonce(seq: u32) -> [u32; 3] {
    [0, 0, seq.swap_bytes()]
}

fn key_words(key: &[u8]) -> [u32; 8] {
    let mut words = [0u32; 8];
    for (word, bytes) in words.iter_mut().zip(key.as_chunks::<4>().0) {
        *word = u32::from_le_bytes(*bytes);
    }
    words
}

fn check_len(len: usize) -> Result<(), Refused> {
    if u64::try_from(len).map_err(|_| Refused)? > MAX_LEN {
        return Err(Refused);
    }
    Ok(())
}

struct Cipher {
    key: [u32; 8],
    nonce: [u32; 3],
}

impl Cipher {
    fn new(key: &[u8; KEY_LEN], nonce: &[u8; NONCE_LEN], len: usize) -> Result<Self, Refused> {
        check_len(len)?;
        let words = key_words(key);
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
        #[cfg(target_arch = "x86_64")]
        if let Some(cpu) = crate::accel::Avx2::detect() {
            cpu.chacha20(&self.key, &self.nonce, 1, data);
            return;
        }
        self.crypt_software(1, data);
    }

    fn crypt_software(&self, first: u32, data: &mut [u8]) {
        let mut counter = first;
        for chunk in data.chunks_mut(64) {
            let stream = block(&self.key, counter, &self.nonce);
            for (byte, pad) in chunk.iter_mut().zip(stream) {
                *byte ^= pad;
            }
            counter = counter.wrapping_add(1);
        }
    }

    fn mac(&self) -> Poly1305 {
        let mut first = block(&self.key, 0, &self.nonce);
        let mac = Poly1305::new(&first);
        ct::zero(&mut first);
        mac
    }

    /// OpenSSH's tag: Poly1305 over the message itself, with nothing around it.
    fn bare_tag(&self, message: &[u8]) -> [u8; TAG_LEN] {
        let mut mac = self.mac();
        mac.bare(message);
        mac.finish()
    }

    fn tag(&self, aad: &[u8], ciphertext: &[u8]) -> [u8; TAG_LEN] {
        let mut mac = self.mac();
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

/// Poly1305 in 64/64/3-bit limbs over 128-bit products.
struct Poly1305 {
    r: [u64; 2],
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
        Self {
            r: [t0 & 0x0ffffffc0fffffff, t1 & 0x0ffffffc0ffffffc],
            h: [0; 3],
            pad: [p0, p1],
        }
    }

    /// One whole 16-byte block with its 2^128 bit set.
    fn block(&mut self, block: &[u8; 16]) {
        self.absorb(block, 1);
    }

    /// h = (h + block + high * 2^128) * r, reduced partly. r1 has its low two bits
    /// clear, so r1 * 2^128 = r1 * 5 / 4 mod p.
    fn absorb(&mut self, block: &[u8; 16], high: u64) {
        let (t0, t1) = two_words(block);
        let [r0, r1] = self.r;
        let s1 = r1 + (r1 >> 2);
        let [h0, h1, h2] = self.h;

        let (h0, carry) = h0.overflowing_add(t0);
        let (h1, carry_a) = h1.overflowing_add(t1);
        let (h1, carry_b) = h1.overflowing_add(u64::from(carry));
        let h2 = h2 + u64::from(carry_a) + u64::from(carry_b) + high;

        let wide = |a: u64, b: u64| u128::from(a) * u128::from(b);
        let d0 = wide(h0, r0) + wide(h1, s1);
        // r is clamped below 2^60 in each limb, so these sums fit in 128 bits.
        let d1 = wide(h0, r1) + wide(h1, r0) + wide(h2, s1) + (d0 >> 64);
        let top = h2 * r0 + ((d1 >> 64) as u64);

        // Everything at 2^130 and up comes back multiplied by 5.
        let back = (top >> 2) + (top & !3);
        let (h0, carry) = (d0 as u64).overflowing_add(back);
        let (h1, carry) = (d1 as u64).overflowing_add(u64::from(carry));
        self.h = [h0, h1, (top & 3) + u64::from(carry)];
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

    /// `data` as Poly1305 itself reads it (RFC 8439 2.5): a short last block ends in
    /// a 1 byte, which takes the place of its 2^128 bit.
    fn bare(&mut self, data: &[u8]) {
        let (blocks, rest) = data.as_chunks::<16>();
        for block in blocks {
            self.block(block);
        }
        if !rest.is_empty() {
            let mut last = [0u8; 16];
            for (out, byte) in last.iter_mut().zip(rest.iter().chain(&[1])) {
                *out = *byte;
            }
            self.absorb(&last, 0);
        }
    }

    fn finish(self) -> [u8; 16] {
        let [h0, h1, h2] = self.h;
        // h - p, by way of h + 5, kept when that reaches 2^130: a partly reduced h
        // is below 2p, so one subtraction is enough.
        let (g0, carry) = h0.overflowing_add(5);
        let (g1, carry) = h1.overflowing_add(u64::from(carry));
        let g2 = h2 + u64::from(carry);
        let keep_g = std::hint::black_box(0u64.wrapping_sub(g2 >> 2));
        let h0 = (h0 & !keep_g) | (g0 & keep_g);
        let h1 = (h1 & !keep_g) | (g1 & keep_g);

        let [p0, p1] = self.pad;
        let (low, carry) = h0.overflowing_add(p0);
        let high = h1.wrapping_add(p1).wrapping_add(u64::from(carry));
        let mut tag = [0u8; 16];
        for (bytes, word) in tag.as_chunks_mut::<8>().0.iter_mut().zip([low, high]) {
            *bytes = word.to_le_bytes();
        }
        tag
    }
}

impl Drop for Poly1305 {
    fn drop(&mut self) {
        self.r.fill(0);
        self.h.fill(0);
        self.pad.fill(0);
        std::hint::black_box((&self.r, &self.h, &self.pad));
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::indexing_slicing, clippy::unwrap_used, clippy::panic)]
    use super::{
        block, check_len, key_words, open, openssh_nonce, seal, Cipher, Poly1305, MAX_LEN,
    };
    use crate::sha::{Digest, Sha256};
    use crate::test_corpus::{expand, hex as field, long_rows, rows, EDGE_LENGTHS};

    /// One way of running the keystream from a given block.
    type Backend = fn(&Cipher, u32, &mut [u8]);

    fn software(cipher: &Cipher, first: u32, data: &mut [u8]) {
        cipher.crypt_software(first, data);
    }

    #[cfg(target_arch = "x86_64")]
    fn hardware() -> Option<Backend> {
        crate::accel::Avx2::detect()?;
        Some(|cipher, first, data| {
            if let Some(cpu) = crate::accel::Avx2::detect() {
                cpu.chacha20(&cipher.key, &cipher.nonce, first, data);
            }
        })
    }

    #[cfg(not(target_arch = "x86_64"))]
    fn hardware() -> Option<Backend> {
        None
    }

    /// Each backend this machine has, which is always the software one.
    fn backends() -> Vec<Backend> {
        let mut all: Vec<Backend> = vec![software];
        all.extend(hardware());
        all
    }

    fn cipher(key: &[u8], nonce: &[u8; 12]) -> Cipher {
        Cipher::new(&key.try_into().unwrap(), nonce, 0).unwrap()
    }

    #[test]
    fn every_backend_answers_wycheproof() {
        let rows = rows(include_str!("../tests/wycheproof/chacha20_poly1305.txt"));
        assert_eq!(rows.len(), 316);
        for row in &rows {
            let cipher = cipher(&row.key, &row.nonce);
            assert_eq!(cipher.tag(&row.aad, &row.sealed) == row.tag, row.valid);
            if row.valid {
                for backend in backends() {
                    let mut data = row.sealed.clone();
                    backend(&cipher, 1, &mut data);
                    assert_eq!(data, row.plain);
                }
            }
        }
    }

    #[test]
    fn every_backend_answers_the_long_records() {
        let rows = long_rows("chacha");
        assert_eq!(rows.len(), 4);
        for row in &rows {
            let cipher = cipher(&row.key, &row.nonce);
            for backend in backends() {
                let mut data = row.plain.clone();
                backend(&cipher, 1, &mut data);
                assert_eq!(Sha256::digest(&data).to_vec(), row.digest);
                assert_eq!(cipher.tag(&row.aad, &data), row.tag);
            }
        }
    }

    // Every length around a batch, and a counter that wraps inside one, against
    // one RFC 8439 block at a time.
    #[test]
    fn every_backend_matches_the_block_function() {
        let cipher = cipher(&expand(3, 32), &expand(4, 12).try_into().unwrap());
        for backend in backends() {
            for first in [1, 2, u32::MAX - 20, u32::MAX - 3, u32::MAX, 0] {
                for &len in EDGE_LENGTHS.iter().filter(|&&n| n <= 1025) {
                    let plain = expand(u64::from(first) + len as u64, len);
                    let mut data = plain.clone();
                    backend(&cipher, first, &mut data);
                    for (at, (chunk, plain)) in data.chunks(64).zip(plain.chunks(64)).enumerate() {
                        let counter = first.wrapping_add(at as u32);
                        let stream = block(&cipher.key, counter, &cipher.nonce);
                        for ((byte, input), pad) in chunk.iter().zip(plain).zip(stream) {
                            assert_eq!(*byte, input ^ pad, "{first} {len} {at}");
                        }
                    }
                }
            }
        }
    }

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
        // h is 5 after the zero block. The next, 2^128 - 5, carries out of the low
        // limb into a high limb already full, so that carry alone reaches the top:
        // 2^129 * 4 = 2^131 = 10 mod p. No RFC vector or corpus row lands there.
        assert_eq!(
            one_time(filled(4, 0), zero, &[zero, filled(0xfb, 0xff)]),
            filled(10, 0)
        );
    }

    #[test]
    fn the_length_bound_stops_the_counter_short_of_block_zero() {
        assert_eq!(MAX_LEN, ((1u64 << 32) - 1) * 64);
        assert!(Cipher::new(&[0; 32], &[0; 12], MAX_LEN as usize).is_ok());
        assert!(Cipher::new(&[0; 32], &[0; 12], MAX_LEN as usize + 1).is_err());
        // A packet's payload has the same bound, past its 4 length bytes.
        let aead = super::OpenSshAead::new(&[0; 64]);
        assert!(aead.cipher(0, MAX_LEN as usize + 4).is_ok());
        assert!(aead.cipher(0, MAX_LEN as usize + 5).is_err());
        assert!(aead.cipher(0, 4).is_ok());
        assert!(aead.cipher(0, 3).is_err());
        assert!(check_len(MAX_LEN as usize + 1).is_err());
    }

    // OpenSSL's Poly1305 at every length around a block, short last blocks included,
    // and RFC 8439's own vectors: `bare` is reachable from outside only through a
    // packet, so this is where it is held to Poly1305 by itself.
    #[test]
    fn bare_poly1305_agrees_with_openssl() {
        let mut count = 0;
        for line in include_str!("../tests/ssh/poly1305.txt")
            .lines()
            .filter(|line| !line.starts_with('#'))
        {
            let fields: Vec<&str> = line.split(' ').collect();
            let [key, message, tag] = fields[..] else {
                panic!("malformed row: {line}");
            };
            let mut block = [0u8; 64];
            block[..32].copy_from_slice(&field(key));
            let mut mac = Poly1305::new(&block);
            mac.bare(&field(message));
            assert_eq!(mac.finish().to_vec(), field(tag), "{line}");
            count += 1;
        }
        assert_eq!(count, 79);
    }

    // The framing on each backend: `tests/ssh_vectors.rs` reaches only the CPU's.
    #[test]
    fn every_backend_answers_the_openssh_packets() {
        let mut count = 0;
        for line in include_str!("../tests/ssh/chachapoly.txt")
            .lines()
            .filter(|line| !line.starts_with('#'))
        {
            let fields: Vec<&str> = line.split(' ').collect();
            let [key, seq, length, sealed, tag] = fields[..] else {
                panic!("malformed row: {line}");
            };
            let key = field(key);
            let length: usize = length.parse().unwrap();
            let sealed = field(sealed);
            let cipher = Cipher {
                key: key_words(&key[..32]),
                nonce: openssh_nonce(seq.parse().unwrap()),
            };
            assert_eq!(cipher.bare_tag(&sealed).to_vec(), field(tag));
            let plain: Vec<u8> = (0..length)
                .map(|i| ((i * 17 + length) & 0xff) as u8)
                .collect();
            for backend in backends() {
                let mut data = plain.clone();
                backend(&cipher, 1, &mut data);
                assert_eq!(data, sealed[4..]);
            }
            count += 1;
        }
        assert_eq!(count, 52);
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

//! SHA-1 (FIPS 180-4), HMAC-SHA1 (RFC 2104) and PBKDF2-HMAC-SHA1 (RFC 8018),
//! which WPA2-PSK fixes: the passphrase-to-PMK mapping, the PTK's PRF and the
//! EAPOL-Key MIC (IEEE 802.11-2020 J.4, 12.7.1.2, 12.7.2). SHA-1's collision
//! weakness does not reach HMAC or PBKDF2 over it; td uses it only here.
//!
//! Key-derived buffers this module owns are zeroed before they are dropped,
//! best effort in safe Rust.

/// A streaming SHA-1.
#[derive(Clone)]
pub struct Sha1 {
    state: [u32; 5],
    block: [u8; 64],
    filled: usize,
    length: u64,
}

impl Default for Sha1 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha1 {
    pub fn new() -> Self {
        Self {
            state: [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476, 0xc3d2e1f0],
            block: [0; 64],
            filled: 0,
            length: 0,
        }
    }

    pub fn update(&mut self, mut bytes: &[u8]) {
        self.length = self.length.wrapping_add(bytes.len() as u64);
        if self.filled > 0 {
            let take = (64 - self.filled).min(bytes.len());
            let (head, rest) = bytes.split_at(take);
            if let Some(slot) = self.block.get_mut(self.filled..self.filled + take) {
                slot.copy_from_slice(head);
            }
            self.filled += take;
            bytes = rest;
            if self.filled < 64 {
                return;
            }
            let block = self.block;
            compress(&mut self.state, &block);
            self.filled = 0;
        }
        let (blocks, rest) = bytes.as_chunks::<64>();
        for block in blocks {
            compress(&mut self.state, block);
        }
        if let Some(slot) = self.block.get_mut(..rest.len()) {
            slot.copy_from_slice(rest);
        }
        self.filled = rest.len();
    }

    pub fn finalize(mut self) -> [u8; 20] {
        let bits = self.length.wrapping_mul(8);
        self.update(&[0x80]);
        let zeros = [0u8; 64];
        let pad = (64 + 56 - self.filled) % 64;
        self.update(zeros.get(..pad).unwrap_or(&[]));
        self.update(&bits.to_be_bytes());
        let mut out = [0u8; 20];
        for (bytes, word) in out.as_chunks_mut::<4>().0.iter_mut().zip(self.state) {
            *bytes = word.to_be_bytes();
        }
        out
    }
}

impl Drop for Sha1 {
    fn drop(&mut self) {
        self.state.fill(0);
        self.block.fill(0);
        std::hint::black_box(&mut self.state);
        std::hint::black_box(&mut self.block);
    }
}

fn compress(state: &mut [u32; 5], block: &[u8; 64]) {
    let mut w = [0u32; 80];
    for (word, bytes) in w.iter_mut().zip(block.as_chunks::<4>().0) {
        *word = u32::from_be_bytes(*bytes);
    }
    for t in 16..80 {
        let at = |i: usize| w.get(i).copied().unwrap_or(0);
        let value = (at(t - 3) ^ at(t - 8) ^ at(t - 14) ^ at(t - 16)).rotate_left(1);
        if let Some(slot) = w.get_mut(t) {
            *slot = value;
        }
    }
    let [mut a, mut b, mut c, mut d, mut e] = *state;
    for (t, word) in w.iter().enumerate() {
        let (f, k) = match t {
            0..=19 => ((b & c) | (!b & d), 0x5a827999),
            20..=39 => (b ^ c ^ d, 0x6ed9eba1),
            40..=59 => ((b & c) | (b & d) | (c & d), 0x8f1bbcdc),
            _ => (b ^ c ^ d, 0xca62c1d6),
        };
        let temp = a
            .rotate_left(5)
            .wrapping_add(f)
            .wrapping_add(e)
            .wrapping_add(k)
            .wrapping_add(*word);
        e = d;
        d = c;
        c = b.rotate_left(30);
        b = a;
        a = temp;
    }
    for (out, value) in state.iter_mut().zip([a, b, c, d, e]) {
        *out = out.wrapping_add(value);
    }
    w.fill(0);
    std::hint::black_box(&mut w);
}

/// SHA-1 of `bytes`.
#[cfg(test)]
pub fn digest(bytes: &[u8]) -> [u8; 20] {
    let mut hash = Sha1::new();
    hash.update(bytes);
    hash.finalize()
}

/// HMAC-SHA1 keyed once: the inner and outer states after their pads, so
/// each MAC costs two compressions plus its message.
pub struct HmacSha1 {
    inner: Sha1,
    outer: Sha1,
}

impl HmacSha1 {
    pub fn new(key: &[u8]) -> Self {
        let mut normalized = [0u8; 64];
        if key.len() > 64 {
            let mut hash = Sha1::new();
            hash.update(key);
            let mut hashed = hash.finalize();
            for (out, byte) in normalized.iter_mut().zip(&hashed) {
                *out = *byte;
            }
            hashed.fill(0);
            std::hint::black_box(&mut hashed);
        } else {
            for (out, byte) in normalized.iter_mut().zip(key) {
                *out = *byte;
            }
        }
        let mut pad = [0u8; 64];
        let mut inner = Sha1::new();
        for (out, byte) in pad.iter_mut().zip(&normalized) {
            *out = byte ^ 0x36;
        }
        inner.update(&pad);
        let mut outer = Sha1::new();
        for (out, byte) in pad.iter_mut().zip(&normalized) {
            *out = byte ^ 0x5c;
        }
        outer.update(&pad);
        normalized.fill(0);
        pad.fill(0);
        std::hint::black_box(&mut normalized);
        std::hint::black_box(&mut pad);
        Self { inner, outer }
    }

    /// The MAC over the concatenation of `parts`.
    pub fn mac(&self, parts: &[&[u8]]) -> [u8; 20] {
        let mut inner = self.inner.clone();
        for part in parts {
            inner.update(part);
        }
        let mut inner_hash = inner.finalize();
        let mut outer = self.outer.clone();
        outer.update(&inner_hash);
        inner_hash.fill(0);
        std::hint::black_box(&mut inner_hash);
        outer.finalize()
    }
}

/// HMAC-SHA1 under `key` over the concatenation of `parts`.
pub fn hmac_sha1(key: &[u8], parts: &[&[u8]]) -> [u8; 20] {
    HmacSha1::new(key).mac(parts)
}

/// PBKDF2-HMAC-SHA1 filling `out`. WPA2 asks for 4096 iterations and 32
/// bytes; anything from one iteration and up to 255 blocks is accepted.
pub fn pbkdf2_hmac_sha1(
    password: &[u8],
    salt: &[u8],
    iterations: u32,
    out: &mut [u8],
) -> Result<(), &'static str> {
    if iterations == 0 || out.len() > 255 * 20 {
        return Err("PBKDF2 needs at least one iteration and at most 255 blocks");
    }
    let prf = HmacSha1::new(password);
    for (index, chunk) in out.chunks_mut(20).enumerate() {
        let counter = (index as u32 + 1).to_be_bytes();
        let mut u = prf.mac(&[salt, &counter]);
        let mut t = u;
        for _ in 1..iterations {
            let next = prf.mac(&[&u]);
            u = next;
            for (acc, byte) in t.iter_mut().zip(&u) {
                *acc ^= byte;
            }
        }
        for (dst, byte) in chunk.iter_mut().zip(&t) {
            *dst = *byte;
        }
        u.fill(0);
        t.fill(0);
        std::hint::black_box(&mut u);
        std::hint::black_box(&mut t);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
            .collect()
    }

    /// FIPS 180's examples and the million-`a` message; expected values from
    /// Python's hashlib. The two-block message is also fed split at every
    /// offset.
    #[test]
    fn sha1_matches_fips_180() {
        let cases: &[(&[u8], &str)] = &[
            (b"", "da39a3ee5e6b4b0d3255bfef95601890afd80709"),
            (b"abc", "a9993e364706816aba3e25717850c26c9cd0d89d"),
            (
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq",
                "84983e441c3bd26ebaae4aa1f95129e5e54670f1",
            ),
        ];
        for (message, expected) in cases {
            assert_eq!(digest(message).to_vec(), hex(expected));
            for at in 0..=message.len() {
                let (head, tail) = message.split_at(at);
                let mut hash = Sha1::new();
                hash.update(head);
                hash.update(tail);
                assert_eq!(hash.finalize().to_vec(), hex(expected), "{at}");
            }
        }
        let mut hash = Sha1::new();
        for _ in 0..1000 {
            hash.update(&[b'a'; 1000]);
        }
        assert_eq!(
            hash.finalize().to_vec(),
            hex("34aa973cd4c4daa4f61eeb2bdbad27316534016f")
        );
    }

    /// The padding boundaries, where the length does and does not fit the
    /// last block, against hashlib; and every length to 130 bytes streamed a
    /// byte at a time against the same message whole.
    #[test]
    fn sha1_padding_boundaries() {
        let message: Vec<u8> = (0..130u8).collect();
        for (len, expected) in [
            (55, "8ae2d46729cfe68ff927af5eec9c7d1b66d65ac2"),
            (56, "636e2ec698dac903498e648bd2f3af641d3c88cb"),
            (63, "6d942da0c4392b123528f2905c713a3ce28364bd"),
            (64, "c6138d514ffa2135bfce0ed0b8fac65669917ec7"),
            (119, "41c89d06001bab4ab78736b44efe7ce18ce6ae08"),
            (120, "d3dbd653bd8597b7475321b60a36891278e6a04a"),
        ] {
            assert_eq!(digest(&message[..len]).to_vec(), hex(expected), "{len}");
        }
        for len in 0..message.len() {
            let mut hash = Sha1::new();
            for byte in &message[..len] {
                hash.update(&[*byte]);
            }
            assert_eq!(hash.finalize(), digest(&message[..len]), "{len}");
        }
    }

    /// RFC 2202's seven HMAC-SHA1 cases, whole and with the message split.
    #[test]
    fn hmac_sha1_matches_rfc_2202() {
        let key_four: Vec<u8> = (1..=25).collect();
        let cases: &[(&[u8], &[u8], &str)] = &[
            (
                &[0x0b; 20],
                b"Hi There",
                "b617318655057264e28bc0b6fb378c8ef146be00",
            ),
            (
                b"Jefe",
                b"what do ya want for nothing?",
                "effcdf6ae5eb2fa2d27416d5f184df9c259a7c79",
            ),
            (
                &[0xaa; 20],
                &[0xdd; 50],
                "125d7342b9ac11cd91a39af48aa17b4f63f175d3",
            ),
            (
                &key_four,
                &[0xcd; 50],
                "4c9007f4026250c6bc8414f9bf50c86c2d7235da",
            ),
            (
                &[0x0c; 20],
                b"Test With Truncation",
                "4c1a03424b55e07fe7f27be1d58bb9324a9a5a04",
            ),
            (
                &[0xaa; 80],
                b"Test Using Larger Than Block-Size Key - Hash Key First",
                "aa4ae5e15272d00e95705637ce8a3b55ed402112",
            ),
            (
                &[0xaa; 80],
                b"Test Using Larger Than Block-Size Key and Larger Than One Block-Size Data",
                "e8e99d0f45237d786d6bbaa7965c7808bbff1a91",
            ),
        ];
        for (key, data, expected) in cases {
            assert_eq!(hmac_sha1(key, &[data]).to_vec(), hex(expected));
            for at in 0..=data.len() {
                let (head, tail) = data.split_at(at);
                assert_eq!(hmac_sha1(key, &[head, tail]).to_vec(), hex(expected));
            }
        }
    }

    /// RFC 6070's PBKDF2-HMAC-SHA1 vectors but the 16777216-iteration one.
    #[test]
    fn pbkdf2_matches_rfc_6070() {
        let cases: &[(&[u8], &[u8], u32, &str)] = &[
            (
                b"password",
                b"salt",
                1,
                "0c60c80f961f0e71f3a9b524af6012062fe037a6",
            ),
            (
                b"password",
                b"salt",
                2,
                "ea6c014dc72d6f8ccd1ed92ace1d41f0d8de8957",
            ),
            (
                b"password",
                b"salt",
                4096,
                "4b007901b765489abead49d926f721d065a429c1",
            ),
            (
                b"passwordPASSWORDpassword",
                b"saltSALTsaltSALTsaltSALTsaltSALTsalt",
                4096,
                "3d2eec4fe41c849b80c8d83662c0e44a8b291a964cf2f07038",
            ),
            (
                b"pass\0word",
                b"sa\0lt",
                4096,
                "56fa6aa75548099dcc37d7f03425e0c3",
            ),
        ];
        for (password, salt, iterations, expected) in cases {
            let expected = hex(expected);
            let mut out = vec![0; expected.len()];
            pbkdf2_hmac_sha1(password, salt, *iterations, &mut out).unwrap();
            assert_eq!(out, expected);
        }
        let mut out = [0u8; 20];
        assert!(pbkdf2_hmac_sha1(b"p", b"s", 0, &mut out).is_err());
        let mut long = vec![0u8; 255 * 20 + 1];
        assert!(pbkdf2_hmac_sha1(b"p", b"s", 1, &mut long).is_err());
    }

    /// IEEE 802.11-2020 J.4.2's passphrase-to-PSK vectors (and the 63-byte
    /// passphrase over a 32-byte SSID, the extremes WPA2 allows).
    #[test]
    fn wpa2_passphrase_to_psk() {
        let cases: &[(&[u8], &[u8], &str)] = &[
            (
                b"password",
                b"IEEE",
                "f42c6fc52df0ebef9ebb4b90b38a5f902e83fe1b135a70e23aed762e9710a12e",
            ),
            (
                b"ThisIsAPassword",
                b"ThisIsASSID",
                "0dc0d6eb90555ed6419756b9a15ec3e3209b63df707dd508d14581f8982721af",
            ),
            (
                &[b'a'; 63],
                &[b'Z'; 32],
                "2d43d0dabfdd635377172efa1fc4b4b87dbfc4219193909ded9a7cfb89a3097b",
            ),
        ];
        for (passphrase, ssid, expected) in cases {
            let mut psk = [0u8; 32];
            pbkdf2_hmac_sha1(passphrase, ssid, 4096, &mut psk).unwrap();
            assert_eq!(psk.to_vec(), hex(expected));
        }
    }
}

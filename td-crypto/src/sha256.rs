//! Fixed-state SHA-256 (FIPS 180-4), without allocation or provider calls.
use crate::{Digest, Error};

const MAX_INPUT_BYTES: u64 = u64::MAX / 8;

/// An inline digest operation. Any update error permanently retires it.
/// Construction, updates and consuming finish allocate no memory.
pub struct Sha256 {
    h: [u32; 8],
    block: [u8; 64],
    bytes: u64,
    used: usize,
    active: bool,
}

impl std::fmt::Debug for Sha256 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Sha256(<redacted>)")
    }
}

impl Sha256 {
    pub fn try_new() -> Result<Self, Error> {
        Ok(Self {
            h: H0,
            block: [0; 64],
            bytes: 0,
            used: 0,
            active: true,
        })
    }

    fn absorb(&mut self, mut input: &[u8]) -> Result<(), Error> {
        if self.used >= 64 {
            return Err(Error::Crypto);
        }
        if self.used != 0 {
            let available = 64usize.checked_sub(self.used).ok_or(Error::Crypto)?;
            let take = available.min(input.len());
            let end = self.used.checked_add(take).ok_or(Error::Crypto)?;
            let head = input.get(..take).ok_or(Error::Crypto)?;
            self.block
                .get_mut(self.used..end)
                .ok_or(Error::Crypto)?
                .copy_from_slice(head);
            input = input.get(take..).ok_or(Error::Crypto)?;
            self.used = end;
            if self.used < 64 {
                return Ok(());
            }
            Self::compress(&mut self.h, &self.block)?;
            self.used = 0;
        }
        let (blocks, tail) = input.as_chunks::<64>();
        for block in blocks {
            Self::compress(&mut self.h, block)?;
        }
        self.block.fill(0);
        self.block
            .get_mut(..tail.len())
            .ok_or(Error::Crypto)?
            .copy_from_slice(tail);
        self.used = tail.len();
        Ok(())
    }

    // The schedule and all accesses depend only on the fixed round number.
    #[inline(never)]
    fn compress(state: &mut [u32; 8], block: &[u8; 64]) -> Result<(), Error> {
        let mut words = [0u32; 64];
        let (input, _) = block.as_chunks::<4>();
        for (dst, src) in words.iter_mut().zip(input) {
            *dst = u32::from_be_bytes(*src);
        }
        for i in 16usize..64 {
            let tap = |back| {
                i.checked_sub(back)
                    .and_then(|index| words.get(index))
                    .copied()
                    .ok_or(Error::Crypto)
            };
            let w16 = tap(16)?;
            let w15 = tap(15)?;
            let w7 = tap(7)?;
            let w2 = tap(2)?;
            let s0 = w15.rotate_right(7) ^ w15.rotate_right(18) ^ (w15 >> 3);
            let s1 = w2.rotate_right(17) ^ w2.rotate_right(19) ^ (w2 >> 10);
            *words.get_mut(i).ok_or(Error::Crypto)? =
                w16.wrapping_add(s0).wrapping_add(w7).wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
        for (&ki, &wi) in K.iter().zip(&words) {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(ki)
                .wrapping_add(wi);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (dst, add) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *dst = dst.wrapping_add(add);
        }
        words.fill(0);
        std::hint::black_box(&mut words);
        Ok(())
    }

    fn complete(&mut self) -> Result<[u8; 32], Error> {
        if !self.active || self.used >= 64 || self.bytes > MAX_INPUT_BYTES {
            return Err(Error::Crypto);
        }
        *self.block.get_mut(self.used).ok_or(Error::Crypto)? = 0x80;
        let end = self.used.checked_add(1).ok_or(Error::Crypto)?;
        self.block.get_mut(end..).ok_or(Error::Crypto)?.fill(0);
        if end > 56 {
            Self::compress(&mut self.h, &self.block)?;
            self.block.fill(0);
        }
        self.block
            .get_mut(56..64)
            .ok_or(Error::Crypto)?
            .copy_from_slice(&(self.bytes * 8).to_be_bytes());
        Self::compress(&mut self.h, &self.block)?;
        let mut output = [0u8; 32];
        let (slots, _) = output.as_chunks_mut::<4>();
        for (dst, word) in slots.iter_mut().zip(self.h) {
            *dst = word.to_be_bytes();
        }
        Ok(output)
    }

    fn clear(&mut self) {
        self.h.fill(0);
        self.block.fill(0);
        self.bytes = 0;
        self.used = 0;
        self.active = false;
        // Hygiene for retained storage, not guaranteed erasure of moved copies.
        std::hint::black_box(self);
    }
}

impl Digest for Sha256 {
    fn update(&mut self, bytes: &[u8]) -> Result<(), Error> {
        if !self.active {
            return Err(Error::Crypto);
        }
        self.active = false;
        let result = (|| {
            let total = u64::try_from(bytes.len())
                .ok()
                .and_then(|count| self.bytes.checked_add(count))
                .filter(|&count| count <= MAX_INPUT_BYTES)
                .ok_or(Error::Crypto)?;
            self.absorb(bytes)?;
            self.bytes = total;
            Ok(())
        })();
        if result.is_ok() {
            self.active = true;
        } else {
            self.clear();
        }
        result
    }

    fn finish(mut self) -> Result<[u8; 32], Error> {
        self.complete()
    }
}

impl Drop for Sha256 {
    fn drop(&mut self) {
        self.clear();
    }
}

/// SHA-256 round constants (fractional parts of cube roots of primes 2..311).
const K: [u32; 64] = [
    0x428a_2f98,
    0x7137_4491,
    0xb5c0_fbcf,
    0xe9b5_dba5,
    0x3956_c25b,
    0x59f1_11f1,
    0x923f_82a4,
    0xab1c_5ed5,
    0xd807_aa98,
    0x1283_5b01,
    0x2431_85be,
    0x550c_7dc3,
    0x72be_5d74,
    0x80de_b1fe,
    0x9bdc_06a7,
    0xc19b_f174,
    0xe49b_69c1,
    0xefbe_4786,
    0x0fc1_9dc6,
    0x240c_a1cc,
    0x2de9_2c6f,
    0x4a74_84aa,
    0x5cb0_a9dc,
    0x76f9_88da,
    0x983e_5152,
    0xa831_c66d,
    0xb003_27c8,
    0xbf59_7fc7,
    0xc6e0_0bf3,
    0xd5a7_9147,
    0x06ca_6351,
    0x1429_2967,
    0x27b7_0a85,
    0x2e1b_2138,
    0x4d2c_6dfc,
    0x5338_0d13,
    0x650a_7354,
    0x766a_0abb,
    0x81c2_c92e,
    0x9272_2c85,
    0xa2bf_e8a1,
    0xa81a_664b,
    0xc24b_8b70,
    0xc76c_51a3,
    0xd192_e819,
    0xd699_0624,
    0xf40e_3585,
    0x106a_a070,
    0x19a4_c116,
    0x1e37_6c08,
    0x2748_774c,
    0x34b0_bcb5,
    0x391c_0cb3,
    0x4ed8_aa4a,
    0x5b9c_ca4f,
    0x682e_6ff3,
    0x748f_82ee,
    0x78a5_636f,
    0x84c8_7814,
    0x8cc7_0208,
    0x90be_fffa,
    0xa450_6ceb,
    0xbef9_a3f7,
    0xc671_78f2,
];

/// Initial hash values (fractional parts of square roots of primes 2..19).
const H0: [u32; 8] = [
    0x6a09_e667,
    0xbb67_ae85,
    0x3c6e_f372,
    0xa54f_f53a,
    0x510e_527f,
    0x9b05_688c,
    0x1f83_d9ab,
    0x5be0_cd19,
];

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    // Existing engine/src/sha256.rs FIPS 180-4/CAVP fixture literals.
    #[test]
    fn known_answers_and_fragmented_updates() {
        for (input, expected) in [
            (
                b"".as_slice(),
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            ),
            (
                b"abc".as_slice(),
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            ),
            (
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq".as_slice(),
                "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1",
            ),
        ] {
            for chunk in [1, 2, 3, 7, 31, 63, 64, 65] {
                let mut digest = Sha256::try_new().unwrap();
                digest.update(b"").unwrap();
                for bytes in input.chunks(chunk) {
                    digest.update(bytes).unwrap();
                }
                assert_eq!(hex(&digest.finish().unwrap()), expected);
            }
        }
        for chunk in [63, 64, 65, 1000] {
            let block = vec![b'a'; chunk];
            let mut remaining = 1_000_000;
            let mut digest = Sha256::try_new().unwrap();
            while remaining != 0 {
                let count = remaining.min(chunk);
                digest.update(block.get(..count).unwrap()).unwrap();
                remaining -= count;
            }
            assert_eq!(
                hex(&digest.finish().unwrap()),
                "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
            );
        }
    }

    fn native(input: &[u8]) -> [u8; 32] {
        aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, input)
            .as_ref()
            .try_into()
            .unwrap()
    }

    #[test]
    fn differential_padding_blocks_and_every_split() {
        let mut data = [0u8; 4097];
        let mut x = 0x1234_5678u32;
        for byte in &mut data {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            *byte = x as u8;
        }
        for size in (0..=257).chain([511, 512, 513, 1023, 1024, 1025, 4095, 4096, 4097]) {
            let input = data.get(..size).unwrap();
            let expected = native(input);
            for chunk in [1, 2, 7, 31, 55, 56, 63, 64, 65, 127, 128, 129, 4097] {
                let mut digest = Sha256::try_new().unwrap();
                for part in input.chunks(chunk) {
                    digest.update(part).unwrap();
                    digest.update(&[]).unwrap();
                }
                assert_eq!(
                    digest.finish().unwrap(),
                    expected,
                    "size={size} chunk={chunk}"
                );
            }
            if size <= 257 {
                for split in 0..=size {
                    let mut digest = Sha256::try_new().unwrap();
                    digest.update(input.get(..split).unwrap()).unwrap();
                    digest.update(input.get(split..).unwrap()).unwrap();
                    assert_eq!(
                        digest.finish().unwrap(),
                        expected,
                        "size={size} split={split}"
                    );
                }
            }
        }
    }

    #[test]
    fn refusal_retires_and_clears_state() {
        // Synthetic lengths isolate each upper padding byte without huge inputs.
        let synthetic = |count| {
            let mut digest = Sha256::try_new().unwrap();
            digest.update(b"x").unwrap();
            digest.bytes = count;
            digest.finish().unwrap()
        };
        let short = synthetic(1);
        for bit in [29, 37, 45, 53, 60] {
            assert_ne!(synthetic(1 + (1u64 << bit)), short, "length bit {bit}");
        }
        let mut digest = Sha256::try_new().unwrap();
        digest.bytes = MAX_INPUT_BYTES - 1;
        digest.update(b"x").unwrap();
        assert_eq!(digest.bytes, MAX_INPUT_BYTES);
        digest.update(&[]).unwrap();
        assert!(digest.finish().is_ok());
        for count in [MAX_INPUT_BYTES, u64::MAX] {
            let mut digest = Sha256::try_new().unwrap();
            digest.update(b"private partial block").unwrap();
            digest.bytes = count;
            assert_eq!(digest.update(b"x"), Err(Error::Crypto));
            assert_eq!(digest.h, [0; 8]);
            assert_eq!(digest.block, [0; 64]);
            assert_eq!(digest.bytes, 0);
            assert_eq!(digest.used, 0);
            assert_eq!(digest.update(&[]), Err(Error::Crypto));
            assert_eq!(digest.finish(), Err(Error::Crypto));
        }
        for used in [64, 65, usize::MAX] {
            let mut digest = Sha256::try_new().unwrap();
            digest.used = used;
            assert_eq!(digest.update(&[]), Err(Error::Crypto));
            assert_eq!(digest.finish(), Err(Error::Crypto));
            let mut digest = Sha256::try_new().unwrap();
            digest.used = used;
            assert_eq!(digest.finish(), Err(Error::Crypto));
        }
        let mut digest = Sha256::try_new().unwrap();
        digest.bytes = u64::MAX;
        assert_eq!(digest.finish(), Err(Error::Crypto));
    }

    #[test]
    fn inline_storage_and_fixed_debug() {
        assert!(std::mem::size_of::<Sha256>() <= 128);
        let mut digest = Sha256::try_new().unwrap();
        digest.update(b"private message").unwrap();
        assert_eq!(format!("{digest:?}"), "Sha256(<redacted>)");
    }
}

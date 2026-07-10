//! SHA-256 (FIPS 180-4) for the recipe evaluator, pure `std`.
//!
//! The runner's cache keys hash small inputs (pinsum manifests, recipe
//! bodies); it previously piped them through an ambient host `sha256sum`,
//! which was the last host-binary lookup on the check path (re #469 —
//! executable provenance). Hashing in-process deletes that lookup.
//!
//! This is a sibling of `builder/src/sha256.rs`, rewritten to the recipes
//! crate's lint bar (no indexing/slicing, no panics): the compression loops
//! run on iterators and `split_at_mut` views instead of `w[i]`.

/// SHA-256 round constants (fractional parts of cube roots of primes 2..311).
const K: [u32; 64] = [
    0x428a_2f98, 0x7137_4491, 0xb5c0_fbcf, 0xe9b5_dba5, 0x3956_c25b, 0x59f1_11f1, 0x923f_82a4,
    0xab1c_5ed5, 0xd807_aa98, 0x1283_5b01, 0x2431_85be, 0x550c_7dc3, 0x72be_5d74, 0x80de_b1fe,
    0x9bdc_06a7, 0xc19b_f174, 0xe49b_69c1, 0xefbe_4786, 0x0fc1_9dc6, 0x240c_a1cc, 0x2de9_2c6f,
    0x4a74_84aa, 0x5cb0_a9dc, 0x76f9_88da, 0x983e_5152, 0xa831_c66d, 0xb003_27c8, 0xbf59_7fc7,
    0xc6e0_0bf3, 0xd5a7_9147, 0x06ca_6351, 0x1429_2967, 0x27b7_0a85, 0x2e1b_2138, 0x4d2c_6dfc,
    0x5338_0d13, 0x650a_7354, 0x766a_0abb, 0x81c2_c92e, 0x9272_2c85, 0xa2bf_e8a1, 0xa81a_664b,
    0xc24b_8b70, 0xc76c_51a3, 0xd192_e819, 0xd699_0624, 0xf40e_3585, 0x106a_a070, 0x19a4_c116,
    0x1e37_6c08, 0x2748_774c, 0x34b0_bcb5, 0x391c_0cb3, 0x4ed8_aa4a, 0x5b9c_ca4f, 0x682e_6ff3,
    0x748f_82ee, 0x78a5_636f, 0x84c8_7814, 0x8cc7_0208, 0x90be_fffa, 0xa450_6ceb, 0xbef9_a3f7,
    0xc671_78f2,
];

/// Initial hash values (fractional parts of square roots of primes 2..19).
const H0: [u32; 8] = [
    0x6a09_e667, 0xbb67_ae85, 0x3c6e_f372, 0xa54f_f53a, 0x510e_527f, 0x9b05_688c, 0x1f83_d9ab,
    0x5be0_cd19,
];

pub struct Sha256 {
    h: [u32; 8],
    block: [u8; 64],
    block_len: usize,
    total_len: u64,
}

impl Sha256 {
    pub fn new() -> Self {
        Sha256 {
            h: H0,
            block: [0; 64],
            block_len: 0,
            total_len: 0,
        }
    }

    pub fn update(&mut self, data: &[u8]) {
        self.total_len = self.total_len.wrapping_add(data.len() as u64);
        let mut rest = data;
        // Top up a partially filled block first.
        if self.block_len > 0 {
            let take = (64 - self.block_len).min(rest.len());
            let (head, tail) = rest.split_at(take);
            if let Some(dst) = self.block.get_mut(self.block_len..self.block_len + take) {
                dst.copy_from_slice(head);
            }
            self.block_len += take;
            rest = tail;
            if self.block_len == 64 {
                let block = self.block;
                self.compress(&block);
                self.block_len = 0;
            }
        }
        let mut chunks = rest.chunks_exact(64);
        for chunk in &mut chunks {
            let mut block = [0u8; 64];
            block.copy_from_slice(chunk);
            self.compress(&block);
        }
        let rem = chunks.remainder();
        if !rem.is_empty() {
            if let Some(dst) = self.block.get_mut(..rem.len()) {
                dst.copy_from_slice(rem);
            }
            self.block_len = rem.len();
        }
    }

    pub fn finalize(mut self) -> [u8; 32] {
        let bit_len = self.total_len.wrapping_mul(8);
        self.update(&[0x80]);
        while self.block_len != 56 {
            self.update(&[0]);
        }
        // The length words are the final 8 bytes of the last block; write
        // them directly and compress, bypassing update()'s length counter.
        if let Some(dst) = self.block.get_mut(56..64) {
            dst.copy_from_slice(&bit_len.to_be_bytes());
        }
        let block = self.block;
        self.compress(&block);
        let mut out = [0u8; 32];
        for (dst, word) in out.chunks_exact_mut(4).zip(self.h.iter()) {
            dst.copy_from_slice(&word.to_be_bytes());
        }
        out
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 64];
        for (slot, chunk) in w.iter_mut().zip(block.chunks_exact(4)) {
            let mut word = [0u8; 4];
            word.copy_from_slice(chunk);
            *slot = u32::from_be_bytes(word);
        }
        // Message schedule: w[i] from taps at i-2, i-7, i-15, i-16. The
        // split keeps this free of panicking index expressions; the taps are
        // structurally in bounds (i >= 16), so the `if let` always matches.
        for i in 16..64 {
            let (done, todo) = w.split_at_mut(i);
            let tap = |back: usize| done.get(i - back).copied();
            if let (Some(w16), Some(w15), Some(w7), Some(w2), Some(slot)) =
                (tap(16), tap(15), tap(7), tap(2), todo.first_mut())
            {
                let s0 = w15.rotate_right(7) ^ w15.rotate_right(18) ^ (w15 >> 3);
                let s1 = w2.rotate_right(17) ^ w2.rotate_right(19) ^ (w2 >> 10);
                *slot = w16
                    .wrapping_add(s0)
                    .wrapping_add(w7)
                    .wrapping_add(s1);
            }
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.h;
        for (&ki, &wi) in K.iter().zip(w.iter()) {
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

        for (slot, add) in self.h.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(add);
        }
    }
}

/// Lowercase hex of a digest, the `sha256sum` wire format.
pub fn to_base16(digest: &[u8; 32]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(64);
    for byte in digest {
        let _ = write!(s, "{byte:02x}");
    }
    s
}

/// One-shot digest of a byte string, in hex.
pub fn hex_digest(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    to_base16(&hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    // FIPS 180-4 / NIST CAVP test vectors.
    #[test]
    fn empty_input() {
        assert_eq!(
            hex_digest(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn abc() {
        assert_eq!(
            hex_digest(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn two_block_message() {
        assert_eq!(
            hex_digest(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn million_a() {
        let mut hasher = Sha256::new();
        let chunk = [b'a'; 1000];
        for _ in 0..1000 {
            hasher.update(&chunk);
        }
        assert_eq!(
            to_base16(&hasher.finalize()),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn split_updates_match_one_shot() {
        let data: Vec<u8> = (0u16..300).map(|i| (i % 251) as u8).collect();
        let one_shot = hex_digest(&data);
        for split in [1usize, 63, 64, 65, 128, 299] {
            let (a, b) = data.split_at(split);
            let mut hasher = Sha256::new();
            hasher.update(a);
            hasher.update(b);
            assert_eq!(to_base16(&hasher.finalize()), one_shot, "split at {split}");
        }
    }
}

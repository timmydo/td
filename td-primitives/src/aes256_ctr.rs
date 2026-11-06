//! AES-256-CTR with a 128-bit big-endian counter, as OpenSSH's `aes256-ctr` key files
//! use it. It runs on `aes_gcm`'s AES, whichever engine the CPU selects, through that
//! engine's own counter mode, which cannot fail, so neither can this: every byte asked
//! for is XORed with keystream. This is the mode alone; a block API stays absent.

use crate::aes_gcm::AesGcm;
use crate::ct;

pub const KEY_LEN: usize = 32;
pub const IV_LEN: usize = 16;

const BLOCK: usize = 16;

pub struct Aes256Ctr {
    /// The one AES key schedule the crate has; the H it also holds goes unused.
    cipher: AesGcm,
    /// The next block's counter.
    counter: u128,
    /// The block before `counter`, of which the first `used` bytes are spent.
    pad: [u8; BLOCK],
    used: usize,
}

impl Aes256Ctr {
    pub fn new(key: &[u8; KEY_LEN], iv: &[u8; IV_LEN]) -> Self {
        Self {
            cipher: AesGcm::new_256(key),
            counter: u128::from_be_bytes(*iv),
            pad: [0; BLOCK],
            used: BLOCK,
        }
    }

    /// XOR the keystream into `data`, going on from where the last call stopped, at
    /// any byte offset. The counter wraps at 128 bits.
    pub fn apply(&mut self, data: &mut [u8]) {
        let spent = data.len().min(BLOCK - self.used);
        let (head, rest) = data.split_at_mut(spent);
        self.spend(head);
        let (whole, tail) = rest.as_chunks_mut::<BLOCK>();
        self.whole_blocks(whole.as_flattened_mut());
        if !tail.is_empty() {
            let mut pad = [0u8; BLOCK];
            self.whole_blocks(&mut pad);
            self.pad = pad;
            ct::zero(&mut pad);
            self.used = 0;
            self.spend(tail);
        }
    }

    /// `data` is no longer than what is left of the pad.
    fn spend(&mut self, data: &mut [u8]) {
        for (byte, mask) in data.iter_mut().zip(self.pad.iter().skip(self.used)) {
            *byte ^= mask;
        }
        self.used += data.len();
    }

    /// The engine counts in the block's last 32 bits and wraps there, so each stretch
    /// stops where they would, and the 96 bits above them carry between stretches.
    fn whole_blocks(&mut self, mut data: &mut [u8]) {
        while !data.is_empty() {
            let mut nonce = [0u8; 12];
            for (out, byte) in nonce.iter_mut().zip(self.counter.to_be_bytes()) {
                *out = byte;
            }
            let first = self.counter as u32;
            let room = ((1u128 << 32) - u128::from(first)) * BLOCK as u128;
            let take = usize::try_from(room).map_or(data.len(), |room| room.min(data.len()));
            let (run, after) = std::mem::take(&mut data).split_at_mut(take);
            self.cipher.ctr(&nonce, first, run);
            self.counter = self.counter.wrapping_add((take / BLOCK) as u128);
            data = after;
        }
    }
}

impl Drop for Aes256Ctr {
    fn drop(&mut self) {
        ct::zero(&mut self.pad);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::indexing_slicing, clippy::unwrap_used, clippy::panic)]
    use super::{Aes256Ctr, BLOCK};
    use crate::aes_gcm::tests::engines;
    use crate::test_corpus::{expand, hex, EDGE_LENGTHS};

    fn on_each_backend(key: &[u8], iv: &[u8; 16]) -> Vec<Aes256Ctr> {
        engines(key)
            .into_iter()
            .map(|cipher| Aes256Ctr {
                cipher,
                counter: u128::from_be_bytes(*iv),
                pad: [0; BLOCK],
                used: BLOCK,
            })
            .collect()
    }

    #[test]
    fn every_backend_answers_the_corpus() {
        let mut count = 0;
        for line in include_str!("../tests/ssh/aes256ctr.txt")
            .lines()
            .filter(|line| !line.starts_with('#'))
        {
            let fields: Vec<&str> = line.split(' ').collect();
            let [key, iv, stream] = fields[..] else {
                panic!("malformed row: {line}");
            };
            let want = hex(stream);
            for mut ctr in on_each_backend(&hex(key), &hex(iv).try_into().unwrap()) {
                let mut data = vec![0u8; want.len()];
                ctr.apply(&mut data);
                assert_eq!(data, want, "{iv} {}", want.len());
            }
            count += 1;
        }
        assert_eq!(count, 60);
    }

    // AES-GCM's own counter is held to one block at a time elsewhere; this holds the
    // carries it cannot do, a word at a time, against the block a 16-byte call makes,
    // for every length around a batch and a counter crossing each word.
    #[test]
    fn every_backend_carries_at_every_length() {
        let key = expand(9, 32);
        let reference = |iv: u128, at: u128, ctr: &Aes256Ctr| {
            let bytes = iv.wrapping_add(at).to_be_bytes();
            let mut pad = [0u8; 16];
            ctr.cipher.ctr(
                bytes[..12].try_into().unwrap(),
                u32::from_be_bytes(bytes[12..].try_into().unwrap()),
                &mut pad,
            );
            pad
        };
        for iv in [
            0u128,
            0xffff_fffd,
            0xffff_ffff,
            0x1_ffff_fffb,
            u128::from(u64::MAX) - 2,
            (1u128 << 96) - 1,
            u128::MAX - 2,
            u128::MAX,
        ] {
            for &len in EDGE_LENGTHS.iter().filter(|&&len| len <= 1025) {
                for mut ctr in on_each_backend(&key, &iv.to_be_bytes()) {
                    let mut data = vec![0u8; len];
                    ctr.apply(&mut data);
                    for (at, chunk) in data.chunks(16).enumerate() {
                        let pad = reference(iv, at as u128, &ctr);
                        assert_eq!(chunk, &pad[..chunk.len()], "{iv:x} {len} {at}");
                    }
                }
            }
        }
    }

    #[test]
    fn the_keystream_is_never_all_zero_and_continues_across_calls() {
        for mut ctr in on_each_backend(&[3; 32], &[0xff; 16]) {
            let mut whole = vec![0u8; 300];
            ctr.apply(&mut whole);
            assert!(whole.chunks(16).all(|block| block.iter().any(|b| *b != 0)));
        }
        let one_call = {
            let mut ctr = Aes256Ctr::new(&[3; 32], &[0xff; 16]);
            let mut data = vec![0u8; 300];
            ctr.apply(&mut data);
            data
        };
        for step in [1usize, 7, 15, 16, 17, 64, 129] {
            let mut ctr = Aes256Ctr::new(&[3; 32], &[0xff; 16]);
            let mut data = vec![0u8; 300];
            for chunk in data.chunks_mut(step) {
                ctr.apply(chunk);
                ctr.apply(&mut []);
            }
            assert_eq!(data, one_call, "{step}");
        }
    }
}

//! AES and GHASH by the CPU's instructions: the same algorithm and the same bytes
//! as `aes_gcm`'s software path, which a test holds them to. GHASH follows Gueron
//! and Kounavis's carryless-multiplication paper, folding eight blocks over powers
//! of H before one reduction. Key expansion stays the software path's, which needs
//! no table and has no second form to keep right.

use std::arch::x86_64::{
    __m128i, _mm_aesenc_si128, _mm_aesenclast_si128, _mm_clmulepi64_si128, _mm_cvtsi128_si64,
    _mm_extract_epi64, _mm_insert_epi32, _mm_or_si128, _mm_set_epi64x, _mm_set_epi8,
    _mm_setzero_si128, _mm_shuffle_epi8, _mm_slli_epi32, _mm_slli_si128, _mm_srli_epi32,
    _mm_srli_si128, _mm_xor_si128,
};

use crate::ct;

/// Round keys and H's powers, each block as its low then high half. The keys
/// outlive a call in this form because the registers holding them do not.
pub(crate) struct Key {
    round: [u64; 30],
    /// H^1 to H^8, byte-reversed as GHASH multiplies them.
    powers: [u64; 16],
    wide: bool,
}

impl Drop for Key {
    fn drop(&mut self) {
        ct::zero_words(&mut self.round);
        ct::zero_words(&mut self.powers);
    }
}

#[inline]
#[target_feature(enable = "aes,pclmulqdq,ssse3,sse4.1")]
fn from_halves(low: u64, high: u64) -> __m128i {
    _mm_set_epi64x(high as i64, low as i64)
}

#[inline]
#[target_feature(enable = "aes,pclmulqdq,ssse3,sse4.1")]
fn from_bytes(block: &[u8; 16]) -> __m128i {
    let whole = u128::from_le_bytes(*block);
    from_halves(whole as u64, (whole >> 64) as u64)
}

#[inline]
#[target_feature(enable = "aes,pclmulqdq,ssse3,sse4.1")]
fn to_bytes(x: __m128i) -> [u8; 16] {
    let low = _mm_cvtsi128_si64(x) as u64;
    let high = _mm_extract_epi64::<1>(x) as u64;
    ((u128::from(high) << 64) | u128::from(low)).to_le_bytes()
}

/// GHASH's bit order is the reverse of the instruction's: byte reversal and the
/// shift in `reduce` make up the difference.
#[inline]
#[target_feature(enable = "aes,pclmulqdq,ssse3,sse4.1")]
fn byte_reversed(x: __m128i) -> __m128i {
    _mm_shuffle_epi8(
        x,
        _mm_set_epi8(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15),
    )
}

#[inline]
#[target_feature(enable = "aes,pclmulqdq,ssse3,sse4.1")]
fn round_keys(key: &Key) -> [__m128i; 15] {
    let mut keys = [_mm_setzero_si128(); 15];
    for (slot, halves) in keys.iter_mut().zip(key.round.as_chunks::<2>().0) {
        let [low, high] = *halves;
        *slot = from_halves(low, high);
    }
    keys
}

#[inline]
#[target_feature(enable = "aes,pclmulqdq,ssse3,sse4.1")]
fn encrypt<const N: usize>(keys: &[__m128i; 15], wide: bool, mut x: [__m128i; N]) -> [__m128i; N] {
    let [first, ..] = keys;
    let (middle, last): (&[__m128i], &__m128i) = if wide {
        let [_, middle @ .., last] = keys;
        (middle, last)
    } else {
        let [_, middle @ .., last, _, _, _, _] = keys;
        (middle, last)
    };
    for lane in &mut x {
        *lane = _mm_xor_si128(*lane, *first);
    }
    for key in middle {
        for lane in &mut x {
            *lane = _mm_aesenc_si128(*lane, *key);
        }
    }
    for lane in &mut x {
        *lane = _mm_aesenclast_si128(*lane, *last);
    }
    x
}

/// `schedule` is FIPS 197's words, each of a block's four in memory order.
#[target_feature(enable = "aes,pclmulqdq,ssse3,sse4.1")]
pub(crate) fn new_key(schedule: &[u32], wide: bool) -> Box<Key> {
    let mut key = Box::new(Key {
        round: [0; 30],
        powers: [0; 16],
        wide,
    });
    for (halves, words) in key
        .round
        .as_chunks_mut::<2>()
        .0
        .iter_mut()
        .zip(schedule.as_chunks::<4>().0)
    {
        let [a, b, c, d] = *words;
        *halves = [
            u64::from(a) | (u64::from(b) << 32),
            u64::from(c) | (u64::from(d) << 32),
        ];
    }
    let [zero] = encrypt(&round_keys(&key), wide, [_mm_setzero_si128()]);
    let h = byte_reversed(zero);
    let mut power = h;
    for halves in key.powers.as_chunks_mut::<2>().0 {
        *halves = [
            _mm_cvtsi128_si64(power) as u64,
            _mm_extract_epi64::<1>(power) as u64,
        ];
        let [low, middle, high] = product(power, h);
        power = reduce(low, middle, high);
    }
    key
}

#[target_feature(enable = "aes,pclmulqdq,ssse3,sse4.1")]
pub(crate) fn encrypt_block(key: &Key, block: &[u8; 16]) -> [u8; 16] {
    let [out] = encrypt(&round_keys(key), key.wide, [from_bytes(block)]);
    to_bytes(out)
}

/// Eight blocks at a time, since the instructions run in parallel. The counter
/// wraps at 32 bits as SP 800-38D's inc32 does.
#[target_feature(enable = "aes,pclmulqdq,ssse3,sse4.1")]
pub(crate) fn ctr(key: &Key, nonce: &[u8; 12], first: u32, data: &mut [u8]) {
    let keys = round_keys(key);
    let mut bytes = [0u8; 16];
    for (out, byte) in bytes.iter_mut().zip(nonce) {
        *out = *byte;
    }
    let base = from_bytes(&bytes);
    let (whole, tail) = data.as_chunks_mut::<16>();
    // Whole batches run on past a short last one, so the tail counts for itself.
    let mut after_whole = first.wrapping_add(whole.len() as u32);
    let mut counter = first;
    for group in whole.chunks_mut(8) {
        let stream: [__m128i; 8] = encrypt(&keys, key.wide, counter_blocks(base, &mut counter));
        for (block, lane) in group.iter_mut().zip(stream) {
            *block = to_bytes(_mm_xor_si128(from_bytes(block), lane));
        }
    }
    if !tail.is_empty() {
        let [lane] = encrypt(&keys, key.wide, counter_blocks(base, &mut after_whole));
        let pad = to_bytes(lane);
        for (byte, mask) in tail.iter_mut().zip(pad) {
            *byte ^= mask;
        }
    }
}

#[inline]
#[target_feature(enable = "aes,pclmulqdq,ssse3,sse4.1")]
fn counter_blocks<const N: usize>(base: __m128i, counter: &mut u32) -> [__m128i; N] {
    let mut blocks = [base; N];
    for block in &mut blocks {
        *block = _mm_insert_epi32::<3>(base, counter.swap_bytes() as i32);
        *counter = counter.wrapping_add(1);
    }
    blocks
}

/// The carryless product's low, middle and high parts, unreduced so that several
/// can be summed first.
#[inline]
#[target_feature(enable = "aes,pclmulqdq,ssse3,sse4.1")]
fn product(a: __m128i, b: __m128i) -> [__m128i; 3] {
    [
        _mm_clmulepi64_si128::<0x00>(a, b),
        _mm_xor_si128(
            _mm_clmulepi64_si128::<0x10>(a, b),
            _mm_clmulepi64_si128::<0x01>(a, b),
        ),
        _mm_clmulepi64_si128::<0x11>(a, b),
    ]
}

/// Fold a sum of products into one block, modulo x^128 + x^7 + x^2 + x + 1.
#[inline]
#[target_feature(enable = "aes,pclmulqdq,ssse3,sse4.1")]
fn reduce(low: __m128i, middle: __m128i, high: __m128i) -> __m128i {
    let mut low = _mm_xor_si128(low, _mm_slli_si128::<8>(middle));
    let mut high = _mm_xor_si128(high, _mm_srli_si128::<8>(middle));

    // One bit left across the whole 256, for the reversed bit order.
    let low_carry = _mm_srli_epi32::<31>(low);
    let high_carry = _mm_srli_epi32::<31>(high);
    low = _mm_or_si128(_mm_slli_epi32::<1>(low), _mm_slli_si128::<4>(low_carry));
    high = _mm_or_si128(
        _mm_or_si128(_mm_slli_epi32::<1>(high), _mm_slli_si128::<4>(high_carry)),
        _mm_srli_si128::<12>(low_carry),
    );

    let spill = _mm_xor_si128(
        _mm_xor_si128(_mm_slli_epi32::<31>(low), _mm_slli_epi32::<30>(low)),
        _mm_slli_epi32::<25>(low),
    );
    low = _mm_xor_si128(low, _mm_slli_si128::<12>(spill));
    let down = _mm_xor_si128(
        _mm_xor_si128(_mm_srli_epi32::<1>(low), _mm_srli_epi32::<2>(low)),
        _mm_xor_si128(_mm_srli_epi32::<7>(low), _mm_srli_si128::<4>(spill)),
    );
    _mm_xor_si128(high, _mm_xor_si128(low, down))
}

/// Absorb `data`, its tail padded with zeros. `y` is GHASH's running value as
/// `aes_gcm` holds it: high half first, in the bit order of the specification.
#[target_feature(enable = "aes,pclmulqdq,ssse3,sse4.1")]
pub(crate) fn ghash(key: &Key, y: &mut [u64; 2], data: &[u8]) {
    let mut powers = [_mm_setzero_si128(); 8];
    for (slot, halves) in powers.iter_mut().zip(key.powers.as_chunks::<2>().0) {
        let [low, high] = *halves;
        *slot = from_halves(low, high);
    }
    let [one, ..] = powers;

    let [y_high, y_low] = *y;
    let mut state = from_halves(y_low, y_high);

    let (groups, rest) = data.as_chunks::<128>();
    for group in groups {
        let mut sum = [_mm_setzero_si128(); 3];
        // (y ^ b0) H^8 ^ b1 H^7 ^ ... ^ b7 H: one reduction per eight blocks.
        let mut carried = state;
        for (block, power) in group.as_chunks::<16>().0.iter().zip(powers.iter().rev()) {
            let x = _mm_xor_si128(byte_reversed(from_bytes(block)), carried);
            carried = _mm_setzero_si128();
            let parts = product(x, *power);
            for (total, part) in sum.iter_mut().zip(parts) {
                *total = _mm_xor_si128(*total, part);
            }
        }
        let [low, middle, high] = sum;
        state = reduce(low, middle, high);
    }

    let (blocks, tail) = rest.as_chunks::<16>();
    for block in blocks {
        let x = _mm_xor_si128(byte_reversed(from_bytes(block)), state);
        let [low, middle, high] = product(x, one);
        state = reduce(low, middle, high);
    }
    if !tail.is_empty() {
        let mut last = [0u8; 16];
        for (out, byte) in last.iter_mut().zip(tail) {
            *out = *byte;
        }
        let x = _mm_xor_si128(byte_reversed(from_bytes(&last)), state);
        let [low, middle, high] = product(x, one);
        state = reduce(low, middle, high);
    }

    *y = [
        _mm_extract_epi64::<1>(state) as u64,
        _mm_cvtsi128_si64(state) as u64,
    ];
}

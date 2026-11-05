//! ChaCha20 by AVX2, eight blocks at a time: the same algorithm and the same bytes
//! as `chacha20_poly1305`'s software path, which a test holds it to. Each of the
//! sixteen state words gets a register with one lane per block, so the four
//! quarter rounds of a round run side by side rather than as one long chain.

use std::arch::x86_64::{
    __m256i, _mm256_add_epi32, _mm256_broadcastsi128_si256, _mm256_extract_epi64, _mm256_or_si256,
    _mm256_permute2x128_si256, _mm256_set1_epi32, _mm256_set_epi32, _mm256_set_epi64x,
    _mm256_shuffle_epi8, _mm256_slli_epi32, _mm256_srli_epi32, _mm256_unpackhi_epi32,
    _mm256_unpackhi_epi64, _mm256_unpacklo_epi32, _mm256_unpacklo_epi64, _mm256_xor_si256,
    _mm_set_epi32,
};

use crate::chacha20_poly1305::SIGMA;

#[inline]
#[target_feature(enable = "avx2")]
fn add(a: __m256i, b: __m256i) -> __m256i {
    _mm256_add_epi32(a, b)
}

/// Rotations by whole bytes are shuffles; these repeat within each half.
#[inline]
#[target_feature(enable = "avx2")]
fn rotations() -> (__m256i, __m256i) {
    (
        _mm256_broadcastsi128_si256(_mm_set_epi32(
            0x0d0c0f0e, 0x09080b0a, 0x05040706, 0x01000302,
        )),
        _mm256_broadcastsi128_si256(_mm_set_epi32(
            0x0e0d0c0f, 0x0a09080b, 0x06050407, 0x02010003,
        )),
    )
}

#[inline]
#[target_feature(enable = "avx2")]
fn quarter(
    [mut a, mut b, mut c, mut d]: [__m256i; 4],
    (by_16, by_8): (__m256i, __m256i),
) -> [__m256i; 4] {
    a = add(a, b);
    d = _mm256_shuffle_epi8(_mm256_xor_si256(d, a), by_16);
    c = add(c, d);
    let x = _mm256_xor_si256(b, c);
    b = _mm256_or_si256(_mm256_slli_epi32::<12>(x), _mm256_srli_epi32::<20>(x));
    a = add(a, b);
    d = _mm256_shuffle_epi8(_mm256_xor_si256(d, a), by_8);
    c = add(c, d);
    let x = _mm256_xor_si256(b, c);
    b = _mm256_or_si256(_mm256_slli_epi32::<7>(x), _mm256_srli_epi32::<25>(x));
    [a, b, c, d]
}

/// Eight registers of one word per lane, as eight registers of one block's eight
/// consecutive words, in block order.
#[inline]
#[target_feature(enable = "avx2")]
fn transpose([r0, r1, r2, r3, r4, r5, r6, r7]: [__m256i; 8]) -> [__m256i; 8] {
    let (t0, t1) = (_mm256_unpacklo_epi32(r0, r1), _mm256_unpackhi_epi32(r0, r1));
    let (t2, t3) = (_mm256_unpacklo_epi32(r2, r3), _mm256_unpackhi_epi32(r2, r3));
    let (t4, t5) = (_mm256_unpacklo_epi32(r4, r5), _mm256_unpackhi_epi32(r4, r5));
    let (t6, t7) = (_mm256_unpacklo_epi32(r6, r7), _mm256_unpackhi_epi32(r6, r7));
    let (u0, u1) = (_mm256_unpacklo_epi64(t0, t2), _mm256_unpackhi_epi64(t0, t2));
    let (u2, u3) = (_mm256_unpacklo_epi64(t1, t3), _mm256_unpackhi_epi64(t1, t3));
    let (u4, u5) = (_mm256_unpacklo_epi64(t4, t6), _mm256_unpackhi_epi64(t4, t6));
    let (u6, u7) = (_mm256_unpacklo_epi64(t5, t7), _mm256_unpackhi_epi64(t5, t7));
    [
        _mm256_permute2x128_si256::<0x20>(u0, u4),
        _mm256_permute2x128_si256::<0x20>(u1, u5),
        _mm256_permute2x128_si256::<0x20>(u2, u6),
        _mm256_permute2x128_si256::<0x20>(u3, u7),
        _mm256_permute2x128_si256::<0x31>(u0, u4),
        _mm256_permute2x128_si256::<0x31>(u1, u5),
        _mm256_permute2x128_si256::<0x31>(u2, u6),
        _mm256_permute2x128_si256::<0x31>(u3, u7),
    ]
}

#[inline]
#[target_feature(enable = "avx2")]
fn from_bytes(bytes: &[u8; 32]) -> __m256i {
    let [w0, w1, w2, w3] = bytes.as_chunks::<8>().0 else {
        return _mm256_set_epi64x(0, 0, 0, 0);
    };
    _mm256_set_epi64x(
        i64::from_le_bytes(*w3),
        i64::from_le_bytes(*w2),
        i64::from_le_bytes(*w1),
        i64::from_le_bytes(*w0),
    )
}

#[inline]
#[target_feature(enable = "avx2")]
fn to_bytes(x: __m256i) -> [u8; 32] {
    let mut out = [0u8; 32];
    let words = [
        _mm256_extract_epi64::<0>(x),
        _mm256_extract_epi64::<1>(x),
        _mm256_extract_epi64::<2>(x),
        _mm256_extract_epi64::<3>(x),
    ];
    for (bytes, word) in out.as_chunks_mut::<8>().0.iter_mut().zip(words) {
        *bytes = word.to_le_bytes();
    }
    out
}

/// XOR the keystream from block `first` into `data`. A short tail spends a whole
/// batch, and the counter wraps at 32 bits as the software path's does.
#[target_feature(enable = "avx2")]
pub(crate) fn crypt(key: &[u32; 8], nonce: &[u32; 3], first: u32, data: &mut [u8]) {
    let by = rotations();
    let [s0, s1, s2, s3] = SIGMA.map(|word| _mm256_set1_epi32(word as i32));
    let [k0, k1, k2, k3, k4, k5, k6, k7] = key.map(|word| _mm256_set1_epi32(word as i32));
    let [n0, n1, n2] = nonce.map(|word| _mm256_set1_epi32(word as i32));
    let mut counter = first;
    for chunk in data.chunks_mut(512) {
        let lane = |at: u32| counter.wrapping_add(at) as i32;
        let c = _mm256_set_epi32(
            lane(7),
            lane(6),
            lane(5),
            lane(4),
            lane(3),
            lane(2),
            lane(1),
            lane(0),
        );
        let (mut x0, mut x1, mut x2, mut x3) = (s0, s1, s2, s3);
        let (mut x4, mut x5, mut x6, mut x7) = (k0, k1, k2, k3);
        let (mut x8, mut x9, mut x10, mut x11) = (k4, k5, k6, k7);
        let (mut x12, mut x13, mut x14, mut x15) = (c, n0, n1, n2);
        for _ in 0..10 {
            [x0, x4, x8, x12] = quarter([x0, x4, x8, x12], by);
            [x1, x5, x9, x13] = quarter([x1, x5, x9, x13], by);
            [x2, x6, x10, x14] = quarter([x2, x6, x10, x14], by);
            [x3, x7, x11, x15] = quarter([x3, x7, x11, x15], by);
            [x0, x5, x10, x15] = quarter([x0, x5, x10, x15], by);
            [x1, x6, x11, x12] = quarter([x1, x6, x11, x12], by);
            [x2, x7, x8, x13] = quarter([x2, x7, x8, x13], by);
            [x3, x4, x9, x14] = quarter([x3, x4, x9, x14], by);
        }
        let front = transpose([
            add(x0, s0),
            add(x1, s1),
            add(x2, s2),
            add(x3, s3),
            add(x4, k0),
            add(x5, k1),
            add(x6, k2),
            add(x7, k3),
        ]);
        let back = transpose([
            add(x8, k4),
            add(x9, k5),
            add(x10, k6),
            add(x11, k7),
            add(x12, c),
            add(x13, n0),
            add(x14, n1),
            add(x15, n2),
        ]);
        let mut stream = front.iter().zip(&back).flat_map(|(one, two)| [*one, *two]);
        let (whole, tail) = chunk.as_chunks_mut::<32>();
        for (block, lane) in whole.iter_mut().zip(&mut stream) {
            *block = to_bytes(_mm256_xor_si256(from_bytes(block), lane));
        }
        if let Some(lane) = stream.next() {
            for (byte, mask) in tail.iter_mut().zip(to_bytes(lane)) {
                *byte ^= mask;
            }
        }
        counter = counter.wrapping_add(8);
    }
}

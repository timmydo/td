//! SHA-256's compression by the SHA extensions: the same algorithm and the same
//! bytes as `sha`'s software path, which a test holds it to. Each pass does four
//! rounds and prepares the next four schedule words.

use std::arch::x86_64::{
    _mm_add_epi32, _mm_alignr_epi8, _mm_extract_epi32, _mm_set_epi32, _mm_set_epi64x,
    _mm_sha256msg1_epu32, _mm_sha256msg2_epu32, _mm_sha256rnds2_epu32, _mm_shuffle_epi32,
    _mm_shuffle_epi8,
};

use crate::sha::K256;

#[target_feature(enable = "sha,ssse3,sse4.1")]
pub(crate) fn compress256(h: &mut [u32; 8], block: &[u8; 64]) {
    let [a, b, c, d, e, f, g, last] = h.map(|word| word as i32);
    let start_abef = _mm_set_epi32(a, b, e, f);
    let start_cdgh = _mm_set_epi32(c, d, g, last);
    let (mut abef, mut cdgh) = (start_abef, start_cdgh);

    // Big-endian words, each in its own lane.
    let swap = _mm_set_epi64x(0x0c0d0e0f08090a0b, 0x0405060700010203);
    let mut schedule = [_mm_set_epi32(0, 0, 0, 0); 4];
    for (slot, bytes) in schedule.iter_mut().zip(block.as_chunks::<16>().0) {
        let whole = u128::from_le_bytes(*bytes);
        let loaded = _mm_set_epi64x((whole >> 64) as i64, whole as i64);
        *slot = _mm_shuffle_epi8(loaded, swap);
    }

    for constants in K256.as_chunks::<4>().0 {
        let [m0, m1, m2, m3] = schedule;
        let [k0, k1, k2, k3] = constants.map(|word| word as i32);
        let due = _mm_add_epi32(m0, _mm_set_epi32(k3, k2, k1, k0));
        cdgh = _mm_sha256rnds2_epu32(cdgh, abef, due);
        abef = _mm_sha256rnds2_epu32(abef, cdgh, _mm_shuffle_epi32::<0x0e>(due));
        let next = _mm_sha256msg2_epu32(
            _mm_add_epi32(_mm_sha256msg1_epu32(m0, m1), _mm_alignr_epi8::<4>(m3, m2)),
            m3,
        );
        schedule = [m1, m2, m3, next];
    }

    let abef = _mm_add_epi32(abef, start_abef);
    let cdgh = _mm_add_epi32(cdgh, start_cdgh);
    *h = [
        _mm_extract_epi32::<3>(abef),
        _mm_extract_epi32::<2>(abef),
        _mm_extract_epi32::<3>(cdgh),
        _mm_extract_epi32::<2>(cdgh),
        _mm_extract_epi32::<1>(abef),
        _mm_extract_epi32::<0>(abef),
        _mm_extract_epi32::<1>(cdgh),
        _mm_extract_epi32::<0>(cdgh),
    ]
    .map(|word| word as u32);
}

//! ML-KEM-768 (FIPS 203): k = 3, eta1 = eta2 = 2, du = 10, dv = 4.
//!
//! Coefficients are i16 in Montgomery or centered form, reduced by the
//! multiply-and-shift Barrett and Montgomery steps; `wrapping` arithmetic
//! keeps a hostile dk, whose coefficients decode up to 4095, from panicking.

use crate::ct;
use crate::keccak::{sha3_256, sha3_512, Shake128, Shake256};

pub const EK_LEN: usize = 1184;
pub const DK_LEN: usize = 2400;
pub const CT_LEN: usize = 1088;
pub const SS_LEN: usize = 32;

const K: usize = 3;
const N: usize = 256;
const Q: i16 = 3329;
/// Q^-1 mod 2^16, centered.
const QINV: i16 = -3327;
/// 2^32 / 128 mod Q: undoes the transform's scale of 128, and through
/// Montgomery reduction also restores the factor `dot` removed.
const INVERSE_NTT_SCALE: i16 = 1441;
/// 2^32 mod Q: through Montgomery reduction, restores the factor `dot` removed.
const MONT_SQUARED: i16 = 1353;
const POLY_BYTES: usize = 384;
const VEC_BYTES: usize = K * POLY_BYTES;
const U_BYTES: usize = K * 320;
const SEED_BYTES: usize = 32;
const SHAKE128_RATE: usize = 168;

/// zeta^BitRev7(i) * 2^16 mod Q with zeta = 17, centered: FIPS 203 section 4.3
/// in Montgomery form. The length is the NTT's 128 pairs.
const ZETAS: [i16; 128] = [
    -1044, -758, -359, -1517, 1493, 1422, 287, 202, -171, 622, 1577, 182, 962, -1202, -1474, 1468,
    573, -1325, 264, 383, -829, 1458, -1602, -130, -681, 1017, 732, 608, -1542, 411, -205, -1571,
    1223, 652, -552, 1015, -1293, 1491, -282, -1544, 516, -8, -320, -666, -1618, -1162, 126, 1469,
    -853, -90, -271, 830, 107, -1421, -247, -951, -398, 961, -1508, -725, 448, -1065, 677, -1275,
    -1103, 430, 555, 843, -1251, 871, 1550, 105, 422, 587, 177, -235, -291, -460, 1574, 1653, -246,
    778, 1159, -147, -777, 1483, -602, 1119, -1590, 644, -872, 349, 418, 329, -156, -75, 817, 1097,
    603, 610, 1322, -1285, -1465, 384, -1215, -136, 1218, -1335, -874, 220, -1187, -1659, -1185,
    -1530, -1278, 794, -1510, -854, -870, 478, -108, -308, 996, 991, 958, -1460, 1522, 1628,
];

type Poly = [i16; N];
type PolyVec = [Poly; K];

const fn montgomery_reduce(a: i32) -> i16 {
    let t = (a as i16).wrapping_mul(QINV);
    (a.wrapping_sub(t as i32 * Q as i32) >> 16) as i16
}

const fn fqmul(a: i16, b: i16) -> i16 {
    montgomery_reduce(a as i32 * b as i32)
}

/// Into [-(Q - 1) / 2, (Q - 1) / 2] for any i16.
const fn barrett_reduce(a: i16) -> i16 {
    const V: i32 = ((1 << 26) + Q as i32 / 2) / Q as i32;
    let t = ((V * a as i32 + (1 << 25)) >> 26) as i16;
    a.wrapping_sub(t.wrapping_mul(Q))
}

/// From (-Q, Q) to [0, Q), by the sign bit alone.
const fn canonical(a: i16) -> u16 {
    a.wrapping_add((a >> 15) & Q) as u16
}

fn reduce(poly: &mut Poly) {
    for coefficient in poly {
        *coefficient = barrett_reduce(*coefficient);
    }
}

/// FIPS 203 algorithm 9.
fn ntt(poly: &mut Poly) {
    let mut zetas = ZETAS.iter().skip(1);
    for half in [128usize, 64, 32, 16, 8, 4, 2] {
        for chunk in poly.chunks_exact_mut(2 * half) {
            let zeta = zetas.next().copied().unwrap_or(0);
            let (low, high) = chunk.split_at_mut(half);
            for (a, b) in low.iter_mut().zip(high) {
                let t = fqmul(zeta, *b);
                *b = a.wrapping_sub(t);
                *a = a.wrapping_add(t);
            }
        }
    }
    reduce(poly);
}

/// FIPS 203 algorithm 10, also leaving the Montgomery factor basemul removed.
fn inverse_ntt(poly: &mut Poly) {
    let mut zetas = ZETAS.iter().rev();
    for half in [2usize, 4, 8, 16, 32, 64, 128] {
        for chunk in poly.chunks_exact_mut(2 * half) {
            let zeta = zetas.next().copied().unwrap_or(0);
            let (low, high) = chunk.split_at_mut(half);
            for (a, b) in low.iter_mut().zip(high) {
                let t = *a;
                *a = barrett_reduce(t.wrapping_add(*b));
                *b = fqmul(zeta, b.wrapping_sub(t));
            }
        }
    }
    for coefficient in poly {
        *coefficient = fqmul(*coefficient, INVERSE_NTT_SCALE);
    }
}

/// Adds a * b to `out`, FIPS 203 algorithms 11 and 12, each degree-two factor
/// taken with its root and that root's negation.
fn base_mul_accumulate(out: &mut Poly, a: &Poly, b: &Poly) {
    fn pair(a: [i16; 2], b: [i16; 2], zeta: i16) -> [i16; 2] {
        let [a0, a1] = a;
        let [b0, b1] = b;
        [
            fqmul(fqmul(a1, b1), zeta).wrapping_add(fqmul(a0, b0)),
            fqmul(a0, b1).wrapping_add(fqmul(a1, b0)),
        ]
    }
    let (out_chunks, _) = out.as_chunks_mut::<4>();
    let (a_chunks, _) = a.as_chunks::<4>();
    let (b_chunks, _) = b.as_chunks::<4>();
    let factors = out_chunks
        .iter_mut()
        .zip(a_chunks.iter().zip(b_chunks))
        .zip(ZETAS.iter().skip(64));
    for ((r, (a, b)), zeta) in factors {
        let [a0, a1, a2, a3] = *a;
        let [b0, b1, b2, b3] = *b;
        let [c0, c1] = pair([a0, a1], [b0, b1], *zeta);
        let [c2, c3] = pair([a2, a3], [b2, b3], zeta.wrapping_neg());
        let [r0, r1, r2, r3] = r;
        *r0 = r0.wrapping_add(c0);
        *r1 = r1.wrapping_add(c1);
        *r2 = r2.wrapping_add(c2);
        *r3 = r3.wrapping_add(c3);
    }
}

/// Sum of row[i] * vector[i], reduced; in the NTT domain with a Montgomery
/// factor removed.
fn dot(row: &PolyVec, vector: &PolyVec) -> Poly {
    let mut out = [0i16; N];
    for (a, b) in row.iter().zip(vector) {
        base_mul_accumulate(&mut out, a, b);
    }
    reduce(&mut out);
    out
}

fn add_assign(out: &mut Poly, other: &Poly) {
    for (a, b) in out.iter_mut().zip(other) {
        *a = a.wrapping_add(*b);
    }
}

/// FIPS 203 algorithm 7. Rejection reads only public bytes.
fn sample_ntt(rho: &[u8; SEED_BYTES], first: u8, second: u8) -> Poly {
    let mut xof = Shake128::new();
    xof.absorb(rho);
    xof.absorb(&[first, second]);
    let mut poly = [0i16; N];
    let mut slots = poly.iter_mut();
    let mut block = [0u8; SHAKE128_RATE];
    'filling: loop {
        xof.squeeze(&mut block);
        for [b0, b1, b2] in block.as_chunks::<3>().0.iter().copied() {
            let d1 = u16::from(b0) | (u16::from(b1 & 0xf) << 8);
            let d2 = u16::from(b1 >> 4) | (u16::from(b2) << 4);
            for candidate in [d1, d2] {
                if candidate < Q as u16 {
                    let Some(slot) = slots.next() else {
                        break 'filling;
                    };
                    *slot = candidate as i16;
                }
            }
        }
    }
    poly
}

/// A-hat from rho: `transposed` swaps the XOF's indices (FIPS 203 algorithm 13
/// line 5 and algorithm 14 line 5, which uses the transpose).
fn matrix(rho: &[u8; SEED_BYTES], transposed: bool) -> [PolyVec; K] {
    let mut out = [[[0i16; N]; K]; K];
    for (row, i) in out.iter_mut().zip(0u8..) {
        for (entry, j) in row.iter_mut().zip(0u8..) {
            *entry = if transposed {
                sample_ntt(rho, i, j)
            } else {
                sample_ntt(rho, j, i)
            };
        }
    }
    out
}

/// PRF_2 then CBD_2, FIPS 203 algorithm 8 and section 4.1.
fn sample_cbd(seed: &[u8; SEED_BYTES], nonce: u8) -> Poly {
    let mut xof = Shake256::new();
    xof.absorb(seed);
    xof.absorb(&[nonce]);
    let mut bytes = [0u8; 2 * N / 4];
    xof.squeeze(&mut bytes);
    let mut poly = [0i16; N];
    let (chunks, _) = poly.as_chunks_mut::<8>();
    for (coefficients, word) in chunks.iter_mut().zip(bytes.as_chunks::<4>().0) {
        let t = u32::from_le_bytes(*word);
        let mut d = (t & 0x5555_5555) + ((t >> 1) & 0x5555_5555);
        for coefficient in coefficients {
            *coefficient = (d & 3) as i16 - ((d >> 2) & 3) as i16;
            d >>= 4;
        }
    }
    ct::zero(&mut bytes);
    poly
}

fn sample_vec(seed: &[u8; SEED_BYTES], first_nonce: u8) -> PolyVec {
    [
        sample_cbd(seed, first_nonce),
        sample_cbd(seed, first_nonce + 1),
        sample_cbd(seed, first_nonce + 2),
    ]
}

/// ByteEncode_12 of coefficients in (-Q, Q).
fn encode_12(vector: &PolyVec, out: &mut [u8]) {
    for (poly, bytes) in vector.iter().zip(out.as_chunks_mut::<POLY_BYTES>().0) {
        let (pairs, _) = poly.as_chunks::<2>();
        for ([a, b], [r0, r1, r2]) in pairs.iter().zip(bytes.as_chunks_mut::<3>().0) {
            let (a, b) = (canonical(*a), canonical(*b));
            *r0 = a as u8;
            *r1 = ((a >> 8) | (b << 4)) as u8;
            *r2 = (b >> 4) as u8;
        }
    }
}

/// ByteDecode_12. It compares nothing, because it also reads secret s: only
/// `encaps`, on the peer's key, asks whether a coefficient is in range.
fn decode_12(bytes: &[u8], out: &mut PolyVec) {
    for (poly, chunk) in out.iter_mut().zip(bytes.as_chunks::<POLY_BYTES>().0) {
        let (pairs, _) = poly.as_chunks_mut::<2>();
        for ([a, b], [r0, r1, r2]) in pairs.iter_mut().zip(chunk.as_chunks::<3>().0) {
            let (r0, r1, r2) = (i16::from(*r0), i16::from(*r1), i16::from(*r2));
            *a = r0 | ((r1 & 0xf) << 8);
            *b = (r1 >> 4) | (r2 << 4);
        }
    }
}

/// Compress_d of a coefficient in (-Q, Q). `reciprocal` is floor(2^shift / Q)
/// so that no division sees a secret; rounding is pinned for every coefficient
/// by a test.
const fn compress(a: i16, d: u32, reciprocal: u64, shift: u32) -> u64 {
    let scaled = ((canonical(a) as u64) << d) + 1665;
    (scaled * reciprocal) >> shift
}

const fn compress_1(a: i16) -> u8 {
    (compress(a, 1, 80635, 28) & 1) as u8
}

const fn compress_4(a: i16) -> u8 {
    (compress(a, 4, 80635, 28) & 0xf) as u8
}

const fn compress_10(a: i16) -> u16 {
    (compress(a, 10, 1_290_167, 32) & 0x3ff) as u16
}

const fn decompress(y: u32, d: u32) -> i16 {
    ((y * Q as u32 + (1 << (d - 1))) >> d) as i16
}

/// ByteEncode_10 of Compress_10, FIPS 203 algorithm 14 line 22.
fn encode_u(vector: &PolyVec, out: &mut [u8]) {
    for (poly, bytes) in vector.iter().zip(out.as_chunks_mut::<320>().0) {
        let (fours, _) = poly.as_chunks::<4>();
        for ([a, b, c, d], [r0, r1, r2, r3, r4]) in fours.iter().zip(bytes.as_chunks_mut::<5>().0) {
            let (a, b, c, d) = (
                compress_10(*a),
                compress_10(*b),
                compress_10(*c),
                compress_10(*d),
            );
            *r0 = a as u8;
            *r1 = ((a >> 8) | (b << 2)) as u8;
            *r2 = ((b >> 6) | (c << 4)) as u8;
            *r3 = ((c >> 4) | (d << 6)) as u8;
            *r4 = (d >> 2) as u8;
        }
    }
}

fn decode_u(bytes: &[u8], out: &mut PolyVec) {
    for (poly, chunk) in out.iter_mut().zip(bytes.as_chunks::<320>().0) {
        let (fours, _) = poly.as_chunks_mut::<4>();
        for ([a, b, c, d], [r0, r1, r2, r3, r4]) in fours.iter_mut().zip(chunk.as_chunks::<5>().0) {
            let (r0, r1, r2, r3, r4) = (
                u32::from(*r0),
                u32::from(*r1),
                u32::from(*r2),
                u32::from(*r3),
                u32::from(*r4),
            );
            *a = decompress((r0 | (r1 << 8)) & 0x3ff, 10);
            *b = decompress(((r1 >> 2) | (r2 << 6)) & 0x3ff, 10);
            *c = decompress(((r2 >> 4) | (r3 << 4)) & 0x3ff, 10);
            *d = decompress(((r3 >> 6) | (r4 << 2)) & 0x3ff, 10);
        }
    }
}

fn encode_v(poly: &Poly, out: &mut [u8]) {
    let (pairs, _) = poly.as_chunks::<2>();
    for ([a, b], byte) in pairs.iter().zip(out) {
        *byte = compress_4(*a) | (compress_4(*b) << 4);
    }
}

fn decode_v(bytes: &[u8], out: &mut Poly) {
    let (pairs, _) = out.as_chunks_mut::<2>();
    for ([a, b], byte) in pairs.iter_mut().zip(bytes) {
        *a = decompress(u32::from(*byte & 0xf), 4);
        *b = decompress(u32::from(*byte >> 4), 4);
    }
}

/// Decompress_1(ByteDecode_1(m)): 1665 where the message bit is set.
fn message_to_poly(message: &[u8; SEED_BYTES]) -> Poly {
    let mut poly = [0i16; N];
    let (eights, _) = poly.as_chunks_mut::<8>();
    for (coefficients, byte) in eights.iter_mut().zip(message) {
        let mut bits = *byte;
        for coefficient in coefficients {
            let mask = std::hint::black_box(i16::from(bits & 1).wrapping_neg());
            *coefficient = mask & 1665;
            bits >>= 1;
        }
    }
    poly
}

fn poly_to_message(poly: &Poly) -> [u8; SEED_BYTES] {
    let mut message = [0u8; SEED_BYTES];
    let (eights, _) = poly.as_chunks::<8>();
    for (coefficients, byte) in eights.iter().zip(message.iter_mut()) {
        let mut bits = 0u8;
        for (coefficient, position) in coefficients.iter().zip(0u8..) {
            bits |= compress_1(*coefficient) << position;
        }
        *byte = bits;
    }
    message
}

fn wipe_polys(polys: &mut [Poly]) {
    for poly in polys.iter_mut() {
        poly.fill(0);
    }
    std::hint::black_box(polys);
}

/// K-PKE.Encrypt, FIPS 203 algorithm 14.
fn pke_encrypt(
    ek: &[u8; EK_LEN],
    message: &[u8; SEED_BYTES],
    randomness: &[u8; SEED_BYTES],
    out: &mut [u8; CT_LEN],
) {
    let (t_bytes, rho_bytes) = ek.split_at(VEC_BYTES);
    let mut rho = [0u8; SEED_BYTES];
    rho.copy_from_slice(rho_bytes);
    let mut t_hat = [[0i16; N]; K];
    decode_12(t_bytes, &mut t_hat);
    let a_transpose = matrix(&rho, true);
    let mut y = sample_vec(randomness, 0);
    let mut e1 = sample_vec(randomness, K as u8);
    let mut e2 = sample_cbd(randomness, 2 * K as u8);
    for poly in &mut y {
        ntt(poly);
    }
    let mut u = [[0i16; N]; K];
    for ((u_poly, row), e) in u.iter_mut().zip(&a_transpose).zip(&e1) {
        *u_poly = dot(row, &y);
        inverse_ntt(u_poly);
        add_assign(u_poly, e);
        reduce(u_poly);
    }
    let mut v = dot(&t_hat, &y);
    inverse_ntt(&mut v);
    add_assign(&mut v, &e2);
    let mut mu = message_to_poly(message);
    add_assign(&mut v, &mu);
    reduce(&mut v);
    let (c1, c2) = out.split_at_mut(U_BYTES);
    encode_u(&u, c1);
    encode_v(&v, c2);
    wipe_polys(&mut y);
    wipe_polys(&mut e1);
    wipe_polys(std::slice::from_mut(&mut e2));
    wipe_polys(std::slice::from_mut(&mut mu));
    wipe_polys(std::slice::from_mut(&mut v));
    wipe_polys(&mut u);
}

/// K-PKE.Decrypt, FIPS 203 algorithm 15.
fn pke_decrypt(s_bytes: &[u8; VEC_BYTES], ct: &[u8; CT_LEN]) -> [u8; SEED_BYTES] {
    let (c1, c2) = ct.split_at(U_BYTES);
    let mut u = [[0i16; N]; K];
    decode_u(c1, &mut u);
    let mut v = [0i16; N];
    decode_v(c2, &mut v);
    let mut s_hat = [[0i16; N]; K];
    decode_12(s_bytes, &mut s_hat);
    for poly in &mut u {
        ntt(poly);
    }
    let mut w = dot(&s_hat, &u);
    inverse_ntt(&mut w);
    for (x, y) in w.iter_mut().zip(&v) {
        *x = barrett_reduce(y.wrapping_sub(*x));
    }
    let message = poly_to_message(&w);
    wipe_polys(&mut s_hat);
    wipe_polys(std::slice::from_mut(&mut w));
    message
}

/// G(m || H(ek)) split as (K, r), FIPS 203 algorithms 17 and 18.
fn derive(message: &[u8; SEED_BYTES], h: &[u8; SEED_BYTES]) -> ([u8; SS_LEN], [u8; SEED_BYTES]) {
    let mut input = [0u8; 2 * SEED_BYTES];
    let (first, second) = input.split_at_mut(SEED_BYTES);
    first.copy_from_slice(message);
    second.copy_from_slice(h);
    let mut output = sha3_512(&input);
    ct::zero(&mut input);
    let mut key = [0u8; SS_LEN];
    let mut r = [0u8; SEED_BYTES];
    let (head, tail) = output.split_at(SS_LEN);
    key.copy_from_slice(head);
    r.copy_from_slice(tail);
    ct::zero(&mut output);
    (key, r)
}

/// FIPS 203 algorithm 16. Returns (ek, dk).
pub fn keygen(d: &[u8; 32], z: &[u8; 32]) -> (Vec<u8>, Vec<u8>) {
    // K-PKE.KeyGen, FIPS 203 algorithm 13: k is domain-separated into G.
    let mut seed_input = [K as u8; SEED_BYTES + 1];
    if let Some(head) = seed_input.get_mut(..SEED_BYTES) {
        head.copy_from_slice(d);
    }
    let mut expanded = sha3_512(&seed_input);
    ct::zero(&mut seed_input);
    let mut rho = [0u8; SEED_BYTES];
    let mut sigma = [0u8; SEED_BYTES];
    let (rho_bytes, sigma_bytes) = expanded.split_at(SEED_BYTES);
    rho.copy_from_slice(rho_bytes);
    sigma.copy_from_slice(sigma_bytes);
    ct::zero(&mut expanded);

    let a_hat = matrix(&rho, false);
    let mut s = sample_vec(&sigma, 0);
    let mut e = sample_vec(&sigma, K as u8);
    ct::zero(&mut sigma);
    for poly in s.iter_mut().chain(e.iter_mut()) {
        ntt(poly);
    }
    let mut t_hat = [[0i16; N]; K];
    for ((t, row), e) in t_hat.iter_mut().zip(&a_hat).zip(&e) {
        *t = dot(row, &s);
        // On the scale of e-hat, which `dot` has not touched.
        for coefficient in t.iter_mut() {
            *coefficient = fqmul(*coefficient, MONT_SQUARED);
        }
        add_assign(t, e);
        reduce(t);
    }

    let mut ek = vec![0u8; EK_LEN];
    let (t_bytes, rho_out) = ek.split_at_mut(VEC_BYTES);
    encode_12(&t_hat, t_bytes);
    rho_out.copy_from_slice(&rho);

    let mut dk = Vec::with_capacity(DK_LEN);
    let mut s_bytes = [0u8; VEC_BYTES];
    encode_12(&s, &mut s_bytes);
    dk.extend_from_slice(&s_bytes);
    dk.extend_from_slice(&ek);
    dk.extend_from_slice(&sha3_256(&ek));
    dk.extend_from_slice(z);
    ct::zero(&mut s_bytes);
    wipe_polys(&mut s);
    wipe_polys(&mut e);
    (ek, dk)
}

/// None if ek has the wrong length or fails the FIPS 203 section 7.2 modulus
/// check. `m` is 32 random bytes. Returns (ct, ss).
pub fn encaps(ek: &[u8], m: &[u8; 32]) -> Option<(Vec<u8>, [u8; SS_LEN])> {
    let ek: &[u8; EK_LEN] = ek.try_into().ok()?;
    let mut t_hat = [[0i16; N]; K];
    decode_12(ek, &mut t_hat);
    if t_hat.as_flattened().iter().any(|c| *c >= Q) {
        return None;
    }
    let (key, mut r) = derive(m, &sha3_256(ek));
    let mut ct = [0u8; CT_LEN];
    pke_encrypt(ek, m, &r, &mut ct);
    ct::zero(&mut r);
    Some((ct.to_vec(), key))
}

/// Implicit rejection per FIPS 203 algorithm 18. None for a wrong length of dk
/// or ct, and for a dk whose stored H(ek) does not match its ek: FIPS 203
/// section 7.3's hash check, a local-file error, not a decapsulation failure.
pub fn decaps(dk: &[u8], ct: &[u8]) -> Option<[u8; SS_LEN]> {
    let dk: &[u8; DK_LEN] = dk.try_into().ok()?;
    let ct: &[u8; CT_LEN] = ct.try_into().ok()?;
    let (s_bytes, rest) = dk.split_first_chunk::<VEC_BYTES>()?;
    let (ek, rest) = rest.split_first_chunk::<EK_LEN>()?;
    let (h, z) = rest.split_first_chunk::<SEED_BYTES>()?;
    if !ct::eq(&sha3_256(ek), h) {
        return None;
    }
    let mut message = pke_decrypt(s_bytes, ct);
    let (mut key, mut r) = derive(&message, h);
    let mut rejected = [0u8; SS_LEN];
    let mut xof = Shake256::new();
    xof.absorb(z);
    xof.absorb(ct);
    xof.squeeze(&mut rejected);
    let mut recomputed = [0u8; CT_LEN];
    pke_encrypt(ek, &message, &r, &mut recomputed);
    let difference = recomputed
        .iter()
        .zip(ct)
        .fold(0u8, |acc, (a, b)| acc | (a ^ b));
    // All ones when the ciphertexts match, whatever the difference was.
    let keep = std::hint::black_box((u16::from(difference).wrapping_sub(1) >> 8) as u8);
    for (out, other) in key.iter_mut().zip(rejected) {
        *out = (*out & keep) | (other & !keep);
    }
    ct::zero(&mut message);
    ct::zero(&mut r);
    ct::zero(&mut rejected);
    ct::zero(&mut recomputed);
    Some(key)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::indexing_slicing, clippy::unwrap_used)]
    use super::*;

    /// FIPS 203 section 4.2.1's rounding, by exact division, for all of Z_q in
    /// both representatives the arithmetic can hand over.
    fn exact(x: i32, d: u32) -> u32 {
        let x = x.rem_euclid(3329) as u32;
        (((x << d) * 2 + 3329) / (2 * 3329)) % (1 << d)
    }

    #[test]
    fn compress_matches_exact_rounding_for_every_coefficient() {
        for value in -3328..=3328i16 {
            assert_eq!(u32::from(compress_1(value)), exact(i32::from(value), 1));
            assert_eq!(u32::from(compress_4(value)), exact(i32::from(value), 4));
            assert_eq!(u32::from(compress_10(value)), exact(i32::from(value), 10));
        }
    }

    // FIPS 203 section 4.2.1 by what it says rather than by a formula: the x in Z_q
    // nearest to (q / 2^d) * y, a tie going up, found by trying them all. The section's
    // identity Compress_d(Decompress_d(y)) = y then ties it to `compress`.
    #[test]
    fn decompress_is_the_nearest_element_of_z_q() {
        for d in [4u32, 10] {
            for y in 0..1u32 << d {
                let distance = |x: i64| (x * (1 << d) - i64::from(y) * 3329).abs();
                let nearest = (0..3329i64).rev().min_by_key(|x| distance(*x)).unwrap();
                let got = decompress(y, d);
                assert_eq!(i64::from(got), nearest, "{d} {y}");
                let back = if d == 4 {
                    u32::from(compress_4(got))
                } else {
                    u32::from(compress_10(got))
                };
                assert_eq!(back, y, "{d} {y}");
            }
        }
    }

    #[test]
    fn zetas_are_the_bit_reversed_powers_of_17() {
        let plain = |zeta: i16| i32::from(fqmul(zeta, 1)).rem_euclid(3329);
        for (index, zeta) in ZETAS.iter().enumerate() {
            let reversed = (index as u8).reverse_bits() >> 1;
            let mut expected = 1i32;
            for _ in 0..reversed {
                expected = expected * 17 % 3329;
            }
            assert_eq!(plain(*zeta), expected, "{index}");
        }
    }

    #[test]
    fn montgomery_constants_are_right() {
        assert_eq!((i32::from(Q) * i32::from(QINV)).rem_euclid(1 << 16), 1);
        assert_eq!(
            i32::from(MONT_SQUARED),
            (1i64 << 32).rem_euclid(3329) as i32
        );
        let scale = (i64::from(INVERSE_NTT_SCALE) * 128).rem_euclid(3329);
        assert_eq!(scale, (1i64 << 32).rem_euclid(3329));
    }

    #[test]
    fn barrett_centers_every_i16() {
        for value in i16::MIN..=i16::MAX {
            let reduced = barrett_reduce(value);
            assert!((-1664..=1664).contains(&reduced), "{value}");
            assert_eq!(
                (i32::from(value) - i32::from(reduced)).rem_euclid(3329),
                0,
                "{value}"
            );
        }
    }

    #[test]
    fn montgomery_stays_in_range_at_extremes() {
        for (a, b) in [
            (i16::MIN, i16::MIN),
            (i16::MAX, i16::MIN),
            (i16::MAX, i16::MAX),
        ] {
            let out = fqmul(a, b);
            let lhs = (i64::from(out) * 65536).rem_euclid(3329);
            assert_eq!(lhs, (i64::from(a) * i64::from(b)).rem_euclid(3329));
        }
    }

    #[test]
    fn ntt_round_trips() {
        let mut poly = [0i16; N];
        for (index, value) in poly.iter_mut().enumerate() {
            *value = (index as i16 * 13) % 3329 - 1664;
        }
        let original = poly;
        ntt(&mut poly);
        inverse_ntt(&mut poly);
        // inverse_ntt leaves one Montgomery factor for basemul's removed one.
        for (out, value) in poly.iter().zip(original) {
            let expected = (i32::from(value) * 65536).rem_euclid(3329);
            assert_eq!(i32::from(*out).rem_euclid(3329), expected);
        }
    }

    #[test]
    fn cbd_has_the_range_of_eta_2() {
        for nonce in 0..8 {
            let poly = sample_cbd(&[nonce; 32], nonce);
            assert!(poly.iter().all(|c| (-2..=2).contains(c)));
        }
    }

    #[test]
    fn hostile_dk_coefficients_do_not_panic() {
        let (_, mut dk) = keygen(&[1; 32], &[2; 32]);
        // All of s and t-hat at the 12-bit maximum, with a matching H(ek).
        dk[..VEC_BYTES].fill(0xff);
        dk[VEC_BYTES..2 * VEC_BYTES].fill(0xff);
        let h = sha3_256(&dk[VEC_BYTES..VEC_BYTES + EK_LEN]);
        dk[VEC_BYTES + EK_LEN..VEC_BYTES + EK_LEN + 32].copy_from_slice(&h);
        assert!(decaps(&dk, &[0xa5; CT_LEN]).is_some());
    }
}

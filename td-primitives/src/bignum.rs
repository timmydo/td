//! Little-endian multi-limb arithmetic, the one Montgomery multiplication the
//! curves and RSA share. Nothing here branches on a limb's value: selections
//! are masks, and only the lengths and the public moduli steer it.

use std::cell::Cell;
use std::hint::black_box;

/// `value == 0` as all ones or zero, without a branch.
pub(crate) fn zero_mask(words: &[u64]) -> u64 {
    let any = words.iter().fold(0, |sum, word| sum | word);
    black_box(((any | any.wrapping_neg()) >> 63).wrapping_sub(1))
}

#[inline]
pub(crate) fn select(out: &mut [u64], other: &[u64], mask: u64) {
    let mask = black_box(mask);
    for (word, replacement) in out.iter_mut().zip(other) {
        *word = (*word & !mask) | (replacement & mask);
    }
}

#[inline]
fn subtract_in_place(out: &mut [u64], other: &[u64], mask: u64) -> u64 {
    let mut borrow = 0u64;
    for (word, take) in out.iter_mut().zip(other) {
        let (first, under) = word.overflowing_sub(take & mask);
        let (second, under_again) = first.overflowing_sub(borrow);
        *word = second;
        borrow = u64::from(under) | u64::from(under_again);
    }
    borrow
}

/// All ones when `a < b`.
#[inline]
pub(crate) fn less_mask(a: &[u64], b: &[u64]) -> u64 {
    let mut borrow = 0u64;
    for (word, take) in a.iter().zip(b) {
        let (first, under) = word.overflowing_sub(*take);
        let (_, under_again) = first.overflowing_sub(borrow);
        borrow = u64::from(under) | u64::from(under_again);
    }
    black_box(borrow.wrapping_neg())
}

/// Subtract `m` once when `carry` is set or `out >= m`. `out` then holds the
/// value below `m`, given that `carry:out < 2m`.
#[inline]
fn reduce_once(out: &mut [u64], m: &[u64], carry: u64) {
    let below = less_mask(out, m);
    let subtract = black_box(carry.wrapping_neg() | !below);
    subtract_in_place(out, m, subtract);
}

/// out = a + b mod m, for a and b below m.
#[inline]
pub(crate) fn add_mod(out: &mut [u64], a: &[u64], b: &[u64], m: &[u64]) {
    let mut carry = 0u64;
    for ((word, x), y) in out.iter_mut().zip(a).zip(b) {
        let (first, over) = x.overflowing_add(*y);
        let (second, over_again) = first.overflowing_add(carry);
        *word = second;
        carry = u64::from(over) | u64::from(over_again);
    }
    reduce_once(out, m, carry);
}

/// out = a - b mod m, for a and b below m.
#[inline]
pub(crate) fn sub_mod(out: &mut [u64], a: &[u64], b: &[u64], m: &[u64]) {
    for (word, x) in out.iter_mut().zip(a) {
        *word = *x;
    }
    let borrow = subtract_in_place(out, b, u64::MAX);
    let mask = black_box(borrow.wrapping_neg());
    let mut carry = 0u64;
    for (word, add) in out.iter_mut().zip(m) {
        let (first, over) = word.overflowing_add(add & mask);
        let (second, over_again) = first.overflowing_add(carry);
        *word = second;
        carry = u64::from(over) | u64::from(over_again);
    }
}

/// -m^-1 mod 2^64 for an odd `m`, by Newton's iteration.
pub(crate) fn negated_inverse(m0: u64) -> u64 {
    let mut inverse = 1u64;
    for _ in 0..6 {
        inverse = inverse.wrapping_mul(2u64.wrapping_sub(m0.wrapping_mul(inverse)));
    }
    inverse.wrapping_neg()
}

/// Montgomery multiplication, the two passes of each word folded into one: out =
/// a * b / 2^(64n) mod m, for an odd `m`, `a` below 2^(64n) and `b` below `m`, all
/// of n limbs. It takes `a` unreduced so that a value can be brought below `m` by
/// multiplying it by R^2 and 1.
#[inline]
pub(crate) fn mont_mul(out: &mut [u64], a: &[u64], b: &[u64], m: &[u64], n0: u64) {
    out.fill(0);
    let (Some(&b0), Some(&m0)) = (b.first(), m.first()) else {
        return;
    };
    let mut high = 0u64;
    {
        // Each word's result lands one limb down: the division by 2^64.
        let cells = Cell::from_mut(&mut *out).as_slice_of_cells();
        let (Some(first), Some(last)) = (cells.first(), cells.last()) else {
            return;
        };
        for &multiplier in a {
            let low = u128::from(first.get()) + u128::from(multiplier) * u128::from(b0);
            let quotient = (low as u64).wrapping_mul(n0);
            // Its low limb is zero by the choice of quotient.
            let cleared = u128::from(low as u64) + u128::from(quotient) * u128::from(m0);
            let mut carry_b = (low >> 64) as u64;
            let mut carry_m = (cleared >> 64) as u64;
            for (pair, (y, z)) in cells.windows(2).zip(b.iter().zip(m).skip(1)) {
                let [below, word] = pair else {
                    continue;
                };
                let sum = u128::from(word.get())
                    + u128::from(multiplier) * u128::from(*y)
                    + u128::from(carry_b);
                let reduced = u128::from(sum as u64)
                    + u128::from(quotient) * u128::from(*z)
                    + u128::from(carry_m);
                below.set(reduced as u64);
                carry_b = (sum >> 64) as u64;
                carry_m = (reduced >> 64) as u64;
            }
            let sum = u128::from(high) + u128::from(carry_b) + u128::from(carry_m);
            last.set(sum as u64);
            high = (sum >> 64) as u64;
        }
    }
    reduce_once(out, m, high);
}

/// Big-endian bytes of exactly 8n length into n limbs.
pub(crate) fn from_be<const N: usize>(bytes: &[u8]) -> Option<[u64; N]> {
    let (chunks, rest) = bytes.as_chunks::<8>();
    if chunks.len() != N || !rest.is_empty() {
        return None;
    }
    let mut out = [0u64; N];
    for (word, chunk) in out.iter_mut().rev().zip(chunks) {
        *word = u64::from_be_bytes(*chunk);
    }
    Some(out)
}

/// `out.len()` big-endian bytes of the low limbs.
pub(crate) fn to_be(words: &[u64], out: &mut [u8]) {
    let mut limbs = words.iter();
    for chunk in out.rchunks_mut(8) {
        let bytes = limbs.next().copied().unwrap_or_default().to_be_bytes();
        for (byte, value) in chunk.iter_mut().rev().zip(bytes.iter().rev()) {
            *byte = *value;
        }
    }
}

/// A public odd modulus of fixed size, with its Montgomery constants.
#[derive(Clone, Copy)]
pub(crate) struct Modulus<const N: usize> {
    pub(crate) m: [u64; N],
    n0: u64,
    /// R mod m, which is 1 in Montgomery form.
    pub(crate) one: [u64; N],
    r2: [u64; N],
}

impl<const N: usize> Modulus<N> {
    pub(crate) fn new(m: [u64; N]) -> Self {
        let n0 = negated_inverse(m.first().copied().unwrap_or_default());
        let mut value = [0u64; N];
        if let Some(low) = value.first_mut() {
            *low = 1;
        }
        let bits = 64 * N;
        let double = |value: &mut [u64; N]| {
            let copy = *value;
            add_mod(value, &copy, &copy, &m);
        };
        for _ in 0..bits {
            double(&mut value);
        }
        let one = value;
        for _ in 0..bits {
            double(&mut value);
        }
        Self {
            m,
            n0,
            one,
            r2: value,
        }
    }

    #[inline]
    pub(crate) fn mul(&self, a: &[u64; N], b: &[u64; N]) -> [u64; N] {
        let mut out = [0u64; N];
        mont_mul(&mut out, a, b, &self.m, self.n0);
        out
    }

    #[inline]
    pub(crate) fn add(&self, a: &[u64; N], b: &[u64; N]) -> [u64; N] {
        let mut out = [0u64; N];
        add_mod(&mut out, a, b, &self.m);
        out
    }

    #[inline]
    pub(crate) fn sub(&self, a: &[u64; N], b: &[u64; N]) -> [u64; N] {
        let mut out = [0u64; N];
        sub_mod(&mut out, a, b, &self.m);
        out
    }

    /// Into Montgomery form, from any value of N limbs.
    pub(crate) fn enter(&self, a: &[u64; N]) -> [u64; N] {
        self.mul(a, &self.r2)
    }

    /// Out of Montgomery form.
    pub(crate) fn leave(&self, a: &[u64; N]) -> [u64; N] {
        let mut unit = [0u64; N];
        if let Some(low) = unit.first_mut() {
            *low = 1;
        }
        self.mul(a, &unit)
    }

    /// base^(m-2) in Montgomery form: the inverse, for a prime `m`.
    pub(crate) fn invert(&self, base: &[u64; N]) -> [u64; N] {
        let mut exponent = self.m;
        let mut two = [0u64; N];
        if let Some(low) = two.first_mut() {
            *low = 2;
        }
        subtract_in_place(&mut exponent, &two, u64::MAX);
        self.pow(base, &exponent)
    }

    /// base^exponent in Montgomery form. The exponent is public, so branching
    /// on its bits leaks nothing of `base`.
    pub(crate) fn pow(&self, base: &[u64; N], exponent: &[u64; N]) -> [u64; N] {
        let mut result = self.one;
        for word in exponent.iter().rev() {
            for bit in (0..64).rev() {
                result = self.mul(&result, &result);
                if (word >> bit) & 1 == 1 {
                    result = self.mul(&result, base);
                }
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::indexing_slicing)]
    use super::{from_be, less_mask, negated_inverse, select, to_be, zero_mask, Modulus};

    const P: [u64; 2] = [0xffffffffffffff61, 0xffffffffffffffff]; // 2^128 - 159, prime

    fn value(words: &[u64; 2]) -> u128 {
        u128::from(words[0]) | (u128::from(words[1]) << 64)
    }

    fn words(value: u128) -> [u64; 2] {
        [value as u64, (value >> 64) as u64]
    }

    /// Schoolbook multiplication mod 2^128 - 159, an oracle sharing no code.
    fn oracle_mul(a: u128, b: u128) -> u128 {
        let p = value(&P);
        let (mut result, mut base, mut factor) = (0u128, a % p, b);
        let add = |x: u128, y: u128| {
            let (sum, over) = x.overflowing_add(y);
            if over || sum >= p {
                sum.wrapping_sub(p)
            } else {
                sum
            }
        };
        while factor != 0 {
            if factor & 1 == 1 {
                result = add(result, base);
            }
            base = add(base, base);
            factor >>= 1;
        }
        result
    }

    fn samples() -> Vec<u128> {
        let p = value(&P);
        let mut out = vec![0, 1, 2, p - 1, p - 2, p / 2, 1 << 64, (1 << 64) - 1];
        let mut state = 88172645463325252u128;
        for _ in 0..64 {
            state = state.wrapping_mul(0x2545f4914f6cdd1d_1234567890abcdef) ^ (state >> 37);
            out.push(state % p);
        }
        out
    }

    #[test]
    fn modular_operations_agree_with_an_oracle() {
        let modulus = Modulus::new(P);
        let p = value(&P);
        for &a in &samples() {
            for &b in &samples() {
                let (x, y) = (words(a), words(b));
                let product = modulus.leave(&modulus.mul(&modulus.enter(&x), &modulus.enter(&y)));
                assert_eq!(value(&product), oracle_mul(a, b), "{a} * {b}");
                // a and b are below p, so a sum past u128 is below 2p.
                let sum = match a.overflowing_add(b) {
                    (sum, false) => sum % p,
                    (sum, true) => sum.wrapping_sub(p),
                };
                assert_eq!(value(&modulus.add(&x, &y)), sum, "{a} + {b}");
                let difference = if a >= b { a - b } else { p - (b - a) };
                assert_eq!(value(&modulus.sub(&x, &y)), difference, "{a} - {b}");
            }
        }
    }

    #[test]
    fn enter_reduces_a_value_above_the_modulus() {
        let modulus = Modulus::new(P);
        let above = [u64::MAX, u64::MAX];
        assert_eq!(
            value(&modulus.leave(&modulus.enter(&above))),
            u128::MAX % value(&P)
        );
    }

    #[test]
    fn inversion_inverts() {
        let modulus = Modulus::new(P);
        for &a in samples().iter().filter(|&&a| a != 0) {
            let entered = modulus.enter(&words(a));
            let product = modulus.mul(&entered, &modulus.invert(&entered));
            assert_eq!(product, modulus.one, "{a}");
        }
    }

    #[test]
    fn newton_inverse_is_the_negated_inverse() {
        for m in [1u64, 3, 0xffffffffffffffc5, 0x5812631a5cf5d3ed] {
            assert_eq!(m.wrapping_mul(negated_inverse(m)), u64::MAX);
        }
    }

    #[test]
    fn masks_and_selection() {
        assert_eq!(zero_mask(&[0, 0]), u64::MAX);
        assert_eq!(zero_mask(&[0, 1 << 63]), 0);
        assert_eq!(zero_mask(&[1, 0]), 0);
        assert_eq!(less_mask(&[1, 0], &[2, 0]), u64::MAX);
        assert_eq!(less_mask(&[2, 0], &[2, 0]), 0);
        assert_eq!(less_mask(&[0, 1], &[u64::MAX, 0]), 0);
        let mut out = [1u64, 2];
        select(&mut out, &[3, 4], 0);
        assert_eq!(out, [1, 2]);
        select(&mut out, &[3, 4], u64::MAX);
        assert_eq!(out, [3, 4]);
    }

    #[test]
    fn bytes_round_trip_big_endian() {
        let bytes: Vec<u8> = (1..=16).collect();
        let limbs: [u64; 2] = from_be(&bytes).unwrap_or_default();
        assert_eq!(limbs, [0x090a0b0c0d0e0f10, 0x0102030405060708]);
        let mut out = [0u8; 16];
        to_be(&limbs, &mut out);
        assert_eq!(out.to_vec(), bytes);
        assert!(from_be::<2>(&bytes[1..]).is_none());
    }
}

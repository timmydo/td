//! Comparison and erasure. Both are best effort in safe Rust: `black_box`
//! discourages the optimizer and promises nothing, which DESIGN.md records.

use std::hint::black_box;

/// Equality reading every byte of equal-length inputs. Lengths are public.
pub fn eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let difference = a.iter().zip(b).fold(0u8, |sum, (x, y)| sum | (x ^ y));
    black_box(difference) == 0
}

pub fn zero(bytes: &mut [u8]) {
    bytes.fill(0);
    black_box(bytes);
}

pub(crate) fn zero_words(words: &mut [u64]) {
    words.fill(0);
    black_box(words);
}

#[cfg(test)]
mod tests {
    #![allow(clippy::indexing_slicing)]
    use super::{eq, zero};

    #[test]
    fn equality_sees_every_byte_and_the_length() {
        let a = [7u8; 32];
        assert!(eq(&a, &a));
        for at in 0..a.len() {
            let mut b = a;
            b[at] ^= 0x80;
            assert!(!eq(&a, &b), "difference at {at}");
        }
        assert!(!eq(&a, &a[..31]));
        assert!(eq(&[], &[]));
    }

    #[test]
    fn zero_clears_the_buffer() {
        let mut a = [7u8; 9];
        zero(&mut a);
        assert_eq!(a, [0; 9]);
    }
}

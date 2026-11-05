//! Entropy, as a plain read of `/dev/random`, which needs neither `unsafe` nor
//! a syscall surface. Since Linux 5.6 it blocks only until the kernel's CRNG is
//! initialized and never afterwards, where `/dev/urandom` would answer before
//! that. A failed read is an error, never a fallback.

use crate::Refused;
use std::fs::File;
use std::io::Read;

pub fn fill(out: &mut [u8]) -> Result<(), Refused> {
    File::open("/dev/random")
        .and_then(|mut device| device.read_exact(out))
        .map_err(|_| Refused)
}

#[cfg(test)]
mod tests {
    use super::fill;

    #[test]
    fn two_draws_differ() {
        let (mut a, mut b) = ([0u8; 32], [0u8; 32]);
        assert_eq!(fill(&mut a), Ok(()));
        assert_eq!(fill(&mut b), Ok(()));
        assert_ne!(a, b);
        assert_ne!(a, [0; 32]);
    }
}

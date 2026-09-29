//! Opaque randomness over the private admitted provider.
use crate::{Entropy, Error};
use std::{marker::PhantomData, rc::Rc};

/// A handle initialized on its owning worker by a nonempty random fill.
/// Native state is thread-local and remains alive after this handle is dropped.
///
/// The warmed handle cannot move to another thread:
/// ```compile_fail,E0277
/// if let Ok(entropy) = td_crypto::SystemEntropy::try_new() {
///     std::thread::spawn(move || drop(entropy));
/// }
/// ```
/// It also cannot be shared between threads:
/// ```compile_fail,E0277
/// fn require_sync<T: Sync>() {}
/// require_sync::<td_crypto::SystemEntropy>();
/// ```
pub struct SystemEntropy {
    _thread: PhantomData<Rc<()>>,
}

impl SystemEntropy {
    /// Cold initialization on the worker that will consume randomness.
    /// Native failures may abort; returned failures use the fixed error.
    pub fn try_new() -> Result<Self, Error> {
        Self::create(aws_lc_rs::rand::fill)
    }

    fn create(
        fill: impl FnOnce(&mut [u8]) -> Result<(), aws_lc_rs::error::Unspecified>,
    ) -> Result<Self, Error> {
        fill_with(&mut [0; 1], fill)?;
        Ok(Self {
            _thread: PhantomData,
        })
    }
}

impl std::fmt::Debug for SystemEntropy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SystemEntropy(<redacted>)")
    }
}

// The admitted non-FIPS Rust path directly invokes RAND_bytes and maps its
// status. Native failures abort; this only handles an ordinary error return.
fn fill_with(
    output: &mut [u8],
    fill: impl FnOnce(&mut [u8]) -> Result<(), aws_lc_rs::error::Unspecified>,
) -> Result<(), Error> {
    if fill(output).is_err() {
        output.fill(0);
        return Err(Error::Entropy);
    }
    Ok(())
}

impl Entropy for SystemEntropy {
    fn fill(&mut self, output: &mut [u8]) -> Result<(), Error> {
        fill_with(output, aws_lc_rs::rand::fill)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn local_randomness_smoke() -> Result<(), Error> {
        let mut entropy = SystemEntropy::try_new()?;
        entropy.fill(&mut [])?;
        let mut first = [0xa5; 66];
        let mut second = [0xa5; 64];
        entropy.fill(first.get_mut(1..65).ok_or(Error::Capacity)?)?;
        entropy.fill(&mut second)?;
        assert_eq!(first.first(), Some(&0xa5));
        assert_eq!(first.last(), Some(&0xa5));
        // Sample inequality catches no-op/constant-output wiring, not RNG quality.
        assert_ne!(first.get(1..65), Some(second.as_slice()));
        assert_eq!(format!("{entropy:?}"), "SystemEntropy(<redacted>)");
        Ok(())
    }

    #[test]
    fn construction_requires_nonempty_successful_initialization() {
        let mut called = false;
        let entropy = SystemEntropy::create(|bytes| {
            called = true;
            assert_eq!(bytes.len(), 1);
            bytes.fill(0x42);
            Ok(())
        })
        .unwrap();
        assert!(called);
        assert_eq!(format!("{entropy:?}"), "SystemEntropy(<redacted>)");
        assert!(matches!(
            SystemEntropy::create(|bytes| {
                bytes.fill(0x42);
                Err(aws_lc_rs::error::Unspecified)
            }),
            Err(Error::Entropy)
        ));
    }

    #[test]
    fn synthetic_partial_error_clears_the_entire_caller_slice() {
        for length in [0, 1, 64] {
            let mut output = [0xa5; 66];
            let target = output.get_mut(1..1 + length).unwrap();
            assert_eq!(
                fill_with(target, |bytes| {
                    for byte in bytes.iter_mut().take(3) {
                        *byte = 0x42;
                    }
                    Err(aws_lc_rs::error::Unspecified)
                }),
                Err(Error::Entropy)
            );
            assert!(output.get(1..1 + length).unwrap().iter().all(|&b| b == 0));
            assert_eq!(output.first(), Some(&0xa5));
            assert!(output.get(1 + length..).unwrap().iter().all(|&b| b == 0xa5));
        }
        let mut output = [0xa5; 64];
        assert_eq!(
            fill_with(&mut output, |bytes| {
                bytes.fill(0x42);
                Ok(())
            }),
            Ok(())
        );
        assert_eq!(output, [0x42; 64]);
    }
}

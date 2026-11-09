//! Opaque randomness from td-primitives' `/dev/random` reader.
use crate::{Entropy, Error};
use std::{marker::PhantomData, rc::Rc};
use td_primitives::Refused;

/// A handle initialized on its owning worker by a nonempty random fill.
/// It holds no state; each fill reads the kernel's random device afresh.
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
    /// It waits until the kernel's random generator is initialized.
    pub fn try_new() -> Result<Self, Error> {
        Self::create(td_primitives::random::fill)
    }

    fn create(fill: impl FnOnce(&mut [u8]) -> Result<(), Refused>) -> Result<Self, Error> {
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

// An open or read failure, a short read among them, is the reader's refusal.
// An empty fill succeeds without calling the reader, so opens no device.
fn fill_with(
    output: &mut [u8],
    fill: impl FnOnce(&mut [u8]) -> Result<(), Refused>,
) -> Result<(), Error> {
    if output.is_empty() {
        return Ok(());
    }
    if fill(output).is_err() {
        output.fill(0);
        return Err(Error::Entropy);
    }
    Ok(())
}

impl Entropy for SystemEntropy {
    fn fill(&mut self, output: &mut [u8]) -> Result<(), Error> {
        fill_with(output, td_primitives::random::fill)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
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
                Err(Refused)
            }),
            Err(Error::Entropy)
        ));
    }

    #[test]
    fn synthetic_partial_error_clears_the_entire_caller_slice() {
        assert_eq!(
            fill_with(&mut [], |_| panic!("an empty fill reads nothing")),
            Ok(())
        );
        for length in [1, 64] {
            let mut output = [0xa5; 66];
            let target = output.get_mut(1..1 + length).unwrap();
            assert_eq!(
                fill_with(target, |bytes| {
                    for byte in bytes.iter_mut().take(3) {
                        *byte = 0x42;
                    }
                    Err(Refused)
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

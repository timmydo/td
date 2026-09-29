//! Opaque streaming SHA-256 over the private, admitted provider.
use crate::{Digest, Error};
use aws_lc_rs::digest::Context;
use std::panic::{catch_unwind, UnwindSafe};

const MAX_INPUT_BYTES: u64 = u64::MAX / 8;

/// A digest operation; initialization is cold and may allocate in the provider.
/// Any update failure retires the operation. No clone or reset is exposed.
pub struct Sha256 {
    context: Option<Context>,
    bytes: u64,
}

impl std::fmt::Debug for Sha256 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Sha256(<redacted>)")
    }
}

// The pinned provider's digest entry points expose fixed-message Rust panics.
// Its context has returned from C before unwinding; discard it on failure.
// Hooks still run. This cannot contain native aborts, OOM or hostile hooks.
fn provider<T>(operation: impl FnOnce() -> T + UnwindSafe) -> Result<T, Error> {
    catch_unwind(operation).map_err(|_| Error::Crypto)
}

impl Sha256 {
    pub fn try_new() -> Result<Self, Error> {
        Self::create(|| Context::new(&aws_lc_rs::digest::SHA256))
    }

    fn create(make: impl FnOnce() -> Context + UnwindSafe) -> Result<Self, Error> {
        Ok(Self {
            context: Some(provider(make)?),
            bytes: 0,
        })
    }

    fn absorb(
        &mut self,
        bytes: &[u8],
        operation: impl FnOnce(Context, &[u8]) -> Context + UnwindSafe,
    ) -> Result<(), Error> {
        let context = self.context.take().ok_or(Error::Crypto)?;
        let total = u64::try_from(bytes.len())
            .ok()
            .and_then(|count| self.bytes.checked_add(count))
            .filter(|&count| count <= MAX_INPUT_BYTES)
            .ok_or(Error::Crypto)?;
        self.context = Some(provider(move || operation(context, bytes))?);
        self.bytes = total;
        Ok(())
    }

    fn complete(
        mut self,
        operation: impl FnOnce(Context) -> aws_lc_rs::digest::Digest + UnwindSafe,
    ) -> Result<[u8; 32], Error> {
        let context = self.context.take().ok_or(Error::Crypto)?;
        let digest = provider(move || operation(context))?;
        digest.as_ref().try_into().map_err(|_| Error::Crypto)
    }
}

impl Digest for Sha256 {
    fn update(&mut self, bytes: &[u8]) -> Result<(), Error> {
        self.absorb(bytes, |mut context, bytes| {
            context.update(bytes);
            context
        })
    }

    fn finish(self) -> Result<[u8; 32], Error> {
        self.complete(Context::finish)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    // Existing engine/src/sha256.rs FIPS 180-4/CAVP fixture literals.
    #[test]
    fn known_answers_and_fragmented_updates() {
        for (input, expected) in [
            (
                b"".as_slice(),
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            ),
            (
                b"abc".as_slice(),
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            ),
            (
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq".as_slice(),
                "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1",
            ),
        ] {
            for chunk in [1, 2, 3, 7, 31, 63, 64, 65] {
                let mut digest = Sha256::try_new().unwrap();
                digest.update(b"").unwrap();
                for bytes in input.chunks(chunk) {
                    digest.update(bytes).unwrap();
                }
                assert_eq!(hex(&digest.finish().unwrap()), expected);
            }
        }
        for chunk in [63, 64, 65, 1000] {
            let block = vec![b'a'; chunk];
            let mut remaining = 1_000_000;
            let mut digest = Sha256::try_new().unwrap();
            while remaining != 0 {
                let count = remaining.min(chunk);
                digest.update(block.get(..count).unwrap()).unwrap();
                remaining -= count;
            }
            assert_eq!(
                hex(&digest.finish().unwrap()),
                "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
            );
        }
    }

    #[test]
    fn initialization_update_and_finalization_unwinds_return_fixed_failure() {
        assert!(matches!(
            Sha256::create(|| panic!("fixture init failure")),
            Err(Error::Crypto)
        ));
        let mut digest = Sha256::try_new().unwrap();
        digest.update(b"prefix").unwrap();
        assert_eq!(
            digest.absorb(b"suffix", |_, _| panic!("fixture update failure")),
            Err(Error::Crypto)
        );
        assert_eq!(digest.update(b"retry"), Err(Error::Crypto));
        assert_eq!(digest.finish(), Err(Error::Crypto));
        let digest = Sha256::try_new().unwrap();
        assert_eq!(
            digest.complete(|_| panic!("fixture finish failure")),
            Err(Error::Crypto)
        );
    }

    #[test]
    fn length_refusal_retires_state_before_provider_update() {
        // Exercise the counter boundary without pretending to hash exabytes.
        let mut digest = Sha256::try_new().unwrap();
        digest.bytes = MAX_INPUT_BYTES - 1;
        digest.update(b"x").unwrap();
        assert_eq!(digest.bytes, MAX_INPUT_BYTES);
        digest.update(b"").unwrap();
        assert!(digest.finish().is_ok());
        for count in [MAX_INPUT_BYTES, u64::MAX] {
            let mut digest = Sha256::try_new().unwrap();
            digest.bytes = count;
            let called = std::sync::atomic::AtomicBool::new(false);
            assert_eq!(
                digest.absorb(b"x", |context, _| {
                    called.store(true, std::sync::atomic::Ordering::Relaxed);
                    context
                }),
                Err(Error::Crypto)
            );
            assert!(!called.load(std::sync::atomic::Ordering::Relaxed));
            assert_eq!(digest.update(b""), Err(Error::Crypto));
            assert_eq!(digest.finish(), Err(Error::Crypto));
        }
    }
}

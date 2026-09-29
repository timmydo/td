//! Opaque P-256 key ownership and the concrete crypto factory.
use crate::{pkcs8, Crypto, Error, Sha256};
use aws_lc_rs::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
use std::{
    panic::{catch_unwind, UnwindSafe},
    sync::Mutex,
};

/// Stateless access to the selected private backend. This does not warm workers.
#[derive(Clone, Copy, Default)]
pub struct Provider;

/// Required generation capacity and maximum accepted P-256 PKCS#8 input length.
pub const P256_PKCS8_CAPACITY: usize = pkcs8::MAX_LEN;

/// An opaque key. Operations serialize; an operation failure retires the key.
/// Native key state and signing scratch allocations are not hot-path qualified.
pub struct P256Key {
    key: Mutex<Option<EcdsaKeyPair>>,
}

impl std::fmt::Debug for P256Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("P256Key(<redacted>)")
    }
}
impl std::fmt::Debug for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Provider")
    }
}

// Hold native contexts inside the unwind boundary and never return partial
// operations. Hooks still run; native abort, blocking and OOM are not contained.
fn provider<T>(operation: impl FnOnce() -> Result<T, Error> + UnwindSafe) -> Result<T, Error> {
    catch_unwind(operation).map_err(|_| Error::Crypto)?
}

impl P256Key {
    fn operate<T>(
        &self,
        operation: impl FnOnce(&EcdsaKeyPair) -> Result<T, Error> + UnwindSafe,
    ) -> Result<T, Error> {
        let mut slot = match self.key.lock() {
            Ok(slot) => slot,
            Err(poisoned) => {
                poisoned.into_inner().take();
                return Err(Error::Crypto);
            }
        };
        let key = slot.take().ok_or(Error::Crypto)?;
        let (key, output) = provider(move || {
            let output = operation(&key)?;
            Ok((key, output))
        })?;
        *slot = Some(key);
        Ok(output)
    }
}

fn generate(
    output: &mut [u8],
    make: impl FnOnce() -> Result<aws_lc_rs::pkcs8::Document, Error> + UnwindSafe,
) -> Result<usize, Error> {
    if output.len() < P256_PKCS8_CAPACITY {
        return Err(Error::Capacity);
    }
    let document = provider(make)?;
    let bytes = document.as_ref();
    pkcs8::validate(bytes).map_err(|_| Error::Crypto)?;
    let target = output.get_mut(..bytes.len()).ok_or(Error::Capacity)?;
    target.copy_from_slice(bytes);
    Ok(bytes.len())
}

impl Crypto for Provider {
    type Sha256 = Sha256;
    type SigningKey = P256Key;

    fn sha256(&self) -> Result<Self::Sha256, Error> {
        Sha256::try_new()
    }

    fn equal_digest(&self, left: &[u8; 32], right: &[u8; 32]) -> bool {
        aws_lc_rs::constant_time::verify_slices_are_equal(left, right).is_ok()
    }

    fn generate_p256(&self, output: &mut [u8]) -> Result<usize, Error> {
        generate(output, || {
            EcdsaKeyPair::generate_pkcs8(
                &ECDSA_P256_SHA256_FIXED_SIGNING,
                &aws_lc_rs::rand::SystemRandom::new(),
            )
            .map_err(|_| Error::Crypto)
        })
    }

    fn load_p256(&self, bytes: &[u8]) -> Result<Self::SigningKey, Error> {
        pkcs8::validate(bytes)?;
        let key = provider(|| {
            EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, bytes)
                .map_err(|_| Error::Invalid)
        })?;
        Ok(P256Key {
            key: Mutex::new(Some(key)),
        })
    }

    fn p256_public(&self, key: &Self::SigningKey, output: &mut [u8; 65]) -> Result<(), Error> {
        let public = key.operate(|key| {
            let bytes: [u8; 65] = key
                .public_key()
                .as_ref()
                .try_into()
                .map_err(|_| Error::Crypto)?;
            if bytes.first() != Some(&4) {
                return Err(Error::Crypto);
            }
            Ok(bytes)
        })?;
        *output = public;
        Ok(())
    }

    fn sign_es256(
        &self,
        key: &Self::SigningKey,
        message: &[u8],
        output: &mut [u8; 64],
    ) -> Result<(), Error> {
        let signature = key.operate(|key| {
            if u64::try_from(message.len()).map_err(|_| Error::Crypto)? > u64::MAX / 8 {
                return Err(Error::Crypto);
            }
            let signature = key
                .sign(&aws_lc_rs::rand::SystemRandom::new(), message)
                .map_err(|_| Error::Crypto)?;
            signature.as_ref().try_into().map_err(|_| Error::Crypto)
        })?;
        *output = signature;
        Ok(())
    }
}

#[cfg(test)]
#[path = "provider_tests.rs"]
mod tests;

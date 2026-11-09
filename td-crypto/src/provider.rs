//! Opaque P-256 key ownership and the concrete crypto factory.
use crate::{pkcs8, Crypto, Error, Sha256};
use std::{
    panic::{catch_unwind, UnwindSafe},
    sync::Mutex,
};
use td_primitives::ec::p256;

/// Stateless access to td-primitives. This does not warm workers.
#[derive(Clone, Copy, Default)]
pub struct Provider;

/// Required generation capacity and maximum accepted P-256 PKCS#8 input length.
pub const P256_PKCS8_CAPACITY: usize = pkcs8::MAX_LEN;

/// Rejection sampling draws again while 32 random bytes are not below the order,
/// which happens about once in four billion draws.
const GENERATION_DRAWS: usize = 64;

/// An opaque key. Operations serialize; an operation failure retires the key.
/// Signing allocates its DER signature; it is not hot-path qualified.
pub struct P256Key {
    key: Mutex<Option<Owned>>,
}

/// The scalar, and the public point derived from it once at load.
struct Owned {
    secret: p256::SecretKey,
    public: [u8; 65],
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

// Hold key state inside the unwind boundary and never return partial
// operations. Hooks still run; abort, blocking and OOM are not contained.
fn provider<T>(operation: impl FnOnce() -> Result<T, Error> + UnwindSafe) -> Result<T, Error> {
    catch_unwind(operation).map_err(|_| Error::Crypto)?
}

impl Provider {
    /// Cold-load one unencrypted P-256 PKCS#8 PRIVATE KEY PEM block.
    /// Input is capped at 16 KiB; decoded key storage is fixed and cleared on
    /// return or Rust unwind. Caller input and compiler copies are not erased.
    /// This validates the key, not its correspondence to any certificate.
    pub fn load_p256_pem(&self, input: &[u8]) -> Result<P256Key, Error> {
        crate::pem::private_key(input, |der| self.load_p256(der))
    }
}

impl P256Key {
    pub(super) fn sign_es256_with<T>(
        &self,
        message: &[u8],
        transform: impl FnOnce(&[u8; 64]) -> Result<T, Error> + UnwindSafe,
    ) -> Result<T, Error> {
        self.operate(|key| {
            if u64::try_from(message.len()).map_err(|_| Error::Crypto)? > u64::MAX / 8 {
                return Err(Error::Crypto);
            }
            let der = key.secret.sign_sha256(message).map_err(|_| Error::Crypto)?;
            transform(&fixed_signature(&der)?)
        })
    }

    fn operate<T>(
        &self,
        operation: impl FnOnce(&Owned) -> Result<T, Error> + UnwindSafe,
    ) -> Result<T, Error> {
        let mut slot = match self.key.lock() {
            Ok(slot) => slot,
            Err(poisoned) => {
                // Dropped where it lies, so no scalar stays in the slot.
                *poisoned.into_inner() = None;
                return Err(Error::Crypto);
            }
        };
        // The key is used where it lies: moving it out and back would leave
        // stale copies of the scalar on the stack at each signature.
        let key = slot.as_ref().ok_or(Error::Crypto)?;
        let result = provider(|| operation(key));
        if result.is_err() {
            // Retire: dropping the key zeroes its scalar.
            *slot = None;
        }
        result
    }
}

/// td-primitives' DER signature, two positive minimal INTEGERs, as r || s.
fn fixed_signature(der: &[u8]) -> Result<[u8; 64], Error> {
    fn integer<'a>(input: &mut &'a [u8]) -> Result<&'a [u8], Error> {
        let [0x02, length, rest @ ..] = *input else {
            return Err(Error::Crypto);
        };
        let value = rest.get(..usize::from(*length)).ok_or(Error::Crypto)?;
        *input = rest.get(usize::from(*length)..).ok_or(Error::Crypto)?;
        let value = match value {
            [0, tail @ ..] if tail.first().is_some_and(|b| b & 0x80 != 0) => tail,
            [first, ..] if first & 0x80 == 0 && *first != 0 => value,
            _ => return Err(Error::Crypto),
        };
        if value.len() > 32 {
            return Err(Error::Crypto);
        }
        Ok(value)
    }
    let [0x30, length, ref rest @ ..] = *der else {
        return Err(Error::Crypto);
    };
    let mut body: &[u8] = rest;
    if body.len() != usize::from(length) {
        return Err(Error::Crypto);
    }
    let mut out = [0u8; 64];
    for half in 0..2 {
        let value = integer(&mut body)?;
        let end = 32 * (half + 1);
        let start = end - value.len();
        out.get_mut(start..end)
            .ok_or(Error::Crypto)?
            .copy_from_slice(value);
    }
    if !body.is_empty() {
        return Err(Error::Crypto);
    }
    Ok(out)
}

fn generate(
    output: &mut [u8],
    make: impl FnOnce() -> Result<[u8; pkcs8::GENERATED_LEN], Error> + UnwindSafe,
) -> Result<usize, Error> {
    if output.len() < P256_PKCS8_CAPACITY {
        return Err(Error::Capacity);
    }
    let mut document = provider(make)?;
    let result = pkcs8::validate(&document)
        .map_err(|_| Error::Crypto)
        .and_then(|()| {
            let target = output.get_mut(..document.len()).ok_or(Error::Capacity)?;
            target.copy_from_slice(&document);
            Ok(document.len())
        });
    td_primitives::ct::zero(&mut document);
    result
}

/// A fresh scalar by rejection sampling from `fill`, written as the
/// generated document. A fill failure is Entropy; exhausted draws are Crypto.
fn fresh_document(
    mut fill: impl FnMut(&mut [u8]) -> Result<(), td_primitives::Refused>,
) -> Result<[u8; pkcs8::GENERATED_LEN], Error> {
    let mut scalar = [0u8; 32];
    let result = (|| {
        for _ in 0..GENERATION_DRAWS {
            fill(&mut scalar).map_err(|_| Error::Entropy)?;
            if let Ok(secret) = p256::SecretKey::from_bytes(&scalar) {
                let public = secret.public_key().map_err(|_| Error::Crypto)?;
                return Ok(pkcs8::write(&scalar, &public));
            }
        }
        Err(Error::Crypto)
    })();
    td_primitives::ct::zero(&mut scalar);
    result
}

impl Crypto for Provider {
    type Sha256 = Sha256;
    type SigningKey = P256Key;

    fn sha256(&self) -> Result<Self::Sha256, Error> {
        Sha256::try_new()
    }

    fn equal_digest(&self, left: &[u8; 32], right: &[u8; 32]) -> bool {
        td_primitives::ct::eq(left, right)
    }

    fn generate_p256(&self, output: &mut [u8]) -> Result<usize, Error> {
        generate(output, || fresh_document(td_primitives::random::fill))
    }

    fn load_p256(&self, bytes: &[u8]) -> Result<Self::SigningKey, Error> {
        let parts = pkcs8::parse(bytes)?;
        let key = provider(|| {
            let secret = p256::SecretKey::from_bytes(parts.scalar).map_err(|_| Error::Invalid)?;
            let public = secret.public_key().map_err(|_| Error::Crypto)?;
            // The embedded point is public: an ordinary comparison suffices.
            if parts.public.is_some_and(|embedded| *embedded != public) {
                return Err(Error::Invalid);
            }
            Ok(Owned { secret, public })
        })?;
        Ok(P256Key {
            key: Mutex::new(Some(key)),
        })
    }

    fn p256_public(&self, key: &Self::SigningKey, output: &mut [u8; 65]) -> Result<(), Error> {
        let public = key.operate(|key| Ok(key.public))?;
        *output = public;
        Ok(())
    }

    fn sign_es256(
        &self,
        key: &Self::SigningKey,
        message: &[u8],
        output: &mut [u8; 64],
    ) -> Result<(), Error> {
        let signature = key.sign_es256_with(message, |signature| Ok(*signature))?;
        *output = signature;
        Ok(())
    }
}

#[cfg(test)]
#[path = "provider_tests.rs"]
mod tests;

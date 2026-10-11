//! The update-retry token's tag (ENCRYPTION.md "Selector update",
//! DESIGN.md "Protected roles"): a staged selector update whose prediction
//! failed although every authenticity check passed is retried at the next
//! PIN boot, and only a token whose tag verifies for the same staged pair
//! lets that retry offer to complete the update without a prediction. The
//! key is HKDF-SHA256 over the volume key with an empty salt and the info
//! `td/disk-protector/update-retry/v1`, a zero byte and the volume UUID;
//! the tag is HMAC-SHA256 under it over `pair`, the SHA-256 of the staged
//! kernel's SHA-256 followed by the staged initramfs's. Whoever can write
//! the ESP or the header cannot forge one without the volume key. Library
//! code: the installed selector's staged-update commit is its one caller.

use crate::hmac::{hkdf_expand, hmac_sha256};
use crate::lockout::VOLUME_UUID_LEN;
use crate::protected::{UpdateRetry, FIELD_LEN};
use crate::sha256::Sha256;

/// The HKDF info's prefix; a zero byte and the volume UUID follow it.
pub const UPDATE_RETRY_DOMAIN: &[u8] = b"td/disk-protector/update-retry/v1";

/// The update-retry key, zeroed on drop.
struct Key([u8; FIELD_LEN]);
impl Drop for Key {
    fn drop(&mut self) {
        td_tpm::zero(&mut self.0);
    }
}
impl Key {
    fn derive(volume_key: &[u8], uuid: &[u8; VOLUME_UUID_LEN]) -> Result<Self, String> {
        if volume_key.is_empty() {
            return Err("the volume key is empty".into());
        }
        let mut key = Self([0; FIELD_LEN]);
        hkdf_expand(
            volume_key,
            &[],
            &[UPDATE_RETRY_DOMAIN, &[0], uuid],
            &mut key.0,
        )?;
        Ok(key)
    }
}

/// The SHA-256 over the staged pair's two digests, kernel first.
pub fn pair(kernel: &[u8; 32], initrd: &[u8; 32]) -> [u8; FIELD_LEN] {
    let mut hash = Sha256::new();
    hash.update(kernel);
    hash.update(initrd);
    hash.finalize()
}

/// The token recording a retry of the staged pair whose files hash to
/// `kernel` and `initrd`, under this volume key and UUID.
pub fn token(
    volume_key: &[u8],
    uuid: &[u8; VOLUME_UUID_LEN],
    kernel: &[u8; 32],
    initrd: &[u8; 32],
) -> Result<UpdateRetry, String> {
    let key = Key::derive(volume_key, uuid)?;
    let pair = pair(kernel, initrd);
    Ok(UpdateRetry {
        pair,
        tag: hmac_sha256(&key.0, &[&pair]),
    })
}

/// Whether `token` records a retry of the staged pair hashing to `kernel`
/// and `initrd` on this volume: its pair is that pair's, and its tag
/// verifies under this volume key and UUID, compared in constant time.
pub fn verifies(
    volume_key: &[u8],
    uuid: &[u8; VOLUME_UUID_LEN],
    token: &UpdateRetry,
    kernel: &[u8; 32],
    initrd: &[u8; 32],
) -> bool {
    let Ok(expected) = self::token(volume_key, uuid, kernel, initrd) else {
        return false;
    };
    // Both compared, whichever fails first.
    let pair = td_tpm::equal(&token.pair, &expected.pair);
    let tag = td_tpm::equal(&token.tag, &expected.tag);
    pair & tag
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lockout::volume_uuid;
    use crate::protected::{fixtures, Token};

    // `tests/token_vectors.py`'s inputs and outputs.
    const UUID: &str = "0f0e0d0c-0b0a-4908-8706-050403020100";
    const PAIR: &str = "05483fb1d64a81bbee3bb71ea3becf9ee94b11fed3753a3bc74c0022f1990ee9";
    const TAG: &str = "12d6440af0b8aecb63b8d1889e42ff561e478ab367cd9fac0f9089342a4d3dee";

    fn volume_key() -> Vec<u8> {
        (0..64).collect()
    }

    fn uuid() -> [u8; VOLUME_UUID_LEN] {
        volume_uuid(UUID).unwrap()
    }

    fn kernel() -> [u8; 32] {
        core::array::from_fn(|at| 0x10 + at as u8)
    }

    fn initrd() -> [u8; 32] {
        core::array::from_fn(|at| 0x30 + at as u8)
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn the_token_is_the_vectors_and_verifies_only_for_its_pair_and_volume() {
        let token = token(&volume_key(), &uuid(), &kernel(), &initrd()).unwrap();
        assert_eq!(hex(&token.pair), PAIR);
        assert_eq!(hex(&token.tag), TAG);
        assert_eq!(Token::UpdateRetry(token.clone()), fixtures::update_retry());
        assert!(verifies(
            &volume_key(),
            &uuid(),
            &token,
            &kernel(),
            &initrd()
        ));
        // Another pair: either file, or the two swapped.
        let mut other = kernel();
        other[0] ^= 1;
        assert!(!verifies(&volume_key(), &uuid(), &token, &other, &initrd()));
        assert!(!verifies(&volume_key(), &uuid(), &token, &kernel(), &other));
        assert!(!verifies(
            &volume_key(),
            &uuid(),
            &token,
            &initrd(),
            &kernel()
        ));
        // Another volume: its key or its UUID.
        let mut key = volume_key();
        key[63] ^= 1;
        assert!(!verifies(&key, &uuid(), &token, &kernel(), &initrd()));
        let mut elsewhere = uuid();
        elsewhere[15] ^= 1;
        assert!(!verifies(
            &volume_key(),
            &elsewhere,
            &token,
            &kernel(),
            &initrd()
        ));
        // A forged tag, or a pair rewritten to match another staging.
        let mut forged = token.clone();
        forged.tag[0] ^= 1;
        assert!(!verifies(
            &volume_key(),
            &uuid(),
            &forged,
            &kernel(),
            &initrd()
        ));
        let mut moved = token.clone();
        moved.pair = pair(&other, &initrd());
        assert!(!verifies(&volume_key(), &uuid(), &moved, &other, &initrd()));
        // A tag that verifies beside a pair that is not the files'.
        let mut damaged = token.clone();
        damaged.pair[0] ^= 1;
        assert!(!verifies(
            &volume_key(),
            &uuid(),
            &damaged,
            &kernel(),
            &initrd()
        ));
        // No volume key derives nothing.
        assert!(super::token(&[], &uuid(), &kernel(), &initrd()).is_err());
        assert!(!verifies(&[], &uuid(), &token, &kernel(), &initrd()));
    }
}

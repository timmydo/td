//! The lockout token's encryption (ENCRYPTION.md "PIN and dictionary-attack
//! policy", DESIGN.md "Protected roles"): the lockout hierarchy's
//! authorization `L` XORed with a key HKDF-SHA256 derives from the volume
//! key, the volume UUID and a fresh nonce, authenticated by HMAC-SHA256
//! over the nonce, the ciphertext and the storage primary's Name. Library
//! code: nothing in production calls it before ENCRYPTION.md increment 8c.

use crate::hmac::{hkdf_expand, hmac_sha256};
use crate::pin::PRIMARY_NAME_LEN;
use crate::protected::{Lockout, FIELD_LEN};
use std::fmt;
use std::path::Path;
use td_tpm::LockoutAuth;

/// The HKDF info's prefix; a zero byte, the volume UUID and the nonce
/// follow it.
pub const LOCKOUT_DOMAIN: &[u8] = b"td/disk-protector/lockout/v1";
/// A LUKS2 UUID's bytes.
pub const VOLUME_UUID_LEN: usize = 16;
/// Blocks until the kernel CSPRNG is initialized: two writes under one
/// volume key must never share a nonce, whose key stream would then be
/// shared.
const NONCE_SOURCE: &str = "/dev/random";

/// Why `open` gave no authorization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenError {
    /// The tag does not verify under this volume key and UUID: another
    /// volume's token, or a forged or damaged one.
    Tag,
    /// The tag verifies but names another storage primary than this TPM's.
    OtherPrimary,
    /// The volume key is empty, or the decrypted authorization is one
    /// td-tpm refuses.
    Invalid(String),
}
impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tag => f.write_str("the lockout token's tag does not verify for this volume"),
            Self::OtherPrimary => {
                f.write_str("the lockout token names another TPM's storage primary")
            }
            Self::Invalid(error) => write!(f, "the lockout token: {error}"),
        }
    }
}
impl std::error::Error for OpenError {}

/// The 16 bytes of a LUKS2 UUID in its 36-character text form, as the
/// header carries it: 8-4-4-4-12 hexadecimal digits of either case.
pub fn volume_uuid(text: &str) -> Option<[u8; VOLUME_UUID_LEN]> {
    let text = text.as_bytes();
    if text.len() != 36 {
        return None;
    }
    let mut out = [0u8; VOLUME_UUID_LEN];
    let mut digits = text
        .iter()
        .enumerate()
        .filter(|(at, _)| !matches!(at, 8 | 13 | 18 | 23));
    for byte in &mut out {
        let mut value = 0u8;
        for _ in 0..2 {
            let (_, digit) = digits.next()?;
            value = value << 4
                | char::from(*digit)
                    .to_digit(16)
                    .and_then(|d| u8::try_from(d).ok())?;
        }
        *byte = value;
    }
    for at in [8, 13, 18, 23] {
        if text.get(at) != Some(&b'-') {
            return None;
        }
    }
    Some(out)
}

/// `k_enc` and `k_mac`, zeroed on drop.
struct Keys([u8; 2 * FIELD_LEN]);
impl Drop for Keys {
    fn drop(&mut self) {
        td_tpm::zero(&mut self.0);
    }
}
impl Keys {
    fn derive(
        volume_key: &[u8],
        uuid: &[u8; VOLUME_UUID_LEN],
        nonce: &[u8; FIELD_LEN],
    ) -> Result<Self, String> {
        if volume_key.is_empty() {
            return Err("the volume key is empty".into());
        }
        let mut keys = Self([0; 2 * FIELD_LEN]);
        hkdf_expand(
            volume_key,
            &[],
            &[LOCKOUT_DOMAIN, &[0], uuid, nonce],
            &mut keys.0,
        )?;
        Ok(keys)
    }

    fn enc(&self) -> &[u8] {
        self.0.get(..FIELD_LEN).unwrap_or_default()
    }

    fn tag(&self, token_nonce: &[u8], ciphertext: &[u8], primary: &[u8]) -> [u8; FIELD_LEN] {
        hmac_sha256(
            self.0.get(FIELD_LEN..).unwrap_or_default(),
            &[token_nonce, ciphertext, primary],
        )
    }
}

/// The lockout token for `auth` under this volume key and UUID, recorded
/// against the storage primary `primary`, with a fresh nonce from
/// `/dev/random`.
pub fn seal(
    volume_key: &[u8],
    uuid: &[u8; VOLUME_UUID_LEN],
    auth: &LockoutAuth,
    primary: &[u8; PRIMARY_NAME_LEN],
) -> Result<Lockout, String> {
    seal_from(Path::new(NONCE_SOURCE), volume_key, uuid, auth, primary)
}

fn seal_from(
    source: &Path,
    volume_key: &[u8],
    uuid: &[u8; VOLUME_UUID_LEN],
    auth: &LockoutAuth,
    primary: &[u8; PRIMARY_NAME_LEN],
) -> Result<Lockout, String> {
    let mut nonce = [0u8; FIELD_LEN];
    crate::read_random(source, &mut nonce, "lockout nonce")?;
    seal_with(volume_key, uuid, auth, primary, nonce)
}

/// `seal` with the nonce given.
fn seal_with(
    volume_key: &[u8],
    uuid: &[u8; VOLUME_UUID_LEN],
    auth: &LockoutAuth,
    primary: &[u8; PRIMARY_NAME_LEN],
    nonce: [u8; FIELD_LEN],
) -> Result<Lockout, String> {
    let keys = Keys::derive(volume_key, uuid, &nonce)?;
    let mut ciphertext = [0u8; FIELD_LEN];
    for ((out, plain), key) in ciphertext.iter_mut().zip(auth.bytes()).zip(keys.enc()) {
        *out = plain ^ key;
    }
    let tag = keys.tag(&nonce, &ciphertext, primary);
    Ok(Lockout {
        primary: *primary,
        nonce,
        ciphertext,
        tag,
        proven: false,
    })
}

/// The authorization in `token`, once its tag verifies in constant time
/// under this volume key and UUID and its Name is `primary`, this TPM's
/// storage primary.
pub fn open(
    volume_key: &[u8],
    uuid: &[u8; VOLUME_UUID_LEN],
    token: &Lockout,
    primary: &[u8; PRIMARY_NAME_LEN],
) -> Result<LockoutAuth, OpenError> {
    let keys = Keys::derive(volume_key, uuid, &token.nonce).map_err(OpenError::Invalid)?;
    let mut tag = keys.tag(&token.nonce, &token.ciphertext, &token.primary);
    let verified = td_tpm::equal(&tag, &token.tag);
    td_tpm::zero(&mut tag);
    if !verified {
        return Err(OpenError::Tag);
    }
    if token.primary != *primary {
        return Err(OpenError::OtherPrimary);
    }
    let mut plain = [0u8; FIELD_LEN];
    for ((out, cipher), key) in plain.iter_mut().zip(&token.ciphertext).zip(keys.enc()) {
        *out = cipher ^ key;
    }
    LockoutAuth::from_bytes(&mut plain).map_err(OpenError::Invalid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protected::Token;

    // `tests/token_vectors.py`'s inputs and outputs.
    const UUID: &str = "0f0e0d0c-0b0a-4908-8706-050403020100";
    const CIPHERTEXT: &str = "d2aed39a185f42aa20e5f5a15f889ec4c2d0b664f582169e9ec05ec8a64f77e3";
    const TAG: &str = "cd71af9127aa8b19eeb3305e68d32c2e56b38221e2be0c8ebc51ef5c6b810ace";
    const TOKEN: &str = r#"{"type":"td-protector","keyslots":[],"role":"lockout","primary":"000b1111111111111111111111111111111111111111111111111111111111111111","nonce":"a0a1a2a3a4a5a6a7a8a9aaabacadaeafb0b1b2b3b4b5b6b7b8b9babbbcbdbebf","ciphertext":"d2aed39a185f42aa20e5f5a15f889ec4c2d0b664f582169e9ec05ec8a64f77e3","tag":"cd71af9127aa8b19eeb3305e68d32c2e56b38221e2be0c8ebc51ef5c6b810ace","proven":"0"}"#;

    fn volume_key() -> Vec<u8> {
        (0..64).collect()
    }

    fn uuid() -> [u8; VOLUME_UUID_LEN] {
        volume_uuid(UUID).unwrap()
    }

    fn nonce() -> [u8; FIELD_LEN] {
        core::array::from_fn(|at| 0xa0 + at as u8)
    }

    fn auth_bytes() -> [u8; FIELD_LEN] {
        core::array::from_fn(|at| 0xc0 + at as u8)
    }

    fn auth() -> LockoutAuth {
        LockoutAuth::from_bytes(&mut auth_bytes()).unwrap()
    }

    fn primary() -> [u8; PRIMARY_NAME_LEN] {
        crate::protected::fixtures::PRIMARY
    }

    fn sealed() -> Lockout {
        seal_with(&volume_key(), &uuid(), &auth(), &primary(), nonce()).unwrap()
    }

    #[test]
    fn the_token_matches_the_independent_vectors() {
        let token = sealed();
        assert_eq!(crate::token::hex(&token.ciphertext), CIPHERTEXT);
        assert_eq!(crate::token::hex(&token.tag), TAG);
        assert_eq!(token.nonce, nonce());
        assert_eq!(token.primary, primary());
        // A fresh token is imported unproven.
        assert!(!token.proven);
        let text = Token::Lockout(token.clone()).encode().unwrap();
        assert_eq!(text, TOKEN);
        assert_eq!(
            Token::decode(TOKEN.as_bytes()).unwrap(),
            Token::Lockout(token)
        );
    }

    #[test]
    fn open_returns_the_authorization_sealed() {
        let opened = open(&volume_key(), &uuid(), &sealed(), &primary()).unwrap();
        assert_eq!(opened.bytes(), &auth_bytes());
        // The mark lies outside the tag.
        let proven = Lockout {
            proven: true,
            ..sealed()
        };
        let opened = open(&volume_key(), &uuid(), &proven, &primary()).unwrap();
        assert_eq!(opened.bytes(), &auth_bytes());
        // Each write's fresh nonce gives another ciphertext for one value.
        let other = seal_with(&volume_key(), &uuid(), &auth(), &primary(), [9; 32]).unwrap();
        assert_ne!(other.ciphertext, sealed().ciphertext);
        let opened = open(&volume_key(), &uuid(), &other, &primary()).unwrap();
        assert_eq!(opened.bytes(), &auth_bytes());
    }

    #[test]
    fn open_refuses_another_volume_a_forged_token_and_another_primary() {
        let token = sealed();
        let mut other_key = volume_key();
        other_key[63] ^= 1;
        assert_eq!(
            open(&other_key, &uuid(), &token, &primary()).unwrap_err(),
            OpenError::Tag
        );
        let mut other_uuid = uuid();
        other_uuid[0] ^= 1;
        assert_eq!(
            open(&volume_key(), &other_uuid, &token, &primary()).unwrap_err(),
            OpenError::Tag
        );
        // Every byte of every field the tag covers, and the tag itself.
        for at in 0..FIELD_LEN {
            for field in 0..3 {
                let mut forged = token.clone();
                match field {
                    0 => forged.nonce[at] ^= 0x80,
                    1 => forged.ciphertext[at] ^= 0x80,
                    _ => forged.tag[at] ^= 0x80,
                }
                assert_eq!(
                    open(&volume_key(), &uuid(), &forged, &primary()).unwrap_err(),
                    OpenError::Tag
                );
            }
        }
        for at in 0..PRIMARY_NAME_LEN {
            let mut forged = token.clone();
            forged.primary[at] ^= 0x80;
            // The Name the token carries is covered by the tag, so it
            // cannot be moved to this TPM's.
            assert_eq!(
                open(&volume_key(), &uuid(), &forged, &forged.primary).unwrap_err(),
                OpenError::Tag
            );
        }
        let mut other_primary = primary();
        other_primary[33] ^= 1;
        assert_eq!(
            open(&volume_key(), &uuid(), &token, &other_primary).unwrap_err(),
            OpenError::OtherPrimary
        );
        assert!(matches!(
            open(&[], &uuid(), &token, &primary()).unwrap_err(),
            OpenError::Invalid(_)
        ));
        assert!(seal_with(&[], &uuid(), &auth(), &primary(), nonce()).is_err());
        // A tag that verifies over a ciphertext decrypting to zero, which
        // td-tpm refuses as an empty authorization.
        let keys = Keys::derive(&volume_key(), &uuid(), &nonce()).unwrap();
        let mut zero = token.clone();
        zero.ciphertext.copy_from_slice(keys.enc());
        zero.tag = keys.tag(&zero.nonce, &zero.ciphertext, &zero.primary);
        assert!(matches!(
            open(&volume_key(), &uuid(), &zero, &primary()).unwrap_err(),
            OpenError::Invalid(_)
        ));
        for error in [
            OpenError::Tag,
            OpenError::OtherPrimary,
            OpenError::Invalid("x".into()),
        ] {
            assert!(error.to_string().starts_with("the lockout token"));
        }
    }

    #[test]
    fn the_nonce_is_one_exact_read_of_the_source() {
        let dir = std::env::temp_dir().join(format!("td-protector-lockout-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("random");
        std::fs::write(&source, nonce()).unwrap();
        let token = seal_from(&source, &volume_key(), &uuid(), &auth(), &primary()).unwrap();
        assert_eq!(token, sealed());
        std::fs::write(&source, &nonce()[..31]).unwrap();
        assert!(
            seal_from(&source, &volume_key(), &uuid(), &auth(), &primary())
                .unwrap_err()
                .contains("lockout nonce")
        );
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(NONCE_SOURCE, "/dev/random");
    }

    #[test]
    fn volume_uuids_are_the_header_text() {
        assert_eq!(
            volume_uuid(UUID).unwrap(),
            [15, 14, 13, 12, 11, 10, 0x49, 8, 0x87, 6, 5, 4, 3, 2, 1, 0]
        );
        assert_eq!(volume_uuid(&UUID.to_uppercase()), volume_uuid(UUID));
        for text in [
            "",
            "0f0e0d0c-0b0a-4908-8706-05040302010",
            "0f0e0d0c-0b0a-4908-8706-0504030201000",
            "0f0e0d0c0b0a-4908-8706-050403020100-",
            "0f0e0d0c-0b0a-4908-8706_050403020100",
            "0f0e0d0g-0b0a-4908-8706-050403020100",
            "+f0e0d0c-0b0a-4908-8706-050403020100",
            "0f0e0d0c-0b0a-4908-8706-05040302010\u{e9}",
        ] {
            assert_eq!(volume_uuid(text), None, "{text:?}");
        }
    }
}

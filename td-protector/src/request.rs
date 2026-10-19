//! The `td-request` token's tag (ENCRYPTION.md "Request tag"): anyone who
//! can write the disk can write a request, so the selector honours only
//! one whose tag verifies under the volume key. The tag covers the header
//! state the request acts on, which every operation changes, so a request
//! copied from an older header copy stops verifying once a like operation
//! has run. Library code: nothing calls it before 8g's storage-worker
//! requests and the selector's request plan.

use crate::hmac::{hkdf_expand, hmac_sha256};
use crate::lockout::VOLUME_UUID_LEN;
use crate::protected::{Operation, Request, Token, FIELD_LEN};
use crate::sha256::Sha256;
use std::path::Path;

/// The HKDF info's prefix; a zero byte, the volume UUID and the nonce
/// follow it.
pub const REQUEST_DOMAIN: &[u8] = b"td/disk-protector/request/v1";
/// Blocks until the kernel CSPRNG is initialized, as the lockout's nonce
/// source does.
const NONCE_SOURCE: &str = "/dev/random";

/// The header state a request acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct State {
    /// The lockout token's nonce, or 32 zero bytes without one.
    lockout: [u8; FIELD_LEN],
    /// SHA-256 over every tpm-pin token's `public` bytes in token-number
    /// order, each preceded by its length as two big-endian bytes.
    pins: [u8; FIELD_LEN],
}

impl State {
    /// The state of a header's protected tokens, numbered as the reader
    /// returns them, in any order. Two lockout tokens refuse, as the
    /// reader does.
    fn of(tokens: &[(u8, Token)]) -> Result<Self, String> {
        let mut lockout = None;
        let mut pins: Vec<(u8, &[u8])> = Vec::new();
        for (number, token) in tokens {
            match token {
                Token::Lockout(found) => {
                    if lockout.replace(found.nonce).is_some() {
                        return Err("the header holds more than one lockout token".into());
                    }
                }
                Token::TpmPin(pin) => pins.push((*number, &pin.sealed.public)),
                _ => {}
            }
        }
        pins.sort_by_key(|(number, _)| *number);
        let mut hash = Sha256::new();
        for (_, public) in pins {
            let len = u16::try_from(public.len())
                .map_err(|_| "a tpm-pin token's public area is too long")?;
            hash.update(&len.to_be_bytes());
            hash.update(public);
        }
        Ok(Self {
            lockout: lockout.unwrap_or([0; FIELD_LEN]),
            pins: hash.finalize(),
        })
    }
}

/// The request key, zeroed on drop.
struct Key([u8; FIELD_LEN]);
impl Drop for Key {
    fn drop(&mut self) {
        td_tpm::zero(&mut self.0);
    }
}
impl Key {
    fn derive(
        volume_key: &[u8],
        uuid: &[u8; VOLUME_UUID_LEN],
        nonce: &[u8; FIELD_LEN],
    ) -> Result<Self, String> {
        if volume_key.is_empty() {
            return Err("the volume key is empty".into());
        }
        let mut key = Self([0; FIELD_LEN]);
        hkdf_expand(
            volume_key,
            &[],
            &[REQUEST_DOMAIN, &[0], uuid, nonce],
            &mut key.0,
        )?;
        Ok(key)
    }
}

/// The tag of a request for `operation` with `nonce` on a header whose
/// protected tokens are `header` (`luks2::Header::protected`), under this
/// volume key and UUID.
pub fn tag(
    volume_key: &[u8],
    uuid: &[u8; VOLUME_UUID_LEN],
    operation: Operation,
    nonce: &[u8; FIELD_LEN],
    header: &[(u8, Token)],
) -> Result<[u8; FIELD_LEN], String> {
    let state = State::of(header)?;
    let key = Key::derive(volume_key, uuid, nonce)?;
    let name = operation.name().as_bytes();
    let len = u8::try_from(name.len()).map_err(|_| "a request's operation name is too long")?;
    Ok(hmac_sha256(
        &key.0,
        &[&[len], name, nonce, &state.lockout, &state.pins],
    ))
}

/// The request for `operation` on `header`, tagged under a fresh nonce
/// from `/dev/random`: two requests under one volume key never share one.
pub fn seal(
    volume_key: &[u8],
    uuid: &[u8; VOLUME_UUID_LEN],
    operation: Operation,
    header: &[(u8, Token)],
) -> Result<Request, String> {
    seal_from(Path::new(NONCE_SOURCE), volume_key, uuid, operation, header)
}

fn seal_from(
    source: &Path,
    volume_key: &[u8],
    uuid: &[u8; VOLUME_UUID_LEN],
    operation: Operation,
    header: &[(u8, Token)],
) -> Result<Request, String> {
    let mut nonce = [0u8; FIELD_LEN];
    crate::read_random(source, &mut nonce, "request nonce")?;
    let tag = tag(volume_key, uuid, operation, &nonce, header)?;
    Ok(Request {
        operation,
        nonce,
        tag,
    })
}

/// Whether `request`'s tag verifies under this volume key and UUID for
/// `header`, the header read at release, compared in constant time.
pub fn verifies(
    volume_key: &[u8],
    uuid: &[u8; VOLUME_UUID_LEN],
    request: &Request,
    header: &[(u8, Token)],
) -> bool {
    match tag(volume_key, uuid, request.operation, &request.nonce, header) {
        Ok(expected) => td_tpm::equal(&request.tag, &expected),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lockout::volume_uuid;
    use crate::protected::{fixtures, Lockout, TpmPin};
    use td_tpm::SealedObject;

    // `tests/token_vectors.py`'s inputs and outputs.
    const UUID: &str = "0f0e0d0c-0b0a-4908-8706-050403020100";
    const TAG: &str = "ce5df8321a7b1efeaccb9504b238a6a56d916d22015da4b88d90c265ed05069d";
    const BARE_TAG: &str = "d295e07fa5b6a469e17252bb3ffe1649c43b85a7708976d27fa35e9f5e83dfe9";

    fn volume_key() -> Vec<u8> {
        (0..64).collect()
    }

    fn uuid() -> [u8; VOLUME_UUID_LEN] {
        volume_uuid(UUID).unwrap()
    }

    fn nonce() -> [u8; FIELD_LEN] {
        core::array::from_fn(|at| 0x80 + at as u8)
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn pin(public: &[u8]) -> Token {
        let Token::TpmPin(pin) = fixtures::tpm_pin(Some(1)) else {
            panic!("the fixture changed kind")
        };
        Token::TpmPin(TpmPin {
            sealed: SealedObject {
                public: public.to_vec(),
                ..pin.sealed
            },
            ..pin
        })
    }

    fn lockout(nonce: [u8; FIELD_LEN]) -> Token {
        let Token::Lockout(lockout) = fixtures::lockout() else {
            panic!("the fixture changed kind")
        };
        Token::Lockout(Lockout { nonce, ..lockout })
    }

    /// The vectors' header: the lockout at the vectors' nonce, then two
    /// tpm-pin tokens, numbered 4 and 9, beside tokens the state ignores.
    fn header() -> Vec<(u8, Token)> {
        vec![
            (9, pin(&[0x00, 0x06, 0xa1, 0xb2, 0xc3])),
            (
                2,
                fixtures::fido2(Some(3), crate::protected::FidoRole::Recovery),
            ),
            (5, lockout(core::array::from_fn(|at| 0xa0 + at as u8))),
            (4, pin(&[0x00, 0x08, 0xab, 0xcd])),
            (7, Token::RecoveryKey { keyslot: Some(0) }),
            (8, fixtures::update_retry()),
        ]
    }

    fn request(header: &[(u8, Token)]) -> Request {
        let operation = Operation::PinChange;
        Request {
            operation,
            nonce: nonce(),
            tag: tag(&volume_key(), &uuid(), operation, &nonce(), header).unwrap(),
        }
    }

    #[test]
    fn the_tag_is_the_vectors_and_the_fixture() {
        let request = request(&header());
        assert_eq!(hex(&request.tag), TAG);
        assert_eq!(Token::Request(request), fixtures::request());
        let bare = tag(
            &volume_key(),
            &uuid(),
            Operation::TpmReenroll,
            &nonce(),
            &[],
        )
        .unwrap();
        assert_eq!(hex(&bare), BARE_TAG);
        // No lockout token is 32 zero bytes; no tpm-pin token hashes
        // nothing.
        assert_eq!(
            State::of(&[]),
            Ok(State {
                lockout: [0; FIELD_LEN],
                pins: Sha256::new().finalize(),
            })
        );
    }

    #[test]
    fn the_tag_verifies_only_for_its_operation_volume_and_state() {
        let header = header();
        let request = request(&header);
        let ok = |request: &Request, key: &[u8], uuid: &[u8; 16], tokens: &[(u8, Token)]| {
            verifies(key, uuid, request, tokens)
        };
        assert!(ok(&request, &volume_key(), &uuid(), &header));
        // The tokens' order in the list is not the token-number order.
        let mut shuffled = header.clone();
        shuffled.reverse();
        assert!(ok(&request, &volume_key(), &uuid(), &shuffled));
        // Another operation, nonce or tag.
        for operation in [Operation::TpmClear, Operation::TpmReenroll] {
            let other = Request {
                operation,
                ..request.clone()
            };
            assert!(!ok(&other, &volume_key(), &uuid(), &header));
        }
        let mut other = request.clone();
        other.nonce[0] ^= 1;
        assert!(!ok(&other, &volume_key(), &uuid(), &header));
        let mut forged = request.clone();
        forged.tag[31] ^= 1;
        assert!(!ok(&forged, &volume_key(), &uuid(), &header));
        // Another volume: its key or its UUID.
        let mut key = volume_key();
        key[0] ^= 1;
        assert!(!ok(&request, &key, &uuid(), &header));
        let mut elsewhere = uuid();
        elsewhere[0] ^= 1;
        assert!(!ok(&request, &volume_key(), &elsewhere, &header));
        // The state each operation changes: the lockout's nonce, or its
        // token gone; a tpm-pin token's public area, one more, one fewer,
        // or the two swapped between their numbers.
        type Change = fn(&mut Vec<(u8, Token)>);
        let changed: &[Change] = &[
            |tokens| tokens[2].1 = lockout([0xa0; 32]),
            |tokens| {
                tokens.remove(2);
            },
            |tokens| tokens[0].1 = pin(&[0x00, 0x06, 0xa1, 0xb2, 0xc4]),
            |tokens| tokens.push((10, pin(&[0x00, 0x01, 0x00]))),
            |tokens| {
                tokens.remove(3);
            },
            |tokens| {
                tokens[0].0 = 4;
                tokens[3].0 = 9;
            },
        ];
        for change in changed {
            let mut tokens = header.clone();
            change(&mut tokens);
            assert!(!ok(&request, &volume_key(), &uuid(), &tokens));
        }
        // Tokens the state does not cover leave the tag as it was: the
        // request itself, as the selector reads it beside its state, an
        // upgrade token, and the header's fido2, marker and update-retry.
        let mut tokens = header.clone();
        tokens.push((10, Token::Request(request.clone())));
        tokens.push((11, fixtures::upgrade()));
        assert!(ok(&request, &volume_key(), &uuid(), &tokens));
        tokens.retain(|(number, _)| ![2, 7, 8, 10, 11].contains(number));
        assert!(ok(&request, &volume_key(), &uuid(), &tokens));
        // No volume key tags or verifies nothing.
        assert!(tag(&[], &uuid(), Operation::PinChange, &nonce(), &header).is_err());
        assert!(!ok(&request, &[], &uuid(), &header));
    }

    #[test]
    fn two_lockout_tokens_have_no_state_and_verify_nothing() {
        let mut tokens = header();
        let request = request(&tokens);
        tokens.push((11, lockout([1; 32])));
        assert!(State::of(&tokens)
            .unwrap_err()
            .contains("more than one lockout"));
        assert!(!verifies(&volume_key(), &uuid(), &request, &tokens));
    }

    #[test]
    fn the_nonce_is_one_exact_read_of_the_source() {
        let dir = td_fs::TempDir::new("td-protector-request-nonce").unwrap();
        let source = dir.path().join("random");
        std::fs::write(&source, nonce()).unwrap();
        let header = header();
        let sealed = seal_from(
            &source,
            &volume_key(),
            &uuid(),
            Operation::PinChange,
            &header,
        )
        .unwrap();
        assert_eq!(Token::Request(sealed), fixtures::request());
        std::fs::write(&source, &nonce()[..31]).unwrap();
        assert!(seal_from(
            &source,
            &volume_key(),
            &uuid(),
            Operation::PinChange,
            &header
        )
        .unwrap_err()
        .contains("request nonce"));
        assert_eq!(NONCE_SOURCE, "/dev/random");
    }
}

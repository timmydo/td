//! The `td-request` token's tag (ENCRYPTION.md "Request tag"): anyone who
//! can write the disk can write a request, so the selector honours only
//! one whose tag verifies under the volume key. The tag covers the header
//! state the request acts on, which every operation changes, so a request
//! copied from an older header copy stops verifying once a like operation
//! has run. Beside it, the selector's request planner (`fate`, `plan`).
//! Library code: nothing calls it before 8g's storage-worker requests and
//! the selector halves.

use crate::hmac::{hkdf_expand, hmac_sha256};
use crate::lockout::VOLUME_UUID_LEN;
use crate::luks2::Header;
use crate::protected::{Operation, Request, Shape, Token, TpmPin, FIELD_LEN};
use crate::protected_plan::{self, Opened, Plan, Refusal, Step};
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

/// What the selector prints as it removes a request whose tag fails.
pub const UNVERIFIED: &str = "td: removed a disk request that does not verify";

/// The header's `td-request` token, with its number.
pub fn pending(header: &Header) -> Option<(u8, &Request)> {
    header
        .protected
        .iter()
        .find_map(|(number, token)| match token {
            Token::Request(request) => Some((*number, request)),
            _ => None,
        })
}

/// Which storage primary this boot's TPM has, against the one the header
/// records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Primary {
    /// A tpm-pin object passed the chain check, or the lockout token
    /// opens for this TPM's primary.
    Recorded,
    /// The chain check found another primary, or the lockout token
    /// verifies but names another.
    Other,
    /// No TPM this boot, or none td could read or prove.
    Unknown,
}

/// A PIN change's new PIN, asked before the cap after a TPM PIN release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewPin {
    /// Sealed and verified: the reseal argument of `plan` holds it.
    Sealed,
    /// An empty entry.
    Declined,
    /// The seal or its verification failed: the old PIN stays.
    Failed,
}

/// What the selector knows of this boot when it plans a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Boot {
    pub opened: Opened,
    pub primary: Primary,
    /// After a TPM PIN release beside a pending PIN change, its entry.
    pub new_pin: Option<NewPin>,
    /// A confirmed reseal, verified before the cap.
    pub reseal: bool,
}

/// The question a request asks before it is carried out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Question {
    /// `td: type clear to clear the TPM: `.
    Clear,
    /// `td: type reenroll to enroll this TPM: `.
    Reenroll,
}

impl Question {
    /// The fate an answer gives: a confirmation carries the request out,
    /// any other answer declines it.
    pub fn answered(self, confirmed: bool) -> Resolved {
        match (self, confirmed) {
            (_, false) => Resolved::Removed(Removed::Declined),
            (Self::Clear, true) => Resolved::Clear,
            (Self::Reenroll, true) => Resolved::Reenroll,
        }
    }
}

/// Why a request is removed without being carried out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Removed {
    /// Its tag does not verify: `UNVERIFIED`.
    Unverified,
    /// The person declined it.
    Declined,
    /// A re-enrollment on the TPM whose primary the header records, a
    /// PIN release's included: nothing is stale.
    RecordedPrimary,
    /// A TPM clear or re-enrollment with no TPM td can prove this boot,
    /// before its question, or a confirmed clear whose lockout step could
    /// not prove this TPM.
    Unproven,
    /// A confirmed TPM clear that TPM2_Clear refused.
    Refused,
}

/// A request's fate once nothing more is asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolved {
    /// Removed as the plan's first write.
    Removed(Removed),
    /// Left for a later boot.
    Left,
    /// A PIN change removed with the confirmed reseal's plan, which sets
    /// a new PIN itself.
    Superseded,
    /// A PIN change carried out through the reseal's plan.
    PinChange,
    /// A TPM clear carried out once TPM2_Clear succeeded.
    Clear,
    /// A re-enrollment carried out, before this boot's lockout take.
    Reenroll,
}

/// A request's fate before any question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fate {
    Resolved(Resolved),
    Ask(Question),
}

/// The pending request's number and fate this boot (DESIGN.md "The
/// selector's `request::plan`"), its tag checked first under the volume
/// key over the header read at release.
pub fn fate(
    header: &Header,
    volume_key: &[u8],
    uuid: &[u8; VOLUME_UUID_LEN],
    boot: &Boot,
) -> Option<(u8, Fate)> {
    let (number, request) = pending(header)?;
    let removed = |why| Fate::Resolved(Resolved::Removed(why));
    if !verifies(volume_key, uuid, request, &header.protected) {
        return Some((number, removed(Removed::Unverified)));
    }
    let pin_release = matches!(boot.opened, Opened::TpmPin { .. });
    let fate = match request.operation {
        Operation::PinChange => Fate::Resolved(match (pin_release, boot.new_pin) {
            (true, Some(NewPin::Sealed)) => Resolved::PinChange,
            (true, Some(NewPin::Declined)) => Resolved::Removed(Removed::Declined),
            (true, _) => Resolved::Left,
            (false, _) if boot.reseal => Resolved::Superseded,
            (false, _) => Resolved::Left,
        }),
        Operation::TpmClear => match boot.primary {
            // A PIN release proved the recorded primary.
            _ if pin_release => Fate::Ask(Question::Clear),
            Primary::Recorded => Fate::Ask(Question::Clear),
            Primary::Other => Fate::Ask(Question::Reenroll),
            Primary::Unknown => removed(Removed::Unproven),
        },
        Operation::TpmReenroll => match boot.primary {
            _ if pin_release => removed(Removed::RecordedPrimary),
            Primary::Other => Fate::Ask(Question::Reenroll),
            Primary::Recorded => removed(Removed::RecordedPrimary),
            Primary::Unknown => removed(Removed::Unproven),
        },
    };
    Some((number, fate))
}

/// Whether `step` changes the state a request's tag covers: a tpm-pin or
/// lockout token's write or removal.
fn covered(header: &Header, step: &Step) -> bool {
    let kind = |token: &Token| matches!(token, Token::TpmPin(_) | Token::Lockout(_));
    match step {
        Step::RemoveToken { token } => header
            .protected
            .iter()
            .any(|(number, held)| number == token && kind(held)),
        Step::ImportToken { token, .. } => kind(token),
        _ => false,
    }
}

/// The boot's plan beside the request `number` and its fate (DESIGN.md
/// "The selector's `request::plan`"). `reseal` is the confirmed reseal's
/// protector, or the PIN change's sealed under the new PIN.
///
/// - Removed: the request's removal, then the boot's own plan.
/// - Left: the boot's own plan.
/// - Superseded and PinChange: the reseal's plan, the request removed
///   just before its first write the tag covers.
/// - Clear and Reenroll, in place of the boot's own plan: each tpm-pin
///   keyslot but the opener's killed under the opener, the request's
///   removal, then each tpm-pin token's and the lockout token's.
///
/// A fate that carries the request out needs its operation and a tag that
/// verifies under `volume_key` and `uuid`, and a re-enrollment a release
/// other than a PIN's (`Refusal::Request`). A reseal
/// beside a pending clear or re-enrollment is refused: none is offered
/// while one is pending.
pub fn plan(
    header: &Header,
    volume_key: &[u8],
    uuid: &[u8; VOLUME_UUID_LEN],
    opened: Opened,
    reseal: Option<&TpmPin>,
    number: u8,
    resolved: Resolved,
) -> Result<Plan, Refusal> {
    let Some((pending, request)) = pending(header) else {
        return Err(Refusal::UnknownToken { token: number });
    };
    if pending != number {
        return Err(Refusal::UnknownToken { token: number });
    }
    if reseal.is_some() && request.operation != Operation::PinChange {
        return Err(Refusal::NoReseal);
    }
    let carried = match resolved {
        Resolved::PinChange => Some(request.operation == Operation::PinChange),
        Resolved::Clear => Some(request.operation == Operation::TpmClear),
        // A clear on another primary is carried out as a re-enrollment.
        // Never at a PIN release, which proved the recorded primary.
        Resolved::Reenroll => Some(
            matches!(
                request.operation,
                Operation::TpmClear | Operation::TpmReenroll
            ) && !matches!(opened, Opened::TpmPin { .. }),
        ),
        Resolved::Removed(_) | Resolved::Left | Resolved::Superseded => None,
    };
    if carried.is_some_and(|ours| !ours || !verifies(volume_key, uuid, request, &header.protected))
    {
        return Err(Refusal::Request);
    }
    let removal = Step::RemoveToken { token: number };
    let before_covered = |plan: Plan| {
        let mut steps = plan.steps().to_vec();
        let at = steps
            .iter()
            .position(|step| covered(header, step))
            .unwrap_or(steps.len());
        steps.insert(at, removal.clone());
        Plan::new(header, steps)
    };
    match resolved {
        Resolved::Removed(_) => {
            let own = protected_plan::plan(header, opened, reseal)?;
            let mut steps = vec![removal.clone()];
            steps.extend_from_slice(own.steps());
            Ok(Plan::new(header, steps))
        }
        Resolved::Left => protected_plan::plan(header, opened, reseal),
        Resolved::Superseded => {
            if request.operation != Operation::PinChange || reseal.is_none() {
                return Err(Refusal::NoReseal);
            }
            protected_plan::plan(header, opened, reseal).map(before_covered)
        }
        Resolved::PinChange => {
            let (Operation::PinChange, Some(new)) = (request.operation, reseal) else {
                return Err(Refusal::NoReseal);
            };
            protected_plan::pin_change(header, opened, new).map(before_covered)
        }
        Resolved::Clear | Resolved::Reenroll => {
            if protected_plan::check(header)? != Shape::Tpm {
                return Err(Refusal::NoLockout);
            }
            let opener = protected_plan::opener(header, opened)?;
            let pins = protected_plan::tpm_pins(header);
            let mut steps = Vec::new();
            let mut killed: Vec<u8> = Vec::new();
            for slot in pins.iter().filter_map(|(_, pin)| pin.keyslot) {
                if slot != opener && !killed.contains(&slot) {
                    steps.push(Step::KillSlot { slot, by: opener });
                    killed.push(slot);
                }
            }
            steps.extend(
                pins.iter()
                    .map(|(token, _)| Step::RemoveToken { token: *token }),
            );
            if let Some((token, _)) = protected_plan::lockout(header) {
                steps.push(Step::RemoveToken { token });
            }
            Ok(before_covered(Plan::new(header, steps)))
        }
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

    /// The request planner over protected_plan's disk model.
    mod plans {
        use super::super::*;
        use crate::protected::Request;
        use crate::protected_plan::tests::{fido, pin, Disk, FIDO_KEY};
        use crate::protected_plan::{Opened, Step};

        const BY_FIDO: Opened = Opened::Fido2 { token: 2 };
        const BY_PIN: Opened = Opened::TpmPin { token: 1 };
        const NUMBER: u8 = 4;

        fn key() -> Vec<u8> {
            (0..64).collect()
        }

        fn uuid() -> [u8; VOLUME_UUID_LEN] {
            crate::lockout::volume_uuid("0f0e0d0c-0b0a-4908-8706-050403020100").unwrap()
        }

        /// The TPM shape with `operation`'s request at token 4, tagged
        /// over the header unless `forged`.
        fn disk(operation: Operation, forged: bool) -> Disk {
            let mut disk = Disk::tpm_shape();
            let nonce = [9; 32];
            let mut tag =
                tag(&key(), &uuid(), operation, &nonce, &disk.header().protected).unwrap();
            if forged {
                tag[0] ^= 1;
            }
            disk.tokens.insert(
                NUMBER,
                Token::Request(Request {
                    operation,
                    nonce,
                    tag,
                }),
            );
            disk
        }

        fn boot(opened: Opened, primary: Primary) -> Boot {
            Boot {
                opened,
                primary,
                new_pin: None,
                reseal: false,
            }
        }

        fn fate_of(disk: &Disk, boot: &Boot) -> Fate {
            let (number, fate) = fate(&disk.header(), &key(), &uuid(), boot).unwrap();
            assert_eq!(number, NUMBER);
            fate
        }

        fn resolved(resolved: Resolved) -> Fate {
            Fate::Resolved(resolved)
        }

        #[test]
        fn a_request_whose_tag_fails_is_removed_whatever_it_asks() {
            for operation in Operation::ALL {
                let disk = disk(*operation, true);
                for opened in [BY_PIN, BY_FIDO, Opened::RecoveryKey] {
                    assert_eq!(
                        fate_of(&disk, &boot(opened, Primary::Other)),
                        resolved(Resolved::Removed(Removed::Unverified))
                    );
                }
            }
            // Another volume's key, or the header changed since the tag.
            let disk = disk(Operation::TpmClear, false);
            let mut other = key();
            other[0] ^= 1;
            let found = fate(
                &disk.header(),
                &other,
                &uuid(),
                &boot(BY_FIDO, Primary::Recorded),
            );
            assert_eq!(
                found,
                Some((NUMBER, resolved(Resolved::Removed(Removed::Unverified))))
            );
            let mut moved = disk.clone();
            moved.tokens.insert(1, Token::TpmPin(pin(Some(1), 2, 101)));
            assert_eq!(
                fate_of(&moved, &boot(BY_FIDO, Primary::Recorded)),
                resolved(Resolved::Removed(Removed::Unverified))
            );
            assert_eq!(
                fate(
                    &Disk::tpm_shape().header(),
                    &key(),
                    &uuid(),
                    &boot(BY_FIDO, Primary::Recorded)
                ),
                None
            );
        }

        #[test]
        fn each_operation_meets_its_fate() {
            let pin_change = disk(Operation::PinChange, false);
            let with = |new_pin, reseal, opened| Boot {
                new_pin,
                reseal,
                ..boot(opened, Primary::Recorded)
            };
            for (boot, expected) in [
                (
                    with(Some(NewPin::Sealed), false, BY_PIN),
                    Resolved::PinChange,
                ),
                (
                    with(Some(NewPin::Declined), false, BY_PIN),
                    Resolved::Removed(Removed::Declined),
                ),
                (with(Some(NewPin::Failed), false, BY_PIN), Resolved::Left),
                (with(None, false, BY_PIN), Resolved::Left),
                (with(None, false, BY_FIDO), Resolved::Left),
                (with(None, true, BY_FIDO), Resolved::Superseded),
                (with(None, true, Opened::RecoveryKey), Resolved::Superseded),
            ] {
                assert_eq!(fate_of(&pin_change, &boot), resolved(expected), "{boot:?}");
            }
            let clear = disk(Operation::TpmClear, false);
            for opened in [BY_FIDO, Opened::RecoveryKey] {
                assert_eq!(
                    fate_of(&clear, &boot(opened, Primary::Recorded)),
                    Fate::Ask(Question::Clear)
                );
                assert_eq!(
                    fate_of(&clear, &boot(opened, Primary::Other)),
                    Fate::Ask(Question::Reenroll)
                );
                assert_eq!(
                    fate_of(&clear, &boot(opened, Primary::Unknown)),
                    resolved(Resolved::Removed(Removed::Unproven))
                );
            }
            // A PIN release proved the recorded primary, whatever the
            // caller reports: a clear asks the clear's question.
            for primary in [Primary::Recorded, Primary::Other, Primary::Unknown] {
                assert_eq!(
                    fate_of(&clear, &boot(BY_PIN, primary)),
                    Fate::Ask(Question::Clear)
                );
            }
            let reenroll = disk(Operation::TpmReenroll, false);
            for (opened, primary, expected) in [
                (BY_FIDO, Primary::Other, Fate::Ask(Question::Reenroll)),
                (
                    Opened::RecoveryKey,
                    Primary::Other,
                    Fate::Ask(Question::Reenroll),
                ),
                (
                    BY_FIDO,
                    Primary::Recorded,
                    resolved(Resolved::Removed(Removed::RecordedPrimary)),
                ),
                (
                    BY_PIN,
                    Primary::Other,
                    resolved(Resolved::Removed(Removed::RecordedPrimary)),
                ),
                (
                    BY_FIDO,
                    Primary::Unknown,
                    resolved(Resolved::Removed(Removed::Unproven)),
                ),
            ] {
                assert_eq!(fate_of(&reenroll, &boot(opened, primary)), expected);
            }
            assert_eq!(Question::Clear.answered(true), Resolved::Clear);
            assert_eq!(Question::Reenroll.answered(true), Resolved::Reenroll);
            for question in [Question::Clear, Question::Reenroll] {
                assert_eq!(
                    question.answered(false),
                    Resolved::Removed(Removed::Declined)
                );
            }
        }

        fn steps(
            disk: &Disk,
            opened: Opened,
            reseal: Option<&TpmPin>,
            fate: Resolved,
        ) -> Vec<Step> {
            planned(disk, opened, reseal, NUMBER, fate)
                .unwrap()
                .confirm(&disk.dump())
                .unwrap()
        }

        fn planned(
            disk: &Disk,
            opened: Opened,
            reseal: Option<&TpmPin>,
            number: u8,
            fate: Resolved,
        ) -> Result<Plan, Refusal> {
            plan(
                &disk.header(),
                &key(),
                &uuid(),
                opened,
                reseal,
                number,
                fate,
            )
        }

        fn commit(disk: &mut Disk, steps: &[Step]) {
            for step in steps {
                disk.apply(step, 3000);
            }
        }

        const REMOVAL: Step = Step::RemoveToken { token: NUMBER };

        #[test]
        fn a_removal_is_the_plans_first_write() {
            // A PIN release beside a superseded protector: the removal,
            // then the boot's own retirement.
            let mut disk = disk(Operation::TpmReenroll, false);
            disk.keyslots.insert(3, 103);
            disk.tokens.insert(5, Token::TpmPin(pin(Some(3), 9, 103)));
            let found = steps(
                &disk,
                BY_PIN,
                None,
                Resolved::Removed(Removed::RecordedPrimary),
            );
            assert_eq!(
                found,
                [
                    REMOVAL,
                    Step::KillSlot { slot: 3, by: 1 },
                    Step::RemoveToken { token: 5 }
                ]
            );
            commit(&mut disk, &found);
            assert_eq!(crate::request::pending(&disk.header()), None);
            // A security key's boot plans nothing of its own.
            let disk = self::disk(Operation::TpmClear, false);
            assert_eq!(
                steps(&disk, BY_FIDO, None, Resolved::Removed(Removed::Declined)),
                [REMOVAL]
            );
        }

        #[test]
        fn a_left_request_stays_with_the_boots_own_plan() {
            let disk = disk(Operation::PinChange, false);
            assert!(steps(&disk, BY_FIDO, None, Resolved::Left).is_empty());
            assert!(steps(&disk, BY_PIN, None, Resolved::Left).is_empty());
            // A selector update's second token on keyslot 1: the boot's
            // retirement runs and the request stays, though it will no
            // longer verify (ENCRYPTION.md "Threat and limits").
            let mut updated = disk.clone();
            updated
                .tokens
                .insert(5, Token::TpmPin(pin(Some(1), 2, 101)));
            assert_eq!(
                steps(&updated, BY_PIN, None, Resolved::Left),
                [Step::RemoveToken { token: 5 }]
            );
        }

        #[test]
        fn a_removal_comes_before_a_reseal_after_a_security_key() {
            let disk = disk(Operation::PinChange, true);
            let new = pin(None, 1, 3000);
            let found = steps(
                &disk,
                BY_FIDO,
                Some(&new),
                Resolved::Removed(Removed::Unverified),
            );
            assert_eq!(found.first(), Some(&REMOVAL));
            assert_eq!(found.get(1), Some(&Step::AddKeyslot { slot: 3, by: 2 }));
            // A superseded request needs the reseal it is superseded by.
            assert_eq!(
                planned(&disk, BY_FIDO, None, NUMBER, Resolved::Superseded).unwrap_err(),
                Refusal::NoReseal
            );
        }

        #[test]
        fn a_pin_change_runs_where_the_reseal_would_and_removes_the_request_before_its_import() {
            let mut disk = disk(Operation::PinChange, false);
            let new = pin(None, 1, 3000);
            let found = steps(&disk, BY_PIN, Some(&new), Resolved::PinChange);
            assert_eq!(
                found,
                [
                    Step::AddKeyslot { slot: 3, by: 1 },
                    REMOVAL,
                    Step::ImportToken {
                        token: Token::TpmPin(pin(Some(3), 1, 3000)),
                        id: None,
                        replace: false,
                    },
                    Step::Test { slot: 3 },
                    Step::KillSlot { slot: 1, by: 3 },
                    Step::RemoveToken { token: 1 },
                ]
            );
            commit(&mut disk, &found);
            assert_eq!(crate::request::pending(&disk.header()), None);
            assert_eq!(disk.keyslots.keys().copied().collect::<Vec<_>>(), [0, 2, 3]);
            // Two tokens on the released keyslot, an update's: the other
            // goes first, and the request before it.
            let mut updated = self::disk(Operation::PinChange, false);
            updated
                .tokens
                .insert(5, Token::TpmPin(pin(Some(1), 2, 101)));
            retag(&mut updated, Operation::PinChange);
            let found = steps(&updated, BY_PIN, Some(&new), Resolved::PinChange);
            assert_eq!(
                found,
                [
                    REMOVAL,
                    Step::RemoveToken { token: 5 },
                    Step::AddKeyslot { slot: 3, by: 1 },
                    Step::ImportToken {
                        token: Token::TpmPin(pin(Some(3), 1, 3000)),
                        id: None,
                        replace: false,
                    },
                    Step::Test { slot: 3 },
                    Step::KillSlot { slot: 1, by: 3 },
                    Step::RemoveToken { token: 1 },
                ]
            );
            commit(&mut updated, &found);
            // Two on different keyslots, the released one the higher
            // token: the other keyslot is killed under the released one
            // and its number reused for the new keyslot.
            let mut split = self::disk(Operation::PinChange, false);
            split.keyslots.insert(3, 103);
            split.tokens.insert(5, Token::TpmPin(pin(Some(3), 9, 103)));
            retag(&mut split, Operation::PinChange);
            let found = steps(
                &split,
                Opened::TpmPin { token: 5 },
                Some(&new),
                Resolved::PinChange,
            );
            assert_eq!(
                found,
                [
                    Step::KillSlot { slot: 1, by: 3 },
                    REMOVAL,
                    Step::RemoveToken { token: 1 },
                    Step::AddKeyslot { slot: 1, by: 3 },
                    Step::ImportToken {
                        token: Token::TpmPin(pin(Some(1), 1, 3000)),
                        id: None,
                        replace: false,
                    },
                    Step::Test { slot: 1 },
                    Step::KillSlot { slot: 3, by: 1 },
                    Step::RemoveToken { token: 5 },
                ]
            );
            commit(&mut split, &found);
            assert_eq!(split.keyslots.get(&1), Some(&3000));
            // A reseal after a security key's release supersedes it.
            let mut superseded = self::disk(Operation::PinChange, false);
            let found = steps(&superseded, BY_FIDO, Some(&new), Resolved::Superseded);
            assert_eq!(found.get(1), Some(&REMOVAL));
            assert!(matches!(found.get(2), Some(Step::ImportToken { .. })));
            commit(&mut superseded, &found);
        }

        #[test]
        fn a_clear_or_reenrollment_retires_every_tpm_pin_token_and_the_lockout() {
            // After a PIN release the released keyslot stays, an orphan;
            // the other keyslot's kill comes before the request's removal.
            let mut disk = disk(Operation::TpmClear, false);
            disk.keyslots.insert(3, 103);
            disk.tokens.insert(5, Token::TpmPin(pin(Some(3), 9, 103)));
            retag(&mut disk, Operation::TpmClear);
            let found = steps(&disk, BY_PIN, None, Resolved::Clear);
            assert_eq!(
                found,
                [
                    Step::KillSlot { slot: 3, by: 1 },
                    REMOVAL,
                    Step::RemoveToken { token: 1 },
                    Step::RemoveToken { token: 5 },
                    Step::RemoveToken { token: 3 },
                ]
            );
            commit(&mut disk, &found);
            assert_eq!(disk.keyslots.keys().copied().collect::<Vec<_>>(), [0, 1, 2]);
            assert_eq!(disk.tokens.keys().copied().collect::<Vec<_>>(), [0, 2]);
            // A security key's re-enrollment kills a keyslot two tokens
            // share once.
            let mut disk = self::disk(Operation::TpmReenroll, false);
            disk.tokens.insert(5, Token::TpmPin(pin(Some(1), 2, 101)));
            retag(&mut disk, Operation::TpmReenroll);
            let found = steps(&disk, BY_FIDO, None, Resolved::Reenroll);
            assert_eq!(
                found,
                [
                    Step::KillSlot { slot: 1, by: 2 },
                    REMOVAL,
                    Step::RemoveToken { token: 1 },
                    Step::RemoveToken { token: 5 },
                    Step::RemoveToken { token: 3 },
                ]
            );
            commit(&mut disk, &found);
            assert_eq!(disk.keyslots.get(&2), Some(&FIDO_KEY));
            // A clear on another primary is carried out as a re-enrollment,
            // but never at a PIN release.
            let disk = self::disk(Operation::TpmClear, false);
            assert!(planned(&disk, BY_FIDO, None, NUMBER, Resolved::Reenroll).is_ok());
            for operation in [Operation::TpmClear, Operation::TpmReenroll] {
                assert_eq!(
                    planned(
                        &self::disk(operation, false),
                        BY_PIN,
                        None,
                        NUMBER,
                        Resolved::Reenroll
                    )
                    .unwrap_err(),
                    Refusal::Request
                );
            }
        }

        /// The disk with its request re-tagged over it as it now stands.
        fn retag(disk: &mut Disk, operation: Operation) {
            let request = tagged(disk, operation);
            disk.tokens.insert(NUMBER, Token::Request(request));
        }

        /// The request re-tagged over the disk as it now stands, without
        /// itself.
        fn tagged(disk: &Disk, operation: Operation) -> Request {
            let mut header = disk.header();
            header.protected.retain(|(number, _)| *number != NUMBER);
            let nonce = [9; 32];
            Request {
                operation,
                nonce,
                tag: tag(&key(), &uuid(), operation, &nonce, &header.protected).unwrap(),
            }
        }

        #[test]
        fn carrying_out_needs_the_requests_operation_and_a_verified_tag() {
            let new = pin(None, 1, 3000);
            for (operation, fate, reseal, opened) in [
                (Operation::PinChange, Resolved::Clear, None, BY_FIDO),
                (Operation::PinChange, Resolved::Reenroll, None, BY_FIDO),
                (Operation::TpmReenroll, Resolved::Clear, None, BY_FIDO),
                (Operation::TpmReenroll, Resolved::PinChange, None, BY_PIN),
            ] {
                let disk = disk(operation, false);
                assert_eq!(
                    planned(&disk, opened, reseal, NUMBER, fate).unwrap_err(),
                    Refusal::Request,
                    "{operation:?} {fate:?}"
                );
            }
            // With the new PIN's protector, a clear's refusal is the reseal's.
            assert_eq!(
                planned(
                    &disk(Operation::TpmClear, false),
                    BY_PIN,
                    Some(&new),
                    NUMBER,
                    Resolved::PinChange
                )
                .unwrap_err(),
                Refusal::NoReseal
            );
            for (operation, fate, reseal, opened) in [
                (
                    Operation::PinChange,
                    Resolved::PinChange,
                    Some(&new),
                    BY_PIN,
                ),
                (Operation::TpmClear, Resolved::Clear, None, BY_FIDO),
                (Operation::TpmReenroll, Resolved::Reenroll, None, BY_FIDO),
            ] {
                let forged = disk(operation, true);
                assert_eq!(
                    planned(&forged, opened, reseal, NUMBER, fate).unwrap_err(),
                    Refusal::Request
                );
                let disk = disk(operation, false);
                assert!(planned(&disk, opened, reseal, NUMBER, fate).is_ok());
                // Another volume's key verifies nothing.
                let mut other = key();
                other[1] ^= 1;
                assert_eq!(
                    plan(
                        &disk.header(),
                        &other,
                        &uuid(),
                        opened,
                        reseal,
                        NUMBER,
                        fate
                    )
                    .unwrap_err(),
                    Refusal::Request
                );
            }
            // A removal or a left request needs no verified tag.
            let forged = disk(Operation::TpmClear, true);
            for fate in [Resolved::Left, Resolved::Removed(Removed::Unverified)] {
                assert!(planned(&forged, BY_FIDO, None, NUMBER, fate).is_ok());
            }
        }

        /// At every cut of a plan that carries a request out or supersedes
        /// it: the request is gone or still verifies over what the cut
        /// left, and the opener's keyslot still opens.
        #[test]
        fn every_cut_leaves_the_request_and_its_state_or_neither() {
            let new = pin(None, 1, 3000);
            let mut updated = disk(Operation::PinChange, false);
            updated
                .tokens
                .insert(5, Token::TpmPin(pin(Some(1), 2, 101)));
            let mut retagged = updated.clone();
            retag(&mut retagged, Operation::PinChange);
            let mut split = disk(Operation::TpmClear, false);
            split.keyslots.insert(3, 103);
            split.tokens.insert(5, Token::TpmPin(pin(Some(3), 9, 103)));
            let mut split_tagged = split.clone();
            retag(&mut split_tagged, Operation::TpmClear);
            let cases: Vec<(Disk, Opened, Option<&TpmPin>, Resolved)> = vec![
                (
                    disk(Operation::PinChange, false),
                    BY_PIN,
                    Some(&new),
                    Resolved::PinChange,
                ),
                (retagged, BY_PIN, Some(&new), Resolved::PinChange),
                (
                    disk(Operation::PinChange, false),
                    BY_FIDO,
                    Some(&new),
                    Resolved::Superseded,
                ),
                (split_tagged.clone(), BY_PIN, None, Resolved::Clear),
                (split_tagged, BY_FIDO, None, Resolved::Reenroll),
                (
                    disk(Operation::TpmReenroll, false),
                    Opened::RecoveryKey,
                    None,
                    Resolved::Reenroll,
                ),
            ];
            for (start, opened, reseal, fate) in cases {
                let opener = crate::protected_plan::opener(&start.header(), opened).unwrap();
                let found = steps(&start, opened, reseal, fate);
                for cut in 0..=found.len() {
                    let mut disk = start.clone();
                    commit(&mut disk, found.get(..cut).unwrap());
                    let header = disk.header();
                    if let Some((_, request)) = pending(&header) {
                        assert!(
                            verifies(&key(), &uuid(), request, &header.protected),
                            "{fate:?} cut {cut}"
                        );
                    }
                    let opens = disk.keyslots.contains_key(&opener)
                        || found.iter().take(cut).any(|step| {
                            matches!(step, Step::Test { slot } if disk.keyslots.contains_key(slot))
                        });
                    assert!(opens, "{fate:?} cut {cut}");
                }
            }
        }

        #[test]
        fn covered_writes_are_tpm_pin_and_lockout_tokens() {
            let header = disk(Operation::TpmClear, false).header();
            for (step, expected) in [
                (Step::RemoveToken { token: 1 }, true),
                (Step::RemoveToken { token: 3 }, true),
                (Step::RemoveToken { token: 2 }, false),
                (Step::RemoveToken { token: NUMBER }, false),
                (Step::KillSlot { slot: 1, by: 2 }, false),
                (Step::AddKeyslot { slot: 3, by: 2 }, false),
                (Step::Test { slot: 1 }, false),
                (
                    Step::ImportToken {
                        token: Token::Lockout(crate::protected_plan::tests::lockout_token(false)),
                        id: Some(3),
                        replace: true,
                    },
                    true,
                ),
                (
                    Step::ImportToken {
                        token: fido(Some(3), crate::protected::FidoRole::Recovery),
                        id: None,
                        replace: false,
                    },
                    false,
                ),
            ] {
                assert_eq!(covered(&header, &step), expected, "{step:?}");
            }
        }

        #[test]
        fn a_plan_refuses_a_reseal_beside_a_clear_and_another_number() {
            let new = pin(None, 1, 3000);
            for operation in [Operation::TpmClear, Operation::TpmReenroll] {
                let disk = disk(operation, false);
                for fate in [Resolved::Left, Resolved::Removed(Removed::Unverified)] {
                    assert_eq!(
                        planned(&disk, BY_FIDO, Some(&new), NUMBER, fate).unwrap_err(),
                        Refusal::NoReseal
                    );
                }
            }
            let disk = disk(Operation::PinChange, false);
            assert_eq!(
                planned(&disk, BY_PIN, None, NUMBER, Resolved::PinChange).unwrap_err(),
                Refusal::NoReseal
            );
            assert_eq!(
                planned(&disk, BY_FIDO, None, 3, Resolved::Left).unwrap_err(),
                Refusal::UnknownToken { token: 3 }
            );
            assert_eq!(
                planned(&Disk::tpm_shape(), BY_FIDO, None, NUMBER, Resolved::Left).unwrap_err(),
                Refusal::UnknownToken { token: NUMBER }
            );
        }
    }
}

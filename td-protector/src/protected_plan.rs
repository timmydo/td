//! The pure planner for protected headers (DESIGN.md "Plans on protected
//! headers"; td-install/ENCRYPTION.md "Protected release", step 4): from
//! the header td's reader returned and what this boot released or
//! resealed, one plan's ordered cryptsetup steps, and the lockout token's
//! writes. It runs no cryptsetup and reaches no TPM; the protected release
//! executes the steps after the cap, once cryptsetup's own metadata agrees
//! with the header (`Plan::confirm`). Upgrading headers are 8f's planner's
//! and device-bound ones `transition`'s; this one refuses both.

use crate::cryptsetup;
use crate::lockout;
use crate::luks2::{self, Header, Metadata, MAX_TPM_PIN_TOKENS};
use crate::pin::PRIMARY_NAME_LEN;
use crate::protected::{Lockout, Shape, Token, TpmPin, UpdateRetry};
use crate::token::MAX_SLOT;
use std::ffi::OsString;
use std::fmt;
use std::path::Path;

/// LUKS2 holds token numbers 0 to 31.
const TOKEN_NUMBERS: usize = MAX_SLOT as usize + 1;

/// Which protector opened the volume this boot: its keyslot's test passed
/// with the secret it released.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opened {
    /// The tpm-pin token `token`, with the TPM PIN.
    TpmPin { token: u8 },
    /// The fido2 token `token`, with a security key.
    Fido2 { token: u8 },
    /// The recovery key, on the keyslot the `recovery-key` marker names.
    RecoveryKey,
}

/// One cryptsetup command of a protected plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// `open --test-passphrase` on `slot` with its own key.
    Test { slot: u8 },
    /// `luksKillSlot` of `slot`, keyslot `by`'s key authorizing it.
    KillSlot { slot: u8, by: u8 },
    /// `token remove` of token `token`.
    RemoveToken { token: u8 },
    /// `luksAddKey` of the resealed protector's keyslot `slot`, keyslot
    /// `by`'s key authorizing it and the new secret as its key file.
    AddKeyslot { slot: u8, by: u8 },
    /// `token import` of `token`: as the lowest free number when `id` is
    /// `None`, as `id` otherwise, replacing the token there when `replace`.
    ImportToken {
        token: Token,
        id: Option<u8>,
        replace: bool,
    },
}

/// What a step's standard input carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input<'a> {
    /// The key of this keyslot: the opener's or the resealed protector's.
    KeyOf(u8),
    /// This token's JSON.
    Token(&'a Token),
    Nothing,
}

impl Step {
    /// The step's exact arguments; `new_key` names the new secret's
    /// descriptor and appears only in `AddKeyslot`'s.
    pub fn args(&self, device: &Path, new_key: &Path) -> Vec<OsString> {
        match self {
            Self::Test { slot } => cryptsetup::test_args(device, *slot),
            Self::KillSlot { slot, .. } => cryptsetup::kill_slot_args(device, *slot),
            Self::RemoveToken { token } => cryptsetup::token_remove_args(device, *token),
            Self::AddKeyslot { slot, by } => cryptsetup::add_key_args(device, *by, *slot, new_key),
            Self::ImportToken {
                id: Some(id),
                replace: true,
                ..
            } => cryptsetup::token_replace_args(device, *id),
            Self::ImportToken { id, .. } => cryptsetup::token_import_args(device, *id),
        }
    }

    pub fn input(&self) -> Input<'_> {
        match self {
            Self::Test { slot } => Input::KeyOf(*slot),
            Self::KillSlot { by, .. } | Self::AddKeyslot { by, .. } => Input::KeyOf(*by),
            Self::RemoveToken { .. } => Input::Nothing,
            Self::ImportToken { token, .. } => Input::Token(token),
        }
    }
}

/// Why no protected plan runs this boot: each leaves the header as it is.
/// The volume still opens with the secret whose keyslot's test passed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The header is device-bound or upgrading (`luks2::Kind`): another
    /// planner's, or none before 8f.
    NotProtected,
    /// A fido2-primary token beside a tpm-pin or lockout token: no shape
    /// holds both ("Protectors and shapes").
    MixedShape,
    /// The TPM-primary shape without a `recovery-key` marker naming a
    /// keyslot: the shape requires the recovery key, and without the
    /// marker no plan can tell its keyslot from an orphan.
    NoRecoveryMarker,
    /// A keyslot that two tokens name, other than two tpm-pin tokens (an
    /// update's pre-seal).
    SharedKeyslot { slot: u8 },
    /// The opened token is not a token of its kind naming a keyslot, or
    /// the recovery key opened a volume whose marker names none.
    UnknownToken { token: u8 },
    /// A reseal is planned only after a fido2 or recovery-key release in
    /// the TPM-primary shape.
    NoReseal,
    /// The FIDO2-primary shape holds no lockout token: td takes the
    /// lockout hierarchy only for the TPM-primary shape.
    NoLockout,
    /// Every keyslot from 1 to 31 is taken.
    NoFreeKeyslot,
    /// Every token number would still be taken when a token is imported.
    NoFreeToken,
    /// A token to import is outside the token format.
    Token(String),
    /// The update's predicted protector is imported only beside the one
    /// tpm-pin token that released this boot, the plan having retired the
    /// rest.
    NotPredicted,
    /// No lockout token on the header verifies under the volume key for the
    /// storage primary the update's predicted protector was sealed under.
    NotAnchored,
    /// cryptsetup's own metadata could not be read or does not agree with
    /// the header the plan was computed from.
    Metadata(String),
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotProtected => f.write_str("the LUKS2 header is not a protected one"),
            Self::MixedShape => f.write_str(
                "the LUKS2 header holds a fido2-primary token beside a tpm-pin or lockout token",
            ),
            Self::NoRecoveryMarker => {
                f.write_str("the LUKS2 header has no recovery-key marker naming a keyslot")
            }
            Self::SharedKeyslot { slot } => {
                write!(f, "keyslot {slot} is named by more than one token")
            }
            Self::UnknownToken { token } => {
                write!(f, "token {token} is not a protector naming a keyslot")
            }
            Self::NoReseal => f.write_str("no reseal is planned after this release"),
            Self::NoLockout => f.write_str("the FIDO2-primary shape holds no lockout token"),
            Self::NoFreeKeyslot => f.write_str("the LUKS2 header has no free keyslot"),
            Self::NoFreeToken => f.write_str("the LUKS2 header has no free token number"),
            Self::Token(error) => write!(f, "the token to import: {error}"),
            Self::NotPredicted => f.write_str(
                "the header holds other than the one tpm-pin token that released this boot",
            ),
            Self::NotAnchored => f.write_str(
                "no lockout token on the header verifies under the volume key for the storage \
                 primary the protector was sealed under",
            ),
            Self::Metadata(error) => f.write_str(error),
        }
    }
}

/// A plan and the header view it was computed from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    view: Metadata,
    steps: Vec<Step>,
}

impl Plan {
    /// The steps, for inspection. The executor runs only those `confirm`
    /// returns.
    pub fn steps(&self) -> &[Step] {
        &self.steps
    }

    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// The steps to execute, once `metadata`, `luksDump
    /// --dump-json-metadata` taken before this boot's first write, agrees
    /// with the view the plan was computed from.
    pub fn confirm(self, metadata: &[u8]) -> Result<Vec<Step>, Refusal> {
        confirm(&self.view, metadata)?;
        Ok(self.steps)
    }
}

/// Whether `metadata` is the header `view`, td's reader's, as cryptsetup
/// itself reads it: the same keyslots and tokens of every type.
pub fn confirm(view: &Metadata, metadata: &[u8]) -> Result<(), Refusal> {
    let theirs = luks2::metadata(metadata).map_err(Refusal::Metadata)?;
    if theirs != *view {
        return Err(Refusal::Metadata(
            "cryptsetup's LUKS2 metadata disagrees with the header copy td read".into(),
        ));
    }
    Ok(())
}

/// The shape a protected header's tokens claim, read leniently for the
/// release's prompts: the FIDO2-primary shape when a fido2-primary token
/// exists, the TPM-primary shape otherwise. `check` says whether a plan
/// may write over it.
pub fn shape(header: &Header) -> Shape {
    let fido_primary = header.protected.iter().any(|(_, token)| {
        matches!(token, Token::Fido2(fido) if fido.role == crate::protected::FidoRole::Primary)
    });
    if fido_primary {
        Shape::Fido2
    } else {
        Shape::Tpm
    }
}

/// The `recovery-key` marker's token number and keyslot, when it names one.
pub fn marker(header: &Header) -> Option<(u8, u8)> {
    header
        .protected
        .iter()
        .find_map(|(number, token)| match token {
            Token::RecoveryKey {
                keyslot: Some(slot),
            } => Some((*number, *slot)),
            _ => None,
        })
}

/// Each tpm-pin token, orphans included, by number.
pub fn tpm_pins(header: &Header) -> Vec<(u8, &TpmPin)> {
    header
        .protected
        .iter()
        .filter_map(|(number, token)| match token {
            Token::TpmPin(pin) => Some((*number, pin)),
            _ => None,
        })
        .collect()
}

/// The lockout token, with its number.
pub fn lockout(header: &Header) -> Option<(u8, &Lockout)> {
    header
        .protected
        .iter()
        .find_map(|(number, token)| match token {
            Token::Lockout(lockout) => Some((*number, lockout)),
            _ => None,
        })
}

/// The header's own rules, before any plan: a protected header of one
/// shape, the TPM-primary shape with its recovery-key marker, and no
/// keyslot named by two tokens unless both are tpm-pin tokens.
pub fn check(header: &Header) -> Result<Shape, Refusal> {
    if header.kind() != luks2::Kind::Protected {
        return Err(Refusal::NotProtected);
    }
    let shape = shape(header);
    let tpm_held = header
        .protected
        .iter()
        .any(|(_, token)| matches!(token, Token::TpmPin(_) | Token::Lockout(_)));
    if shape == Shape::Fido2 && tpm_held {
        return Err(Refusal::MixedShape);
    }
    if shape == Shape::Tpm && marker(header).is_none() {
        return Err(Refusal::NoRecoveryMarker);
    }
    for slot in &header.keyslots {
        let named: Vec<bool> = header
            .protected
            .iter()
            .filter(|(_, token)| token.keyslot() == Some(*slot))
            .map(|(_, token)| matches!(token, Token::TpmPin(_)))
            .chain(
                header
                    .foreign
                    .iter()
                    .filter(|(_, slots)| slots.contains(slot))
                    .map(|_| false),
            )
            .collect();
        if named.len() > 1 && !named.iter().all(|pin| *pin) {
            return Err(Refusal::SharedKeyslot { slot: *slot });
        }
    }
    Ok(shape)
}

/// The keyslot `opened` names, checked against its kind.
fn opener(header: &Header, opened: Opened) -> Result<u8, Refusal> {
    let named = |number: u8, kind: &dyn Fn(&Token) -> bool| {
        header
            .protected
            .iter()
            .find(|(n, token)| *n == number && kind(token))
            .and_then(|(_, token)| token.keyslot())
            .ok_or(Refusal::UnknownToken { token: number })
    };
    match opened {
        Opened::TpmPin { token } => named(token, &|t| matches!(t, Token::TpmPin(_))),
        Opened::Fido2 { token } => named(token, &|t| matches!(t, Token::Fido2(_))),
        Opened::RecoveryKey => marker(header)
            .map(|(_, slot)| slot)
            .ok_or(Refusal::NoRecoveryMarker),
    }
}

/// Whether `opened` is the shape's primary protector: tpm-pin in the
/// TPM-primary shape, fido2-primary in the FIDO2-primary one.
fn primary(header: &Header, shape: Shape, opened: Opened) -> bool {
    match (shape, opened) {
        (Shape::Tpm, Opened::TpmPin { .. }) => true,
        (Shape::Fido2, Opened::Fido2 { token }) => header.protected.iter().any(|(n, t)| {
            *n == token
                && matches!(t, Token::Fido2(fido) if fido.role == crate::protected::FidoRole::Primary)
        }),
        _ => false,
    }
}

/// One boot's plan over a protected header (DESIGN.md "Plans on protected
/// headers"). `opened` is the protector whose keyslot's test passed this
/// boot, which the release ran: the plan does not test it again.
/// `reseal` is the confirmed reseal's new tpm-pin protector, verified
/// before the cap, its `keyslot` ignored: the plan names it.
///
/// - A release by the shape's primary protector removes the orphans: any
///   keyslot no token of any type names, killed only beside a marker
///   naming the recovery keyslot, and any td token naming no keyslot. A
///   tpm-pin release then retires every other tpm-pin token, and every
///   tpm-pin keyslot no remaining token names: two tokens may share one
///   keyslot, an update's pre-seal.
/// - A confirmed reseal, after a fido2 or recovery-key release in the
///   TPM-primary shape, removes the orphans, adds its keyslot under the
///   opener, imports its token, tests it, then retires every other
///   tpm-pin keyslot and token. Should its token exceed the two-token
///   bound, those are retired first, the opener's keyslot still opening.
/// - Any other release, a fido2-recovery or recovery-key one without a
///   reseal, plans nothing: the header, orphans included, is unchanged.
///
/// No plan kills the opener's keyslot, the marker's, or a fido2 one, and
/// every kill carries the key of a keyslot that remains.
pub fn plan(header: &Header, opened: Opened, reseal: Option<&TpmPin>) -> Result<Plan, Refusal> {
    let shape = check(header)?;
    let opener = opener(header, opened)?;
    let done = |steps: Vec<Step>| Plan {
        view: header.metadata(),
        steps,
    };
    let primary = primary(header, shape, opened);
    if reseal.is_some() && (primary || shape != Shape::Tpm) {
        return Err(Refusal::NoReseal);
    }
    if !primary && reseal.is_none() {
        return Ok(done(Vec::new()));
    }
    // Orphan keyslots: none named by a token of any type, foreign and
    // protected tokens both (`foreign_keyslots`). Without a marker naming
    // the recovery keyslot none is killed, since it could be that one.
    let named = header.foreign_keyslots();
    let orphan_slots: Vec<u8> = match marker(header) {
        Some((_, marker_slot)) => header
            .keyslots
            .iter()
            .copied()
            .filter(|slot| *slot != marker_slot && *slot != opener && !named.contains(slot))
            .collect(),
        None => Vec::new(),
    };
    let mut steps: Vec<Step> = orphan_slots
        .iter()
        .map(|slot| Step::KillSlot {
            slot: *slot,
            by: opener,
        })
        .collect();
    // Orphan tokens: td protectors naming no keyslot. A lockout or request
    // token names none by its format and is no orphan.
    let orphan_tokens: Vec<u8> = header
        .protected
        .iter()
        .filter(|(_, token)| {
            matches!(
                token,
                Token::TpmPin(_) | Token::Fido2(_) | Token::RecoveryKey { .. }
            ) && token.keyslot().is_none()
        })
        .map(|(number, _)| *number)
        .collect();
    steps.extend(
        orphan_tokens
            .iter()
            .map(|token| Step::RemoveToken { token: *token }),
    );
    // The tpm-pin tokens a retirement may take, orphans aside.
    let pins: Vec<(u8, u8)> = tpm_pins(header)
        .into_iter()
        .filter_map(|(number, pin)| pin.keyslot.map(|slot| (number, slot)))
        .collect();
    let retire = |steps: &mut Vec<Step>, kept: Option<(u8, u8)>, by: u8, old: &[(u8, u8)]| {
        let mut killed: Vec<u8> = Vec::new();
        for (_, slot) in old {
            let remains = kept.is_some_and(|(_, kept_slot)| kept_slot == *slot);
            if !remains && !killed.contains(slot) {
                steps.push(Step::KillSlot { slot: *slot, by });
                killed.push(*slot);
            }
        }
        for (number, _) in old {
            steps.push(Step::RemoveToken { token: *number });
        }
    };
    let Some(reseal) = reseal else {
        if let Opened::TpmPin { token } = opened {
            let old: Vec<(u8, u8)> = pins.iter().copied().filter(|(n, _)| *n != token).collect();
            retire(&mut steps, Some((token, opener)), opener, &old);
        }
        return Ok(done(steps));
    };
    // The reseal: retired first when its token would exceed the bound.
    let early = pins.len() >= MAX_TPM_PIN_TOKENS;
    if early {
        retire(&mut steps, None, opener, &pins);
    }
    let retired: Vec<u8> = if early {
        pins.iter().map(|(_, slot)| *slot).collect()
    } else {
        Vec::new()
    };
    let taken: Vec<u8> = header
        .keyslots
        .iter()
        .copied()
        .filter(|slot| !orphan_slots.contains(slot) && !retired.contains(slot))
        .collect();
    let removed = orphan_tokens.len() + if early { pins.len() } else { 0 };
    let held = header
        .token_numbers()
        .len()
        .checked_sub(removed)
        .ok_or(Refusal::NoFreeToken)?;
    if held >= TOKEN_NUMBERS {
        return Err(Refusal::NoFreeToken);
    }
    let slot = (1..=MAX_SLOT)
        .find(|slot| !taken.contains(slot))
        .ok_or(Refusal::NoFreeKeyslot)?;
    let token = Token::TpmPin(TpmPin {
        keyslot: Some(slot),
        ..reseal.clone()
    });
    token.encode().map_err(Refusal::Token)?;
    steps.push(Step::AddKeyslot { slot, by: opener });
    steps.push(Step::ImportToken {
        token,
        id: None,
        replace: false,
    });
    steps.push(Step::Test { slot });
    if !early {
        retire(&mut steps, None, slot, &pins);
    }
    Ok(done(steps))
}

/// The lockout token's import before a take (ENCRYPTION.md "Lockout"): as
/// the number the header's lockout token holds, replacing it, or else as
/// the lowest number no token holds. It returns that number, which the
/// take's later writes name. Only a header `check` admits in the
/// TPM-primary shape takes one.
pub fn lockout_import(header: &Header, token: &Lockout) -> Result<(u8, Step), Refusal> {
    if check(header)? != Shape::Tpm {
        return Err(Refusal::NoLockout);
    }
    let (id, replace) = match lockout(header) {
        Some((number, _)) => (number, true),
        None => (free_token(header)?, false),
    };
    let token = Token::Lockout(token.clone());
    token.encode().map_err(Refusal::Token)?;
    Ok((
        id,
        Step::ImportToken {
            token,
            id: Some(id),
            replace,
        },
    ))
}

/// The lockout token at `number`, rewritten with `proven` set, once a
/// take's LockReset or a proof under it succeeded.
pub fn lockout_proven(number: u8, token: &Lockout) -> Result<Step, Refusal> {
    let token = Token::Lockout(Lockout {
        proven: true,
        ..token.clone()
    });
    token.encode().map_err(Refusal::Token)?;
    Ok(Step::ImportToken {
        token,
        id: Some(number),
        replace: true,
    })
}

/// An unproven lockout token's removal after a refused proof.
pub fn lockout_remove(number: u8) -> Step {
    Step::RemoveToken { token: number }
}

/// The update-retry token, with its number.
pub fn update_retry(header: &Header) -> Option<(u8, &UpdateRetry)> {
    header
        .protected
        .iter()
        .find_map(|(number, token)| match token {
            Token::UpdateRetry(retry) => Some((*number, retry)),
            _ => None,
        })
}

/// The lowest token number no token holds.
fn free_token(header: &Header) -> Result<u8, Refusal> {
    let used = header.token_numbers();
    (0..=MAX_SLOT)
        .find(|number| !used.contains(number))
        .ok_or(Refusal::NoFreeToken)
}

/// A selector update's commit (ENCRYPTION.md "Selector update"): the
/// pre-seal's object as a second tpm-pin token naming `keyslot`, the
/// keyslot the PIN released this boot, beside that one token alone, as the
/// lowest free number. Only a header `check` admits in the TPM-primary
/// shape takes one, and only when its lockout token verifies under
/// `volume_key` for the object's primary, as a reseal's import requires.
pub fn predicted_import(
    header: &Header,
    keyslot: u8,
    token: &TpmPin,
    volume_key: &[u8],
) -> Result<Step, Refusal> {
    if check(header)? != Shape::Tpm {
        return Err(Refusal::NotPredicted);
    }
    match tpm_pins(header).as_slice() {
        [(_, released)] if released.keyslot == Some(keyslot) => {}
        _ => return Err(Refusal::NotPredicted),
    }
    if !anchors(header, volume_key, &token.primary) {
        return Err(Refusal::NotAnchored);
    }
    free_token(header)?;
    let token = Token::TpmPin(TpmPin {
        keyslot: Some(keyslot),
        ..token.clone()
    });
    token.encode().map_err(Refusal::Token)?;
    Ok(Step::ImportToken {
        token,
        id: None,
        replace: false,
    })
}

/// The header's lockout token verifies under `volume_key` and the header's
/// UUID for the storage primary `primary`: the anchor a reseal's and a
/// predicted protector's import require (ENCRYPTION.md "Lockout").
pub fn anchors(header: &Header, volume_key: &[u8], primary: &[u8; PRIMARY_NAME_LEN]) -> bool {
    lockout::volume_uuid(&header.uuid).is_some_and(|uuid| {
        lockout(header)
            .is_some_and(|(_, token)| lockout::open(volume_key, &uuid, token, primary).is_ok())
    })
}

/// The update-retry token's write: replacing the header's own, or as the
/// lowest free number. Only the TPM-primary shape stages an update.
pub fn retry_write(header: &Header, retry: &UpdateRetry) -> Result<Step, Refusal> {
    if check(header)? != Shape::Tpm {
        return Err(Refusal::NotProtected);
    }
    let (id, replace) = match update_retry(header) {
        Some((number, _)) => (number, true),
        None => (free_token(header)?, false),
    };
    let token = Token::UpdateRetry(retry.clone());
    token.encode().map_err(Refusal::Token)?;
    Ok(Step::ImportToken {
        token,
        id: Some(id),
        replace,
    })
}

/// The update-retry token's removal, when the header holds one: a commit
/// and a deletion of the staged update both remove it.
pub fn retry_remove(header: &Header) -> Option<Step> {
    update_retry(header).map(|(number, _)| Step::RemoveToken { token: number })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::luks2::HeaderCopy;
    use crate::pin::PinPcrs;
    use crate::protected::{Fido2, FidoRole, Operation};
    use std::collections::BTreeMap;
    use td_tpm::SealedObject;

    /// The recovery key's identity in the model.
    const RECOVERY_KEY: u32 = 1000;
    /// A security key's passphrase in the model.
    const FIDO_KEY: u32 = 2000;

    /// A tpm-pin protector sealed to boot chain `chain`, releasing `key`.
    fn pin(slot: Option<u8>, chain: u8, key: u32) -> TpmPin {
        TpmPin {
            keyslot: slot,
            pcrs: PinPcrs::Selector,
            primary: [0x0b; 34],
            salt: [0x5a; 32],
            sealed: SealedObject {
                public: [&[chain][..], &key.to_be_bytes()].concat(),
                private: vec![1],
            },
        }
    }

    fn chain_of(pin: &TpmPin) -> u8 {
        pin.sealed.public[0]
    }

    fn key_of(pin: &TpmPin) -> u32 {
        u32::from_be_bytes(pin.sealed.public[1..5].try_into().unwrap())
    }

    fn fido(slot: Option<u8>, role: FidoRole) -> Token {
        Token::Fido2(Fido2 {
            keyslot: slot,
            role,
            credential: vec![0xc0; 16],
            salt: [3; 32],
            x: [4; 32],
            y: [5; 32],
        })
    }

    fn lockout_token(proven: bool) -> Lockout {
        Lockout {
            primary: [0x0b; 34],
            nonce: [1; 32],
            ciphertext: [2; 32],
            tag: [3; 32],
            proven,
        }
    }

    /// A LUKS2 header as cryptsetup keeps it: each keyslot's key, td's
    /// protected tokens and foreign tokens.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Disk {
        keyslots: BTreeMap<u8, u32>,
        tokens: BTreeMap<u8, Token>,
        foreign: BTreeMap<u8, Vec<u8>>,
    }

    impl Disk {
        /// The TPM-primary shape as an upgrade leaves it: the recovery key
        /// in keyslot 0 and its marker as token 0, the tpm-pin protector of
        /// chain 1 in keyslot 1 as token 1, a fido2-recovery token 2 on
        /// keyslot 2, and the lockout token 3.
        fn tpm_shape() -> Self {
            let mut disk = Self {
                keyslots: BTreeMap::from([(0, RECOVERY_KEY), (1, 101), (2, FIDO_KEY)]),
                tokens: BTreeMap::new(),
                foreign: BTreeMap::new(),
            };
            disk.tokens
                .insert(0, Token::RecoveryKey { keyslot: Some(0) });
            disk.tokens.insert(1, Token::TpmPin(pin(Some(1), 1, 101)));
            disk.tokens.insert(2, fido(Some(2), FidoRole::Recovery));
            disk.tokens.insert(3, Token::Lockout(lockout_token(true)));
            disk
        }

        /// The FIDO2-primary shape: two fido2-primary tokens on keyslots 1
        /// and 2 and a fido2-recovery one on keyslot 3, the recovery key
        /// in keyslot 0 and its marker.
        fn fido_shape() -> Self {
            let mut disk = Self {
                keyslots: BTreeMap::from([
                    (0, RECOVERY_KEY),
                    (1, FIDO_KEY + 1),
                    (2, FIDO_KEY + 2),
                    (3, FIDO_KEY + 3),
                ]),
                tokens: BTreeMap::new(),
                foreign: BTreeMap::new(),
            };
            disk.tokens
                .insert(0, Token::RecoveryKey { keyslot: Some(0) });
            disk.tokens.insert(1, fido(Some(1), FidoRole::Primary));
            disk.tokens.insert(2, fido(Some(2), FidoRole::Primary));
            disk.tokens.insert(3, fido(Some(3), FidoRole::Recovery));
            disk
        }

        fn header(&self) -> Header {
            Header {
                copy: HeaderCopy::Primary,
                seqid: 1,
                hdr_size: 0x4000,
                uuid: "0f0e0d0c-0b0a-4908-8706-050403020100".into(),
                label: "td-system".into(),
                keyslots: self.keyslots.keys().copied().collect(),
                tokens: Vec::new(),
                orphans: Vec::new(),
                foreign: self
                    .foreign
                    .iter()
                    .map(|(number, slots)| (*number, slots.clone()))
                    .collect(),
                protected: self
                    .tokens
                    .iter()
                    .map(|(number, token)| (*number, token.clone()))
                    .collect(),
            }
        }

        /// The header as `luksDump --dump-json-metadata` prints it, which
        /// the reader's own bounds then judge.
        fn dump(&self) -> Vec<u8> {
            let keyslots: Vec<String> = self
                .keyslots
                .keys()
                .map(|slot| format!("\"{slot}\": {{\"type\": \"luks2\"}}"))
                .collect();
            let mut tokens: Vec<(u8, String)> = self
                .tokens
                .iter()
                .map(|(number, token)| (*number, token.encode().unwrap()))
                .collect();
            tokens.extend(self.foreign.iter().map(|(number, slots)| {
                let slots: Vec<String> = slots.iter().map(|s| format!("\"{s}\"")).collect();
                (
                    *number,
                    format!(
                        "{{\"type\":\"systemd-tpm2\",\"keyslots\":[{}]}}",
                        slots.join(",")
                    ),
                )
            }));
            tokens.sort();
            let tokens: Vec<String> = tokens
                .iter()
                .map(|(number, json)| format!("\"{number}\": {json}"))
                .collect();
            format!(
                "{{\"keyslots\": {{{}}}, \"tokens\": {{{}}}, \"segments\": {{}}, \"digests\": {{}}, \"config\": {{}}}}",
                keyslots.join(", "),
                tokens.join(", ")
            )
            .into_bytes()
        }

        /// One step as cryptsetup commits it, the new keyslot's key `new`.
        fn apply(&mut self, step: &Step, new: u32) {
            match step {
                Step::Test { slot } => assert!(self.keyslots.contains_key(slot), "{step:?}"),
                Step::KillSlot { slot, by } => {
                    // cryptsetup destroys it only once another keyslot's key
                    // opens: `by`'s, which must remain.
                    assert!(slot != by && self.keyslots.contains_key(by), "{step:?}");
                    assert!(self.keyslots.remove(slot).is_some(), "{step:?}");
                    for token in self.tokens.values_mut() {
                        if token.keyslot() == Some(*slot) {
                            strip(token);
                        }
                    }
                    for slots in self.foreign.values_mut() {
                        slots.retain(|s| s != slot);
                    }
                }
                Step::RemoveToken { token } => {
                    assert!(self.tokens.remove(token).is_some(), "{step:?}")
                }
                Step::AddKeyslot { slot, by } => {
                    assert!(self.keyslots.contains_key(by), "{step:?}");
                    assert!(self.keyslots.insert(*slot, new).is_none(), "{step:?}");
                }
                Step::ImportToken { token, id, replace } => {
                    if let Some(slot) = token.keyslot() {
                        assert!(self.keyslots.contains_key(&slot), "{step:?}");
                    }
                    let number = match id {
                        Some(id) => {
                            assert_eq!(self.tokens.contains_key(id), *replace, "{step:?}");
                            assert!(!self.foreign.contains_key(id), "{step:?}");
                            *id
                        }
                        None => (0..=MAX_SLOT)
                            .find(|n| !self.tokens.contains_key(n) && !self.foreign.contains_key(n))
                            .unwrap(),
                    };
                    self.tokens.insert(number, token.clone());
                }
            }
            // The reader admits every state a step leaves.
            luks2::metadata(&self.dump()).unwrap();
        }

        /// The keyslots, and each protected token as (number, keyslot).
        fn shape(&self) -> (Vec<u8>, Vec<(u8, Option<u8>)>) {
            (
                self.keyslots.keys().copied().collect(),
                self.tokens
                    .iter()
                    .map(|(number, token)| (*number, token.keyslot()))
                    .collect(),
            )
        }
    }

    /// cryptsetup strips a destroyed keyslot from every token naming it.
    fn strip(token: &mut Token) {
        match token {
            Token::TpmPin(TpmPin { keyslot, .. })
            | Token::Fido2(Fido2 { keyslot, .. })
            | Token::RecoveryKey { keyslot } => *keyslot = None,
            _ => {}
        }
    }

    fn steps(disk: &Disk, opened: Opened, reseal: Option<&TpmPin>) -> Vec<Step> {
        plan(&disk.header(), opened, reseal)
            .unwrap()
            .confirm(&disk.dump())
            .unwrap()
    }

    fn refused(disk: &Disk, opened: Opened, reseal: Option<&TpmPin>) -> Refusal {
        plan(&disk.header(), opened, reseal).unwrap_err()
    }

    fn import(slot: u8, chain: u8, key: u32) -> Step {
        Step::ImportToken {
            token: Token::TpmPin(pin(Some(slot), chain, key)),
            id: None,
            replace: false,
        }
    }

    #[test]
    fn a_pin_release_retires_the_other_tpm_pin_and_the_orphans() {
        let mut disk = Disk::tpm_shape();
        // A superseded tpm-pin protector, an orphan keyslot an interrupted
        // add left, and an orphan fido2 token whose keyslot was killed.
        disk.keyslots.insert(3, 103);
        disk.tokens.insert(4, Token::TpmPin(pin(Some(3), 9, 103)));
        disk.keyslots.insert(5, 105);
        disk.tokens.insert(6, fido(None, FidoRole::Recovery));
        let plan = steps(&disk, Opened::TpmPin { token: 1 }, None);
        assert_eq!(
            plan,
            [
                Step::KillSlot { slot: 5, by: 1 },
                Step::RemoveToken { token: 6 },
                Step::KillSlot { slot: 3, by: 1 },
                Step::RemoveToken { token: 4 },
            ]
        );
        for step in &plan {
            disk.apply(step, 0);
        }
        assert_eq!(
            disk.shape(),
            (
                vec![0, 1, 2],
                vec![(0, Some(0)), (1, Some(1)), (2, Some(2)), (3, None)]
            )
        );
        // The next boot's plan is empty.
        assert!(steps(&disk, Opened::TpmPin { token: 1 }, None).is_empty());
    }

    /// An update's pre-seal leaves two tpm-pin tokens on one keyslot: the
    /// boot after retires the token that did not release and keeps the
    /// keyslot the released one names.
    #[test]
    fn a_shared_tpm_pin_keyslot_is_kept_for_the_token_that_released() {
        let mut disk = Disk::tpm_shape();
        disk.tokens.insert(4, Token::TpmPin(pin(Some(1), 2, 101)));
        for (released, retired) in [(4, 1), (1, 4)] {
            let plan = steps(&disk, Opened::TpmPin { token: released }, None);
            assert_eq!(plan, [Step::RemoveToken { token: retired }]);
        }
    }

    /// A fido2-recovery or recovery-key release without a reseal plans
    /// nothing, orphans included: no tpm-pin protector is retired by a
    /// boot that did not seal one.
    #[test]
    fn a_recovery_release_without_a_reseal_plans_nothing() {
        let mut disk = Disk::tpm_shape();
        disk.keyslots.insert(5, 105);
        disk.tokens.insert(6, Token::TpmPin(pin(None, 9, 0)));
        for opened in [Opened::Fido2 { token: 2 }, Opened::RecoveryKey] {
            assert!(steps(&disk, opened, None).is_empty(), "{opened:?}");
        }
    }

    #[test]
    fn a_reseal_adds_under_the_opener_and_retires_every_other_tpm_pin() {
        for (opened, by) in [(Opened::RecoveryKey, 0), (Opened::Fido2 { token: 2 }, 2)] {
            let mut disk = Disk::tpm_shape();
            disk.keyslots.insert(5, 105);
            let new = pin(None, 7, 107);
            let plan = steps(&disk, opened, Some(&new));
            assert_eq!(
                plan,
                [
                    Step::KillSlot { slot: 5, by },
                    Step::AddKeyslot { slot: 3, by },
                    import(3, 7, 107),
                    Step::Test { slot: 3 },
                    Step::KillSlot { slot: 1, by: 3 },
                    Step::RemoveToken { token: 1 },
                ],
                "{opened:?}"
            );
            for step in &plan {
                disk.apply(step, 107);
            }
            assert_eq!(
                disk.shape(),
                (
                    vec![0, 2, 3],
                    vec![(0, Some(0)), (2, Some(2)), (3, None), (4, Some(3))]
                )
            );
        }
    }

    /// Two tpm-pin tokens and a reseal would hold three, over the reader's
    /// bound: both are retired first, the opener's keyslot opening.
    #[test]
    fn a_reseal_beside_two_tpm_pin_tokens_retires_them_first() {
        let mut disk = Disk::tpm_shape();
        disk.keyslots.insert(3, 103);
        disk.tokens.insert(4, Token::TpmPin(pin(Some(3), 2, 103)));
        let new = pin(None, 7, 107);
        let plan = steps(&disk, Opened::RecoveryKey, Some(&new));
        assert_eq!(
            plan,
            [
                Step::KillSlot { slot: 1, by: 0 },
                Step::KillSlot { slot: 3, by: 0 },
                Step::RemoveToken { token: 1 },
                Step::RemoveToken { token: 4 },
                Step::AddKeyslot { slot: 1, by: 0 },
                import(1, 7, 107),
                Step::Test { slot: 1 },
            ]
        );
        // `apply` reads every state back through the reader's bounds.
        for step in &plan {
            disk.apply(step, 107);
        }
        let pins = disk
            .tokens
            .values()
            .filter(|t| matches!(t, Token::TpmPin(_)))
            .count();
        assert_eq!(pins, 1);
    }

    #[test]
    fn a_reseal_is_planned_only_after_a_secondary_release_in_the_tpm_shape() {
        let new = pin(None, 7, 107);
        assert_eq!(
            refused(&Disk::tpm_shape(), Opened::TpmPin { token: 1 }, Some(&new)),
            Refusal::NoReseal
        );
        let disk = Disk::fido_shape();
        for opened in [
            Opened::Fido2 { token: 1 },
            Opened::Fido2 { token: 3 },
            Opened::RecoveryKey,
        ] {
            assert_eq!(refused(&disk, opened, Some(&new)), Refusal::NoReseal);
        }
    }

    /// The FIDO2-primary shape: a primary release removes orphans and
    /// never a fido2 keyslot or the recovery keyslot; a recovery one plans
    /// nothing.
    #[test]
    fn a_fido2_primary_release_removes_only_orphans() {
        let mut disk = Disk::fido_shape();
        disk.keyslots.insert(6, 106);
        disk.tokens.insert(5, fido(None, FidoRole::Recovery));
        assert_eq!(
            steps(&disk, Opened::Fido2 { token: 2 }, None),
            [
                Step::KillSlot { slot: 6, by: 2 },
                Step::RemoveToken { token: 5 }
            ]
        );
        assert!(steps(&disk, Opened::Fido2 { token: 3 }, None).is_empty());
        assert!(steps(&disk, Opened::RecoveryKey, None).is_empty());
        // Without a marker no keyslot is an orphan: keyslot 0 could hold
        // the recovery key.
        disk.tokens.remove(&0);
        assert_eq!(
            steps(&disk, Opened::Fido2 { token: 2 }, None),
            [Step::RemoveToken { token: 5 }]
        );
        assert_eq!(
            refused(&disk, Opened::RecoveryKey, None),
            Refusal::NoRecoveryMarker
        );
    }

    /// Forged headers: each is refused before any step, so nothing is
    /// written; the release still tests whatever it releases.
    #[test]
    fn forged_headers_plan_nothing() {
        let new = pin(None, 7, 107);
        let mut cases: Vec<(&str, Disk, Refusal)> = Vec::new();
        let mut disk = Disk::tpm_shape();
        disk.tokens
            .insert(4, Token::TpmPin(pin(Some(2), 1, FIDO_KEY)));
        cases.push((
            "a tpm-pin token on the fido2 keyslot",
            disk,
            Refusal::SharedKeyslot { slot: 2 },
        ));
        let mut disk = Disk::tpm_shape();
        disk.tokens
            .insert(4, Token::TpmPin(pin(Some(0), 1, RECOVERY_KEY)));
        cases.push((
            "a tpm-pin token on the recovery keyslot",
            disk,
            Refusal::SharedKeyslot { slot: 0 },
        ));
        let mut disk = Disk::tpm_shape();
        disk.keyslots.insert(3, 103);
        disk.tokens.insert(4, fido(Some(1), FidoRole::Recovery));
        cases.push((
            "a fido2 token on the tpm-pin keyslot",
            disk,
            Refusal::SharedKeyslot { slot: 1 },
        ));
        let mut disk = Disk::tpm_shape();
        disk.foreign.insert(9, vec![1]);
        cases.push((
            "a foreign token on the tpm-pin keyslot",
            disk,
            Refusal::SharedKeyslot { slot: 1 },
        ));
        let mut disk = Disk::tpm_shape();
        disk.tokens.insert(4, fido(Some(2), FidoRole::Primary));
        disk.tokens.insert(2, fido(None, FidoRole::Recovery));
        cases.push((
            "a fido2-primary token beside a tpm-pin one",
            disk,
            Refusal::MixedShape,
        ));
        let mut disk = Disk::fido_shape();
        disk.tokens.insert(4, Token::Lockout(lockout_token(true)));
        cases.push((
            "a lockout token in the FIDO2 shape",
            disk,
            Refusal::MixedShape,
        ));
        let mut disk = Disk::tpm_shape();
        disk.tokens.remove(&0);
        cases.push((
            "the TPM shape without a marker",
            disk,
            Refusal::NoRecoveryMarker,
        ));
        let mut disk = Disk::tpm_shape();
        disk.tokens.insert(0, Token::RecoveryKey { keyslot: None });
        cases.push((
            "a marker naming no keyslot",
            disk,
            Refusal::NoRecoveryMarker,
        ));
        let mut disk = Disk::tpm_shape();
        disk.tokens.insert(
            5,
            Token::Upgrade {
                shape: Shape::Tpm,
                from: [0; 32],
            },
        );
        cases.push(("an upgrading header", disk, Refusal::NotProtected));
        for (case, disk, refusal) in cases {
            for opened in [
                Opened::TpmPin { token: 1 },
                Opened::Fido2 { token: 2 },
                Opened::RecoveryKey,
            ] {
                for reseal in [None, Some(&new)] {
                    assert_eq!(refused(&disk, opened, reseal), refusal, "{case}");
                }
            }
            assert_eq!(check(&disk.header()), Err(refusal.clone()), "{case}");
            assert_eq!(
                lockout_import(&disk.header(), &lockout_token(false)),
                Err(refusal),
                "{case}"
            );
        }
        // A device-bound header is the device-bound planner's.
        let mut header = Disk::tpm_shape().header();
        header.protected.clear();
        assert_eq!(check(&header), Err(Refusal::NotProtected));
    }

    #[test]
    fn the_opened_token_must_be_its_kind_naming_a_keyslot() {
        let mut disk = Disk::tpm_shape();
        disk.tokens.insert(6, Token::TpmPin(pin(None, 1, 0)));
        for (opened, token) in [
            (Opened::TpmPin { token: 2 }, 2),
            (Opened::TpmPin { token: 3 }, 3),
            (Opened::TpmPin { token: 6 }, 6),
            (Opened::Fido2 { token: 1 }, 1),
            (Opened::Fido2 { token: 9 }, 9),
        ] {
            assert_eq!(
                refused(&disk, opened, None),
                Refusal::UnknownToken { token },
                "{opened:?}"
            );
        }
        // A request token names no keyslot and is never an orphan.
        disk.tokens.insert(
            7,
            Token::Request {
                operation: Operation::PinChange,
            },
        );
        assert_eq!(
            steps(&disk, Opened::TpmPin { token: 1 }, None),
            [Step::RemoveToken { token: 6 }]
        );
    }

    #[test]
    fn metadata_that_disagrees_runs_no_step() {
        let disk = Disk::tpm_shape();
        let mut other = disk.clone();
        other.tokens.insert(1, Token::TpmPin(pin(Some(1), 9, 101)));
        let plan = plan(&disk.header(), Opened::TpmPin { token: 1 }, None).unwrap();
        assert!(matches!(
            plan.clone().confirm(&other.dump()),
            Err(Refusal::Metadata(_))
        ));
        assert!(matches!(plan.confirm(b"{"), Err(Refusal::Metadata(_))));
    }

    #[test]
    fn a_reseal_with_no_free_keyslot_or_token_number_is_refused() {
        let new = pin(None, 7, 107);
        let mut disk = Disk::tpm_shape();
        for slot in 3..=31u8 {
            disk.keyslots.insert(slot, 100 + u32::from(slot));
        }
        disk.foreign.insert(4, (3..=31u8).collect());
        assert_eq!(
            refused(&disk, Opened::RecoveryKey, Some(&new)),
            Refusal::NoFreeKeyslot
        );
        let mut disk = Disk::tpm_shape();
        for number in 4..=31u8 {
            disk.foreign.insert(number, Vec::new());
        }
        assert_eq!(
            refused(&disk, Opened::RecoveryKey, Some(&new)),
            Refusal::NoFreeToken
        );
        // An object over the token's bounds is refused before any step.
        let mut big = pin(None, 7, 107);
        big.sealed.public = vec![0; 129];
        assert!(matches!(
            refused(&Disk::tpm_shape(), Opened::RecoveryKey, Some(&big)),
            Refusal::Token(_)
        ));
    }

    /// The lockout token's writes: the import replaces the token there or
    /// takes the lowest free number, a proof marks it proven in place, and
    /// a refused proof removes it.
    #[test]
    fn the_lockout_token_is_written_where_the_planner_says() {
        let token = lockout_token(false);
        let mut disk = Disk::tpm_shape();
        let (number, step) = lockout_import(&disk.header(), &token).unwrap();
        assert_eq!(number, 3);
        assert_eq!(
            step,
            Step::ImportToken {
                token: Token::Lockout(token.clone()),
                id: Some(3),
                replace: true
            }
        );
        disk.apply(&step, 0);
        disk.tokens.remove(&3);
        disk.foreign.insert(4, vec![]);
        let (number, step) = lockout_import(&disk.header(), &token).unwrap();
        assert_eq!(number, 3);
        assert!(matches!(
            step,
            Step::ImportToken {
                id: Some(3),
                replace: false,
                ..
            }
        ));
        disk.apply(&step, 0);
        let step = lockout_proven(3, &token).unwrap();
        disk.apply(&step, 0);
        assert_eq!(disk.tokens[&3], Token::Lockout(lockout_token(true)));
        disk.apply(&lockout_remove(3), 0);
        assert!(!disk.tokens.contains_key(&3));
        assert_eq!(
            lockout_import(&Disk::fido_shape().header(), &token),
            Err(Refusal::NoLockout)
        );
    }

    /// Which protector the model's next boot releases: the lowest tpm-pin
    /// token sealed to the running chain whose keyslot holds its key, or
    /// else the recovery key, which then reseals.
    fn next_boot(disk: &Disk, chain: u8) -> Opened {
        disk.tokens
            .iter()
            .find_map(|(number, token)| match token {
                Token::TpmPin(pin)
                    if chain_of(pin) == chain
                        && pin
                            .keyslot
                            .is_some_and(|slot| disk.keyslots.get(&slot) == Some(&key_of(pin))) =>
                {
                    Some(Opened::TpmPin { token: *number })
                }
                _ => None,
            })
            .unwrap_or(Opened::RecoveryKey)
    }

    /// Boots until a plan is empty, from a cut after `cut` steps of the
    /// first, each recovery boot's reseal sealing a fresh object to
    /// `chain`.
    fn converge(mut disk: Disk, chain: u8, first: Opened, cut: usize) -> Disk {
        let mut opened = first;
        let mut fresh = 200;
        for boot in 0..4 {
            let reseal = (opened == Opened::RecoveryKey).then(|| {
                fresh += 1;
                pin(None, chain, fresh)
            });
            let plan = steps(&disk, opened, reseal.as_ref());
            if plan.is_empty() {
                return disk;
            }
            let run = if boot == 0 { cut } else { plan.len() };
            for step in plan.iter().take(run) {
                disk.apply(step, fresh);
            }
            opened = next_boot(&disk, chain);
        }
        panic!("no empty plan within four boots: {disk:?}");
    }

    /// Every plan cut after each of its steps: the next boot's plan
    /// completes it, the header always within the reader's bounds, and
    /// every cut converges on the same shape: the marker's keyslot, the
    /// fido2 one, one tpm-pin protector of the running chain, the lockout.
    #[test]
    fn every_cut_of_a_plan_converges_on_the_next_boot() {
        let mut superseded = Disk::tpm_shape();
        superseded.keyslots.insert(3, 103);
        superseded
            .tokens
            .insert(4, Token::TpmPin(pin(Some(3), 2, 103)));
        superseded.keyslots.insert(5, 105);
        for (case, disk, chain, opened, reseal) in [
            (
                "pin release",
                superseded.clone(),
                1,
                Opened::TpmPin { token: 1 },
                false,
            ),
            (
                "reseal, one old",
                Disk::tpm_shape(),
                7,
                Opened::RecoveryKey,
                true,
            ),
            ("reseal, two old", superseded, 7, Opened::RecoveryKey, true),
        ] {
            let new = reseal.then(|| pin(None, chain, 201));
            let count = plan(&disk.header(), opened, new.as_ref())
                .unwrap()
                .steps()
                .len();
            for cut in 0..=count {
                let done = converge(disk.clone(), chain, opened, cut);
                let pins: Vec<&TpmPin> = done
                    .tokens
                    .values()
                    .filter_map(|t| match t {
                        Token::TpmPin(pin) => Some(pin),
                        _ => None,
                    })
                    .collect();
                assert_eq!(pins.len(), 1, "{case} cut {cut}: {done:?}");
                assert_eq!(chain_of(pins[0]), chain, "{case} cut {cut}");
                let slot = pins[0].keyslot.unwrap();
                assert_eq!(done.keyslots[&slot], key_of(pins[0]), "{case} cut {cut}");
                let mut keyslots = vec![0, 2, slot];
                keyslots.sort_unstable();
                assert_eq!(
                    done.keyslots.keys().copied().collect::<Vec<_>>(),
                    keyslots,
                    "{case} cut {cut}"
                );
                assert_eq!(done.keyslots[&0], RECOVERY_KEY);
                assert_eq!(done.tokens.len(), 4, "{case} cut {cut}: {done:?}");
            }
        }
    }
}

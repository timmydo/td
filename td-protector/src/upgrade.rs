//! The re-encrypting upgrade's pure planner (DESIGN.md "Protected roles";
//! td-install/ENCRYPTION.md "Re-encrypting upgrade"): from the header td's
//! reader returned and its upgrade view, the upgrade's state U0 to U5, what
//! the selector gathers before the cap, and one state's ordered steps after
//! it. It runs no cryptsetup and reaches no TPM. The selector runs a plan
//! once cryptsetup's own metadata agrees with the view (`Plan::confirm`),
//! reads the header again and plans the next state, until the `td-upgrade`
//! token is gone. Protected headers are `protected_plan`'s and device-bound
//! ones `transition`'s; this one also removes the pending fido2 tokens a
//! cancelled or interrupted phase 1 leaves on a device-bound header.

use crate::cryptsetup::{self, Resilience};
use crate::luks2::{self, Header, Kind, Metadata, UpgradeView, Volume, JSON_AREA_LEN};
use crate::pin::PinPcrs;
use crate::protected::{
    Fido2, FidoRole, Lockout, Shape, Token, TpmPin, FIELD_LEN, MAX_CREDENTIAL_ID,
    MAX_PIN_PRIVATE_BYTES, MAX_PIN_PUBLIC_BYTES,
};
use crate::token::{Role, MAX_SLOT};
use std::ffi::OsString;
use std::fmt;
use std::path::Path;
use td_tpm::SealedObject;

/// LUKS2 holds token numbers 0 to 31.
const TOKEN_NUMBERS: usize = MAX_SLOT as usize + 1;

/// The most an added keyslot grows the JSON area's text: 305 bytes
/// measured with the pinned cryptsetup for td's PBKDF2 keyslot, two more
/// for a two-digit number in the keyslots object and the digest's list.
pub const KEYSLOT_JSON: usize = 320;
/// The most a reencryption grows it: `reencrypt --init-only` adds 1208
/// bytes with journal resilience and 1244 with checksum, measured with the
/// pinned cryptsetup, and an interrupted reencryption splits a segment,
/// 138 bytes more measured on a 20 GiB file; rounded up.
pub const REENCRYPT_JSON: usize = 1536;
/// A token's number, quotes, colon and comma beside its own text.
const TOKEN_ENTRY: usize = 6;
/// The longest lockout token's text, which a take imports.
const LOCKOUT_JSON: usize = 376;

/// The upgrade's state, read from the header alone (ENCRYPTION.md's table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum State {
    /// The device-bound protector beside keyslot 0, the pending tokens and
    /// `td-upgrade`; the data segment as phase 1 found it.
    U0,
    /// No device-bound protector; the data segment as phase 1 found it.
    U1,
    /// cryptsetup's online-reencryption requirement is present.
    U2,
    /// The data segment changed; no requirement and no marker.
    U3,
    /// The marker, and some pending token, or in the TPM shape the tpm-pin
    /// or lockout token, missing.
    U4,
    /// The marker and every protector the shape needs.
    U5,
}

impl State {
    /// Only U0 may still be cancelled: after its commit nothing cancels.
    pub fn cancellable(self) -> bool {
        self == Self::U0
    }

    /// Only U0 waits for mains power; a resumption on battery warns and
    /// continues.
    pub fn needs_mains(self) -> bool {
        self == Self::U0
    }
}

/// Whether a machine's power supplies let the upgrade start: a `Mains`
/// supply online, or no `Battery` one. Each is a `/sys/class/power_supply`
/// entry's `type` and whether its `online` reads `1`.
pub fn on_mains(supplies: &[(&str, bool)]) -> bool {
    supplies
        .iter()
        .any(|(kind, online)| *kind == "Mains" && *online)
        || !supplies.iter().any(|(kind, _)| *kind == "Battery")
}

/// The reencryption's resilience for a disk whose
/// `queue/atomic_write_unit_max_bytes` reads `unit`: checksum where whole
/// 4096-byte sectors are written atomically, journal otherwise or when the
/// disk states nothing.
pub fn resilience(unit: Option<u64>) -> Resilience {
    match unit {
        Some(unit) if unit >= 4096 => Resilience::Checksum,
        _ => Resilience::Journal,
    }
}

/// A secret the selector holds this boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// The recovery key, which authorizes every step.
    Recovery,
    /// The passphrase derived from pending fido2 token `token`'s key.
    Fido2 { token: u8 },
    /// The secret sealed for the new tpm-pin protector.
    TpmPin,
}

/// One step of an upgrade plan: a cryptsetup command, or the lockout take,
/// which the selector runs ("Lockout").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// `open --test-passphrase` on `slot` with `key`.
    Test { slot: u8, key: Key },
    /// `luksKillSlot` of `slot`, the recovery key authorizing it on keyslot
    /// `by`.
    KillSlot { slot: u8, by: u8 },
    /// `token remove` of token `token`.
    RemoveToken { token: u8 },
    /// `luksAddKey` of keyslot `slot` for `key`, the recovery key
    /// authorizing it on keyslot `by`.
    AddKeyslot { slot: u8, by: u8, key: Key },
    /// `token import` of `token`: as the lowest free number when `id` is
    /// `None`, as `id` otherwise, replacing the token there when `replace`.
    ImportToken {
        token: Token,
        id: Option<u8>,
        replace: bool,
    },
    /// `reencrypt` under the selector's mapping, the recovery key unlocking
    /// keyslot `slot`.
    Reencrypt { slot: u8, resilience: Resilience },
    /// `reencrypt --resume-only` with the recovery key.
    ResumeReencrypt,
    /// The lockout take, after the tpm-pin keyslot's test: the selector
    /// runs it, importing its token where `lockout_import` says.
    TakeLockout,
}

/// What a step's standard input carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input<'a> {
    Key(Key),
    Token(&'a Token),
    Nothing,
}

impl Step {
    /// The cryptsetup arguments, `name` the selector's mapping and
    /// `new_key` the added keyslot's key file; `None` for the lockout take,
    /// which is no cryptsetup command.
    pub fn args(&self, device: &Path, name: &str, new_key: &Path) -> Option<Vec<OsString>> {
        Some(match self {
            Self::Test { slot, .. } => cryptsetup::test_args(device, *slot),
            Self::KillSlot { slot, .. } => cryptsetup::kill_slot_args(device, *slot),
            Self::RemoveToken { token } => cryptsetup::token_remove_args(device, *token),
            Self::AddKeyslot { slot, by, .. } => {
                cryptsetup::add_key_args(device, *by, *slot, new_key)
            }
            Self::ImportToken {
                id: Some(id),
                replace: true,
                ..
            } => cryptsetup::token_replace_args(device, *id),
            Self::ImportToken { id, .. } => cryptsetup::token_import_args(device, *id),
            Self::Reencrypt { slot, resilience } => {
                cryptsetup::reencrypt_args(device, name, *slot, *resilience)
            }
            Self::ResumeReencrypt => cryptsetup::reencrypt_resume_args(device, name),
            Self::TakeLockout => return None,
        })
    }

    pub fn input(&self) -> Input<'_> {
        match self {
            Self::Test { key, .. } => Input::Key(*key),
            Self::KillSlot { .. }
            | Self::AddKeyslot { .. }
            | Self::Reencrypt { .. }
            | Self::ResumeReencrypt => Input::Key(Key::Recovery),
            Self::ImportToken { token, .. } => Input::Token(token),
            Self::RemoveToken { .. } | Self::TakeLockout => Input::Nothing,
        }
    }

    /// The key an added keyslot opens with, which its key file carries.
    pub fn new_key(&self) -> Option<Key> {
        match self {
            Self::AddKeyslot { key, .. } => Some(*key),
            _ => None,
        }
    }
}

/// Why no upgrade plan runs this boot: each leaves the header as it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The header carries no `td-upgrade` token (`luks2::Kind`).
    NotUpgrading,
    /// The header is not device-bound: pending tokens are removed only
    /// from one.
    NotDeviceBound,
    /// A cancellation after U0's commit.
    NotCancellable,
    /// A token of another type: the reencryption keeps the recovery
    /// keyslot alone, so its protector would lose its keyslot.
    Foreign { token: u8 },
    /// A fido2-primary token in the TPM shape, or a tpm-pin or lockout
    /// token in the FIDO2 shape.
    MixedShape,
    /// The FIDO2 shape without a fido2-primary token.
    NoPrimary,
    /// A token the state cannot hold.
    Unexpected { token: u8 },
    /// A keyslot of a type other than `luks2` outside a reencryption.
    NotLuks2 { slot: u8 },
    /// A keyslot two tokens name.
    SharedKeyslot { slot: u8 },
    /// No single keyslot the recovery key's state names: keyslot 0 in U0
    /// and U1, the one `luks2` keyslot no token names in U3, the marker's
    /// from U4.
    NoRecoveryKeyslot,
    /// No single digest names the data segment outside a reencryption.
    NoSegment,
    /// A reencryption whose one keyslot of type `reencrypt` is not in mode
    /// `reencrypt`: a resumption would decrypt or encrypt the volume.
    NotReencryption,
    /// A pending fido2 token whose passphrase was not gathered this boot.
    MissingFido2 { token: u8 },
    /// The TPM shape's tpm-pin protector was neither sealed nor found
    /// unusable before the cap.
    NoTpmPin,
    /// A lockout take outside U4 in the TPM shape with a tpm-pin keyslot and
    /// no lockout token.
    NoTake,
    /// Every keyslot from 1 to 31 is taken.
    NoFreeKeyslot,
    /// Every token number would still be taken when a token is imported.
    NoFreeToken,
    /// The plan's header could grow past the JSON area, which holds at
    /// most its length less one byte of text before a NUL.
    TooLarge { bytes: usize },
    /// A token to import is outside the token format.
    Token(String),
    /// cryptsetup's own metadata could not be read or does not agree with
    /// the header the plan was computed from.
    Metadata(String),
    /// Phase 1 over a header that is not converged: device-bound, keyslot 0
    /// and one other the one device-bound token names, every keyslot
    /// `luks2`, no reencryption, a data segment identity, and no protected
    /// token but pending fido2 ones.
    NotConverged,
    /// Phase 1 enrolls 0 to 2 new keys, at least one in the FIDO2 shape,
    /// each a pending token of the shape's role.
    NewKeys,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotUpgrading => f.write_str("the LUKS2 header carries no td-upgrade token"),
            Self::NotDeviceBound => f.write_str("the LUKS2 header is not a device-bound one"),
            Self::NotCancellable => f.write_str("the upgrade is past the point of cancelling"),
            Self::Foreign { token } => write!(f, "token {token} is of a type td does not upgrade"),
            Self::MixedShape => {
                f.write_str("the LUKS2 header holds tokens of the other shape than its upgrade")
            }
            Self::NoPrimary => {
                f.write_str("the FIDO2-primary upgrade holds no fido2-primary token")
            }
            Self::Unexpected { token } => {
                write!(f, "token {token} does not belong in this upgrade state")
            }
            Self::NotLuks2 { slot } => write!(f, "keyslot {slot} is not a luks2 keyslot"),
            Self::SharedKeyslot { slot } => {
                write!(f, "keyslot {slot} is named by more than one token")
            }
            Self::NoRecoveryKeyslot => {
                f.write_str("the LUKS2 header has no single recovery-key keyslot")
            }
            Self::NoSegment => f.write_str("no single LUKS2 digest names the data segment"),
            Self::NotReencryption => {
                f.write_str("the LUKS2 reencryption in progress is not a reencryption")
            }
            Self::MissingFido2 { token } => {
                write!(f, "pending fido2 token {token} was not presented")
            }
            Self::NoTpmPin => f.write_str("no tpm-pin protector was sealed"),
            Self::NoTake => f.write_str("no lockout take belongs in this upgrade state"),
            Self::NoFreeKeyslot => f.write_str("the LUKS2 header has no free keyslot"),
            Self::NoFreeToken => f.write_str("the LUKS2 header has no free token number"),
            Self::TooLarge { bytes } => write!(
                f,
                "the LUKS2 header could grow to {bytes} bytes of its {JSON_AREA_LEN}-byte JSON area"
            ),
            Self::Token(error) => write!(f, "the token to import: {error}"),
            Self::Metadata(error) => f.write_str(error),
            Self::NotConverged => {
                f.write_str("the LUKS2 header is not a converged device-bound one")
            }
            Self::NewKeys => f.write_str("the upgrade's new keys do not fit its shape"),
        }
    }
}

/// A plan and the header view it was computed from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    view: Metadata,
    volume: Option<Volume>,
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
    /// --dump-json-metadata` taken before the plan's first write, agrees
    /// with the view the plan was computed from: the same keyslots and
    /// tokens of every type and, for an upgrading header, the same data
    /// segment, requirement and keyslot types.
    pub fn confirm(self, metadata: &[u8]) -> Result<Vec<Step>, Refusal> {
        let agrees = match &self.volume {
            Some(volume) => {
                luks2::upgrade_metadata(metadata).map_err(Refusal::Metadata)?
                    == (self.view, volume.clone())
            }
            None => luks2::metadata(metadata).map_err(Refusal::Metadata)? == self.view,
        };
        if !agrees {
            return Err(Refusal::Metadata(
                "cryptsetup's LUKS2 metadata disagrees with the header copy td read".into(),
            ));
        }
        Ok(self.steps)
    }
}

/// What a header's state and shape need gathered before the cap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Needs {
    pub state: State,
    pub shape: Shape,
    /// The pending fido2 tokens whose keys are presented, in turn.
    pub fido2: Vec<u8>,
    /// The TPM shape without a tpm-pin protector: the new PIN twice and a
    /// sealed and verified secret, unless the TPM is unusable for td.
    pub tpm_pin: bool,
    /// The TPM shape without a lockout token: the plan ends in the take
    /// unless the TPM is unusable, which the selector therefore reads
    /// before the cap even when nothing is sealed.
    pub lockout: bool,
    /// The recovery key, which authorizes every step before U5. A U5
    /// header instead releases as a protected one.
    pub recovery_key: bool,
}

/// What the selector gathered before the cap, and the disk's resilience.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gathered<'a> {
    /// The pending fido2 tokens whose passphrases were derived.
    pub fido2: &'a [u8],
    /// The pending fido2 tokens the person dropped before the cap, which
    /// U4's plan removes (`droppable`).
    pub dropped: &'a [u8],
    pub tpm: Tpm<'a>,
    pub resilience: Resilience,
}

/// The TPM shape's new protector, as gathered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tpm<'a> {
    /// Nothing sealed: the FIDO2 shape, or a usable TPM beside a tpm-pin
    /// protector that exists.
    None,
    /// Sealed and verified before the cap; its `keyslot` is ignored.
    Sealed(&'a TpmPin),
    /// Absent, without a SHA-256 bank, holding a lockout authorization td
    /// cannot prove, or refused at the seal: the upgrade completes without
    /// adding a tpm-pin or lockout token. The selector passes it whenever
    /// it found the TPM so, whether or not anything was to be sealed.
    Unusable,
    /// The storage primary's Name differs from the one the tpm-pin token
    /// records, found at the take: U4 kills that protector's keyslot and
    /// removes its token, since no lockout token anchors it (ENCRYPTION.md
    /// "Bus"), and adds no lockout token.
    Changed,
}

/// Whether pending fido2 token `token` may join `dropped`: a pending one
/// not yet dropped, leaving the FIDO2 shape a fido2-primary token.
pub fn droppable(view: &UpgradeView, dropped: &[u8], token: u8) -> bool {
    let Ok((_, shape)) = check(view) else {
        return false;
    };
    let header = &view.header;
    if !pending(header).contains(&token) || dropped.contains(&token) {
        return false;
    }
    shape != Shape::Fido2 || primary_left(header, dropped, Some(token))
}

/// Whether a fido2-primary token remains once `dropped`, and `also`, go.
fn primary_left(header: &Header, dropped: &[u8], also: Option<u8>) -> bool {
    fido2s(header).iter().any(|(number, fido)| {
        fido.role == FidoRole::Primary && !dropped.contains(number) && Some(*number) != also
    })
}

/// The `td-upgrade` token's number, shape and `from`.
fn upgrade_token(header: &Header) -> Option<(u8, Shape, [u8; FIELD_LEN])> {
    header
        .protected
        .iter()
        .find_map(|(number, token)| match token {
            Token::Upgrade { shape, from } => Some((*number, *shape, *from)),
            _ => None,
        })
}

/// The `recovery-key` marker's keyslot, `Some(None)` for one naming none.
fn marker(header: &Header) -> Option<Option<u8>> {
    header.protected.iter().find_map(|(_, token)| match token {
        Token::RecoveryKey { keyslot } => Some(*keyslot),
        _ => None,
    })
}

/// Each fido2 token, by number.
fn fido2s(header: &Header) -> Vec<(u8, &Fido2)> {
    header
        .protected
        .iter()
        .filter_map(|(number, token)| match token {
            Token::Fido2(fido) => Some((*number, fido)),
            _ => None,
        })
        .collect()
}

/// The pending fido2 tokens: those naming no keyslot.
pub fn pending(header: &Header) -> Vec<u8> {
    fido2s(header)
        .into_iter()
        .filter(|(_, fido)| fido.keyslot.is_none())
        .map(|(number, _)| number)
        .collect()
}

/// The keyslot a tpm-pin token names, if one does.
fn tpm_pin_keyslot(header: &Header) -> Option<u8> {
    header.protected.iter().find_map(|(_, token)| match token {
        Token::TpmPin(pin) => pin.keyslot,
        _ => None,
    })
}

fn has_lockout(header: &Header) -> bool {
    header
        .protected
        .iter()
        .any(|(_, token)| matches!(token, Token::Lockout(_)))
}

/// The first-boot and device-bound td tokens, orphans included, by number.
fn device_bound(header: &Header) -> Vec<u8> {
    let mut numbers: Vec<u8> = header
        .tokens
        .iter()
        .map(|(number, _)| *number)
        .chain(header.orphans.iter().map(|(number, _)| *number))
        .collect();
    numbers.sort_unstable();
    numbers
}

/// The header's own rules and its state (ENCRYPTION.md's table), before any
/// plan. Before the marker exists only pending fido2 tokens and the
/// `td-upgrade` token are protected ones, and only U0 and U1 hold
/// device-bound tokens; from U4 the marker names the recovery keyslot,
/// beside fido2 tokens, at most one tpm-pin token and the lockout token,
/// no keyslot named twice.
pub fn check(view: &UpgradeView) -> Result<(State, Shape), Refusal> {
    let header = &view.header;
    let volume = &view.volume;
    if header.kind() != Kind::Upgrading {
        return Err(Refusal::NotUpgrading);
    }
    let Some((_, shape, from)) = upgrade_token(header) else {
        return Err(Refusal::NotUpgrading);
    };
    if let Some((token, _)) = header.foreign.first() {
        return Err(Refusal::Foreign { token: *token });
    }
    for (number, token) in &header.protected {
        match token {
            Token::Fido2(fido) if fido.role == FidoRole::Primary && shape == Shape::Tpm => {
                return Err(Refusal::MixedShape)
            }
            Token::TpmPin(_) | Token::Lockout(_) if shape == Shape::Fido2 => {
                return Err(Refusal::MixedShape)
            }
            Token::UpdateRetry(_) | Token::Request { .. } => {
                return Err(Refusal::Unexpected { token: *number })
            }
            _ => {}
        }
    }
    let primary = fido2s(header)
        .iter()
        .any(|(_, fido)| fido.role == FidoRole::Primary);
    if shape == Shape::Fido2 && !primary {
        return Err(Refusal::NoPrimary);
    }
    if !volume.reencryption {
        if let Some(slot) = header
            .keyslots
            .iter()
            .find(|slot| !volume.luks2.contains(slot))
        {
            return Err(Refusal::NotLuks2 { slot: *slot });
        }
    }
    let unchanged = !volume.reencryption && volume.segment.ok_or(Refusal::NoSegment)? == from;
    let first_device_bound = device_bound(header).first().copied();
    let marker = marker(header);
    if marker.is_none() || unchanged || volume.reencryption {
        // Before the marker: pending fido2 tokens and `td-upgrade` alone,
        // and device-bound tokens only while the data segment is unchanged.
        for (number, token) in &header.protected {
            let pending = matches!(token, Token::Fido2(fido) if fido.keyslot.is_none());
            if !pending && !matches!(token, Token::Upgrade { .. }) {
                return Err(Refusal::Unexpected { token: *number });
            }
        }
        if let Some(token) = first_device_bound.filter(|_| !unchanged) {
            return Err(Refusal::Unexpected { token });
        }
        if volume.reencryption {
            if volume.reencrypt_modes != ["reencrypt"] {
                return Err(Refusal::NotReencryption);
            }
            return Ok((State::U2, shape));
        }
        if !unchanged {
            return Ok((State::U3, shape));
        }
        if !header.keyslots.contains(&0) {
            return Err(Refusal::NoRecoveryKeyslot);
        }
        if let Some((token, _)) = header.tokens.iter().find(|(_, t)| t.keyslot() == 0) {
            return Err(Refusal::Unexpected { token: *token });
        }
        let state = if header.tokens.is_empty() {
            State::U1
        } else {
            State::U0
        };
        return Ok((state, shape));
    }
    // From U4: the marker naming the recovery keyslot.
    if let Some(token) = first_device_bound {
        return Err(Refusal::Unexpected { token });
    }
    let Some(Some(_)) = marker else {
        return Err(Refusal::NoRecoveryKeyslot);
    };
    let mut pins = header
        .protected
        .iter()
        .filter(|(_, token)| matches!(token, Token::TpmPin(_)));
    if let (Some(_), Some((token, _))) = (pins.next(), pins.next()) {
        return Err(Refusal::Unexpected { token: *token });
    }
    for slot in &header.keyslots {
        let named = header
            .protected
            .iter()
            .filter(|(_, token)| token.keyslot() == Some(*slot))
            .count();
        if named > 1 {
            return Err(Refusal::SharedKeyslot { slot: *slot });
        }
    }
    let complete = pending(header).is_empty()
        && (shape == Shape::Fido2 || (tpm_pin_keyslot(header).is_some() && has_lockout(header)));
    Ok((if complete { State::U5 } else { State::U4 }, shape))
}

/// What the selector gathers before the cap in this state.
pub fn needs(view: &UpgradeView) -> Result<Needs, Refusal> {
    let (state, shape) = check(view)?;
    let before = state != State::U5;
    Ok(Needs {
        state,
        shape,
        fido2: if before {
            pending(&view.header)
        } else {
            Vec::new()
        },
        tpm_pin: before && shape == Shape::Tpm && tpm_pin_keyslot(&view.header).is_none(),
        lockout: before && shape == Shape::Tpm && !has_lockout(&view.header),
        recovery_key: before,
    })
}

/// The lowest keyslot from 1 not in `taken`.
fn free_keyslot(taken: &[u8]) -> Result<u8, Refusal> {
    (1..=MAX_SLOT)
        .find(|slot| !taken.contains(slot))
        .ok_or(Refusal::NoFreeKeyslot)
}

/// One state's plan after the cap (ENCRYPTION.md "Re-encrypting upgrade",
/// step 3), every step authorized by the recovery key, tested first on the
/// recovery keyslot:
///
/// - U0, and U1's leftovers: kill every keyslot but 0, then remove the
///   first-boot and device-bound tokens, orphans included. U1 then
///   re-encrypts with the recovery key on keyslot 0.
/// - U2: resume the reencryption, which tests the key itself.
/// - U3: import the marker naming the one `luks2` keyslot no token names.
/// - U4: kill any `luks2` keyslot no token names but the marker's and
///   remove an orphan tpm-pin token and each dropped pending token, then
///   for each other pending fido2 token add its keyslot, test it and only
///   then replace the token with one naming it, so a keyslot no test
///   proved stays an orphan the next boot kills; in the TPM shape add the
///   tpm-pin protector the same way and take the lockout, or after a
///   changed primary kill and remove that protector. When no take
///   follows, the `td-upgrade` token is removed last.
/// - U5: remove the `td-upgrade` token; the release already opened the
///   volume, so nothing is tested again.
///
/// Every plan's header, grown by what it adds and never shrunk by what it
/// removes, must fit the JSON area with its terminating NUL.
pub fn plan(view: &UpgradeView, gathered: &Gathered) -> Result<Plan, Refusal> {
    let (state, shape) = check(view)?;
    let header = &view.header;
    let (upgrade, _, _) = upgrade_token(header).ok_or(Refusal::NotUpgrading)?;
    let mut steps = Vec::new();
    let mut growth = 0usize;
    match state {
        State::U0 | State::U1 => {
            steps.push(Step::Test {
                slot: 0,
                key: Key::Recovery,
            });
            steps.extend(
                header
                    .keyslots
                    .iter()
                    .filter(|slot| **slot != 0)
                    .map(|slot| Step::KillSlot { slot: *slot, by: 0 }),
            );
            steps.extend(
                device_bound(header)
                    .into_iter()
                    .map(|token| Step::RemoveToken { token }),
            );
            if state == State::U1 {
                steps.push(Step::Reencrypt {
                    slot: 0,
                    resilience: gathered.resilience,
                });
                growth += REENCRYPT_JSON;
            }
        }
        State::U2 => steps.push(Step::ResumeReencrypt),
        State::U3 => {
            let named = header.foreign_keyslots();
            let mut unnamed = view.volume.luks2.iter().filter(|s| !named.contains(s));
            let (Some(slot), None) = (unnamed.next(), unnamed.next()) else {
                return Err(Refusal::NoRecoveryKeyslot);
            };
            let token = Token::RecoveryKey {
                keyslot: Some(*slot),
            };
            growth += imported(&token)?;
            if header.token_numbers().len() >= TOKEN_NUMBERS {
                return Err(Refusal::NoFreeToken);
            }
            steps.push(Step::Test {
                slot: *slot,
                key: Key::Recovery,
            });
            steps.push(Step::ImportToken {
                token,
                id: None,
                replace: false,
            });
        }
        State::U4 => {
            let Some(Some(by)) = marker(header) else {
                return Err(Refusal::NoRecoveryKeyslot);
            };
            steps.push(Step::Test {
                slot: by,
                key: Key::Recovery,
            });
            let named = header.foreign_keyslots();
            let orphans: Vec<u8> = header
                .keyslots
                .iter()
                .copied()
                .filter(|slot| *slot != by && !named.contains(slot))
                .collect();
            steps.extend(
                orphans
                    .iter()
                    .map(|slot| Step::KillSlot { slot: *slot, by }),
            );
            let orphan_pins: Vec<u8> = header
                .protected
                .iter()
                .filter(|(_, token)| matches!(token, Token::TpmPin(pin) if pin.keyslot.is_none()))
                .map(|(number, _)| *number)
                .collect();
            steps.extend(
                orphan_pins
                    .iter()
                    .map(|token| Step::RemoveToken { token: *token }),
            );
            for token in gathered.dropped {
                if !pending(header).contains(token) {
                    return Err(Refusal::Unexpected { token: *token });
                }
            }
            if shape == Shape::Fido2 && !primary_left(header, gathered.dropped, None) {
                return Err(Refusal::NoPrimary);
            }
            steps.extend(
                gathered
                    .dropped
                    .iter()
                    .map(|token| Step::RemoveToken { token: *token }),
            );
            let mut taken: Vec<u8> = header
                .keyslots
                .iter()
                .copied()
                .filter(|slot| !orphans.contains(slot))
                .collect();
            for (number, fido) in fido2s(header) {
                if fido.keyslot.is_some() || gathered.dropped.contains(&number) {
                    continue;
                }
                if !gathered.fido2.contains(&number) {
                    return Err(Refusal::MissingFido2 { token: number });
                }
                let slot = free_keyslot(&taken)?;
                taken.push(slot);
                let key = Key::Fido2 { token: number };
                let old = Token::Fido2(fido.clone());
                let token = Token::Fido2(Fido2 {
                    keyslot: Some(slot),
                    ..fido.clone()
                });
                let lengths = (
                    token.encode().map_err(Refusal::Token)?.len(),
                    old.encode().map_err(Refusal::Token)?.len(),
                );
                growth += KEYSLOT_JSON + lengths.0.saturating_sub(lengths.1);
                steps.push(Step::AddKeyslot { slot, by, key });
                steps.push(Step::Test { slot, key });
                steps.push(Step::ImportToken {
                    token,
                    id: Some(number),
                    replace: true,
                });
            }
            let mut take = false;
            if shape == Shape::Tpm && gathered.tpm == Tpm::Changed {
                let pins: Vec<(u8, u8)> = header
                    .protected
                    .iter()
                    .filter_map(|(number, token)| match token {
                        Token::TpmPin(pin) => pin.keyslot.map(|slot| (*number, slot)),
                        _ => None,
                    })
                    .collect();
                for (token, slot) in pins {
                    steps.push(Step::KillSlot { slot, by });
                    steps.push(Step::RemoveToken { token });
                }
            } else if shape == Shape::Tpm && gathered.tpm != Tpm::Unusable {
                if tpm_pin_keyslot(header).is_none() {
                    let Tpm::Sealed(sealed) = gathered.tpm else {
                        return Err(Refusal::NoTpmPin);
                    };
                    let slot = free_keyslot(&taken)?;
                    let token = Token::TpmPin(TpmPin {
                        keyslot: Some(slot),
                        ..sealed.clone()
                    });
                    growth += KEYSLOT_JSON + imported(&token)?;
                    if header.token_numbers().len() - orphan_pins.len() >= TOKEN_NUMBERS {
                        return Err(Refusal::NoFreeToken);
                    }
                    steps.push(Step::AddKeyslot {
                        slot,
                        by,
                        key: Key::TpmPin,
                    });
                    steps.push(Step::Test {
                        slot,
                        key: Key::TpmPin,
                    });
                    steps.push(Step::ImportToken {
                        token,
                        id: None,
                        replace: false,
                    });
                }
                take = !has_lockout(header);
            }
            if take {
                growth += LOCKOUT_JSON + TOKEN_ENTRY;
                steps.push(Step::TakeLockout);
            } else {
                steps.push(Step::RemoveToken { token: upgrade });
            }
        }
        State::U5 => steps.push(Step::RemoveToken { token: upgrade }),
    }
    let bytes = view.json_len.saturating_add(growth);
    if bytes >= JSON_AREA_LEN {
        return Err(Refusal::TooLarge { bytes });
    }
    Ok(Plan {
        view: header.metadata(),
        volume: Some(view.volume.clone()),
        steps,
    })
}

/// What importing `token` as a new number adds to the JSON area's text.
fn imported(token: &Token) -> Result<usize, Refusal> {
    Ok(token.encode().map_err(Refusal::Token)?.len() + TOKEN_ENTRY)
}

/// The lockout token's import in a take the plan's `TakeLockout` asks
/// for: as the lowest number no token holds, which the take's later
/// writes name (`protected_plan::lockout_proven` and `lockout_remove`).
/// Only a TPM-shape header in U4 whose tpm-pin protector names a keyslot
/// and which holds no lockout token takes one.
pub fn lockout_import(view: &UpgradeView, token: &Lockout) -> Result<(u8, Step), Refusal> {
    let (state, shape) = check(view)?;
    if state != State::U4
        || shape != Shape::Tpm
        || tpm_pin_keyslot(&view.header).is_none()
        || has_lockout(&view.header)
    {
        return Err(Refusal::NoTake);
    }
    let used = view.header.token_numbers();
    let id = (0..=MAX_SLOT)
        .find(|number| !used.contains(number))
        .ok_or(Refusal::NoFreeToken)?;
    let token = Token::Lockout(token.clone());
    token.encode().map_err(Refusal::Token)?;
    Ok((
        id,
        Step::ImportToken {
            token,
            id: Some(id),
            replace: false,
        },
    ))
}

/// A cancellation in U0 (ENCRYPTION.md "The upgrade boot", step 1): remove
/// the `td-upgrade` token, then the pending fido2 tokens it leaves as
/// orphans. The boot then runs the device-bound recovery flow.
///
/// It asks only what makes a header U0 (no reencryption, the data segment
/// as phase 1 found it, a device-bound token naming a keyslot) and that
/// the header it leaves holds no protected token beside the device-bound
/// ones, which the reader would refuse; not `check`'s other rules: its
/// removals lose nothing, so an upgrading header `check` refuses, such as
/// one a foreign token joined after phase 1, can still leave the upgrade
/// before its first commit.
pub fn cancel(view: &UpgradeView) -> Result<Plan, Refusal> {
    let header = &view.header;
    if header.kind() != Kind::Upgrading {
        return Err(Refusal::NotUpgrading);
    }
    let (upgrade, _, from) = upgrade_token(header).ok_or(Refusal::NotUpgrading)?;
    let pending = pending(header);
    let leaves_device_bound = header
        .protected
        .iter()
        .all(|(number, _)| *number == upgrade || pending.contains(number));
    let u0 = !view.volume.reencryption
        && view.volume.segment == Some(from)
        && !header.tokens.is_empty()
        && leaves_device_bound;
    if !u0 {
        return Err(Refusal::NotCancellable);
    }
    let mut steps = vec![Step::RemoveToken { token: upgrade }];
    steps.extend(pending.into_iter().map(|token| Step::RemoveToken { token }));
    Ok(Plan {
        view: header.metadata(),
        volume: Some(view.volume.clone()),
        steps,
    })
}

/// The most new keys phase 1 enrolls.
const NEW_KEYS: usize = 2;

/// Phase 1's checks (ENCRYPTION.md "Re-encrypting upgrade", phase 1;
/// DESIGN.md "Upgrade plans"), before the storage worker presents its
/// review: the header is converged, `count` new keys fit `shape`, and
/// every plan the upgrade will make fits the JSON area. The bound is every
/// later plan's growth taken at once, each token at its largest and none
/// shrunk by a removal: phase 1's imports, then the boot's reencryption, a
/// keyslot per new key, the marker and, in the TPM shape, the tpm-pin
/// protector and the lockout token. It returns that bound, which must
/// leave the area's last byte.
pub fn phase_one_fits(view: &UpgradeView, shape: Shape, count: usize) -> Result<usize, Refusal> {
    let header = &view.header;
    let volume = &view.volume;
    let ([(_, bound)], [0, slot]) = (header.tokens.as_slice(), header.keyslots.as_slice()) else {
        return Err(Refusal::NotConverged);
    };
    let converged = header.kind() == Kind::DeviceBound
        && bound.role() == Role::DeviceBound
        && bound.keyslot() == *slot
        && header.orphans.is_empty()
        && header.foreign.is_empty()
        && !volume.reencryption
        && volume.segment.is_some()
        && volume.luks2 == header.keyslots;
    if !converged {
        return Err(Refusal::NotConverged);
    }
    if count > NEW_KEYS || (shape == Shape::Fido2 && count == 0) {
        return Err(Refusal::NewKeys);
    }
    let keyed = Some(MAX_SLOT);
    let key = imported(&Token::Fido2(Fido2 {
        keyslot: keyed,
        role: FidoRole::Recovery,
        credential: vec![0; MAX_CREDENTIAL_ID],
        salt: [0; FIELD_LEN],
        x: [0; FIELD_LEN],
        y: [0; FIELD_LEN],
    }))?;
    let mut growth = count * (key + KEYSLOT_JSON)
        + imported(&Token::Upgrade {
            shape,
            from: [0; FIELD_LEN],
        })?
        + REENCRYPT_JSON
        + imported(&Token::RecoveryKey { keyslot: keyed })?;
    if shape == Shape::Tpm {
        let mut pin = 0;
        for pcrs in [PinPcrs::Selector, PinPcrs::SecureBoot] {
            pin = pin.max(imported(&Token::TpmPin(TpmPin {
                keyslot: keyed,
                pcrs,
                primary: [0; crate::pin::PRIMARY_NAME_LEN],
                salt: [0; crate::pin::SALT_LEN],
                sealed: SealedObject {
                    public: vec![0; MAX_PIN_PUBLIC_BYTES],
                    private: vec![0; MAX_PIN_PRIVATE_BYTES],
                },
            }))?);
        }
        growth += KEYSLOT_JSON + pin + LOCKOUT_JSON + TOKEN_ENTRY;
    }
    let bytes = view.json_len.saturating_add(growth);
    if bytes >= JSON_AREA_LEN {
        return Err(Refusal::TooLarge { bytes });
    }
    Ok(bytes)
}

/// Phase 1's header commits over the re-read view (DESIGN.md "Upgrade
/// plans"): `phase_one_fits` again, then the removal of each pending token
/// a crashed phase 1 left, each new key's pending token as the lowest
/// free number, and last the `td-upgrade` token naming `shape` and the
/// view's data segment identity. The storage worker writes the selector
/// between the removals and the first import.
pub fn phase_one(view: &UpgradeView, shape: Shape, keys: &[Fido2]) -> Result<Plan, Refusal> {
    phase_one_fits(view, shape, keys.len())?;
    let role = match shape {
        Shape::Tpm => FidoRole::Recovery,
        Shape::Fido2 => FidoRole::Primary,
    };
    if keys
        .iter()
        .any(|key| key.keyslot.is_some() || key.role != role)
    {
        return Err(Refusal::NewKeys);
    }
    let from = view.volume.segment.ok_or(Refusal::NoSegment)?;
    let mut steps: Vec<Step> = pending(&view.header)
        .into_iter()
        .map(|token| Step::RemoveToken { token })
        .collect();
    let tokens = keys
        .iter()
        .map(|key| Token::Fido2(key.clone()))
        .chain([Token::Upgrade { shape, from }]);
    for token in tokens {
        // Within the bound `phase_one_fits` took, or refused here.
        token.encode().map_err(Refusal::Token)?;
        steps.push(Step::ImportToken {
            token,
            id: None,
            replace: false,
        });
    }
    Ok(Plan {
        view: view.header.metadata(),
        volume: Some(view.volume.clone()),
        steps,
    })
}

/// The pending fido2 tokens a device-bound header holds without a
/// `td-upgrade` token, a phase 1 or cancellation interrupted, removed:
/// they name no keyslot, so their removal loses nothing.
pub fn pending_orphans(header: &Header) -> Result<Plan, Refusal> {
    if header.kind() != Kind::DeviceBound {
        return Err(Refusal::NotDeviceBound);
    }
    Ok(Plan {
        view: header.metadata(),
        volume: None,
        steps: pending(header)
            .into_iter()
            .map(|token| Step::RemoveToken { token })
            .collect(),
    })
}

#[cfg(test)]
#[path = "upgrade_tests.rs"]
pub(crate) mod tests;

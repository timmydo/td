//! Private session protocol and a physical attention lifetime.

use crate::attention::{Field, Notice};
use crate::authority::consent::{
    upgrade_ceiling, Admitted, DiskRequest, Enrollment, Fingerprint, LoginStep, Managed, Operation,
    Platform, Proof, Recovery, Request, Role, SelectorForm, Slot, UpgradeShape, UpgradeStep,
    LOGIN_CEREMONY, LOGIN_KEYS, LOGIN_READING, LOGIN_TWO_CEREMONIES, LOGIN_TWO_KEY_ENROLLMENT,
    PROTECTOR_CEILING, REQUEST_CEILING, SELECTOR_UPDATE_CEILING, UPGRADE_CANCEL_CEILING,
};
use crate::authority::Exchange;
use crate::input::EvdevOrigin;
use crate::runtime::Runtime;
use std::collections::BTreeSet;
use std::fs::File;
use std::io::{self, Read};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

const ACTIVE: u8 = 0;
const CANCELLED: u8 = 1;
const COMMITTED: u8 = 2;
/// A submitted PIN taken for its `1c`: cancellation marked after this is
/// honoured up to the write, and after it is a `15` like Escape's.
const SENDING: u8 = 4;

/// A store operation's or an installation review's attention lifetime.
const SECRET_LIFETIME: Duration = Duration::from_secs(120);
/// How long a committed device-bound disk installation's notice stays
/// before attention closes by itself.
const RETURN_NOTICE: Duration = Duration::from_secs(4);

/// FIDO's PIN profile (td-login/TOKEN-LOGIN.md, "Token profile"): 4 to 63
/// printable ASCII bytes, so each byte is one code point.
const PIN_SHORTEST: usize = 4;
const PIN_LONGEST: usize = 63;
/// The PIN field's fixed buffer.
const PIN_CAPACITY: usize = 64;
/// The recovery-key profile: the 48 passphrase digits td-protector's
/// codec admits, without separators.
const RECOVERY_DIGITS: usize = 48;

/// Whether this build offers the disk proofs, `R` and `S` on the
/// authorization screen: td-authd's `storage::OPERATIONS`, which decodes
/// `1b 0b`. Until td-install/ENCRYPTION.md's item 9 a production build
/// shows `NOT AVAILABLE IN THIS BUILD` for each and sends nothing; a
/// source test pins the two to one value.
pub(crate) const DISK_PROOFS: bool = cfg!(test);
/// `1c` as written: its tag, the length byte, the longest description that
/// byte counts, and the longest PIN.
const PIN_REQUEST: usize = 2 + 255 + PIN_LONGEST;
/// `/proc/swaps` and `/proc/self/limits` are read up to this many bytes.
const PROC_TEXT: u64 = 64 * 1024;

/// Why a PIN field did not open: this process's memory could reach swap or
/// a core dump.
const UNPROTECTED: &[&str] = &["PIN ENTRY NEEDS NO SWAP AND NO CORE DUMPS"];

/// One fresh physical key press for the PIN field, already mapped through
/// its keymap. Its `Debug` never shows a byte.
#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum FieldKey {
    Byte(u8),
    /// A key that names a digit for the recovery-key profile: a number-row
    /// digit at either Shift level, or a keypad digit while NumLock types
    /// one; `byte` is what it types for a PIN.
    Digit {
        byte: u8,
        digit: u8,
    },
    Erase,
    Submit,
}

impl std::fmt::Debug for FieldKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Byte(_) => "Byte(..)",
            Self::Digit { .. } => "Digit(..)",
            Self::Erase => "Erase",
            Self::Submit => "Submit",
        })
    }
}

/// A PIN as it is typed: a fixed buffer, zeroed on every exit and when
/// dropped. Never cloned, formatted or moved into a `String` or `Vec`.
struct PinBuffer {
    bytes: [u8; PIN_CAPACITY],
    length: usize,
}

impl PinBuffer {
    /// A 64th byte refuses.
    fn push(&mut self, byte: u8) -> bool {
        if self.length >= PIN_LONGEST {
            return false;
        }
        let Some(slot) = self.bytes.get_mut(self.length) else {
            return false;
        };
        *slot = byte;
        self.length += 1;
        true
    }

    fn pop(&mut self) -> bool {
        let Some(last) = self.length.checked_sub(1) else {
            return false;
        };
        if let Some(slot) = self.bytes.get_mut(last) {
            *slot = 0;
        }
        self.length = last;
        true
    }

    fn clear(&mut self) {
        self.bytes.fill(0);
        std::hint::black_box(&mut self.bytes);
        self.length = 0;
    }
}

impl Default for PinBuffer {
    fn default() -> Self {
        Self {
            bytes: [0; PIN_CAPACITY],
            length: 0,
        }
    }
}

impl Drop for PinBuffer {
    fn drop(&mut self) {
        self.clear();
        #[cfg(test)]
        tests::dropped(&self.bytes);
    }
}

/// The field's profile: a PIN's, the recovery key's 48 digits, or a
/// choose step's one digit naming a removable token, the removable
/// positions as bits from the lowest for position 1.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum Profile {
    #[default]
    Pin,
    Recovery,
    Choose(u8),
}

impl Profile {
    /// The profile of `step`, a presented secret step.
    pub(crate) fn of(step: &Request) -> Self {
        if step.asks_recovery_key() {
            return Self::Recovery;
        }
        match (step.protector_step(), step.removal()) {
            (Some(LoginStep::Choose), Some(removal)) => Self::Choose(
                (1..=4u8)
                    .filter(|position| removal.removable(*position))
                    .fold(0, |bits, position| {
                        bits | 1u8
                            .checked_shl(u32::from(position.saturating_sub(1)))
                            .unwrap_or(0)
                    }),
            ),
            _ => Self::Pin,
        }
    }
}

/// The PIN field's state: open for a presented PIN step root asked a PIN
/// for, until its PIN is submitted and taken, or it closes.
#[derive(Default)]
pub(crate) struct PinField {
    open: bool,
    submitted: bool,
    profile: Profile,
    /// When the field's own paint was seen on glass: only a press made
    /// after it may type.
    shown: Option<u128>,
    pin: PinBuffer,
}

impl PinField {
    #[cfg(test)]
    pub fn open(&mut self) {
        self.open_for(Profile::Pin);
    }

    /// Opens the field in the step's profile.
    pub fn open_for(&mut self, profile: Profile) {
        self.pin.clear();
        self.open = true;
        self.submitted = false;
        self.profile = profile;
        self.shown = None;
    }

    pub fn close(&mut self) {
        self.pin.clear();
        self.open = false;
        self.submitted = false;
        self.profile = Profile::Pin;
        self.shown = None;
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// One key while the field is open and not yet submitted: the masked
    /// length to show when the bytes changed. A byte outside printable
    /// ASCII, a 64th byte, Backspace on nothing and an Enter before four
    /// bytes change nothing. In the recovery-key profile only a digit
    /// types, a 49th refused, and Enter submits only at exactly 48. In the
    /// choose profile a digit naming a removable position types and
    /// submits at once, and every other key is ignored.
    pub fn key(&mut self, key: FieldKey) -> Option<usize> {
        if !self.open || self.submitted {
            return None;
        }
        let changed = match (self.profile, key) {
            (Profile::Pin, FieldKey::Byte(byte) | FieldKey::Digit { byte, .. }) => {
                (0x20..=0x7e).contains(&byte) && self.pin.push(byte)
            }
            (Profile::Recovery, FieldKey::Digit { digit, .. }) => {
                digit.is_ascii_digit() && self.pin.length < RECOVERY_DIGITS && self.pin.push(digit)
            }
            (Profile::Choose(removable), FieldKey::Digit { digit, .. }) => {
                let named = digit
                    .checked_sub(b'1')
                    .filter(|bit| *bit < 4)
                    .and_then(|bit| 1u8.checked_shl(u32::from(bit)))
                    .is_some_and(|bit| removable & bit != 0);
                self.submitted = named && self.pin.push(digit);
                self.submitted
            }
            (Profile::Recovery | Profile::Choose(_), FieldKey::Byte(_))
            | (Profile::Choose(_), FieldKey::Erase | FieldKey::Submit) => false,
            (_, FieldKey::Erase) => self.pin.pop(),
            (profile, FieldKey::Submit) => {
                self.submitted = if profile == Profile::Recovery {
                    self.pin.length == RECOVERY_DIGITS
                } else {
                    self.pin.length >= PIN_SHORTEST
                };
                false
            }
        };
        changed.then_some(self.pin.length)
    }

    /// A submitted PIN copied into `into` and zeroed here, closing the
    /// field: its length.
    fn take(&mut self, into: &mut [u8]) -> Option<usize> {
        if !self.open || !self.submitted {
            return None;
        }
        let length = self.pin.length;
        let taken = match (into.get_mut(..length), self.pin.bytes.get(..length)) {
            (Some(into), Some(pin)) => {
                into.copy_from_slice(pin);
                Some(length)
            }
            _ => None,
        };
        self.close();
        taken
    }

    #[cfg(test)]
    pub fn typed(&self) -> &[u8] {
        self.pin.bytes.get(..self.pin.length).unwrap_or(&[])
    }

    #[cfg(test)]
    pub fn submitted(&self) -> bool {
        self.submitted
    }

    #[cfg(test)]
    fn raw(&self) -> [u8; PIN_CAPACITY] {
        self.pin.bytes
    }
}

/// `1c` as it is written, PIN included: a fixed buffer zeroed when dropped.
struct PinRequest {
    bytes: [u8; PIN_REQUEST],
}

impl PinRequest {
    fn clear(&mut self) {
        self.bytes.fill(0);
        std::hint::black_box(&mut self.bytes);
        #[cfg(test)]
        tests::request_cleared(&self.bytes);
    }
}

impl Drop for PinRequest {
    fn drop(&mut self) {
        self.clear();
        #[cfg(test)]
        tests::dropped(&self.bytes);
    }
}

/// td-secret's PIN-holder check (`store::require_protected_memory`), for
/// this process: no active swap and a zero core-dump soft limit.
fn protected_memory() -> Result<(), String> {
    memory_state(proc_text("/proc/swaps"), proc_text("/proc/self/limits"))
}

fn proc_text(path: &str) -> io::Result<String> {
    let mut text = String::new();
    File::open(path)?
        .take(PROC_TEXT + 1)
        .read_to_string(&mut text)?;
    if u64::try_from(text.len()).map_or(true, |length| length > PROC_TEXT) {
        return Err(io::Error::other("oversized process table"));
    }
    Ok(text)
}

fn memory_state(swaps: io::Result<String>, limits: io::Result<String>) -> Result<(), String> {
    match swaps {
        // Linux registers /proc/swaps only when CONFIG_SWAP is enabled.
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
        Ok(swaps) => {
            if swaps.lines().next().is_none()
                || swaps.lines().skip(1).any(|line| !line.trim().is_empty())
            {
                return Err("a PIN needs swap to be disabled".into());
            }
        }
    }
    let limits = limits.map_err(|error| error.to_string())?;
    let core = limits
        .lines()
        .find_map(|line| line.strip_prefix("Max core file size"));
    if core.and_then(|line| line.split_whitespace().next()) != Some("0") {
        return Err("a PIN needs a zero core-dump soft limit".into());
    }
    Ok(())
}

/// A login step's or an upgrade's new key's PIN step.
fn asks_pin(request: &Request) -> bool {
    request.asks_pin()
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Selection {
    Unlock(Role),
    Enroll(Recovery),
    Write,
    Install,
    Login(LoginSelection),
    Elevation(Elevation),
    Power(crate::authority::consent::Power),
    Suspend,
}
impl From<Role> for Selection {
    fn from(role: Role) -> Self {
        Self::Unlock(role)
    }
}
impl Selection {
    fn operation(&self) -> Option<Operation> {
        Some(match self {
            Self::Write
            | Self::Install
            | Self::Login(_)
            | Self::Elevation(_)
            | Self::Power(_)
            | Self::Suspend => return None,
            Self::Unlock(role) => Operation::Unlock { role: *role },
            Self::Enroll(recovery) => Operation::Enroll {
                platform: Platform::TpmPcr7,
                recovery: *recovery,
                step: Enrollment::CreatePrimary,
            },
        })
    }
    fn request(&self) -> Result<Vec<u8>, String> {
        Ok(match self {
            Self::Suspend => return Err("suspend requires a lock presentation receipt".into()),
            Self::Power(choice) => vec![0x21, choice.byte()],
            Self::Write => vec![0x18],
            Self::Install => vec![0x19],
            Self::Unlock(Role::Primary) => vec![0x12, 1],
            Self::Unlock(Role::Recovery) => vec![0x12, 2],
            Self::Enroll(Recovery::Unrecoverable) => vec![0x16, 0],
            Self::Enroll(Recovery::SecondToken) => vec![0x16, 1],
            Self::Login(login) => return login.request(),
            Self::Elevation(elevation) => elevation.request(),
        })
    }
    /// The attention lifetime, fixed at the selection and never renewed.
    fn lifetime(&self) -> Duration {
        match self {
            Self::Login(login) => login.ceiling(),
            _ => SECRET_LIFETIME,
        }
    }
    /// Whether a physical confirmation on the presented prompt must come
    /// before commit: an approving Enter for an update or an elevation, a
    /// fresh Enter for a live boot's whole-disk installation. A login
    /// disclosure step's approval comes before its receipt instead.
    fn confirms(&self) -> bool {
        matches!(self, Self::Install | Self::Elevation(_))
    }
}

/// An elevation (td-compositor/DESIGN.md, "Elevation consent"): what the
/// attention menu's `B` asks root for, and the queued request its `H`
/// selects. Only an approving Enter on its presented prompt confirms it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Elevation {
    Rollback,
    Hostname,
}

impl Elevation {
    /// The elevation `request` describes.
    fn of(request: &Request) -> Option<Self> {
        match request.operation() {
            Operation::DeployRollback { .. } => Some(Self::Rollback),
            Operation::SetHostname { .. }
            | Operation::ManageTime { .. }
            | Operation::EnrollAdminKey { .. } => Some(Self::Hostname),
            _ => None,
        }
    }

    /// Root's request (td-authd/DESIGN.md, "Elevation operations"), which
    /// takes no operand.
    fn request(self) -> Vec<u8> {
        match self {
            Self::Rollback => vec![0x1d],
            Self::Hostname => vec![0x1e],
        }
    }
}

/// The press a prompt awaiting approval counts first (DESIGN.md,
/// "Elevation consent"): Enter with no Control, Alt or Super held, or any
/// other key, which ends the request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ApprovalPress {
    Enter,
    Other,
}

/// How long, in CLOCK_MONOTONIC nanoseconds, a prompt awaiting approval is
/// on glass before Enter approves it. An earlier Enter, one already on its
/// way when the prompt appeared, ends the request instead.
pub(crate) const APPROVAL_SETTLE: u128 = 500_000_000;

/// A presented prompt that waits for its physical confirmation.
struct Presented {
    request: Request,
    /// A press must be stamped strictly later than this: the
    /// CLOCK_MONOTONIC sample taken after the prompt's full-frame
    /// submission.
    after: u128,
    /// Whether its first counted press came: that press alone approved or
    /// ended it.
    decided: bool,
    /// The approving Enter's stamp, while a press stamped before it, read
    /// later from another keyboard, may still withdraw it.
    approved_at: Option<u128>,
}

/// A login-key operation (td-login/TOKEN-LOGIN.md): root's request `1b`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum LoginSelection {
    Notebook,
    Unlock,
    /// First enrollment of one or two keys.
    Enroll(u8),
    Add,
    /// The slots to remove, positions strictly increasing, and the keys
    /// the removal leaves of the list it was chosen from, which fix its
    /// ceiling: root's description must agree.
    Remove(Vec<Slot>, u8),
    /// Storage operation `01` (td-authd/DESIGN.md amendment 10), root's
    /// `20 01`, polled as a login operation is: one step, approved by Enter,
    /// then its commit.
    SelectorUpdate,
    /// Storage operation `02`, root's `20 02 S N`: phase 1 of the
    /// re-encrypting upgrade to `shape` with `count` new keys, its review
    /// then each new key's ceremony, and its commit.
    Upgrade {
        shape: UpgradeShape,
        count: u8,
    },
    /// Storage operation `03`, root's `20 03`: the upgrade's cancellation,
    /// one step as the selector update's.
    UpgradeCancel,
    /// Storage operations `04` to `09`, root's `20 04` to `20 09`:
    /// protector management with its proof and an addition's role.
    Protector(Managed),
    /// A first enrollment, addition or removal authorized by a disk proof,
    /// root's `1b 0b P` (amendment 10); offered only where `DISK_PROOFS`.
    Proven(Proof, Box<LoginSelection>),
}

impl LoginSelection {
    /// The bytes td-authd's `login::Selection::decode` reads, after `1b`.
    fn request(&self) -> Result<Vec<u8>, String> {
        Ok(match self {
            Self::Notebook => vec![0x29],
            Self::Proven(proof, inner) => match (inner.as_ref(), inner.request()?.as_slice()) {
                (Self::Enroll(_) | Self::Add | Self::Remove(..), [0x1b, rest @ ..]) => {
                    [&[0x1b, 0x0b, proof.byte()][..], rest].concat()
                }
                _ => return Err("a disk proof authorizes no such operation".into()),
            },
            Self::SelectorUpdate => vec![0x20, 1],
            Self::Upgrade { shape, count } if shape.takes(*count) => {
                vec![0x20, 2, shape.byte(), *count]
            }
            Self::Upgrade { .. } => return Err("an upgrade's shape does not take its count".into()),
            Self::UpgradeCancel => vec![0x20, 3],
            Self::Protector(managed) => match managed {
                Managed::Add { proof, role } => vec![0x20, 4, proof.byte(), role.byte()],
                Managed::Remove { proof } => vec![0x20, 5, proof.byte()],
                Managed::RecoveryRemove => vec![0x20, 6],
                Managed::Request { proof, request } => {
                    vec![0x20, 6u8.saturating_add(request.byte()), proof.byte()]
                }
            },
            Self::Unlock => vec![0x1b, 7],
            Self::Enroll(keys @ (1 | 2)) => vec![0x1b, 8, *keys],
            Self::Enroll(_) => return Err("a first enrollment has one or two keys".into()),
            Self::Add => vec![0x1b, 9],
            Self::Remove(slots, _) => {
                let count = u8::try_from(slots.len())
                    .ok()
                    .filter(|count| (1..=LOGIN_KEYS).contains(count))
                    .ok_or("invalid login removal set")?;
                let ordered = slots
                    .windows(2)
                    .all(|pair| matches!(pair, [low, high] if low.position < high.position));
                if !ordered
                    || !slots
                        .iter()
                        .all(|slot| (1..=LOGIN_KEYS).contains(&slot.position))
                {
                    return Err("invalid login removal set".into());
                }
                let mut bytes = vec![0x1b, 10, count];
                for slot in slots {
                    bytes.push(slot.position);
                    bytes.extend_from_slice(&slot.key);
                }
                bytes
            }
        })
    }

    /// td-authd's ceiling: one ceremony per key the person handles in turn,
    /// and the reading allowance when the operation discloses.
    fn ceiling(&self) -> Duration {
        match self {
            Self::Notebook => crate::authority::consent::NOTEBOOK_CEILING,
            Self::Unlock => LOGIN_CEREMONY,
            Self::Remove(_, after) if *after > 1 => LOGIN_CEREMONY,
            Self::Remove(..) | Self::Enroll(1) => LOGIN_CEREMONY.saturating_add(LOGIN_READING),
            Self::Add => LOGIN_TWO_CEREMONIES,
            Self::Enroll(_) => LOGIN_TWO_KEY_ENROLLMENT,
            Self::SelectorUpdate => SELECTOR_UPDATE_CEILING,
            Self::Upgrade { count, .. } => upgrade_ceiling(*count),
            Self::UpgradeCancel => UPGRADE_CANCEL_CEILING,
            Self::Protector(Managed::Request { .. }) => REQUEST_CEILING,
            Self::Protector(_) => PROTECTOR_CEILING,
            // The proof replaces an addition's or removal's authorizing
            // key, and adds a ceremony to an enrollment.
            Self::Proven(_, inner) => match inner.as_ref() {
                Self::Enroll(_) => inner.ceiling().saturating_add(LOGIN_CEREMONY),
                other => other.ceiling(),
            },
        }
    }

    /// The operation a disk proof authorizes, and the proof; any other
    /// selection is its own.
    fn authorized(&self) -> (&Self, Option<Proof>) {
        match self {
            Self::Proven(proof, inner) => (inner.as_ref(), Some(*proof)),
            other => (other, None),
        }
    }

    /// A storage operation (amendment 10): root's `20`, whose end changes
    /// no login key.
    fn storage(&self) -> bool {
        matches!(
            self,
            Self::SelectorUpdate | Self::Upgrade { .. } | Self::UpgradeCancel | Self::Protector(_)
        )
    }

    /// Root's answer byte to the selection: `9b` for a login operation,
    /// `a0` for a storage operation.
    fn answer(&self) -> u8 {
        if *self == Self::Notebook {
            return 0xa9;
        }
        if self.storage() {
            0xa0
        } else {
            0x9b
        }
    }

    /// The rows an end of this operation shows.
    fn failure(&self, kind: u8, detail: u8) -> Option<&'static [&'static str]> {
        match self {
            Self::SelectorUpdate | Self::UpgradeCancel => storage_failure(kind, detail),
            Self::Upgrade { .. } => upgrade_failure(kind, detail),
            Self::Protector(managed) => protector_failure(*managed, kind, detail),
            Self::Proven(..) => proof_failure(kind, detail),
            _ => login_failure(kind, detail),
        }
    }

    /// Whether the operation may change the record.
    fn writes(&self) -> bool {
        *self != Self::Unlock
    }

    /// Whether `request` describes this operation for the configured human,
    /// authorized as it was chosen: by the chosen disk proof (consent tags
    /// 13 to 15), or by an enrolled key, whose descriptions carry none.
    fn selects(&self, request: &Request) -> bool {
        match (self, request.operation()) {
            (Self::SelectorUpdate, _) => {
                return request.owner() == 1000 && request.selector_update().is_some()
            }
            // Root's review: the chosen shape and count, keyed.
            (
                Self::Upgrade { shape, count },
                Operation::Upgrade {
                    account,
                    shape: described,
                    count: counted,
                    step: UpgradeStep::Review,
                    ..
                },
            ) => {
                return request.owner() == 1000
                    && *account == 1000
                    && described == shape
                    && counted == count
            }
            (Self::UpgradeCancel, Operation::UpgradeCancel) => return request.owner() == 1000,
            (Self::Protector(managed), operation) => {
                return request.owner() == 1000 && protector_first(*managed, operation)
            }
            (Self::Upgrade { .. } | Self::UpgradeCancel, _) => return false,
            _ => {}
        }
        let (selection, proof) = self.authorized();
        request.owner() == 1000
            && request.login_proof() == proof
            && request.login_ceiling() == Some(self.ceiling())
            && match (selection, request.operation()) {
                (Self::Unlock, Operation::LoginUnlock { account, .. })
                | (Self::Add, Operation::LoginAdd { account, .. }) => *account == 1000,
                (Self::Enroll(keys), Operation::LoginEnroll { account, after, .. }) => {
                    *account == 1000 && after == keys
                }
                (
                    Self::Remove(slots, left),
                    Operation::LoginRemove {
                        account,
                        after,
                        removed,
                        ..
                    },
                ) => *account == 1000 && after == left && removed == slots,
                _ => false,
            }
    }

    /// The success rows; a selector update's name what its form's next
    /// boot does.
    fn success(&self, request: Option<&Request>) -> &'static [&'static str] {
        match self {
            Self::Notebook => &["NOTEBOOK OPERATION FINISHED"],
            Self::Unlock => &["SESSION UNLOCKED"],
            Self::Enroll(_) => &["LOGIN KEYS ENROLLED"],
            Self::Add => &["LOGIN KEY ADDED"],
            Self::Remove(..) => &["LOGIN KEYS REMOVED"],
            Self::Proven(_, inner) => inner.success(request),
            Self::Upgrade { .. } => &[
                "THE NEXT BOOT PROTECTS THE DISK",
                "HAVE THE RECOVERY KEY READY",
            ],
            Self::UpgradeCancel => &[
                "DISK PROTECTION CANCELLED",
                "THE NEXT BOOT MAY NEED",
                "THE RECOVERY KEY",
            ],
            Self::Protector(managed) => match managed {
                Managed::Add { .. } => &["DISK SECURITY KEY ADDED"],
                Managed::Remove { .. } => &["DISK SECURITY KEY REMOVED"],
                Managed::RecoveryRemove => &["DISK RECOVERY KEY REMOVED"],
                // Then the next-boot rows its description showed.
                Managed::Request { request, .. } => match request {
                    DiskRequest::PinChange => &[
                        "PIN CHANGE REQUESTED",
                        "THE NEXT BOOT BY THE PIN",
                        "ASKS FOR A NEW PIN",
                    ],
                    DiskRequest::TpmClear => &[
                        "TPM CLEAR REQUESTED",
                        "THE NEXT BOOT ASKS TO CLEAR",
                        "THE TPM AND ALL ITS KEYS",
                        "BOOTS AFTER NEED A SECURITY KEY",
                        "OR THE RECOVERY KEY",
                    ],
                    DiskRequest::Reenroll => &[
                        "TPM RE-ENROLLMENT REQUESTED",
                        "THE NEXT BOOT BY A SECURITY KEY",
                        "OR RECOVERY KEY ASKS TO RE-ENROLL",
                    ],
                },
            },
            Self::SelectorUpdate => match request.and_then(Request::selector_update) {
                Some((SelectorForm::Staged, _)) => &[
                    "BOOT SELECTOR UPDATED",
                    "THE NEXT BOOT ASKS FOR THE DISK PIN",
                ],
                Some((SelectorForm::DeviceBound, _)) => &[
                    "BOOT SELECTOR UPDATED",
                    "THE NEXT BOOT NEEDS THE RECOVERY KEY",
                ],
                _ => &["BOOT SELECTOR UPDATED"],
            },
        }
    }
}

/// Whether `operation` is root's first description of `managed`: account
/// 1000, the selection's proof, role and request, and its first step, the
/// proof's, a removal's choose step, or tag 21's keyed identify step.
fn protector_first(managed: Managed, operation: &Operation) -> bool {
    let first = |proof| match proof {
        Proof::RecoveryKey => LoginStep::RecoveryKey,
        Proof::DiskKey => LoginStep::DiskIdentify,
    };
    match (managed, operation) {
        (
            Managed::Add { proof, role },
            Operation::ProtectorAdd {
                account,
                proof: described,
                role: taken,
                step,
            },
        ) => *account == 1000 && *described == proof && *taken == role && *step == first(proof),
        (
            Managed::Remove { proof },
            Operation::ProtectorRemove {
                account,
                proof: described,
                removal,
                step: LoginStep::Choose,
            },
        ) => *account == 1000 && *described == proof && removal.chosen == 0,
        (
            Managed::RecoveryRemove,
            Operation::RecoveryRemove {
                account,
                step: LoginStep::DiskIdentify,
                ..
            },
        ) => *account == 1000,
        (
            Managed::Request { proof, request },
            Operation::ProtectorRequest {
                account,
                proof: described,
                request: requested,
                step,
                ..
            },
        ) => {
            *account == 1000
                && *described == proof
                && *requested == request
                && *step == first(proof)
        }
        _ => false,
    }
}

/// The key-management screen's removal digits over the enrolled keys in
/// canonical slot order: the list of root's last `1a` answer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Removal {
    keys: Vec<Fingerprint>,
    chosen: BTreeSet<u8>,
}

impl Removal {
    pub fn new(keys: Vec<Fingerprint>) -> Option<Self> {
        (1..=usize::from(LOGIN_KEYS))
            .contains(&keys.len())
            .then(|| Self {
                keys,
                chosen: BTreeSet::new(),
            })
    }

    /// A digit names a key by its 1-based position, and again deselects it.
    /// Answers whether the digit names a key.
    pub fn toggle(&mut self, position: u8) -> bool {
        if position == 0 || usize::from(position) > self.keys.len() {
            return false;
        }
        if !self.chosen.remove(&position) {
            self.chosen.insert(position);
        }
        true
    }

    /// What the screen shows: the key count, and the chosen positions as
    /// bits from the lowest for position 1.
    pub fn shown(&self) -> Notice {
        let keys = u8::try_from(self.keys.len()).unwrap_or(LOGIN_KEYS);
        let chosen = self.chosen.iter().fold(0u8, |bits, position| {
            bits | 1u8
                .checked_shl(u32::from(position.saturating_sub(1)))
                .unwrap_or(0)
        });
        Notice::Removing { keys, chosen }
    }

    /// Enter: the chosen slots, or nothing while none is chosen.
    pub fn selection(&self) -> Option<LoginSelection> {
        if self.chosen.is_empty() {
            return None;
        }
        let slots = self
            .chosen
            .iter()
            .map(|position| {
                let index = usize::from(*position).checked_sub(1)?;
                Some(Slot {
                    position: *position,
                    key: *self.keys.get(index)?,
                })
            })
            .collect::<Option<Vec<_>>>()?;
        let after = self.keys.len().checked_sub(slots.len())?;
        Some(LoginSelection::Remove(slots, u8::try_from(after).ok()?))
    }
}

/// A login step's prompt this output cannot hold whole, which is refused
/// rather than clipped: the operation ends before its token I/O.
pub(crate) const TOO_SMALL: &[&str] = &["THIS SCREEN IS TOO SMALL", "FOR THIS PROMPT"];

/// `1a`'s unavailable cause for an unenrolled protected machine
/// (td-authd/DESIGN.md, amendment 9), which no failure kind shares.
pub(crate) const PROTECTED: u8 = 0x0d;
/// Its rows: no login key, and the boot that admits the session.
pub(crate) const PROTECTED_ROWS: &[&str] = &[
    "NO LOGIN KEYS ENROLLED",
    "BOOT WITH THE DISK PIN OR A",
    "PRIMARY DISK KEY TO UNLOCK",
];

/// The rows an unavailable `1a` state shows: its cause's failure rows,
/// or `PROTECTED_ROWS`.
pub(crate) fn unavailable_rows(cause: u8) -> Option<&'static [&'static str]> {
    match cause {
        PROTECTED => Some(PROTECTED_ROWS),
        0x0a..=0x0c => login_failure(cause, 0),
        _ => None,
    }
}

/// TOKEN-LOGIN.md's text for root's terminal kind and detail, in rows the
/// narrowest output's chrome holds whole, or none for a pair root never
/// sends.
/// Cancellation adds nothing: the person chose it, or this client did and
/// already said why.
pub(crate) fn login_failure(kind: u8, detail: u8) -> Option<&'static [&'static str]> {
    Some(match (kind, detail) {
        (0x01, _) => &["WRONG PIN"],
        (0x02, 0) => &["REMOVE AND REINSERT THIS KEY"],
        (0x03, 0) => &["PIN BLOCKED; USE ANOTHER KEY"],
        (0x04, 0) => &["THIS KEY IS NOT ENROLLED HERE"],
        (0x05, 0) => &["CONNECT EXACTLY ONE KEY"],
        (0x06, 1) => &["KEY HAS NO HMAC-SECRET"],
        (0x06, 2) => &["KEY ALWAYS REQUIRES UV"],
        (0x06, 3) => &["KEY CANNOT HOLD A PIN"],
        (0x06, 4) => &["SET A PIN WITH ANOTHER TOOL"],
        (0x06, 5) => &["KEY CANNOT LIST ENROLLED KEYS"],
        (0x06, 6) => &["KEY CANNOT BE SELECTED"],
        (0x07, 0) => &["TOUCH DENIED"],
        (0x08, 0) => &["TIMED OUT"],
        (0x09, 0) => &["NO LOGIN KEYS ENROLLED"],
        (0x0a, 0) => &["LOGIN KEY STATE UNAVAILABLE:", "DIRECTORY DAMAGED"],
        (0x0b, 0) => &["LOGIN KEY STATE UNAVAILABLE:", "RECORD DAMAGED"],
        (0x0c, 0) => &["LOGIN KEY STATE UNAVAILABLE:", "STATE COULD NOT BE READ"],
        (0x0d, 0) => &["KEYS CHANGED; NOTHING WRITTEN"],
        (0x0e, 1..=5) => &["RESULT UNCERTAIN"],
        (0x0f | 0x10, 0) => &["THE OPERATION FAILED"],
        (0x11, 0) => &["KEY ALREADY ENROLLED"],
        (0x12, 0) => &["A RETAINED SYSTEM CANNOT READ KEYS"],
        (0x80, 0) => &[],
        (0x81, 0) => &["EIGHT KEYS ALREADY ENROLLED"],
        (0x82, 0) => &["LOGIN KEYS ALREADY ENROLLED"],
        (0x83, 0) => &["KEY LIST CHANGED; REOPEN"],
        (0x84, 0) => &["ENROLLING HERE NEEDS A DISK PROOF"],
        (0x85, 0) => &["THE LAST KEY STAYS ON THIS MACHINE"],
        _ => return None,
    })
}

/// A disk proof's end (td-login/TOKEN-LOGIN.md, "Enrollment, addition and
/// removal"): root's `86`, the storage worker's own kinds, and otherwise a
/// login operation's.
pub(crate) fn proof_failure(kind: u8, detail: u8) -> Option<&'static [&'static str]> {
    Some(match (kind, detail) {
        (0x86, 0) => &["THIS MACHINE HAS NO DISK PROOF"],
        (0x20, 0) => &["NO ENCRYPTED TD VOLUME"],
        (0x21, 0) => &["THE DISK CANNOT DO THIS NOW"],
        (0x25, 0) => &["RECOVERY KEY MISTYPED"],
        (0x26, 0) => &["WRONG KEY FOR THIS DISK"],
        (0x28, 0) => &["THE DISK CHANGED; NOTHING WRITTEN"],
        _ => return login_failure(kind, detail),
    })
}

/// DESIGN.md's "Storage operations" text for a selector update's end, or
/// none for a pair root never sends one.
pub(crate) fn storage_failure(kind: u8, detail: u8) -> Option<&'static [&'static str]> {
    Some(match (kind, detail) {
        (0x08, 0) => &["TIMED OUT"],
        (0x0e, 0) => &["RESULT UNCERTAIN"],
        (0x0f | 0x10, 0) => &["THE OPERATION FAILED"],
        (0x20, 0) => &["NO ENCRYPTED TD VOLUME"],
        (0x21, 0) => &["THE DISK CANNOT DO THIS NOW"],
        (0x22, 0) => &[
            "RESTART INTO THE CURRENT SYSTEM",
            "BEFORE UPDATING THE SELECTOR",
        ],
        (0x23, 0) => &["THE SYSTEM BOOT FILES DO NOT", "MATCH ITS MANIFEST"],
        (0x24, 0) => &["THE EFI PARTITION IS FOREIGN", "OR HAS TOO LITTLE SPACE"],
        (0x28, 0) => &["THE DISK CHANGED; NOTHING WRITTEN"],
        (0x80, 0) => &[],
        _ => return None,
    })
}

/// DESIGN.md's "Storage operations" text for an upgrade's end: its own, a
/// new key's as a first enrollment's, and otherwise a selector update's.
pub(crate) fn upgrade_failure(kind: u8, detail: u8) -> Option<&'static [&'static str]> {
    Some(match (kind, detail) {
        (0x27, 1) => &["NO USABLE TPM:", "NONE FOUND", "CHOOSE SECURITY KEYS ONLY"],
        (0x27, 2) => &[
            "NO USABLE TPM:",
            "NO SHA-256 PCR BANK",
            "CHOOSE SECURITY KEYS ONLY",
        ],
        (0x27, 3) => &[
            "NO USABLE TPM:",
            "ITS LOCKOUT IS ALREADY SET",
            "CHOOSE SECURITY KEYS ONLY",
        ],
        (0x87, 0) => &["NEEDS AN ENROLLED LOGIN KEY"],
        (0x22, 0) => &[
            "RESTART INTO THE CURRENT SYSTEM",
            "BEFORE PROTECTING THE DISK",
        ],
        (0x0e, 0) => &[
            "RESULT UNCERTAIN",
            "THE NEXT BOOT MAY NEED",
            "THE RECOVERY KEY",
        ],
        (0x01, _) | (0x06, 1..=6) | (0x02 | 0x03 | 0x05 | 0x07 | 0x11, 0) => {
            return login_failure(kind, detail)
        }
        _ => return storage_failure(kind, detail),
    })
}

/// DESIGN.md's "Protector management" text for its ends: root's `21`
/// details, the chosen key's `04`, an addition's uncertain end, a proof's
/// or a new key's kinds as a disk proof's, and otherwise a selector
/// update's.
pub(crate) fn protector_failure(
    managed: Managed,
    kind: u8,
    detail: u8,
) -> Option<&'static [&'static str]> {
    Some(match (kind, detail) {
        (0x21, 1) => &["THE DISK TAKES NO MORE KEYS", "IN THAT ROLE"],
        (0x21, 2) => &[
            "EVERY PROTECTOR LEFT IS NEEDED",
            "TO KEEP THE DISK OPENABLE",
        ],
        (0x21, 3) => &["THE DISK HAS NO TPM PROTECTOR", "FOR THIS REQUEST"],
        (0x04, 1) => &[
            "THAT IS THE KEY BEING REMOVED",
            "AUTHORIZE WITH ANOTHER KEY",
        ],
        (0x0e, 0) if matches!(managed, Managed::Add { .. }) => &[
            "RESULT UNCERTAIN",
            "THE NEW KEY MAY OPEN THE DISK",
            "EVEN IF IT IS NOT LISTED",
        ],
        (0x01, _) | (0x06, 1..=6) | (0x02..=0x05 | 0x07 | 0x11 | 0x25 | 0x26, 0) => {
            return proof_failure(kind, detail)
        }
        _ => return storage_failure(kind, detail),
    })
}

pub(crate) struct Attempt {
    origin: EvdevOrigin,
    runtime: Arc<Mutex<Runtime>>,
    selection: Selection,
    state: AtomicU8,
    deadline: Option<Instant>,
    presentation: Mutex<Option<Presented>>,
    confirmed: AtomicBool,
    /// The PIN field, shared by the evdev adapter that types into it and
    /// the authority worker that sends what it holds.
    field: Mutex<PinField>,
    /// The evdev adapter's seat that opened this lifetime: a login unlock's
    /// success ends the lifetime through it.
    seat: Option<crate::input::Seat>,
}

impl Attempt {
    #[cfg(test)]
    pub fn new(
        origin: EvdevOrigin,
        runtime: Arc<Mutex<Runtime>>,
        selection: impl Into<Selection>,
    ) -> Arc<Self> {
        Self::seated(origin, runtime, selection, None)
    }

    /// An attempt the evdev adapter's `seat` opened.
    pub fn seated(
        origin: EvdevOrigin,
        runtime: Arc<Mutex<Runtime>>,
        selection: impl Into<Selection>,
        seat: Option<crate::input::Seat>,
    ) -> Arc<Self> {
        let selection = selection.into();
        Arc::new(Self {
            origin,
            runtime,
            deadline: Instant::now().checked_add(selection.lifetime()),
            selection,
            state: AtomicU8::new(ACTIVE),
            presentation: Mutex::new(None),
            confirmed: AtomicBool::new(false),
            field: Mutex::new(PinField::default()),
            seat,
        })
    }

    #[cfg(test)]
    pub fn selection(&self) -> &Selection {
        &self.selection
    }

    /// A login unlock whose last step this client committed, with no
    /// cancellation since: an Escape that came first keeps the session
    /// locked, whatever root then reports.
    pub fn unlock_committed(&self) -> bool {
        self.selection == Selection::Login(LoginSelection::Unlock)
            && self.state.load(Ordering::SeqCst) == COMMITTED
    }

    /// Root reported this login unlock's success: the session leaves its
    /// lock surface and the lifetime ends, through the seat that opened it
    /// and under its locks, so no Escape interleaves.
    fn unlocked(&self) -> Result<(), String> {
        match &self.seat {
            Some(seat) if self.unlock_committed() => seat.unlocked(self),
            _ => Ok(()),
        }
    }

    /// Closes this attempt's screen through the seat that opened it, as
    /// Escape would; only a committed installation asks.
    fn release(&self) -> Result<(), String> {
        if self.state.load(Ordering::SeqCst) != COMMITTED {
            return Ok(());
        }
        match &self.seat {
            Some(seat) => seat.release(self),
            None => Err("no input seat opened this attention".into()),
        }
    }

    /// Input records cancellation before waiting for painting or channel I/O.
    /// Escape's cancellation zeroes the PIN field too. The mark is one
    /// atomic change of the state `take_pin` moves to `SENDING`, so exactly
    /// one of them comes first.
    pub fn cancel(&self) {
        self.state.fetch_or(CANCELLED, Ordering::SeqCst);
        self.close_field();
    }

    /// Closes the PIN field, zeroing it, whatever a panicking holder left.
    fn close_field(&self) {
        self.field
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .close();
    }

    /// Opens the PIN field beneath `step`, the presented PIN step root asked
    /// a PIN for, and waits for its paint to be on glass: until then no key
    /// types, and afterwards only a press made after it.
    fn open_field(&self, step: &Request) -> Result<(), String> {
        let presentation = {
            let mut field = self.field.lock().unwrap_or_else(PoisonError::into_inner);
            if !self.active() {
                field.close();
                return Err("physical attention was cancelled".into());
            }
            field.open_for(Profile::of(step));
            let shown = self
                .runtime
                .lock()
                .map_err(|_| "runtime lock poisoned".to_string())
                .and_then(|mut runtime| {
                    runtime.show_attention_field(&self.origin, step, Field::Pin(0))
                });
            match shown {
                Ok(presentation) => presentation,
                Err(error) => {
                    field.close();
                    return Err(error);
                }
            }
        };
        // Outside both locks, since the completion is delivered under the
        // runtime's; bounded as a prompt's presentation is.
        let completed = Instant::now()
            .checked_add(crate::runtime::ATTENTION_PRESENTATION_DEADLINE)
            .into_iter()
            .chain(self.deadline)
            .min()
            .ok_or_else(|| "physical secret operation expired".to_string())
            .and_then(|deadline| presentation.wait(deadline));
        let mut field = self.field.lock().unwrap_or_else(PoisonError::into_inner);
        let completed = completed.and_then(|()| {
            self.runtime
                .lock()
                .map_err(|_| "runtime lock poisoned".to_string())?
                .finish_field_presentation(&presentation, step)
        });
        match completed {
            Ok(completed) if field.is_open() && self.active() => {
                field.shown = Some(completed);
                Ok(())
            }
            Ok(_) => {
                field.close();
                Err("physical attention was cancelled".into())
            }
            Err(error) => {
                field.close();
                Err(error)
            }
        }
    }

    /// One key for the PIN field, which only the evdev adapter can offer,
    /// pressed at `timestamp`. A field the person cannot see takes nothing
    /// more: a failed repaint zeroes it and cancels the attempt.
    pub fn field_key(
        &self,
        _origin: &EvdevOrigin,
        key: FieldKey,
        timestamp: u128,
    ) -> Result<(), String> {
        let mut field = self.field.lock().unwrap_or_else(PoisonError::into_inner);
        if !field.is_open() {
            return Ok(());
        }
        if !self.active() {
            field.close();
            return Ok(());
        }
        // A press from before the field was on glass, queued or delivered
        // late, was not made at it: the rule a prompt's confirmation keeps.
        if !field.shown.is_some_and(|completed| timestamp > completed) {
            return Ok(());
        }
        let Some(length) = field.key(key) else {
            return Ok(());
        };
        let shown = u8::try_from(length)
            .map_err(|_| "PIN field length overflow".to_string())
            .and_then(|length| {
                self.runtime
                    .lock()
                    .map_err(|_| "runtime lock poisoned".to_string())?
                    .update_attention_field(&self.origin, Field::Pin(length))
            });
        if shown.is_err() {
            field.close();
            self.state.fetch_or(CANCELLED, Ordering::SeqCst);
        }
        Ok(())
    }

    /// A submitted PIN copied into `into` and zeroed in the field: its
    /// length. The attempt moves from `ACTIVE` to `SENDING` first, in the
    /// one compare-exchange cancellation's mark races: a cancellation that
    /// came first leaves nothing copied, and one after it is honoured by
    /// `sending` up to the write. `finish_sending` ends the send.
    fn take_pin(&self, into: &mut [u8]) -> Option<usize> {
        let mut field = self.field.lock().unwrap_or_else(PoisonError::into_inner);
        if !field.is_open() || !field.submitted {
            return None;
        }
        if self.expired()
            || self
                .state
                .compare_exchange(ACTIVE, SENDING, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
        {
            field.close();
            return None;
        }
        let taken = field.take(into);
        if taken.is_none() {
            self.finish_sending();
        }
        taken
    }

    /// Whether a taken PIN may still be written: no cancellation since the
    /// take, and the deadline not passed.
    fn sending(&self) -> bool {
        self.state.load(Ordering::SeqCst) == SENDING && !self.expired()
    }

    /// Back to `ACTIVE` after a send, unless cancellation marked it.
    fn finish_sending(&self) {
        let _ = self
            .state
            .compare_exchange(SENDING, ACTIVE, Ordering::SeqCst, Ordering::SeqCst);
    }

    /// After the PIN: the key wants its touch.
    fn show_touch(&self, step: &Request) -> Result<(), String> {
        self.runtime
            .lock()
            .map_err(|_| "runtime lock poisoned".to_string())?
            .show_attention_field(&self.origin, step, Field::Touch)
            .map(|_| ())
    }

    #[cfg(test)]
    fn field_raw(&self) -> [u8; PIN_CAPACITY] {
        self.field
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .raw()
    }

    fn expired(&self) -> bool {
        self.deadline
            .is_none_or(|deadline| Instant::now() >= deadline)
    }

    fn active(&self) -> bool {
        self.state.load(Ordering::SeqCst) == ACTIVE
            && self
                .deadline
                .is_some_and(|deadline| Instant::now() < deadline)
    }

    fn commit(&self) -> bool {
        if !self.active() || (self.selection.confirms() && !self.confirmed.load(Ordering::SeqCst)) {
            return false;
        }
        self.state
            .compare_exchange(ACTIVE, COMMITTED, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    fn present(&self, request: Request) -> Result<Request, String> {
        if !self.active() {
            return Err("physical attention was cancelled".into());
        }
        let presentation = {
            let mut runtime = self.runtime.lock().map_err(|_| "runtime lock poisoned")?;
            if !self.active() {
                return Err("physical attention was cancelled".into());
            }
            let remaining = self
                .deadline
                .and_then(|deadline| deadline.checked_duration_since(Instant::now()))
                .map(|duration| duration.as_secs())
                .filter(|seconds| *seconds > 0)
                .ok_or("physical secret operation expired")?;
            runtime.begin_attention_presentation(&self.origin, request, Some(remaining))?
        };
        // Outside the runtime lock, because the completion this waits for is
        // delivered under it. Bounded by the attempt's own deadline too.
        let waited = Instant::now()
            .checked_add(crate::runtime::ATTENTION_PRESENTATION_DEADLINE)
            .into_iter()
            .chain(self.deadline)
            .min()
            .ok_or_else(|| "physical secret operation expired".to_string())
            .and_then(|deadline| presentation.wait(deadline));
        let mut runtime = self.runtime.lock().map_err(|_| "runtime lock poisoned")?;
        let receipt = match waited {
            Ok(()) => runtime.finish_attention_presentation(presentation)?,
            Err(error) => {
                runtime.abandon_attention_presentation(presentation.request());
                return Err(error);
            }
        };
        // Also the attempt's deadline, which a paint that landed before a
        // late wait does not excuse. Its prompt is withdrawn with it, since
        // nothing will answer it now.
        if !self.active() {
            runtime.abandon_attention_presentation(receipt.request());
            return Err("physical attention was cancelled during presentation".into());
        }
        let completed = receipt.completed();
        let request = receipt.into_request();
        if self.selection.confirms() || matches!(self.selection, Selection::Login(_)) {
            let mut presentation = self
                .presentation
                .lock()
                .map_err(|_| "presentation receipt lock poisoned")?;
            // A login step replaces the one before it, whose approval is
            // spent: it goes with it, under the lock an Enter sets it.
            if matches!(self.selection, Selection::Login(_)) {
                self.confirmed.store(false, Ordering::SeqCst);
            }
            *presentation =
                (self.selection.confirms() || request.awaits_approval()).then(|| Presented {
                    request: request.clone(),
                    after: completed,
                    decided: false,
                    approved_at: None,
                });
        }
        Ok(request)
    }

    /// Whether `request`, the presented login disclosure step, was approved
    /// by Enter: only then does its receipt go to root.
    fn approved(&self, request: &Request) -> bool {
        self.confirmed.load(Ordering::SeqCst)
            && self.presentation.lock().is_ok_and(|presented| {
                presented
                    .as_ref()
                    .is_some_and(|presented| presented.request == *request)
            })
    }

    /// Whether a press stamped `timestamp` can answer `presented`: strictly
    /// later than its `after`, within this attempt's lifetime, while that
    /// exact prompt is still visible outside drain.
    fn answers(&self, runtime: &Runtime, presented: &Presented, timestamp: u128) -> bool {
        timestamp > presented.after
            && self.active()
            && runtime.attention_request_visible(&presented.request)
    }

    /// A fresh Enter, which only the physical evdev adapter can offer,
    /// confirms a live boot's whole-disk installation and nothing else: an
    /// update is approved through `approve`.
    pub fn confirm_install(&self, _origin: &EvdevOrigin, timestamp: u128) -> Result<(), String> {
        if self.selection != Selection::Install || !self.active() {
            return Ok(());
        }
        let runtime = self.runtime.lock().map_err(|_| "runtime lock poisoned")?;
        let presentation = self
            .presentation
            .lock()
            .map_err(|_| "presentation receipt lock poisoned")?;
        if let Some(presented) = &*presentation {
            if matches!(presented.request.operation(), Operation::InstallDisk { .. })
                && self.answers(&runtime, presented, timestamp)
            {
                self.confirmed.store(true, Ordering::SeqCst);
            }
        }
        Ok(())
    }

    /// The first press, pressed at `timestamp`, that a presented prompt
    /// awaiting approval counts, which only the physical evdev adapter can
    /// offer (an update, an elevation, a protector step or a login
    /// disclosure step): answers whether it ended the request. A press the
    /// prompt cannot answer (stamped before the prompt was on glass, a
    /// withdrawn or replaced prompt, one already decided) neither approves
    /// nor ends it. Enter at least `APPROVAL_SETTLE` after the prompt was on
    /// glass approves; any other key, or an Enter sooner, cancels the
    /// attempt as Escape does, and the adapter then drains the screen.
    /// Keyboards are read apart, so the first press delivered need not be
    /// the first pressed: until the commit, another key stamped before the
    /// approving Enter withdraws it and ends the request the same way.
    pub fn approve(
        &self,
        _origin: &EvdevOrigin,
        press: ApprovalPress,
        timestamp: u128,
    ) -> Result<bool, String> {
        if !matches!(
            self.selection,
            Selection::Install | Selection::Elevation(_) | Selection::Login(_)
        ) || !self.active()
        {
            return Ok(false);
        }
        let ended = {
            let runtime = self.runtime.lock().map_err(|_| "runtime lock poisoned")?;
            let mut presentation = self
                .presentation
                .lock()
                .map_err(|_| "presentation receipt lock poisoned")?;
            let Some(presented) = presentation.as_mut().filter(|presented| {
                presented.request.awaits_approval() && self.answers(&runtime, presented, timestamp)
            }) else {
                return Ok(false);
            };
            if presented.decided {
                let earlier = press == ApprovalPress::Other
                    && presented.approved_at.is_some_and(|at| timestamp < at);
                if !earlier {
                    return Ok(false);
                }
                // The commit takes ACTIVE to COMMITTED and this takes it to
                // CANCELLED: exactly one of the two has the request, and a
                // login receipt needs the attempt still active.
                if self
                    .state
                    .compare_exchange(ACTIVE, CANCELLED, Ordering::SeqCst, Ordering::SeqCst)
                    .is_err()
                {
                    return Ok(false);
                }
                presented.approved_at = None;
                self.confirmed.store(false, Ordering::SeqCst);
                drop(presentation);
                drop(runtime);
                self.close_field();
                return Ok(true);
            }
            presented.decided = true;
            let settled = presented
                .after
                .checked_add(APPROVAL_SETTLE)
                .is_some_and(|settled| timestamp >= settled);
            let approved = press == ApprovalPress::Enter && settled;
            if approved {
                presented.approved_at = Some(timestamp);
                self.confirmed.store(true, Ordering::SeqCst);
            }
            !approved
        };
        // Outside the runtime's guard, since cancellation takes the PIN
        // field's, which every holder takes before the runtime's.
        if ended {
            self.cancel();
        }
        Ok(ended)
    }

    pub fn notice(&self, notice: crate::attention::Notice) -> Result<(), String> {
        let visible = || matches!(self.state.load(Ordering::SeqCst), ACTIVE | COMMITTED);
        if !visible() {
            return Ok(());
        }
        let mut runtime = self.runtime.lock().map_err(|_| "runtime lock poisoned")?;
        if !visible() {
            return Ok(());
        }
        runtime.attention_notice(&self.origin, notice).map(|_| ())
    }
}

struct Pending {
    attempt: Arc<Attempt>,
    request: Request,
    receipt: Option<Request>,
    committed: bool,
    cancelled: bool,
    /// When a committed device-bound installation's notice was shown.
    returning: Option<Instant>,
    /// Whether that installation's screen was asked to close.
    released: bool,
}

/// A whole-disk installation whose recovery key td-setup shows and takes
/// back: it completes only after the attention screen has closed.
fn returns_for_recovery_key(request: &Request) -> bool {
    matches!(
        request.operation(),
        Operation::InstallDisk {
            preserve: false,
            storage: crate::authority::consent::Storage::DeviceBound,
            ..
        }
    )
}

struct Inspection {
    attempt: Arc<Attempt>,
    begin: bool,
}

/// A login-key operation from root's `9b 01` on. Root's descriptions alone
/// are checked here (td-authd/DESIGN.md, "Immutable consent description
/// prerequisite"); the record and the worker's report stay root's.
#[cfg_attr(test, derive(Clone))]
struct LoginPending {
    attempt: Arc<Attempt>,
    selection: LoginSelection,
    nonce: [u8; 32],
    /// Root's current description; none before the worker's baseline.
    request: Option<Request>,
    receipt: Option<Request>,
    /// The current step, presented and waiting for its approving Enter
    /// before its receipt: a disclosure step.
    shown: Option<Request>,
    /// The current step is the operation's final one: only its commit
    /// follows.
    last: bool,
    /// An addition's authorizing key, which its new key's steps never name.
    authorizer: Option<Fingerprint>,
    /// The position a removal's choose step sent, which its next step
    /// must carry.
    chosen: Option<u8>,
    /// The current PIN step's field has opened.
    field: bool,
    /// The current PIN step's `1c` was sent: only then may its successor or
    /// its commit follow.
    pin_sent: bool,
    committed: bool,
    cancelled: bool,
}

/// However the operation leaves this client, its PIN field is zeroed.
impl Drop for LoginPending {
    fn drop(&mut self) {
        self.attempt.close_field();
    }
}

pub(crate) struct Client {
    pending: Option<Pending>,
    login: Option<LoginPending>,
    /// A login operation ended since the authority worker last asked: its
    /// cue to read the login state again (`1a`).
    login_ended: bool,
    inspection: Option<Inspection>,
    /// The revocation byte of root's last `1a`.
    revocation: crate::authority::Revocation,
    /// The check before a PIN field opens.
    memory: fn() -> Result<(), String>,
}

impl Default for Client {
    fn default() -> Self {
        Self {
            pending: None,
            login: None,
            login_ended: false,
            inspection: None,
            revocation: crate::authority::Revocation::Settled,
            memory: protected_memory,
        }
    }
}

#[cfg(test)]
impl Client {
    /// A client whose memory check passes, whatever the test host's swap.
    pub(crate) fn trusting_memory() -> Self {
        Self {
            memory: || Ok(()),
            ..Self::default()
        }
    }

    /// A client a login operation just ended in.
    pub(crate) fn with_login_ended() -> Self {
        Self {
            login_ended: true,
            ..Self::default()
        }
    }
}

impl Client {
    /// Whether a login operation ended since the last call.
    pub fn take_login_end(&mut self) -> bool {
        std::mem::take(&mut self.login_ended)
    }

    /// Root's last `1a` revocation byte. While it reads `02` no request
    /// that takes the operation slot is sent, a continuation included,
    /// since root refuses one as a protocol violation while it restarts
    /// the machine after a failed revocation (td-authd/DESIGN.md,
    /// amendment 7): the attempt shows that notice instead.
    pub fn set_revocation(&mut self, revocation: crate::authority::Revocation) {
        self.revocation = revocation;
    }

    /// `Some` while root restarts: the attempt shows that notice, or
    /// nothing once it is no longer active, and no request is sent.
    fn restarting(&self, attempt: &Attempt) -> Option<Result<(), String>> {
        (self.revocation == crate::authority::Revocation::Restarting).then(|| {
            if attempt.active() {
                attempt.notice(Notice::Restarting)
            } else {
                Ok(())
            }
        })
    }

    pub fn start(&mut self, wire: &mut impl Exchange, attempt: Arc<Attempt>) -> Result<(), String> {
        if !attempt.active() {
            return Ok(());
        }
        if self.pending.is_some() || self.login.is_some() || self.inspection.is_some() {
            return attempt.notice(Notice::Busy);
        }
        if matches!(attempt.selection, Selection::Enroll(_)) {
            return self.inspect(wire, attempt, true);
        }
        if let Selection::Login(selection) = &attempt.selection {
            let selection = selection.clone();
            return self.begin_login(wire, attempt, selection);
        }
        self.begin(wire, attempt)
    }

    fn begin_login(
        &mut self,
        wire: &mut impl Exchange,
        attempt: Arc<Attempt>,
        selection: LoginSelection,
    ) -> Result<(), String> {
        if let Some(shown) = self.restarting(&attempt) {
            return shown;
        }
        let response = wire.exchange(&selection.request()?)?;
        // Root starts every login selection (TOKEN-LOGIN.md increment 5's
        // A5): any other answer, the old refusal `9b 00` included, is a
        // violation. A storage selection's `a0 00` is root's refusal as
        // this build's (td-authd/DESIGN.md amendment 10).
        let answer = selection.answer();
        let nonce = match response.as_slice() {
            [0xa9, 2] if answer == 0xa9 => return attempt.notice(Notice::NotAvailable),
            [0xa9, 0] if answer == 0xa9 => {
                return attempt.notice(Notice::Login(&[
                    "CHOOSE AN OPERATION IN THE NOTEBOOK WINDOW",
                    "THEN RETURN HERE AND PRESS N",
                ]))
            }
            [0xa0, 0] if answer == 0xa0 => return attempt.notice(Notice::NotAvailable),
            [tag, 1, nonce @ ..] if *tag == answer => {
                <[u8; 32]>::try_from(nonce).map_err(|_| "invalid login operation start")?
            }
            _ => return Err("invalid login operation start".into()),
        };
        if nonce == [0; 32] {
            return Err("invalid login operation nonce".into());
        }
        self.login = Some(LoginPending {
            attempt,
            selection,
            nonce,
            request: None,
            receipt: None,
            shown: None,
            last: false,
            authorizer: None,
            chosen: None,
            field: false,
            pin_sent: false,
            committed: false,
            cancelled: false,
        });
        Ok(())
    }

    fn tick_notebook(&mut self, wire: &mut impl Exchange) -> Result<(), String> {
        let Some(pending) = &mut self.login else {
            return Ok(());
        };
        if !pending.attempt.active() && !pending.cancelled {
            pending.abandon(wire)?;
        }
        let response = wire.exchange(&[0x11])?;
        let [0x91, status, rest @ ..] = response.as_slice() else {
            return Err("invalid notebook status".into());
        };
        if *status == 0x0b && rest.is_empty() && pending.request.is_none() {
            return Ok(());
        }
        if matches!(*status, 6 | 7) {
            if !rest.is_empty() {
                let description = Request::decode(rest)?;
                if description.nonce() != &pending.nonce
                    || description.owner() != 1000
                    || !matches!(description.operation(), Operation::Notebook(_))
                {
                    return Err("notebook end changed its identity".into());
                }
            }
            pending.attempt.close_field();
            let attempt = Arc::clone(&pending.attempt);
            self.login = None;
            return attempt.notice(Notice::Login(if *status == 6 {
                &["NOTEBOOK OPERATION FINISHED"]
            } else {
                &["NOTEBOOK CANCELLED OR FAILED", "VAULT LOCKED"]
            }));
        }
        let description = Request::decode(rest)?;
        let Operation::Notebook(next) = description.operation() else {
            return Err("notebook status names another operation".into());
        };
        if description.nonce() != &pending.nonce || description.owner() != 1000 {
            return Err("notebook status changed identity".into());
        }
        if pending.request.as_ref() != Some(&description) {
            if !matches!(*status, 3 | 4) {
                return Err("notebook step was not presented".into());
            }
            if let Some(current) = &pending.request {
                let Operation::Notebook(before) = current.operation() else {
                    return Err("missing notebook binding".into());
                };
                let same_operation =
                    before.operation == next.operation && before.action == next.action;
                let repeated_exchange = next.step == 0
                    && matches!((before.method, before.step), (0x30, 3) | (0x31, 1))
                    && pending.pin_sent;
                if pending.receipt.as_ref() != Some(current)
                    || (current.asks_pin() && !pending.pin_sent)
                    || !same_operation
                    || !(next.follows(before) || repeated_exchange)
                {
                    return Err("notebook skipped or changed a presented step".into());
                }
            } else if next.step != 0 {
                return Err("notebook omitted initial approval".into());
            }
            pending.attempt.close_field();
            pending.request = Some(description.clone());
            pending.receipt = None;
            pending.shown = None;
            pending.field = false;
            pending.pin_sent = false;
        }
        let presented = pending.receipt.as_ref() == Some(&description);
        match *status {
            3 => {}
            4 if !pending.cancelled && !presented => {
                if pending.shown.is_none() {
                    match pending.attempt.present(description.clone()) {
                        Ok(receipt) if receipt == description && pending.attempt.active() => {
                            pending.shown = Some(receipt)
                        }
                        _ => return pending.abandon(wire),
                    }
                }
                if (!description.awaits_approval() || pending.attempt.approved(&description))
                    && pending.attempt.active()
                {
                    let mut receipt = vec![0x13];
                    receipt.extend_from_slice(&description.encode());
                    if wire.exchange(&receipt)? != [0x93] {
                        return Err("invalid notebook receipt acknowledgement".into());
                    }
                    pending.receipt = pending.shown.take();
                }
            }
            0x0c if presented
                && !pending.cancelled
                && description.asks_pin()
                && !pending.pin_sent =>
            {
                if pending.field {
                    pending.send_pin(wire, &description)?;
                } else {
                    pending.open_field(wire, self.memory, &description)?;
                }
            }
            _ => return Err("out-of-order notebook status".into()),
        }
        Ok(())
    }

    fn tick_login(&mut self, wire: &mut impl Exchange) -> Result<(), String> {
        if self
            .login
            .as_ref()
            .is_some_and(|p| p.selection == LoginSelection::Notebook)
        {
            return self.tick_notebook(wire);
        }
        let memory = self.memory;
        let Some(login) = &mut self.login else {
            return Ok(());
        };
        if !login.attempt.active() && !login.committed && !login.cancelled {
            login.abandon(wire)?;
        }
        let response = wire.exchange(&[0x11])?;
        let [0x91, status, rest @ ..] = response.as_slice() else {
            return Err("invalid login operation status".into());
        };
        match *status {
            0x0b if rest.is_empty() && login.request.is_none() => return Ok(()),
            0x0d | 0x0e => {
                let [kind, detail, description @ ..] = rest else {
                    return Err("invalid login operation end".into());
                };
                // Root describes every answer from its first step on.
                let described = !description.is_empty();
                if login.request.is_some() && !described {
                    return Err("login operation end lost its description".into());
                }
                if described {
                    let description = Request::decode(description)?;
                    if login.committed && login.request.as_ref() != Some(&description) {
                        return Err("login operation end changed its committed step".into());
                    }
                    login.admit(&description, *status)?;
                }
                // Only an acknowledged write's commit can leave its outcome
                // uncertain, and from then on nothing else can be reported
                // but a storage operation's `28`, which its worker sends
                // only before any write.
                let uncertain = *status == 0x0e;
                let definite = login.selection.storage() && *kind == 0x28;
                if uncertain != (login.committed && login.selection.writes() && !definite) {
                    return Err("login operation reported the wrong kind of end".into());
                }
                if !login.ends(uncertain, *kind, *detail, described) {
                    return Err("login operation ended in a way root cannot".into());
                }
                let rows = login
                    .selection
                    .failure(*kind, *detail)
                    .ok_or("invalid login failure kind")?;
                let attempt = Arc::clone(&login.attempt);
                let login_operation = !login.selection.storage();
                self.login = None;
                self.login_ended = login_operation;
                // `0e`'s own first row is the notice's.
                return if uncertain {
                    attempt.notice(Notice::Uncertain(if *kind == 0x0e {
                        rows.get(1..).unwrap_or_default()
                    } else {
                        rows
                    }))
                } else if rows.is_empty() {
                    Ok(())
                } else {
                    attempt.notice(Notice::Login(rows))
                };
            }
            _ => {}
        }
        let description = Request::decode(rest)?;
        login.admit(&description, *status)?;
        let presented = login.receipt.as_ref() == Some(&description);
        let open = !login.committed && !login.cancelled;
        match *status {
            0x03 => {}
            0x04 if login.receipt.is_none() && open => {
                if login.shown.is_none() {
                    match login.attempt.present(description.clone()) {
                        Ok(receipt) if receipt == description && login.attempt.active() => {
                            login.shown = Some(receipt);
                        }
                        Err(error) if error == crate::attention::PROMPT_TOO_TALL => {
                            return login.abandon_saying(wire, TOO_SMALL)
                        }
                        _ => return login.abandon(wire),
                    }
                }
                // A disclosure step's receipt waits for its approving Enter
                // (DESIGN.md, "Elevation consent"); any other step's
                // follows its presentation. Root polls again meanwhile.
                if login.shown.as_ref() == Some(&description)
                    && (!description.awaits_approval() || login.attempt.approved(&description))
                    && login.attempt.active()
                {
                    let mut bytes = vec![0x13];
                    bytes.extend_from_slice(&description.encode());
                    if wire.exchange(&bytes)? != [0x93] {
                        return Err("invalid presentation acknowledgement".into());
                    }
                    login.receipt = login.shown.take();
                }
            }
            // Root waits for the presented PIN step's PIN: the field opens,
            // and once the person submits it the PIN goes as `1c`.
            0x0c if presented && open && asks_pin(&description) && !login.pin_sent => {
                if login.field {
                    login.send_pin(wire, &description)?;
                } else {
                    login.open_field(wire, memory, &description)?;
                }
            }
            0x05 if login.last
                && presented
                && open
                && (login.pin_sent || !asks_pin(&description)) =>
            {
                // This CAS chooses between physical cancellation and consent.
                if login.attempt.commit() {
                    login.committed = true;
                    let mut bytes = vec![0x14];
                    bytes.extend_from_slice(&description.encode());
                    if wire.exchange(&bytes)? != [0x94] {
                        return Err("invalid commit acknowledgement".into());
                    }
                } else {
                    login.abandon(wire)?;
                }
            }
            0x06 if login.committed => {
                let attempt = Arc::clone(&login.attempt);
                let rows = login.selection.success(login.request.as_ref());
                let unlock = login.selection == LoginSelection::Unlock;
                let login_operation = !login.selection.storage();
                self.login = None;
                self.login_ended = login_operation;
                attempt.notice(Notice::Login(rows))?;
                // Only root's success for the unlock this client committed,
                // whose every step it admitted, leaves the lock surface.
                if unlock {
                    attempt.unlocked()?;
                }
            }
            _ => return Err("out-of-order login operation status".into()),
        }
        Ok(())
    }

    fn inspect(
        &mut self,
        wire: &mut impl Exchange,
        attempt: Arc<Attempt>,
        begin: bool,
    ) -> Result<(), String> {
        if let Some(shown) = self.restarting(&attempt) {
            return shown;
        }
        if wire.exchange(&[0x17])? != [0x97] {
            return Err("invalid store inspection start".into());
        }
        self.inspection = Some(Inspection { attempt, begin });
        Ok(())
    }

    fn begin(&mut self, wire: &mut impl Exchange, attempt: Arc<Attempt>) -> Result<(), String> {
        if !attempt.active() {
            return Ok(());
        }
        if let Some(shown) = self.restarting(&attempt) {
            return shown;
        }
        if attempt.selection == Selection::Suspend {
            if !attempt.commit() {
                return Ok(());
            }
            let locked = attempt
                .seat
                .as_ref()
                .ok_or("suspend needs the physical seat")?
                .prepare_suspend(&attempt, || {
                    if wire.exchange(&[3])? != [0x83] {
                        return Err("invalid suspend preparation heartbeat".into());
                    }
                    Ok(())
                })?;
            let Some(locked) = locked else {
                return attempt.notice(Notice::PowerFailed);
            };
            let resumed = suspend_exchange(wire, locked)?;
            if locked {
                return Ok(());
            }
            return attempt.notice(if resumed {
                Notice::Resumed
            } else {
                Notice::PowerFailed
            });
        }
        if matches!(attempt.selection, Selection::Power(_)) && !attempt.commit() {
            return Ok(());
        }
        let response = wire.exchange(&attempt.selection.request()?)?;
        if let Selection::Power(choice) = attempt.selection {
            return match response.as_slice() {
                [0xa1, 0] => attempt.notice(Notice::PowerAccepted(choice)),
                [0xa1, 1] => attempt.notice(Notice::Busy),
                [0xa1, 2] => attempt.notice(Notice::PowerFailed),
                _ => Err("invalid power response".into()),
            };
        }
        if attempt.selection == Selection::Write && response == [0x98, 0] {
            return attempt.notice(crate::attention::Notice::NoWrite);
        }
        if attempt.selection == Selection::Install && response == [0x99, 0] {
            return attempt.notice(crate::attention::Notice::NoInstall);
        }
        // Root refused the queued update before any description: nothing
        // is presented (td-authd/DESIGN.md, amendment 8, and "Elevation
        // operations" for the principal table's `deploy-publish` row).
        if attempt.selection == Selection::Install && response == [0x99, 1] {
            return attempt.notice(crate::attention::Notice::UpdateRefused);
        }
        if attempt.selection == Selection::Install && response == [0x99, 2] {
            return attempt.notice(crate::attention::Notice::ElevationRefused);
        }
        // Root refused the elevation before any description
        // (td-authd/DESIGN.md, "Elevation operations"): a busy slot, the
        // principal table, no two deployments to choose between, or no
        // queued hostname change.
        let refused = match (&attempt.selection, response.as_slice()) {
            (Selection::Elevation(Elevation::Rollback), [0x9d, 0]) => {
                Some(crate::attention::Notice::Busy)
            }
            (Selection::Elevation(Elevation::Rollback), [0x9d, 1])
            | (Selection::Elevation(Elevation::Hostname), [0x9e, 1]) => {
                Some(crate::attention::Notice::ElevationRefused)
            }
            (Selection::Elevation(Elevation::Rollback), [0x9d, 2]) => {
                Some(crate::attention::Notice::NoPrevious)
            }
            (Selection::Elevation(Elevation::Hostname), [0x9e, 0]) => {
                Some(crate::attention::Notice::NoHostname)
            }
            _ => None,
        };
        if let Some(notice) = refused {
            return attempt.notice(notice);
        }
        let Some((&0x92, bytes)) = response.split_first() else {
            return Err("invalid secret operation start response".into());
        };
        let request = Request::decode(bytes)?;
        let selected = match attempt.selection.operation() {
            Some(operation) => request.operation() == &operation,
            None => match attempt.selection {
                Selection::Write => matches!(
                    request.operation(),
                    Operation::Set {
                        requester: 1000,
                        ..
                    }
                ),
                Selection::Elevation(elevation) => Elevation::of(&request) == Some(elevation),
                // An update on an installed system; a whole disk on a live
                // boot.
                Selection::Install => matches!(
                    request.operation(),
                    Operation::Install {
                        requester: 1000,
                        ..
                    } | Operation::InstallDisk {
                        requester: 1000,
                        ..
                    }
                ),
                _ => false,
            },
        };
        if request.owner() != 1000 || !selected {
            return Err("root secret request changed the selected operation".into());
        }
        self.pending = Some(Pending {
            attempt,
            request,
            receipt: None,
            committed: false,
            cancelled: false,
            returning: None,
            released: false,
        });
        Ok(())
    }

    pub fn tick(&mut self, wire: &mut impl Exchange) -> Result<(), String> {
        if self.inspection.is_some() {
            return self.poll_inspection(wire);
        }
        if self.login.is_some() {
            let result = self.tick_login(wire);
            // A violation ends the generation: nothing typed outlives it.
            if result.is_err() {
                if let Some(login) = &self.login {
                    login.attempt.close_field();
                }
            }
            return result;
        }
        let Some(pending) = &mut self.pending else {
            return Ok(());
        };
        if !pending.attempt.active() && !pending.committed && !pending.cancelled {
            pending.cancel(wire)?;
        }
        let response = wire.exchange(&[0x11])?;
        let [0x91, status, bytes @ ..] = response.as_slice() else {
            return Err("invalid secret operation status".into());
        };
        let description = Request::decode(bytes)?;
        if description != pending.request {
            let following = pending.request.following_enrollment_step()?;
            if following.as_ref() != Some(&description)
                || pending.receipt.as_ref() != Some(&pending.request)
                || !matches!(*status, 3 | 4 | 7)
            {
                return Err("secret operation status changed its request".into());
            }
            pending.receipt = None;
            pending.request = description;
        }
        match status {
            // Committed, a device-bound installation waits for its key in
            // td-setup: say so, then close the screen by itself.
            3 if pending.committed
                && !pending.released
                && pending.attempt.selection == Selection::Install
                && returns_for_recovery_key(&pending.request) =>
            {
                match pending.returning {
                    None => {
                        pending.attempt.notice(Notice::Returning)?;
                        pending.returning = Some(Instant::now());
                    }
                    Some(shown) if shown.elapsed() >= RETURN_NOTICE => {
                        pending.released = true;
                        // A screen left open still closes with Escape.
                        if let Err(error) = pending.attempt.release() {
                            eprintln!("td-compositor: close attention after consent: {error}");
                        }
                    }
                    Some(_) => (),
                }
            }
            3 => (),
            4 if pending.receipt.is_none() && !pending.committed && !pending.cancelled => {
                match pending.attempt.present(pending.request.clone()) {
                    Ok(receipt) if receipt == pending.request && pending.attempt.active() => {
                        let mut bytes = vec![0x13];
                        bytes.extend_from_slice(&receipt.encode());
                        if wire.exchange(&bytes)? != [0x93] {
                            return Err("invalid presentation acknowledgement".into());
                        }
                        pending.receipt = Some(receipt);
                    }
                    _ => pending.cancel(wire)?,
                }
            }
            5 if pending.request.following_enrollment_step()?.is_none()
                && pending.receipt.as_ref() == Some(&pending.request)
                && !pending.committed
                && !pending.cancelled =>
            {
                if pending.attempt.selection.confirms()
                    && pending.attempt.active()
                    && !pending.attempt.confirmed.load(Ordering::SeqCst)
                {
                    return Ok(());
                }
                // This CAS chooses between physical cancellation and consent.
                // No runtime lock spans the subsequent bounded exchange.
                if pending.attempt.commit() {
                    pending.committed = true;
                    let mut bytes = vec![0x14];
                    bytes.extend_from_slice(&pending.request.encode());
                    if wire.exchange(&bytes)? != [0x94] {
                        return Err("invalid commit acknowledgement".into());
                    }
                } else {
                    pending.cancel(wire)?;
                }
            }
            6 if pending.committed => {
                pending.attempt.notice(
                    if matches!(pending.attempt.selection, Selection::Enroll(_)) {
                        crate::attention::Notice::Enrolled
                    } else if pending.attempt.selection == Selection::Elevation(Elevation::Rollback)
                    {
                        crate::attention::Notice::RolledBack
                    } else if pending.attempt.selection == Selection::Elevation(Elevation::Hostname)
                    {
                        if matches!(
                            pending.request.operation(),
                            Operation::EnrollAdminKey { .. }
                        ) {
                            crate::attention::Notice::AdminKeySaved
                        } else if matches!(
                            pending.request.operation(),
                            Operation::ManageTime { .. }
                        ) {
                            crate::attention::Notice::TimeSaved
                        } else {
                            crate::attention::Notice::HostnameSaved
                        }
                    } else if pending.attempt.selection == Selection::Install {
                        crate::attention::Notice::Installed
                    } else if pending.attempt.selection == Selection::Write {
                        crate::attention::Notice::Stored
                    } else {
                        crate::attention::Notice::Unlocked
                    },
                )?;
                self.pending = None;
            }
            7 => {
                let attempt = Arc::clone(&pending.attempt);
                self.pending = None;
                if matches!(attempt.selection, Selection::Enroll(_)) {
                    self.inspect(wire, attempt, false)?;
                } else {
                    attempt.notice(crate::attention::Notice::Failed)?;
                }
            }
            _ => return Err("out-of-order secret operation status".into()),
        }
        Ok(())
    }

    fn poll_inspection(&mut self, wire: &mut impl Exchange) -> Result<(), String> {
        let response = wire.exchange(&[0x11])?;
        if response == [0x91, 8] {
            return Ok(());
        }
        let state = match response.as_slice() {
            [0x91, 9, state @ 0..=3] => Some(*state),
            [0x91, 10] => None,
            _ => return Err("invalid store inspection result".into()),
        };
        let inspection = self.inspection.take().ok_or("missing store inspection")?;
        if inspection.begin && matches!(state, Some(0 | 1)) && inspection.attempt.active() {
            return self.begin(wire, inspection.attempt);
        }
        inspection.attempt.notice(match state {
            Some(2 | 3) => crate::attention::Notice::Enrolled,
            Some(0 | 1) => crate::attention::Notice::Unenrolled,
            _ => crate::attention::Notice::Unavailable,
        })
    }
}

/// Poll a single submitted operation; never resubmit after an uncertain reply.
fn suspend_exchange(wire: &mut impl Exchange, locked: bool) -> Result<bool, String> {
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(65))
        .ok_or("suspend deadline overflow")?;
    let started = wire.exchange(&[0x25, u8::from(locked)])?;
    match started.as_slice() {
        [0xa5, 0] => {}
        [0xa5, 1 | 2] => return Ok(false),
        _ => return Err("invalid suspend start response".into()),
    }
    let mut response = started;
    loop {
        match response.as_slice() {
            [0xa5, 3] => return Ok(true),
            [0xa5, 0] if Instant::now() < deadline => {}
            [0xa5, 1 | 2] => return Ok(false),
            _ => return Err("invalid or timed out suspend response".into()),
        }
        std::thread::sleep(Duration::from_millis(20));
        response = wire.exchange(&[0x26])?;
    }
}

impl Pending {
    fn cancel(&mut self, wire: &mut impl Exchange) -> Result<(), String> {
        cancel(wire, self.request.nonce())?;
        self.cancelled = true;
        Ok(())
    }
}

impl LoginPending {
    fn cancel(&mut self, wire: &mut impl Exchange) -> Result<(), String> {
        self.attempt.close_field();
        cancel(wire, &self.nonce)?;
        self.cancelled = true;
        Ok(())
    }

    /// Opens the presented PIN step's field, once this process's memory is
    /// checked as td-secret's worker checks its own: a PIN must reach
    /// neither swap nor a core dump. A refusal cancels and says why.
    fn open_field(
        &mut self,
        wire: &mut impl Exchange,
        memory: fn() -> Result<(), String>,
        step: &Request,
    ) -> Result<(), String> {
        if memory().is_err() {
            self.cancel(wire)?;
            return self.attempt.notice(Notice::Login(UNPROTECTED));
        }
        if self.attempt.open_field(step).is_err() {
            return self.abandon(wire);
        }
        self.field = true;
        Ok(())
    }

    /// Sends the submitted PIN as `1c` with `step`, its PIN step, from a
    /// buffer zeroed as soon as the write returns, before root's answer is
    /// awaited; nothing until the person submits one. A cancellation or
    /// the deadline before the final check drops it unsent, and the next
    /// poll cancels; one after that check is a `15` that follows the PIN,
    /// as an Escape just after the write would be.
    fn send_pin(&mut self, wire: &mut impl Exchange, step: &Request) -> Result<(), String> {
        let description = step.encode();
        let length = u8::try_from(description.len())
            .map_err(|_| "login description exceeds a PIN request".to_string())?;
        let start = description.len().saturating_add(2);
        let mut request = PinRequest {
            bytes: [0; PIN_REQUEST],
        };
        request
            .bytes
            .get_mut(..start)
            .and_then(|head| head.split_first_chunk_mut::<2>())
            .ok_or("invalid PIN request")
            .map(|(tag, rest)| {
                *tag = [0x1c, length];
                rest.copy_from_slice(&description);
            })?;
        let into = request
            .bytes
            .get_mut(start..)
            .ok_or("invalid PIN request")?;
        let Some(pin) = self.attempt.take_pin(into) else {
            return Ok(());
        };
        // A choose step's digit names no secret.
        if step.protector_step() == Some(LoginStep::Choose) {
            self.chosen = into
                .first()
                .and_then(|digit| digit.checked_sub(b'0'))
                .filter(|_| pin == 1);
        }
        #[cfg(test)]
        tests::pin_taken();
        let sent = match start
            .checked_add(pin)
            .and_then(|end| request.bytes.get(..end))
        {
            // Checked again immediately before the write.
            Some(bytes) if self.attempt.sending() => Some(wire.send(bytes)),
            Some(_) => None,
            None => Some(Err("invalid PIN request".to_string())),
        };
        request.clear();
        self.attempt.finish_sending();
        let Some(sent) = sent else {
            return Ok(());
        };
        sent?;
        self.pin_sent = true;
        let answer = wire.receive()?;
        match answer.as_slice() {
            [0x9c, 0] => {
                if self.attempt.show_touch(step).is_err() {
                    self.abandon(wire)?;
                }
                Ok(())
            }
            // Root's operation had ended, or its deadline passed: the PIN
            // was dropped, and the end follows.
            [0x9c, 1] => Ok(()),
            _ => Err("invalid login PIN acknowledgement".into()),
        }
    }

    /// This client's own cancellation, which withdraws any prompt and says
    /// why: the attempt's deadline passed, or a presentation failed. After
    /// Escape the screen is closing and `notice` shows nothing.
    fn abandon(&mut self, wire: &mut impl Exchange) -> Result<(), String> {
        self.abandon_saying(wire, &["THE OPERATION FAILED"])
    }

    /// As `abandon`, saying `why` unless the attempt's time ran out.
    fn abandon_saying(
        &mut self,
        wire: &mut impl Exchange,
        why: &'static [&'static str],
    ) -> Result<(), String> {
        self.cancel(wire)?;
        self.attempt
            .notice(Notice::Login(if self.attempt.expired() {
                &["TIMED OUT"]
            } else {
                why
            }))
    }

    /// Whether root can end this operation with `kind` and `detail` here
    /// (td-authd/src/login.rs): any typed worker failure, `0e` only once a
    /// write's commit was acknowledged, cancellation only after this
    /// client's, and each refusal of a selection only before any
    /// description, since root refuses at the worker's baseline.
    fn ends(&self, uncertain: bool, kind: u8, detail: u8, described: bool) -> bool {
        if self.selection == LoginSelection::SelectorUpdate {
            // td-compositor/DESIGN.md, "Storage operations": `20` to `22`
            // from the baseline, before any description; `28` and `0e`
            // only after the acknowledged commit.
            return match (kind, detail) {
                (0x20..=0x22, 0) => !described,
                (0x23 | 0x24 | 0x08 | 0x0f | 0x10, 0) => true,
                (0x28, 0) => self.committed && !uncertain,
                (0x0e, 0) => uncertain,
                (0x80, 0) => self.cancelled,
                _ => false,
            };
        }
        // An upgrade's: root's `87` and the baseline's refusals before any
        // description; its reads' kinds, `28` among them, while the review
        // awaits its receipt, the TPM's for the TPM shape alone; a new
        // key's from its connect step on; and `28` again, with `0e`, after
        // the acknowledged commit.
        if let LoginSelection::Upgrade { shape, .. } = self.selection {
            let step = self.request.as_ref().and_then(Request::upgrade_step);
            let review = step == Some(UpgradeStep::Review) && self.receipt.is_none();
            let key = matches!(step, Some(UpgradeStep::Key { .. })) && !self.committed;
            return match (kind, detail) {
                (0x87 | 0x22, 0) => !described,
                (0x20 | 0x21, 0) => !described || review,
                (0x23 | 0x24, 0) => review,
                (0x27, 1..=3) => review && shape == UpgradeShape::Tpm,
                (0x01, _) | (0x06, 1..=6) | (0x02 | 0x03 | 0x05 | 0x07 | 0x11, 0) => key,
                (0x28, 0) => !uncertain && (review || self.committed),
                (0x0e, 0) => uncertain,
                (0x08 | 0x0f | 0x10, 0) => true,
                (0x80, 0) => self.cancelled,
                _ => false,
            };
        }
        // A cancellation's: `20` before the commit, `21` and `28` before
        // its receipt, and `28` and `0e` after the acknowledged commit.
        if self.selection == LoginSelection::UpgradeCancel {
            let unreceived = self.receipt.is_none() && !self.committed;
            return match (kind, detail) {
                (0x20, 0) => !self.committed,
                (0x21, 0) => unreceived,
                (0x28, 0) => !uncertain && ((described && unreceived) || self.committed),
                (0x0e, 0) => uncertain,
                (0x08 | 0x0f | 0x10, 0) => true,
                (0x80, 0) => self.cancelled,
                _ => false,
            };
        }
        // Protector management's (DESIGN.md, "Protector management"): the
        // baseline's `21`, with no detail or the selection's own refusal
        // (consent's `Protected::refusal`), before any description, `20`
        // before the commit, a disk proof's kinds and a removal's chosen
        // key's `04` from the proof's first step, never at the choose
        // step, `11` while an addition's new key's step is current, and
        // `0e` after the acknowledged commit.
        if let LoginSelection::Protector(managed) = self.selection {
            let step = self.request.as_ref().and_then(Request::protector_step);
            let open = described && !self.committed && step != Some(LoginStep::Choose);
            let new_key = matches!(
                step,
                Some(
                    LoginStep::Connect
                        | LoginStep::Create { .. }
                        | LoginStep::Prove { .. }
                        | LoginStep::Repeat { .. }
                        | LoginStep::Probe { .. }
                )
            );
            let proof = match managed {
                Managed::Add { proof, .. }
                | Managed::Remove { proof }
                | Managed::Request { proof, .. } => proof,
                Managed::RecoveryRemove => Proof::DiskKey,
            };
            let refusal = match managed {
                Managed::Add { .. } => 1,
                Managed::Remove { .. } | Managed::RecoveryRemove => 2,
                Managed::Request { .. } => 3,
            };
            return match (kind, detail) {
                (0x21, detail) if detail == 0 || detail == refusal => !described,
                (0x20, 0) => !self.committed,
                (0x25, 0) => open && proof == Proof::RecoveryKey,
                (0x26, 0) => open,
                (0x28, 0) => described && !uncertain,
                (0x04, 1) => open && matches!(managed, Managed::Remove { .. }),
                (0x11, 0) => open && new_key,
                (0x01, _) | (0x06, 1..=6) | (0x02..=0x05 | 0x07, 0) => open,
                (0x0e, 0) => uncertain,
                (0x08 | 0x0f | 0x10, 0) => true,
                (0x80, 0) => self.cancelled,
                _ => false,
            };
        }
        let (selection, proof) = self.selection.authorized();
        // A disk proof's own (amendment 10): root's `86` and the volume's
        // `21` before any description; the storage worker's `20` from its
        // baseline or its re-read, `25` and `26` from its keyslot test, and
        // `28`, which precedes any write, before the commit. With a proof
        // a first enrollment is never `84`.
        if let Some(proof) = proof {
            match (kind, detail) {
                (0x86 | 0x21, 0) => return !described,
                (0x20, 0) => return true,
                (0x25, 0) => return described && proof == Proof::RecoveryKey,
                (0x26, 0) => return described,
                (0x28, 0) => return described && !uncertain && !self.committed,
                (0x84, 0) => return false,
                _ => {}
            }
        }
        match (kind, detail) {
            (0x01, _) | (0x06, 1..=6) | (0x02..=0x05 | 0x07..=0x0d | 0x0f..=0x12, 0) => true,
            (0x0e, 1..=5) => uncertain,
            (0x80, 0) => self.cancelled,
            (0x81, 0) => !described && *selection == LoginSelection::Add,
            (0x82, 0) => !described && matches!(selection, LoginSelection::Enroll(_)),
            (0x83, 0) => !described && matches!(selection, LoginSelection::Remove(..)),
            // A protected machine's refusals (td-login/TOKEN-LOGIN.md,
            // "Enrollment, addition and removal").
            (0x84, 0) => !described && matches!(selection, LoginSelection::Enroll(_)),
            (0x85, 0) => !described && matches!(selection, LoginSelection::Remove(..)),
            _ => false,
        }
    }

    /// Admits root's description of the operation. The first must be the
    /// selected operation's first step under root's `9b` nonce. Each later
    /// change must follow a receipt of its predecessor and be that step's
    /// one legal successor, and in an addition the new key's steps never
    /// name the authorizing key, which consent leaves to this reader.
    fn admit(&mut self, description: &Request, status: u8) -> Result<(), String> {
        let Some(current) = &self.request else {
            // A removal chosen from a stale list, whose count left differs
            // from root's baseline, is refused here as a changed operation.
            if !matches!(status, 0x03 | 0x04 | 0x0d | 0x0e)
                || description.nonce() != &self.nonce
                || !self.selection.selects(description)
            {
                return Err("root login request changed the selected operation".into());
            }
            if let LoginSelection::Upgrade { .. } = self.selection {
                // The review, last only with no new key.
                self.last = description.upgrade_start()? == Admitted::Last;
            } else if let LoginSelection::Protector(managed) = self.selection {
                // Only a request by the recovery key has one step.
                self.last = matches!(
                    managed,
                    Managed::Request {
                        proof: Proof::RecoveryKey,
                        ..
                    }
                );
            } else if self.selection.storage() {
                // One step: its presentation is also its last.
                self.last = true;
            } else {
                description.login_start()?;
                // A removal's recovery key is its proof's one step and the
                // operation's last; every other first step has successors.
                self.last = matches!(
                    &self.selection,
                    LoginSelection::Proven(Proof::RecoveryKey, inner)
                        if matches!(inner.as_ref(), LoginSelection::Remove(..))
                );
            }
            self.request = Some(description.clone());
            return Ok(());
        };
        if current == description {
            return Ok(());
        }
        if matches!(
            self.selection,
            LoginSelection::SelectorUpdate | LoginSelection::UpgradeCancel
        ) {
            return Err("a one-step storage operation has one description".into());
        }
        // `03` too: root that read the next invitation, then stopped at its
        // deadline or a cancellation, describes that step while it reaps.
        // Presenting it still takes `04`, and committing a receipt.
        if !matches!(status, 0x03 | 0x04 | 0x0d | 0x0e) || self.receipt.as_ref() != Some(current) {
            return Err("login status changed a step that was not presented".into());
        }
        // Root moves past a PIN step only once it has the PIN.
        if asks_pin(current) && !self.pin_sent {
            return Err("login status passed a PIN step before its PIN".into());
        }
        let admitted = match self.selection {
            LoginSelection::Upgrade { .. } => current.upgrade_successor(description)?,
            LoginSelection::Protector(_) => {
                // After the choose step, the position this client sent.
                if current.protector_step() == Some(LoginStep::Choose)
                    && description.removal().map(|removal| removal.chosen) != self.chosen
                {
                    return Err("root's removal changed its chosen key".into());
                }
                current.protector_successor(description)?
            }
            _ => current.login_successor(description)?,
        };
        let protector = matches!(self.selection, LoginSelection::Protector(_));
        match description.login_step().or(description.protector_step()) {
            Some(LoginStep::Authorize { key, .. }) => self.authorizer = Some(key),
            Some(LoginStep::DiskAuthorize { key, .. }) if protector => self.authorizer = Some(key),
            Some(
                LoginStep::Prove { key, .. }
                | LoginStep::Repeat { key, .. }
                | LoginStep::Probe { key },
            ) if self.authorizer == Some(key) => {
                return Err("a new login key cannot be the key that authorized it".into());
            }
            _ => {}
        }
        self.last = admitted == Admitted::Last;
        self.request = Some(description.clone());
        self.receipt = None;
        self.shown = None;
        self.field = false;
        self.pin_sent = false;
        Ok(())
    }
}

fn cancel(wire: &mut impl Exchange, nonce: &[u8; 32]) -> Result<(), String> {
    let mut bytes = vec![0x15];
    bytes.extend_from_slice(nonce);
    if !matches!(wire.exchange(&bytes)?.as_slice(), [0x95, 0 | 2]) {
        return Err("invalid cancellation acknowledgement".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
    use super::*;

    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU64;

    /// The recovery-key profile (DESIGN.md, "Storage operations"): only a
    /// key that names a digit types, as that digit whatever its Shift
    /// level; a 49th is refused; Enter submits only at exactly 48; and the
    /// field's take zeroes it as a PIN's. The PIN profile types the same
    /// keys' bytes.
    #[test]
    fn the_recovery_key_profile_takes_exactly_48_digits() {
        let digit = |n: usize| FieldKey::Digit {
            byte: b"!@#$%^&*()"[n % 10],
            digit: b'0' + (n % 10) as u8,
        };
        let mut field = PinField::default();
        field.open_for(Profile::Recovery);
        for ignored in *b"1- a" {
            assert_eq!(field.key(FieldKey::Byte(ignored)), None);
        }
        for n in 0..47 {
            assert_eq!(field.key(digit(n)), Some(n + 1));
        }
        assert_eq!(field.key(FieldKey::Submit), None);
        assert!(!field.submitted());
        assert_eq!(field.key(digit(47)), Some(48));
        assert_eq!(field.key(digit(48)), None);
        assert_eq!(field.key(FieldKey::Erase), Some(47));
        assert_eq!(field.key(digit(47)), Some(48));
        let expected: Vec<u8> = (0..48).map(|n| b'0' + (n % 10) as u8).collect();
        assert_eq!(field.typed(), expected);
        field.key(FieldKey::Submit);
        assert!(field.submitted());
        let mut into = [0; PIN_CAPACITY];
        assert_eq!(field.take(&mut into), Some(48));
        assert_eq!(&into[..48], expected);
        assert!(field.raw().iter().all(|byte| *byte == 0));
        assert!(!field.is_open());
        // Closed, the next field is a PIN's: the same key types its byte,
        // and four bytes submit.
        field.open_for(Profile::Pin);
        for n in 0..4 {
            assert_eq!(field.key(digit(n)), Some(n + 1));
        }
        assert_eq!(field.typed(), b"!@#$");
        field.key(FieldKey::Submit);
        assert!(field.submitted());
    }

    thread_local! {
        /// Each PIN buffer and `1c` request as it was dropped, after its
        /// zeroing: whether every byte was zero.
        static DROPPED: RefCell<Vec<bool>> = const { RefCell::new(Vec::new()) };
        /// The scripted wire's writes and reads, and each `1c` request's
        /// zeroing between them.
        static WIRE: RefCell<Vec<WireEvent>> = const { RefCell::new(Vec::new()) };
        /// Run once a PIN is taken, before the check that precedes its
        /// write: a person pressing Escape in between.
        static TAKEN: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum WireEvent {
        Sent(Vec<u8>),
        /// A `1c` request zeroed: whether every byte was zero.
        Cleared(bool),
        Received,
    }

    pub(super) fn request_cleared(bytes: &[u8]) {
        let zero = bytes.iter().all(|byte| *byte == 0);
        WIRE.with(|wire| wire.borrow_mut().push(WireEvent::Cleared(zero)));
    }

    pub(super) fn pin_taken() {
        if let Some(hook) = TAKEN.with(|taken| taken.borrow_mut().take()) {
            hook();
        }
    }

    fn wire_events() -> Vec<WireEvent> {
        WIRE.with(|wire| std::mem::take(&mut *wire.borrow_mut()))
    }

    /// Each `1c` written: what followed it until the next write.
    fn after_pins(events: &[WireEvent]) -> Vec<Vec<WireEvent>> {
        events
            .iter()
            .enumerate()
            .filter(|(_, event)| matches!(event, WireEvent::Sent(bytes) if bytes.first() == Some(&0x1c)))
            .map(|(at, _)| {
                events[at + 1..]
                    .iter()
                    .take_while(|event| !matches!(event, WireEvent::Sent(_)))
                    .cloned()
                    .collect()
            })
            .collect()
    }

    /// The drop hook: what a dropped PIN holder left behind.
    pub(super) fn dropped(bytes: &[u8]) {
        DROPPED.with(|dropped| {
            dropped
                .borrow_mut()
                .push(bytes.iter().all(|byte| *byte == 0))
        });
    }

    fn drops() -> Vec<bool> {
        DROPPED.with(|dropped| std::mem::take(&mut *dropped.borrow_mut()))
    }

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    struct Screen {
        path: PathBuf,
        attempt: Arc<Attempt>,
    }
    impl Screen {
        fn new() -> Self {
            Self::sized(800, 600)
        }
        fn sized(width: usize, height: usize) -> Self {
            let path = std::env::temp_dir().join(format!(
                "td-secret-screen-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            let mut runtime = Runtime::new(
                crate::framebuffer::Framebuffer::test_file(&path, width, height, width * 4)
                    .unwrap(),
            );
            runtime.enable_attention(true);
            runtime
                .attention(&crate::input::test_origin(), true)
                .unwrap();
            Self {
                path,
                attempt: Attempt::new(
                    crate::input::test_origin(),
                    Arc::new(Mutex::new(runtime)),
                    Role::Primary,
                ),
            }
        }
    }
    impl Drop for Screen {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    #[test]
    fn notebook_attention_requires_approval_then_a_bound_protected_pin() {
        for changed in [false, true] {
            let temp = crate::temp_dir::TempDir::new("td-notebook-attention").unwrap();
            let path = temp.path().join("frame");
            let mut runtime = Runtime::new(
                crate::framebuffer::Framebuffer::test_file(&path, 800, 600, 3200).unwrap(),
            );
            runtime.enable_attention(true);
            runtime
                .attention(&crate::input::test_origin(), true)
                .unwrap();
            let screen = Screen {
                path,
                attempt: Attempt::new(
                    crate::input::test_origin(),
                    Arc::new(Mutex::new(runtime)),
                    Selection::Login(LoginSelection::Notebook),
                ),
            };
            let before = Request::new(
                NONCE,
                1000,
                Operation::Notebook(crate::authority::consent::Notebook {
                    method: 0x31,
                    action: 3,
                    purpose: 3,
                    role: 1,
                    step: 0,
                    operation: [2; 32],
                    token: [3; 32],
                }),
            )
            .unwrap();
            let Operation::Notebook(mut binding) = before.operation().clone() else {
                panic!();
            };
            binding.step = 1;
            if changed {
                binding.operation[0] ^= 1;
            }
            let pin = Request::new(NONCE, 1000, Operation::Notebook(binding)).unwrap();
            let mut replies = VecDeque::from([
                [&[0xa9, 1][..], &NONCE].concat(),
                vec![0x91, 0x0b],
                status(4, &before),
                status(4, &before),
                vec![0x93],
                status(4, &pin),
            ]);
            if !changed {
                replies.extend([
                    vec![0x93],
                    status(0x0c, &pin),
                    status(0x0c, &pin),
                    vec![0x9c, 0],
                    status(6, &pin),
                ]);
            }
            let mut wire = Wire {
                replies,
                calls: Vec::new(),
                typist: Some(Arc::clone(&screen.attempt)),
            };
            let mut client = Client::trusting_memory();
            client
                .start(&mut wire, Arc::clone(&screen.attempt))
                .unwrap();
            let mut refused = false;
            for _ in 0..20 {
                if client.tick(&mut wire).is_err() {
                    refused = true;
                    break;
                }
                if client.login.is_none() {
                    break;
                }
            }
            assert_eq!(refused, changed);
            assert_eq!(
                wire.calls
                    .iter()
                    .filter(|c| c.first() == Some(&0x1c))
                    .count(),
                usize::from(!changed)
            );
            if !changed {
                assert!(wire.calls.contains(&pin_request(&pin, PIN)));
            }
        }
    }

    struct Wire {
        replies: VecDeque<Vec<u8>>,
        calls: Vec<Vec<u8>>,
        /// A person at the attention screen: once a PIN field is open and
        /// empty, they type `PIN` into it and press Enter.
        typist: Option<Arc<Attempt>>,
    }
    impl Exchange for Wire {
        fn send(&mut self, bytes: &[u8]) -> Result<(), String> {
            self.calls.push(bytes.to_vec());
            WIRE.with(|wire| wire.borrow_mut().push(WireEvent::Sent(bytes.to_vec())));
            Ok(())
        }
        fn receive(&mut self) -> Result<Vec<u8>, String> {
            WIRE.with(|wire| wire.borrow_mut().push(WireEvent::Received));
            let reply = self.replies.pop_front().ok_or("uncertain delivery")?;
            if let Some(attempt) = &self.typist {
                let (empty, profile) = {
                    let field = attempt.field.lock().unwrap();
                    (
                        field.is_open() && field.typed().is_empty() && !field.submitted(),
                        field.profile,
                    )
                };
                if empty {
                    type_pin(attempt, typed_for(profile));
                }
                // A login disclosure on glass: they type its key.
                if matches!(attempt.selection, Selection::Login(_)) {
                    approve_presented(attempt);
                }
            }
            Ok(reply)
        }
    }
    /// The PIN the tests' person types.
    const PIN: &[u8] = b"1234";
    /// The recovery key they type at a recovery-key step: 48 digits.
    const RECOVERY: &[u8] = b"012345678901234567890123456789012345678901234567";
    /// A press's evdev time: just after its field was on glass, or before
    /// any field was.
    fn pressed(attempt: &Attempt) -> u128 {
        attempt
            .field
            .lock()
            .unwrap()
            .shown
            .map_or(0, |shown| shown + 1)
    }
    /// An approving Enter on the presented prompt once it has settled, as
    /// the evdev adapter offers it; nothing once decided or for a prompt
    /// that does not await approval.
    fn approve_presented(attempt: &Attempt) {
        let shown = attempt
            .presentation
            .lock()
            .unwrap()
            .as_ref()
            .filter(|presented| !presented.decided && presented.request.awaits_approval())
            .map(|presented| presented.after);
        if let Some(after) = shown {
            assert!(!attempt
                .approve(
                    &crate::input::test_origin(),
                    ApprovalPress::Enter,
                    after + APPROVAL_SETTLE
                )
                .unwrap());
        }
    }
    /// `pin`'s keys, then Enter, as the evdev adapter offers them.
    /// What the test's person types in each profile: a choose step's
    /// first position.
    fn typed_for(profile: Profile) -> &'static [u8] {
        match profile {
            Profile::Pin => PIN,
            Profile::Recovery => RECOVERY,
            // The highest removable position, so a client that sent any
            // other would be refused at the next step.
            Profile::Choose(mask) => {
                let position = 8 - mask.leading_zeros() as usize;
                &b"12345678"[position.saturating_sub(1)..position]
            }
        }
    }

    fn type_pin(attempt: &Attempt, pin: &[u8]) {
        for byte in pin {
            // A digit's key names its digit, as the number row's does.
            let key = if byte.is_ascii_digit() {
                FieldKey::Digit {
                    byte: *byte,
                    digit: *byte,
                }
            } else {
                FieldKey::Byte(*byte)
            };
            attempt
                .field_key(&crate::input::test_origin(), key, pressed(attempt))
                .unwrap();
        }
        attempt
            .field_key(
                &crate::input::test_origin(),
                FieldKey::Submit,
                pressed(attempt),
            )
            .unwrap();
    }
    /// A client whose memory check passes, whatever this host's swap.
    fn login_client() -> Client {
        Client::trusting_memory()
    }
    fn request() -> Request {
        Request::new(
            [42; 32],
            1000,
            Operation::Unlock {
                role: Role::Primary,
            },
        )
        .unwrap()
    }
    fn tagged(tag: &[u8]) -> Vec<u8> {
        [tag, request().encode().as_slice()].concat()
    }
    fn wire(replies: Vec<Vec<u8>>) -> Wire {
        Wire {
            replies: replies.into(),
            calls: Vec::new(),
            typist: None,
        }
    }
    /// `replies`, with a person at `screen` who types the PIN.
    fn typing(replies: Vec<Vec<u8>>, screen: &Screen) -> Wire {
        Wire {
            typist: Some(Arc::clone(&screen.attempt)),
            ..wire(replies)
        }
    }

    #[test]
    fn an_update_root_refuses_shows_its_text_and_presents_nothing() {
        for (reply, notice) in [
            (vec![0x99, 1], Notice::UpdateRefused),
            (vec![0x99, 0], Notice::NoInstall),
            (vec![0x99, 2], Notice::ElevationRefused),
        ] {
            let mut screen = Screen::new();
            Arc::get_mut(&mut screen.attempt).unwrap().selection = Selection::Install;
            let mut client = Client::default();
            let mut wire = wire(vec![reply]);
            client
                .start(&mut wire, Arc::clone(&screen.attempt))
                .unwrap();
            assert_eq!(wire.calls, [vec![0x19]]);
            assert!(client.pending.is_none());
            let runtime = screen.attempt.runtime.lock().unwrap();
            assert_eq!(runtime.attention_shown(), Some(notice));
            assert!(!runtime.attention_request_visible(&request()));
        }
        // No refusal is a write's or another selection's answer, and no
        // other byte is one.
        for selection in [Selection::Write, Selection::Unlock(Role::Primary)] {
            for reply in [vec![0x99, 1], vec![0x99, 2]] {
                let mut screen = Screen::new();
                Arc::get_mut(&mut screen.attempt).unwrap().selection = selection.clone();
                assert!(Client::default()
                    .start(&mut wire(vec![reply]), Arc::clone(&screen.attempt))
                    .is_err());
            }
        }
        for reply in [vec![0x99, 3], vec![0x99], vec![0x99, 2, 0]] {
            let mut screen = Screen::new();
            Arc::get_mut(&mut screen.attempt).unwrap().selection = Selection::Install;
            assert!(Client::default()
                .start(&mut wire(vec![reply]), Arc::clone(&screen.attempt))
                .is_err());
        }
    }

    #[test]
    fn install_also_admits_a_whole_disk_installation_for_the_session_owner() {
        let disk = |owner: u32, preserve: bool| {
            Request::new(
                [42; 32],
                owner,
                Operation::InstallDisk {
                    preserve,
                    requester: owner,
                    disk: "vda".into(),
                    capacity: 8 << 30,
                    model: Some(crate::authority::consent::Label::model(b"QEMU HARDDISK")),
                    serial: None,
                    hostname: "td".into(),
                    username: "alice".into(),
                    deployment: [0xab; 8],
                    storage: crate::authority::consent::Storage::DeviceBound,
                },
            )
            .unwrap()
        };
        for preserve in [false, true] {
            let mut screen = Screen::new();
            Arc::get_mut(&mut screen.attempt).unwrap().selection = Selection::Install;
            let mut client = Client::default();
            let reply = [&[0x92][..], disk(1000, preserve).encode().as_slice()].concat();
            client
                .start(&mut wire(vec![reply]), Arc::clone(&screen.attempt))
                .unwrap();
            assert_eq!(
                client.pending.as_ref().unwrap().request,
                disk(1000, preserve)
            );
            let mut screen = Screen::new();
            Arc::get_mut(&mut screen.attempt).unwrap().selection = Selection::Install;
            let reply = [&[0x92][..], disk(1001, preserve).encode().as_slice()].concat();
            assert!(Client::default()
                .start(&mut wire(vec![reply]), Arc::clone(&screen.attempt))
                .is_err());
        }
    }

    /// Root's description of a queued update for the owner.
    fn update() -> Request {
        Request::new(
            [42; 32],
            1000,
            Operation::Install {
                deployment: "ab".repeat(32),
                requester: 1000,
            },
        )
        .unwrap()
    }

    /// An update (`deploy-publish`) commits once, only after an approving
    /// Enter on the presented prompt, and the commit is root's exact
    /// description. The whole-disk installation's Enter, fresh or not,
    /// confirms nothing.
    #[test]
    fn an_update_commits_only_after_an_approving_enter() {
        let mut screen = Screen::new();
        Arc::get_mut(&mut screen.attempt).unwrap().selection = Selection::Install;
        let described = update();
        let invitation = status(5, &described);
        let (mut client, mut wire, shown) = presented_elevation(
            &screen,
            &described,
            vec![
                invitation.clone(),
                invitation,
                vec![0x94],
                status(6, &described),
            ],
        );
        assert_eq!(wire.calls[0], [0x19]);
        // Invited to commit, the client waits: the disk installation's
        // Enter after presentation, or at any later time, confirms nothing.
        for at in [shown + APPROVAL_SETTLE, u128::MAX] {
            screen
                .attempt
                .confirm_install(&crate::input::test_origin(), at)
                .unwrap();
        }
        assert!(!confirmed(&screen) && screen.attempt.active());
        client.tick(&mut wire).unwrap();
        assert_eq!(sent(&wire, 0x14), 0);
        assert!(!press(
            &screen,
            ApprovalPress::Enter,
            shown + APPROVAL_SETTLE
        ));
        assert!(confirmed(&screen));
        client.tick(&mut wire).unwrap();
        let commits: Vec<_> = wire
            .calls
            .iter()
            .filter(|call| call.first() == Some(&0x14))
            .collect();
        assert_eq!(commits, [&description(&[0x14], &described)]);
        client.tick(&mut wire).unwrap();
        assert!(client.pending.is_none());
        assert_eq!(
            screen.attempt.runtime.lock().unwrap().attention_shown(),
            Some(Notice::Installed)
        );
        assert_eq!(sent(&wire, 0x14), 1);
    }

    /// An update's first counted press other than a settled Enter, another
    /// key or an Enter inside the settle, ends it unapproved as an
    /// elevation's does; a press stamped before the prompt was on glass
    /// counts for nothing.
    #[test]
    fn an_updates_first_press_decides_and_an_early_one_counts_for_nothing() {
        for (first, at) in [
            (ApprovalPress::Other, 1),
            (ApprovalPress::Other, APPROVAL_SETTLE),
            (ApprovalPress::Enter, 1),
            (ApprovalPress::Enter, APPROVAL_SETTLE - 1),
        ] {
            let mut screen = Screen::new();
            Arc::get_mut(&mut screen.attempt).unwrap().selection = Selection::Install;
            let described = update();
            let (mut client, mut wire, shown) = presented_elevation(
                &screen,
                &described,
                vec![vec![0x95, 0], status(7, &described)],
            );
            for (early, at) in [
                (ApprovalPress::Enter, shown),
                (ApprovalPress::Other, shown - 1),
            ] {
                assert!(!press(&screen, early, at));
            }
            assert!(screen.attempt.active());
            assert!(press(&screen, first, shown + at));
            assert!(!screen.attempt.active() && !confirmed(&screen));
            assert!(!press(
                &screen,
                ApprovalPress::Enter,
                shown + APPROVAL_SETTLE + 1
            ));
            assert!(!confirmed(&screen));
            client.tick(&mut wire).unwrap();
            assert!(client.pending.is_none());
            assert_eq!(sent(&wire, 0x15), 1);
            assert_eq!(sent(&wire, 0x14), 0);
        }
    }

    /// Keyboards are read apart: a key another keyboard stamped before the
    /// approving Enter, delivered after it but before the commit, withdraws
    /// the approval and ends the request; one stamped after it, or an Enter,
    /// changes nothing.
    #[test]
    fn an_earlier_key_read_after_an_approving_enter_withdraws_it() {
        let approving = |screen: &Screen| -> (Client, Wire, u128) {
            let described = update();
            let (client, wire, shown) = presented_elevation(
                screen,
                &described,
                vec![vec![0x95, 0], status(7, &described)],
            );
            let enter = shown + APPROVAL_SETTLE + 10;
            assert!(!press(screen, ApprovalPress::Enter, enter));
            assert!(confirmed(screen) && screen.attempt.active());
            (client, wire, enter)
        };
        let mut screen = Screen::new();
        Arc::get_mut(&mut screen.attempt).unwrap().selection = Selection::Install;
        let (mut client, mut wire, enter) = approving(&screen);
        assert!(press(&screen, ApprovalPress::Other, enter - 1));
        assert!(!screen.attempt.active() && !confirmed(&screen));
        assert!(!press(&screen, ApprovalPress::Other, enter - 2));
        client.tick(&mut wire).unwrap();
        assert!(client.pending.is_none());
        assert_eq!(sent(&wire, 0x15), 1);
        assert_eq!(sent(&wire, 0x14), 0);
        for late in [(ApprovalPress::Other, 1), (ApprovalPress::Enter, 0)] {
            let mut screen = Screen::new();
            Arc::get_mut(&mut screen.attempt).unwrap().selection = Selection::Install;
            let (_, _, enter) = approving(&screen);
            assert!(!press(&screen, late.0, enter + late.1));
            assert!(!press(&screen, ApprovalPress::Enter, enter - 1));
            assert!(confirmed(&screen) && screen.attempt.active());
        }
        // Once committed, an earlier key read too late changes nothing.
        let mut screen = Screen::new();
        Arc::get_mut(&mut screen.attempt).unwrap().selection = Selection::Install;
        let (_, _, enter) = approving(&screen);
        assert!(screen.attempt.commit());
        assert!(!press(&screen, ApprovalPress::Other, enter - 1));
        assert!(confirmed(&screen));
    }

    /// A live boot's whole-disk installation keeps its fresh Enter after
    /// complete presentation, and no approval press confirms or ends it.
    #[test]
    fn a_disk_installation_waits_for_fresh_enter_and_takes_no_approval_press() {
        for preserve in [false, true] {
            let mut screen = Screen::new();
            Arc::get_mut(&mut screen.attempt).unwrap().selection = Selection::Install;
            let request =
                disk_preserving(crate::authority::consent::Storage::Unencrypted, preserve);
            let tagged = |tag: &[u8]| [tag, request.encode().as_slice()].concat();
            let mut wire = wire(vec![
                tagged(&[0x92]),
                tagged(&[0x91, 4]),
                vec![0x93],
                tagged(&[0x91, 5]),
                tagged(&[0x91, 5]),
                vec![0x94],
                tagged(&[0x91, 6]),
            ]);
            let mut client = Client::default();
            client
                .start(&mut wire, Arc::clone(&screen.attempt))
                .unwrap();
            screen
                .attempt
                .confirm_install(&crate::input::test_origin(), u128::MAX)
                .unwrap();
            assert!(!screen.attempt.confirmed.load(Ordering::SeqCst));
            client.tick(&mut wire).unwrap();
            let completed = screen
                .attempt
                .presentation
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .after;
            for approval in [ApprovalPress::Enter, ApprovalPress::Other] {
                assert!(!press(&screen, approval, completed + APPROVAL_SETTLE));
            }
            assert!(!confirmed(&screen) && screen.attempt.active());
            screen
                .attempt
                .confirm_install(&crate::input::test_origin(), completed)
                .unwrap();
            client.tick(&mut wire).unwrap();
            assert!(!wire.calls.iter().any(|call| call.first() == Some(&0x14)));
            screen
                .attempt
                .confirm_install(&crate::input::test_origin(), completed + 1)
                .unwrap();
            client.tick(&mut wire).unwrap();
            screen
                .attempt
                .confirm_install(&crate::input::test_origin(), completed + 2)
                .unwrap();
            client.tick(&mut wire).unwrap();
            assert!(client.pending.is_none());
            assert_eq!(
                wire.calls
                    .iter()
                    .filter(|call| call.first() == Some(&0x14))
                    .count(),
                1
            );
        }
    }

    fn disk(storage: crate::authority::consent::Storage) -> Request {
        disk_preserving(storage, false)
    }

    fn disk_preserving(storage: crate::authority::consent::Storage, preserve: bool) -> Request {
        Request::new(
            [42; 32],
            1000,
            Operation::InstallDisk {
                preserve,
                requester: 1000,
                disk: "vda".into(),
                capacity: 8 << 30,
                model: None,
                serial: None,
                hostname: "td".into(),
                username: "alice".into(),
                deployment: [0xab; 8],
                storage,
            },
        )
        .unwrap()
    }

    /// A committed device-bound disk installation shows the returning
    /// notice on the first status after commit and, once it has stood
    /// `RETURN_NOTICE`, asks the seat to close the screen, once; an
    /// unencrypted one keeps its prompt until the installed notice.
    #[test]
    fn a_committed_device_bound_installation_says_so_then_closes_attention() {
        use crate::authority::consent::Storage;
        for storage in [Storage::DeviceBound, Storage::Unencrypted] {
            let request = disk(storage);
            let tagged = |tag: &[u8]| [tag, request.encode().as_slice()].concat();
            let mut screen = Screen::new();
            Arc::get_mut(&mut screen.attempt).unwrap().selection = Selection::Install;
            let mut wire = wire(vec![
                tagged(&[0x92]),
                tagged(&[0x91, 4]),
                vec![0x93],
                tagged(&[0x91, 5]),
                vec![0x94],
                tagged(&[0x91, 3]),
                tagged(&[0x91, 3]),
                tagged(&[0x91, 3]),
                tagged(&[0x91, 3]),
            ]);
            let mut client = Client::default();
            client
                .start(&mut wire, Arc::clone(&screen.attempt))
                .unwrap();
            client.tick(&mut wire).unwrap();
            let completed = screen
                .attempt
                .presentation
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .after;
            screen
                .attempt
                .confirm_install(&crate::input::test_origin(), completed + 1)
                .unwrap();
            client.tick(&mut wire).unwrap();
            assert!(client.pending.as_ref().unwrap().committed);
            let prompt = std::fs::read(&screen.path).unwrap();
            client.tick(&mut wire).unwrap();
            let pending = client.pending.as_ref().unwrap();
            let bound = storage == Storage::DeviceBound;
            assert_eq!(pending.returning.is_some(), bound);
            // The notice replaced the prompt only for device-bound storage.
            assert_eq!(std::fs::read(&screen.path).unwrap() != prompt, bound);
            client.tick(&mut wire).unwrap();
            assert!(!client.pending.as_ref().unwrap().released);
            if let Some(shown) = client.pending.as_mut().unwrap().returning.as_mut() {
                *shown = Instant::now().checked_sub(RETURN_NOTICE).unwrap();
            }
            client.tick(&mut wire).unwrap();
            assert_eq!(client.pending.as_ref().unwrap().released, bound);
            // Asked once: a later status leaves it.
            client.tick(&mut wire).unwrap();
            assert_eq!(client.pending.as_ref().unwrap().released, bound);
            assert!(wire.replies.is_empty());
        }
    }

    /// Escape before the commit still declines: no notice, no release, and
    /// the cancellation goes to root.
    #[test]
    fn escape_before_commit_still_declines_a_device_bound_installation() {
        let request = disk(crate::authority::consent::Storage::DeviceBound);
        let tagged = |tag: &[u8]| [tag, request.encode().as_slice()].concat();
        let mut screen = Screen::new();
        Arc::get_mut(&mut screen.attempt).unwrap().selection = Selection::Install;
        let mut wire = wire(vec![
            tagged(&[0x92]),
            tagged(&[0x91, 4]),
            vec![0x93],
            vec![0x95, 0],
            tagged(&[0x91, 3]),
        ]);
        let mut client = Client::default();
        client
            .start(&mut wire, Arc::clone(&screen.attempt))
            .unwrap();
        client.tick(&mut wire).unwrap();
        screen.attempt.cancel();
        client.tick(&mut wire).unwrap();
        let pending = client.pending.as_ref().unwrap();
        assert!(pending.cancelled && !pending.committed);
        assert!(pending.returning.is_none() && !pending.released);
        assert!(!wire.calls.iter().any(|call| call.first() == Some(&0x14)));
        assert!(wire.calls.iter().any(|call| call.first() == Some(&0x15)));
    }

    #[test]
    fn installation_escape_or_hidden_prompt_cannot_be_confirmed() {
        for cancel in [false, true] {
            let mut screen = Screen::new();
            Arc::get_mut(&mut screen.attempt).unwrap().selection = Selection::Install;
            let request = disk(crate::authority::consent::Storage::Unencrypted);
            screen.attempt.present(request).unwrap();
            if cancel {
                screen.attempt.cancel();
            } else {
                screen
                    .attempt
                    .runtime
                    .lock()
                    .unwrap()
                    .attention_notice(
                        &crate::input::test_origin(),
                        crate::attention::Notice::Failed,
                    )
                    .unwrap();
            }
            screen
                .attempt
                .confirm_install(&crate::input::test_origin(), u128::MAX)
                .unwrap();
            assert!(!screen.attempt.commit());
        }
    }

    // Elevation consent: the approving Enter.

    const ELEVATIONS: &[Elevation] = &[Elevation::Rollback, Elevation::Hostname];

    /// Root's description of `elevation` for the owner.
    fn elevation(elevation: Elevation) -> Request {
        let operation = match elevation {
            Elevation::Rollback => Operation::DeployRollback {
                current: "a".repeat(64),
                previous: "b".repeat(64),
            },
            Elevation::Hostname => Operation::SetHostname {
                requester: 1000,
                old: "td".into(),
                new: "td-laptop".into(),
            },
        };
        Request::new([42; 32], 1000, operation).unwrap()
    }

    #[test]
    fn suspend_polls_once_submitted_and_never_retries_a_refusal() {
        for locked in [false, true] {
            let mut wire = Wire {
                replies: VecDeque::from([vec![0xa5, 0], vec![0xa5, 0], vec![0xa5, 3]]),
                calls: vec![],
                typist: None,
            };
            assert!(suspend_exchange(&mut wire, locked).unwrap());
            assert_eq!(
                wire.calls,
                vec![vec![0x25, u8::from(locked)], vec![0x26], vec![0x26]]
            );
            for reply in [
                vec![0xa5, 1],
                vec![0xa5, 2],
                vec![0xa5, 3],
                vec![0xa5],
                vec![0xa1, 0],
            ] {
                let mut wire = Wire {
                    replies: VecDeque::from([reply.clone()]),
                    calls: vec![],
                    typist: None,
                };
                if matches!(reply.as_slice(), [0xa5, 1 | 2]) {
                    assert!(!suspend_exchange(&mut wire, locked).unwrap());
                } else {
                    assert!(suspend_exchange(&mut wire, locked).is_err());
                }
                assert_eq!(wire.calls, vec![vec![0x25, u8::from(locked)]]);
            }
        }
    }

    #[test]
    fn suspend_without_a_live_physical_seat_never_sends() {
        for cancel in [false, true] {
            let mut screen = Screen::new();
            Arc::get_mut(&mut screen.attempt).unwrap().selection = Selection::Suspend;
            if cancel {
                screen.attempt.cancel();
            }
            let mut wire = Wire {
                replies: VecDeque::new(),
                calls: vec![],
                typist: None,
            };
            assert_eq!(
                Client::default()
                    .start(&mut wire, Arc::clone(&screen.attempt))
                    .is_ok(),
                cancel
            );
            assert!(wire.calls.is_empty());
            assert!(Selection::Suspend.request().is_err());
        }
    }

    #[test]
    fn power_uses_only_its_fixed_request_and_rejects_other_replies() {
        use crate::authority::consent::Power;
        for choice in [Power::Restart, Power::Off] {
            for reply in [
                vec![0xa1, 0],
                vec![0xa1, 1],
                vec![0xa1, 2],
                vec![0xa1, 3],
                vec![0x99, 0],
            ] {
                let mut screen = Screen::new();
                Arc::get_mut(&mut screen.attempt).unwrap().selection = Selection::Power(choice);
                let accepted = matches!(reply.as_slice(), [0xa1, 0..=2]);
                let mut wire = Wire {
                    replies: VecDeque::from([reply]),
                    calls: vec![],
                    typist: None,
                };
                let mut client = Client::default();
                assert_eq!(
                    client.start(&mut wire, Arc::clone(&screen.attempt)).is_ok(),
                    accepted
                );
                assert_eq!(wire.calls, vec![vec![0x21, choice.byte()]]);
                client.tick(&mut wire).unwrap();
                assert_eq!(wire.calls.len(), 1);
            }
        }
    }

    #[test]
    fn power_submission_claims_commit_and_never_sends_a_cancelled_attempt() {
        use crate::authority::consent::Power;
        for cancel in [true, false] {
            let mut screen = Screen::new();
            Arc::get_mut(&mut screen.attempt).unwrap().selection = Selection::Power(Power::Restart);
            if cancel {
                screen.attempt.cancel();
            }
            struct PowerWire {
                attempt: Arc<Attempt>,
                sent: bool,
            }
            impl Exchange for PowerWire {
                fn send(&mut self, bytes: &[u8]) -> Result<(), String> {
                    assert_eq!(self.attempt.state.load(Ordering::SeqCst), COMMITTED);
                    assert_eq!(bytes, [0x21, 0]);
                    self.sent = true;
                    // Cancellation after the execution decision cannot retract it.
                    self.attempt.cancel();
                    Ok(())
                }
                fn receive(&mut self) -> Result<Vec<u8>, String> {
                    Ok(vec![0xa1, 0])
                }
            }
            let mut wire = PowerWire {
                attempt: Arc::clone(&screen.attempt),
                sent: false,
            };
            Client::default()
                .start(&mut wire, Arc::clone(&screen.attempt))
                .unwrap();
            assert_eq!(wire.sent, !cancel);
        }
    }

    fn elevation_screen(elevation: Elevation) -> Screen {
        let mut screen = Screen::new();
        Arc::get_mut(&mut screen.attempt).unwrap().selection = Selection::Elevation(elevation);
        screen
    }

    /// `request` selected and presented, its presentation acknowledged,
    /// then `then`: the client, the wire, and when the prompt was on glass.
    fn presented_elevation(
        screen: &Screen,
        request: &Request,
        then: Vec<Vec<u8>>,
    ) -> (Client, Wire, u128) {
        let mut replies = vec![
            description(&[0x92], request),
            status(4, request),
            vec![0x93],
        ];
        replies.extend(then);
        let mut wire = wire(replies);
        let mut client = Client::default();
        client
            .start(&mut wire, Arc::clone(&screen.attempt))
            .unwrap();
        client.tick(&mut wire).unwrap();
        assert_eq!(sent(&wire, 0x13), 1);
        let shown = screen
            .attempt
            .presentation
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .after;
        (client, wire, shown)
    }

    /// The evdev adapter's offer of `approval` pressed at `at`: whether
    /// it ended the request.
    fn press(screen: &Screen, approval: ApprovalPress, at: u128) -> bool {
        screen
            .attempt
            .approve(&crate::input::test_origin(), approval, at)
            .unwrap()
    }

    fn confirmed(screen: &Screen) -> bool {
        screen.attempt.confirmed.load(Ordering::SeqCst)
    }

    /// Each elevation's prompt commits once, only after an approving Enter
    /// on the presented prompt, and the commit is root's exact
    /// description. Root's success shows each one's own screen.
    #[test]
    fn each_elevation_commits_only_after_an_approving_enter() {
        for (elevation, request) in [
            (Elevation::Rollback, vec![0x1d]),
            (Elevation::Hostname, vec![0x1e]),
        ] {
            let screen = elevation_screen(elevation);
            let described = super::tests::elevation(elevation);
            let invitation = status(5, &described);
            let (mut client, mut wire, shown) = presented_elevation(
                &screen,
                &described,
                vec![
                    invitation.clone(),
                    invitation,
                    vec![0x94],
                    status(6, &described),
                ],
            );
            assert_eq!(wire.calls[0], request);
            // Invited to commit, the client waits for the approval.
            client.tick(&mut wire).unwrap();
            assert_eq!(sent(&wire, 0x14), 0);
            assert!(!confirmed(&screen) && screen.attempt.active());
            assert!(!press(
                &screen,
                ApprovalPress::Enter,
                shown + APPROVAL_SETTLE
            ));
            assert!(confirmed(&screen));
            client.tick(&mut wire).unwrap();
            let commits: Vec<_> = wire
                .calls
                .iter()
                .filter(|call| call.first() == Some(&0x14))
                .collect();
            assert_eq!(commits, [&description(&[0x14], &described)]);
            assert!(client.pending.as_ref().unwrap().committed);
            client.tick(&mut wire).unwrap();
            assert!(client.pending.is_none());
            assert_eq!(
                screen.attempt.runtime.lock().unwrap().attention_shown(),
                Some(match elevation {
                    Elevation::Rollback => Notice::RolledBack,
                    Elevation::Hostname => Notice::HostnameSaved,
                })
            );
            assert_eq!(sent(&wire, 0x14), 1);
        }
    }

    /// Root's `9d` and `9e` refusals before any description: each shows
    /// its text and presents nothing. Each answers its own elevation alone,
    /// and no other refusal byte is one.
    #[test]
    fn an_elevation_root_refuses_shows_its_text_and_presents_nothing() {
        for (elevation, reply, request, notice) in [
            (Elevation::Rollback, vec![0x9d, 0], 0x1d, Notice::Busy),
            (
                Elevation::Rollback,
                vec![0x9d, 1],
                0x1d,
                Notice::ElevationRefused,
            ),
            (Elevation::Rollback, vec![0x9d, 2], 0x1d, Notice::NoPrevious),
            (Elevation::Hostname, vec![0x9e, 0], 0x1e, Notice::NoHostname),
            (
                Elevation::Hostname,
                vec![0x9e, 1],
                0x1e,
                Notice::ElevationRefused,
            ),
        ] {
            let screen = elevation_screen(elevation);
            let mut client = Client::default();
            let mut wire = wire(vec![reply]);
            client
                .start(&mut wire, Arc::clone(&screen.attempt))
                .unwrap();
            assert_eq!(wire.calls, [vec![request]]);
            assert!(client.pending.is_none());
            assert_eq!(
                screen.attempt.runtime.lock().unwrap().attention_shown(),
                Some(notice)
            );
        }
        for (elevation, reply) in [
            (Elevation::Rollback, vec![0x9d, 3]),
            (Elevation::Rollback, vec![0x9d]),
            (Elevation::Rollback, vec![0x9d, 0, 0]),
            (Elevation::Rollback, vec![0x99, 1]),
            (Elevation::Rollback, vec![0x9e, 0]),
            (Elevation::Hostname, vec![0x9e, 2]),
            (Elevation::Hostname, vec![0x9e]),
            (Elevation::Hostname, vec![0x9e, 0, 0]),
            (Elevation::Hostname, vec![0x9d, 0]),
            (Elevation::Hostname, vec![0x9d, 2]),
        ] {
            let screen = elevation_screen(elevation);
            assert!(Client::default()
                .start(&mut wire(vec![reply]), Arc::clone(&screen.attempt))
                .is_err());
        }
        for selection in [Selection::Install, Selection::Write] {
            for reply in [vec![0x9d, 1], vec![0x9e, 1]] {
                let mut screen = Screen::new();
                Arc::get_mut(&mut screen.attempt).unwrap().selection = selection.clone();
                assert!(Client::default()
                    .start(&mut wire(vec![reply]), Arc::clone(&screen.attempt))
                    .is_err());
            }
        }
    }

    /// The whole-disk installation's Enter never confirms an elevation, nor
    /// an approval press a prompt that awaits none: a disk installation,
    /// or a write.
    #[test]
    fn install_enter_never_confirms_an_elevation_nor_a_press_a_prompt_awaiting_none() {
        for &elevation in ELEVATIONS {
            let screen = elevation_screen(elevation);
            let described = super::tests::elevation(elevation);
            let (mut client, mut wire, shown) =
                presented_elevation(&screen, &described, vec![status(5, &described)]);
            for at in [shown + 1, shown + APPROVAL_SETTLE, u128::MAX] {
                screen
                    .attempt
                    .confirm_install(&crate::input::test_origin(), at)
                    .unwrap();
            }
            assert!(!confirmed(&screen));
            client.tick(&mut wire).unwrap();
            assert_eq!(sent(&wire, 0x14), 0);
            assert!(!screen.attempt.commit());
        }
        for (selection, request) in [
            (
                Selection::Install,
                disk(crate::authority::consent::Storage::DeviceBound),
            ),
            (Selection::Write, update()),
        ] {
            let mut screen = Screen::new();
            Arc::get_mut(&mut screen.attempt).unwrap().selection = selection;
            screen.attempt.present(request).unwrap();
            for approval in [ApprovalPress::Enter, ApprovalPress::Other] {
                assert!(!press(&screen, approval, u128::MAX));
            }
            assert!(!confirmed(&screen) && screen.attempt.active());
        }
    }

    /// A press stamped before the prompt was on glass counts for nothing,
    /// Enter or not; the first counted press decides, and once Enter
    /// approved, a later press, any key, changes nothing.
    #[test]
    fn a_press_before_the_prompt_was_on_glass_counts_for_nothing() {
        for &elevation in ELEVATIONS {
            let screen = elevation_screen(elevation);
            // Selected, not yet presented: nothing to answer.
            assert!(!press(&screen, ApprovalPress::Enter, u128::MAX));
            assert!(!press(&screen, ApprovalPress::Other, u128::MAX));
            let described = super::tests::elevation(elevation);
            let (_client, _wire, shown) = presented_elevation(&screen, &described, vec![]);
            for (approval, at) in [
                (ApprovalPress::Enter, shown),
                (ApprovalPress::Enter, shown - 1),
                (ApprovalPress::Other, shown),
                (ApprovalPress::Other, 0),
            ] {
                assert!(!press(&screen, approval, at));
            }
            assert!(screen.attempt.active());
            assert!(
                !screen
                    .attempt
                    .presentation
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .decided
            );
            assert!(!press(
                &screen,
                ApprovalPress::Enter,
                shown + APPROVAL_SETTLE
            ));
            assert!(confirmed(&screen));
            // Decided: a later press, Enter or another key, has no say.
            assert!(!press(
                &screen,
                ApprovalPress::Other,
                shown + APPROVAL_SETTLE + 1
            ));
            assert!(confirmed(&screen) && screen.attempt.active());
        }
    }

    /// The first counted press, if it is another key or an Enter inside
    /// the settle, ends the request unapproved: the attempt is cancelled,
    /// the client cancels with root and never commits, and a settled Enter
    /// after it changes nothing.
    #[test]
    fn a_first_press_other_than_a_settled_enter_ends_the_request() {
        for &elevation in ELEVATIONS {
            for (first, at) in [
                (ApprovalPress::Other, 1),
                (ApprovalPress::Other, APPROVAL_SETTLE),
                (ApprovalPress::Enter, 1),
                (ApprovalPress::Enter, APPROVAL_SETTLE - 1),
            ] {
                let screen = elevation_screen(elevation);
                let described = super::tests::elevation(elevation);
                let (mut client, mut wire, shown) = presented_elevation(
                    &screen,
                    &described,
                    vec![vec![0x95, 0], status(7, &described)],
                );
                assert!(screen.attempt.active());
                assert!(press(&screen, first, shown + at));
                assert!(!screen.attempt.active() && !confirmed(&screen));
                assert!(!press(
                    &screen,
                    ApprovalPress::Enter,
                    shown + APPROVAL_SETTLE + 1
                ));
                assert!(!confirmed(&screen));
                client.tick(&mut wire).unwrap();
                assert!(client.pending.is_none());
                assert_eq!(sent(&wire, 0x15), 1);
                assert_eq!(sent(&wire, 0x14), 0);
            }
        }
    }

    /// A prompt replaced by a notice, withdrawn or draining, or a cancelled
    /// attempt, takes no press: none approves, ends or confirms.
    #[test]
    fn a_hidden_prompt_or_cancelled_attempt_takes_no_press() {
        let presses = |shown: u128| {
            [
                (ApprovalPress::Other, shown + 1),
                (ApprovalPress::Enter, shown + APPROVAL_SETTLE),
            ]
        };
        for &elevation in ELEVATIONS {
            for case in 0..3 {
                let screen = elevation_screen(elevation);
                let described = super::tests::elevation(elevation);
                let (_client, _wire, shown) = presented_elevation(&screen, &described, vec![]);
                let mut runtime = screen.attempt.runtime.lock().unwrap();
                match case {
                    0 => {
                        runtime
                            .attention_notice(
                                &crate::input::test_origin(),
                                crate::attention::Notice::Failed,
                            )
                            .unwrap();
                    }
                    1 => runtime.abandon_attention_presentation(&described),
                    _ => {
                        runtime
                            .drain_attention(&crate::input::test_origin())
                            .unwrap();
                    }
                }
                drop(runtime);
                for (approval, at) in presses(shown) {
                    assert!(!press(&screen, approval, at));
                }
                assert!(!confirmed(&screen) && screen.attempt.active());
                assert!(!screen.attempt.commit());
            }
            let screen = elevation_screen(elevation);
            let described = super::tests::elevation(elevation);
            let (_client, _wire, shown) = presented_elevation(&screen, &described, vec![]);
            screen.attempt.cancel();
            for (approval, at) in presses(shown) {
                assert!(!press(&screen, approval, at));
            }
            assert!(!confirmed(&screen));
        }
    }

    /// Root's description must be the selected elevation for the owner:
    /// another elevation, an installation, or another owner ends the
    /// channel before anything is presented.
    #[test]
    fn root_must_describe_the_selected_elevation_for_the_owner() {
        let other_owner = Request::new(
            [42; 32],
            1001,
            Operation::DeployRollback {
                current: "a".repeat(64),
                previous: "b".repeat(64),
            },
        )
        .unwrap();
        let update = update();
        for (selection, described) in [
            (
                Selection::Elevation(Elevation::Rollback),
                elevation(Elevation::Hostname),
            ),
            (
                Selection::Elevation(Elevation::Hostname),
                elevation(Elevation::Rollback),
            ),
            (Selection::Elevation(Elevation::Rollback), other_owner),
            (Selection::Elevation(Elevation::Rollback), update),
            (Selection::Install, elevation(Elevation::Rollback)),
            (Selection::Write, elevation(Elevation::Hostname)),
        ] {
            let mut screen = Screen::new();
            Arc::get_mut(&mut screen.attempt).unwrap().selection = selection;
            let mut wire = wire(vec![description(&[0x92], &described)]);
            let mut client = Client::default();
            assert_eq!(
                client
                    .start(&mut wire, Arc::clone(&screen.attempt))
                    .unwrap_err(),
                "root secret request changed the selected operation"
            );
            assert!(client.pending.is_none());
        }
    }

    #[test]
    fn actual_presentation_and_one_commit_precede_success() {
        let screen = Screen::new();
        let mut client = Client::default();
        let mut wire = wire(vec![
            tagged(&[0x92]),
            tagged(&[0x91, 4]),
            vec![0x93],
            tagged(&[0x91, 5]),
            vec![0x94],
            tagged(&[0x91, 6]),
        ]);
        client
            .start(&mut wire, Arc::clone(&screen.attempt))
            .unwrap();
        let inert = std::fs::read(&screen.path).unwrap();
        client.tick(&mut wire).unwrap();
        assert_ne!(std::fs::read(&screen.path).unwrap(), inert);
        assert_eq!(client.pending.as_ref().unwrap().receipt, Some(request()));
        assert!(screen.attempt.active());
        client.tick(&mut wire).unwrap();
        assert!(!screen.attempt.active());
        client.tick(&mut wire).unwrap();
        assert!(client.pending.is_none());
        assert_eq!(
            wire.calls,
            [
                vec![0x12, 1],
                vec![0x11],
                tagged(&[0x13]),
                vec![0x11],
                tagged(&[0x14]),
                vec![0x11]
            ]
        );
    }

    #[test]
    fn cancellation_before_presentation_or_commit_sends_no_commit() {
        for presented in [false, true] {
            let screen = Screen::new();
            let mut client = Client::default();
            let mut replies = vec![tagged(&[0x92])];
            if presented {
                replies.extend([tagged(&[0x91, 4]), vec![0x93]]);
            }
            replies.extend([vec![0x95, 0], tagged(&[0x91, 7])]);
            let mut wire = wire(replies);
            client
                .start(&mut wire, Arc::clone(&screen.attempt))
                .unwrap();
            if presented {
                client.tick(&mut wire).unwrap();
            }
            screen.attempt.cancel();
            client.tick(&mut wire).unwrap();
            assert!(client.pending.is_none());
            assert!(!wire.calls.iter().any(|bytes| bytes.first() == Some(&0x14)));
            assert_eq!(
                wire.calls
                    .iter()
                    .filter(|bytes| bytes.first() == Some(&0x15))
                    .count(),
                1
            );
        }
    }

    #[test]
    fn cancellation_during_commit_poll_wins_before_the_execution_decision() {
        struct CancelOnCommit<'a> {
            wire: Wire,
            attempt: &'a Attempt,
        }
        impl Exchange for CancelOnCommit<'_> {
            fn send(&mut self, bytes: &[u8]) -> Result<(), String> {
                self.wire.send(bytes)
            }
            fn receive(&mut self) -> Result<Vec<u8>, String> {
                let response = self.wire.receive()?;
                if response.starts_with(&[0x91, 5]) {
                    self.attempt.cancel();
                }
                Ok(response)
            }
        }
        let screen = Screen::new();
        let mut client = Client::default();
        let mut channel = CancelOnCommit {
            wire: wire(vec![
                tagged(&[0x92]),
                tagged(&[0x91, 4]),
                vec![0x93],
                tagged(&[0x91, 5]),
                vec![0x95, 0],
                tagged(&[0x91, 7]),
            ]),
            attempt: &screen.attempt,
        };
        client
            .start(&mut channel, Arc::clone(&screen.attempt))
            .unwrap();
        client.tick(&mut channel).unwrap();
        client.tick(&mut channel).unwrap();
        client.tick(&mut channel).unwrap();
        assert!(client.pending.is_none());
        assert!(!channel
            .wire
            .calls
            .iter()
            .any(|bytes| bytes.first() == Some(&0x14)));
        assert!(channel
            .wire
            .calls
            .iter()
            .any(|bytes| bytes.first() == Some(&0x15)));
    }

    #[test]
    fn cancellation_after_worker_failure_retires_the_relocked_result() {
        let screen = Screen::new();
        let mut client = Client::default();
        let mut wire = wire(vec![tagged(&[0x92]), vec![0x95, 2], tagged(&[0x91, 7])]);
        client
            .start(&mut wire, Arc::clone(&screen.attempt))
            .unwrap();
        screen.attempt.cancel();
        client.tick(&mut wire).unwrap();
        assert!(client.pending.is_none());
    }

    #[test]
    fn uncertain_commit_delivery_is_never_retried() {
        let screen = Screen::new();
        let mut client = Client::default();
        let mut wire = wire(vec![
            tagged(&[0x92]),
            tagged(&[0x91, 4]),
            vec![0x93],
            tagged(&[0x91, 5]),
        ]);
        client
            .start(&mut wire, Arc::clone(&screen.attempt))
            .unwrap();
        client.tick(&mut wire).unwrap();
        assert_eq!(client.tick(&mut wire).unwrap_err(), "uncertain delivery");
        assert!(!screen.attempt.active());
        assert!(!screen.attempt.commit());
        wire.replies.push_back(tagged(&[0x91, 5]));
        assert!(client.tick(&mut wire).is_err());
        assert_eq!(
            wire.calls
                .iter()
                .filter(|bytes| bytes.first() == Some(&0x14))
                .count(),
            1
        );
    }

    #[test]
    fn failed_presentation_cancels_without_starting_token_acquisition() {
        let screen = Screen::new();
        screen
            .attempt
            .runtime
            .lock()
            .unwrap()
            .begin_compound_commit()
            .unwrap();
        let mut client = Client::default();
        let mut wire = wire(vec![tagged(&[0x92]), tagged(&[0x91, 4]), vec![0x95, 0]]);
        client
            .start(&mut wire, Arc::clone(&screen.attempt))
            .unwrap();
        client.tick(&mut wire).unwrap();
        assert!(client.pending.as_ref().unwrap().cancelled);
        assert!(client.pending.as_ref().unwrap().receipt.is_none());
        assert!(!wire.calls.iter().any(|bytes| bytes.first() == Some(&0x13)));
    }

    #[test]
    fn cancelled_queued_attempt_is_never_started() {
        let screen = Screen::new();
        screen.attempt.cancel();
        let mut wire = wire(vec![]);
        Client::default()
            .start(&mut wire, Arc::clone(&screen.attempt))
            .unwrap();
        assert!(wire.calls.is_empty());
        assert!(!screen.attempt.commit());
    }

    #[test]
    fn cancellation_and_commit_have_one_atomic_winner() {
        for _ in 0..64 {
            let screen = Screen::new();
            let attempt = Arc::clone(&screen.attempt);
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let other = Arc::clone(&barrier);
            let commit = std::thread::spawn(move || {
                other.wait();
                attempt.commit()
            });
            barrier.wait();
            screen.attempt.cancel();
            let committed = commit.join().unwrap();
            let state = screen.attempt.state.load(Ordering::SeqCst);
            assert_eq!(
                state,
                if committed {
                    COMMITTED | CANCELLED
                } else {
                    CANCELLED
                }
            );
            assert!(!screen.attempt.commit());
        }
    }

    #[test]
    fn stale_requests_and_success_without_commit_end_the_channel() {
        for response in [tagged(&[0x91, 6]), tagged(&[0x91, 5]), vec![0x91, 2], {
            let stale = Request::new([43; 32], 1000, request().operation().clone()).unwrap();
            [&[0x91, 4][..], stale.encode().as_slice()].concat()
        }] {
            let screen = Screen::new();
            let mut client = Client::default();
            let mut wire = wire(vec![tagged(&[0x92]), response]);
            client
                .start(&mut wire, Arc::clone(&screen.attempt))
                .unwrap();
            assert!(client.tick(&mut wire).is_err());
            assert!(!wire
                .calls
                .iter()
                .any(|bytes| bytes.first() == Some(&0x13) || bytes.first() == Some(&0x14)));
        }
    }

    #[test]
    fn late_result_cannot_paint_into_a_reopened_attention_screen() {
        let screen = Screen::new();
        assert!(screen.attempt.commit());
        screen.attempt.cancel();
        let mut runtime = screen.attempt.runtime.lock().unwrap();
        runtime
            .attention(&crate::input::test_origin(), false)
            .unwrap();
        runtime
            .attention(&crate::input::test_origin(), true)
            .unwrap();
        drop(runtime);
        let before = std::fs::read(&screen.path).unwrap();
        screen
            .attempt
            .notice(crate::attention::Notice::Unlocked)
            .unwrap();
        assert_eq!(std::fs::read(&screen.path).unwrap(), before);
    }

    fn enrollment(recovery: Recovery, step: Enrollment) -> Request {
        Request::new(
            [42; 32],
            1000,
            Operation::Enroll {
                platform: Platform::TpmPcr7,
                recovery,
                step,
            },
        )
        .unwrap()
    }
    fn description(tag: &[u8], request: &Request) -> Vec<u8> {
        [tag, request.encode().as_slice()].concat()
    }

    #[test]
    fn pending_write_displays_the_root_target_and_requires_one_exact_commit() {
        for role in [Role::Primary, Role::Recovery] {
            let mut screen = Screen::new();
            Arc::get_mut(&mut screen.attempt).unwrap().selection = Selection::Write;
            let request = Request::new(
                [43; 32],
                1000,
                Operation::Set {
                    application: "mail".into(),
                    name: "main".into(),
                    application_uid: 65537,
                    requester: 1000,
                    role,
                },
            )
            .unwrap();
            let mut wire = wire(vec![
                description(&[0x92], &request),
                description(&[0x91, 4], &request),
                vec![0x93],
                description(&[0x91, 5], &request),
                vec![0x94],
                description(&[0x91, 6], &request),
            ]);
            let mut client = Client::default();
            client
                .start(&mut wire, Arc::clone(&screen.attempt))
                .unwrap();
            let before = std::fs::read(&screen.path).unwrap();
            client.tick(&mut wire).unwrap();
            assert_ne!(std::fs::read(&screen.path).unwrap(), before);
            assert_eq!(
                client.pending.as_ref().unwrap().receipt,
                Some(request.clone())
            );
            client.tick(&mut wire).unwrap();
            client.tick(&mut wire).unwrap();
            assert!(client.pending.is_none());
            assert_eq!(
                wire.calls,
                [
                    vec![0x18],
                    vec![0x11],
                    description(&[0x13], &request),
                    vec![0x11],
                    description(&[0x14], &request),
                    vec![0x11]
                ]
            );
        }
    }

    #[test]
    fn empty_write_queue_is_a_notice_and_a_different_operation_is_refused() {
        for response in [vec![0x98, 0], tagged(&[0x92])] {
            let mut screen = Screen::new();
            Arc::get_mut(&mut screen.attempt).unwrap().selection = Selection::Write;
            let mut client = Client::default();
            let no_write = response == [0x98, 0];
            let mut wire = wire(vec![response]);
            assert_eq!(
                client.start(&mut wire, Arc::clone(&screen.attempt)).is_ok(),
                no_write
            );
            assert!(client.pending.is_none());
            assert_eq!(wire.calls, [vec![0x18]]);
        }
    }
    fn enrollment_screen(recovery: Recovery) -> Screen {
        let mut screen = Screen::new();
        Arc::get_mut(&mut screen.attempt).unwrap().selection = Selection::Enroll(recovery);
        screen
    }

    #[test]
    fn failed_or_invalid_successor_consumes_the_presentation_slot() {
        for invalid in [true, false] {
            let screen = enrollment_screen(Recovery::SecondToken);
            let first = enrollment(Recovery::SecondToken, Enrollment::CreatePrimary);
            let next = first.following_enrollment_step().unwrap().unwrap();
            let mut runtime = screen.attempt.runtime.lock().unwrap();
            runtime
                .present_attention_request_with_time(&crate::input::test_origin(), first, Some(120))
                .unwrap();
            let request = if invalid {
                enrollment(Recovery::SecondToken, Enrollment::CreateRecovery)
            } else {
                next.clone()
            };
            // A zero budget fails raster preparation after admitting the successor.
            let remaining = if invalid { Some(119) } else { Some(0) };
            assert!(runtime
                .present_attention_request_with_time(
                    &crate::input::test_origin(),
                    request,
                    remaining
                )
                .is_err());
            assert!(runtime
                .present_attention_request_with_time(&crate::input::test_origin(), next, Some(119))
                .is_err());
        }
    }

    #[test]
    fn enrollment_inspects_then_presents_each_exact_step_before_one_commit() {
        for recovery in [Recovery::Unrecoverable, Recovery::SecondToken] {
            let screen = enrollment_screen(recovery);
            let initial = enrollment(recovery, Enrollment::CreatePrimary);
            let mut steps = vec![initial.clone()];
            while let Some(next) = steps.last().unwrap().following_enrollment_step().unwrap() {
                steps.push(next);
            }
            let last = steps.last().unwrap();
            let mut replies = vec![vec![0x97], vec![0x91, 9, 0], description(&[0x92], &initial)];
            for step in &steps {
                replies.extend([description(&[0x91, 4], step), vec![0x93]]);
            }
            replies.extend([
                description(&[0x91, 5], last),
                vec![0x94],
                description(&[0x91, 6], last),
            ]);
            let mut wire = wire(replies);
            let mut client = Client::default();
            client
                .start(&mut wire, Arc::clone(&screen.attempt))
                .unwrap();
            client.tick(&mut wire).unwrap();
            for step in &steps {
                let previous = std::fs::read(&screen.path).unwrap();
                client.tick(&mut wire).unwrap();
                assert_ne!(std::fs::read(&screen.path).unwrap(), previous);
                assert_eq!(
                    client.pending.as_ref().unwrap().receipt.as_ref(),
                    Some(step)
                );
            }
            client.tick(&mut wire).unwrap();
            client.tick(&mut wire).unwrap();
            assert!(client.pending.is_none());
            assert_eq!(
                wire.calls
                    .iter()
                    .filter(|call| call.first() == Some(&0x13))
                    .count(),
                steps.len()
            );
            assert_eq!(
                wire.calls
                    .iter()
                    .filter(|call| call.first() == Some(&0x14))
                    .count(),
                1
            );
        }
    }

    #[test]
    fn enrollment_refuses_skipped_steps_and_commit_before_final_proof() {
        for status in [4, 5] {
            let recovery = Recovery::SecondToken;
            let screen = enrollment_screen(recovery);
            let initial = enrollment(recovery, Enrollment::CreatePrimary);
            let forged = if status == 4 {
                enrollment(recovery, Enrollment::CreateRecovery)
            } else {
                initial.clone()
            };
            let mut wire = wire(vec![
                vec![0x97],
                vec![0x91, 9, 1],
                description(&[0x92], &initial),
                description(&[0x91, 4], &initial),
                vec![0x93],
                description(&[0x91, status], &forged),
            ]);
            let mut client = Client::default();
            client
                .start(&mut wire, Arc::clone(&screen.attempt))
                .unwrap();
            client.tick(&mut wire).unwrap();
            client.tick(&mut wire).unwrap();
            assert!(client.tick(&mut wire).is_err());
            assert!(!wire.calls.iter().any(|call| call.first() == Some(&0x14)));
        }
    }

    #[test]
    fn root_successor_timeout_waits_for_cleanup_then_inspects_without_acknowledging() {
        let recovery = Recovery::SecondToken;
        let screen = enrollment_screen(recovery);
        let first = enrollment(recovery, Enrollment::CreatePrimary);
        let next = first.following_enrollment_step().unwrap().unwrap();
        let mut wire = wire(vec![
            vec![0x97],
            vec![0x91, 9, 0],
            description(&[0x92], &first),
            description(&[0x91, 4], &first),
            vec![0x93],
            description(&[0x91, 3], &next),
            description(&[0x91, 7], &next),
            vec![0x97],
            vec![0x91, 9, 0],
        ]);
        let mut client = Client::default();
        client
            .start(&mut wire, Arc::clone(&screen.attempt))
            .unwrap();
        client.tick(&mut wire).unwrap();
        client.tick(&mut wire).unwrap();
        assert!(!client.pending.as_ref().unwrap().cancelled);
        client.tick(&mut wire).unwrap();
        assert_eq!(client.pending.as_ref().unwrap().request, next);
        assert!(client.pending.as_ref().unwrap().receipt.is_none());
        client.tick(&mut wire).unwrap();
        client.tick(&mut wire).unwrap();
        assert!(client.pending.is_none() && client.inspection.is_none());
        assert_eq!(
            wire.calls
                .iter()
                .filter(|call| call.first() == Some(&0x13))
                .count(),
            1
        );
        assert_eq!(
            wire.calls
                .iter()
                .filter(|call| call.first() == Some(&0x17))
                .count(),
            2
        );
        assert!(!wire.calls.iter().any(|call| call.first() == Some(&0x14)));
    }

    #[test]
    fn failed_enrollment_uses_a_new_inspection_and_never_retries_the_operation() {
        for state in [0, 2, 3] {
            let recovery = Recovery::SecondToken;
            let screen = enrollment_screen(recovery);
            let initial = enrollment(recovery, Enrollment::CreatePrimary);
            let mut wire = wire(vec![
                vec![0x97],
                vec![0x91, 9, 0],
                description(&[0x92], &initial),
                description(&[0x91, 7], &initial),
                vec![0x97],
                vec![0x91, 9, state],
            ]);
            let mut client = Client::default();
            client
                .start(&mut wire, Arc::clone(&screen.attempt))
                .unwrap();
            client.tick(&mut wire).unwrap();
            client.tick(&mut wire).unwrap();
            client.tick(&mut wire).unwrap();
            assert!(client.pending.is_none() && client.inspection.is_none());
            assert_eq!(
                wire.calls
                    .iter()
                    .filter(|call| call.first() == Some(&0x16))
                    .count(),
                1
            );
            assert_eq!(
                wire.calls
                    .iter()
                    .filter(|call| call.first() == Some(&0x17))
                    .count(),
                2
            );
            assert!(!wire.calls.iter().any(|call| call.first() == Some(&0x12)));
        }
    }

    #[test]
    fn cancellation_or_token_state_during_inspection_never_starts_enrollment() {
        for cancel in [false, true] {
            let screen = enrollment_screen(Recovery::Unrecoverable);
            let mut wire = wire(vec![vec![0x97], vec![0x91, 9, if cancel { 0 } else { 3 }]]);
            let mut client = Client::default();
            client
                .start(&mut wire, Arc::clone(&screen.attempt))
                .unwrap();
            if cancel {
                screen.attempt.cancel();
            }
            client.tick(&mut wire).unwrap();
            assert!(client.pending.is_none() && client.inspection.is_none());
            assert_eq!(wire.calls, [vec![0x17], vec![0x11]]);
        }
    }

    // Login-key operations.

    const NONCE: [u8; 32] = [9; 32];
    const ENROLLED: [Fingerprint; 3] = [[0xa1; 4], [0xa2; 4], [0xa3; 4]];
    const NEW: Fingerprint = [0xb1; 4];
    const BACKUP: Fingerprint = [0xb2; 4];

    impl Screen {
        /// At 1024x768, which holds every disclosure prompt whole.
        fn login(selection: LoginSelection) -> Self {
            Self::login_at(selection, 1024, 768)
        }
        fn login_at(selection: LoginSelection, width: usize, height: usize) -> Self {
            let mut screen = Self::sized(width, height);
            screen.attempt = Attempt::new(
                crate::input::test_origin(),
                Arc::clone(&screen.attempt.runtime),
                Selection::Login(selection),
            );
            screen
        }
        fn shown(&self) -> Option<Notice> {
            self.attempt.runtime.lock().unwrap().attention_shown()
        }
        /// The PIN field or touch request over the retained prompt.
        fn field(&self) -> Option<Field> {
            self.attempt.runtime.lock().unwrap().attention_field_shown()
        }
        /// A login attempt whose lifetime ends `after` from now.
        fn expiring(selection: LoginSelection, after: Duration) -> (Self, Instant) {
            let mut screen = Self::login(selection.clone());
            let deadline = Instant::now() + after;
            screen.attempt = Arc::new(Attempt {
                origin: crate::input::test_origin(),
                runtime: Arc::clone(&screen.attempt.runtime),
                selection: Selection::Login(selection),
                state: AtomicU8::new(ACTIVE),
                deadline: Some(deadline),
                presentation: Mutex::new(None),
                confirmed: AtomicBool::new(false),
                field: Mutex::new(PinField::default()),
                seat: None,
            });
            (screen, deadline)
        }
        /// A login attempt whose lifetime ends `after` from now, `setup`
        /// run on it with at least half that lifetime to spare, so what the
        /// case does next still runs before the deadline, and a prompt the
        /// setup presents shows at least a second left. A host too loaded
        /// for that discards the attempt, its result or panic alike, and
        /// runs `setup` again with twice the lifetime, up to 64 seconds,
        /// so a case proves what its deadline does without assuming a
        /// speed. At that ceiling a panic is resumed.
        fn expiring_after<T>(
            selection: LoginSelection,
            mut after: Duration,
            setup: impl Fn(&Self) -> T,
        ) -> (Self, Instant, T) {
            assert!(after >= Duration::from_secs(2));
            loop {
                let (screen, deadline) = Self::expiring(selection.clone(), after);
                let set = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| setup(&screen)));
                let spare = Instant::now() + after / 2 <= deadline;
                if spare || after >= Duration::from_secs(64) {
                    match set {
                        Ok(value) if spare => return (screen, deadline, value),
                        Ok(_) => panic!("setup never finished in half its lifetime"),
                        Err(panic) => std::panic::resume_unwind(panic),
                    }
                }
                after *= 2;
            }
        }
    }

    fn login(operation: Operation) -> Request {
        Request::new(NONCE, 1000, operation).unwrap()
    }
    fn unlock_step(step: LoginStep) -> Request {
        login(Operation::LoginUnlock {
            account: 1000,
            before: 3,
            after: 3,
            step,
        })
    }
    /// Its first step discloses.
    fn enroll_step(after: u8, key: u8, step: LoginStep) -> Request {
        login(Operation::LoginEnroll {
            account: 1000,
            before: 0,
            after,
            key,
            step,
            proof: None,
        })
    }
    fn add_step(step: LoginStep) -> Request {
        login(Operation::LoginAdd {
            account: 1000,
            before: 3,
            after: 4,
            step,
            proof: None,
        })
    }
    fn enroll_op(after: u8, step: LoginStep) -> Operation {
        Operation::LoginEnroll {
            account: 1000,
            before: 0,
            after,
            key: 1,
            step,
            proof: None,
        }
    }
    fn add_op(step: LoginStep) -> Operation {
        Operation::LoginAdd {
            account: 1000,
            before: 3,
            after: 4,
            step,
            proof: None,
        }
    }
    fn remove_op(step: LoginStep) -> Operation {
        Operation::LoginRemove {
            account: 1000,
            before: 3,
            after: 1,
            removed: removal(),
            step,
            proof: None,
        }
    }
    /// `operation` authorized by `proof`, its first step disclosing where
    /// the proof's does.
    fn proven(operation: Operation, proof: Proof) -> Request {
        let operation = match operation {
            Operation::LoginEnroll {
                account,
                before,
                after,
                key,
                step,
                ..
            } => Operation::LoginEnroll {
                account,
                before,
                after,
                key,
                step,
                proof: Some(proof),
            },
            Operation::LoginAdd {
                account,
                before,
                after,
                step,
                ..
            } => Operation::LoginAdd {
                account,
                before,
                after,
                step,
                proof: Some(proof),
            },
            Operation::LoginRemove {
                account,
                before,
                after,
                removed,
                step,
                ..
            } => Operation::LoginRemove {
                account,
                before,
                after,
                removed,
                step,
                proof: Some(proof),
            },
            other => other,
        };
        login(operation)
    }
    fn removal() -> Vec<Slot> {
        vec![
            Slot {
                position: 1,
                key: ENROLLED[0],
            },
            Slot {
                position: 3,
                key: ENROLLED[2],
            },
        ]
    }
    /// Leaving one key: its identify discloses.
    fn remove_step(step: LoginStep) -> Request {
        login(Operation::LoginRemove {
            account: 1000,
            before: 3,
            after: 1,
            removed: removal(),
            step,
            proof: None,
        })
    }
    fn new_key(key: Fingerprint) -> [LoginStep; 5] {
        [
            LoginStep::Connect,
            LoginStep::Create { retries: 8 },
            LoginStep::Prove { key, retries: 8 },
            LoginStep::Repeat { key, retries: 7 },
            LoginStep::Probe { key },
        ]
    }
    /// Each operation's steps in their only order, as root presents them.
    fn steps(selection: &LoginSelection) -> Vec<Request> {
        match selection {
            LoginSelection::Notebook => panic!("notebook tests drive their own"),
            LoginSelection::Unlock => vec![
                unlock_step(LoginStep::Identify),
                unlock_step(LoginStep::Unlock {
                    key: ENROLLED[1],
                    retries: 8,
                }),
            ],
            LoginSelection::Enroll(after) => (1..=*after)
                .flat_map(|key| {
                    new_key(if key == 1 { NEW } else { BACKUP })
                        .map(|step| enroll_step(*after, key, step))
                })
                .collect(),
            LoginSelection::Add => [
                LoginStep::Identify,
                LoginStep::Authorize {
                    key: ENROLLED[1],
                    retries: 8,
                },
            ]
            .into_iter()
            .chain(new_key(NEW))
            .map(add_step)
            .collect(),
            LoginSelection::Remove(..) => vec![
                remove_step(LoginStep::Identify),
                remove_step(LoginStep::Authorize {
                    key: ENROLLED[0],
                    retries: 8,
                }),
            ],
            LoginSelection::SelectorUpdate => panic!("storage tests drive their own"),
            LoginSelection::Upgrade { shape, count } => std::iter::once(UpgradeStep::Review)
                .chain((1..=*count).flat_map(|key| {
                    new_key(if key == 1 { NEW } else { BACKUP })
                        .map(move |step| UpgradeStep::Key { key, step })
                }))
                .map(|step| upgrade_at(*shape, *count, step))
                .collect(),
            LoginSelection::UpgradeCancel => vec![upgrade_cancel()],
            LoginSelection::Protector(managed) => protector_steps(*managed),
            LoginSelection::Proven(proof, inner) => {
                let firsts = match proof {
                    Proof::RecoveryKey => vec![LoginStep::RecoveryKey],
                    Proof::DiskKey => vec![
                        LoginStep::DiskIdentify,
                        LoginStep::DiskAuthorize {
                            key: DISK,
                            retries: 8,
                        },
                    ],
                };
                let enroll = |after, key, step| {
                    proven(
                        Operation::LoginEnroll {
                            account: 1000,
                            before: 0,
                            after,
                            key,
                            step,
                            proof: None,
                        },
                        *proof,
                    )
                };
                match inner.as_ref() {
                    LoginSelection::Add => firsts
                        .into_iter()
                        .chain(new_key(NEW))
                        .map(|step| proven(add_op(step), *proof))
                        .collect(),
                    LoginSelection::Remove(..) => firsts
                        .into_iter()
                        .map(|step| proven(remove_op(step), *proof))
                        .collect(),
                    LoginSelection::Enroll(after) => firsts
                        .into_iter()
                        .map(|step| enroll(*after, 1, step))
                        .chain((1..=*after).flat_map(|key| {
                            new_key(if key == 1 { NEW } else { BACKUP })
                                .map(|step| enroll(*after, key, step))
                        }))
                        .collect(),
                    _ => panic!("a disk proof authorizes no {inner:?}"),
                }
            }
        }
    }
    /// A header fido2 token's fingerprint, which a disk authorize step
    /// names.
    const DISK: Fingerprint = [0xd1; 4];
    /// Each disk proof of each operation it authorizes.
    fn proven_selections() -> Vec<LoginSelection> {
        let remove = || Box::new(LoginSelection::Remove(removal(), 1));
        vec![
            LoginSelection::Proven(Proof::RecoveryKey, Box::new(LoginSelection::Enroll(1))),
            LoginSelection::Proven(Proof::DiskKey, Box::new(LoginSelection::Enroll(2))),
            LoginSelection::Proven(Proof::RecoveryKey, Box::new(LoginSelection::Add)),
            LoginSelection::Proven(Proof::DiskKey, Box::new(LoginSelection::Add)),
            LoginSelection::Proven(Proof::RecoveryKey, remove()),
            LoginSelection::Proven(Proof::DiskKey, remove()),
        ]
    }
    fn selections() -> Vec<LoginSelection> {
        vec![
            LoginSelection::Unlock,
            LoginSelection::Enroll(1),
            LoginSelection::Enroll(2),
            LoginSelection::Add,
            LoginSelection::Remove(removal(), 1),
        ]
    }
    fn status(code: u8, request: &Request) -> Vec<u8> {
        description(&[0x91, code], request)
    }
    fn started() -> Vec<u8> {
        [&[0x9b, 1][..], &NONCE].concat()
    }
    fn ended(code: u8, kind: u8, detail: u8) -> Vec<u8> {
        vec![0x91, code, kind, detail]
    }
    fn cancelled() -> Vec<u8> {
        [&[0x15][..], &NONCE].concat()
    }

    /// `step`'s `1c` carrying `pin`.
    fn pin_request(step: &Request, pin: &[u8]) -> Vec<u8> {
        let encoded = step.encode();
        [
            &[0x1c, u8::try_from(encoded.len()).unwrap()][..],
            &encoded,
            pin,
        ]
        .concat()
    }

    /// Root's statuses for `presented` steps, from the start: the first
    /// while root waits for the worker to repeat it, then each presented,
    /// acknowledged and worked on, and the client's requests they draw. A
    /// PIN step's field opens at its first `0c`; the person types the PIN
    /// before the next, which the client answers with `1c`.
    fn presented(selection: &LoginSelection, count: usize) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
        let steps = steps(selection);
        let started = [&[selection.answer(), 1][..], &NONCE].concat();
        let mut replies = vec![started, vec![0x91, 0x0b], status(3, &steps[0])];
        let mut calls = vec![
            Selection::Login(selection.clone()).request().unwrap(),
            vec![0x11],
            vec![0x11],
        ];
        for step in steps.iter().take(count) {
            // A disclosure is presented at one poll and, approved,
            // acknowledged at the next.
            if step.awaits_approval() {
                replies.push(status(4, step));
                calls.push(vec![0x11]);
            }
            replies.extend([status(4, step), vec![0x93]]);
            calls.extend([vec![0x11], description(&[0x13], step)]);
            if asks_pin(step) {
                replies.extend([status(0x0c, step), status(0x0c, step), vec![0x9c, 0]]);
                let secret = typed_for(Profile::of(step));
                calls.extend([vec![0x11], vec![0x11], pin_request(step, secret)]);
            }
            replies.push(status(3, step));
            calls.push(vec![0x11]);
        }
        (replies, calls)
    }

    fn drive(screen: &Screen, replies: Vec<Vec<u8>>) -> (Client, Wire, Result<(), String>) {
        let mut wire = typing(replies, screen);
        let mut client = login_client();
        let mut result = client.start(&mut wire, Arc::clone(&screen.attempt));
        while result.is_ok() && client.login.is_some() && !wire.replies.is_empty() {
            result = client.tick(&mut wire);
        }
        (client, wire, result)
    }

    fn sent(wire: &Wire, tag: u8) -> usize {
        wire.calls
            .iter()
            .filter(|call| call.first() == Some(&tag))
            .count()
    }

    #[test]
    fn every_login_operation_runs_its_whole_status_sequence() {
        for selection in selections() {
            let screen = Screen::login(selection.clone());
            let steps = steps(&selection);
            let last = steps.last().unwrap();
            let (mut replies, mut calls) = presented(&selection, steps.len());
            replies.extend([
                status(5, last),
                vec![0x94],
                status(3, last),
                status(6, last),
            ]);
            calls.extend([
                vec![0x11],
                description(&[0x14], last),
                vec![0x11],
                vec![0x11],
            ]);
            let (mut client, wire, result) = drive(&screen, replies);
            assert_eq!(result, Ok(()), "{selection:?}");
            assert!(client.login.is_none());
            // Its success is followed by `1a`, once.
            assert!(client.take_login_end());
            assert!(!client.take_login_end());
            assert_eq!(wire.calls, calls, "{selection:?}");
            assert_eq!(sent(&wire, 0x13), steps.len());
            // One `1c` for each PIN step, and only for those.
            let pins = steps.iter().filter(|step| asks_pin(step)).count();
            assert!(pins > 0);
            assert_eq!(sent(&wire, 0x1c), pins, "{selection:?}");
            assert_eq!(screen.shown(), Some(Notice::Login(selection.success(None))));
            assert_eq!(screen.attempt.field_raw(), [0; PIN_CAPACITY]);
        }
    }

    /// Each disk proof's whole status sequence (DESIGN.md, "Storage
    /// operations"): `1b 0b P` and the operation; the proof's steps, the
    /// recovery key typed as its 48 digits and sent as `1c`'s secret, a
    /// disk authorize step's PIN; then, once root's description is the
    /// login worker's, the new keys' steps, or for a removal the commit
    /// over the proof's last step.
    #[test]
    fn every_disk_proof_runs_its_whole_status_sequence() {
        for selection in proven_selections() {
            let screen = Screen::login(selection.clone());
            let steps = steps(&selection);
            let last = steps.last().unwrap();
            let (mut replies, mut calls) = presented(&selection, steps.len());
            let LoginSelection::Proven(proof, _) = &selection else {
                panic!("{selection:?}");
            };
            assert_eq!(calls[0][..3], [0x1b, 0x0b, proof.byte()]);
            replies.extend([
                status(5, last),
                vec![0x94],
                status(3, last),
                status(6, last),
            ]);
            calls.extend([
                vec![0x11],
                description(&[0x14], last),
                vec![0x11],
                vec![0x11],
            ]);
            let (mut client, wire, result) = drive(&screen, replies);
            assert_eq!(result, Ok(()), "{selection:?}");
            assert!(client.login.is_none());
            assert!(client.take_login_end());
            assert_eq!(wire.calls, calls, "{selection:?}");
            let recovery = steps
                .iter()
                .filter(|step| step.login_step() == Some(LoginStep::RecoveryKey))
                .count();
            assert_eq!(
                wire.calls
                    .iter()
                    .filter(|call| call.ends_with(RECOVERY))
                    .count(),
                recovery
            );
            assert_eq!(screen.shown(), Some(Notice::Login(selection.success(None))));
            assert_eq!(screen.attempt.field_raw(), [0; PIN_CAPACITY]);
        }
    }

    /// A disk proof's ends: root's `86` and the volume's `21` before any
    /// description; the storage worker's `20` at either point, `25` only
    /// for the recovery key and `26` only after a description, and its
    /// `28` definite. A proof's enrollment never ends `84`, and an end
    /// root cannot send ends the generation.
    #[test]
    fn a_disk_proofs_ends_show_their_rows_where_root_can_send_them() {
        let add = |proof| LoginSelection::Proven(proof, Box::new(LoginSelection::Add));
        let enroll =
            LoginSelection::Proven(Proof::RecoveryKey, Box::new(LoginSelection::Enroll(1)));
        let first = |selection: &LoginSelection| steps(selection)[0].clone();
        for (selection, described, kind, rows) in [
            (
                add(Proof::RecoveryKey),
                false,
                0x86,
                Some(&["THIS MACHINE HAS NO DISK PROOF"][..]),
            ),
            (
                add(Proof::DiskKey),
                false,
                0x21,
                Some(&["THE DISK CANNOT DO THIS NOW"][..]),
            ),
            (
                add(Proof::DiskKey),
                false,
                0x20,
                Some(&["NO ENCRYPTED TD VOLUME"][..]),
            ),
            (
                add(Proof::RecoveryKey),
                true,
                0x20,
                Some(&["NO ENCRYPTED TD VOLUME"][..]),
            ),
            (
                add(Proof::RecoveryKey),
                true,
                0x25,
                Some(&["RECOVERY KEY MISTYPED"][..]),
            ),
            (
                add(Proof::RecoveryKey),
                true,
                0x26,
                Some(&["WRONG KEY FOR THIS DISK"][..]),
            ),
            (
                add(Proof::DiskKey),
                true,
                0x26,
                Some(&["WRONG KEY FOR THIS DISK"][..]),
            ),
            (
                add(Proof::DiskKey),
                true,
                0x28,
                Some(&["THE DISK CHANGED; NOTHING WRITTEN"][..]),
            ),
            (
                add(Proof::DiskKey),
                true,
                0x04,
                Some(&["THIS KEY IS NOT ENROLLED HERE"][..]),
            ),
            (add(Proof::RecoveryKey), true, 0x86, None),
            (add(Proof::RecoveryKey), true, 0x21, None),
            (add(Proof::DiskKey), true, 0x25, None),
            (add(Proof::RecoveryKey), false, 0x25, None),
            (add(Proof::RecoveryKey), false, 0x26, None),
            (enroll.clone(), false, 0x84, None),
            (
                enroll,
                false,
                0x82,
                Some(&["LOGIN KEYS ALREADY ENROLLED"][..]),
            ),
        ] {
            let screen = Screen::login(selection.clone());
            let mut replies = vec![started(), vec![0x91, 0x0b]];
            let mut end = ended(0x0d, kind, 0);
            if described {
                end.extend_from_slice(&first(&selection).encode());
            }
            replies.push(end);
            let (client, _, result) = drive(&screen, replies);
            match rows {
                Some(rows) => {
                    assert_eq!(result, Ok(()), "{selection:?} {kind:02x}");
                    assert!(client.login.is_none());
                    assert_eq!(screen.shown(), Some(Notice::Login(rows)), "{kind:02x}");
                }
                None => assert!(result.is_err(), "{selection:?} {kind:02x} {described}"),
            }
        }
    }

    /// A description that carries another proof than the one chosen, or
    /// none, never selects a disk proof, and a proof's never selects an
    /// enrolled key's operation.
    #[test]
    fn a_disk_proof_selects_only_its_own_proofs_descriptions() {
        let rk = LoginSelection::Proven(Proof::RecoveryKey, Box::new(LoginSelection::Add));
        let dk = LoginSelection::Proven(Proof::DiskKey, Box::new(LoginSelection::Add));
        let first = steps(&rk)[0].clone();
        assert!(rk.selects(&first));
        assert!(!dk.selects(&first));
        assert!(!LoginSelection::Add.selects(&first));
        assert!(!rk.selects(&steps(&LoginSelection::Add)[0]));
        assert_eq!(rk.request().unwrap(), [0x1b, 0x0b, 0x01, 0x09],);
        assert_eq!(
            LoginSelection::Proven(Proof::DiskKey, Box::new(LoginSelection::Enroll(2)))
                .request()
                .unwrap(),
            [0x1b, 0x0b, 0x02, 0x08, 0x02],
        );
        for inner in [LoginSelection::Unlock, LoginSelection::SelectorUpdate] {
            assert!(LoginSelection::Proven(Proof::RecoveryKey, Box::new(inner))
                .request()
                .is_err());
        }
    }

    // Storage operations (DESIGN.md, "Storage operations").

    fn selector_update(form: SelectorForm) -> Request {
        Request::new(
            NONCE,
            1000,
            Operation::SelectorUpdate {
                form,
                current: "ab".repeat(32),
            },
        )
        .unwrap()
    }

    /// Root's statuses for a selector update to its receipt: `a0 01`, the
    /// wait for the worker's baseline, the description at `03`, its
    /// presentation at one `04` and its receipt, the key typed, at the
    /// next; and the client's requests.
    fn storage_presented(update: &Request) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
        (
            vec![
                [&[0xa0, 1][..], &NONCE].concat(),
                vec![0x91, 0x0b],
                status(3, update),
                status(4, update),
                status(4, update),
                vec![0x93],
            ],
            vec![
                vec![0x20, 1],
                vec![0x11],
                vec![0x11],
                vec![0x11],
                vec![0x11],
                description(&[0x13], update),
            ],
        )
    }

    #[test]
    fn a_selector_update_runs_to_each_forms_success() {
        for (form, rows) in [
            (
                SelectorForm::Staged,
                &[
                    "BOOT SELECTOR UPDATED",
                    "THE NEXT BOOT ASKS FOR THE DISK PIN",
                ][..],
            ),
            (
                SelectorForm::DeviceBound,
                &[
                    "BOOT SELECTOR UPDATED",
                    "THE NEXT BOOT NEEDS THE RECOVERY KEY",
                ],
            ),
            (SelectorForm::Fido2, &["BOOT SELECTOR UPDATED"]),
        ] {
            let update = selector_update(form);
            let screen = Screen::login(LoginSelection::SelectorUpdate);
            let (mut replies, mut calls) = storage_presented(&update);
            replies.extend([status(5, &update), vec![0x94], status(6, &update)]);
            calls.extend([vec![0x11], description(&[0x14], &update), vec![0x11]]);
            let (mut client, wire, result) = drive(&screen, replies);
            assert_eq!(result, Ok(()), "{form:?}");
            assert!(client.login.is_none());
            assert_eq!(wire.calls, calls, "{form:?}");
            assert_eq!(screen.shown(), Some(Notice::Login(rows)));
            // Nothing about login keys changed.
            assert!(!client.take_login_end());
        }
    }

    /// Every end the table names where root can send it, and the
    /// violations: an end where root cannot send it, a second description
    /// and a commit request before the receipt.
    #[test]
    fn a_selector_update_ends_only_where_root_can_end_it() {
        let update = selector_update(SelectorForm::Staged);
        let before: Vec<Vec<u8>> = vec![[&[0xa0, 1][..], &NONCE].concat(), vec![0x91, 0x0b]];
        let (presented, _) = storage_presented(&update);
        let committed: Vec<Vec<u8>> = presented
            .iter()
            .cloned()
            .chain([status(5, &update), vec![0x94]])
            .collect();
        let end = |code: u8, kind: u8, described: bool| {
            let mut bytes = vec![0x91, code, kind, 0];
            if described {
                bytes.extend_from_slice(&update.encode());
            }
            bytes
        };
        let run = |prefix: &[Vec<u8>], last: Vec<u8>| {
            let screen = Screen::login(LoginSelection::SelectorUpdate);
            let replies = prefix.iter().cloned().chain([last]).collect();
            let (client, _, result) = drive(&screen, replies);
            (client.login.is_none(), result, screen.shown())
        };
        for kind in [0x20, 0x21, 0x22] {
            let (ended, result, shown) = run(&before, end(0x0d, kind, false));
            assert!(ended && result.is_ok(), "{kind:02x}");
            assert_eq!(shown, storage_failure(kind, 0).map(Notice::Login));
            // Not once a description exists.
            assert!(run(&presented, end(0x0d, kind, true)).1.is_err());
        }
        for kind in [0x23, 0x24, 0x08, 0x0f, 0x10] {
            let (ended, result, shown) = run(&presented, end(0x0d, kind, true));
            assert!(ended && result.is_ok(), "{kind:02x}");
            assert_eq!(shown, storage_failure(kind, 0).map(Notice::Login));
        }
        // After the commit: `28` is definite, any other end uncertain.
        let (ended, result, shown) = run(&committed, end(0x0d, 0x28, true));
        assert!(ended && result.is_ok());
        assert_eq!(
            shown,
            Some(Notice::Login(&["THE DISK CHANGED; NOTHING WRITTEN"]))
        );
        let (ended, result, shown) = run(&committed, end(0x0e, 0x0e, true));
        assert!(ended && result.is_ok());
        assert_eq!(shown, Some(Notice::Uncertain(&[])));
        assert!(run(&committed, end(0x0d, 0x23, true)).1.is_err());
        // `28` and an uncertain end need the commit.
        assert!(run(&presented, end(0x0d, 0x28, true)).1.is_err());
        assert!(run(&presented, end(0x0e, 0x0e, true)).1.is_err());
        // A kind the table does not name.
        assert!(run(&presented, end(0x0d, 0x25, true)).1.is_err());
        // A second description.
        let other = selector_update(SelectorForm::Fido2);
        assert!(run(&presented, status(3, &other)).1.is_err());
        // A commit request before the receipt.
        assert!(run(&presented[..3], status(5, &update)).1.is_err());
        // A login operation's answer is not a storage operation's.
        let (_, result, _) = run(&[], [&[0x9b, 1][..], &NONCE].concat());
        assert!(result.is_err());
        let (ended, result, shown) = run(&[], vec![0xa0, 0]);
        assert!(ended && result.is_ok());
        assert_eq!(shown, Some(Notice::NotAvailable));
    }

    /// Every storage end's rows fit the narrowest output's chrome.
    #[test]
    fn storage_texts_fit_the_narrowest_chrome() {
        let columns = crate::attention::layout(320, 200).1;
        for kind in 0..=u8::MAX {
            for row in storage_failure(kind, 0).unwrap_or_default() {
                assert!(row.bytes().all(crate::ui::is_mapped), "{row}");
                assert!(row.len() <= columns, "{row}");
            }
        }
        for form in [
            SelectorForm::Staged,
            SelectorForm::DeviceBound,
            SelectorForm::Fido2,
        ] {
            for row in LoginSelection::SelectorUpdate.success(Some(&selector_update(form))) {
                assert!(row.len() <= columns, "{row}");
            }
        }
        // Protector management's ends and successes; a request's success
        // ends with the next-boot rows its description showed.
        for selection in protectors() {
            let LoginSelection::Protector(managed) = selection else {
                panic!("{selection:?}")
            };
            for kind in 0..=u8::MAX {
                for detail in 0..=7 {
                    for row in protector_failure(managed, kind, detail).unwrap_or_default() {
                        assert!(row.bytes().all(crate::ui::is_mapped), "{row}");
                        assert!(row.len() <= columns, "{row}");
                    }
                }
            }
            let success = selection.success(None);
            for row in success {
                assert!(row.bytes().all(crate::ui::is_mapped), "{row}");
                assert!(row.len() <= columns, "{row}");
            }
            if let Managed::Request { .. } = managed {
                let lines = steps(&selection)[0].lines();
                let next_boot: Vec<&str> = lines
                    .iter()
                    .map(String::as_str)
                    .skip_while(|line| !line.starts_with("THE NEXT BOOT"))
                    .take_while(|line| {
                        !line.starts_with("REPLACES") && !line.starts_with("AUTHORIZED BY")
                    })
                    .collect();
                assert_eq!(next_boot, success[1..], "{managed:?}");
            }
        }
    }

    // Protector management (DESIGN.md, "Protector management").

    const PRIMARY: Fingerprint = [0x51; 4];
    const SECOND: Fingerprint = [0x52; 4];
    const SPARE: Fingerprint = [0x53; 4];
    const MADE: Fingerprint = [0x0e; 4];

    /// The FIDO2 shape's list: two primary keys and a recovery key.
    fn listed(chosen: u8) -> crate::authority::consent::Removal {
        use crate::authority::consent::{DiskRole, DiskToken, Removal};
        let token = |role, key| DiskToken { role, key };
        Removal {
            shape: UpgradeShape::Fido2,
            marker: true,
            tpm_pin: false,
            tokens: vec![
                token(DiskRole::Primary, PRIMARY),
                token(DiskRole::Primary, SECOND),
                token(DiskRole::Recovery, SPARE),
            ],
            chosen,
        }
    }

    /// Each protector operation's steps in their only order: the chosen
    /// position is the typed `3`, and a disk key authorizes with the
    /// second primary key.
    fn protector_steps(managed: Managed) -> Vec<Request> {
        let authorize = LoginStep::DiskAuthorize {
            key: SECOND,
            retries: 8,
        };
        let proof = |proof| match proof {
            Proof::RecoveryKey => vec![LoginStep::RecoveryKey],
            Proof::DiskKey => vec![LoginStep::DiskIdentify, authorize],
        };
        match managed {
            Managed::Add { proof: by, role } => proof(by)
                .into_iter()
                .chain(new_key(MADE))
                .map(|step| {
                    login(Operation::ProtectorAdd {
                        account: 1000,
                        proof: by,
                        role,
                        step,
                    })
                })
                .collect(),
            Managed::Remove { proof: by } => std::iter::once((0, LoginStep::Choose))
                .chain(proof(by).into_iter().map(|step| (3, step)))
                .map(|(chosen, step)| {
                    login(Operation::ProtectorRemove {
                        account: 1000,
                        proof: by,
                        removal: listed(chosen),
                        step,
                    })
                })
                .collect(),
            Managed::RecoveryRemove => vec![
                login(Operation::RecoveryRemove {
                    account: 1000,
                    step: LoginStep::DiskIdentify,
                }),
                login(Operation::RecoveryRemove {
                    account: 1000,
                    step: authorize,
                }),
            ],
            // A TPM clear replaces a pending PIN change.
            Managed::Request { proof: by, request } => proof(by)
                .into_iter()
                .map(|step| {
                    login(Operation::ProtectorRequest {
                        account: 1000,
                        proof: by,
                        request,
                        pending: (request == DiskRequest::TpmClear)
                            .then_some(DiskRequest::PinChange),
                        step,
                    })
                })
                .collect(),
        }
    }

    fn protectors() -> Vec<LoginSelection> {
        use crate::authority::consent::DiskRole;
        [
            Managed::Add {
                proof: Proof::RecoveryKey,
                role: DiskRole::Recovery,
            },
            Managed::Add {
                proof: Proof::DiskKey,
                role: DiskRole::Primary,
            },
            Managed::Remove {
                proof: Proof::DiskKey,
            },
            Managed::Remove {
                proof: Proof::RecoveryKey,
            },
            Managed::RecoveryRemove,
            Managed::Request {
                proof: Proof::RecoveryKey,
                request: DiskRequest::PinChange,
            },
            Managed::Request {
                proof: Proof::DiskKey,
                request: DiskRequest::TpmClear,
            },
            Managed::Request {
                proof: Proof::DiskKey,
                request: DiskRequest::Reenroll,
            },
        ]
        .map(LoginSelection::Protector)
        .into()
    }

    /// `20 04` to `20 09`: every step presented, tag 21's keyed, each
    /// secret step's `1c`, a removal's chosen digit among them, the commit
    /// over the last step, and the success rows; nothing about login keys
    /// changed.
    #[test]
    fn protector_management_runs_its_whole_status_sequences() {
        for selection in protectors() {
            let screen = Screen::login(selection.clone());
            let steps = steps(&selection);
            let last = steps.last().unwrap();
            let (mut replies, mut calls) = presented(&selection, steps.len());
            replies.extend([
                status(5, last),
                vec![0x94],
                status(3, last),
                status(6, last),
            ]);
            calls.extend([
                vec![0x11],
                description(&[0x14], last),
                vec![0x11],
                vec![0x11],
            ]);
            let (mut client, wire, result) = drive(&screen, replies);
            assert_eq!(result, Ok(()), "{selection:?}");
            assert!(client.login.is_none());
            assert!(!client.take_login_end());
            assert_eq!(wire.calls, calls, "{selection:?}");
            let secrets = steps.iter().filter(|step| asks_pin(step)).count();
            assert_eq!(sent(&wire, 0x1c), secrets, "{selection:?}");
            // Only tag 21's identify step awaits approval, as a login
            // disclosure.
            let disclosing = selection == LoginSelection::Protector(Managed::RecoveryRemove);
            assert_eq!(steps[0].awaits_approval(), disclosing);
            assert_eq!(screen.shown(), Some(Notice::Login(selection.success(None))));
            assert_eq!(screen.attempt.field_raw(), [0; PIN_CAPACITY]);
        }
        use crate::authority::consent::DiskRole;
        for (managed, bytes) in [
            (
                Managed::Add {
                    proof: Proof::DiskKey,
                    role: DiskRole::Recovery,
                },
                &[0x20, 4, 2, 2][..],
            ),
            (
                Managed::Remove {
                    proof: Proof::RecoveryKey,
                },
                &[0x20, 5, 1],
            ),
            (Managed::RecoveryRemove, &[0x20, 6]),
            (
                Managed::Request {
                    proof: Proof::DiskKey,
                    request: DiskRequest::TpmClear,
                },
                &[0x20, 8, 2],
            ),
        ] {
            assert_eq!(
                LoginSelection::Protector(managed).request(),
                Ok(bytes.to_vec())
            );
        }
        assert_eq!(
            LoginSelection::Protector(Managed::RecoveryRemove).ceiling(),
            Duration::from_secs(360)
        );
    }

    /// Each end where root can send it, with its rows, and the violations:
    /// an end where it cannot, another operation's first description, an
    /// unkeyed tag 21 and a removal's step past another position.
    #[test]
    fn protector_management_ends_only_where_root_can_end_it() {
        let run = |selection: &LoginSelection, prefix: &[Vec<u8>], last: Vec<u8>| {
            let screen = Screen::login(selection.clone());
            let replies = prefix.iter().cloned().chain([last]).collect();
            let (client, _, result) = drive(&screen, replies);
            (client.login.is_none() && result.is_ok(), screen.shown())
        };
        // A status that does not end the operation: whether the client
        // refused it.
        let refused = |selection: &LoginSelection, prefix: &[Vec<u8>], last: Vec<u8>| {
            let screen = Screen::login(selection.clone());
            let replies = prefix.iter().cloned().chain([last]).collect();
            drive(&screen, replies).2.is_err()
        };
        let end = |code: u8, kind: u8, detail: u8, step: Option<&Request>| {
            let mut bytes = vec![0x91, code, kind, detail];
            if let Some(step) = step {
                bytes.extend_from_slice(&step.encode());
            }
            bytes
        };
        for selection in protectors() {
            let LoginSelection::Protector(managed) = selection else {
                panic!("{selection:?}")
            };
            let shows = |kind, detail| protector_failure(managed, kind, detail).map(Notice::Login);
            let steps = steps(&selection);
            let first = &steps[0];
            let before = presented(&selection, 0).0[..2].to_vec();
            let described = presented(&selection, 0).0;
            let refusal = match managed {
                Managed::Add { .. } => 1,
                Managed::Request { .. } => 3,
                _ => 2,
            };
            for detail in 0..=3 {
                let ended = run(&selection, &before, end(0x0d, 0x21, detail, None));
                if detail == 0 || detail == refusal {
                    assert_eq!(ended, (true, shows(0x21, detail)), "{detail}");
                } else {
                    assert!(!ended.0, "{selection:?} {detail}");
                }
                assert!(!run(&selection, &described, end(0x0d, 0x21, detail, Some(first))).0);
            }
            assert_eq!(
                run(&selection, &before, end(0x0d, 0x20, 0, None)),
                (true, shows(0x20, 0))
            );
            // No `28` before a description: root has read nothing to test.
            assert!(!run(&selection, &before, end(0x0d, 0x28, 0, None)).0);
            // No commit asked for before the last step.
            if steps.len() > 1 {
                let mut worked = presented(&selection, 1).0;
                worked.push(status(5, first));
                let (_, wire, result) = drive(&Screen::login(selection.clone()), worked);
                assert!(result.is_err() && sent(&wire, 0x14) == 0, "{selection:?}");
            }
            let removal = matches!(managed, Managed::Remove { .. });
            for (kind, detail) in [(0x20, 0), (0x28, 0)] {
                assert_eq!(
                    run(&selection, &described, end(0x0d, kind, detail, Some(first))),
                    (true, shows(kind, detail)),
                    "{selection:?} {kind:02x}"
                );
            }
            // A proof's kinds from its first step: a removal's is after
            // the choose step, at which none stands.
            let (proving, proof_step) = if removal {
                (presented(&selection, 1).0, &steps[1])
            } else {
                (described.clone(), first)
            };
            for (kind, detail) in [(0x26, 0), (0x01, 7), (0x05, 0)] {
                assert_eq!(
                    run(
                        &selection,
                        &proving,
                        end(0x0d, kind, detail, Some(proof_step))
                    ),
                    (true, shows(kind, detail)),
                    "{selection:?} {kind:02x}"
                );
            }
            if removal {
                for (kind, detail) in [(0x26, 0), (0x01, 7), (0x05, 0), (0x04, 1), (0x25, 0)] {
                    assert!(!run(&selection, &described, end(0x0d, kind, detail, Some(first))).0);
                }
            }
            let recovery = matches!(
                managed,
                Managed::Add {
                    proof: Proof::RecoveryKey,
                    ..
                } | Managed::Remove {
                    proof: Proof::RecoveryKey
                } | Managed::Request {
                    proof: Proof::RecoveryKey,
                    ..
                }
            );
            for (kind, detail, stands) in [
                (0x25, 0, recovery),
                (0x04, 1, removal),
                // A new key's at a proof step, and `0e` before a commit.
                (0x11, 0, false),
                (0x0e, 0, false),
            ] {
                assert_eq!(
                    run(
                        &selection,
                        &proving,
                        end(0x0d, kind, detail, Some(proof_step))
                    )
                    .0,
                    stands,
                    "{selection:?} {kind:02x}"
                );
            }
            // An addition's new key's kinds at its create step.
            if let Some(create) = steps
                .iter()
                .position(|step| matches!(step.protector_step(), Some(LoginStep::Create { .. })))
            {
                let (keying, _) = presented(&selection, create);
                let step = &steps[create];
                for (kind, detail) in [(0x11, 0), (0x06, 6), (0x07, 0)] {
                    assert_eq!(
                        run(&selection, &keying, end(0x0d, kind, detail, Some(step))),
                        (true, shows(kind, detail)),
                        "{kind:02x}"
                    );
                }
            }
            // After the commit: a definite `28`, or uncertain with its rows.
            let last = steps.last().unwrap();
            let (mut committed, _) = presented(&selection, steps.len());
            committed.extend([status(5, last), vec![0x94]]);
            assert_eq!(
                run(&selection, &committed, end(0x0d, 0x28, 0, Some(last))),
                (true, shows(0x28, 0))
            );
            let uncertain: &[&str] = match managed {
                Managed::Add { .. } => {
                    &["THE NEW KEY MAY OPEN THE DISK", "EVEN IF IT IS NOT LISTED"]
                }
                _ => &[],
            };
            assert_eq!(
                run(&selection, &committed, end(0x0e, 0x0e, 0, Some(last))),
                (true, Some(Notice::Uncertain(uncertain)))
            );
            // After the commit every end is uncertain but a `28`, and no
            // proof's kind stands there.
            for (kind, detail) in [(0x26, 0), (0x04, 1), (0x20, 0), (0x11, 0), (0x01, 7)] {
                assert!(
                    !run(&selection, &committed, end(0x0e, kind, detail, Some(last))).0,
                    "{selection:?} {kind:02x}"
                );
            }
            // Another operation's first description.
            let other = if removal {
                protector_steps(Managed::RecoveryRemove)
            } else {
                protector_steps(Managed::Remove {
                    proof: Proof::DiskKey,
                })
            };
            assert!(refused(&selection, &before, status(3, &other[0])));
            assert!(!refused(&selection, &before, status(3, first)));
        }
        // A removal's next step past another position than the typed one.
        let removal = LoginSelection::Protector(Managed::Remove {
            proof: Proof::DiskKey,
        });
        let (chosen, _) = presented(&removal, 1);
        let elsewhere = login(Operation::ProtectorRemove {
            account: 1000,
            proof: Proof::DiskKey,
            removal: listed(2),
            step: LoginStep::DiskIdentify,
        });
        assert!(refused(&removal, &chosen, status(3, &elsewhere)));
        assert!(!refused(&removal, &chosen, status(3, &steps(&removal)[1])));
        // A first description of the same kind that changes the role, the
        // proof or the first step.
        use crate::authority::consent::DiskRole;
        let add = |proof, role, step| {
            login(Operation::ProtectorAdd {
                account: 1000,
                proof,
                role,
                step,
            })
        };
        let primary = LoginSelection::Protector(Managed::Add {
            proof: Proof::RecoveryKey,
            role: DiskRole::Primary,
        });
        let before = presented(&primary, 0).0[..2].to_vec();
        assert!(!refused(
            &primary,
            &before,
            status(
                3,
                &add(
                    Proof::RecoveryKey,
                    DiskRole::Primary,
                    LoginStep::RecoveryKey
                )
            )
        ));
        for other in [
            add(
                Proof::RecoveryKey,
                DiskRole::Recovery,
                LoginStep::RecoveryKey,
            ),
            add(Proof::DiskKey, DiskRole::Primary, LoginStep::DiskIdentify),
            add(Proof::RecoveryKey, DiskRole::Primary, LoginStep::Connect),
        ] {
            assert!(refused(&primary, &before, status(3, &other)), "{other:?}");
        }
        let before = presented(&removal, 0).0[..2].to_vec();
        assert!(refused(&removal, &before, status(3, &steps(&removal)[1])));
        // A request's that changes the request, the proof or the step.
        let request = |proof, request, step| {
            login(Operation::ProtectorRequest {
                account: 1000,
                proof,
                request,
                pending: None,
                step,
            })
        };
        let pin_change = LoginSelection::Protector(Managed::Request {
            proof: Proof::RecoveryKey,
            request: DiskRequest::PinChange,
        });
        let requested = presented(&pin_change, 0).0[..2].to_vec();
        assert!(!refused(
            &pin_change,
            &requested,
            status(
                3,
                &request(
                    Proof::RecoveryKey,
                    DiskRequest::PinChange,
                    LoginStep::RecoveryKey
                )
            )
        ));
        for other in [
            request(
                Proof::RecoveryKey,
                DiskRequest::TpmClear,
                LoginStep::RecoveryKey,
            ),
            request(
                Proof::DiskKey,
                DiskRequest::PinChange,
                LoginStep::DiskIdentify,
            ),
        ] {
            assert!(
                refused(&pin_change, &requested, status(3, &other)),
                "{other:?}"
            );
        }
        let by_key = LoginSelection::Protector(Managed::Request {
            proof: Proof::DiskKey,
            request: DiskRequest::PinChange,
        });
        let requested = presented(&by_key, 0).0[..2].to_vec();
        let authorize = request(
            Proof::DiskKey,
            DiskRequest::PinChange,
            LoginStep::DiskAuthorize {
                key: SECOND,
                retries: 8,
            },
        );
        assert!(refused(&by_key, &requested, status(3, &authorize)));
        let by_recovery = login(Operation::ProtectorRemove {
            account: 1000,
            proof: Proof::RecoveryKey,
            removal: listed(0),
            step: LoginStep::Choose,
        });
        assert!(refused(&removal, &before, status(3, &by_recovery)));
        // A new key that is the key authorizing its addition.
        let by_key = LoginSelection::Protector(Managed::Add {
            proof: Proof::DiskKey,
            role: DiskRole::Primary,
        });
        let steps = steps(&by_key);
        let create = steps
            .iter()
            .position(|step| matches!(step.protector_step(), Some(LoginStep::Create { .. })))
            .unwrap();
        let (created, _) = presented(&by_key, create + 1);
        let prove = |key| {
            login(Operation::ProtectorAdd {
                account: 1000,
                proof: Proof::DiskKey,
                role: DiskRole::Primary,
                step: LoginStep::Prove { key, retries: 8 },
            })
        };
        assert_eq!(steps[create + 1], prove(MADE));
        assert!(!refused(&by_key, &created, status(3, &prove(MADE))));
        assert!(refused(&by_key, &created, status(3, &prove(SECOND))));
    }

    /// A choose step's field: a digit naming a removable position types and
    /// submits at once; any other digit, byte, Erase or Enter changes
    /// nothing.
    #[test]
    fn the_choose_profile_takes_one_removable_digit() {
        let choose = login(Operation::ProtectorRemove {
            account: 1000,
            proof: Proof::DiskKey,
            removal: listed(0),
            step: LoginStep::Choose,
        });
        // Every listed key is removable: two primaries and the marker.
        assert_eq!(Profile::of(&choose), Profile::Choose(0b111));
        // Without the marker the only primary key is needed.
        use crate::authority::consent::{DiskRole, DiskToken, Removal};
        let needed = login(Operation::ProtectorRemove {
            account: 1000,
            proof: Proof::DiskKey,
            removal: Removal {
                shape: UpgradeShape::Fido2,
                marker: false,
                tpm_pin: false,
                tokens: vec![
                    DiskToken {
                        role: DiskRole::Primary,
                        key: PRIMARY,
                    },
                    DiskToken {
                        role: DiskRole::Recovery,
                        key: SECOND,
                    },
                    DiskToken {
                        role: DiskRole::Recovery,
                        key: SPARE,
                    },
                ],
                chosen: 0,
            },
            step: LoginStep::Choose,
        });
        assert_eq!(Profile::of(&needed), Profile::Choose(0b110));
        // A fourth protector is a position too.
        let mut four = listed(0);
        four.tokens.push(DiskToken {
            role: DiskRole::Recovery,
            key: MADE,
        });
        let four = login(Operation::ProtectorRemove {
            account: 1000,
            proof: Proof::DiskKey,
            removal: four,
            step: LoginStep::Choose,
        });
        assert_eq!(Profile::of(&four), Profile::Choose(0b1111));
        let digit = |digit| FieldKey::Digit { byte: digit, digit };
        let mut field = PinField::default();
        field.open_for(Profile::Choose(0b101));
        for ignored in [
            digit(b'2'),
            digit(b'4'),
            digit(b'0'),
            FieldKey::Byte(b'1'),
            FieldKey::Erase,
            FieldKey::Submit,
        ] {
            assert_eq!(field.key(ignored), None);
        }
        assert!(!field.submitted());
        assert_eq!(field.key(digit(b'3')), Some(1));
        assert!(field.submitted());
        assert_eq!(field.key(digit(b'1')), None);
        let mut into = [0; 4];
        assert_eq!(field.take(&mut into), Some(1));
        assert_eq!(into[0], b'3');
        assert_eq!(field.raw(), [0; PIN_CAPACITY]);
        // A recovery-key step of protector management takes 48 digits.
        let recovery = &protector_steps(Managed::Add {
            proof: Proof::RecoveryKey,
            role: crate::authority::consent::DiskRole::Recovery,
        })[0];
        assert_eq!(Profile::of(recovery), Profile::Recovery);
    }

    // The upgrade and its cancellation (DESIGN.md, "The upgrade" and "The
    // cancellation").

    fn upgrade_at(shape: UpgradeShape, count: u8, step: UpgradeStep) -> Request {
        login(Operation::Upgrade {
            account: 1000,
            shape,
            count,
            current: "ab".repeat(32),
            step,
        })
    }
    fn upgrade_cancel() -> Request {
        login(Operation::UpgradeCancel)
    }
    fn upgrades() -> Vec<LoginSelection> {
        [
            (UpgradeShape::Tpm, 0),
            (UpgradeShape::Tpm, 2),
            (UpgradeShape::Fido2, 1),
            (UpgradeShape::Fido2, 2),
        ]
        .map(|(shape, count)| LoginSelection::Upgrade { shape, count })
        .into()
    }

    /// `20 02 S N` and `20 03`: the review keyed, each new key's ceremony
    /// with a `1c` for its create, prove and repeat steps, the commit over
    /// the last step, and the success rows; nothing about login keys
    /// changed.
    #[test]
    fn an_upgrade_and_a_cancellation_run_their_whole_status_sequences() {
        let mut selections = upgrades();
        selections.push(LoginSelection::UpgradeCancel);
        for selection in selections {
            let screen = Screen::login(selection.clone());
            let steps = steps(&selection);
            let last = steps.last().unwrap();
            let (mut replies, mut calls) = presented(&selection, steps.len());
            replies.extend([
                status(5, last),
                vec![0x94],
                status(3, last),
                status(6, last),
            ]);
            calls.extend([
                vec![0x11],
                description(&[0x14], last),
                vec![0x11],
                vec![0x11],
            ]);
            let (mut client, wire, result) = drive(&screen, replies);
            assert_eq!(result, Ok(()), "{selection:?}");
            assert!(client.login.is_none());
            assert!(!client.take_login_end());
            assert_eq!(wire.calls, calls, "{selection:?}");
            let keys = match &selection {
                LoginSelection::Upgrade { count, .. } => usize::from(*count),
                _ => 0,
            };
            assert_eq!(sent(&wire, 0x1c), 3 * keys, "{selection:?}");
            assert_eq!(sent(&wire, 0x13), steps.len());
            assert_eq!(screen.shown(), Some(Notice::Login(selection.success(None))));
            assert_eq!(screen.attempt.field_raw(), [0; PIN_CAPACITY]);
        }
        assert_eq!(
            LoginSelection::Upgrade {
                shape: UpgradeShape::Fido2,
                count: 2
            }
            .request(),
            Ok(vec![0x20, 2, 2, 2])
        );
        assert_eq!(LoginSelection::UpgradeCancel.request(), Ok(vec![0x20, 3]));
        for (shape, count) in [(UpgradeShape::Tpm, 3), (UpgradeShape::Fido2, 0)] {
            assert!(LoginSelection::Upgrade { shape, count }.request().is_err());
        }
    }

    /// Each end where root can send it, and the violations: an end where it
    /// cannot, a review for another shape or count, a step out of order and
    /// a commit before the last step.
    #[test]
    fn an_upgrade_ends_only_where_root_can_end_it() {
        let run = |selection: &LoginSelection, prefix: &[Vec<u8>], last: Vec<u8>| {
            let screen = Screen::login(selection.clone());
            let replies = prefix.iter().cloned().chain([last]).collect();
            let (client, _, result) = drive(&screen, replies);
            (client.login.is_none() && result.is_ok(), screen.shown())
        };
        let end = |code: u8, kind: u8, detail: u8, step: Option<&Request>| {
            let mut bytes = vec![0x91, code, kind, detail];
            if let Some(step) = step {
                bytes.extend_from_slice(&step.encode());
            }
            bytes
        };
        for selection in upgrades() {
            let LoginSelection::Upgrade { shape, count } = selection else {
                panic!("{selection:?}")
            };
            let steps = steps(&selection);
            let review = &steps[0];
            let before = presented(&selection, 0).0[..2].to_vec();
            // The review described, presented and not yet received.
            let reviewing = presented(&selection, 0).0;
            let shows = |kind, detail| upgrade_failure(kind, detail).map(Notice::Login);
            for kind in [0x87, 0x22, 0x20, 0x21] {
                assert_eq!(
                    run(&selection, &before, end(0x0d, kind, 0, None)),
                    (true, shows(kind, 0)),
                    "{kind:02x}"
                );
            }
            for kind in [0x87, 0x22] {
                assert!(!run(&selection, &reviewing, end(0x0d, kind, 0, Some(review))).0);
            }
            for (kind, detail) in [(0x20, 0), (0x21, 0), (0x23, 0), (0x24, 0), (0x28, 0)] {
                assert_eq!(
                    run(
                        &selection,
                        &reviewing,
                        end(0x0d, kind, detail, Some(review))
                    ),
                    (true, shows(kind, detail)),
                    "{kind:02x}"
                );
            }
            for detail in 1..=3 {
                let tpm = shape == UpgradeShape::Tpm;
                let (ended, shown) = run(
                    &selection,
                    &reviewing,
                    end(0x0d, 0x27, detail, Some(review)),
                );
                assert_eq!(ended, tpm, "{shape:?}");
                if tpm {
                    assert_eq!(shown, shows(0x27, detail));
                }
            }
            // A new key's kinds need its ceremony.
            assert!(!run(&selection, &reviewing, end(0x0d, 0x11, 0, Some(review))).0);
            assert!(!run(&selection, &reviewing, end(0x0d, 0x01, 7, Some(review))).0);
            if count > 0 {
                let (keying, _) = presented(&selection, 2);
                let create = &steps[2];
                for (kind, detail) in [(0x01, 7), (0x05, 0), (0x06, 6), (0x07, 0), (0x11, 0)] {
                    assert_eq!(
                        run(&selection, &keying, end(0x0d, kind, detail, Some(create))),
                        (true, shows(kind, detail)),
                        "{kind:02x}"
                    );
                }
                // The reads' kinds are the review's alone.
                for kind in [0x20, 0x21, 0x23, 0x24, 0x28] {
                    assert!(
                        !run(&selection, &keying, end(0x0d, kind, 0, Some(create))).0,
                        "{kind:02x}"
                    );
                }
                // A step out of order: the create before its connect.
                let (mut skipped, _) = presented(&selection, 1);
                skipped.pop();
                assert!(!run(&selection, &skipped, status(3, create)).0);
                // A commit before the last step.
                let (keyed, _) = presented(&selection, 3);
                assert!(!run(&selection, &keyed, status(5, &steps[2])).0);
            }
            // After the commit: a definite `28`, or uncertain with its rows.
            let last = steps.last().unwrap();
            let (mut committed, _) = presented(&selection, steps.len());
            committed.extend([status(5, last), vec![0x94]]);
            assert_eq!(
                run(&selection, &committed, end(0x0d, 0x28, 0, Some(last))),
                (true, shows(0x28, 0))
            );
            assert_eq!(
                run(&selection, &committed, end(0x0e, 0x0e, 0, Some(last))),
                (
                    true,
                    Some(Notice::Uncertain(&[
                        "THE NEXT BOOT MAY NEED",
                        "THE RECOVERY KEY"
                    ]))
                )
            );
            assert!(!run(&selection, &committed, end(0x0d, 0x23, 0, Some(last))).0);
            // A review for another count.
            let other = upgrade_at(shape, if count == 2 { 1 } else { 2 }, UpgradeStep::Review);
            assert!(!run(&selection, &before, status(3, &other)).0);
            let cancel = upgrade_cancel();
            assert!(!run(&selection, &before, status(3, &cancel)).0);
        }
    }

    /// A cancellation's ends: `20` before its commit, `21` and a definite
    /// `28` before its receipt, `28` and `0e` after the commit, and one
    /// description.
    #[test]
    fn a_cancellation_ends_only_where_root_can_end_it() {
        let selection = LoginSelection::UpgradeCancel;
        let cancel = upgrade_cancel();
        let run = |prefix: &[Vec<u8>], last: Vec<u8>| {
            let screen = Screen::login(selection.clone());
            let replies = prefix.iter().cloned().chain([last]).collect();
            let (client, _, result) = drive(&screen, replies);
            (client.login.is_none() && result.is_ok(), screen.shown())
        };
        let end = |code: u8, kind: u8, described: bool| {
            let mut bytes = vec![0x91, code, kind, 0];
            if described {
                bytes.extend_from_slice(&cancel.encode());
            }
            bytes
        };
        let shows = |kind| storage_failure(kind, 0).map(Notice::Login);
        let (described, _) = presented(&selection, 0);
        let before = described[..2].to_vec();
        let (received, _) = presented(&selection, 1);
        let mut committed = received.clone();
        committed.extend([status(5, &cancel), vec![0x94]]);
        for kind in [0x20, 0x21] {
            assert_eq!(run(&before, end(0x0d, kind, false)), (true, shows(kind)));
            assert_eq!(run(&described, end(0x0d, kind, true)), (true, shows(kind)));
        }
        assert_eq!(run(&described, end(0x0d, 0x28, true)), (true, shows(0x28)));
        assert_eq!(run(&received, end(0x0d, 0x20, true)), (true, shows(0x20)));
        assert!(!run(&received, end(0x0d, 0x21, true)).0);
        assert!(!run(&received, end(0x0d, 0x28, true)).0);
        assert!(!run(&before, end(0x0d, 0x22, false)).0);
        assert_eq!(run(&committed, end(0x0d, 0x28, true)), (true, shows(0x28)));
        assert_eq!(
            run(&committed, end(0x0e, 0x0e, true)),
            (true, Some(Notice::Uncertain(&[])))
        );
        assert!(!run(&committed, end(0x0d, 0x20, true)).0);
        // One description.
        assert!(!run(&received, status(3, &upgrade_cancel_other())).0);
    }
    /// Another cancellation, under another nonce.
    fn upgrade_cancel_other() -> Request {
        Request::new([7; 32], 1000, Operation::UpgradeCancel).unwrap()
    }

    /// Every upgrade end's and success's rows fit the narrowest chrome,
    /// and its own are DESIGN.md's.
    #[test]
    fn upgrade_texts_fit_the_narrowest_chrome() {
        let tpm = |reason| Some(vec!["NO USABLE TPM:", reason, "CHOOSE SECURITY KEYS ONLY"]);
        assert_eq!(
            upgrade_failure(0x27, 1).map(<[_]>::to_vec),
            tpm("NONE FOUND")
        );
        assert_eq!(
            upgrade_failure(0x27, 2).map(<[_]>::to_vec),
            tpm("NO SHA-256 PCR BANK")
        );
        assert_eq!(
            upgrade_failure(0x27, 3).map(<[_]>::to_vec),
            tpm("ITS LOCKOUT IS ALREADY SET")
        );
        assert_eq!(upgrade_failure(0x27, 4), None);
        assert_eq!(
            upgrade_failure(0x87, 0),
            Some(&["NEEDS AN ENROLLED LOGIN KEY"][..])
        );
        assert_eq!(
            upgrade_failure(0x22, 0),
            Some(
                &[
                    "RESTART INTO THE CURRENT SYSTEM",
                    "BEFORE PROTECTING THE DISK"
                ][..]
            )
        );
        assert_eq!(upgrade_failure(0x11, 0), login_failure(0x11, 0));
        assert_eq!(upgrade_failure(0x23, 0), storage_failure(0x23, 0));
        assert_eq!(
            LoginSelection::Upgrade {
                shape: UpgradeShape::Tpm,
                count: 0
            }
            .success(None),
            [
                "THE NEXT BOOT PROTECTS THE DISK",
                "HAVE THE RECOVERY KEY READY"
            ]
        );
        assert_eq!(
            LoginSelection::UpgradeCancel.success(None),
            [
                "DISK PROTECTION CANCELLED",
                "THE NEXT BOOT MAY NEED",
                "THE RECOVERY KEY"
            ]
        );
        let columns = crate::attention::layout(320, 200).1;
        for kind in 0..=u8::MAX {
            for detail in 0..=8 {
                for row in upgrade_failure(kind, detail).unwrap_or_default() {
                    assert!(row.bytes().all(crate::ui::is_mapped), "{row}");
                    assert!(row.len() <= columns, "{row}");
                }
            }
        }
        let mut selections = upgrades();
        selections.push(LoginSelection::UpgradeCancel);
        for selection in selections {
            for row in selection.success(None) {
                assert!(row.len() <= columns, "{row}");
            }
        }
    }

    /// A disclosure's approving Enter confirms only its own prompt: the
    /// next login step's presentation clears the confirmation with it.
    #[test]
    fn a_disclosures_approval_does_not_outlive_its_prompt() {
        let origin = crate::input::test_origin();
        let selection = LoginSelection::Enroll(2);
        let screen = Screen::login(selection.clone());
        let steps = steps(&selection);
        assert_eq!(
            screen.attempt.present(steps[0].clone()),
            Ok(steps[0].clone())
        );
        let after = screen
            .attempt
            .presentation
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .after;
        assert!(!screen
            .attempt
            .approve(&origin, ApprovalPress::Enter, after + APPROVAL_SETTLE)
            .unwrap());
        assert!(confirmed(&screen) && screen.attempt.approved(&steps[0]));
        assert_eq!(
            screen.attempt.present(steps[1].clone()),
            Ok(steps[1].clone())
        );
        assert!(!confirmed(&screen));
        assert!(!screen.attempt.approved(&steps[0]));
        assert!(screen.attempt.presentation.lock().unwrap().is_none());
    }

    /// A login disclosure step's receipt follows only an approving Enter
    /// (TOKEN-LOGIN.md increment 5's A4): root's `04` polls draw nothing
    /// until then, the whole-disk installation's Enter confirms nothing,
    /// and a press stamped at the prompt's own sample counts for nothing.
    /// The receipt then carries the exact description, and the next
    /// step's follows its presentation. Another key first, or an Enter
    /// inside the settle, cancels instead.
    #[test]
    fn a_disclosure_steps_receipt_waits_for_an_approving_enter() {
        let origin = crate::input::test_origin();
        for selection in [
            LoginSelection::Enroll(1),
            LoginSelection::Enroll(2),
            LoginSelection::Remove(removal(), 1),
        ] {
            let screen = Screen::login(selection.clone());
            let steps = steps(&selection);
            let first = &steps[0];
            assert!(first.awaits_approval());
            assert!(!steps[1].awaits_approval());
            let (mut replies, _) = presented(&selection, 0);
            replies.extend([status(4, first), status(4, first)]);
            let mut root = wire(replies);
            let mut client = login_client();
            client
                .start(&mut root, Arc::clone(&screen.attempt))
                .unwrap();
            while !root.replies.is_empty() {
                client.tick(&mut root).unwrap();
            }
            assert_eq!(sent(&root, 0x13), 0, "{selection:?}");
            let after = screen
                .attempt
                .presentation
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .after;
            screen
                .attempt
                .confirm_install(&origin, after + APPROVAL_SETTLE)
                .unwrap();
            assert!(!screen
                .attempt
                .approve(&origin, ApprovalPress::Enter, after)
                .unwrap());
            root.replies.push_back(status(4, first));
            client.tick(&mut root).unwrap();
            assert_eq!(sent(&root, 0x13), 0);
            assert!(!screen
                .attempt
                .approve(&origin, ApprovalPress::Enter, after + APPROVAL_SETTLE)
                .unwrap());
            root.replies.extend([
                status(4, first),
                vec![0x93],
                status(4, &steps[1]),
                vec![0x93],
            ]);
            client.tick(&mut root).unwrap();
            client.tick(&mut root).unwrap();
            assert!(root.replies.is_empty());
            assert_eq!(
                root.calls
                    .iter()
                    .filter(|call| call.first() == Some(&0x13))
                    .collect::<Vec<_>>(),
                [
                    &description(&[0x13], first),
                    &description(&[0x13], &steps[1])
                ]
            );
            // Another key first, or an Enter inside the settle, ends it
            // with `15`.
            for (press, at) in [
                (ApprovalPress::Other, APPROVAL_SETTLE),
                (ApprovalPress::Enter, APPROVAL_SETTLE - 1),
            ] {
                let screen = Screen::login(selection.clone());
                let (mut replies, _) = presented(&selection, 0);
                replies.push(status(4, first));
                let mut wire = wire(replies);
                let mut client = login_client();
                client
                    .start(&mut wire, Arc::clone(&screen.attempt))
                    .unwrap();
                while !wire.replies.is_empty() {
                    client.tick(&mut wire).unwrap();
                }
                let after = screen
                    .attempt
                    .presentation
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .after;
                assert!(screen.attempt.approve(&origin, press, after + at).unwrap());
                wire.replies.extend([vec![0x95, 0], status(3, first)]);
                client.tick(&mut wire).unwrap();
                assert_eq!(sent(&wire, 0x15), 1);
                assert_eq!(sent(&wire, 0x13), 0);
            }
        }
    }

    /// Root starts every login selection with its nonce; anything else,
    /// the refusal `9b 00` that preceded activation included, ends the
    /// paired generation with nothing shown.
    #[test]
    fn a_login_start_is_roots_nonce_or_a_violation() {
        for selection in [
            LoginSelection::Enroll(1),
            LoginSelection::Enroll(2),
            LoginSelection::Add,
            LoginSelection::Remove(removal(), 1),
        ] {
            let screen = Screen::login(selection.clone());
            let before = screen.shown();
            let (client, wire, result) = drive(&screen, vec![[&[0x9b, 1][..], &NONCE].concat()]);
            assert_eq!(result, Ok(()));
            assert!(client.login.is_some());
            assert_eq!(wire.calls.len(), 1);
            assert_eq!(screen.shown(), before);
            let screen = Screen::login(selection);
            let (_, _, result) = drive(&screen, vec![vec![0x9b, 0]]);
            assert!(result.is_err());
            assert_eq!(screen.shown(), before);
        }
        for start in [
            vec![0x9b, 1],
            [&[0x9b, 1][..], &[0; 32]].concat(),
            [&[0x9b, 1][..], &NONCE, &[0]].concat(),
            vec![0x9b, 2],
            vec![0x92],
        ] {
            let screen = Screen::login(LoginSelection::Add);
            assert!(drive(&screen, vec![start]).2.is_err());
        }
    }

    #[test]
    fn the_first_description_is_the_selected_operation_under_roots_nonce() {
        let other = |nonce| {
            Request::new(
                nonce,
                1000,
                Operation::LoginAdd {
                    account: 1000,
                    before: 3,
                    after: 4,
                    step: LoginStep::Identify,
                    proof: None,
                },
            )
            .unwrap()
        };
        for (selection, first) in [
            // Another nonce.
            (LoginSelection::Add, other([8; 32])),
            // Another kind.
            (LoginSelection::Add, unlock_step(LoginStep::Identify)),
            (LoginSelection::Unlock, add_step(LoginStep::Identify)),
            // Another key count.
            (
                LoginSelection::Enroll(1),
                enroll_step(2, 1, LoginStep::Connect),
            ),
            // Other slots.
            (
                LoginSelection::Remove(removal()[..1].to_vec(), 1),
                remove_step(LoginStep::Identify),
            ),
            // Another count left, and so another ceiling.
            (
                LoginSelection::Remove(removal(), 2),
                remove_step(LoginStep::Identify),
            ),
            // Not the operation's first step.
            (
                LoginSelection::Unlock,
                unlock_step(LoginStep::Unlock {
                    key: ENROLLED[0],
                    retries: 8,
                }),
            ),
            (
                LoginSelection::Enroll(2),
                enroll_step(2, 2, LoginStep::Connect),
            ),
            // The same operation authorized by a disk proof, tags 13 to 15,
            // whose addition and removal keep the ordinary ceilings.
            (
                LoginSelection::Enroll(1),
                proven(enroll_op(1, LoginStep::RecoveryKey), Proof::RecoveryKey),
            ),
            (
                LoginSelection::Enroll(2),
                proven(enroll_op(2, LoginStep::DiskIdentify), Proof::DiskKey),
            ),
            (
                LoginSelection::Add,
                proven(add_op(LoginStep::RecoveryKey), Proof::RecoveryKey),
            ),
            (
                LoginSelection::Add,
                proven(add_op(LoginStep::DiskIdentify), Proof::DiskKey),
            ),
            (
                LoginSelection::Remove(removal(), 1),
                proven(remove_op(LoginStep::RecoveryKey), Proof::RecoveryKey),
            ),
            (
                LoginSelection::Remove(removal(), 1),
                proven(remove_op(LoginStep::DiskIdentify), Proof::DiskKey),
            ),
        ] {
            for code in [3, 4, 0x0d] {
                let screen = Screen::login(selection.clone());
                let reply = if code == 0x0d {
                    [&[0x91, 0x0d, 0x0f, 0][..], &first.encode()].concat()
                } else {
                    status(code, &first)
                };
                let (_, wire, result) = drive(&screen, vec![started(), reply]);
                assert!(result.is_err(), "{selection:?} {first:?}");
                assert_eq!(sent(&wire, 0x13), 0);
                assert_eq!(sent(&wire, 0x15), 0);
            }
        }
        // Valid, but not with a status that cannot carry a first description.
        for code in [5, 6, 0x0c] {
            let screen = Screen::login(LoginSelection::Unlock);
            let reply = status(code, &unlock_step(LoginStep::Identify));
            assert!(drive(&screen, vec![started(), reply]).2.is_err());
        }
        // A description before any is a baseline wait out of turn.
        let screen = Screen::login(LoginSelection::Unlock);
        let (_, wire, result) = drive(&screen, presented(&LoginSelection::Unlock, 1).0);
        assert_eq!(result, Ok(()));
        let mut replies = presented(&LoginSelection::Unlock, 1).0;
        replies.push(vec![0x91, 0x0b]);
        let screen = Screen::login(LoginSelection::Unlock);
        assert!(drive(&screen, replies).2.is_err());
        assert_eq!(sent(&wire, 0x13), 1);
    }

    #[test]
    fn a_changed_nonce_or_kind_skipped_or_repeated_step_is_refused() {
        let unlock = LoginSelection::Unlock;
        let add = LoginSelection::Add;
        let enroll = LoginSelection::Enroll(2);
        let add_steps = steps(&add);
        let enroll_steps = steps(&enroll);
        let changed_nonce =
            Request::new([8; 32], 1000, steps(&unlock)[1].operation().clone()).unwrap();
        for (selection, count, next) in [
            // The nonce changes at the second step.
            (unlock.clone(), 1, status(4, &changed_nonce)),
            // The kind changes at the second step.
            (
                unlock.clone(),
                1,
                status(
                    4,
                    &add_step(LoginStep::Authorize {
                        key: ENROLLED[1],
                        retries: 8,
                    }),
                ),
            ),
            // Authorize skipped.
            (add.clone(), 1, status(4, &add_steps[2])),
            // Prove skipped.
            (add.clone(), 4, status(4, &add_steps[5])),
            // The second key's connect before the first key's probe.
            (enroll.clone(), 4, status(4, &enroll_steps[5])),
            // The same step presented again.
            (add.clone(), 2, status(4, &add_steps[1])),
            // A step root goes back to.
            (add.clone(), 3, status(4, &add_steps[1])),
            // A next step with no presentation required. Under `03` it is
            // admitted: root read it as it stopped
            // (`a_successor_read_as_root_stopped_ends_the_operation`).
            (add.clone(), 1, status(5, &add_steps[1])),
            (add.clone(), 1, status(0x0c, &add_steps[1])),
            // A repeat naming another credential than its prove.
            (
                add.clone(),
                5,
                status(
                    4,
                    &add_step(LoginStep::Repeat {
                        key: BACKUP,
                        retries: 7,
                    }),
                ),
            ),
            // A PIN step whose key has no attempt left.
            (
                add.clone(),
                1,
                status(
                    4,
                    &add_step(LoginStep::Authorize {
                        key: ENROLLED[1],
                        retries: 0,
                    }),
                ),
            ),
        ] {
            let screen = Screen::login(selection.clone());
            let (mut replies, _) = presented(&selection, count);
            replies.push(next);
            let (_, wire, result) = drive(&screen, replies);
            assert!(result.is_err(), "{selection:?} after {count}");
            assert_eq!(sent(&wire, 0x13), count);
            assert_eq!(sent(&wire, 0x14), 0);
            // Refused before any presentation, not by the renderer's own
            // check: no presentation failed, so nothing was cancelled.
            assert_eq!(sent(&wire, 0x15), 0, "{selection:?} after {count}");
        }
        // A step is replaced only after its own receipt: an unpresented
        // first step cannot be followed.
        let screen = Screen::login(add.clone());
        let replies = vec![
            started(),
            status(3, &add_steps[0]),
            status(4, &add_steps[1]),
        ];
        let (_, wire, result) = drive(&screen, replies);
        assert_eq!(
            result,
            Err("login status changed a step that was not presented".into())
        );
        assert_eq!(sent(&wire, 0x13), 0);
    }

    #[test]
    fn an_additions_new_key_never_names_the_key_that_authorized_it() {
        let add = LoginSelection::Add;
        let steps = steps(&add);
        let authorizer = ENROLLED[1];
        let prove = add_step(LoginStep::Prove {
            key: authorizer,
            retries: 8,
        });
        // Consent sees one step and its predecessor, so it admits this.
        assert_eq!(steps[3].login_successor(&prove), Ok(Admitted::Next));
        let screen = Screen::login(add.clone());
        let (mut replies, _) = presented(&add, 4);
        replies.push(status(4, &prove));
        let (_, wire, result) = drive(&screen, replies);
        assert_eq!(
            result,
            Err("a new login key cannot be the key that authorized it".into())
        );
        assert_eq!(sent(&wire, 0x13), 4);
        // Another enrolled key is root's to refuse, not this reader's.
        let screen = Screen::login(add.clone());
        let (mut replies, _) = presented(&add, 4);
        replies.extend([
            status(
                4,
                &add_step(LoginStep::Prove {
                    key: ENROLLED[0],
                    retries: 8,
                }),
            ),
            vec![0x93],
        ]);
        assert_eq!(drive(&screen, replies).2, Ok(()));
    }

    #[test]
    fn a_commit_is_legal_only_after_the_last_step() {
        for selection in selections() {
            let steps = steps(&selection);
            for count in 1..steps.len() {
                let screen = Screen::login(selection.clone());
                let (mut replies, _) = presented(&selection, count);
                replies.push(status(5, &steps[count - 1]));
                let (_, wire, result) = drive(&screen, replies);
                assert!(result.is_err(), "{selection:?} after {count}");
                assert_eq!(sent(&wire, 0x14), 0);
            }
            // The last step, but its presentation failed.
            let screen = Screen::login(selection.clone());
            let (replies, _) = presented(&selection, steps.len() - 1);
            let (mut client, mut wire, result) = drive(&screen, replies);
            assert_eq!(result, Ok(()));
            screen
                .attempt
                .runtime
                .lock()
                .unwrap()
                .begin_compound_commit()
                .unwrap();
            let last = steps.last().unwrap();
            wire.replies
                .extend([status(4, last), vec![0x95, 0], status(5, last)]);
            client.tick(&mut wire).unwrap();
            assert!(client.tick(&mut wire).is_err(), "{selection:?}");
            assert_eq!(sent(&wire, 0x13), steps.len() - 1);
            assert_eq!(sent(&wire, 0x14), 0);
            assert_eq!(sent(&wire, 0x15), 1);
        }
    }

    /// Cancels the attempt, as Escape does, with root's `at`th answer.
    struct CancelAt<'a> {
        wire: Wire,
        attempt: &'a Attempt,
        at: usize,
    }
    impl Exchange for CancelAt<'_> {
        fn send(&mut self, bytes: &[u8]) -> Result<(), String> {
            self.wire.send(bytes)
        }
        fn receive(&mut self) -> Result<Vec<u8>, String> {
            let response = self.wire.receive()?;
            if self.wire.calls.len() == self.at {
                self.attempt.cancel();
            }
            Ok(response)
        }
    }

    #[test]
    fn cancellation_at_each_stage_sends_one_cancel_and_no_commit() {
        let unlock = LoginSelection::Unlock;
        let steps = steps(&unlock);
        let identify = steps[0].clone();
        let last = steps[1].clone();
        let end = |request: &Request| [&ended(0x0d, 0x80, 0)[..], &request.encode()].concat();
        // Escape before the baseline, at the first step, after a
        // presentation, and at the commit: root's answer at which the
        // attempt is cancelled, then the cancellation and its end.
        let mut at_commit = presented(&unlock, 2).0;
        let commit = at_commit.len() + 1;
        at_commit.push(status(5, &last));
        let mut after_presentation = presented(&unlock, 1).0;
        let presentation = after_presentation.len();
        after_presentation.extend([vec![0x95, 0], end(&identify)]);
        at_commit.extend([vec![0x95, 0], end(&last)]);
        // As the PIN field opens, and once the PIN is typed and submitted
        // but not yet sent.
        let mut at_field = presented(&unlock, 1).0;
        at_field.extend([status(4, &last), vec![0x93], status(0x0c, &last)]);
        let field = at_field.len();
        let mut at_typed = at_field.clone();
        at_field.extend([vec![0x95, 0], end(&last)]);
        at_typed.push(status(0x0c, &last));
        let typed = at_typed.len();
        at_typed.extend([vec![0x95, 0], end(&last)]);
        for (replies, at) in [
            (at_field, field),
            (at_typed, typed),
            (
                vec![
                    started(),
                    vec![0x91, 0x0b],
                    vec![0x95, 0],
                    ended(0x0d, 0x80, 0),
                ],
                2,
            ),
            (
                vec![
                    started(),
                    status(3, &identify),
                    vec![0x95, 0],
                    end(&identify),
                ],
                2,
            ),
            (after_presentation, presentation),
            (at_commit, commit),
        ] {
            let screen = Screen::login(unlock.clone());
            let mut wire = CancelAt {
                wire: typing(replies, &screen),
                attempt: &screen.attempt,
                at,
            };
            let mut client = login_client();
            client
                .start(&mut wire, Arc::clone(&screen.attempt))
                .unwrap();
            while client.login.is_some() {
                client.tick(&mut wire).unwrap();
            }
            assert!(wire.wire.replies.is_empty(), "{at}");
            assert_eq!(sent(&wire.wire, 0x15), 1);
            assert_eq!(sent(&wire.wire, 0x14), 0);
            assert_eq!(
                wire.wire
                    .calls
                    .iter()
                    .filter(|c| **c == cancelled())
                    .count(),
                1
            );
            assert!(!screen.attempt.commit());
            // Escape's PIN, typed or not, went nowhere and is gone.
            assert_eq!(sent(&wire.wire, 0x1c), usize::from(at == commit));
            assert_eq!(screen.attempt.field_raw(), [0; PIN_CAPACITY]);
        }
        // Not for a step that asks none, nor before that step's own
        // presentation.
        for wants in [status(0x0c, &identify), status(0x0c, &last)] {
            let screen = Screen::login(unlock.clone());
            let mut replies = presented(&unlock, 1).0;
            replies.push(wants);
            let (_, wire, result) = drive(&screen, replies);
            assert!(result.is_err());
            assert_eq!(sent(&wire, 0x15), 0);
        }
    }

    /// Every kind root sends, and its rows.
    fn kinds() -> Vec<(u8, u8, &'static [&'static str])> {
        let mut kinds: Vec<(u8, u8, &'static [&'static str])> = vec![
            (0x01, 0, &["WRONG PIN"]),
            (0x01, 7, &["WRONG PIN"]),
            (0x02, 0, &["REMOVE AND REINSERT THIS KEY"]),
            (0x03, 0, &["PIN BLOCKED; USE ANOTHER KEY"]),
            (0x04, 0, &["THIS KEY IS NOT ENROLLED HERE"]),
            (0x05, 0, &["CONNECT EXACTLY ONE KEY"]),
            (0x06, 1, &["KEY HAS NO HMAC-SECRET"]),
            (0x06, 2, &["KEY ALWAYS REQUIRES UV"]),
            (0x06, 3, &["KEY CANNOT HOLD A PIN"]),
            (0x06, 4, &["SET A PIN WITH ANOTHER TOOL"]),
            (0x06, 5, &["KEY CANNOT LIST ENROLLED KEYS"]),
            (0x06, 6, &["KEY CANNOT BE SELECTED"]),
            (0x07, 0, &["TOUCH DENIED"]),
            (0x08, 0, &["TIMED OUT"]),
            (0x09, 0, &["NO LOGIN KEYS ENROLLED"]),
            (
                0x0a,
                0,
                &["LOGIN KEY STATE UNAVAILABLE:", "DIRECTORY DAMAGED"],
            ),
            (0x0b, 0, &["LOGIN KEY STATE UNAVAILABLE:", "RECORD DAMAGED"]),
            (
                0x0c,
                0,
                &["LOGIN KEY STATE UNAVAILABLE:", "STATE COULD NOT BE READ"],
            ),
            (0x0d, 0, &["KEYS CHANGED; NOTHING WRITTEN"]),
            (0x0f, 0, &["THE OPERATION FAILED"]),
            (0x10, 0, &["THE OPERATION FAILED"]),
            (0x11, 0, &["KEY ALREADY ENROLLED"]),
            (0x12, 0, &["A RETAINED SYSTEM CANNOT READ KEYS"]),
            (0x80, 0, &[]),
            (0x81, 0, &["EIGHT KEYS ALREADY ENROLLED"]),
            (0x82, 0, &["LOGIN KEYS ALREADY ENROLLED"]),
            (0x83, 0, &["KEY LIST CHANGED; REOPEN"]),
            (0x84, 0, &["ENROLLING HERE NEEDS A DISK PROOF"]),
            (0x85, 0, &["THE LAST KEY STAYS ON THIS MACHINE"]),
        ];
        for detail in 1..=5 {
            kinds.push((0x0e, detail, &["RESULT UNCERTAIN"]));
        }
        kinds
    }

    /// Where this client stands when root's end arrives.
    #[derive(Clone, Copy, Debug)]
    enum Point {
        /// Before the worker's baseline: no description yet.
        Baseline,
        /// The end itself carries the first description.
        First,
        /// After the first step's presentation.
        Presented,
        /// After this client's cancellation, before any description or
        /// after the first step's presentation.
        Cancelled(bool),
        /// After the commit's acknowledgement.
        Committed,
    }
    const POINTS: &[Point] = &[
        Point::Baseline,
        Point::First,
        Point::Presented,
        Point::Cancelled(false),
        Point::Cancelled(true),
        Point::Committed,
    ];

    /// Root's reachable ends at `point`, read from td-authd/src/login.rs
    /// rather than from this client: `failure` admits the worker's typed
    /// kinds in every phase, `0e` (1 to 5) only once a write's commit was
    /// acknowledged, root ending INTERNAL before that; root's own TIMEOUT
    /// and INTERNAL are worker kinds too; CANCELLED answers a `15`; and
    /// `begin` refuses at the baseline, before root has any description,
    /// with ENROLLED, NO_RECORD, FULL, SELECTION or INTERNAL.
    fn reachable(selection: &LoginSelection, point: Point) -> BTreeSet<(u8, u8)> {
        let mut ends: BTreeSet<(u8, u8)> = (0..=u8::MAX).map(|detail| (0x01, detail)).collect();
        ends.extend((1..=6).map(|detail| (0x06, detail)));
        ends.extend(
            [
                2, 3, 4, 5, 7, 8, 9, 0x0a, 0x0b, 0x0c, 0x0d, 0x0f, 0x10, 0x11, 0x12,
            ]
            .map(|kind| (kind, 0)),
        );
        if matches!(point, Point::Committed) && selection.writes() {
            ends.extend((1..=5).map(|detail| (0x0e, detail)));
        }
        if matches!(point, Point::Cancelled(_)) {
            ends.insert((0x80, 0));
        }
        if matches!(point, Point::Baseline | Point::Cancelled(false)) {
            ends.insert(match selection {
                LoginSelection::Unlock => (0x09, 0),
                LoginSelection::Enroll(_) => (0x82, 0),
                LoginSelection::Add => (0x81, 0),
                LoginSelection::Remove(..) => (0x83, 0),
                LoginSelection::Notebook
                | LoginSelection::SelectorUpdate
                | LoginSelection::Upgrade { .. }
                | LoginSelection::UpgradeCancel
                | LoginSelection::Protector(_)
                | LoginSelection::Proven(..) => {
                    panic!("storage and disk proof tests drive their own")
                }
            });
            match selection {
                LoginSelection::Enroll(_) => ends.insert((0x84, 0)),
                LoginSelection::Remove(..) => ends.insert((0x85, 0)),
                LoginSelection::Notebook
                | LoginSelection::Unlock
                | LoginSelection::Add
                | LoginSelection::SelectorUpdate
                | LoginSelection::Upgrade { .. }
                | LoginSelection::UpgradeCancel
                | LoginSelection::Protector(_)
                | LoginSelection::Proven(..) => false,
            };
        }
        ends
    }

    /// The client at `point`, and the description and status root's end
    /// carries there.
    fn at(selection: &LoginSelection, point: Point) -> (Screen, LoginPending, Vec<u8>, u8) {
        let steps = steps(selection);
        let last = steps.last().unwrap();
        let screen = Screen::login(selection.clone());
        let (replies, description) = match point {
            Point::Baseline | Point::Cancelled(false) => {
                (vec![started(), vec![0x91, 0x0b]], vec![])
            }
            Point::First => (vec![started(), vec![0x91, 0x0b]], steps[0].encode()),
            Point::Presented | Point::Cancelled(true) => {
                (presented(selection, 1).0, steps[0].encode())
            }
            Point::Committed => {
                let mut replies = presented(selection, steps.len()).0;
                replies.extend([status(5, last), vec![0x94], status(3, last)]);
                (replies, last.encode())
            }
        };
        let (mut client, driven, result) = drive(&screen, replies);
        assert_eq!(result, Ok(()), "{selection:?} {point:?}");
        assert!(driven.replies.is_empty());
        let mut login = client.login.take().unwrap();
        if matches!(point, Point::Cancelled(_)) {
            screen.attempt.cancel();
            let mut cancelling = wire(vec![vec![0x95, 0]]);
            login.cancel(&mut cancelling).unwrap();
            assert_eq!(cancelling.calls, [cancelled()]);
        }
        let uncertain = matches!(point, Point::Committed) && selection.writes();
        (
            screen,
            login,
            description,
            if uncertain { 0x0e } else { 0x0d },
        )
    }

    /// Root's end `kind`, `detail` and `description` under `status`, to the
    /// client as it stands in `login`.
    fn end_at(
        login: &LoginPending,
        status: u8,
        kind: u8,
        detail: u8,
        description: &[u8],
    ) -> (Client, Result<(), String>) {
        let mut wire = wire(vec![
            [&ended(status, kind, detail)[..], description].concat()
        ]);
        let mut client = Client {
            login: Some(login.clone()),
            ..login_client()
        };
        let result = client.tick(&mut wire);
        (client, result)
    }

    /// Every (status, kind, detail, description) root can end with is
    /// accepted, at every point of every operation, and nothing else: the
    /// whole kind-by-detail grid against the client's rule, and through
    /// the client every reachable end, `01` by sample, and every kind with
    /// details on and off its own.
    #[test]
    fn the_client_accepts_exactly_the_ends_root_can_reach() {
        for selection in selections() {
            for point in POINTS {
                let (_screen, login, description, status) = at(&selection, *point);
                let reachable = reachable(&selection, *point);
                let described = !description.is_empty();
                let uncertain = status == 0x0e;
                for kind in 0..=u8::MAX {
                    for detail in 0..=u8::MAX {
                        assert_eq!(
                            login.ends(uncertain, kind, detail, described),
                            reachable.contains(&(kind, detail)),
                            "{selection:?} {point:?} {kind:02x} {detail:02x}"
                        );
                    }
                }
                let tried = (0..=u8::MAX)
                    .flat_map(|kind| [0, 1, 6, 7].map(|detail| (kind, detail)))
                    // WRONG PIN's every count is the grid's; samples here.
                    .chain(reachable.iter().copied().filter(|(kind, _)| *kind != 0x01));
                for (kind, detail) in tried {
                    let (client, result) = end_at(&login, status, kind, detail, &description);
                    let accepted = reachable.contains(&(kind, detail));
                    assert_eq!(
                        result.is_ok(),
                        accepted,
                        "{selection:?} {point:?} {kind:02x} {detail:02x} {result:?}"
                    );
                    if accepted {
                        assert!(client.login.is_none());
                        // Every end, uncertain ones too, is followed by `1a`.
                        let mut client = client;
                        assert!(client.take_login_end());
                        assert!(!client.take_login_end());
                    }
                }
                // The other status is a violation, at every point.
                let other = if uncertain { 0x0d } else { 0x0e };
                assert!(end_at(&login, other, 0x08, 0, &description).1.is_err());
                if described {
                    // Root describes every answer from its first step on;
                    // after a commit, only the committed step.
                    if !matches!(point, Point::First) {
                        assert!(end_at(&login, status, 0x08, 0, &[]).1.is_err());
                    }
                    if matches!(point, Point::Committed) {
                        let first = steps(&selection)[0].encode();
                        assert!(end_at(&login, status, 0x08, 0, &first).1.is_err());
                    }
                } else {
                    let first = steps(&selection)[0].encode();
                    assert!(end_at(&login, status, 0x08, 0, &first).1.is_ok());
                    // A selection's refusal never comes with a description.
                    for refusal in [0x81, 0x82, 0x83, 0x84, 0x85] {
                        assert!(end_at(&login, status, refusal, 0, &first).1.is_err());
                    }
                }
            }
        }
    }

    #[test]
    fn every_end_kind_shows_its_text() {
        // The table names exactly the pairs root can reach somewhere.
        let named: BTreeSet<(u8, u8)> = (0..=u8::MAX)
            .flat_map(|kind| (0..=u8::MAX).map(move |detail| (kind, detail)))
            .filter(|(kind, detail)| login_failure(*kind, *detail).is_some())
            .collect();
        let anywhere: BTreeSet<(u8, u8)> = selections()
            .iter()
            .flat_map(|selection| POINTS.iter().flat_map(|point| reachable(selection, *point)))
            .collect();
        assert_eq!(named, anywhere);
        // Every row of every end and success, each glyph drawn, and whole
        // on the narrowest output's chrome rows.
        let columns = crate::attention::layout(320, 200).1;
        let selections = selections();
        let rows = named
            .iter()
            .filter_map(|(kind, detail)| login_failure(*kind, *detail))
            .chain(selections.iter().map(|selection| selection.success(None)))
            .chain([&["TIMED OUT"][..], &["THE OPERATION FAILED"]]);
        for rows in rows {
            for row in rows {
                assert!(row.bytes().all(crate::ui::is_mapped), "{row}");
                assert!(row.len() <= columns, "{row}");
            }
        }
        for (kind, detail, rows) in kinds() {
            assert_eq!(login_failure(kind, detail), Some(rows));
            // Where root reaches it, and what the screen then shows.
            let (selection, point, shown) = match kind {
                // A cancellation shows nothing: Escape's screen is closing.
                0x80 => (
                    LoginSelection::Unlock,
                    Point::Cancelled(false),
                    Notice::default(),
                ),
                0x81 => (LoginSelection::Add, Point::Baseline, Notice::Login(rows)),
                0x82 => (
                    LoginSelection::Enroll(1),
                    Point::Baseline,
                    Notice::Login(rows),
                ),
                0x83 | 0x85 => (
                    LoginSelection::Remove(removal(), 1),
                    Point::Baseline,
                    Notice::Login(rows),
                ),
                0x84 => (
                    LoginSelection::Enroll(1),
                    Point::Baseline,
                    Notice::Login(rows),
                ),
                0x0e => (
                    LoginSelection::Remove(removal(), 1),
                    Point::Committed,
                    Notice::Uncertain(&[]),
                ),
                _ => (
                    LoginSelection::Unlock,
                    Point::Presented,
                    Notice::Login(rows),
                ),
            };
            let (screen, login, description, status) = at(&selection, point);
            let (client, result) = end_at(&login, status, kind, detail, &description);
            assert_eq!(result, Ok(()), "{kind:02x} {detail:02x}");
            assert!(client.login.is_none());
            // A presented prompt is replaced, not left standing.
            assert_eq!(screen.shown(), Some(shown), "{kind:02x}");
        }
    }

    #[test]
    fn only_an_acknowledged_writes_end_is_uncertain() {
        for selection in selections() {
            let (screen, login, description, status) = at(&selection, Point::Committed);
            assert_eq!(status == 0x0e, selection.writes());
            let (_, result) = end_at(&login, status, 0x08, 0, &description);
            assert_eq!(result, Ok(()));
            assert_eq!(
                screen.shown(),
                Some(if selection.writes() {
                    Notice::Uncertain(&["TIMED OUT"])
                } else {
                    Notice::Login(&["TIMED OUT"])
                })
            );
            // A write's own report of an attempted write, only after commit.
            assert_eq!(
                end_at(&login, status, 0x0e, 1, &description).1.is_ok(),
                selection.writes()
            );
            for point in [Point::Presented, Point::Cancelled(true)] {
                let (_screen, login, description, _) = at(&selection, point);
                assert!(end_at(&login, 0x0e, 0x08, 0, &description).1.is_err());
                assert!(end_at(&login, 0x0d, 0x0e, 1, &description).1.is_err());
            }
        }
    }

    /// Root that read the next invitation, then stopped at its deadline or
    /// on a cancellation, describes that never-presented step under `03`
    /// while it reaps, and ends with it. The client takes it as the
    /// successor it is, presents nothing and commits nothing.
    #[test]
    fn a_successor_read_as_root_stopped_ends_the_operation() {
        for selection in selections() {
            let steps = steps(&selection);
            let next = &steps[1];
            let screen = Screen::login(selection.clone());
            let mut replies = presented(&selection, 1).0;
            replies.extend([
                status(3, next),
                [&ended(0x0d, 0x08, 0)[..], &next.encode()].concat(),
            ]);
            let (client, timed_out, result) = drive(&screen, replies);
            assert_eq!(result, Ok(()), "{selection:?}");
            assert!(client.login.is_none());
            assert_eq!(sent(&timed_out, 0x13), 1);
            assert_eq!(sent(&timed_out, 0x14), 0);
            assert_eq!(screen.shown(), Some(Notice::Login(&["TIMED OUT"])));
            // On Escape: the cancellation, then the successor, then its end.
            let screen = Screen::login(selection.clone());
            let mut replies = presented(&selection, 1).0;
            let at = replies.len();
            replies.extend([
                vec![0x95, 0],
                status(3, next),
                [&ended(0x0d, 0x80, 0)[..], &next.encode()].concat(),
            ]);
            let mut wire = CancelAt {
                wire: typing(replies, &screen),
                attempt: &screen.attempt,
                at,
            };
            let mut client = login_client();
            client
                .start(&mut wire, Arc::clone(&screen.attempt))
                .unwrap();
            while client.login.is_some() {
                client.tick(&mut wire).unwrap();
            }
            assert!(wire.wire.replies.is_empty());
            assert_eq!(sent(&wire.wire, 0x15), 1);
            assert_eq!(sent(&wire.wire, 0x13), 1);
            assert_eq!(sent(&wire.wire, 0x14), 0);
            // Admitted, it is not presented, so nothing commits it.
            let screen = Screen::login(selection.clone());
            let mut replies = presented(&selection, 1).0;
            replies.extend([status(3, next), status(5, next)]);
            let (_, refused, result) = drive(&screen, replies);
            assert!(result.is_err(), "{selection:?}");
            assert_eq!(sent(&refused, 0x14), 0);
            // Still only after its predecessor's receipt, and only the
            // successor: not before the first step was presented, and not
            // a step further on.
            let screen = Screen::login(selection.clone());
            let replies = vec![
                started(),
                vec![0x91, 0x0b],
                status(3, &steps[0]),
                status(3, next),
            ];
            assert!(drive(&screen, replies).2.is_err(), "{selection:?}");
            if let Some(later) = steps.get(2) {
                let screen = Screen::login(selection.clone());
                let mut replies = presented(&selection, 1).0;
                replies.push(status(3, later));
                let (_, wire, result) = drive(&screen, replies);
                assert!(result.is_err(), "{selection:?}");
                assert_eq!(sent(&wire, 0x15), 0);
            }
        }
    }

    /// A prompt this output cannot hold whole ends the operation before
    /// its token I/O and says so: a one-key enrollment's disclosure at
    /// 800x600. A two-key enrollment's fits there.
    #[test]
    fn a_prompt_too_tall_for_the_screen_says_so() {
        let one = LoginSelection::Enroll(1);
        let first = steps(&one)[0].clone();
        let end = [&ended(0x0d, 0x80, 0)[..], &first.encode()].concat();
        let screen = Screen::login_at(one, 800, 600);
        let mut root = wire(vec![
            started(),
            vec![0x91, 0x0b],
            status(3, &first),
            status(4, &first),
            vec![0x95, 0],
            end,
        ]);
        let mut client = login_client();
        client
            .start(&mut root, Arc::clone(&screen.attempt))
            .unwrap();
        while client.login.is_some() {
            client.tick(&mut root).unwrap();
        }
        assert!(root.replies.is_empty());
        assert_eq!(sent(&root, 0x15), 1);
        assert_eq!(sent(&root, 0x13), 0);
        assert_eq!(screen.shown(), Some(Notice::Login(TOO_SMALL)));
        let two = LoginSelection::Enroll(2);
        let screen = Screen::login_at(two.clone(), 800, 600);
        let first = steps(&two)[0].clone();
        assert_eq!(screen.attempt.present(first.clone()), Ok(first));
    }

    /// The client's own cancellation says why and withdraws the prompt: its
    /// deadline, or a presentation that failed.
    #[test]
    fn the_clients_own_cancellation_says_why() {
        let unlock = LoginSelection::Unlock;
        let steps = steps(&unlock);
        let end = [&ended(0x0d, 0x80, 0)[..], &steps[0].encode()].concat();
        let (screen, deadline, mut client) =
            Screen::expiring_after(unlock.clone(), Duration::from_millis(2000), |screen| {
                let (client, _, result) = drive(screen, presented(&unlock, 1).0);
                assert_eq!(result, Ok(()));
                // The prompt is up.
                assert_eq!(screen.shown(), None);
                client
            });
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        let mut expired = wire(vec![vec![0x95, 0], end.clone()]);
        client.tick(&mut expired).unwrap();
        assert!(client.login.is_none());
        assert_eq!(expired.calls, [cancelled(), vec![0x11]]);
        assert_eq!(screen.shown(), Some(Notice::Login(&["TIMED OUT"])));

        struct FailAt<'a> {
            wire: Wire,
            runtime: &'a Mutex<Runtime>,
            at: usize,
        }
        impl Exchange for FailAt<'_> {
            fn send(&mut self, bytes: &[u8]) -> Result<(), String> {
                self.wire.send(bytes)
            }
            fn receive(&mut self) -> Result<Vec<u8>, String> {
                let response = self.wire.receive()?;
                if self.wire.calls.len() == self.at {
                    self.runtime.lock().unwrap().fail_next_repaint();
                }
                Ok(response)
            }
        }
        let screen = Screen::login(unlock.clone());
        // The first step's `04` draws a presentation whose paint fails.
        let mut wire = FailAt {
            wire: wire(vec![
                started(),
                vec![0x91, 0x0b],
                status(3, &steps[0]),
                status(4, &steps[0]),
                vec![0x95, 0],
                end,
            ]),
            runtime: &screen.attempt.runtime,
            at: 4,
        };
        let mut client = login_client();
        client
            .start(&mut wire, Arc::clone(&screen.attempt))
            .unwrap();
        while client.login.is_some() {
            client.tick(&mut wire).unwrap();
        }
        assert_eq!(sent(&wire.wire, 0x15), 1);
        assert_eq!(sent(&wire.wire, 0x13), 0);
        assert_eq!(
            screen.shown(),
            Some(Notice::Login(&["THE OPERATION FAILED"]))
        );
    }

    /// A disclosing operation's lifetime adds root's reading allowance to
    /// its ceremonies, as root's and the worker's ceilings do.
    #[test]
    fn a_login_attempt_lives_its_operations_ceiling_and_no_longer() {
        assert_eq!(LOGIN_READING, Duration::from_secs(120));
        assert_eq!(LOGIN_TWO_KEY_ENROLLMENT, Duration::from_secs(360));
        for (selection, seconds) in [
            (LoginSelection::Unlock, 120),
            (LoginSelection::Enroll(1), 240),
            (LoginSelection::Remove(removal(), 1), 240),
            (LoginSelection::Enroll(2), 360),
            (LoginSelection::Add, 240),
        ] {
            // Bracketed by the clock on both sides, however long the
            // attempt took to make.
            let before = Instant::now();
            let screen = Screen::login(selection.clone());
            let after = Instant::now();
            let deadline = screen.attempt.deadline.unwrap();
            assert!(deadline >= before + Duration::from_secs(seconds));
            assert!(deadline <= after + Duration::from_secs(seconds));
            assert_eq!(
                steps(&selection)[0].login_ceiling(),
                Some(Duration::from_secs(seconds))
            );
            // Its first prompt shows what is left, within that ceiling.
            let first = steps(&selection)[0].clone();
            assert_eq!(screen.attempt.present(first.clone()), Ok(first));
        }
        let screen = Screen::new();
        let lifetime = screen
            .attempt
            .deadline
            .unwrap()
            .duration_since(Instant::now());
        assert!(lifetime <= Duration::from_secs(120));
    }

    #[test]
    fn removal_digits_build_the_exact_1b_request() {
        let wire = |selection: LoginSelection| Selection::Login(selection).request();
        assert_eq!(wire(LoginSelection::Unlock), Ok(vec![0x1b, 7]));
        assert_eq!(wire(LoginSelection::Enroll(1)), Ok(vec![0x1b, 8, 1]));
        assert_eq!(wire(LoginSelection::Enroll(2)), Ok(vec![0x1b, 8, 2]));
        assert_eq!(wire(LoginSelection::Add), Ok(vec![0x1b, 9]));
        for refused in [
            LoginSelection::Enroll(0),
            LoginSelection::Enroll(3),
            LoginSelection::Remove(Vec::new(), 1),
            LoginSelection::Remove(removal().into_iter().rev().collect(), 1),
            LoginSelection::Remove(
                vec![Slot {
                    position: 9,
                    key: NEW,
                }],
                1,
            ),
            LoginSelection::Remove(
                vec![Slot {
                    position: 0,
                    key: NEW,
                }],
                1,
            ),
            LoginSelection::Remove(
                (1..=9)
                    .map(|position| Slot { position, key: NEW })
                    .collect(),
                0,
            ),
        ] {
            assert!(wire(refused).is_err());
        }
        assert!(Removal::new(Vec::new()).is_none());
        assert!(Removal::new(vec![NEW; 9]).is_none());
        let mut digits = Removal::new(ENROLLED.to_vec()).unwrap();
        assert_eq!(digits.selection(), None);
        assert_eq!(digits.shown(), Notice::Removing { keys: 3, chosen: 0 });
        for refused in [0, 4, 9] {
            assert!(!digits.toggle(refused));
        }
        assert!(digits.toggle(3) && digits.toggle(1));
        assert_eq!(
            digits.shown(),
            Notice::Removing {
                keys: 3,
                chosen: 0b101
            }
        );
        let chosen = digits.selection().unwrap();
        assert_eq!(chosen, LoginSelection::Remove(removal(), 1));
        assert_eq!(
            wire(chosen),
            Ok([&[0x1b, 10, 2, 1][..], &ENROLLED[0], &[3], &ENROLLED[2]].concat())
        );
        assert!(digits.toggle(1));
        assert_eq!(
            digits.selection(),
            Some(LoginSelection::Remove(
                vec![Slot {
                    position: 3,
                    key: ENROLLED[2],
                }],
                2
            ))
        );
        assert!(digits.toggle(3));
        assert_eq!(digits.selection(), None);
        let mut eight = Removal::new(vec![NEW; 8]).unwrap();
        for position in 1..=8 {
            assert!(eight.toggle(position));
        }
        assert_eq!(
            eight.shown(),
            Notice::Removing {
                keys: 8,
                chosen: 0xff
            }
        );
        assert_eq!(wire(eight.selection().unwrap()).unwrap().len(), 3 + 8 * 5);
    }

    #[test]
    fn a_login_operation_occupies_the_client() {
        let screen = Screen::login(LoginSelection::Unlock);
        let mut wire = wire(vec![started()]);
        let mut client = login_client();
        client
            .start(&mut wire, Arc::clone(&screen.attempt))
            .unwrap();
        let other = Screen::new();
        client.start(&mut wire, Arc::clone(&other.attempt)).unwrap();
        assert_eq!(wire.calls.len(), 1);
        assert_eq!(other.shown(), Some(Notice::Busy));
    }

    // The PIN field.

    /// An unlock driven to root's first `0c` for its PIN step, with no
    /// one typing yet, and `then` queued after it.
    fn at_pin(screen: &Screen, then: Vec<Vec<u8>>) -> (Client, Wire) {
        let unlock = LoginSelection::Unlock;
        let last = steps(&unlock)[1].clone();
        let mut replies = presented(&unlock, 1).0;
        replies.extend([status(4, &last), vec![0x93], status(0x0c, &last)]);
        let mut wire = wire(replies);
        let mut client = login_client();
        client
            .start(&mut wire, Arc::clone(&screen.attempt))
            .unwrap();
        // An ended operation takes no more replies.
        while client.login.is_some() && !wire.replies.is_empty() {
            client.tick(&mut wire).unwrap();
        }
        assert!(wire.replies.is_empty(), "the operation ended early");
        wire.replies.extend(then);
        (client, wire)
    }

    fn type_bytes(screen: &Screen, bytes: &[u8]) {
        for byte in bytes {
            screen
                .attempt
                .field_key(
                    &crate::input::test_origin(),
                    FieldKey::Byte(*byte),
                    pressed(&screen.attempt),
                )
                .unwrap();
        }
    }

    fn unlock_pin_step() -> Request {
        steps(&LoginSelection::Unlock)[1].clone()
    }

    #[test]
    fn root_asking_for_a_pin_opens_the_field_and_the_pin_goes_once_as_1c() {
        let last = unlock_pin_step();
        let screen = Screen::login(LoginSelection::Unlock);
        let (mut client, mut wire) = at_pin(&screen, vec![status(0x0c, &last)]);
        // The field is up over the presented step, empty; nothing is sent.
        assert_eq!(screen.field(), Some(Field::Pin(0)));
        assert_eq!(screen.shown(), None);
        assert_eq!(sent(&wire, 0x1c), 0);
        // Typing shows one mask per byte; root asking again sends nothing
        // before Enter.
        type_bytes(&screen, b"9a ~");
        assert_eq!(screen.field(), Some(Field::Pin(4)));
        client.tick(&mut wire).unwrap();
        assert_eq!(sent(&wire, 0x1c), 0);
        screen
            .attempt
            .field_key(
                &crate::input::test_origin(),
                FieldKey::Submit,
                pressed(&screen.attempt),
            )
            .unwrap();
        wire.replies
            .extend([status(0x0c, &last), vec![0x9c, 0], status(3, &last)]);
        drops();
        client.tick(&mut wire).unwrap();
        // The exact `1c`, written from a buffer zeroed once it was; the
        // field zeroed as the PIN left it; then the touch request.
        assert_eq!(wire.calls.last(), Some(&pin_request(&last, b"9a ~")));
        assert_eq!(drops(), [true]);
        assert_eq!(screen.attempt.field_raw(), [0; PIN_CAPACITY]);
        assert_eq!(screen.field(), Some(Field::Touch));
        // The field is closed: nothing types, and root's work goes on.
        type_bytes(&screen, b"5678");
        assert_eq!(screen.attempt.field_raw(), [0; PIN_CAPACITY]);
        client.tick(&mut wire).unwrap();
        wire.replies.extend([
            status(5, &last),
            vec![0x94],
            status(3, &last),
            status(6, &last),
        ]);
        while client.login.is_some() {
            client.tick(&mut wire).unwrap();
        }
        assert_eq!(sent(&wire, 0x1c), 1);
        assert_eq!(sent(&wire, 0x14), 1);
        assert_eq!(screen.shown(), Some(Notice::Login(&["SESSION UNLOCKED"])));
    }

    /// Typing before root asks for the PIN reaches no field, and an Enter
    /// then submits nothing: no `1c` precedes root's `0c`.
    #[test]
    fn no_pin_is_taken_or_sent_before_root_asks_for_it() {
        let unlock = LoginSelection::Unlock;
        let last = unlock_pin_step();
        let screen = Screen::login(unlock.clone());
        let mut replies = presented(&unlock, 1).0;
        replies.extend([status(4, &last), vec![0x93]]);
        let mut wire = wire(replies);
        let mut client = login_client();
        client
            .start(&mut wire, Arc::clone(&screen.attempt))
            .unwrap();
        while !wire.replies.is_empty() {
            client.tick(&mut wire).unwrap();
        }
        // The prompt is presented and acknowledged; root has not asked.
        type_pin(&screen.attempt, b"1234");
        assert_eq!(screen.attempt.field_raw(), [0; PIN_CAPACITY]);
        assert_eq!(screen.field(), None);
        wire.replies
            .extend([status(3, &last), status(0x0c, &last), status(0x0c, &last)]);
        while !wire.replies.is_empty() {
            client.tick(&mut wire).unwrap();
        }
        // The field opened empty, and that early Enter submitted nothing.
        assert_eq!(sent(&wire, 0x1c), 0);
        assert_eq!(screen.field(), Some(Field::Pin(0)));
        // A `0c` for a step that asks none, or for one not yet presented,
        // opens nothing (`cancellation_at_each_stage_...`).
    }

    /// Root moves past a PIN step only once it has the PIN, so a successor
    /// or a commit before this client's `1c` is a violation.
    #[test]
    fn a_pin_steps_successor_or_commit_needs_its_1c() {
        let add = LoginSelection::Add;
        let add_steps = steps(&add);
        let (authorize, connect) = (&add_steps[1], &add_steps[2]);
        for (field, next) in [
            // As root asks, with the field open, or before it asks at all.
            (true, status(4, connect)),
            (true, status(3, connect)),
            (false, status(4, connect)),
            (
                false,
                [&ended(0x0d, 0x08, 0)[..], &connect.encode()].concat(),
            ),
        ] {
            let screen = Screen::login(add.clone());
            let mut replies = presented(&add, 1).0;
            replies.extend([status(4, authorize), vec![0x93]]);
            if field {
                replies.push(status(0x0c, authorize));
            }
            replies.push(next);
            let mut wire = wire(replies);
            let mut client = login_client();
            client
                .start(&mut wire, Arc::clone(&screen.attempt))
                .unwrap();
            let mut result = Ok(());
            while result.is_ok() && !wire.replies.is_empty() {
                if field && wire.replies.len() == 1 {
                    type_bytes(&screen, b"1234");
                }
                result = client.tick(&mut wire);
            }
            assert_eq!(
                result,
                Err("login status passed a PIN step before its PIN".into())
            );
            assert_eq!(sent(&wire, 0x13), 2);
            assert_eq!(sent(&wire, 0x1c), 0);
            // A violation leaves nothing typed behind.
            assert_eq!(screen.attempt.field_raw(), [0; PIN_CAPACITY]);
        }
        // The unlock step is the last: its commit waits for its PIN too.
        for asked in [false, true] {
            let last = unlock_pin_step();
            let screen = Screen::login(LoginSelection::Unlock);
            let mut replies = presented(&LoginSelection::Unlock, 1).0;
            replies.extend([status(4, &last), vec![0x93]]);
            if asked {
                replies.push(status(0x0c, &last));
            }
            replies.push(status(5, &last));
            let (_, wire, result) = drive(&screen, replies);
            assert_eq!(result, Err("out-of-order login operation status".into()));
            assert_eq!(sent(&wire, 0x14), 0);
            assert_eq!(sent(&wire, 0x1c), 0);
        }
        // After its `1c` the successor is admitted and presented.
        let screen = Screen::login(add.clone());
        let mut replies = presented(&add, 2).0;
        replies.extend([status(4, connect), vec![0x93]]);
        let (client, wire, result) = drive(&screen, replies);
        assert_eq!(result, Ok(()));
        assert_eq!(sent(&wire, 0x13), 3);
        assert_eq!(
            client.login.as_ref().unwrap().receipt.as_ref(),
            Some(connect)
        );
        // Its prompt replaced the touch request.
        assert_eq!(screen.field(), None);
    }

    /// Root asks once per PIN step: a second `0c` after this client's `1c`,
    /// or any answer to `1c` but root's two, is a violation.
    #[test]
    fn a_pin_is_answered_only_as_root_answers_one() {
        let last = unlock_pin_step();
        let screen = Screen::login(LoginSelection::Unlock);
        let mut replies = presented(&LoginSelection::Unlock, 2).0;
        replies.push(status(0x0c, &last));
        let (_, wire, result) = drive(&screen, replies);
        assert_eq!(result, Err("out-of-order login operation status".into()));
        assert_eq!(sent(&wire, 0x1c), 1);
        for answer in [vec![0x9c], vec![0x9c, 2], vec![0x9c, 0, 0], vec![0x93]] {
            let screen = Screen::login(LoginSelection::Unlock);
            let (mut client, mut wire) = at_pin(&screen, vec![status(0x0c, &last), answer]);
            type_pin(&screen.attempt, b"1234");
            assert_eq!(
                client.tick(&mut wire),
                Err("invalid login PIN acknowledgement".into())
            );
            assert_eq!(screen.attempt.field_raw(), [0; PIN_CAPACITY]);
        }
    }

    /// Root that ended the operation, or whose deadline passed, as the PIN
    /// arrived answers `9c 01` and drops it; its end follows.
    #[test]
    fn a_pin_root_could_not_take_is_followed_by_its_end() {
        let last = unlock_pin_step();
        let screen = Screen::login(LoginSelection::Unlock);
        let (mut client, mut wire) = at_pin(
            &screen,
            vec![
                status(0x0c, &last),
                vec![0x9c, 1],
                [&ended(0x0d, 0x08, 0)[..], &last.encode()].concat(),
            ],
        );
        type_pin(&screen.attempt, b"1234");
        while client.login.is_some() {
            client.tick(&mut wire).unwrap();
        }
        assert_eq!(sent(&wire, 0x1c), 1);
        assert_eq!(sent(&wire, 0x15), 0);
        assert_eq!(screen.shown(), Some(Notice::Login(&["TIMED OUT"])));
        assert_eq!(screen.attempt.field_raw(), [0; PIN_CAPACITY]);
    }

    /// A wrong PIN is the worker's typed failure, so root ends the
    /// operation: WRONG PIN, and nothing retries. Another attempt is a new
    /// operation from a new chord.
    #[test]
    fn a_wrong_pin_ends_the_operation() {
        for (selection, count) in [
            (LoginSelection::Unlock, 2),
            (LoginSelection::Add, 2),
            (LoginSelection::Enroll(1), 2),
        ] {
            let steps = steps(&selection);
            let step = &steps[count - 1];
            assert!(asks_pin(step));
            let screen = Screen::login(selection.clone());
            let mut replies = presented(&selection, count).0;
            replies.push([&ended(0x0d, 0x01, 7)[..], &step.encode()].concat());
            let (client, wire, result) = drive(&screen, replies);
            assert_eq!(result, Ok(()), "{selection:?}");
            assert!(client.login.is_none());
            assert_eq!(sent(&wire, 0x1c), 1);
            assert_eq!(sent(&wire, 0x14), 0);
            assert_eq!(sent(&wire, 0x15), 0);
            assert_eq!(screen.shown(), Some(Notice::Login(&["WRONG PIN"])));
            assert_eq!(screen.attempt.field_raw(), [0; PIN_CAPACITY]);
        }
    }

    /// The PIN field shares the attempt's lifetime: its deadline while
    /// typing zeroes the field, cancels and says TIMED OUT, and a key after
    /// it types nothing.
    #[test]
    fn a_deadline_while_typing_zeroes_the_field_and_times_out() {
        for tick in [true, false] {
            let last = unlock_pin_step();
            let (screen, deadline, mut client) = Screen::expiring_after(
                LoginSelection::Unlock,
                Duration::from_millis(2000),
                |screen| {
                    let (client, _) = at_pin(screen, vec![]);
                    type_bytes(screen, b"12");
                    assert_ne!(screen.attempt.field_raw(), [0; PIN_CAPACITY]);
                    client
                },
            );
            while Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            if tick {
                // Zeroed by the client's own cancellation, before root's
                // end arrives.
                let mut expired = wire(vec![vec![0x95, 0], status(3, &last)]);
                client.tick(&mut expired).unwrap();
                assert!(client.login.as_ref().unwrap().cancelled);
                assert_eq!(expired.calls, [cancelled(), vec![0x11]]);
                assert_eq!(screen.shown(), Some(Notice::Login(&["TIMED OUT"])));
                assert_eq!(screen.attempt.field_raw(), [0; PIN_CAPACITY]);
                let end = [&ended(0x0d, 0x80, 0)[..], &last.encode()].concat();
                client.tick(&mut wire(vec![end])).unwrap();
                assert!(client.login.is_none());
                assert_eq!(screen.shown(), Some(Notice::Login(&["TIMED OUT"])));
            } else {
                type_bytes(&screen, b"3");
                assert_eq!(screen.field(), Some(Field::Pin(2)));
            }
            assert_eq!(screen.attempt.field_raw(), [0; PIN_CAPACITY]);
        }
    }

    /// At the PIN step with `pin` typed and submitted, root asking for it.
    fn submitted(screen: &Screen, pin: &[u8]) -> (Client, Wire) {
        let (client, mut wire) = at_pin(screen, vec![]);
        type_pin(&screen.attempt, pin);
        wire.replies.push_back(status(0x0c, &unlock_pin_step()));
        (client, wire)
    }

    /// The `1c` request is zeroed as soon as its write returns, before
    /// root's answer is read, and again when dropped.
    #[test]
    fn the_pin_request_is_zeroed_before_roots_answer_is_read() {
        let screen = Screen::login(LoginSelection::Unlock);
        let (mut client, mut wire) = submitted(&screen, b"4321");
        wire.replies.push_back(vec![0x9c, 0]);
        wire_events();
        client.tick(&mut wire).unwrap();
        assert_eq!(sent(&wire, 0x1c), 1);
        let after = after_pins(&wire_events());
        assert_eq!(after.len(), 1);
        assert_eq!(
            after[0][..2],
            [WireEvent::Cleared(true), WireEvent::Received]
        );
        assert_eq!(screen.field(), Some(Field::Touch));
    }

    /// Escape after the PIN was taken but before its write: the check
    /// immediately before the write drops it unsent and zeroed, and the
    /// next poll cancels.
    #[test]
    fn escape_between_the_take_and_the_write_sends_no_pin() {
        let last = unlock_pin_step();
        let screen = Screen::login(LoginSelection::Unlock);
        let (mut client, mut wire) = submitted(&screen, b"4321");
        let attempt = Arc::clone(&screen.attempt);
        TAKEN.with(|taken| *taken.borrow_mut() = Some(Box::new(move || attempt.cancel())));
        wire_events();
        drops();
        client.tick(&mut wire).unwrap();
        assert_eq!(sent(&wire, 0x1c), 0);
        assert!(!wire_events()
            .iter()
            .any(|event| matches!(event, WireEvent::Sent(bytes) if bytes.first() == Some(&0x1c))));
        assert_eq!(drops(), [true]);
        assert_eq!(screen.attempt.field_raw(), [0; PIN_CAPACITY]);
        // Cancelled, as Escape's mark says: the next poll sends `15`.
        let end = [&ended(0x0d, 0x80, 0)[..], &last.encode()].concat();
        wire.replies.extend([vec![0x95, 0], end]);
        let before = wire.calls.len();
        client.tick(&mut wire).unwrap();
        assert_eq!(wire.calls[before..], [cancelled(), vec![0x11]]);
        assert!(client.login.is_none());
        assert_eq!(sent(&wire, 0x1c), 0);
    }

    /// The deadline passing between the take and the write drops the PIN
    /// unsent too, and the attempt times out.
    #[test]
    fn a_deadline_between_the_take_and_the_write_sends_no_pin() {
        let last = unlock_pin_step();
        let (screen, deadline, (mut client, mut wire)) = Screen::expiring_after(
            LoginSelection::Unlock,
            Duration::from_millis(2000),
            |screen| submitted(screen, b"4321"),
        );
        let waited = Arc::new(AtomicBool::new(false));
        TAKEN.with(|taken| {
            let waited = Arc::clone(&waited);
            *taken.borrow_mut() = Some(Box::new(move || {
                while Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(20));
                }
                waited.store(true, Ordering::SeqCst);
            }))
        });
        client.tick(&mut wire).unwrap();
        // The PIN was taken before the deadline, which passed before its
        // write.
        assert!(waited.load(Ordering::SeqCst));
        assert_eq!(sent(&wire, 0x1c), 0);
        let end = [&ended(0x0d, 0x80, 0)[..], &last.encode()].concat();
        wire.replies.extend([vec![0x95, 0], end]);
        client.tick(&mut wire).unwrap();
        assert!(client.login.is_none());
        assert_eq!(sent(&wire, 0x1c), 0);
        assert_eq!(screen.shown(), Some(Notice::Login(&["TIMED OUT"])));
    }

    /// Escape that comes after the write loses the race: the PIN was
    /// committed to the send, and root gets `15` next, as for an Escape
    /// just after it.
    #[test]
    fn escape_after_the_write_is_a_cancellation_that_follows_the_pin() {
        let last = unlock_pin_step();
        let screen = Screen::login(LoginSelection::Unlock);
        let (mut client, inner) = submitted(&screen, b"4321");
        let at = inner.calls.len() + 2;
        let mut wire = CancelAt {
            wire: inner,
            attempt: &screen.attempt,
            at,
        };
        wire.wire.replies.push_back(vec![0x9c, 0]);
        client.tick(&mut wire).unwrap();
        assert_eq!(sent(&wire.wire, 0x1c), 1);
        let end = [&ended(0x0d, 0x80, 0)[..], &last.encode()].concat();
        wire.wire.replies.extend([vec![0x95, 0], end]);
        let before = wire.wire.calls.len();
        client.tick(&mut wire).unwrap();
        assert_eq!(wire.wire.calls[before..], [cancelled(), vec![0x11]]);
        assert!(client.login.is_none());
        assert_eq!(screen.attempt.field_raw(), [0; PIN_CAPACITY]);
    }

    /// A field whose paint is still queued behind a flip takes no key. Once
    /// on glass, a batch delivered late drops the press stamped before the
    /// field was on glass and types the one after.
    #[test]
    fn the_field_takes_only_presses_made_after_its_paint_is_on_glass() {
        let (chain, _log) = crate::drm::testing::chain(800, 600);
        let runtime = Arc::new(Mutex::new(Runtime::new(chain)));
        let held = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let display = {
            let (runtime, held, stop) =
                (Arc::clone(&runtime), Arc::clone(&held), Arc::clone(&stop));
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    if !held.load(Ordering::SeqCst) {
                        let mut runtime = runtime.lock().unwrap();
                        if let Some(frame) = runtime.frame_in_flight() {
                            runtime
                                .output_event(crate::output::OutputEvent::Presented(frame))
                                .unwrap();
                        }
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
            })
        };
        {
            let mut runtime = runtime.lock().unwrap();
            runtime.enable_attention(true);
            runtime
                .attention(&crate::input::test_origin(), true)
                .unwrap();
        }
        let unlock = LoginSelection::Unlock;
        let attempt = Attempt::new(
            crate::input::test_origin(),
            Arc::clone(&runtime),
            Selection::Login(unlock.clone()),
        );
        let steps = steps(&unlock);
        attempt.present(steps[0].clone()).unwrap();
        attempt.present(steps[1].clone()).unwrap();
        held.store(true, Ordering::SeqCst);
        let opening = {
            let attempt = Arc::clone(&attempt);
            let step = steps[1].clone();
            std::thread::spawn(move || attempt.open_field(&step))
        };
        while runtime.lock().unwrap().attention_field_shown() != Some(Field::Pin(0)) {
            std::thread::sleep(Duration::from_millis(1));
        }
        let press = |byte: u8, timestamp: u128| {
            attempt
                .field_key(
                    &crate::input::test_origin(),
                    FieldKey::Byte(byte),
                    timestamp,
                )
                .unwrap();
        };
        // Queued: the field is open but not on glass, so nothing types,
        // however late its press.
        press(b'1', u128::MAX);
        assert!(attempt.field.lock().unwrap().is_open());
        assert!(attempt.field.lock().unwrap().typed().is_empty());
        held.store(false, Ordering::SeqCst);
        opening.join().unwrap().unwrap();
        // A delayed batch: the press stamped when the paint landed, not
        // after it, is dropped; the one after it types.
        let shown = attempt.field.lock().unwrap().shown.unwrap();
        press(b'2', shown);
        press(b'3', shown + 1);
        assert_eq!(attempt.field.lock().unwrap().typed(), b"3");
        stop.store(true, Ordering::SeqCst);
        display.join().unwrap();
    }

    /// A PIN step whose field would not fit beneath its prompt is not
    /// presented: at 800x600 the removal of eight keys fails at its
    /// authorize prompt, before any receipt, so no PIN reaches the key.
    #[test]
    fn a_pin_step_without_room_for_its_field_is_never_presented() {
        use crate::authority::consent::{Slot, LOGIN_KEYS};
        // `removed` of eight keys; a disclosure step carries its key.
        let removal = |removed: u8, step| {
            Request::new(
                NONCE,
                1000,
                Operation::LoginRemove {
                    account: 1000,
                    before: LOGIN_KEYS,
                    after: LOGIN_KEYS - removed,
                    removed: (1..=removed)
                        .map(|position| Slot {
                            position,
                            key: [0xa1; 4],
                        })
                        .collect(),
                    step,
                    proof: None,
                },
            )
            .unwrap()
        };
        let screen = Screen::login_at(LoginSelection::Unlock, 800, 600);
        let authorize = removal(
            LOGIN_KEYS,
            LoginStep::Authorize {
                key: [0xa1; 4],
                retries: 8,
            },
        );
        assert_eq!(
            screen.attempt.present(authorize).unwrap_err(),
            "output cannot hold the PIN field beneath this prompt"
        );
        // A removal's silent first step, which asks no PIN and, leaving
        // two keys, discloses nothing, is shown at that size.
        let screen = Screen::login_at(LoginSelection::Unlock, 800, 600);
        let identify = removal(LOGIN_KEYS - 2, LoginStep::Identify);
        assert!(screen.attempt.present(identify).is_ok());
    }

    /// A paint that fails as the field opens takes it off the screen and
    /// closes it: the prompt alone stays, and no key types.
    #[test]
    fn a_field_whose_paint_fails_is_closed() {
        let screen = Screen::login(LoginSelection::Unlock);
        let steps = steps(&LoginSelection::Unlock);
        screen.attempt.present(steps[0].clone()).unwrap();
        screen.attempt.present(steps[1].clone()).unwrap();
        screen.attempt.runtime.lock().unwrap().fail_next_repaint();
        assert!(screen.attempt.open_field(&steps[1]).is_err());
        assert_eq!(screen.field(), None);
        assert!(!screen.attempt.field.lock().unwrap().is_open());
        type_bytes(&screen, b"12");
        assert!(screen.attempt.field.lock().unwrap().typed().is_empty());
        // A key's failed repaint closes it too.
        screen.attempt.open_field(&steps[1]).unwrap();
        screen.attempt.runtime.lock().unwrap().fail_next_repaint();
        type_bytes(&screen, b"1");
        assert_eq!(screen.field(), None);
        assert!(!screen.attempt.field.lock().unwrap().is_open());
        assert!(!screen.attempt.active());
    }

    /// Before a field opens the client checks this process as td-secret's
    /// worker checks its own: a failure cancels, opens nothing and says why.
    #[test]
    fn swap_or_core_dumps_refuse_the_field() {
        let unlock = LoginSelection::Unlock;
        let last = unlock_pin_step();
        let screen = Screen::login(unlock.clone());
        let mut replies = presented(&unlock, 1).0;
        replies.extend([
            status(4, &last),
            vec![0x93],
            status(0x0c, &last),
            vec![0x95, 0],
            status(3, &last),
            [&ended(0x0d, 0x80, 0)[..], &last.encode()].concat(),
        ]);
        let mut wire = typing(replies, &screen);
        let mut client = Client {
            memory: || Err("active swap".into()),
            ..Client::default()
        };
        client
            .start(&mut wire, Arc::clone(&screen.attempt))
            .unwrap();
        while client.login.is_some() {
            client.tick(&mut wire).unwrap();
        }
        assert!(wire.replies.is_empty());
        assert_eq!(sent(&wire, 0x15), 1);
        assert_eq!(sent(&wire, 0x1c), 0);
        assert_eq!(screen.field(), None);
        // The cancellation's end adds nothing to the reason.
        assert_eq!(screen.shown(), Some(Notice::Login(UNPROTECTED)));
        assert!(UNPROTECTED
            .iter()
            .all(|row| row.len() <= crate::attention::layout(320, 200).1
                && row.bytes().all(crate::ui::is_mapped)));
    }

    #[test]
    fn the_memory_check_is_td_secrets() {
        let header = "Filename Type Size Used Priority\n";
        let limits = "Max core file size        0                    unlimited            bytes\n";
        let swaps = |text: &str| Ok(text.to_string());
        assert!(memory_state(swaps(header), swaps(limits)).is_ok());
        assert!(memory_state(swaps(&format!("{header}\n  \n")), swaps(limits)).is_ok());
        // Swap compiled out.
        assert!(memory_state(Err(io::ErrorKind::NotFound.into()), swaps(limits)).is_ok());
        for refused in [
            swaps(&format!("{header}/swapfile file 1048572 0 -2\n")),
            swaps(""),
            Err(io::ErrorKind::PermissionDenied.into()),
        ] {
            assert!(memory_state(refused, swaps(limits)).is_err());
        }
        for refused in [
            "Max core file size        1                    unlimited            bytes\n",
            "Max core file size        unlimited            unlimited            bytes\n",
            "Max open files            0                    0                    files\n",
            "",
        ] {
            assert!(
                memory_state(swaps(header), swaps(refused)).is_err(),
                "{refused}"
            );
        }
        assert!(memory_state(swaps(header), Err(io::ErrorKind::NotFound.into())).is_err());
        // The client's own check reads this process's tables.
        assert_eq!((Client::default().memory)(), protected_memory());
    }

    /// Every way out of the field leaves its buffer zero: the send, Escape,
    /// the client's cancellation, root's end, a violation, a failed send
    /// and a drop.
    #[test]
    fn every_exit_from_the_field_zeroes_it() {
        let last = unlock_pin_step();
        let zero = [0; PIN_CAPACITY];
        // Escape while typing.
        let screen = Screen::login(LoginSelection::Unlock);
        let _open = at_pin(&screen, vec![]);
        type_bytes(&screen, b"4321");
        assert_ne!(screen.attempt.field_raw(), zero);
        screen.attempt.cancel();
        assert_eq!(screen.attempt.field_raw(), zero);
        // Root ends the operation while the person types.
        let screen = Screen::login(LoginSelection::Unlock);
        let (mut client, mut wire) = at_pin(
            &screen,
            vec![[&ended(0x0d, 0x07, 0)[..], &last.encode()].concat()],
        );
        type_bytes(&screen, b"4321");
        client.tick(&mut wire).unwrap();
        assert!(client.login.is_none());
        assert_eq!(screen.attempt.field_raw(), zero);
        // A violation while the person types.
        let screen = Screen::login(LoginSelection::Unlock);
        let (mut client, mut wire) = at_pin(&screen, vec![status(4, &last)]);
        type_bytes(&screen, b"4321");
        assert!(client.tick(&mut wire).is_err());
        assert_eq!(screen.attempt.field_raw(), zero);
        // The client's own cancellation: the touch request fails to paint.
        let screen = Screen::login(LoginSelection::Unlock);
        let (mut client, mut wire) = at_pin(
            &screen,
            vec![status(0x0c, &last), vec![0x9c, 0], vec![0x95, 0]],
        );
        type_pin(&screen.attempt, b"4321");
        screen.attempt.runtime.lock().unwrap().fail_next_repaint();
        client.tick(&mut wire).unwrap();
        assert_eq!(sent(&wire, 0x15), 1);
        assert!(client.login.as_ref().unwrap().cancelled);
        assert_eq!(screen.attempt.field_raw(), zero);
        assert_eq!(
            screen.shown(),
            Some(Notice::Login(&["THE OPERATION FAILED"]))
        );
        // A send that fails: the field and the request are both zeroed.
        let screen = Screen::login(LoginSelection::Unlock);
        let (mut client, mut wire) = at_pin(&screen, vec![status(0x0c, &last)]);
        type_pin(&screen.attempt, b"4321");
        drops();
        assert_eq!(client.tick(&mut wire), Err("uncertain delivery".into()));
        assert_eq!(wire.calls.last(), Some(&pin_request(&last, b"4321")));
        assert_eq!(drops(), [true]);
        assert_eq!(screen.attempt.field_raw(), zero);
        // A drop.
        let mut field = PinField::default();
        field.open();
        assert_eq!(field.key(FieldKey::Byte(b'7')), Some(1));
        drops();
        drop(field);
        assert_eq!(drops(), [true]);
        // The send itself is `root_asking_for_a_pin_opens_the_field_...`.
    }

    /// The field's bounds: printable ASCII, 4 to 63 bytes, Backspace one
    /// byte at a time, nothing after Enter.
    #[test]
    fn the_field_takes_fidos_pin_profile() {
        let mut field = PinField::default();
        assert_eq!(field.key(FieldKey::Byte(b'1')), None);
        field.open();
        for byte in [0x1f, 0x7f, 0x80, 0xff] {
            assert_eq!(field.key(FieldKey::Byte(byte)), None);
        }
        assert_eq!(field.key(FieldKey::Erase), None);
        for count in 1..=PIN_LONGEST {
            assert_eq!(field.key(FieldKey::Byte(b'~')), Some(count));
        }
        // A 64th byte refuses.
        assert_eq!(field.key(FieldKey::Byte(b' ')), None);
        assert_eq!(field.typed().len(), PIN_LONGEST);
        assert_eq!(field.key(FieldKey::Erase), Some(PIN_LONGEST - 1));
        assert_eq!(field.raw()[PIN_LONGEST - 1], 0);
        field.open();
        for (index, byte) in b"abc".iter().enumerate() {
            assert_eq!(field.key(FieldKey::Byte(*byte)), Some(index + 1));
        }
        // Too short: Enter does nothing.
        assert_eq!(field.key(FieldKey::Submit), None);
        assert!(!field.submitted());
        let mut taken = [0; PIN_CAPACITY];
        assert_eq!(field.take(&mut taken), None);
        assert_eq!(field.key(FieldKey::Byte(b'd')), Some(4));
        assert_eq!(field.key(FieldKey::Submit), None);
        assert!(field.submitted());
        assert_eq!(field.key(FieldKey::Byte(b'e')), None);
        assert_eq!(field.key(FieldKey::Erase), None);
        assert_eq!(field.take(&mut taken[..3]), None);
        assert!(!field.is_open());
        assert_eq!(field.raw(), [0; PIN_CAPACITY]);
        field.open();
        type_into(&mut field, b"abcd");
        assert_eq!(field.take(&mut taken), Some(4));
        assert_eq!(&taken[..4], b"abcd");
        assert_eq!(field.raw(), [0; PIN_CAPACITY]);
        assert_eq!(format!("{:?}", FieldKey::Byte(b'7')), "Byte(..)");
    }

    fn type_into(field: &mut PinField, bytes: &[u8]) {
        for byte in bytes {
            field.key(FieldKey::Byte(*byte));
        }
        field.key(FieldKey::Submit);
    }

    /// The field's screen is drawn from a count: two PINs of one length
    /// paint the same pixels, and another length does not.
    /// On glass: the field opens beneath the presented step's prompt, whose
    /// pixels stay exactly as presented, and typing changes only the
    /// field's band.
    #[test]
    fn the_field_leaves_the_presented_prompt_on_glass() {
        let unlock = LoginSelection::Unlock;
        let last = unlock_pin_step();
        let screen = Screen::login_at(unlock.clone(), 800, 600);
        let mut replies = presented(&unlock, 1).0;
        replies.extend([status(4, &last), vec![0x93]]);
        let mut wire = wire(replies);
        let mut client = login_client();
        client
            .start(&mut wire, Arc::clone(&screen.attempt))
            .unwrap();
        while !wire.replies.is_empty() {
            client.tick(&mut wire).unwrap();
        }
        let prompt = std::fs::read(&screen.path).unwrap();
        assert_eq!(prompt.len(), 3200 * 600);
        wire.replies.push_back(status(0x0c, &last));
        client.tick(&mut wire).unwrap();
        assert_eq!(screen.field(), Some(Field::Pin(0)));
        type_bytes(&screen, b"12345");
        let field = std::fs::read(&screen.path).unwrap();
        // The prompt's last ink row, then its row gap: all untouched.
        let background = &prompt[..4];
        let foot = prompt
            .chunks(3200)
            .rposition(|row| row.chunks(4).any(|pixel| pixel != background))
            .unwrap();
        let kept = (foot + 1 + 8) * 3200;
        assert!(field[..kept] == prompt[..kept]);
        assert!(field[kept..] != prompt[kept..]);
    }

    #[test]
    fn the_field_paints_a_count_never_a_character() {
        // One prompt for every PIN, each typed into the emptied field: the
        // prompt's time left, painted once when shown, is the same for all.
        let screen = Screen::login(LoginSelection::Unlock);
        let _open = at_pin(&screen, vec![]);
        let painted = |pin: &[u8]| {
            type_bytes(&screen, pin);
            assert_eq!(
                screen.field(),
                Some(Field::Pin(u8::try_from(pin.len()).unwrap()))
            );
            let pixels = std::fs::read(&screen.path).unwrap();
            for _ in pin {
                screen
                    .attempt
                    .field_key(
                        &crate::input::test_origin(),
                        FieldKey::Erase,
                        pressed(&screen.attempt),
                    )
                    .unwrap();
            }
            assert_eq!(screen.field(), Some(Field::Pin(0)));
            pixels
        };
        let first = painted(b"ab3#");
        assert_eq!(first, painted(b"ZZ9 "));
        assert_eq!(first, painted(b"}~!."));
        assert_ne!(first, painted(b"ab3"));
    }

    /// Only a login unlock this client committed, with no cancellation
    /// since, leaves the lock surface: not before its commit, not after
    /// Escape, and never another operation's success.
    #[test]
    fn only_a_committed_uncancelled_unlock_leaves_the_lock() {
        let unlock = Screen::login(LoginSelection::Unlock);
        assert!(!unlock.attempt.unlock_committed());
        assert!(unlock.attempt.commit());
        assert!(unlock.attempt.unlock_committed());
        unlock.attempt.cancel();
        assert!(!unlock.attempt.unlock_committed());
        for selection in selections().into_iter().skip(1) {
            let screen = Screen::login(selection);
            assert!(screen.attempt.commit());
            assert!(!screen.attempt.unlock_committed());
        }
        let store = Screen::new();
        assert!(store.attempt.commit());
        assert!(!store.attempt.unlock_committed());
    }

    /// While root's last `1a` reads `02`, no selection sends anything: each
    /// would take the slot. It shows the restart notice instead; every
    /// other revocation byte starts the attempt as before.
    #[test]
    fn no_selection_is_sent_while_root_restarts_after_a_failed_revocation() {
        use crate::authority::Revocation;
        let restarting = || {
            [
                Screen::new(),
                enrollment_screen(Recovery::SecondToken),
                Screen::login(LoginSelection::Unlock),
                Screen::login(LoginSelection::Add),
            ]
        };
        for screen in restarting() {
            let mut client = Client::default();
            client.set_revocation(Revocation::Restarting);
            let mut wire = wire(Vec::new());
            client
                .start(&mut wire, Arc::clone(&screen.attempt))
                .unwrap();
            assert!(wire.calls.is_empty());
            assert_eq!(screen.shown(), Some(Notice::Restarting));
            assert!(client.pending.is_none() && client.inspection.is_none());
            assert!(client.login.is_none());
        }
        // An attempt no longer active is dropped silently.
        let screen = Screen::new();
        screen.attempt.cancel();
        let mut idle = wire(Vec::new());
        let mut client = Client::default();
        client.set_revocation(Revocation::Restarting);
        client
            .start(&mut idle, Arc::clone(&screen.attempt))
            .unwrap();
        assert!(idle.calls.is_empty());
        for revocation in [Revocation::Settled, Revocation::Pending, Revocation::Held] {
            let screen = enrollment_screen(Recovery::SecondToken);
            let mut client = Client::default();
            client.set_revocation(revocation);
            let mut wire = wire(vec![vec![0x97]]);
            client
                .start(&mut wire, Arc::clone(&screen.attempt))
                .unwrap();
            assert_eq!(wire.calls, [vec![0x17]], "{revocation:?}");
        }
    }

    /// The continuations take the slot too: an enrollment's begin after
    /// its inspection, and the new inspection after a failed enrollment,
    /// are not sent once `02` reads.
    #[test]
    fn no_continuation_takes_the_slot_while_root_restarts() {
        use crate::authority::Revocation;
        let screen = enrollment_screen(Recovery::SecondToken);
        let mut root = wire(vec![vec![0x97], vec![0x91, 9, 0]]);
        let mut client = Client::default();
        client
            .start(&mut root, Arc::clone(&screen.attempt))
            .unwrap();
        client.set_revocation(Revocation::Restarting);
        client.tick(&mut root).unwrap();
        assert_eq!(root.calls, [vec![0x17], vec![0x11]]);
        assert_eq!(screen.shown(), Some(Notice::Restarting));
        assert!(client.pending.is_none() && client.inspection.is_none());

        let screen = enrollment_screen(Recovery::SecondToken);
        let initial = enrollment(Recovery::SecondToken, Enrollment::CreatePrimary);
        let mut root = wire(vec![
            vec![0x97],
            vec![0x91, 9, 0],
            description(&[0x92], &initial),
            description(&[0x91, 7], &initial),
        ]);
        let mut client = Client::default();
        client
            .start(&mut root, Arc::clone(&screen.attempt))
            .unwrap();
        client.tick(&mut root).unwrap();
        client.set_revocation(Revocation::Restarting);
        client.tick(&mut root).unwrap();
        assert_eq!(
            root.calls
                .iter()
                .filter(|call| call.first() == Some(&0x17))
                .count(),
            1
        );
        assert_eq!(screen.shown(), Some(Notice::Restarting));
        assert!(client.pending.is_none() && client.inspection.is_none());
    }
}

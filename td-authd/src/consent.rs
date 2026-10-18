//! Immutable public request description shared with the trusted renderer.

use std::time::Duration;

// Every includer declares the shared `td-firstboot/src/hostname.rs` beside
// this module, so a hostname description admits exactly firstboot's names.
use super::hostname::Hostname;

const MAGIC: &[u8; 8] = b"TDCONS01";
const MAX_BYTES: usize = 256;
// Whole-disk installation summary bounds, in displayed (escaped) bytes.
pub const DISK_NAME_BYTES: usize = 31;
pub const MODEL_WIDTH: usize = 32;
pub const SERIAL_WIDTH: usize = 24;
pub const HOSTNAME_BYTES: usize = 63;
pub const USERNAME_BYTES: usize = 32;
const ABSENT_LABEL: u8 = 0xff;
const TRUNCATED_LABEL: u8 = 0x80;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Role {
    Primary,
    Recovery,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Recovery {
    SecondToken,
    Unrecoverable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Enrollment {
    CreatePrimary,
    ProvePrimary,
    CreateRecovery,
    ProveRecovery,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Platform {
    TpmPcr7,
}

/// The most login keys one account enrolls (td-login/TOKEN-LOGIN.md).
pub const LOGIN_KEYS: u8 = 8;

/// One ceremony with one key: root's and the worker's ceiling for an
/// unlock, a removal and a one-key enrollment (td-login/TOKEN-LOGIN.md).
pub const LOGIN_CEREMONY: Duration = Duration::from_secs(120);
/// An addition and a two-key enrollment: two ceremonies.
pub const LOGIN_TWO_CEREMONIES: Duration = Duration::from_secs(240);
/// Added to the ceiling of an operation that shows a disclosure, every
/// first enrollment and a removal leaving at most one key: the person
/// reads it and types its approval key before any token I/O, and that
/// time does not come out of a ceremony's.
pub const LOGIN_READING: Duration = Duration::from_secs(120);
/// A two-key first enrollment's ceiling: two ceremonies and the reading.
pub const LOGIN_TWO_KEY_ENROLLMENT: Duration = Duration::from_secs(360);
/// The longest login ceiling, a two-key first enrollment authorized by a
/// disk proof, which counts as one more ceremony: the worker's bound until
/// a description names its operation (td-authd/DESIGN.md, amendment 10).
pub const LOGIN_LONGEST: Duration = Duration::from_secs(480);

/// How a disk installation stores its volume, as its plan names it
/// (td-install/INSTALLER.md "Storage choice").
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Storage {
    Unencrypted,
    DeviceBound,
}

/// The first four bytes of a credential ID's SHA-256, shown as hex.
pub type Fingerprint = [u8; 4];

/// The disk proof that authorizes a login operation in place of an
/// enrolled key, consent tags 13 to 15 (td-login/TOKEN-LOGIN.md,
/// "Enrollment, addition and removal"): its proof byte.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Proof {
    /// The volume's recovery key, `1`.
    RecoveryKey,
    /// An authorize-phase assertion from a disk security key, `2`.
    DiskKey,
}

impl Proof {
    pub fn byte(self) -> u8 {
        match self {
            Self::RecoveryKey => 1,
            Self::DiskKey => 2,
        }
    }

    pub fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            1 => Some(Self::RecoveryKey),
            2 => Some(Self::DiskKey),
            _ => None,
        }
    }

    /// The proof's step bytes, in their only order.
    fn steps(self) -> &'static [u8] {
        match self {
            Self::RecoveryKey => &[9],
            Self::DiskKey => &[10, 11],
        }
    }
}

/// What the storage worker's baseline offers a disk proof: whether a
/// `recovery-key` marker names a keyslot, and the fingerprints of the
/// header's fido2 tokens, whatever their role.
#[derive(Clone, Copy, Debug)]
pub struct DiskProofs<'a> {
    pub recovery_key: bool,
    pub tokens: &'a [Fingerprint],
}

/// How a selector update writes the pair, tag 16's form byte, which root
/// derives from the volume's state (td-install/ENCRYPTION.md, "Selector
/// update").
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectorForm {
    /// Direct, on a device-bound volume: the next boot needs the recovery
    /// key and a confirmed reseal.
    DeviceBound,
    /// Direct, on a FIDO2-primary volume: every boot asks for a key anyway.
    Fido2,
    /// Staged, on a TPM-primary volume: the next PIN boot completes it.
    Staged,
}

impl SelectorForm {
    pub fn byte(self) -> u8 {
        match self {
            Self::DeviceBound => 1,
            Self::Fido2 => 2,
            Self::Staged => 3,
        }
    }

    pub fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            1 => Some(Self::DeviceBound),
            2 => Some(Self::Fido2),
            3 => Some(Self::Staged),
            _ => None,
        }
    }

    /// The consent rows naming what the next boot does.
    pub fn rows(self) -> [&'static str; 2] {
        match self {
            Self::Staged => [
                "THE NEXT BOOT ASKS FOR THE DISK PIN",
                "AND COMPLETES THE UPDATE",
            ],
            Self::DeviceBound => [
                "THE NEXT BOOT NEEDS THE RECOVERY KEY",
                "AND A CONFIRMED RESEAL",
            ],
            Self::Fido2 => ["EVERY BOOT ASKS FOR A SECURITY KEY", "AS IT DOES NOW"],
        }
    }
}

/// A selector update's ceiling, root's and the worker's: 120 seconds for
/// the worker's reads and the approval key, 120 for the writes
/// (td-authd/DESIGN.md, amendment 10).
pub const SELECTOR_UPDATE_CEILING: Duration = Duration::from_secs(240);

/// The upgrade's cancellation's ceiling, split as a selector update's.
pub const UPGRADE_CANCEL_CEILING: Duration = SELECTOR_UPDATE_CEILING;

/// The protected shape an upgrade chooses, tag 17's shape byte
/// (td-install/ENCRYPTION.md, "Re-encrypting upgrade", phase 1).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UpgradeShape {
    /// TPM primary, `1`: its new keys are fido2-recovery tokens.
    Tpm,
    /// FIDO2 primary, `2`: its new keys are fido2-primary tokens.
    Fido2,
}

impl UpgradeShape {
    pub fn byte(self) -> u8 {
        match self {
            Self::Tpm => 1,
            Self::Fido2 => 2,
        }
    }

    pub fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            1 => Some(Self::Tpm),
            2 => Some(Self::Fido2),
            _ => None,
        }
    }

    /// Whether phase 1 may enroll `count` new keys in this shape: zero to
    /// two recovery keys beside the TPM, one or two primary keys.
    pub fn takes(self, count: u8) -> bool {
        match self {
            Self::Tpm => count <= 2,
            Self::Fido2 => (1..=2).contains(&count),
        }
    }
}

/// An upgrade's ceiling, root's and the worker's: a selector update's
/// 120 seconds for the reads and the review and 120 for the writes, and
/// 120 per new key's ceremony (td-authd/DESIGN.md, amendment 10).
pub fn upgrade_ceiling(count: u8) -> Duration {
    SELECTOR_UPDATE_CEILING.saturating_add(LOGIN_CEREMONY.saturating_mul(u32::from(count)))
}

/// The review step's byte, after the login steps' and the disk proof's.
const REVIEW: u8 = 12;

/// A new key's ceremony, in its only order: connect, create, prove,
/// repeat and probe, a first enrollment's and an upgrade's.
const CEREMONY: &[u8] = &[8, 3, 4, 5, 6];

/// One presented step of an upgrade: its review, which discloses and
/// carries the approval key, or a step of a new key's ceremony.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UpgradeStep {
    Review,
    /// `key` is 1 through the new key count; `step` is connect, create,
    /// prove, repeat or probe.
    Key {
        key: u8,
        step: LoginStep,
    },
}

impl UpgradeStep {
    /// Its key byte, `0` for the review, and its step byte.
    fn position(self) -> (u8, u8) {
        match self {
            Self::Review => (0, REVIEW),
            Self::Key { key, step } => (key, step.byte()),
        }
    }

    /// The ceremony step a key step presents, none for the review.
    pub fn login(self) -> Option<LoginStep> {
        match self {
            Self::Review => None,
            Self::Key { step, .. } => Some(step),
        }
    }
}

/// The step that must follow `position` in an upgrade of `count` new
/// keys, or none after its last.
fn upgrade_following(count: u8, (key, byte): (u8, u8)) -> Option<(u8, u8)> {
    if key == 0 {
        return (count > 0).then_some((1, LoginStep::Connect.byte()));
    }
    let index = CEREMONY.iter().position(|step| *step == byte)?;
    match CEREMONY.get(index.saturating_add(1)) {
        Some(next) => Some((key, *next)),
        None if key < count => Some((key.saturating_add(1), LoginStep::Connect.byte())),
        None => None,
    }
}

/// The renderer's columns at its narrowest accepted output, 320 pixels less
/// 48 of margin in 8-pixel Unifont cells (td-compositor/src/attention.rs).
/// Every login row fits, so wrapping never splits a fingerprint.
pub const PROMPT_COLUMNS: usize = 34;

/// The approval key an elevation prompt, an update's and a login
/// disclosure step's ask for: two ASCII digits, each `2` to `9`, typed in
/// order (td-authd/DESIGN.md, "Elevation operations"). No other value can
/// be constructed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ApprovalKey([u8; 2]);

impl ApprovalKey {
    pub fn new(digits: [u8; 2]) -> Result<Self, String> {
        if digits.iter().all(|digit| (b'2'..=b'9').contains(digit)) {
            Ok(Self(digits))
        } else {
            Err("invalid consent approval key".into())
        }
    }

    /// The two ASCII digits, in the order they are typed.
    pub fn digits(self) -> [u8; 2] {
        self.0
    }
}

/// One key a removal names: its 1-based position in the record's canonical
/// slot order, which the key-management screen's digits also name, and its
/// fingerprint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Slot {
    pub position: u8,
    pub key: Fingerprint,
}

/// What admitting a login step allows next: another step, or, after the
/// operation's final step, only its commit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Admitted {
    Next,
    Last,
}

/// One presented step of a login-key operation. Each step's byte is its
/// client-data phase byte; `Connect` sends no assertion and has none.
/// Retries are the key's own unverified getPINRetries answer. An identified
/// key is the key's own silent selection, not proof of which key it is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoginStep {
    /// Silent selection of the connected enrolled key.
    Identify,
    Authorize {
        key: Fingerprint,
        retries: u8,
    },
    Create {
        retries: u8,
    },
    Prove {
        key: Fingerprint,
        retries: u8,
    },
    Repeat {
        key: Fingerprint,
        retries: u8,
    },
    /// Silent check that the new credential is selectable.
    Probe {
        key: Fingerprint,
    },
    Unlock {
        key: Fingerprint,
        retries: u8,
    },
    /// The person connects the next new key alone.
    Connect,
    /// A disk proof's one step: the person types the volume's recovery
    /// key, which the storage worker tests.
    RecoveryKey,
    /// Silent selection of the connected disk security key among the
    /// header's fido2 tokens.
    DiskIdentify,
    /// The disk security key's PIN and assertion; `key` names a header
    /// fido2 token, never a login slot.
    DiskAuthorize {
        key: Fingerprint,
        retries: u8,
    },
}

impl LoginStep {
    fn byte(self) -> u8 {
        match self {
            Self::Identify => 1,
            Self::Authorize { .. } => 2,
            Self::Create { .. } => 3,
            Self::Prove { .. } => 4,
            Self::Repeat { .. } => 5,
            Self::Probe { .. } => 6,
            Self::Unlock { .. } => 7,
            Self::Connect => 8,
            Self::RecoveryKey => 9,
            Self::DiskIdentify => 10,
            Self::DiskAuthorize { .. } => 11,
        }
    }

    /// Whether the step asks for a secret through the PIN field: a key's
    /// PIN, or the recovery-key step's recovery key.
    pub fn asks_pin(self) -> bool {
        self.retries().is_some() || self == Self::RecoveryKey
    }

    /// Whether the step is a disk proof's, which the storage worker
    /// presents.
    pub fn proves(self) -> bool {
        matches!(
            self,
            Self::RecoveryKey | Self::DiskIdentify | Self::DiskAuthorize { .. }
        )
    }

    fn key(self) -> Option<Fingerprint> {
        match self {
            Self::Authorize { key, .. }
            | Self::Prove { key, .. }
            | Self::Repeat { key, .. }
            | Self::Probe { key }
            | Self::Unlock { key, .. }
            | Self::DiskAuthorize { key, .. } => Some(key),
            Self::Identify
            | Self::Create { .. }
            | Self::Connect
            | Self::RecoveryKey
            | Self::DiskIdentify => None,
        }
    }

    /// Present exactly on the steps that ask for a key's PIN.
    fn retries(self) -> Option<u8> {
        match self {
            Self::Authorize { retries, .. }
            | Self::Create { retries }
            | Self::Prove { retries, .. }
            | Self::Repeat { retries, .. }
            | Self::Unlock { retries, .. }
            | Self::DiskAuthorize { retries, .. } => Some(retries),
            Self::Identify
            | Self::Probe { .. }
            | Self::Connect
            | Self::RecoveryKey
            | Self::DiskIdentify => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LoginKind {
    Unlock,
    Enroll,
    Add,
    Remove,
}

impl LoginKind {
    /// Tags 7 to 10, or 13 to 15 for a disk proof's enrollment, addition
    /// and removal.
    fn tag(self, proof: Option<Proof>) -> u8 {
        let tag = match self {
            Self::Unlock => 7,
            Self::Enroll => 8,
            Self::Add => 9,
            Self::Remove => 10,
        };
        if proof.is_some() {
            tag + 5
        } else {
            tag
        }
    }

    /// Step bytes in their only order; enrollment repeats its key steps,
    /// from connect, per key. A disk proof's steps replace identify and
    /// authorize, and come first in an enrollment.
    fn steps(self, proof: Option<Proof>) -> &'static [u8] {
        match (self, proof) {
            (Self::Unlock, _) => &[1, 7],
            (Self::Enroll, None) => CEREMONY,
            (Self::Enroll, Some(Proof::RecoveryKey)) => &[9, 8, 3, 4, 5, 6],
            (Self::Enroll, Some(Proof::DiskKey)) => &[10, 11, 8, 3, 4, 5, 6],
            (Self::Add, None) => &[1, 2, 8, 3, 4, 5, 6],
            (Self::Add, Some(Proof::RecoveryKey)) => &[9, 8, 3, 4, 5, 6],
            (Self::Add, Some(Proof::DiskKey)) => &[10, 11, 8, 3, 4, 5, 6],
            (Self::Remove, None) => &[1, 2],
            (Self::Remove, Some(proof)) => proof.steps(),
        }
    }
}

/// The fields every login-key operation shares.
struct Login<'a> {
    kind: LoginKind,
    account: u32,
    before: u8,
    after: u8,
    /// Which new key of a first enrollment this step concerns; 1 otherwise.
    key: u8,
    removed: &'a [Slot],
    step: LoginStep,
    /// A disclosure step's approval key, on no other step.
    approval: Option<ApprovalKey>,
    /// The disk proof authorizing it, in every step of tags 13 to 15.
    proof: Option<Proof>,
}

impl Login<'_> {
    fn valid(&self, owner: u32) -> bool {
        let counts = match self.kind {
            LoginKind::Unlock => {
                (1..=LOGIN_KEYS).contains(&self.before) && self.after == self.before
            }
            LoginKind::Enroll => {
                self.before == 0
                    && (1..=2).contains(&self.after)
                    && (1..=self.after).contains(&self.key)
            }
            LoginKind::Add => {
                (1..LOGIN_KEYS).contains(&self.before)
                    && self.before.checked_add(1) == Some(self.after)
            }
            LoginKind::Remove => {
                (1..=LOGIN_KEYS).contains(&self.before)
                    && !self.removed.is_empty()
                    && usize::from(self.before).checked_sub(self.removed.len())
                        == Some(usize::from(self.after))
                    && self
                        .removed
                        .iter()
                        .all(|slot| (1..=self.before).contains(&slot.position))
                    && self
                        .removed
                        .windows(2)
                        .all(|pair| matches!(pair, [low, high] if low.position < high.position))
            }
        };
        self.account == owner
            && counts
            && self.steps().contains(&self.step.byte())
            // A proof is made once, before an enrollment's first key.
            && (!self.step.proves() || self.key == 1)
            && self.approval.is_some() == self.discloses()
    }

    fn steps(&self) -> &'static [u8] {
        self.kind.steps(self.proof)
    }

    /// Whether this step carries a disclosure (td-login/TOKEN-LOGIN.md,
    /// "Enrollment, addition and removal"), and so the approval key: the
    /// operation's first step, of a first enrollment's first key (connect,
    /// or its disk proof's first step) and of a removal that leaves at most
    /// one key (identify, or its disk proof's first step). Each comes
    /// before any token I/O and discloses for the whole operation, so no
    /// later step carries one.
    fn discloses(&self) -> bool {
        self.reads() && self.key == 1 && self.steps().first() == Some(&self.step.byte())
    }

    /// Whether the operation shows a disclosure, at its first step.
    fn reads(&self) -> bool {
        match self.kind {
            LoginKind::Enroll => true,
            LoginKind::Remove => self.after <= 1,
            LoginKind::Unlock | LoginKind::Add => false,
        }
    }

    /// The baseline holds the counted keys (none before an enrollment) and
    /// every removal slot's fingerprint at its position.
    fn matches(&self, baseline: &[Fingerprint]) -> bool {
        let enrolled = match self.kind {
            LoginKind::Enroll => 0,
            _ => self.before,
        };
        baseline.len() == usize::from(enrolled)
            && self.removed.iter().all(|slot| {
                usize::from(slot.position)
                    .checked_sub(1)
                    .and_then(|index| baseline.get(index))
                    == Some(&slot.key)
            })
    }

    /// Root's ceiling, which is the worker's and the compositor's
    /// attempt's: one ceremony per key the person handles in turn, a disk
    /// proof counting as one, and the reading allowance when the operation
    /// discloses.
    fn ceiling(&self) -> Duration {
        // A proof replaces an addition's or removal's authorizing key, but
        // adds a ceremony to an enrollment, which has none.
        let ceremonies = match (self.kind, self.after, self.proof) {
            (LoginKind::Unlock | LoginKind::Remove, _, _) | (LoginKind::Enroll, 1, None) => {
                LOGIN_CEREMONY
            }
            (LoginKind::Add, _, _)
            | (LoginKind::Enroll, 1, Some(_))
            | (LoginKind::Enroll, _, None) => LOGIN_TWO_CEREMONIES,
            (LoginKind::Enroll, _, Some(_)) => LOGIN_TWO_CEREMONIES.saturating_add(LOGIN_CEREMONY),
        };
        if self.reads() {
            ceremonies.saturating_add(LOGIN_READING)
        } else {
            ceremonies
        }
    }

    /// The step that must follow this one, with its new-key ordinal, or none
    /// after the operation's final step.
    fn following(&self) -> Option<(u8, u8)> {
        let steps = self.steps();
        let index = steps.iter().position(|byte| *byte == self.step.byte())?;
        match steps.get(index.saturating_add(1)) {
            Some(step) => Some((*step, self.key)),
            // The next key starts at its connect; a proof is made once.
            None if self.kind == LoginKind::Enroll && self.key < self.after => {
                Some((LoginStep::Connect.byte(), self.key.saturating_add(1)))
            }
            None => None,
        }
    }
}

/// A disk label as the trusted renderer shows it: printable ASCII other than
/// space and backslash as itself, backslash as `\\`, space as `\s`, every
/// other byte as `\xNN`, cut before the first escape that would pass the
/// field's width. A space is escaped so a label cannot imitate an indented
/// continuation or a fixed line; `\s` keeps ordinary model names whole.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Label {
    text: String,
    truncated: bool,
}

impl Label {
    /// A disk's reported model, escaped to its field.
    pub fn model(raw: &[u8]) -> Self {
        Self::escape(raw, MODEL_WIDTH)
    }

    /// A disk's reported serial, escaped to its field.
    pub fn serial(raw: &[u8]) -> Self {
        Self::escape(raw, SERIAL_WIDTH)
    }

    fn escape(raw: &[u8], width: usize) -> Self {
        use std::fmt::Write as _;
        let mut text = String::with_capacity(width);
        let mut truncated = false;
        for byte in raw {
            let unit = if displayed(*byte) {
                1
            } else if matches!(*byte, b'\\' | b' ') {
                2
            } else {
                4
            };
            if text.len().saturating_add(unit) > width {
                truncated = true;
                break;
            }
            // Writing to a String cannot fail.
            let _ = match *byte {
                b'\\' => text.write_str("\\\\"),
                b' ' => text.write_str("\\s"),
                byte if displayed(byte) => text.write_char(char::from(byte)),
                byte => write!(text, "\\x{byte:02x}"),
            };
        }
        Self { text, truncated }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn truncated(&self) -> bool {
        self.truncated
    }

    /// What `escape` emits for some input within `width`; a truncated label
    /// was cut only where no further escape fitted (each is at most 4 bytes).
    fn canonical(&self, width: usize) -> bool {
        if self.text.len() > width || (self.truncated && self.text.len().saturating_add(4) <= width)
        {
            return false;
        }
        let mut rest = self.text.as_bytes();
        loop {
            rest = match rest {
                [] => return true,
                [b'\\', b'\\' | b's', tail @ ..] => tail,
                [b'\\', b'x', high, low, tail @ ..] => match (hex_digit(*high), hex_digit(*low)) {
                    (Some(high), Some(low)) if escaped(high << 4 | low) => tail,
                    _ => return false,
                },
                [byte, tail @ ..] if displayed(*byte) => tail,
                _ => return false,
            };
        }
    }
}

/// Bytes a label shows as themselves.
fn displayed(byte: u8) -> bool {
    byte.is_ascii_graphic() && byte != b'\\'
}

/// Bytes a label shows as `\xNN`; a backslash and a space have their own.
fn escaped(byte: u8) -> bool {
    !displayed(byte) && !matches!(byte, b'\\' | b' ')
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Operation {
    /// `deploy-publish`: install a locally built deployment as current.
    Install {
        key: ApprovalKey,
        deployment: String,
        requester: u32,
    },
    /// Erase one whole disk and install the live medium's deployment on it.
    /// A fixed summary of the plan; the plan itself is bound elsewhere.
    InstallDisk {
        requester: u32,
        disk: String,
        capacity: u64,
        model: Option<Label>,
        serial: Option<Label>,
        hostname: String,
        username: String,
        /// The first eight bytes of the deployment ID, shown as hex.
        deployment: [u8; 8],
        storage: Storage,
    },
    Enroll {
        platform: Platform,
        recovery: Recovery,
        step: Enrollment,
    },
    Unlock {
        role: Role,
    },
    Set {
        role: Role,
        application: String,
        name: String,
        application_uid: u32,
        // Pin the authenticated requester separately; this profile admits only its owner.
        requester: u32,
    },
    /// Unlock the locked session with one enrolled login key.
    LoginUnlock {
        account: u32,
        before: u8,
        after: u8,
        step: LoginStep,
    },
    /// First enrollment of `after` keys; `key` is the one this step creates.
    /// `approval` is the disclosure's key, on the operation's first step
    /// only. `proof`, in every step, is the disk proof authorizing it.
    LoginEnroll {
        account: u32,
        before: u8,
        after: u8,
        key: u8,
        step: LoginStep,
        approval: Option<ApprovalKey>,
        proof: Option<Proof>,
    },
    LoginAdd {
        account: u32,
        before: u8,
        after: u8,
        step: LoginStep,
        proof: Option<Proof>,
    },
    /// `removed` is in strictly increasing slot position. `approval` is the
    /// disclosure's key, on the first step of a removal that leaves at most
    /// one key only.
    LoginRemove {
        account: u32,
        before: u8,
        after: u8,
        removed: Vec<Slot>,
        step: LoginStep,
        approval: Option<ApprovalKey>,
        proof: Option<Proof>,
    },
    /// `deploy-rollback`: move `current` back to `previous`, the pair the
    /// two boot selectors name.
    DeployRollback {
        key: ApprovalKey,
        current: String,
        previous: String,
    },
    /// `set-hostname`: replace the configured name `old` with `new`.
    SetHostname {
        key: ApprovalKey,
        requester: u32,
        old: String,
        new: String,
    },
    /// Storage operation `01`: write the running deployment `current`'s
    /// kernel and selector to the ESP in `form`. One step, confirmed by
    /// typing `key`.
    SelectorUpdate {
        key: ApprovalKey,
        form: SelectorForm,
        current: String,
    },
    /// Storage operation `02`: phase 1 of the re-encrypting upgrade to
    /// `shape`, enrolling `count` new keys, writing the running
    /// deployment `current`'s selector. `approval` is the review's key,
    /// on the review only.
    Upgrade {
        account: u32,
        shape: UpgradeShape,
        count: u8,
        current: String,
        step: UpgradeStep,
        approval: Option<ApprovalKey>,
    },
    /// Storage operation `03`: the upgrade's cancellation in state U0.
    /// One step, confirmed by typing `key`.
    UpgradeCancel {
        key: ApprovalKey,
    },
}

impl Operation {
    fn login(&self) -> Option<Login<'_>> {
        let login =
            |kind, account: &u32, before: &u8, after: &u8, key, removed, step: &LoginStep| Login {
                kind,
                account: *account,
                before: *before,
                after: *after,
                key,
                removed,
                step: *step,
                approval: None,
                proof: None,
            };
        Some(match self {
            Self::LoginUnlock {
                account,
                before,
                after,
                step,
            } => login(LoginKind::Unlock, account, before, after, 1, &[][..], step),
            Self::LoginEnroll {
                account,
                before,
                after,
                key,
                step,
                approval,
                proof,
            } => Login {
                approval: *approval,
                proof: *proof,
                ..login(
                    LoginKind::Enroll,
                    account,
                    before,
                    after,
                    *key,
                    &[][..],
                    step,
                )
            },
            Self::LoginAdd {
                account,
                before,
                after,
                step,
                proof,
            } => Login {
                proof: *proof,
                ..login(LoginKind::Add, account, before, after, 1, &[][..], step)
            },
            Self::LoginRemove {
                account,
                before,
                after,
                removed,
                step,
                approval,
                proof,
            } => Login {
                approval: *approval,
                proof: *proof,
                ..login(
                    LoginKind::Remove,
                    account,
                    before,
                    after,
                    1,
                    removed.as_slice(),
                    step,
                )
            },
            _ => return None,
        })
    }

    /// This login step with `key` as its approval key when the step
    /// carries a disclosure, as root draws one for each such step; every
    /// other step and operation unchanged.
    pub fn disclosed(self, key: ApprovalKey) -> Self {
        if !self.login().is_some_and(|login| login.discloses()) {
            return self;
        }
        match self {
            Self::LoginEnroll {
                account,
                before,
                after,
                key: ordinal,
                step,
                approval: _,
                proof,
            } => Self::LoginEnroll {
                account,
                before,
                after,
                key: ordinal,
                step,
                approval: Some(key),
                proof,
            },
            Self::LoginRemove {
                account,
                before,
                after,
                removed,
                step,
                approval: _,
                proof,
            } => Self::LoginRemove {
                account,
                before,
                after,
                removed,
                step,
                approval: Some(key),
                proof,
            },
            other => other,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Request {
    nonce: [u8; 32],
    owner: u32,
    operation: Operation,
}

impl Request {
    /// The authority supplies fresh entropy and independently admitted identities.
    pub fn new(nonce: [u8; 32], owner: u32, operation: Operation) -> Result<Self, String> {
        if nonce == [0; 32] || !(1000..=65533).contains(&owner) {
            return Err("invalid consent request identity".into());
        }
        if operation.login().is_some_and(|login| !login.valid(owner)) {
            return Err("invalid consent login operation".into());
        }
        match &operation {
            Operation::Install {
                key: _,
                deployment,
                requester,
            } if !deployment_id(deployment) || *requester != owner => {
                return Err("invalid consent installation target".into());
            }
            Operation::InstallDisk {
                requester,
                disk,
                capacity,
                model,
                serial,
                hostname,
                username,
                deployment: _,
                storage: _,
            } if *requester != owner
                || !disk_name(disk)
                || *capacity == 0
                || !model
                    .as_ref()
                    .is_none_or(|label| label.canonical(MODEL_WIDTH))
                || !serial
                    .as_ref()
                    .is_none_or(|label| label.canonical(SERIAL_WIDTH))
                || !shown_name(hostname, HOSTNAME_BYTES)
                || !shown_name(username, USERNAME_BYTES) =>
            {
                return Err("invalid consent disk installation summary".into());
            }
            Operation::Enroll {
                platform: _,
                recovery: Recovery::Unrecoverable,
                step: Enrollment::CreateRecovery | Enrollment::ProveRecovery,
            } => return Err("unrecoverable enrollment cannot enroll a recovery token".into()),
            Operation::Set {
                role: _,
                application,
                name,
                application_uid,
                requester,
            } if !application_name(application)
                || !secret_name(name)
                || !(65536..=2147483647).contains(application_uid)
                || *requester != owner =>
            {
                return Err("invalid consent credential target".into());
            }
            Operation::DeployRollback {
                key: _,
                current,
                previous,
            } if !deployment_id(current) || !deployment_id(previous) || current == previous => {
                return Err("invalid consent rollback pair".into());
            }
            Operation::SelectorUpdate { current, .. } if !deployment_id(current) => {
                return Err("invalid consent selector update deployment".into());
            }
            Operation::Upgrade {
                account,
                shape,
                count,
                current,
                step,
                approval,
            } if *account != owner
                || !shape.takes(*count)
                || !deployment_id(current)
                || approval.is_some() != (*step == UpgradeStep::Review)
                || match step {
                    UpgradeStep::Review => false,
                    UpgradeStep::Key { key, step } => {
                        !(1..=*count).contains(key) || !CEREMONY.contains(&step.byte())
                    }
                } =>
            {
                return Err("invalid consent upgrade".into());
            }
            Operation::SetHostname {
                key: _,
                requester,
                old,
                new,
            } if *requester != owner
                || Hostname::parse(old).is_err()
                || Hostname::parse(new).is_err()
                || old == new =>
            {
                return Err("invalid consent hostname change".into());
            }
            _ => {}
        }
        Ok(Self {
            nonce,
            owner,
            operation,
        })
    }

    pub fn nonce(&self) -> &[u8; 32] {
        &self.nonce
    }
    pub fn owner(&self) -> u32 {
        self.owner
    }
    pub fn operation(&self) -> &Operation {
        &self.operation
    }

    /// Whether this describes a step of a login-key operation.
    pub fn is_login(&self) -> bool {
        self.operation.login().is_some()
    }

    /// The step a login description presents.
    pub fn login_step(&self) -> Option<LoginStep> {
        self.operation.login().map(|login| login.step)
    }

    /// The key-ceremony step a description presents: a login step, or an
    /// upgrade's new key's. Its PIN steps take a `1c`.
    pub fn ceremony_step(&self) -> Option<LoginStep> {
        self.login_step()
            .or(self.upgrade_step().and_then(UpgradeStep::login))
    }

    /// The approval key a login disclosure step carries; none on every
    /// other step and description.
    pub fn login_approval(&self) -> Option<ApprovalKey> {
        self.operation.login().and_then(|login| login.approval)
    }

    /// A login operation's ceiling, root's and the worker's alike, fixed by
    /// its kind and counts and so the same at every step.
    pub fn login_ceiling(&self) -> Option<Duration> {
        self.operation.login().map(|login| login.ceiling())
    }

    /// The ceiling a prompt's time line counts from where the operation
    /// fixes one: a login operation's, a selector update's, an
    /// upgrade's or its cancellation's.
    pub fn ceiling(&self) -> Option<Duration> {
        match self.operation {
            Operation::SelectorUpdate { .. } => Some(SELECTOR_UPDATE_CEILING),
            Operation::Upgrade { count, .. } => Some(upgrade_ceiling(count)),
            Operation::UpgradeCancel { .. } => Some(UPGRADE_CANCEL_CEILING),
            _ => self.login_ceiling(),
        }
    }

    /// The only next presentation in this same enrollment operation.
    pub fn following_enrollment_step(&self) -> Result<Option<Self>, String> {
        let Operation::Enroll {
            platform,
            recovery,
            step,
        } = &self.operation
        else {
            return Ok(None);
        };
        let next = match (*step, *recovery) {
            (Enrollment::CreatePrimary, _) => Enrollment::ProvePrimary,
            (Enrollment::ProvePrimary, Recovery::SecondToken) => Enrollment::CreateRecovery,
            (Enrollment::CreateRecovery, Recovery::SecondToken) => Enrollment::ProveRecovery,
            (Enrollment::ProvePrimary, Recovery::Unrecoverable)
            | (Enrollment::ProveRecovery, Recovery::SecondToken) => return Ok(None),
            _ => return Err("invalid enrollment progression".into()),
        };
        Self::new(
            self.nonce,
            self.owner,
            Operation::Enroll {
                platform: *platform,
                recovery: *recovery,
                step: next,
            },
        )
        .map(Some)
    }

    /// The shape of a login operation's first presentation, without the
    /// record: its step is the operation's first, identify, or connect for
    /// an enrollment's first key, never a later key's.
    pub fn login_start(&self) -> Result<(), String> {
        self.start().map(|_| ())
    }

    fn start(&self) -> Result<Login<'_>, String> {
        let login = self.operation.login().ok_or("not a login operation step")?;
        if login.steps().first() != Some(&login.step.byte()) || login.key != 1 {
            return Err("a login operation begins at its first step".into());
        }
        Ok(login)
    }

    /// The disk proof a login description carries, in every step of tags
    /// 13 to 15; none on every other description.
    pub fn login_proof(&self) -> Option<Proof> {
        self.operation.login().and_then(|login| login.proof)
    }

    /// Root's first presentation of a login operation authorized by a disk
    /// proof: `begin_login`'s rules, a proof, and its first step one the
    /// storage worker's baseline `disk` offers.
    pub fn begin_proof(
        nonce: [u8; 32],
        owner: u32,
        operation: Operation,
        baseline: &[Fingerprint],
        disk: DiskProofs<'_>,
    ) -> Result<Self, String> {
        let request = Self::begun(nonce, owner, operation, baseline)?;
        request.proof_start(disk)?;
        Ok(request)
    }

    /// The login side of a disk proof's first description, before any
    /// storage baseline exists: `begin_login`'s rules against the login
    /// record, a proof, and the proof's first step. It checks the shape
    /// only, not the volume's offer; `begin_proof` against the storage
    /// worker's baseline checks that before anything is presented.
    pub fn begin_proof_shape(
        nonce: [u8; 32],
        owner: u32,
        operation: Operation,
        baseline: &[Fingerprint],
    ) -> Result<Self, String> {
        let request = Self::begun(nonce, owner, operation, baseline)?;
        if request.start()?.proof.is_none() {
            return Err("not a disk proof".into());
        }
        Ok(request)
    }

    /// The shape of a disk proof's first description without the login
    /// record, as the storage worker checks root's: `login_start`, a
    /// proof, and its first step one the worker's own baseline `disk`
    /// offers.
    pub fn proof_start(&self, disk: DiskProofs<'_>) -> Result<(), String> {
        let login = self.start()?;
        if login.proof.is_none() || !offered(login.step, disk) {
            return Err("disk proof the volume does not offer".into());
        }
        Ok(())
    }

    /// The login worker's first frame of an operation a disk proof
    /// authorized, which root sends only once the storage worker proved
    /// it: the proof's last step (recovery key, or disk authorize with
    /// retries that are not zero) of the operation's first key, against
    /// the worker's own baseline. `Last` means only the commit follows, as
    /// for a removal; otherwise `login_successor` admits what does.
    pub fn begin_proven(
        nonce: [u8; 32],
        owner: u32,
        operation: Operation,
        baseline: &[Fingerprint],
    ) -> Result<(Self, Admitted), String> {
        let request = Self::new(nonce, owner, operation)?;
        let login = request
            .operation
            .login()
            .ok_or("not a login operation step")?;
        let last = login.proof.and_then(|proof| proof.steps().last().copied());
        if last != Some(login.step.byte())
            || login.key != 1
            || login.step.retries() == Some(0)
            || !login.matches(baseline)
        {
            return Err("a proven login operation resumes at its proof's last step".into());
        }
        let admitted = match login.following() {
            Some(_) => Admitted::Next,
            None => Admitted::Last,
        };
        Ok((request, admitted))
    }

    /// Root's first presentation of a login operation an enrolled key
    /// authorizes: `login_start`, then `baseline`, the presented record's
    /// slot fingerprints in its canonical order, which the counts and any
    /// removal set must match. A disk proof's begins at `begin_proof`.
    pub fn begin_login(
        nonce: [u8; 32],
        owner: u32,
        operation: Operation,
        baseline: &[Fingerprint],
    ) -> Result<Self, String> {
        let request = Self::begun(nonce, owner, operation, baseline)?;
        if request.login_proof().is_some() {
            return Err("a disk proof begins as one".into());
        }
        Ok(request)
    }

    fn begun(
        nonce: [u8; 32],
        owner: u32,
        operation: Operation,
        baseline: &[Fingerprint],
    ) -> Result<Self, String> {
        let request = Self::new(nonce, owner, operation)?;
        if !request.start()?.matches(baseline) {
            return Err("login step does not match its baseline record".into());
        }
        Ok(request)
    }

    /// The shape of `next` as this login step's successor, from the two
    /// descriptions alone: `next` keeps the nonce, owner, operation,
    /// account, counts and removal slots; its step is the only legal next
    /// one, an enrollment's probe moving to the next key's connect; a
    /// repeat or probe names the credential the step before it named; and
    /// a PIN step's retries are not zero. Whether a fingerprint is the
    /// record's or the created credential needs root's record and the
    /// worker's report, so a step that passes here may still be one root
    /// refuses. `Last` means `next` is the operation's final step; nothing
    /// follows it.
    pub fn login_successor(&self, next: &Self) -> Result<Admitted, String> {
        self.successor(next).map(|(admitted, _, _)| admitted)
    }

    fn successor<'a>(&'a self, next: &'a Self) -> Result<(Admitted, Login<'a>, Login<'a>), String> {
        let (Some(current), Some(following)) = (self.operation.login(), next.operation.login())
        else {
            return Err("not a login operation step".into());
        };
        if next.nonce != self.nonce
            || next.owner != self.owner
            || following.kind != current.kind
            || following.account != current.account
            || following.before != current.before
            || following.after != current.after
            || following.removed != current.removed
            || following.proof != current.proof
        {
            return Err("login step changed its operation".into());
        }
        let Some(order) = current.following() else {
            return Err("login operation has no further step".into());
        };
        if (following.step.byte(), following.key) != order {
            return Err("illegal login step order".into());
        }
        // A ceremony keeps the credential its prove step named.
        if matches!(
            following.step,
            LoginStep::Repeat { .. } | LoginStep::Probe { .. }
        ) && following.step.key() != current.step.key()
        {
            return Err("login step changed its key".into());
        }
        if following.step.retries() == Some(0) {
            return Err("a blocked key cannot take a PIN".into());
        }
        let admitted = match following.following() {
            Some(_) => Admitted::Next,
            None => Admitted::Last,
        };
        Ok((admitted, current, following))
    }

    /// Admits `invitation`, the worker's next step of this login operation:
    /// `login_successor`, then the device data against what root holds.
    /// Root supplies the presented record's slot fingerprints in canonical
    /// order and the credential the worker reported creating in the current
    /// key's ceremony, which exists only between its create and probe steps.
    /// The record must hold the counted keys and every removal slot. A
    /// step's key names a baseline slot (authorize, unlock) or the created
    /// credential (prove, repeat, probe), which no baseline slot names.
    /// `Last` means the admitted step is the operation's final one, after
    /// which only its commit may follow.
    pub fn admit_login_step(
        &self,
        invitation: &Self,
        baseline: &[Fingerprint],
        created: Option<Fingerprint>,
    ) -> Result<Admitted, String> {
        let (admitted, current, next) = self.successor(invitation)?;
        if !current.matches(baseline) {
            return Err("login step does not match its baseline record".into());
        }
        let created_is = |key| created == Some(key) && !baseline.contains(&key);
        let named = match next.step {
            LoginStep::Identify | LoginStep::Connect | LoginStep::Create { .. } => {
                created.is_none()
            }
            LoginStep::Authorize { key, .. } | LoginStep::Unlock { key, .. } => {
                created.is_none() && baseline.contains(&key)
            }
            LoginStep::Prove { key, .. }
            | LoginStep::Repeat { key, .. }
            | LoginStep::Probe { key } => created_is(key),
            // The storage worker's, admitted by `admit_proof_step`.
            LoginStep::RecoveryKey | LoginStep::DiskIdentify | LoginStep::DiskAuthorize { .. } => {
                false
            }
        };
        if !named {
            return Err("login step names a key root did not admit".into());
        }
        Ok(admitted)
    }

    /// Admits `invitation`, the storage worker's next step of a disk
    /// proof: `login_successor`, the login baseline as `admit_login_step`
    /// holds it, and a proof step the storage worker's baseline `disk`
    /// offers, a disk authorize step's key naming one of its fido2 tokens
    /// and no login key, even by a four-byte collision.
    pub fn admit_proof_step(
        &self,
        invitation: &Self,
        baseline: &[Fingerprint],
        disk: DiskProofs<'_>,
    ) -> Result<Admitted, String> {
        let (admitted, current, next) = self.successor(invitation)?;
        if !current.matches(baseline) {
            return Err("login step does not match its baseline record".into());
        }
        if !offered(next.step, disk) {
            return Err("disk proof step the volume does not offer".into());
        }
        if let LoginStep::DiskAuthorize { key, .. } = next.step {
            if baseline.contains(&key) {
                return Err("disk security key names a login key".into());
            }
        }
        Ok(admitted)
    }

    /// The step an upgrade description presents; none on every other.
    pub fn upgrade_step(&self) -> Option<UpgradeStep> {
        match self.operation {
            Operation::Upgrade { step, .. } => Some(step),
            _ => None,
        }
    }

    /// The approval key an upgrade's review or its cancellation carries.
    pub fn upgrade_approval(&self) -> Option<ApprovalKey> {
        match self.operation {
            Operation::Upgrade { approval, .. } => approval,
            Operation::UpgradeCancel { key } => Some(key),
            _ => None,
        }
    }

    /// The shape of an upgrade's first presentation: its review. `Last`
    /// when it enrolls no new key, so only its commit follows.
    pub fn upgrade_start(&self) -> Result<Admitted, String> {
        match self.operation {
            Operation::Upgrade {
                step: UpgradeStep::Review,
                count,
                ..
            } => Ok(if count == 0 {
                Admitted::Last
            } else {
                Admitted::Next
            }),
            Operation::Upgrade { .. } => Err("an upgrade begins at its review".into()),
            _ => Err("not an upgrade step".into()),
        }
    }

    /// The shape of `next` as this upgrade step's successor, from the two
    /// descriptions alone: `next` keeps the nonce, owner, account, shape,
    /// count and deployment; its step is the only legal next one, a key's
    /// probe moving to the next key's connect; a repeat or probe names the
    /// credential its prove named; and a PIN step's retries are not zero.
    /// `Last` means `next` is the upgrade's final step.
    pub fn upgrade_successor(&self, next: &Self) -> Result<Admitted, String> {
        let (
            Operation::Upgrade {
                account,
                shape,
                count,
                current,
                step,
                ..
            },
            Operation::Upgrade {
                account: next_account,
                shape: next_shape,
                count: next_count,
                current: next_current,
                step: next_step,
                ..
            },
        ) = (&self.operation, &next.operation)
        else {
            return Err("not an upgrade step".into());
        };
        if next.nonce != self.nonce
            || next.owner != self.owner
            || next_account != account
            || next_shape != shape
            || next_count != count
            || next_current != current
        {
            return Err("upgrade step changed its operation".into());
        }
        if upgrade_following(*count, step.position()) != Some(next_step.position()) {
            return Err("illegal upgrade step order".into());
        }
        let (current_key, following) = (step.login(), next_step.login());
        if matches!(
            following,
            Some(LoginStep::Repeat { .. } | LoginStep::Probe { .. })
        ) && following.and_then(LoginStep::key) != current_key.and_then(LoginStep::key)
        {
            return Err("upgrade step changed its key".into());
        }
        if following.and_then(LoginStep::retries) == Some(0) {
            return Err("a blocked key cannot take a PIN".into());
        }
        Ok(match upgrade_following(*count, next_step.position()) {
            Some(_) => Admitted::Next,
            None => Admitted::Last,
        })
    }

    /// Admits `invitation`, the storage worker's next upgrade step:
    /// `upgrade_successor`, then the device data. `baseline` is the
    /// fingerprints of the header's fido2 tokens; `created` the credential
    /// the worker reported creating in the current key's ceremony, which
    /// exists only between its create and probe steps. A prove, repeat or
    /// probe names the created credential, which no baseline token names.
    pub fn admit_upgrade_step(
        &self,
        invitation: &Self,
        baseline: &[Fingerprint],
        created: Option<Fingerprint>,
    ) -> Result<Admitted, String> {
        let admitted = self.upgrade_successor(invitation)?;
        let named = match invitation.upgrade_step().and_then(UpgradeStep::login) {
            Some(LoginStep::Connect | LoginStep::Create { .. }) => created.is_none(),
            Some(
                LoginStep::Prove { key, .. }
                | LoginStep::Repeat { key, .. }
                | LoginStep::Probe { key },
            ) => created == Some(key) && !baseline.contains(&key),
            _ => false,
        };
        if !named {
            return Err("upgrade step names a key root did not admit".into());
        }
        Ok(admitted)
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(MAX_BYTES);
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&self.nonce);
        bytes.extend_from_slice(&self.owner.to_be_bytes());
        match &self.operation {
            Operation::Install {
                key,
                deployment,
                requester,
            } => {
                bytes.push(5);
                bytes.extend_from_slice(&key.digits());
                bytes.extend_from_slice(&requester.to_be_bytes());
                bytes.extend_from_slice(deployment.as_bytes());
            }
            Operation::InstallDisk {
                requester,
                disk,
                capacity,
                model,
                serial,
                hostname,
                username,
                deployment,
                storage,
            } => {
                bytes.push(6);
                bytes.extend_from_slice(&requester.to_be_bytes());
                bytes.extend_from_slice(&capacity.to_be_bytes());
                // Construction bounds every string below 128 bytes.
                put_text(&mut bytes, disk);
                put_label(&mut bytes, model.as_ref());
                put_label(&mut bytes, serial.as_ref());
                put_text(&mut bytes, hostname);
                put_text(&mut bytes, username);
                bytes.extend_from_slice(deployment);
                bytes.push(match storage {
                    Storage::Unencrypted => 0,
                    Storage::DeviceBound => 1,
                });
            }
            Operation::Enroll {
                platform,
                recovery,
                step,
            } => {
                bytes.push(1);
                bytes.push(match platform {
                    Platform::TpmPcr7 => 1,
                });
                bytes.push(match recovery {
                    Recovery::SecondToken => 1,
                    Recovery::Unrecoverable => 0,
                });
                bytes.push(match step {
                    Enrollment::CreatePrimary => 1,
                    Enrollment::ProvePrimary => 2,
                    Enrollment::CreateRecovery => 3,
                    Enrollment::ProveRecovery => 4,
                });
            }
            Operation::Unlock { role } => {
                bytes.push(2);
                bytes.push(match role {
                    Role::Primary => 1,
                    Role::Recovery => 2,
                });
            }
            Operation::Set {
                role,
                application,
                name,
                application_uid,
                requester,
            } => {
                bytes.push(4);
                bytes.push(match role {
                    Role::Primary => 1,
                    Role::Recovery => 2,
                });
                bytes.extend_from_slice(&application_uid.to_be_bytes());
                bytes.extend_from_slice(&requester.to_be_bytes());
                // Construction bounds both strings to 64 ASCII bytes.
                bytes.push(application.len() as u8);
                bytes.extend_from_slice(application.as_bytes());
                bytes.push(name.len() as u8);
                bytes.extend_from_slice(name.as_bytes());
            }
            Operation::LoginUnlock { .. }
            | Operation::LoginEnroll { .. }
            | Operation::LoginAdd { .. }
            | Operation::LoginRemove { .. } => {
                if let Some(login) = self.operation.login() {
                    bytes.push(login.kind.tag(login.proof));
                    bytes.extend_from_slice(&login.account.to_be_bytes());
                    if let Some(proof) = login.proof {
                        bytes.push(proof.byte());
                    }
                    bytes.push(login.before);
                    bytes.push(login.after);
                    match login.kind {
                        LoginKind::Enroll => bytes.push(login.key),
                        LoginKind::Remove => {
                            // Construction bounds the set to LOGIN_KEYS.
                            bytes.push(login.removed.len() as u8);
                            for slot in login.removed {
                                bytes.push(slot.position);
                                bytes.extend_from_slice(&slot.key);
                            }
                        }
                        LoginKind::Unlock | LoginKind::Add => {}
                    }
                    bytes.push(login.step.byte());
                    if let Some(key) = login.step.key() {
                        bytes.extend_from_slice(&key);
                    }
                    if let Some(retries) = login.step.retries() {
                        bytes.push(retries);
                    }
                    // Last, since only the step and counts say it belongs.
                    if let Some(approval) = login.approval {
                        bytes.extend_from_slice(&approval.digits());
                    }
                }
            }
            Operation::DeployRollback {
                key,
                current,
                previous,
            } => {
                bytes.push(11);
                bytes.extend_from_slice(&key.digits());
                bytes.extend_from_slice(current.as_bytes());
                bytes.extend_from_slice(previous.as_bytes());
            }
            Operation::SetHostname {
                key,
                requester,
                old,
                new,
            } => {
                bytes.push(12);
                bytes.extend_from_slice(&key.digits());
                bytes.extend_from_slice(&requester.to_be_bytes());
                // Construction bounds both names to 63 bytes.
                put_text(&mut bytes, old);
                put_text(&mut bytes, new);
            }
            Operation::SelectorUpdate { key, form, current } => {
                bytes.push(16);
                bytes.extend_from_slice(&key.digits());
                bytes.push(form.byte());
                bytes.extend_from_slice(current.as_bytes());
            }
            Operation::Upgrade {
                account,
                shape,
                count,
                current,
                step,
                approval,
            } => {
                bytes.push(17);
                bytes.extend_from_slice(&account.to_be_bytes());
                bytes.push(shape.byte());
                bytes.push(*count);
                bytes.extend_from_slice(current.as_bytes());
                let (key, byte) = step.position();
                bytes.push(key);
                bytes.push(byte);
                if let Some(step) = step.login() {
                    if let Some(key) = step.key() {
                        bytes.extend_from_slice(&key);
                    }
                    if let Some(retries) = step.retries() {
                        bytes.push(retries);
                    }
                }
                if let Some(approval) = approval {
                    bytes.extend_from_slice(&approval.digits());
                }
            }
            Operation::UpgradeCancel { key } => {
                bytes.push(18);
                bytes.extend_from_slice(&key.digits());
            }
        }
        bytes
    }

    /// A selector update's approval key; none on every other description.
    pub fn selector_update(&self) -> Option<(ApprovalKey, SelectorForm, &str)> {
        match &self.operation {
            Operation::SelectorUpdate { key, form, current } => Some((*key, *form, current)),
            _ => None,
        }
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > MAX_BYTES {
            return Err("oversized consent request".into());
        }
        let mut input = Input(bytes);
        if input.take(8)? != MAGIC {
            return Err("unsupported consent request version".into());
        }
        let nonce = input
            .take(32)?
            .try_into()
            .map_err(|_| "invalid consent nonce")?;
        let owner = input.number()?;
        let operation = match input.byte()? {
            5 => Operation::Install {
                key: input.approval_key()?,
                requester: input.number()?,
                deployment: input.deployment()?,
            },
            6 => {
                let requester = input.number()?;
                let capacity = input.wide()?;
                Operation::InstallDisk {
                    requester,
                    capacity,
                    disk: input.text()?,
                    model: input.label()?,
                    serial: input.label()?,
                    hostname: input.text()?,
                    username: input.text()?,
                    deployment: input
                        .take(8)?
                        .try_into()
                        .map_err(|_| "truncated deployment prefix")?,
                    storage: match input.byte()? {
                        0 => Storage::Unencrypted,
                        1 => Storage::DeviceBound,
                        _ => return Err("invalid consent disk storage".into()),
                    },
                }
            }
            1 => {
                let platform = match input.byte()? {
                    1 => Platform::TpmPcr7,
                    _ => return Err("unsupported consent platform profile".into()),
                };
                let recovery = match input.byte()? {
                    1 => Recovery::SecondToken,
                    0 => Recovery::Unrecoverable,
                    _ => return Err("invalid consent recovery policy".into()),
                };
                let step = match input.byte()? {
                    1 => Enrollment::CreatePrimary,
                    2 => Enrollment::ProvePrimary,
                    3 => Enrollment::CreateRecovery,
                    4 => Enrollment::ProveRecovery,
                    _ => return Err("invalid consent enrollment step".into()),
                };
                Operation::Enroll {
                    platform,
                    recovery,
                    step,
                }
            }
            2 => Operation::Unlock {
                role: match input.byte()? {
                    1 => Role::Primary,
                    2 => Role::Recovery,
                    _ => return Err("invalid consent token role".into()),
                },
            },
            4 => {
                let role = match input.byte()? {
                    1 => Role::Primary,
                    2 => Role::Recovery,
                    _ => return Err("invalid write token role".into()),
                };
                let application_uid = input.number()?;
                let requester = input.number()?;
                Operation::Set {
                    role,
                    application: input.text()?,
                    name: input.text()?,
                    application_uid,
                    requester,
                }
            }
            tag @ (7..=10 | 13..=15) => {
                let account = input.number()?;
                // Tags 13 to 15 are 8 to 10 with the proof after the account.
                let (tag, proof) = if tag >= 13 {
                    let proof =
                        Proof::from_byte(input.byte()?).ok_or("invalid consent disk proof")?;
                    (tag - 5, Some(proof))
                } else {
                    (tag, None)
                };
                let before = input.byte()?;
                let after = input.byte()?;
                let operation = match tag {
                    7 => Operation::LoginUnlock {
                        account,
                        before,
                        after,
                        step: input.login_step()?,
                    },
                    8 => {
                        let key = input.byte()?;
                        Operation::LoginEnroll {
                            account,
                            before,
                            after,
                            key,
                            step: input.login_step()?,
                            approval: None,
                            proof,
                        }
                    }
                    9 => Operation::LoginAdd {
                        account,
                        before,
                        after,
                        step: input.login_step()?,
                        proof,
                    },
                    _ => {
                        let count = input.byte()?;
                        if count > LOGIN_KEYS {
                            return Err("invalid consent login key set".into());
                        }
                        let mut removed = Vec::with_capacity(usize::from(count));
                        for _ in 0..count {
                            let position = input.byte()?;
                            removed.push(Slot {
                                position,
                                key: input.fingerprint()?,
                            });
                        }
                        Operation::LoginRemove {
                            account,
                            before,
                            after,
                            removed,
                            step: input.login_step()?,
                            approval: None,
                            proof,
                        }
                    }
                };
                // A disclosure step's key ends the value: the step and the
                // counts alone say it belongs, and on any other step two
                // more bytes are trailing.
                if operation.login().is_some_and(|login| login.discloses()) {
                    operation.disclosed(input.approval_key()?)
                } else {
                    operation
                }
            }
            11 => {
                let key = input.approval_key()?;
                let current = input.deployment()?;
                Operation::DeployRollback {
                    key,
                    current,
                    previous: input.deployment()?,
                }
            }
            12 => {
                let key = input.approval_key()?;
                let requester = input.number()?;
                let old = input.text()?;
                Operation::SetHostname {
                    key,
                    requester,
                    old,
                    new: input.text()?,
                }
            }
            16 => {
                let key = input.approval_key()?;
                let form = SelectorForm::from_byte(input.byte()?)
                    .ok_or("invalid consent selector update form")?;
                Operation::SelectorUpdate {
                    key,
                    form,
                    current: input.deployment()?,
                }
            }
            17 => {
                let account = input.number()?;
                let shape = UpgradeShape::from_byte(input.byte()?)
                    .ok_or("invalid consent upgrade shape")?;
                let count = input.byte()?;
                let current = input.deployment()?;
                let (step, approval) = match input.byte()? {
                    0 => {
                        if input.byte()? != REVIEW {
                            return Err("invalid consent upgrade review".into());
                        }
                        (UpgradeStep::Review, Some(input.approval_key()?))
                    }
                    key => (
                        UpgradeStep::Key {
                            key,
                            step: input.login_step()?,
                        },
                        None,
                    ),
                };
                Operation::Upgrade {
                    account,
                    shape,
                    count,
                    current,
                    step,
                    approval,
                }
            }
            18 => Operation::UpgradeCancel {
                key: input.approval_key()?,
            },
            _ => return Err("unknown consent operation".into()),
        };
        if !input.0.is_empty() {
            return Err("trailing consent request bytes".into());
        }
        Self::new(nonce, owner, operation)
    }

    /// Display human-relevant operation arguments; retain the nonce without displaying it.
    pub fn lines(&self) -> Vec<String> {
        let mut lines = vec![
            "TD SECURE ATTENTION".into(),
            format!("SESSION USER {}", self.owner),
        ];
        match &self.operation {
            Operation::Install {
                key, deployment, ..
            } => {
                lines.push("INSTALL BUILT SYSTEM".into());
                lines.push(format!("DEPLOYMENT: {deployment}"));
                lines.push("PREVIOUS SYSTEM KEPT FOR ROLLBACK".into());
                lines.push("RESTART REQUIRED TO USE THIS SYSTEM".into());
                approval_lines(&mut lines, *key);
            }
            Operation::InstallDisk {
                requester: _,
                disk,
                capacity,
                model,
                serial,
                hostname,
                username,
                deployment,
                storage,
            } => {
                lines.push("ERASE DISK AND INSTALL TD".into());
                lines.push(format!("DISK: {disk}"));
                lines.push(format!(
                    "SIZE: {}.{} GB, {capacity} BYTES",
                    capacity / 1_000_000_000,
                    capacity / 100_000_000 % 10
                ));
                lines.push(label_line("MODEL", model.as_ref()));
                lines.push(label_line("SERIAL", serial.as_ref()));
                lines.push("ALL DATA ON THIS DISK WILL BE LOST".into());
                lines.push(format!("HOSTNAME: {hostname}"));
                lines.push(format!("USER: {username}"));
                let prefix = hex(deployment);
                lines.push(format!("DEPLOYMENT: {prefix}..."));
                lines.push(
                    match storage {
                        Storage::Unencrypted => "UNENCRYPTED STORAGE, AUTOMATIC LOGIN",
                        Storage::DeviceBound => "ENCRYPTED TO THIS COMPUTER, AUTOMATIC LOGIN",
                    }
                    .into(),
                );
                lines.push("ENTER: ERASE AND INSTALL   ESC: CANCEL".into());
            }
            Operation::Enroll {
                platform,
                recovery,
                step,
            } => {
                lines.push("ENROLL SECRET STORE".into());
                lines.push(
                    match platform {
                        Platform::TpmPcr7 => "PLATFORM: TPM PCR 7",
                    }
                    .into(),
                );
                lines.push(
                    match recovery {
                        Recovery::SecondToken => "RECOVERY: SECOND RECOVERY TOKEN REQUIRED",
                        Recovery::Unrecoverable => "RECOVERY: NONE. LOST TOKEN LOSES SECRETS",
                    }
                    .into(),
                );
                lines.push(
                    match step {
                        Enrollment::CreatePrimary => "CREATE PRIMARY TOKEN CREDENTIAL",
                        Enrollment::ProvePrimary => "VERIFY PRIMARY TOKEN CREDENTIAL",
                        Enrollment::CreateRecovery => "CREATE RECOVERY TOKEN CREDENTIAL",
                        Enrollment::ProveRecovery => "VERIFY RECOVERY TOKEN CREDENTIAL",
                    }
                    .into(),
                );
                lines.push(
                    match step {
                        Enrollment::CreatePrimary | Enrollment::ProvePrimary => {
                            "TOUCH THE PRIMARY TOKEN"
                        }
                        Enrollment::CreateRecovery | Enrollment::ProveRecovery => {
                            "TOUCH THE RECOVERY TOKEN"
                        }
                    }
                    .into(),
                );
            }
            Operation::Unlock { role } => {
                lines.push("UNLOCK SECRET STORE FOR THIS SESSION".into());
                lines.push(
                    match role {
                        Role::Primary => "TOUCH THE PRIMARY TOKEN",
                        Role::Recovery => "TOUCH THE RECOVERY TOKEN",
                    }
                    .into(),
                );
            }
            Operation::Set {
                role,
                application,
                name,
                application_uid,
                requester,
            } => {
                lines.push("SET ONE APPLICATION CREDENTIAL".into());
                lines.push(format!("REQUESTER UID {requester}"));
                lines.push(format!("APPLICATION UID {application_uid}"));
                lines.push(format!("APPLICATION: {application}"));
                lines.push(format!("CREDENTIAL: {name}"));
                lines.push(
                    match role {
                        Role::Primary => "TOUCH THE PRIMARY TOKEN",
                        Role::Recovery => "TOUCH THE RECOVERY TOKEN",
                    }
                    .into(),
                );
            }
            Operation::LoginUnlock { .. }
            | Operation::LoginEnroll { .. }
            | Operation::LoginAdd { .. }
            | Operation::LoginRemove { .. } => {
                if let Some(login) = self.operation.login() {
                    login_lines(&mut lines, &login);
                }
            }
            Operation::DeployRollback {
                key,
                current,
                previous,
            } => {
                lines.push("ROLL BACK TO THE PREVIOUS SYSTEM".into());
                lines.push(format!("CURRENT: {current}"));
                lines.push(format!("PREVIOUS: {previous}"));
                lines.push("TAKES EFFECT AT THE NEXT RESTART".into());
                approval_lines(&mut lines, *key);
            }
            Operation::SetHostname {
                key,
                requester,
                old,
                new,
            } => {
                lines.push("CHANGE HOSTNAME".into());
                lines.push(format!("REQUESTER UID {requester}"));
                lines.push(format!("OLD NAME: {old}"));
                lines.push(format!("NEW NAME: {new}"));
                lines.push("A RESTART COMPLETES THE CHANGE".into());
                approval_lines(&mut lines, *key);
            }
            Operation::SelectorUpdate { key, form, current } => {
                lines.push("UPDATE THE BOOT SELECTOR ON DISK".into());
                lines.push("FROM THE RUNNING SYSTEM".into());
                lines.push(format!("DEPLOYMENT: {current}"));
                lines.extend(form.rows().map(String::from));
                approval_lines(&mut lines, *key);
            }
            Operation::Upgrade {
                shape,
                count,
                current,
                step,
                approval,
                ..
            } => upgrade_lines(&mut lines, *shape, *count, current, *step, *approval),
            Operation::UpgradeCancel { key } => {
                lines.extend(UPGRADE_CANCEL.iter().map(|row| row.to_string()));
                approval_lines(&mut lines, *key);
            }
        }
        // A key's rows carry their own Escape.
        if !matches!(
            self.operation,
            Operation::Install { .. }
                | Operation::InstallDisk { .. }
                | Operation::DeployRollback { .. }
                | Operation::SetHostname { .. }
                | Operation::SelectorUpdate { .. }
        ) && self.login_approval().is_none()
            && self.upgrade_approval().is_none()
        {
            lines.push("ESC TO CANCEL".into());
        }
        lines
    }
}

pub(crate) fn application_name(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 64
        && text
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase())
        && text.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"-_.".contains(&byte)
        })
}

/// A kernel disk name, as the installation plan admits one.
fn disk_name(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= DISK_NAME_BYTES
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
}

/// A name shown verbatim: printable ASCII without space or backslash, so it
/// cannot imitate an indented continuation, a fixed line or an escape.
fn shown_name(text: &str, max: usize) -> bool {
    !text.is_empty() && text.len() <= max && text.bytes().all(displayed)
}

fn label_line(field: &str, label: Option<&Label>) -> String {
    match label {
        None => format!("{field}: NOT REPORTED"),
        Some(label) if label.text().is_empty() => format!("{field}: REPORTED EMPTY"),
        Some(label) if label.truncated() => format!("{field}: {} (TRUNCATED)", label.text()),
        Some(label) => format!("{field}: {}", label.text()),
    }
}

/// Whether the storage worker's baseline offers this disk proof step: the
/// recovery-key step a `recovery-key` marker, disk identify a fido2 token,
/// and disk authorize the token its fingerprint names. No other step is a
/// proof's.
fn offered(step: LoginStep, disk: DiskProofs<'_>) -> bool {
    match step {
        LoginStep::RecoveryKey => disk.recovery_key,
        LoginStep::DiskIdentify => !disk.tokens.is_empty(),
        LoginStep::DiskAuthorize { key, .. } => disk.tokens.contains(&key),
        _ => false,
    }
}

/// A login operation's rows. The unlock prompt, shown on the lock surface,
/// omits the key count, which unlocking does not change.
fn login_lines(lines: &mut Vec<String>, login: &Login<'_>) {
    let change = format!(
        "({} -> {} {})",
        login.before,
        login.after,
        if login.after == 1 { "KEY" } else { "KEYS" }
    );
    match login.kind {
        LoginKind::Unlock => lines.push("UNLOCK SESSION WITH A LOGIN KEY".into()),
        LoginKind::Enroll => {
            lines.push(format!("ENROLL LOGIN KEYS {change}"));
            lines.push(format!("NEW KEY {} OF {}", login.key, login.after));
        }
        LoginKind::Add => lines.push(format!("ADD A LOGIN KEY {change}")),
        LoginKind::Remove => lines.push(match login.removed.len() {
            1 => format!("REMOVE A LOGIN KEY {change}"),
            count => format!("REMOVE {count} LOGIN KEYS {change}"),
        }),
    }
    lines.push(format!("ACCOUNT UID {}", login.account));
    // Two to a row fit PROMPT_COLUMNS, so wrapping never splits a slot.
    for (index, slots) in login.removed.chunks(2).enumerate() {
        let mut row = String::from(if index == 0 { "REMOVE:" } else { "       " });
        for slot in slots {
            row.push_str(&format!(" {}:{}", slot.position, hex(&slot.key)));
        }
        lines.push(row);
    }
    // A removal of every key says so on each step; its disclosure step
    // says it last, as the disclosure's first row.
    if login.kind == LoginKind::Remove && login.after == 0 && login.approval.is_none() {
        lines.push(NO_KEY.into());
    }
    match login.proof {
        Some(Proof::RecoveryKey) => lines.push("AUTHORIZED BY DISK RECOVERY KEY".into()),
        Some(Proof::DiskKey) => lines.push("AUTHORIZED BY A DISK SECURITY KEY".into()),
        None => {}
    }
    match login.step {
        LoginStep::Identify => lines.push("CONNECT ONLY ONE ENROLLED KEY".into()),
        LoginStep::Connect => {
            if login.kind == LoginKind::Add {
                lines.push("REMOVE THE AUTHORIZING KEY".into());
            } else if login.key > 1 {
                lines.push("REMOVE THE PREVIOUS KEY".into());
            }
            lines.push("CONNECT ONLY THE NEW KEY".into());
        }
        LoginStep::Authorize { key, .. } => {
            lines.push(format!("AUTHORIZE WITH KEY {}", hex(&key)));
        }
        LoginStep::Unlock { key, .. } => lines.push(format!("UNLOCK WITH KEY {}", hex(&key))),
        LoginStep::Create { .. } => lines.push("CREATE A LOGIN CREDENTIAL".into()),
        LoginStep::Prove { key, .. } => lines.push(format!("VERIFY NEW KEY {}", hex(&key))),
        LoginStep::Repeat { key, .. } => {
            lines.push(format!("VERIFY NEW KEY {} AGAIN", hex(&key)));
        }
        LoginStep::Probe { key } => {
            lines.push(format!("CHECKING NEW KEY {}", hex(&key)));
            lines.push("KEEP IT CONNECTED AND TOUCH IT".into());
        }
        LoginStep::RecoveryKey => lines.push("TYPE THE DISK RECOVERY KEY".into()),
        LoginStep::DiskIdentify => lines.push("CONNECT ONE DISK SECURITY KEY ONLY".into()),
        LoginStep::DiskAuthorize { key, .. } => {
            lines.push(format!("DISK SECURITY KEY {}", hex(&key)));
        }
    }
    if let Some(retries) = login.step.retries() {
        lines.push(format!("{retries} PIN ATTEMPTS LEFT ON THIS KEY"));
        lines.push("ENTER ITS PIN, THEN TOUCH THE KEY".into());
    }
    if let Some(key) = login.approval {
        let disclosure: &[&[&str]] = match (login.kind, login.after) {
            (LoginKind::Enroll, 1) => &[ENROLLMENT_DISCLOSURE, ONE_KEY_DISCLOSURE],
            (LoginKind::Enroll, _) => &[ENROLLMENT_DISCLOSURE],
            (_, 0) => &[NO_KEY_DISCLOSURE],
            _ => &[ONE_KEY_DISCLOSURE],
        };
        lines.extend(
            disclosure
                .iter()
                .flat_map(|rows| rows.iter())
                .map(|row| row.to_string()),
        );
        approval_lines(lines, key);
    }
}

/// An upgrade's rows: its shape and count, then the review's ID and
/// disclosure with the approval key, or a key step's rows as a first
/// enrollment's, without the ID its bytes bind.
fn upgrade_lines(
    lines: &mut Vec<String>,
    shape: UpgradeShape,
    count: u8,
    current: &str,
    step: UpgradeStep,
    approval: Option<ApprovalKey>,
) {
    match shape {
        UpgradeShape::Tpm => {
            lines.push("PROTECT THE DISK: TPM AND PIN".into());
            lines.push(format!("NEW RECOVERY KEYS: {count}"));
        }
        UpgradeShape::Fido2 => {
            lines.push("PROTECT THE DISK: SECURITY KEYS".into());
            lines.push(format!("NEW PRIMARY KEYS: {count}"));
        }
    }
    match step {
        UpgradeStep::Review => {
            lines.push(format!("DEPLOYMENT: {current}"));
            lines.extend(UPGRADE_REVIEW.iter().map(|row| row.to_string()));
            lines.push(
                match (shape, count) {
                    (UpgradeShape::Tpm, 0) => "AND FOR A NEW PIN",
                    (UpgradeShape::Tpm, _) => "AND FOR THESE KEYS AND A NEW PIN",
                    (UpgradeShape::Fido2, _) => "AND FOR THESE KEYS",
                }
                .into(),
            );
            if shape == UpgradeShape::Fido2 {
                lines.extend(FIDO2_TRADE_OFFS.iter().map(|row| row.to_string()));
            }
            if let Some(key) = approval {
                approval_lines(lines, key);
            }
        }
        UpgradeStep::Key { key, step } => {
            lines.push(format!("NEW KEY {key} OF {count}"));
            match step {
                LoginStep::Connect => {
                    if key > 1 {
                        lines.push("REMOVE THE PREVIOUS KEY".into());
                    }
                    lines.push("CONNECT ONLY THE NEW KEY".into());
                }
                LoginStep::Create { .. } => lines.push("CREATE A DISK CREDENTIAL".into()),
                LoginStep::Prove { key, .. } => lines.push(format!("VERIFY NEW KEY {}", hex(&key))),
                LoginStep::Repeat { key, .. } => {
                    lines.push(format!("VERIFY NEW KEY {} AGAIN", hex(&key)));
                }
                LoginStep::Probe { key } => {
                    lines.push(format!("CHECKING NEW KEY {}", hex(&key)));
                    lines.push("KEEP IT CONNECTED AND TOUCH IT".into());
                }
                // Construction admits only the ceremony's steps.
                _ => {}
            }
            if let Some(retries) = step.retries() {
                lines.push(format!("{retries} PIN ATTEMPTS LEFT ON THIS KEY"));
                lines.push("ENTER ITS PIN, THEN TOUCH THE KEY".into());
            }
        }
    }
}

/// The review's disclosure (td-install/ENCRYPTION.md, phase 1), before
/// the row naming what the upgrade boot asks for. Every row fits
/// `PROMPT_COLUMNS` and the renderer's glyphs.
const UPGRADE_REVIEW: &[&str] = &[
    "THE NEXT BOOT REWRITES THE DISK",
    "ONE READ AND ONE OR TWO WRITES",
    "OF THE WHOLE VOLUME",
    "IT NEEDS MAINS POWER",
    "THE MACHINE IS UNUSABLE UNTIL DONE",
    "IT ASKS FOR THE RECOVERY KEY",
    "WHICH STAYS THE RECOVERY PROTECTOR",
];

/// The FIDO2 shape's two trade-offs, which its choice discloses.
const FIDO2_TRADE_OFFS: &[&str] = &[
    "AN ALTERED BOOT CAN TAKE THE DISK",
    "FROM ONE USE OF A KEY AND PIN",
    "WHILE A FIRMWARE TPM GUESS COUNT",
    "CAN ROLL BACK WITH ITS FLASH",
];

/// The cancellation's rows, before its approval key's.
const UPGRADE_CANCEL: &[&str] = &[
    "CANCEL PROTECTING THE DISK",
    "THE NEXT BOOT MAY NEED",
    "THE RECOVERY KEY AND A RESEAL",
];

/// What a first enrollment changes (td-login/TOKEN-LOGIN.md, "Enrollment,
/// addition and removal"). Every disclosure row fits `PROMPT_COLUMNS`.
const ENROLLMENT_DISCLOSURE: &[&str] = &[
    "BOOT AND LOCK NEED A KEY AND PIN",
    "CONSOLE AND SSH SESSIONS END NOW",
    "SSH ONLY FOR THE PRIMARY ACCOUNT",
    "ADMINISTRATION NEEDS A LOGIN",
    "PIN SET ON ANOTHER LAYOUT MAY FAIL",
];

/// What one enrolled key risks: a one-key enrollment's addition to the
/// enrollment disclosure, and a removal that leaves one key.
const ONE_KEY_DISCLOSURE: &[&str] = &[
    "LOST KEY OR BLOCKED PIN: NO LOGIN",
    "RECOVERY NEEDS BOOTING OTHER CODE",
    "HERE, WHICH ALSO BYPASSES THE LOCK",
    "A BACKUP KEY CAN BE ADDED LATER",
];

const NO_KEY: &str = "LOGIN WILL NOT NEED A KEY";

/// A removal of every key: login returns to needing none, and the cutover
/// back restarts `sshd`, ending its open sessions.
const NO_KEY_DISCLOSURE: &[&str] = &[
    NO_KEY,
    "CONSOLE LOGIN AND THE ORDINARY",
    "SSH POLICY RETURN",
    "OPEN SSH SESSIONS END NOW",
];

/// The approval key's rows, last on an elevation prompt, which carry its
/// Escape. Each fits `PROMPT_COLUMNS`, so wrapping never splits the key.
fn approval_lines(lines: &mut Vec<String>, key: ApprovalKey) {
    let [first, second] = key.digits();
    lines.push(format!(
        "APPROVE: TYPE {} THEN {}",
        char::from(first),
        char::from(second)
    ));
    lines.push("ESC: CANCEL".into());
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut text = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        // Writing to a String cannot fail.
        let _ = write!(text, "{byte:02x}");
    }
    text
}

fn put_text(bytes: &mut Vec<u8>, text: &str) {
    bytes.push(text.len() as u8);
    bytes.extend_from_slice(text.as_bytes());
}

fn put_label(bytes: &mut Vec<u8>, label: Option<&Label>) {
    match label {
        None => bytes.push(ABSENT_LABEL),
        Some(label) => {
            let length = label.text.len() as u8;
            bytes.push(if label.truncated {
                length | TRUNCATED_LABEL
            } else {
                length
            });
            bytes.extend_from_slice(label.text.as_bytes());
        }
    }
}

fn secret_name(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 64
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
}

struct Input<'a>(&'a [u8]);
impl<'a> Input<'a> {
    fn take(&mut self, count: usize) -> Result<&'a [u8], String> {
        let bytes = self.0.get(..count).ok_or("truncated consent request")?;
        self.0 = self.0.get(count..).ok_or("truncated consent request")?;
        Ok(bytes)
    }
    fn byte(&mut self) -> Result<u8, String> {
        self.take(1)?
            .first()
            .copied()
            .ok_or_else(|| "truncated consent byte".into())
    }
    fn number(&mut self) -> Result<u32, String> {
        Ok(u32::from_be_bytes(
            self.take(4)?
                .try_into()
                .map_err(|_| "truncated consent number")?,
        ))
    }
    fn text(&mut self) -> Result<String, String> {
        let length = usize::from(self.byte()?);
        String::from_utf8(self.take(length)?.to_vec()).map_err(|_| "invalid consent text".into())
    }
    fn wide(&mut self) -> Result<u64, String> {
        Ok(u64::from_be_bytes(
            self.take(8)?
                .try_into()
                .map_err(|_| "truncated consent number")?,
        ))
    }
    fn approval_key(&mut self) -> Result<ApprovalKey, String> {
        ApprovalKey::new(
            self.take(2)?
                .try_into()
                .map_err(|_| "truncated consent approval key")?,
        )
    }
    fn deployment(&mut self) -> Result<String, String> {
        String::from_utf8(self.take(64)?.to_vec())
            .map_err(|_| "invalid deployment ID encoding".into())
    }
    fn fingerprint(&mut self) -> Result<Fingerprint, String> {
        self.take(4)?
            .try_into()
            .map_err(|_| "truncated consent fingerprint".into())
    }
    fn login_step(&mut self) -> Result<LoginStep, String> {
        Ok(match self.byte()? {
            1 => LoginStep::Identify,
            2 => {
                let key = self.fingerprint()?;
                LoginStep::Authorize {
                    key,
                    retries: self.byte()?,
                }
            }
            3 => LoginStep::Create {
                retries: self.byte()?,
            },
            4 => {
                let key = self.fingerprint()?;
                LoginStep::Prove {
                    key,
                    retries: self.byte()?,
                }
            }
            5 => {
                let key = self.fingerprint()?;
                LoginStep::Repeat {
                    key,
                    retries: self.byte()?,
                }
            }
            6 => LoginStep::Probe {
                key: self.fingerprint()?,
            },
            7 => {
                let key = self.fingerprint()?;
                LoginStep::Unlock {
                    key,
                    retries: self.byte()?,
                }
            }
            8 => LoginStep::Connect,
            9 => LoginStep::RecoveryKey,
            10 => LoginStep::DiskIdentify,
            11 => {
                let key = self.fingerprint()?;
                LoginStep::DiskAuthorize {
                    key,
                    retries: self.byte()?,
                }
            }
            _ => return Err("invalid consent login step".into()),
        })
    }
    fn label(&mut self) -> Result<Option<Label>, String> {
        let head = self.byte()?;
        if head == ABSENT_LABEL {
            return Ok(None);
        }
        let length = usize::from(head & !TRUNCATED_LABEL);
        Ok(Some(Label {
            text: String::from_utf8(self.take(length)?.to_vec())
                .map_err(|_| "invalid consent label")?,
            truncated: head & TRUNCATED_LABEL != 0,
        }))
    }
}

pub(crate) fn deployment_id(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    #[test]
    fn enrollment_progression_preserves_identity_and_stops_at_the_selected_final_proof() {
        for recovery in [Recovery::SecondToken, Recovery::Unrecoverable] {
            let mut request = Request::new(
                [42; 32],
                1000,
                Operation::Enroll {
                    platform: Platform::TpmPcr7,
                    recovery,
                    step: Enrollment::CreatePrimary,
                },
            )
            .unwrap();
            let mut expected = vec![Enrollment::ProvePrimary];
            if recovery == Recovery::SecondToken {
                expected.extend([Enrollment::CreateRecovery, Enrollment::ProveRecovery]);
            }
            for step in expected {
                request = request.following_enrollment_step().unwrap().unwrap();
                assert_eq!(
                    request,
                    Request::new(
                        [42; 32],
                        1000,
                        Operation::Enroll {
                            platform: Platform::TpmPcr7,
                            recovery,
                            step,
                        }
                    )
                    .unwrap()
                );
            }
            assert_eq!(request.following_enrollment_step().unwrap(), None);
        }
        let unlock = Request::new(
            [42; 32],
            1000,
            Operation::Unlock {
                role: Role::Primary,
            },
        )
        .unwrap();
        assert_eq!(unlock.following_enrollment_step().unwrap(), None);
    }

    const CURRENT: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const PREVIOUS: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

    fn approval(digits: &[u8; 2]) -> ApprovalKey {
        ApprovalKey::new(*digits).unwrap()
    }

    fn rollback(current: &str, previous: &str) -> Operation {
        Operation::DeployRollback {
            key: approval(b"47"),
            current: current.into(),
            previous: previous.into(),
        }
    }

    fn approval_key(request: &Request) -> Option<ApprovalKey> {
        match request.operation() {
            Operation::Install { key, .. }
            | Operation::DeployRollback { key, .. }
            | Operation::SetHostname { key, .. }
            | Operation::SelectorUpdate { key, .. } => Some(*key),
            _ => None,
        }
    }

    fn selector_update(form: SelectorForm, current: &str) -> Operation {
        Operation::SelectorUpdate {
            key: approval(b"58"),
            form,
            current: current.into(),
        }
    }

    fn upgrade(shape: UpgradeShape, count: u8, step: UpgradeStep) -> Operation {
        Operation::Upgrade {
            account: 1000,
            shape,
            count,
            current: CURRENT.into(),
            step,
            approval: (step == UpgradeStep::Review).then(|| approval(b"58")),
        }
    }

    fn key_step(key: u8, step: LoginStep) -> UpgradeStep {
        UpgradeStep::Key { key, step }
    }

    /// Every step of an upgrade of `count` new keys, in its only order,
    /// the created credentials `0x0k0k0k0k`.
    fn upgrade_steps(count: u8) -> Vec<UpgradeStep> {
        let mut steps = vec![UpgradeStep::Review];
        for key in 1..=count {
            let made = [key; 4];
            steps.extend([
                key_step(key, LoginStep::Connect),
                key_step(key, LoginStep::Create { retries: 8 }),
                key_step(
                    key,
                    LoginStep::Prove {
                        key: made,
                        retries: 8,
                    },
                ),
                key_step(
                    key,
                    LoginStep::Repeat {
                        key: made,
                        retries: 7,
                    },
                ),
                key_step(key, LoginStep::Probe { key: made }),
            ]);
        }
        steps
    }

    fn rename(requester: u32, old: &str, new: &str) -> Operation {
        Operation::SetHostname {
            key: approval(b"29"),
            requester,
            old: old.into(),
            new: new.into(),
        }
    }

    fn install(deployment: &str, requester: u32) -> Operation {
        Operation::Install {
            key: approval(b"58"),
            deployment: deployment.into(),
            requester,
        }
    }

    /// Tag 5 carries the approval key right after its tag byte, then the
    /// requester and one canonical ID, 115 bytes in all; its rows end with
    /// the key's, and no row offers Enter.
    #[test]
    fn installation_has_one_canonical_id_and_the_requester_must_be_its_owner() {
        for id in [
            "a".repeat(63),
            "a".repeat(65),
            "A".repeat(64),
            "g".repeat(64),
            format!("{}\n", "a".repeat(63)),
        ] {
            assert!(Request::new([1; 32], 1000, install(&id, 1000)).is_err());
        }
        assert!(Request::new([1; 32], 1000, install(&"a".repeat(64), 1001)).is_err());
        let request = Request::new([1; 32], 1000, install(&"a".repeat(64), 1000)).unwrap();
        let mut literal = b"TDCONS01".to_vec();
        literal.extend([1; 32]);
        literal.extend([0, 0, 3, 232, 5, b'5', b'8', 0, 0, 3, 232]);
        literal.extend([b'a'; 64]);
        assert_eq!(literal.len(), 115);
        assert_eq!(request.encode(), literal);
        assert_eq!(Request::decode(&literal).unwrap(), request);
        assert_eq!(
            request.lines(),
            [
                "TD SECURE ATTENTION".to_string(),
                "SESSION USER 1000".into(),
                "INSTALL BUILT SYSTEM".into(),
                format!("DEPLOYMENT: {}", "a".repeat(64)),
                "PREVIOUS SYSTEM KEPT FOR ROLLBACK".into(),
                "RESTART REQUIRED TO USE THIS SYSTEM".into(),
                "APPROVE: TYPE 5 THEN 8".into(),
                "ESC: CANCEL".into(),
            ]
        );
        assert!(!request.lines().iter().any(|line| line.contains("ENTER")));
        // L4's keyless tag-5 value no longer decodes.
        let mut keyless = b"TDCONS01".to_vec();
        keyless.extend([1; 32]);
        keyless.extend([0, 0, 3, 232, 5, 0, 0, 3, 232]);
        keyless.extend([b'a'; 64]);
        assert_eq!(
            Request::decode(&keyless).err().unwrap(),
            "invalid consent approval key"
        );
    }

    /// The literal tag-11 and tag-12 values: the key right after the tag
    /// byte, then the fields in their order, and each one's rows.
    #[test]
    fn elevation_descriptions_have_independent_literal_encodings_and_rows() {
        let request = Request::new([1; 32], 1000, rollback(CURRENT, PREVIOUS)).unwrap();
        let mut literal = b"TDCONS01".to_vec();
        literal.extend([1; 32]);
        literal.extend([0, 0, 3, 232, 11, b'4', b'7']);
        literal.extend(CURRENT.bytes());
        literal.extend(PREVIOUS.bytes());
        assert_eq!(literal.len(), 175);
        assert_eq!(request.encode(), literal);
        assert_eq!(Request::decode(&literal).unwrap(), request);
        assert_eq!(
            request.lines(),
            [
                "TD SECURE ATTENTION".to_string(),
                "SESSION USER 1000".into(),
                "ROLL BACK TO THE PREVIOUS SYSTEM".into(),
                format!("CURRENT: {CURRENT}"),
                format!("PREVIOUS: {PREVIOUS}"),
                "TAKES EFFECT AT THE NEXT RESTART".into(),
                "APPROVE: TYPE 4 THEN 7".into(),
                "ESC: CANCEL".into(),
            ]
        );

        let request = Request::new([1; 32], 1000, rename(1000, "td", "my-laptop")).unwrap();
        let mut literal = b"TDCONS01".to_vec();
        literal.extend([1; 32]);
        literal.extend([0, 0, 3, 232, 12, b'2', b'9', 0, 0, 3, 232, 2]);
        literal.extend(b"td");
        literal.push(9);
        literal.extend(b"my-laptop");
        assert_eq!(request.encode(), literal);
        assert_eq!(Request::decode(&literal).unwrap(), request);
        assert_eq!(
            request.lines(),
            [
                "TD SECURE ATTENTION",
                "SESSION USER 1000",
                "CHANGE HOSTNAME",
                "REQUESTER UID 1000",
                "OLD NAME: td",
                "NEW NAME: my-laptop",
                "A RESTART COMPLETES THE CHANGE",
                "APPROVE: TYPE 2 THEN 9",
                "ESC: CANCEL",
            ]
        );
    }

    /// Bytes 45 and 46 are the key, in typing order, for every tag that
    /// carries one: a swapped pair is the other key, never the same one or
    /// a refusal.
    #[test]
    fn the_approval_key_follows_the_tag_byte_in_typing_order() {
        for operation in [
            install(CURRENT, 1000),
            rollback(CURRENT, PREVIOUS),
            rename(1000, "td", "my-laptop"),
            selector_update(SelectorForm::Staged, CURRENT),
        ] {
            let request = Request::new([1; 32], 1000, operation).unwrap();
            let bytes = request.encode();
            let key = approval_key(&request).unwrap();
            assert_eq!(bytes[45..47], key.digits());
            let mut swapped = bytes.clone();
            swapped.swap(45, 46);
            let decoded = Request::decode(&swapped).unwrap();
            let [first, second] = key.digits();
            assert_eq!(approval_key(&decoded).unwrap().digits(), [second, first]);
            assert_eq!(
                decoded.lines().iter().rev().nth(1).unwrap(),
                &format!(
                    "APPROVE: TYPE {} THEN {}",
                    char::from(second),
                    char::from(first)
                )
            );
        }
    }

    /// Exactly the 64 keys of two digits from 2 to 9 exist, and a decoder
    /// refuses every other byte at either position of every tag carrying
    /// one.
    #[test]
    fn every_approval_digit_outside_two_to_nine_refuses() {
        let mut admitted = 0;
        for first in 0..=255u8 {
            for second in 0..=255u8 {
                let key = ApprovalKey::new([first, second]);
                let digits = (b'2'..=b'9').contains(&first) && (b'2'..=b'9').contains(&second);
                assert_eq!(key.is_ok(), digits, "{first} {second}");
                if digits {
                    admitted += 1;
                    assert_eq!(key.unwrap().digits(), [first, second]);
                }
            }
        }
        assert_eq!(admitted, 64);
        for operation in [
            install(CURRENT, 1000),
            rollback(CURRENT, PREVIOUS),
            rename(1000, "td", "my-laptop"),
            selector_update(SelectorForm::Staged, CURRENT),
        ] {
            let bytes = Request::new([1; 32], 1000, operation).unwrap().encode();
            for index in [45, 46] {
                for byte in 0..=255u8 {
                    let mut changed = bytes.clone();
                    changed[index] = byte;
                    let decoded = Request::decode(&changed);
                    if (b'2'..=b'9').contains(&byte) {
                        assert!(decoded.is_ok(), "{index} {byte}");
                    } else {
                        assert_eq!(
                            decoded.err().unwrap(),
                            "invalid consent approval key",
                            "{index} {byte}"
                        );
                    }
                }
            }
        }
    }

    /// Tag 17: the account, shape, count and running deployment's ID,
    /// then the step, 119 bytes for the review and 122 at the widest; tag
    /// 18: the key alone, 47 bytes (td-authd/DESIGN.md, amendment 10).
    #[test]
    fn an_upgrade_and_its_cancellation_encode_exactly() {
        let mut header = b"TDCONS01".to_vec();
        header.extend([1; 32]);
        header.extend(1000u32.to_be_bytes());
        assert_eq!(header.len(), 44);
        let review = Request::new(
            [1; 32],
            1000,
            upgrade(UpgradeShape::Tpm, 2, UpgradeStep::Review),
        )
        .unwrap();
        let mut literal = header.clone();
        literal.push(17);
        literal.extend(1000u32.to_be_bytes());
        literal.extend([1, 2]);
        literal.extend(CURRENT.as_bytes());
        literal.extend([0, 12, b'5', b'8']);
        assert_eq!(literal.len(), 119);
        assert_eq!(review.encode(), literal);
        assert_eq!(Request::decode(&literal).unwrap(), review);
        assert_eq!(review.upgrade_approval(), Some(approval(b"58")));
        assert!(!review.is_login());
        // The widest value: a prove or repeat step.
        let prove = Request::new(
            [1; 32],
            1000,
            upgrade(
                UpgradeShape::Fido2,
                2,
                key_step(
                    2,
                    LoginStep::Repeat {
                        key: [9, 8, 7, 6],
                        retries: 3,
                    },
                ),
            ),
        )
        .unwrap();
        let mut literal = header.clone();
        literal.push(17);
        literal.extend(1000u32.to_be_bytes());
        literal.extend([2, 2]);
        literal.extend(CURRENT.as_bytes());
        literal.extend([2, 5, 9, 8, 7, 6, 3]);
        assert_eq!(literal.len(), 122);
        assert_eq!(prove.encode(), literal);
        assert_eq!(Request::decode(&literal).unwrap(), prove);
        assert_eq!(prove.upgrade_approval(), None);
        for step in upgrade_steps(2) {
            let request = Request::new([1; 32], 1000, upgrade(UpgradeShape::Tpm, 2, step)).unwrap();
            assert!(request.encode().len() <= 122);
            assert_eq!(Request::decode(&request.encode()).unwrap(), request);
            assert_eq!(request.upgrade_step(), Some(step));
        }
        let cancel = Request::new(
            [1; 32],
            1000,
            Operation::UpgradeCancel {
                key: approval(b"47"),
            },
        )
        .unwrap();
        let mut literal = header;
        literal.extend([18, b'4', b'7']);
        assert_eq!(literal.len(), 47);
        assert_eq!(cancel.encode(), literal);
        assert_eq!(Request::decode(&literal).unwrap(), cancel);
        assert_eq!(cancel.upgrade_approval(), Some(approval(b"47")));
        assert_eq!(cancel.upgrade_step(), None);
        assert_eq!(cancel.ceiling(), Some(UPGRADE_CANCEL_CEILING));
        for end in 0..literal.len() {
            assert!(Request::decode(&literal[..end]).is_err(), "{end}");
        }
        literal.push(0);
        assert!(Request::decode(&literal).is_err());
    }

    #[test]
    fn an_upgrade_review_discloses_phase_one_and_a_key_step_shows_its_ceremony() {
        let deployment = format!("DEPLOYMENT: {CURRENT}");
        let review = |shape, count| {
            Request::new([1; 32], 1000, upgrade(shape, count, UpgradeStep::Review))
                .unwrap()
                .lines()
        };
        let disclosure = [
            "THE NEXT BOOT REWRITES THE DISK",
            "ONE READ AND ONE OR TWO WRITES",
            "OF THE WHOLE VOLUME",
            "IT NEEDS MAINS POWER",
            "THE MACHINE IS UNUSABLE UNTIL DONE",
            "IT ASKS FOR THE RECOVERY KEY",
            "WHICH STAYS THE RECOVERY PROTECTOR",
        ];
        let expected = |head: [&str; 2], asks: &str, more: &[&str]| {
            let mut rows = vec![
                "TD SECURE ATTENTION".to_string(),
                "SESSION USER 1000".into(),
                head[0].into(),
                head[1].into(),
                deployment.clone(),
            ];
            rows.extend(disclosure.map(String::from));
            rows.push(asks.into());
            rows.extend(more.iter().map(|row| row.to_string()));
            rows.extend(["APPROVE: TYPE 5 THEN 8".into(), "ESC: CANCEL".into()]);
            rows
        };
        let tpm = ["PROTECT THE DISK: TPM AND PIN", "NEW RECOVERY KEYS: 0"];
        assert_eq!(
            review(UpgradeShape::Tpm, 0),
            expected(tpm, "AND FOR A NEW PIN", &[])
        );
        let tpm = ["PROTECT THE DISK: TPM AND PIN", "NEW RECOVERY KEYS: 2"];
        assert_eq!(
            review(UpgradeShape::Tpm, 2),
            expected(tpm, "AND FOR THESE KEYS AND A NEW PIN", &[])
        );
        let fido = ["PROTECT THE DISK: SECURITY KEYS", "NEW PRIMARY KEYS: 1"];
        let fido_review = review(UpgradeShape::Fido2, 1);
        assert_eq!(
            fido_review,
            expected(
                fido,
                "AND FOR THESE KEYS",
                &[
                    "AN ALTERED BOOT CAN TAKE THE DISK",
                    "FROM ONE USE OF A KEY AND PIN",
                    "WHILE A FIRMWARE TPM GUESS COUNT",
                    "CAN ROLL BACK WITH ITS FLASH",
                ],
            )
        );
        // The FIDO2 review is four rows taller; the compositor adds the time
        // line, so 20 and 16 rows where the ID takes one.
        assert_eq!(fido_review.len(), 19);
        assert_eq!(review(UpgradeShape::Tpm, 2).len(), 15);
        // A key step: a first enrollment's rows, no ID, and Escape.
        let rows = |step| {
            Request::new([1; 32], 1000, upgrade(UpgradeShape::Tpm, 2, step))
                .unwrap()
                .lines()
        };
        let head = |more: &[&str]| -> Vec<String> {
            let mut all = vec![
                "TD SECURE ATTENTION",
                "SESSION USER 1000",
                "PROTECT THE DISK: TPM AND PIN",
                "NEW RECOVERY KEYS: 2",
            ];
            all.extend_from_slice(more);
            all.push("ESC TO CANCEL");
            all.into_iter().map(String::from).collect()
        };
        assert_eq!(
            rows(key_step(1, LoginStep::Connect)),
            head(&["NEW KEY 1 OF 2", "CONNECT ONLY THE NEW KEY"])
        );
        assert_eq!(
            rows(key_step(2, LoginStep::Connect)),
            head(&[
                "NEW KEY 2 OF 2",
                "REMOVE THE PREVIOUS KEY",
                "CONNECT ONLY THE NEW KEY"
            ])
        );
        assert_eq!(
            rows(key_step(1, LoginStep::Create { retries: 8 })),
            head(&[
                "NEW KEY 1 OF 2",
                "CREATE A DISK CREDENTIAL",
                "8 PIN ATTEMPTS LEFT ON THIS KEY",
                "ENTER ITS PIN, THEN TOUCH THE KEY"
            ])
        );
        assert_eq!(
            rows(key_step(
                1,
                LoginStep::Probe {
                    key: [0xab, 0xcd, 0, 1]
                }
            )),
            head(&[
                "NEW KEY 1 OF 2",
                "CHECKING NEW KEY abcd0001",
                "KEEP IT CONNECTED AND TOUCH IT"
            ])
        );
        // Every row but the ID's fits, with no glyph the renderer lacks.
        for shape in [UpgradeShape::Tpm, UpgradeShape::Fido2] {
            for step in upgrade_steps(2) {
                let request = Request::new([1; 32], 1000, upgrade(shape, 2, step)).unwrap();
                for row in request.lines() {
                    if row != deployment {
                        assert!(row.len() <= PROMPT_COLUMNS, "{row}");
                    }
                    assert!(!row.contains(['\'', '(', ')']), "{row}");
                }
            }
        }
        let cancel = Request::new(
            [1; 32],
            1000,
            Operation::UpgradeCancel {
                key: approval(b"47"),
            },
        )
        .unwrap();
        assert_eq!(
            cancel.lines(),
            [
                "TD SECURE ATTENTION",
                "SESSION USER 1000",
                "CANCEL PROTECTING THE DISK",
                "THE NEXT BOOT MAY NEED",
                "THE RECOVERY KEY AND A RESEAL",
                "APPROVE: TYPE 4 THEN 7",
                "ESC: CANCEL",
            ]
        );
    }

    #[test]
    fn invalid_upgrades_refuse() {
        let new = |owner, operation| Request::new([1; 32], owner, operation);
        // The account is the owner; each shape takes its own counts.
        assert!(new(1001, upgrade(UpgradeShape::Tpm, 0, UpgradeStep::Review)).is_err());
        for (shape, counts) in [(UpgradeShape::Tpm, 0..=2), (UpgradeShape::Fido2, 1..=2)] {
            for count in 0..=4 {
                let made = new(1000, upgrade(shape, count, UpgradeStep::Review));
                assert_eq!(made.is_ok(), counts.contains(&count), "{shape:?} {count}");
            }
        }
        // The review carries the approval key and a key step none.
        let mut bare = upgrade(UpgradeShape::Tpm, 1, UpgradeStep::Review);
        if let Operation::Upgrade { approval, .. } = &mut bare {
            *approval = None;
        }
        assert!(new(1000, bare).is_err());
        let mut keyed = upgrade(UpgradeShape::Tpm, 1, key_step(1, LoginStep::Connect));
        if let Operation::Upgrade { approval: key, .. } = &mut keyed {
            *key = Some(approval(b"58"));
        }
        assert!(new(1000, keyed).is_err());
        // A key step names a new key and a ceremony step.
        for (key, step) in [
            (0, LoginStep::Connect),
            (2, LoginStep::Connect),
            (1, LoginStep::Identify),
            (
                1,
                LoginStep::Authorize {
                    key: [1; 4],
                    retries: 8,
                },
            ),
            (1, LoginStep::RecoveryKey),
            (1, LoginStep::DiskIdentify),
        ] {
            assert!(
                new(1000, upgrade(UpgradeShape::Tpm, 1, key_step(key, step))).is_err(),
                "{key} {step:?}"
            );
        }
        let mut wrong = upgrade(UpgradeShape::Tpm, 1, UpgradeStep::Review);
        if let Operation::Upgrade { current, .. } = &mut wrong {
            *current = CURRENT.to_uppercase();
        }
        assert!(new(1000, wrong).is_err());
        // The decoder refuses a shape, a review byte and trailing bytes.
        let bytes = new(1000, upgrade(UpgradeShape::Tpm, 1, UpgradeStep::Review))
            .unwrap()
            .encode();
        for shape in [0, 3, 0xff] {
            let mut bad = bytes.clone();
            bad[49] = shape;
            assert_eq!(
                Request::decode(&bad).err().unwrap(),
                "invalid consent upgrade shape"
            );
        }
        let mut bad = bytes.clone();
        bad[116] = 8;
        assert_eq!(
            Request::decode(&bad).err().unwrap(),
            "invalid consent upgrade review"
        );
        let mut bad = bytes.clone();
        bad[115] = 1;
        assert!(Request::decode(&bad).is_err());
        for end in 0..bytes.len() {
            assert!(Request::decode(&bytes[..end]).is_err(), "{end}");
        }
        let mut long = bytes;
        long.push(0);
        assert!(Request::decode(&long).is_err());
    }

    #[test]
    fn an_upgrades_steps_follow_their_only_order() {
        let request = |count, step| {
            Request::new([1; 32], 1000, upgrade(UpgradeShape::Tpm, count, step)).unwrap()
        };
        // No new key: the review is the last step.
        assert_eq!(
            request(0, UpgradeStep::Review).upgrade_start(),
            Ok(Admitted::Last)
        );
        let steps = upgrade_steps(2);
        assert_eq!(steps.len(), 11);
        assert_eq!(request(2, steps[0]).upgrade_start(), Ok(Admitted::Next));
        assert!(request(2, steps[1]).upgrade_start().is_err());
        for (index, pair) in steps.windows(2).enumerate() {
            let admitted = request(2, pair[0]).upgrade_successor(&request(2, pair[1]));
            let expected = if index == steps.len() - 2 {
                Admitted::Last
            } else {
                Admitted::Next
            };
            assert_eq!(admitted, Ok(expected), "{pair:?}");
        }
        // Nothing follows the last step, and no step is skipped or repeated.
        let last = request(2, steps[10]);
        assert!(last.upgrade_successor(&request(2, steps[1])).is_err());
        for (from, to) in [(0, 2), (1, 1), (5, 7), (5, 1), (6, 10)] {
            assert!(
                request(2, steps[from])
                    .upgrade_successor(&request(2, steps[to]))
                    .is_err(),
                "{from} {to}"
            );
        }
        // A successor keeps every field of the operation.
        let review = request(2, steps[0]);
        let connect = request(2, steps[1]);
        let changed = |edit: &dyn Fn(&mut Operation)| {
            let mut operation = connect.operation().clone();
            edit(&mut operation);
            Request::new([1; 32], 1000, operation).unwrap()
        };
        let shape = changed(&|operation| {
            if let Operation::Upgrade { shape, .. } = operation {
                *shape = UpgradeShape::Fido2;
            }
        });
        let count = changed(&|operation| {
            if let Operation::Upgrade { count, .. } = operation {
                *count = 1;
            }
        });
        let current = changed(&|operation| {
            if let Operation::Upgrade { current, .. } = operation {
                *current = PREVIOUS.into();
            }
        });
        for next in [shape, count, current] {
            assert_eq!(
                review.upgrade_successor(&next),
                Err("upgrade step changed its operation".into())
            );
        }
        let other_nonce = Request::new([2; 32], 1000, connect.operation().clone()).unwrap();
        assert!(review.upgrade_successor(&other_nonce).is_err());
        // A repeat or probe keeps the prove's credential, and a PIN step
        // needs retries left.
        let prove = request(2, steps[3]);
        let stranger = request(
            2,
            key_step(
                1,
                LoginStep::Repeat {
                    key: [7; 4],
                    retries: 7,
                },
            ),
        );
        assert_eq!(
            prove.upgrade_successor(&stranger),
            Err("upgrade step changed its key".into())
        );
        let blocked = request(
            2,
            key_step(
                1,
                LoginStep::Repeat {
                    key: [1; 4],
                    retries: 0,
                },
            ),
        );
        assert!(prove.upgrade_successor(&blocked).is_err());
        // Neither a login step nor a cancellation is an upgrade's.
        let cancel = Request::new(
            [1; 32],
            1000,
            Operation::UpgradeCancel {
                key: approval(b"47"),
            },
        )
        .unwrap();
        assert!(cancel.upgrade_start().is_err());
        assert!(review.upgrade_successor(&cancel).is_err());
        // The ceilings: 240 seconds and 120 more per new key.
        for (count, seconds) in [(0, 240), (1, 360), (2, 480)] {
            assert_eq!(
                request(count, UpgradeStep::Review).ceiling(),
                Some(Duration::from_secs(seconds))
            );
        }
        assert_eq!(upgrade_ceiling(2), LOGIN_LONGEST);
    }

    #[test]
    fn an_upgrade_step_names_only_the_credential_its_ceremony_created() {
        let request =
            |step| Request::new([1; 32], 1000, upgrade(UpgradeShape::Fido2, 2, step)).unwrap();
        let steps = upgrade_steps(2);
        let baseline = [[0xee; 4]];
        let admit = |from: usize, created| {
            request(steps[from]).admit_upgrade_step(&request(steps[from + 1]), &baseline, created)
        };
        // Connect and create come before any credential exists.
        assert_eq!(admit(0, None), Ok(Admitted::Next));
        assert_eq!(admit(1, None), Ok(Admitted::Next));
        assert!(admit(1, Some([1; 4])).is_err());
        // Prove, repeat and probe name exactly the created credential.
        for from in 2..5 {
            assert_eq!(admit(from, Some([1; 4])), Ok(Admitted::Next), "{from}");
            assert!(admit(from, None).is_err(), "{from}");
            assert!(admit(from, Some([2; 4])).is_err(), "{from}");
        }
        // Never one a header token already names, even by collision.
        let collides = [[1; 4]];
        assert!(request(steps[2])
            .admit_upgrade_step(&request(steps[3]), &collides, Some([1; 4]))
            .is_err());
        // The next key's connect forgets the last key's credential.
        assert_eq!(admit(5, None), Ok(Admitted::Next));
        assert_eq!(admit(9, Some([2; 4])), Ok(Admitted::Last));
        // Its key steps are ceremony steps, which a PIN step's `1c` reads;
        // the review is none.
        assert_eq!(request(steps[0]).ceremony_step(), None);
        assert_eq!(
            request(steps[2]).ceremony_step(),
            Some(LoginStep::Create { retries: 8 })
        );
    }

    /// Tag 16: the key, one form byte of 1 to 3 and the running
    /// deployment's canonical ID, 112 bytes in all, with one step and the
    /// form's rows (td-authd/DESIGN.md, amendment 10).
    #[test]
    fn a_selector_update_names_its_form_and_the_running_deployment() {
        for (form, byte, rows) in [
            (
                SelectorForm::DeviceBound,
                1,
                [
                    "THE NEXT BOOT NEEDS THE RECOVERY KEY",
                    "AND A CONFIRMED RESEAL",
                ],
            ),
            (
                SelectorForm::Fido2,
                2,
                ["EVERY BOOT ASKS FOR A SECURITY KEY", "AS IT DOES NOW"],
            ),
            (
                SelectorForm::Staged,
                3,
                [
                    "THE NEXT BOOT ASKS FOR THE DISK PIN",
                    "AND COMPLETES THE UPDATE",
                ],
            ),
        ] {
            let request = Request::new([1; 32], 1000, selector_update(form, CURRENT)).unwrap();
            let mut literal = b"TDCONS01".to_vec();
            literal.extend([1; 32]);
            literal.extend(1000u32.to_be_bytes());
            literal.extend([16, b'5', b'8', byte]);
            literal.extend(CURRENT.as_bytes());
            assert_eq!(literal.len(), 112);
            assert_eq!(request.encode(), literal);
            assert_eq!(Request::decode(&literal).unwrap(), request);
            assert_eq!(
                request.selector_update(),
                Some((approval(b"58"), form, CURRENT))
            );
            assert!(!request.is_login());
            assert_eq!(
                request.lines(),
                [
                    "TD SECURE ATTENTION",
                    "SESSION USER 1000",
                    "UPDATE THE BOOT SELECTOR ON DISK",
                    "FROM THE RUNNING SYSTEM",
                    &format!("DEPLOYMENT: {CURRENT}"),
                    rows[0],
                    rows[1],
                    "APPROVE: TYPE 5 THEN 8",
                    "ESC: CANCEL",
                ]
            );
        }
        let bytes = Request::new(
            [1; 32],
            1000,
            selector_update(SelectorForm::Staged, CURRENT),
        )
        .unwrap()
        .encode();
        for form in [0, 4, 0xff] {
            let mut bad = bytes.clone();
            bad[47] = form;
            assert_eq!(
                Request::decode(&bad).err().unwrap(),
                "invalid consent selector update form"
            );
        }
        for byte in *b"Ag\n" {
            let mut bad = bytes.clone();
            bad[48] = byte;
            assert_eq!(
                Request::decode(&bad).err().unwrap(),
                "invalid consent selector update deployment"
            );
        }
        assert_eq!(
            Request::decode(&bytes[..111]).err().unwrap(),
            "truncated consent request"
        );
        let mut long = bytes.clone();
        long.push(b'a');
        assert_eq!(
            Request::decode(&long).err().unwrap(),
            "trailing consent request bytes"
        );
        for id in ["a".repeat(63), "A".repeat(64), String::new()] {
            assert_eq!(
                Request::new([1; 32], 1000, selector_update(SelectorForm::Fido2, &id))
                    .err()
                    .unwrap(),
                "invalid consent selector update deployment"
            );
        }
        // No other description answers as a selector update.
        let rollback = Request::new([1; 32], 1000, rollback(CURRENT, PREVIOUS)).unwrap();
        assert_eq!(rollback.selector_update(), None);
    }

    /// Both IDs are canonical and differ; each is exactly 64 bytes, so the
    /// value is exactly 175.
    #[test]
    fn a_rollback_names_two_distinct_canonical_deployments() {
        for id in [
            "a".repeat(63),
            "a".repeat(65),
            "A".repeat(64),
            "g".repeat(64),
            format!("{}\n", "a".repeat(63)),
            String::new(),
        ] {
            for operation in [rollback(&id, PREVIOUS), rollback(CURRENT, &id)] {
                assert_eq!(
                    Request::new([1; 32], 1000, operation).err().unwrap(),
                    "invalid consent rollback pair"
                );
            }
        }
        assert_eq!(
            Request::new([1; 32], 1000, rollback(CURRENT, CURRENT))
                .err()
                .unwrap(),
            "invalid consent rollback pair"
        );
        let bytes = Request::new([1; 32], 1000, rollback(CURRENT, PREVIOUS))
            .unwrap()
            .encode();
        // Each ID's first byte, uppercase, outside hex, or not UTF-8.
        for index in [47, 111] {
            for byte in [b'A', b'g', b'\n', 0xff] {
                let mut bad = bytes.clone();
                bad[index] = byte;
                assert!(Request::decode(&bad).is_err(), "{index} {byte}");
            }
        }
        let mut same = bytes.clone();
        same[111..175].copy_from_slice(CURRENT.as_bytes());
        assert_eq!(
            Request::decode(&same).err().unwrap(),
            "invalid consent rollback pair"
        );
        assert_eq!(
            Request::decode(&bytes[..174]).err().unwrap(),
            "truncated consent request"
        );
        let mut long = bytes.clone();
        long.push(b'a');
        assert_eq!(
            Request::decode(&long).err().unwrap(),
            "trailing consent request bytes"
        );
    }

    /// Both names are what `Hostname::parse` admits, they differ, and the
    /// requester is the owner. The widest value is 179 bytes.
    #[test]
    fn a_hostname_change_names_two_canonical_names_for_its_owner() {
        let widest = Request::new(
            [1; 32],
            1000,
            rename(1000, &"a".repeat(63), &"b".repeat(63)),
        )
        .unwrap();
        assert_eq!(widest.encode().len(), 179);
        assert!(widest.encode().len() <= MAX_BYTES);
        assert_eq!(Request::decode(&widest.encode()).unwrap(), widest);
        let narrowest = Request::new([1; 32], 1000, rename(1000, "a", "b")).unwrap();
        assert_eq!(narrowest.encode().len(), 55);
        for name in [
            String::new(),
            "a".repeat(64),
            "TD".into(),
            "1host".into(),
            "-host".into(),
            "host-".into(),
            ".host".into(),
            "a..b".into(),
            "a b".into(),
            "a\nb".into(),
            "a/b".into(),
            "höst".into(),
            "127.0.0.1".into(),
        ] {
            for operation in [rename(1000, &name, "td"), rename(1000, "td", &name)] {
                assert_eq!(
                    Request::new([1; 32], 1000, operation).err().unwrap(),
                    "invalid consent hostname change",
                    "{name:?}"
                );
            }
        }
        for operation in [rename(1000, "td", "td"), rename(1001, "td", "host")] {
            assert_eq!(
                Request::new([1; 32], 1000, operation).err().unwrap(),
                "invalid consent hostname change"
            );
        }
        // Decoded: td -> my-laptop, with the requester at bytes 47 to 50,
        // the old name's length at 51 and the new name's length at 54.
        let bytes = Request::new([1; 32], 1000, rename(1000, "td", "my-laptop"))
            .unwrap()
            .encode();
        for (index, byte, refusal) in [
            (49, 4, "invalid consent hostname change"),
            (52, b'T', "invalid consent hostname change"),
            (52, b'1', "invalid consent hostname change"),
            (53, b'-', "invalid consent hostname change"),
            (55, b'.', "invalid consent hostname change"),
            (60, 0xff, "invalid consent text"),
            (54, 10, "truncated consent request"),
            (54, 8, "trailing consent request bytes"),
        ] {
            let mut bad = bytes.clone();
            bad[index] = byte;
            assert_eq!(
                Request::decode(&bad).err().unwrap(),
                refusal,
                "{index} {byte}"
            );
        }
        let mut same = bytes[..51].to_vec();
        same.extend([2, b't', b'd', 2, b't', b'd']);
        let mut empty_old = bytes[..51].to_vec();
        empty_old.extend([0, 9]);
        empty_old.extend(b"my-laptop");
        let mut empty_new = bytes[..54].to_vec();
        empty_new.push(0);
        for bad in [same, empty_old, empty_new] {
            assert_eq!(
                Request::decode(&bad).err().unwrap(),
                "invalid consent hostname change"
            );
        }
        let mut long = widest.encode();
        long[115] = 64;
        long.insert(116, b'b');
        assert_eq!(
            Request::decode(&long).err().unwrap(),
            "invalid consent hostname change"
        );
        let mut trailing = bytes.clone();
        trailing.push(b'a');
        assert_eq!(
            Request::decode(&trailing).err().unwrap(),
            "trailing consent request bytes"
        );
    }

    /// Each fixed row fits the narrowest prompt, so wrapping never splits
    /// the key, and the key's two rows close the prompt in place of the
    /// generic Escape row.
    #[test]
    fn elevation_rows_end_with_the_key_and_its_escape() {
        // An installation's rows predate the key; its key rows fit as the
        // others' do.
        let lines = Request::new([1; 32], 65533, install(CURRENT, 65533))
            .unwrap()
            .lines();
        assert_eq!(lines[lines.len() - 1], "ESC: CANCEL");
        assert!(lines[lines.len() - 2].starts_with("APPROVE: TYPE "));
        assert!(lines[lines.len() - 2].len() <= PROMPT_COLUMNS);
        assert!(!lines.iter().any(|line| line == "ESC TO CANCEL"));
        for operation in [
            rollback(CURRENT, PREVIOUS),
            rename(65533, &"a".repeat(63), &"b".repeat(63)),
        ] {
            let request = Request::new([1; 32], 65533, operation).unwrap();
            let lines = request.lines();
            assert_eq!(lines[lines.len() - 1], "ESC: CANCEL");
            assert!(lines[lines.len() - 2].starts_with("APPROVE: TYPE "));
            assert!(!lines.iter().any(|line| line == "ESC TO CANCEL"));
            for line in &lines {
                let argument = ["CURRENT: ", "PREVIOUS: ", "OLD NAME: ", "NEW NAME: "]
                    .iter()
                    .any(|field| line.starts_with(field));
                assert!(argument || line.len() <= PROMPT_COLUMNS, "{line:?}");
                assert!(line.is_ascii() && !line.contains('\n'));
            }
        }
    }

    fn disk(model: Option<Label>, serial: Option<Label>) -> Operation {
        Operation::InstallDisk {
            requester: 1000,
            disk: "nvme0n1".into(),
            capacity: 512_110_190_592,
            model,
            serial,
            hostname: "td".into(),
            username: "tester".into(),
            deployment: [0xab, 0xcd, 0, 1, 2, 3, 4, 0xff],
            storage: Storage::Unencrypted,
        }
    }

    /// Every field at its bound: the largest request this profile encodes.
    fn widest_disk() -> Operation {
        Operation::InstallDisk {
            requester: 1000,
            disk: "d".repeat(DISK_NAME_BYTES),
            capacity: u64::MAX,
            model: Some(Label::escape(&[0xff; 64], MODEL_WIDTH)),
            serial: Some(Label::escape(&[b'\\'; 64], SERIAL_WIDTH)),
            hostname: "h".repeat(HOSTNAME_BYTES),
            username: "u".repeat(USERNAME_BYTES),
            deployment: [0xff; 8],
            storage: Storage::DeviceBound,
        }
    }

    #[test]
    fn labels_escape_every_byte_a_line_could_be_forged_with() {
        let label = Label::escape(b"Disk A\\b\n\xc3\xa9~", 64);
        assert_eq!(label.text(), "Disk\\sA\\\\b\\x0a\\xc3\\xa9~");
        assert!(!label.truncated());
        assert!(label.canonical(64));
        // Cut before the escape that would pass the width, never inside it.
        let cut = Label::escape(b"ab\x00", 5);
        assert_eq!((cut.text(), cut.truncated()), ("ab", true));
        assert!(cut.canonical(5));
        let exact = Label::escape(b"ab\x00", 6);
        assert_eq!((exact.text(), exact.truncated()), ("ab\\x00", false));
        let canonical = |text: &str, truncated, width| {
            Label {
                text: text.into(),
                truncated,
            }
            .canonical(width)
        };
        for bad in [
            "a b", "\\", "\\x41", "\\x5c", "\\x20", "\\xC3", "\\x0", "\\q", "a\nb", "\u{e9}",
        ] {
            assert!(!canonical(bad, false, 64), "{bad:?}");
        }
        assert!(canonical("\\\\\\s", false, 64));
        // Truncated with room for another escape is not a cut escape made.
        assert!(!canonical("ab", true, 6));
        assert!(canonical("ab", true, 5));
        assert!(!canonical("abc", false, 2));
    }

    #[test]
    fn disk_installation_shows_its_whole_fixed_summary() {
        let request = Request::new(
            [7; 32],
            1000,
            disk(Some(Label::escape(b"Samsung SSD 980", MODEL_WIDTH)), None),
        )
        .unwrap();
        assert_eq!(
            request.lines(),
            [
                "TD SECURE ATTENTION",
                "SESSION USER 1000",
                "ERASE DISK AND INSTALL TD",
                "DISK: nvme0n1",
                "SIZE: 512.1 GB, 512110190592 BYTES",
                "MODEL: Samsung\\sSSD\\s980",
                "SERIAL: NOT REPORTED",
                "ALL DATA ON THIS DISK WILL BE LOST",
                "HOSTNAME: td",
                "USER: tester",
                "DEPLOYMENT: abcd0001020304ff...",
                "UNENCRYPTED STORAGE, AUTOMATIC LOGIN",
                "ENTER: ERASE AND INSTALL   ESC: CANCEL",
            ]
        );
        let mut literal = b"TDCONS01".to_vec();
        literal.extend([7; 32]);
        literal.extend([0, 0, 3, 232, 6, 0, 0, 3, 232]);
        literal.extend(512_110_190_592_u64.to_be_bytes());
        literal.push(7);
        literal.extend(b"nvme0n1");
        literal.push(17);
        literal.extend(b"Samsung\\sSSD\\s980");
        literal.push(0xff);
        literal.push(2);
        literal.extend(b"td");
        literal.push(6);
        literal.extend(b"tester");
        literal.extend([0xab, 0xcd, 0, 1, 2, 3, 4, 0xff]);
        literal.push(0);
        assert_eq!(request.encode(), literal);
        // Device-bound storage is the one byte after the prefix, and its
        // row replaces the unencrypted one.
        let mut bound = disk(Some(Label::escape(b"Samsung SSD 980", MODEL_WIDTH)), None);
        if let Operation::InstallDisk { storage, .. } = &mut bound {
            *storage = Storage::DeviceBound;
        }
        let bound = Request::new([7; 32], 1000, bound).unwrap();
        let mut lines = request.lines();
        lines[11] = "ENCRYPTED TO THIS COMPUTER, AUTOMATIC LOGIN".into();
        assert_eq!(bound.lines(), lines);
        let mut bound_literal = literal.clone();
        *bound_literal.last_mut().unwrap() = 1;
        assert_eq!(bound.encode(), bound_literal);
        assert_eq!(Request::decode(&bound_literal).unwrap(), bound);
        for code in 2..=255 {
            *bound_literal.last_mut().unwrap() = code;
            assert_eq!(
                Request::decode(&bound_literal).err().unwrap(),
                "invalid consent disk storage"
            );
        }

        let truncated = Request::new(
            [7; 32],
            1000,
            disk(None, Some(Label::escape(&[b'S'; 30], SERIAL_WIDTH))),
        )
        .unwrap();
        assert!(truncated
            .lines()
            .contains(&format!("SERIAL: {} (TRUNCATED)", "S".repeat(24))));
        let empty = Request::new([7; 32], 1000, disk(Some(Label::escape(b"", 32)), None)).unwrap();
        assert!(empty.lines().contains(&"MODEL: REPORTED EMPTY".to_string()));
        // A disk reporting a marker's text shows it as its own label: every
        // marker holds a space, which a label always escapes.
        let forged = Request::new(
            [7; 32],
            1000,
            disk(Some(Label::model(b"REPORTED EMPTY")), None),
        )
        .unwrap();
        assert!(forged
            .lines()
            .contains(&"MODEL: REPORTED\\sEMPTY".to_string()));
    }

    /// The wire is held to the same checks as construction.
    #[test]
    fn a_decoded_disk_installation_is_refused_where_construction_would_be() {
        let request = Request::new(
            [7; 32],
            1000,
            disk(Some(Label::model(b"ab")), Some(Label::serial(b"cd"))),
        )
        .unwrap();
        let bytes = request.encode();
        // Header 45, requester 4, capacity 8, disk name 1 + 7; then the model.
        let model = 45 + 4 + 8 + 8;
        assert_eq!(&bytes[model..model + 3], &[2, b'a', b'b']);
        for (offset, value) in [
            (model + 1, b' '),
            (model + 1, b'\\'),
            (model, 0x82),
            (58, b'/'),
        ] {
            let mut bad = bytes.clone();
            bad[offset] = value;
            assert!(Request::decode(&bad).is_err(), "{offset} {value}");
        }
        // A whole model one byte over its width, framed correctly.
        let mut over = bytes[..model].to_vec();
        over.push(33);
        over.extend([b'm'; 33]);
        over.extend(&bytes[model + 3..]);
        assert_eq!(
            Request::decode(&over).err().unwrap(),
            "invalid consent disk installation summary"
        );
        let mut zero = bytes.clone();
        zero[49..57].fill(0);
        assert!(Request::decode(&zero).is_err());
    }

    #[test]
    fn disk_installation_fits_the_request_bound_and_refuses_unshowable_names() {
        let widest = Request::new([7; 32], 1000, widest_disk()).unwrap();
        assert!(
            widest.encode().len() <= MAX_BYTES,
            "{}",
            widest.encode().len()
        );
        assert_eq!(widest.encode().len(), 253);
        assert_eq!(Request::decode(&widest.encode()).unwrap(), widest);

        let make = |edit: fn(&mut Operation)| {
            let mut operation = disk(None, None);
            edit(&mut operation);
            Request::new([7; 32], 1000, operation)
        };
        assert!(make(|_| {}).is_ok());
        let refused: [fn(&mut Operation); 13] = [
            |op| {
                if let Operation::InstallDisk { requester, .. } = op {
                    *requester = 1001;
                }
            },
            |op| {
                if let Operation::InstallDisk { disk, .. } = op {
                    *disk = String::new();
                }
            },
            |op| {
                if let Operation::InstallDisk { disk, .. } = op {
                    *disk = "d".repeat(DISK_NAME_BYTES + 1);
                }
            },
            |op| {
                if let Operation::InstallDisk { disk, .. } = op {
                    *disk = "../sda".into();
                }
            },
            |op| {
                if let Operation::InstallDisk { capacity, .. } = op {
                    *capacity = 0;
                }
            },
            |op| {
                if let Operation::InstallDisk { hostname, .. } = op {
                    *hostname = String::new();
                }
            },
            |op| {
                if let Operation::InstallDisk { hostname, .. } = op {
                    *hostname = "h".repeat(HOSTNAME_BYTES + 1);
                }
            },
            |op| {
                if let Operation::InstallDisk { hostname, .. } = op {
                    *hostname = "td host".into();
                }
            },
            |op| {
                if let Operation::InstallDisk { hostname, .. } = op {
                    *hostname = "td\\s".into();
                }
            },
            |op| {
                if let Operation::InstallDisk { username, .. } = op {
                    *username = "u".repeat(USERNAME_BYTES + 1);
                }
            },
            |op| {
                if let Operation::InstallDisk { username, .. } = op {
                    *username = "t\u{e9}".into();
                }
            },
            |op| {
                if let Operation::InstallDisk { model, .. } = op {
                    *model = Some(Label::escape(&[b'm'; 40], 64));
                }
            },
            |op| {
                if let Operation::InstallDisk { serial, .. } = op {
                    *serial = Some(Label {
                        text: "a b".into(),
                        truncated: false,
                    });
                }
            },
        ];
        for (index, edit) in refused.into_iter().enumerate() {
            assert!(make(edit).is_err(), "case {index}");
        }
    }

    #[test]
    fn every_operation_roundtrips_and_refuses_truncation_or_trailing_bytes() {
        let mut operations = vec![
            Operation::Install {
                key: ApprovalKey::new(*b"92").unwrap(),
                deployment: "ab".repeat(32),
                requester: 1000,
            },
            disk(None, None),
            disk(
                Some(Label::escape(b"Model \xff", MODEL_WIDTH)),
                Some(Label::escape(&[b'9'; 40], SERIAL_WIDTH)),
            ),
            widest_disk(),
            rollback(CURRENT, PREVIOUS),
            rollback(PREVIOUS, CURRENT),
            rename(1000, "a", "b"),
            rename(1000, &"a".repeat(63), &"b".repeat(63)),
            Operation::Unlock {
                role: Role::Primary,
            },
            Operation::Unlock {
                role: Role::Recovery,
            },
            Operation::Set {
                role: Role::Primary,
                application: "mail".into(),
                name: "Main".into(),
                application_uid: 65537,
                requester: 1000,
            },
        ];
        for recovery in [Recovery::SecondToken, Recovery::Unrecoverable] {
            for step in [
                Enrollment::CreatePrimary,
                Enrollment::ProvePrimary,
                Enrollment::CreateRecovery,
                Enrollment::ProveRecovery,
            ] {
                let operation = Operation::Enroll {
                    platform: Platform::TpmPcr7,
                    recovery,
                    step,
                };
                if recovery == Recovery::Unrecoverable
                    && matches!(step, Enrollment::CreateRecovery | Enrollment::ProveRecovery)
                {
                    assert!(Request::new([1; 32], 1000, operation).is_err());
                } else {
                    operations.push(operation);
                }
            }
        }
        operations.extend(every_login_operation());
        let mut encodings = std::collections::BTreeSet::new();
        for operation in operations {
            let request = Request::new([1; 32], 1000, operation).unwrap();
            let mut bytes = request.encode();
            assert_eq!(Request::decode(&bytes).unwrap(), request);
            assert!(encodings.insert(bytes.clone()));
            for end in 0..bytes.len() {
                assert!(Request::decode(&bytes[..end]).is_err());
            }
            bytes.push(0);
            assert!(Request::decode(&bytes).is_err());
            assert!(request
                .lines()
                .iter()
                .all(|line| line.is_ascii() && !line.contains('\n')));
        }
    }

    #[test]
    fn wire_version_identity_and_token_role_have_independent_literal_encoding() {
        let request = Request::new(
            [42; 32],
            1000,
            Operation::Unlock {
                role: Role::Recovery,
            },
        )
        .unwrap();
        let mut literal = b"TDCONS01".to_vec();
        literal.extend([42; 32]);
        literal.extend([0, 0, 3, 232, 2, 2]);
        assert_eq!(request.encode(), literal);
        for owner in [0, 991, 999, 65534, 65536, u32::MAX] {
            assert!(Request::new(
                [1; 32],
                owner,
                Operation::Unlock {
                    role: Role::Primary
                }
            )
            .is_err());
        }
        assert!(Request::new(
            [0; 32],
            1000,
            Operation::Unlock {
                role: Role::Primary
            }
        )
        .is_err());
        for index in [0, 44, 45] {
            let mut bad = literal.clone();
            bad[index] = 255;
            assert!(Request::decode(&bad).is_err());
        }
        assert!(Request::decode(&[0; MAX_BYTES + 1]).is_err());
    }

    #[test]
    fn enrollment_profile_is_bound_by_an_independent_wire_tag() {
        let request = Request::new(
            [1; 32],
            1000,
            Operation::Enroll {
                platform: Platform::TpmPcr7,
                recovery: Recovery::SecondToken,
                step: Enrollment::CreatePrimary,
            },
        )
        .unwrap();
        let mut bytes = request.encode();
        assert_eq!(&bytes[44..], &[1, 1, 1, 1]);
        assert!(request
            .lines()
            .iter()
            .any(|line| line == "PLATFORM: TPM PCR 7"));
        bytes[45] = 2;
        assert!(Request::decode(&bytes).is_err());
    }

    #[test]
    fn credential_display_keeps_every_public_argument() {
        let request = Request::new(
            [42; 32],
            1001,
            Operation::Set {
                role: Role::Primary,
                application: "mail".into(),
                name: "Main_Account-2026".into(),
                application_uid: 65537,
                requester: 1001,
            },
        )
        .unwrap();
        assert_eq!(
            request.lines(),
            [
                "TD SECURE ATTENTION",
                "SESSION USER 1001",
                "SET ONE APPLICATION CREDENTIAL",
                "REQUESTER UID 1001",
                "APPLICATION UID 65537",
                "APPLICATION: mail",
                "CREDENTIAL: Main_Account-2026",
                "TOUCH THE PRIMARY TOKEN",
                "ESC TO CANCEL",
            ]
        );
        let enrollment = Request::new(
            [1; 32],
            1000,
            Operation::Enroll {
                platform: Platform::TpmPcr7,
                recovery: Recovery::Unrecoverable,
                step: Enrollment::ProvePrimary,
            },
        )
        .unwrap();
        assert_eq!(&enrollment.encode()[44..], &[1, 1, 0, 2]);
        assert_eq!(
            enrollment.lines(),
            [
                "TD SECURE ATTENTION",
                "SESSION USER 1000",
                "ENROLL SECRET STORE",
                "PLATFORM: TPM PCR 7",
                "RECOVERY: NONE. LOST TOKEN LOSES SECRETS",
                "VERIFY PRIMARY TOKEN CREDENTIAL",
                "TOUCH THE PRIMARY TOKEN",
                "ESC TO CANCEL",
            ]
        );
    }

    #[test]
    fn target_is_bounded_and_case_sensitive_without_control_characters() {
        let make = |app: &str, name: &str, uid, requester| {
            Request::new(
                [1; 32],
                1000,
                Operation::Set {
                    role: Role::Primary,
                    application: app.into(),
                    name: name.into(),
                    application_uid: uid,
                    requester,
                },
            )
        };
        let upper = make("mail", "Main", 65537, 1000).unwrap();
        let lower = make("mail", "main", 65537, 1000).unwrap();
        assert_ne!(upper.encode(), lower.encode());
        assert_ne!(upper.lines(), lower.lines());
        for app in ["", "Mail", "../mail", "mail\n", "mail/other"] {
            assert!(make(app, "main", 65537, 1000).is_err());
        }
        for name in [
            "",
            "../main",
            "main\n",
            "main/other",
            "main other",
            "main.other",
        ] {
            assert!(make("mail", name, 65537, 1000).is_err());
        }
        assert!(make(&"a".repeat(65), "main", 65537, 1000).is_err());
        assert!(make("mail", &"a".repeat(65), 65537, 1000).is_err());
        assert!(make("mail", "main", 1000, 1000).is_err());
        assert!(make("mail", "main", 65537, 1001).is_err());
        assert!(
            make(&"a".repeat(64), &"A".repeat(64), 65537, 1000)
                .unwrap()
                .encode()
                .len()
                <= MAX_BYTES
        );
    }

    const A: Fingerprint = [0x3f, 0xa2, 0xc1, 0xd0];
    const B: Fingerprint = [0x01, 0x02, 0x03, 0x04];
    const N: Fingerprint = [0xde, 0xad, 0xbe, 0xef];
    const M: Fingerprint = [0x00, 0xff, 0x00, 0xff];

    fn slot(position: u8, key: Fingerprint) -> Slot {
        Slot { position, key }
    }

    fn every_step(key: Fingerprint) -> [LoginStep; 8] {
        [
            LoginStep::Identify,
            LoginStep::Authorize { key, retries: 8 },
            LoginStep::Create { retries: 0 },
            LoginStep::Prove { key, retries: 255 },
            LoginStep::Repeat { key, retries: 1 },
            LoginStep::Probe { key },
            LoginStep::Unlock { key, retries: 3 },
            LoginStep::Connect,
        ]
    }

    /// Every count, key ordinal, removal set and step a login operation may
    /// carry.
    fn every_login_operation() -> Vec<Operation> {
        let sets = [
            vec![slot(1, A)],
            vec![slot(2, A)],
            vec![slot(1, B), slot(2, A), slot(3, A)],
            (1..=8)
                .map(|position| slot(position, [position; 4]))
                .collect(),
            (1..=9).map(|position| slot(position, A)).collect(),
        ];
        let mut operations = Vec::new();
        for step in every_step(A) {
            for before in 0..=LOGIN_KEYS + 1 {
                operations.push(Operation::LoginUnlock {
                    account: 1000,
                    before,
                    after: before,
                    step,
                });
                operations.push(Operation::LoginAdd {
                    account: 1000,
                    before,
                    after: before.saturating_add(1),
                    step,
                    proof: None,
                });
                for removed in &sets {
                    if let Some(after) = before.checked_sub(removed.len() as u8) {
                        operations.push(Operation::LoginRemove {
                            account: 1000,
                            before,
                            after,
                            removed: removed.clone(),
                            step,
                            approval: None,
                            proof: None,
                        });
                    }
                }
            }
            for after in 1..=2 {
                for key in 1..=after {
                    operations.push(Operation::LoginEnroll {
                        account: 1000,
                        before: 0,
                        after,
                        key,
                        step,
                        approval: None,
                        proof: None,
                    });
                }
            }
        }
        // Each disclosure step with its key.
        let mut operations: Vec<Operation> = operations
            .into_iter()
            .map(|operation| operation.disclosed(disclosure()))
            .collect();
        operations.retain(|operation| Request::new([1; 32], 1000, operation.clone()).is_ok());
        operations
    }

    /// The key every test disclosure step carries.
    fn disclosure() -> ApprovalKey {
        approval(b"36")
    }

    fn login(operation: Operation) -> Request {
        Request::new([9; 32], 1000, operation).unwrap()
    }

    fn header(tag: u8) -> Vec<u8> {
        let mut literal = b"TDCONS01".to_vec();
        literal.extend([9; 32]);
        literal.extend([0, 0, 3, 232, tag, 0, 0, 3, 232]);
        literal
    }

    fn add(step: LoginStep) -> Operation {
        Operation::LoginAdd {
            account: 1000,
            before: 2,
            after: 3,
            step,
            proof: None,
        }
    }

    /// With the key on its disclosure step.
    fn enroll(after: u8, key: u8, step: LoginStep) -> Operation {
        Operation::LoginEnroll {
            account: 1000,
            before: 0,
            after,
            key,
            step,
            approval: None,
            proof: None,
        }
        .disclosed(disclosure())
    }

    /// A removal from a three-key record, with the key on its disclosure
    /// step.
    fn remove(removed: Vec<Slot>, step: LoginStep) -> Operation {
        Operation::LoginRemove {
            account: 1000,
            before: 3,
            after: 3 - removed.len() as u8,
            removed,
            step,
            approval: None,
            proof: None,
        }
        .disclosed(disclosure())
    }

    fn unlock(step: LoginStep) -> Operation {
        Operation::LoginUnlock {
            account: 1000,
            before: 2,
            after: 2,
            step,
        }
    }

    #[test]
    fn every_login_step_has_an_independent_literal_encoding() {
        // Account 1000, then the counts, the operation's own field, the step.
        let cases: Vec<(Operation, u8, Vec<u8>)> = vec![
            (unlock(LoginStep::Identify), 7, vec![2, 2, 1]),
            (
                unlock(LoginStep::Unlock { key: A, retries: 8 }),
                7,
                vec![2, 2, 7, 0x3f, 0xa2, 0xc1, 0xd0, 8],
            ),
            (
                enroll(2, 1, LoginStep::Connect),
                8,
                vec![0, 2, 1, 8, b'3', b'6'],
            ),
            (
                enroll(2, 2, LoginStep::Create { retries: 7 }),
                8,
                vec![0, 2, 2, 3, 7],
            ),
            (
                enroll(1, 1, LoginStep::Prove { key: N, retries: 6 }),
                8,
                vec![0, 1, 1, 4, 0xde, 0xad, 0xbe, 0xef, 6],
            ),
            (
                enroll(1, 1, LoginStep::Repeat { key: N, retries: 5 }),
                8,
                vec![0, 1, 1, 5, 0xde, 0xad, 0xbe, 0xef, 5],
            ),
            (
                enroll(1, 1, LoginStep::Probe { key: N }),
                8,
                vec![0, 1, 1, 6, 0xde, 0xad, 0xbe, 0xef],
            ),
            (add(LoginStep::Identify), 9, vec![2, 3, 1]),
            (
                add(LoginStep::Authorize { key: A, retries: 4 }),
                9,
                vec![2, 3, 2, 0x3f, 0xa2, 0xc1, 0xd0, 4],
            ),
            (add(LoginStep::Connect), 9, vec![2, 3, 8]),
            (
                remove(vec![slot(1, B), slot(3, A)], LoginStep::Identify),
                10,
                vec![
                    3, 1, 2, 1, 1, 2, 3, 4, 3, 0x3f, 0xa2, 0xc1, 0xd0, 1, b'3', b'6',
                ],
            ),
            (
                Operation::LoginRemove {
                    account: 1000,
                    before: 1,
                    after: 0,
                    removed: vec![slot(1, A)],
                    step: LoginStep::Authorize { key: A, retries: 0 },
                    approval: None,
                    proof: None,
                },
                10,
                vec![
                    1, 0, 1, 1, 0x3f, 0xa2, 0xc1, 0xd0, 2, 0x3f, 0xa2, 0xc1, 0xd0, 0,
                ],
            ),
        ];
        let mut tags = std::collections::BTreeSet::new();
        let mut steps = std::collections::BTreeSet::new();
        for (operation, tag, tail) in cases {
            let request = login(operation);
            let mut literal = header(tag);
            literal.extend(&tail);
            assert_eq!(request.encode(), literal, "{request:?}");
            assert_eq!(Request::decode(&literal).unwrap(), request);
            tags.insert(tag);
            steps.insert(request.operation().login().unwrap().step.byte());
        }
        assert!(tags.into_iter().eq(7..=10));
        assert!(steps.into_iter().eq(1..=8));
    }

    /// An operation, its tag, its value's bytes after the account, the
    /// value's width and its rows.
    type Vector = (Operation, u8, Vec<u8>, usize, Vec<&'static str>);

    /// The four disclosure steps (td-login/TOKEN-LOGIN.md increment 5's
    /// A4), each with its key as the value's last two bytes, its width,
    /// and its rows: the step's, the disclosure, then the key's.
    fn disclosure_vectors() -> Vec<Vector> {
        let key = Some(approval(b"92"));
        let enrollment = [
            "BOOT AND LOCK NEED A KEY AND PIN",
            "CONSOLE AND SSH SESSIONS END NOW",
            "SSH ONLY FOR THE PRIMARY ACCOUNT",
            "ADMINISTRATION NEEDS A LOGIN",
            "PIN SET ON ANOTHER LAYOUT MAY FAIL",
        ];
        let one_key = [
            "LOST KEY OR BLOCKED PIN: NO LOGIN",
            "RECOVERY NEEDS BOOTING OTHER CODE",
            "HERE, WHICH ALSO BYPASSES THE LOCK",
            "A BACKUP KEY CAN BE ADDED LATER",
        ];
        let keyed = ["APPROVE: TYPE 9 THEN 2", "ESC: CANCEL"];
        let header = ["TD SECURE ATTENTION", "SESSION USER 1000"];
        let enroll = |after| Operation::LoginEnroll {
            account: 1000,
            before: 0,
            after,
            key: 1,
            step: LoginStep::Connect,
            approval: key,
            proof: None,
        };
        let remove = |before, removed: Vec<Slot>| Operation::LoginRemove {
            account: 1000,
            before,
            after: before - removed.len() as u8,
            removed,
            step: LoginStep::Identify,
            approval: key,
            proof: None,
        };
        let rows = |head: &[&'static str], tail: &[&[&'static str]]| {
            let mut rows = header.to_vec();
            rows.extend(head);
            rows.extend(tail.iter().flat_map(|rows| rows.iter()));
            rows.extend(keyed);
            rows
        };
        vec![
            (
                enroll(1),
                8,
                vec![0, 1, 1, 8, b'9', b'2'],
                55,
                rows(
                    &[
                        "ENROLL LOGIN KEYS (0 -> 1 KEY)",
                        "NEW KEY 1 OF 1",
                        "ACCOUNT UID 1000",
                        "CONNECT ONLY THE NEW KEY",
                    ],
                    &[&enrollment, &one_key],
                ),
            ),
            (
                enroll(2),
                8,
                vec![0, 2, 1, 8, b'9', b'2'],
                55,
                rows(
                    &[
                        "ENROLL LOGIN KEYS (0 -> 2 KEYS)",
                        "NEW KEY 1 OF 2",
                        "ACCOUNT UID 1000",
                        "CONNECT ONLY THE NEW KEY",
                    ],
                    &[&enrollment],
                ),
            ),
            (
                remove(3, vec![slot(1, B), slot(3, A)]),
                10,
                vec![
                    3, 1, 2, 1, 1, 2, 3, 4, 3, 0x3f, 0xa2, 0xc1, 0xd0, 1, b'9', b'2',
                ],
                65,
                rows(
                    &[
                        "REMOVE 2 LOGIN KEYS (3 -> 1 KEY)",
                        "ACCOUNT UID 1000",
                        "REMOVE: 1:01020304 3:3fa2c1d0",
                        "CONNECT ONLY ONE ENROLLED KEY",
                    ],
                    &[&one_key],
                ),
            ),
            (
                remove(2, vec![slot(1, B), slot(2, A)]),
                10,
                vec![
                    2, 0, 2, 1, 1, 2, 3, 4, 2, 0x3f, 0xa2, 0xc1, 0xd0, 1, b'9', b'2',
                ],
                65,
                rows(
                    &[
                        "REMOVE 2 LOGIN KEYS (2 -> 0 KEYS)",
                        "ACCOUNT UID 1000",
                        "REMOVE: 1:01020304 2:3fa2c1d0",
                        "CONNECT ONLY ONE ENROLLED KEY",
                    ],
                    &[&[
                        "LOGIN WILL NOT NEED A KEY",
                        "CONSOLE LOGIN AND THE ORDINARY",
                        "SSH POLICY RETURN",
                        "OPEN SSH SESSIONS END NOW",
                    ]],
                ),
            ),
        ]
    }

    /// `operation` with its approval field set to `approval`, whatever its
    /// step; an operation without the field unchanged.
    fn keyed(operation: Operation, approval: Option<ApprovalKey>) -> Operation {
        match operation {
            Operation::LoginEnroll {
                account,
                before,
                after,
                key,
                step,
                approval: _,
                proof,
            } => Operation::LoginEnroll {
                account,
                before,
                after,
                key,
                step,
                approval,
                proof,
            },
            Operation::LoginRemove {
                account,
                before,
                after,
                removed,
                step,
                approval: _,
                proof,
            } => Operation::LoginRemove {
                account,
                before,
                after,
                removed,
                step,
                approval,
                proof,
            },
            other => other,
        }
    }

    #[test]
    fn each_disclosure_step_has_an_independent_literal_encoding_and_rows() {
        for (operation, tag, tail, width, rows) in disclosure_vectors() {
            let request = login(operation);
            let mut literal = header(tag);
            literal.extend(&tail);
            assert_eq!(literal.len(), width);
            assert_eq!(request.encode(), literal, "{request:?}");
            assert_eq!(Request::decode(&literal).unwrap(), request);
            assert_eq!(request.login_approval(), Some(approval(b"92")));
            assert_eq!(request.lines(), rows);
            for row in &rows {
                assert!(row.len() <= PROMPT_COLUMNS, "{row:?}");
            }
            // The decoder requires the key: without it, or with one byte
            // of it, the value is cut; construction without it refuses.
            for cut in [1, 2] {
                assert_eq!(
                    Request::decode(&literal[..literal.len() - cut])
                        .err()
                        .unwrap(),
                    "truncated consent request"
                );
            }
            let keyless = keyed(request.operation().clone(), None);
            assert_eq!(
                Request::new([9; 32], 1000, keyless.clone()).err().unwrap(),
                "invalid consent login operation"
            );
            // Root's draw attaches the key here, and only here.
            assert_eq!(login(keyless.disclosed(approval(b"92"))), request);
            // Each digit is 2 to 9, in typing order.
            for index in [literal.len() - 2, literal.len() - 1] {
                for byte in 0..=255u8 {
                    let mut changed = literal.clone();
                    changed[index] = byte;
                    let decoded = Request::decode(&changed);
                    if (b'2'..=b'9').contains(&byte) {
                        assert!(decoded.is_ok(), "{index} {byte}");
                    } else {
                        assert_eq!(
                            decoded.err().unwrap(),
                            "invalid consent approval key",
                            "{index} {byte}"
                        );
                    }
                }
            }
            let mut swapped = literal.clone();
            swapped.swap(literal.len() - 2, literal.len() - 1);
            let swapped = Request::decode(&swapped).unwrap();
            assert_eq!(swapped.login_approval(), Some(approval(b"29")));
            assert_eq!(
                swapped.lines().iter().rev().nth(1).unwrap(),
                "APPROVE: TYPE 2 THEN 9"
            );
        }
    }

    /// No other step carries a key: two more bytes are trailing, and
    /// construction with one refuses.
    #[test]
    fn only_a_disclosure_step_carries_the_key() {
        let keyless = [
            enroll(2, 2, LoginStep::Connect),
            enroll(1, 1, LoginStep::Create { retries: 8 }),
            enroll(2, 1, LoginStep::Probe { key: N }),
            remove(vec![slot(2, A)], LoginStep::Identify),
            remove(
                vec![slot(1, B), slot(3, A)],
                LoginStep::Authorize { key: B, retries: 8 },
            ),
            Operation::LoginRemove {
                account: 1000,
                before: 1,
                after: 0,
                removed: vec![slot(1, A)],
                step: LoginStep::Authorize { key: A, retries: 8 },
                approval: None,
                proof: None,
            },
            add(LoginStep::Identify),
            add(LoginStep::Connect),
            unlock(LoginStep::Identify),
        ];
        for operation in keyless {
            let request = login(operation.clone());
            assert_eq!(request.login_approval(), None);
            assert_eq!(operation.clone().disclosed(approval(b"92")), operation);
            let mut bytes = request.encode();
            bytes.extend(b"92");
            assert_eq!(
                Request::decode(&bytes).err().unwrap(),
                "trailing consent request bytes",
                "{operation:?}"
            );
            // Unlock and addition have no field for one.
            let keyed = keyed(operation.clone(), Some(approval(b"92")));
            if keyed != operation {
                assert_eq!(
                    Request::new([9; 32], 1000, keyed).err().unwrap(),
                    "invalid consent login operation"
                );
            }
        }
        // Exactly the disclosure steps carry it among every login step.
        for operation in every_login_operation() {
            let request = login(operation.clone());
            let disclosure = match (&operation, place(&request)) {
                (Operation::LoginEnroll { .. }, ("connect", 1)) => true,
                (Operation::LoginRemove { after, .. }, ("identify", _)) => *after <= 1,
                _ => false,
            };
            assert_eq!(
                request.login_approval().is_some(),
                disclosure,
                "{operation:?}"
            );
            let lines = request.lines();
            if disclosure {
                assert_eq!(lines[lines.len() - 2], "APPROVE: TYPE 3 THEN 6");
                assert_eq!(lines[lines.len() - 1], "ESC: CANCEL");
                assert!(!lines.iter().any(|line| line == "ESC TO CANCEL"));
            } else {
                assert_eq!(lines[lines.len() - 1], "ESC TO CANCEL");
                assert!(!lines.iter().any(|line| line.starts_with("APPROVE")));
            }
        }
    }

    #[test]
    fn login_operations_roundtrip_and_refuse_every_cut_and_trailing_byte() {
        let operations = every_login_operation();
        // Unlock's 2 steps at 8 counts, enrollment's 5 for each of 3 keys,
        // addition's 7 at 7 counts, removal's 2 over each set's valid counts.
        assert_eq!(
            operations.len(),
            2 * 8 + 5 * 3 + 7 * 7 + 2 * (8 + 7 + 6 + 1)
        );
        for operation in operations {
            let request = Request::new([1; 32], 1000, operation).unwrap();
            let mut bytes = request.encode();
            assert_eq!(Request::decode(&bytes).unwrap(), request);
            for end in 0..bytes.len() {
                assert!(Request::decode(&bytes[..end]).is_err(), "{request:?} {end}");
            }
            bytes.push(0);
            assert!(Request::decode(&bytes).is_err());
        }
    }

    #[test]
    fn unknown_login_tags_steps_slots_and_inconsistent_counts_refuse() {
        let mut identify = header(7);
        identify.extend([2, 2, 1]);
        assert!(Request::decode(&identify).is_ok());
        // 13 to 15 are the disk proofs', 16 to 18 the storage
        // operations', 19 to 25 reserved.
        for tag in [0, 3, 19, 25, 255] {
            let mut bad = identify.clone();
            bad[44] = tag;
            assert_eq!(
                Request::decode(&bad).err().unwrap(),
                "unknown consent operation",
                "{tag}"
            );
        }
        for byte in [0, 12, 255] {
            let mut bad = identify.clone();
            bad[51] = byte;
            assert_eq!(
                Request::decode(&bad).err().unwrap(),
                "invalid consent login step"
            );
        }
        // A step another operation owns is refused as it is decoded.
        for (tag, tail) in [
            (7, vec![2, 2, 8]),
            (7, vec![2, 2, 2, 1, 2, 3, 4, 8]),
            (8, vec![0, 1, 1, 1]),
            (8, vec![0, 1, 1, 7, 1, 2, 3, 4, 8]),
            (9, vec![2, 3, 7, 1, 2, 3, 4, 8]),
            (10, vec![2, 1, 1, 1, 1, 2, 3, 4, 3, 8]),
            (10, vec![2, 1, 1, 1, 1, 2, 3, 4, 6, 1, 2, 3, 4]),
        ] {
            let mut bytes = header(tag);
            bytes.extend(tail);
            assert_eq!(
                Request::decode(&bytes).err().unwrap(),
                "invalid consent login operation",
                "{bytes:?}"
            );
        }
        let removal = |before: u8, positions: &[u8]| {
            let mut bytes = header(10);
            bytes.extend([before, before.saturating_sub(positions.len() as u8)]);
            bytes.push(positions.len() as u8);
            for position in positions {
                bytes.push(*position);
                bytes.extend(A);
            }
            bytes.push(1);
            // Leaving at most one key, identify discloses.
            if before.saturating_sub(positions.len() as u8) <= 1 {
                bytes.extend(b"36");
            }
            Request::decode(&bytes)
        };
        assert!(removal(3, &[1, 3]).is_ok());
        assert!(removal(3, &[3]).is_ok());
        // Non-increasing, repeated, zero and out-of-range positions.
        for positions in [&[3, 1][..], &[2, 2], &[0], &[4], &[1, 4], &[]] {
            assert_eq!(
                removal(3, positions).err().unwrap(),
                "invalid consent login operation",
                "{positions:?}"
            );
        }
        assert_eq!(
            removal(8, &[1, 2, 3, 4, 5, 6, 7, 8, 9]).err().unwrap(),
            "invalid consent login key set"
        );
        let refused = [
            unlock(LoginStep::Create { retries: 1 }),
            Operation::LoginUnlock {
                account: 1000,
                before: 0,
                after: 0,
                step: LoginStep::Identify,
            },
            Operation::LoginUnlock {
                account: 1000,
                before: 9,
                after: 9,
                step: LoginStep::Identify,
            },
            Operation::LoginUnlock {
                account: 1000,
                before: 2,
                after: 3,
                step: LoginStep::Identify,
            },
            Operation::LoginUnlock {
                account: 1001,
                before: 2,
                after: 2,
                step: LoginStep::Identify,
            },
            Operation::LoginEnroll {
                account: 1000,
                before: 1,
                after: 2,
                key: 1,
                step: LoginStep::Connect,
                approval: None,
                proof: None,
            },
            enroll(3, 1, LoginStep::Connect),
            enroll(0, 0, LoginStep::Connect),
            enroll(1, 2, LoginStep::Connect),
            enroll(2, 0, LoginStep::Connect),
            enroll(2, 1, LoginStep::Identify),
            Operation::LoginAdd {
                account: 1000,
                before: 0,
                after: 1,
                step: LoginStep::Identify,
                proof: None,
            },
            Operation::LoginAdd {
                account: 1000,
                before: 8,
                after: 9,
                step: LoginStep::Identify,
                proof: None,
            },
            Operation::LoginAdd {
                account: 1000,
                before: 2,
                after: 2,
                step: LoginStep::Identify,
                proof: None,
            },
            add(LoginStep::Unlock { key: A, retries: 1 }),
            remove(vec![slot(1, A)], LoginStep::Connect),
            Operation::LoginRemove {
                account: 1000,
                before: 2,
                after: 0,
                removed: vec![slot(1, A)],
                step: LoginStep::Identify,
                approval: None,
                proof: None,
            },
            Operation::LoginRemove {
                account: 1000,
                before: 1,
                after: 0,
                removed: vec![slot(1, A), slot(1, A)],
                step: LoginStep::Identify,
                approval: None,
                proof: None,
            },
            Operation::LoginRemove {
                account: 1000,
                before: 9,
                after: 8,
                removed: vec![slot(1, A)],
                step: LoginStep::Identify,
                approval: None,
                proof: None,
            },
        ];
        for operation in refused {
            assert_eq!(
                Request::new([1; 32], 1000, operation.clone())
                    .err()
                    .unwrap(),
                "invalid consent login operation",
                "{operation:?}"
            );
        }
    }

    #[test]
    fn eight_removal_slots_fit_the_request_bound() {
        let request = login(Operation::LoginRemove {
            account: 1000,
            before: LOGIN_KEYS,
            after: 0,
            removed: (1..=LOGIN_KEYS)
                .map(|position| slot(position, [0xff; 4]))
                .collect(),
            step: LoginStep::Authorize {
                key: [0xff; 4],
                retries: 255,
            },
            approval: None,
            proof: None,
        });
        // Header 45, account and counts 6, the set 1 + 8 * 5, the step 1 + 4 + 1.
        assert_eq!(request.encode().len(), 98);
        assert!(request.encode().len() <= MAX_BYTES);
        assert_eq!(Request::decode(&request.encode()).unwrap(), request);
        let widest = every_login_operation()
            .into_iter()
            .map(|operation| login(operation).encode().len())
            .max();
        assert_eq!(widest, Some(98));
        // The widest disclosure step, a removal of eight keys at its
        // identify, carries the key and stays below it.
        let identify = login(
            Operation::LoginRemove {
                account: 1000,
                before: LOGIN_KEYS,
                after: 0,
                removed: (1..=LOGIN_KEYS)
                    .map(|position| slot(position, [0xff; 4]))
                    .collect(),
                step: LoginStep::Identify,
                approval: None,
                proof: None,
            }
            .disclosed(disclosure()),
        );
        // Header 45, account and counts 6, the set 1 + 8 * 5, the step 1,
        // the key 2.
        assert_eq!(identify.encode().len(), 95);
        let widest = every_login_operation()
            .into_iter()
            .map(login)
            .filter(|request| request.login_approval().is_some())
            .map(|request| request.encode().len())
            .max();
        assert_eq!(widest, Some(95));
    }

    /// No login row wraps at the renderer's narrowest output, even with the
    /// widest owner and account, so wrapping never splits a fingerprint.
    #[test]
    fn every_login_row_fits_the_narrowest_prompt() {
        for operation in every_login_operation() {
            let mut operation = operation;
            if let Operation::LoginUnlock { account, .. }
            | Operation::LoginEnroll { account, .. }
            | Operation::LoginAdd { account, .. }
            | Operation::LoginRemove { account, .. } = &mut operation
            {
                *account = 65533;
            }
            let request = Request::new([1; 32], 65533, operation).unwrap();
            for line in request.lines() {
                assert!(line.len() <= PROMPT_COLUMNS, "{line:?}");
            }
        }
    }

    /// Begins at `first`, admits `steps` with their created keys, expecting
    /// `Last` only at the end, then refuses every step after the last.
    fn walk(
        first: Operation,
        baseline: &[Fingerprint],
        steps: &[(Operation, Option<Fingerprint>)],
    ) {
        let mut current = Request::begin_login([9; 32], 1000, first, baseline).unwrap();
        for (index, (operation, created)) in steps.iter().enumerate() {
            let next = login(operation.clone());
            let expected = if index + 1 == steps.len() {
                Admitted::Last
            } else {
                Admitted::Next
            };
            assert_eq!(
                current.admit_login_step(&next, baseline, *created),
                Ok(expected),
                "{operation:?}"
            );
            current = next;
        }
        for operation in every_login_operation() {
            let next = login(operation);
            for created in [None, Some(A), Some(N), Some(M)] {
                assert!(current.admit_login_step(&next, baseline, created).is_err());
            }
        }
    }

    #[test]
    fn login_steps_are_admitted_only_in_their_order() {
        let baseline = [B, A];
        walk(
            unlock(LoginStep::Identify),
            &baseline,
            &[(unlock(LoginStep::Unlock { key: A, retries: 8 }), None)],
        );
        walk(
            add(LoginStep::Identify),
            &baseline,
            &[
                (add(LoginStep::Authorize { key: B, retries: 8 }), None),
                (add(LoginStep::Connect), None),
                (add(LoginStep::Create { retries: 8 }), None),
                (add(LoginStep::Prove { key: N, retries: 8 }), Some(N)),
                (add(LoginStep::Repeat { key: N, retries: 7 }), Some(N)),
                (add(LoginStep::Probe { key: N }), Some(N)),
            ],
        );
        // Two slots may share a fingerprint; positions tell them apart.
        let both = vec![slot(1, A), slot(3, A)];
        walk(
            remove(both.clone(), LoginStep::Identify),
            &[A, B, A],
            &[(
                remove(both, LoginStep::Authorize { key: A, retries: 1 }),
                None,
            )],
        );
        let ceremony = |after, key, new| {
            vec![
                (enroll(after, key, LoginStep::Create { retries: 8 }), None),
                (
                    enroll(
                        after,
                        key,
                        LoginStep::Prove {
                            key: new,
                            retries: 8,
                        },
                    ),
                    Some(new),
                ),
                (
                    enroll(
                        after,
                        key,
                        LoginStep::Repeat {
                            key: new,
                            retries: 8,
                        },
                    ),
                    Some(new),
                ),
                (enroll(after, key, LoginStep::Probe { key: new }), Some(new)),
            ]
        };
        walk(enroll(1, 1, LoginStep::Connect), &[], &ceremony(1, 1, N));
        // The first key's probe is not the last step of a two-key enrollment.
        let mut two = ceremony(2, 1, N);
        two.push((enroll(2, 2, LoginStep::Connect), None));
        two.extend(ceremony(2, 2, M));
        walk(enroll(2, 1, LoginStep::Connect), &[], &two);
    }

    #[test]
    fn a_login_operation_begins_only_at_its_first_step() {
        let begin = |operation, baseline: &[Fingerprint]| {
            Request::begin_login([9; 32], 1000, operation, baseline)
        };
        assert!(begin(unlock(LoginStep::Identify), &[B, A]).is_ok());
        assert!(begin(enroll(2, 1, LoginStep::Connect), &[]).is_ok());
        for (operation, baseline) in [
            (
                unlock(LoginStep::Unlock { key: A, retries: 8 }),
                &[B, A][..],
            ),
            (add(LoginStep::Authorize { key: A, retries: 8 }), &[B, A]),
            (add(LoginStep::Connect), &[B, A]),
            (enroll(2, 1, LoginStep::Create { retries: 8 }), &[]),
            (enroll(2, 2, LoginStep::Connect), &[]),
            (
                remove(
                    vec![slot(1, B)],
                    LoginStep::Authorize { key: B, retries: 8 },
                ),
                &[B, A, N],
            ),
        ] {
            assert_eq!(
                begin(operation.clone(), baseline).err().unwrap(),
                "a login operation begins at its first step",
                "{operation:?}"
            );
        }
        // The counts and removal slots must be the presented record's.
        for (operation, baseline) in [
            (unlock(LoginStep::Identify), &[A][..]),
            (enroll(1, 1, LoginStep::Connect), &[A]),
            (remove(vec![slot(2, B)], LoginStep::Identify), &[B, A, N]),
        ] {
            assert_eq!(
                begin(operation, baseline).err().unwrap(),
                "login step does not match its baseline record"
            );
        }
        let store = Operation::Unlock {
            role: Role::Primary,
        };
        assert!(begin(store, &[]).is_err());
    }

    #[test]
    fn login_step_admission_refuses_what_root_did_not_derive() {
        let baseline = [B, A];
        let admit = |current: &Operation, next: Operation, baseline: &[Fingerprint], created| {
            login(current.clone()).admit_login_step(&login(next), baseline, created)
        };
        let identify = add(LoginStep::Identify);
        let authorize = |key| add(LoginStep::Authorize { key, retries: 8 });
        assert_eq!(
            admit(&identify, authorize(A), &baseline, None),
            Ok(Admitted::Next)
        );
        // A forged fingerprint, or a created key reported early.
        assert!(admit(&identify, authorize(N), &baseline, None).is_err());
        assert!(admit(&identify, authorize(A), &baseline, Some(N)).is_err());
        let selecting = unlock(LoginStep::Identify);
        let unlocking = |key| unlock(LoginStep::Unlock { key, retries: 8 });
        assert_eq!(
            admit(&selecting, unlocking(A), &baseline, None),
            Ok(Admitted::Last)
        );
        assert!(admit(&selecting, unlocking(N), &baseline, None).is_err());
        let removing = remove(vec![slot(2, A)], LoginStep::Identify);
        let removal = remove(
            vec![slot(2, A)],
            LoginStep::Authorize { key: B, retries: 8 },
        );
        assert_eq!(
            admit(&removing, removal.clone(), &[B, A, N], None),
            Ok(Admitted::Last)
        );
        assert!(admit(&removing, removal.clone(), &[B, A, N], Some(N)).is_err());
        let create = add(LoginStep::Create { retries: 8 });
        let prove = |key| add(LoginStep::Prove { key, retries: 8 });
        assert!(admit(&create, prove(N), &baseline, Some(N)).is_ok());
        assert!(admit(&create, prove(N), &baseline, None).is_err());
        assert!(admit(&create, prove(N), &baseline, Some(M)).is_err());
        assert!(admit(&create, prove(A), &baseline, Some(N)).is_err());
        // A created credential no baseline slot may name.
        assert!(admit(&create, prove(A), &baseline, Some(A)).is_err());
        let repeat = |key| add(LoginStep::Repeat { key, retries: 8 });
        assert!(admit(&prove(N), repeat(N), &baseline, Some(N)).is_ok());
        assert!(admit(&prove(N), repeat(M), &baseline, Some(M)).is_err());
        assert!(admit(&prove(A), repeat(A), &baseline, Some(A)).is_err());
        let probe = |key| add(LoginStep::Probe { key });
        assert_eq!(
            admit(&repeat(N), probe(N), &baseline, Some(N)),
            Ok(Admitted::Last)
        );
        assert!(admit(&repeat(N), probe(M), &baseline, Some(M)).is_err());
        assert!(admit(&repeat(A), probe(A), &baseline, Some(A)).is_err());
        let connect = add(LoginStep::Connect);
        assert!(admit(&authorize(A), connect.clone(), &baseline, None).is_ok());
        assert!(admit(&authorize(A), connect, &baseline, Some(N)).is_err());
        assert!(admit(&add(LoginStep::Connect), create.clone(), &baseline, Some(N)).is_err());
        // A blocked key, which the worker reports instead of asking its PIN.
        let blocked = add(LoginStep::Authorize { key: A, retries: 0 });
        assert_eq!(
            admit(&identify, blocked, &baseline, None).err().unwrap(),
            "a blocked key cannot take a PIN"
        );
        let blocked = add(LoginStep::Create { retries: 0 });
        assert!(admit(&add(LoginStep::Connect), blocked, &baseline, None).is_err());
        let blocked = unlock(LoginStep::Unlock { key: A, retries: 0 });
        assert!(admit(&selecting, blocked, &baseline, None).is_err());
        let blocked = add(LoginStep::Prove { key: N, retries: 0 });
        assert!(admit(&create, blocked, &baseline, Some(N)).is_err());
        // Counts inconsistent with the baseline.
        assert!(admit(&identify, authorize(A), &[A], None).is_err());
        assert!(admit(&identify, authorize(A), &[B, A, N], None).is_err());
        assert!(admit(&selecting, unlocking(A), &[A], None).is_err());
        assert!(admit(&selecting, unlocking(A), &[A, B, N], None).is_err());
        let three = Operation::LoginAdd {
            account: 1000,
            before: 3,
            after: 4,
            step: LoginStep::Authorize { key: A, retries: 8 },
            proof: None,
        };
        assert!(admit(&identify, three, &[B, A, N], None).is_err());
        let first = enroll(1, 1, LoginStep::Connect);
        let created = |after| enroll(after, 1, LoginStep::Create { retries: 8 });
        assert!(admit(&first, created(1), &[], None).is_ok());
        assert!(admit(&first, created(1), &[A], None).is_err());
        assert!(admit(&first, created(2), &[], None).is_err());
        // A removal slot whose position names another fingerprint, or none.
        assert!(admit(&removing, removal.clone(), &[A, B, N], None).is_err());
        assert!(admit(&removing, removal, &[B, M, N], None).is_err());
        let other = remove(
            vec![slot(1, B)],
            LoginStep::Authorize { key: B, retries: 8 },
        );
        assert!(admit(&removing, other, &[B, A, N], None).is_err());
        let moved = remove(
            vec![slot(3, A)],
            LoginStep::Authorize { key: B, retries: 8 },
        );
        assert!(admit(&removing, moved, &[B, A, A], None).is_err());
        // Another operation, nonce, owner or account.
        assert!(admit(&identify, unlocking(A), &baseline, None).is_err());
        let current = login(identify.clone());
        let renonced = Request::new([8; 32], 1000, authorize(A)).unwrap();
        assert!(current
            .admit_login_step(&renonced, &baseline, None)
            .is_err());
        let other_account = Request::new(
            [9; 32],
            1001,
            Operation::LoginAdd {
                account: 1001,
                before: 2,
                after: 3,
                step: LoginStep::Authorize { key: A, retries: 8 },
                proof: None,
            },
        )
        .unwrap();
        assert!(current
            .admit_login_step(&other_account, &baseline, None)
            .is_err());
        let store = Request::new(
            [9; 32],
            1000,
            Operation::Unlock {
                role: Role::Primary,
            },
        )
        .unwrap();
        assert!(current.admit_login_step(&store, &baseline, None).is_err());
        assert!(store.admit_login_step(&current, &baseline, None).is_err());
        // Out of order: repeated, skipped, backwards or restarted.
        for (from, to, created) in [
            (&identify, add(LoginStep::Identify), None),
            (&identify, add(LoginStep::Connect), None),
            (&authorize(A), create.clone(), None),
            (&authorize(A), add(LoginStep::Identify), None),
            (&create, repeat(N), Some(N)),
            (&prove(N), probe(N), Some(N)),
            (&prove(N), prove(N), Some(N)),
        ] {
            assert!(admit(from, to, &baseline, created).is_err());
        }
        // A first enrollment's second key follows only the first's probe,
        // and only the second key's probe is last.
        let probe_first = enroll(2, 1, LoginStep::Probe { key: N });
        assert_eq!(
            admit(
                &enroll(2, 1, LoginStep::Repeat { key: N, retries: 8 }),
                probe_first.clone(),
                &[],
                Some(N)
            ),
            Ok(Admitted::Next)
        );
        assert_eq!(
            admit(&probe_first, enroll(2, 2, LoginStep::Connect), &[], None),
            Ok(Admitted::Next)
        );
        assert!(admit(&probe_first, enroll(2, 1, LoginStep::Connect), &[], None).is_err());
        assert!(admit(&probe_first, enroll(2, 2, LoginStep::Connect), &[], Some(N)).is_err());
        assert_eq!(
            admit(
                &enroll(2, 2, LoginStep::Repeat { key: M, retries: 8 }),
                enroll(2, 2, LoginStep::Probe { key: M }),
                &[],
                Some(M)
            ),
            Ok(Admitted::Last)
        );
        let create_first = enroll(2, 1, LoginStep::Create { retries: 8 });
        let create_second = enroll(2, 2, LoginStep::Create { retries: 8 });
        assert!(admit(&create_first, create_second, &[], None).is_err());
        let single = enroll(1, 1, LoginStep::Probe { key: N });
        assert!(admit(&single, enroll(1, 1, LoginStep::Connect), &[], None).is_err());
    }

    /// A step's name and new-key ordinal, read from the operation alone.
    fn place(request: &Request) -> (&'static str, u8) {
        let (step, key) = match request.operation() {
            Operation::LoginEnroll { key, step, .. } => (*step, *key),
            Operation::LoginUnlock { step, .. }
            | Operation::LoginAdd { step, .. }
            | Operation::LoginRemove { step, .. } => (*step, 1),
            _ => return ("none", 0),
        };
        let name = match step {
            LoginStep::Identify => "identify",
            LoginStep::Authorize { .. } => "authorize",
            LoginStep::Create { .. } => "create",
            LoginStep::Prove { .. } => "prove",
            LoginStep::Repeat { .. } => "repeat",
            LoginStep::Probe { .. } => "probe",
            LoginStep::Unlock { .. } => "unlock",
            LoginStep::Connect => "connect",
            LoginStep::RecoveryKey => "recovery key",
            LoginStep::DiskIdentify => "disk identify",
            LoginStep::DiskAuthorize { .. } => "disk authorize",
        };
        (name, key)
    }

    /// A step's fingerprint and retries, read from the operation alone.
    fn device(request: &Request) -> (Option<Fingerprint>, Option<u8>) {
        let step = match request.operation() {
            Operation::LoginUnlock { step, .. }
            | Operation::LoginEnroll { step, .. }
            | Operation::LoginAdd { step, .. }
            | Operation::LoginRemove { step, .. } => *step,
            _ => return (None, None),
        };
        match step {
            LoginStep::Identify
            | LoginStep::Connect
            | LoginStep::RecoveryKey
            | LoginStep::DiskIdentify => (None, None),
            LoginStep::Create { retries } => (None, Some(retries)),
            LoginStep::Probe { key } => (Some(key), None),
            LoginStep::Authorize { key, retries }
            | LoginStep::Prove { key, retries }
            | LoginStep::Repeat { key, retries }
            | LoginStep::Unlock { key, retries }
            | LoginStep::DiskAuthorize { key, retries } => (Some(key), Some(retries)),
        }
    }

    #[test]
    fn login_successors_follow_each_operations_literal_table() {
        let ceremony = |key| {
            [
                ("connect", key),
                ("create", key),
                ("prove", key),
                ("repeat", key),
                ("probe", key),
            ]
        };
        let one = ceremony(1).to_vec();
        let two = [ceremony(1), ceremony(2)].concat();
        /// An operation's step builder, its new-key count and its order.
        type Table<'a> = (fn(u8, LoginStep) -> Operation, u8, &'a [(&'a str, u8)]);
        let tables: &[Table] = &[
            (|_, step| unlock(step), 1, &[("identify", 1), ("unlock", 1)]),
            (
                |_, step| remove(vec![slot(2, A)], step),
                1,
                &[("identify", 1), ("authorize", 1)],
            ),
            (
                |_, step| add(step),
                1,
                &[
                    ("identify", 1),
                    ("authorize", 1),
                    ("connect", 1),
                    ("create", 1),
                    ("prove", 1),
                    ("repeat", 1),
                    ("probe", 1),
                ],
            ),
            (|key, step| enroll(1, key, step), 1, &one),
            (|key, step| enroll(2, key, step), 2, &two),
        ];
        for (make, ordinals, table) in tables {
            // Every description of the operation: each step with two
            // fingerprints and blocked or unblocked retries, at each ordinal.
            let mut steps = Vec::new();
            for key in [A, N] {
                for step in every_step(key).into_iter().chain([
                    LoginStep::Authorize { key, retries: 0 },
                    LoginStep::Create { retries: 8 },
                    LoginStep::Repeat { key, retries: 0 },
                    LoginStep::Unlock { key, retries: 0 },
                ]) {
                    for ordinal in 1..=*ordinals {
                        if let Ok(request) = Request::new([9; 32], 1000, make(ordinal, step)) {
                            steps.push(request);
                        }
                    }
                }
            }
            let mut admitted = Vec::new();
            let mut device_refusals = 0;
            for current in &steps {
                let index = table.iter().position(|entry| *entry == place(current));
                assert!(index.is_some(), "{:?}", place(current));
                for next in &steps {
                    let ordered = index
                        .and_then(|index| table.get(index + 1))
                        .is_some_and(|entry| *entry == place(next));
                    // A ceremony keeps its credential; a blocked key takes
                    // no PIN.
                    let kept = !matches!(place(next).0, "repeat" | "probe")
                        || device(next).0 == device(current).0;
                    let unblocked = device(next).1 != Some(0);
                    let expected = (ordered && kept && unblocked).then(|| {
                        if Some(&place(next)) == table.last() {
                            Admitted::Last
                        } else {
                            Admitted::Next
                        }
                    });
                    if ordered && expected.is_none() {
                        device_refusals += 1;
                    }
                    let result = current.login_successor(next);
                    assert_eq!(result.clone().ok(), expected, "{current:?} -> {next:?}");
                    if result.is_ok() {
                        admitted.push((place(current), place(next), result));
                    }
                }
            }
            // Each table pair is admitted, with fingerprints only the record
            // or the worker's report can refuse.
            for pair in table.windows(2) {
                assert!(admitted.iter().any(|(from, to, _)| [*from, *to] == pair));
            }
            assert!(admitted.iter().all(
                |(_, to, result)| (Some(to) == table.last()) == (*result == Ok(Admitted::Last))
            ));
            assert!(device_refusals > 0);
        }
    }

    #[test]
    fn a_two_key_enrollment_moves_to_its_second_key_only_after_the_first_probe() {
        let successor = |current, next| login(current).login_successor(&login(next));
        let probe = |key, new| enroll(2, key, LoginStep::Probe { key: new });
        let connect = |key| enroll(2, key, LoginStep::Connect);
        let create = |key| enroll(2, key, LoginStep::Create { retries: 8 });
        assert_eq!(successor(probe(1, N), connect(2)), Ok(Admitted::Next));
        assert_eq!(successor(connect(2), create(2)), Ok(Admitted::Next));
        // The second key's probe ends the operation.
        for next in [connect(1), connect(2), create(2)] {
            assert_eq!(
                successor(probe(2, M), next),
                Err("login operation has no further step".into())
            );
        }
        for (current, next) in [
            // The first key again, or its ceremony restarted.
            (probe(1, N), connect(1)),
            (probe(1, N), create(1)),
            // The second key's connect skipped.
            (probe(1, N), create(2)),
            // The second key before the first finishes.
            (
                enroll(2, 1, LoginStep::Repeat { key: N, retries: 8 }),
                connect(2),
            ),
            (connect(1), connect(2)),
            (create(1), create(2)),
            // Back to the first key.
            (connect(2), create(1)),
        ] {
            assert_eq!(
                successor(current.clone(), next.clone()),
                Err("illegal login step order".into()),
                "{current:?} -> {next:?}"
            );
        }
        // The second key's ceremony keeps its own credential, which may be
        // any the worker reports, the first key's included.
        let prove = |new| {
            enroll(
                2,
                2,
                LoginStep::Prove {
                    key: new,
                    retries: 8,
                },
            )
        };
        let repeat = |new| {
            enroll(
                2,
                2,
                LoginStep::Repeat {
                    key: new,
                    retries: 8,
                },
            )
        };
        assert_eq!(successor(create(2), prove(M)), Ok(Admitted::Next));
        assert_eq!(successor(prove(M), repeat(M)), Ok(Admitted::Next));
        assert_eq!(successor(repeat(M), probe(2, M)), Ok(Admitted::Last));
        assert_eq!(
            successor(prove(M), repeat(N)),
            Err("login step changed its key".into())
        );
        assert_eq!(
            successor(repeat(M), probe(2, N)),
            Err("login step changed its key".into())
        );
        // A one-key enrollment ends at its first probe.
        assert_eq!(
            successor(
                enroll(1, 1, LoginStep::Probe { key: N }),
                enroll(1, 1, LoginStep::Connect)
            ),
            Err("login operation has no further step".into())
        );
        assert_eq!(
            successor(enroll(1, 1, LoginStep::Probe { key: N }), connect(2)),
            Err("login step changed its operation".into())
        );
    }

    #[test]
    fn a_login_successor_refuses_a_changed_operation_key_or_order() {
        // Field by field, past what `Request::new` would build: each check
        // stands on its own.
        let forged = |nonce, owner, operation| Request {
            nonce,
            owner,
            operation,
        };
        let changed = Err("login step changed its operation".into());
        let order = Err("illegal login step order".into());
        let identify = login(add(LoginStep::Identify));
        let authorize = |account, before, after| Operation::LoginAdd {
            account,
            before,
            after,
            step: LoginStep::Authorize { key: N, retries: 8 },
            proof: None,
        };
        assert_eq!(
            identify.login_successor(&login(authorize(1000, 2, 3))),
            Ok(Admitted::Next)
        );
        for next in [
            Request::new([8; 32], 1000, authorize(1000, 2, 3)).unwrap(),
            Request::new([9; 32], 1001, authorize(1001, 2, 3)).unwrap(),
            forged([9; 32], 1001, authorize(1000, 2, 3)),
            forged([9; 32], 1000, authorize(1001, 2, 3)),
            login(authorize(1000, 3, 4)),
            forged([9; 32], 1000, authorize(1000, 1, 3)),
            forged([9; 32], 1000, authorize(1000, 2, 2)),
            // Removal's authorize, its counts unchanged.
            forged(
                [9; 32],
                1000,
                Operation::LoginRemove {
                    account: 1000,
                    before: 2,
                    after: 3,
                    removed: Vec::new(),
                    step: LoginStep::Authorize { key: N, retries: 8 },
                    approval: None,
                    proof: None,
                },
            ),
        ] {
            assert_eq!(identify.login_successor(&next), changed, "{next:?}");
        }
        // The removal slots alone, the counts kept: another position,
        // another fingerprint, or another slot as well.
        let removing = login(remove(vec![slot(2, A)], LoginStep::Identify));
        let removal = |removed| {
            forged(
                [9; 32],
                1000,
                Operation::LoginRemove {
                    account: 1000,
                    before: 3,
                    after: 2,
                    removed,
                    step: LoginStep::Authorize { key: B, retries: 8 },
                    approval: None,
                    proof: None,
                },
            )
        };
        assert_eq!(
            removing.login_successor(&removal(vec![slot(2, A)])),
            Ok(Admitted::Last)
        );
        for removed in [
            vec![slot(1, A)],
            vec![slot(2, B)],
            vec![slot(2, A), slot(3, A)],
        ] {
            let next = removal(removed);
            assert_eq!(removing.login_successor(&next), changed, "{next:?}");
        }
        // Repeated, skipped or backwards.
        for (current, next) in [
            (add(LoginStep::Identify), add(LoginStep::Identify)),
            (add(LoginStep::Identify), add(LoginStep::Connect)),
            (
                add(LoginStep::Authorize { key: A, retries: 8 }),
                add(LoginStep::Create { retries: 8 }),
            ),
            (
                add(LoginStep::Prove { key: N, retries: 8 }),
                add(LoginStep::Probe { key: N }),
            ),
            (add(LoginStep::Connect), add(LoginStep::Identify)),
            (unlock(LoginStep::Identify), unlock(LoginStep::Identify)),
        ] {
            assert_eq!(
                login(current.clone()).login_successor(&login(next.clone())),
                order,
                "{current:?} -> {next:?}"
            );
        }
        // A ceremony keeps the credential its prove step named, and a
        // blocked key takes no PIN.
        let key = Err("login step changed its key".into());
        let blocked = Err("a blocked key cannot take a PIN".into());
        let prove = |new, retries| add(LoginStep::Prove { key: new, retries });
        let repeat = |new, retries| add(LoginStep::Repeat { key: new, retries });
        let probe = |new| add(LoginStep::Probe { key: new });
        for (current, next, expected) in [
            (prove(N, 8), repeat(N, 1), Ok(Admitted::Next)),
            (repeat(N, 8), probe(N), Ok(Admitted::Last)),
            (prove(N, 8), repeat(M, 8), key.clone()),
            (repeat(N, 8), probe(M), key),
            (
                add(LoginStep::Identify),
                add(LoginStep::Authorize { key: A, retries: 0 }),
                blocked.clone(),
            ),
            (
                add(LoginStep::Connect),
                add(LoginStep::Create { retries: 0 }),
                blocked.clone(),
            ),
            (
                add(LoginStep::Create { retries: 8 }),
                prove(N, 0),
                blocked.clone(),
            ),
            (prove(N, 8), repeat(N, 0), blocked.clone()),
            (
                unlock(LoginStep::Identify),
                unlock(LoginStep::Unlock { key: A, retries: 0 }),
                blocked,
            ),
        ] {
            assert_eq!(
                login(current.clone()).login_successor(&login(next.clone())),
                expected,
                "{current:?} -> {next:?}"
            );
        }
        // Nothing follows the final step.
        let unlocked = login(unlock(LoginStep::Unlock { key: A, retries: 8 }));
        for next in [
            unlock(LoginStep::Identify),
            unlock(LoginStep::Unlock { key: A, retries: 8 }),
        ] {
            assert_eq!(
                unlocked.login_successor(&login(next)),
                Err("login operation has no further step".into())
            );
        }
        // Neither side may be another kind of request.
        let store = Request::new(
            [9; 32],
            1000,
            Operation::Unlock {
                role: Role::Primary,
            },
        )
        .unwrap();
        let not_login = Err("not a login operation step".into());
        assert_eq!(identify.login_successor(&store), not_login);
        assert_eq!(store.login_successor(&identify), not_login);
        assert_eq!(store.login_successor(&store), not_login);
    }

    #[test]
    fn root_admits_only_what_the_successor_shape_admits_and_then_the_device_data() {
        // The shape passes a fingerprint no record holds, and a credential
        // the worker did not report; root refuses both.
        let selecting = login(unlock(LoginStep::Identify));
        let forged = login(unlock(LoginStep::Unlock { key: N, retries: 8 }));
        assert_eq!(selecting.login_successor(&forged), Ok(Admitted::Last));
        assert!(selecting.admit_login_step(&forged, &[B, A], None).is_err());
        let create = login(add(LoginStep::Create { retries: 8 }));
        let prove = login(add(LoginStep::Prove { key: N, retries: 8 }));
        assert_eq!(create.login_successor(&prove), Ok(Admitted::Next));
        assert!(create.admit_login_step(&prove, &[B, A], None).is_err());
        assert!(create.admit_login_step(&prove, &[B, A], Some(M)).is_err());
        assert_eq!(
            create.admit_login_step(&prove, &[B, A], Some(N)),
            Ok(Admitted::Next)
        );
        // A wrong baseline is root's alone.
        let unlocking = login(unlock(LoginStep::Unlock { key: A, retries: 8 }));
        assert_eq!(selecting.login_successor(&unlocking), Ok(Admitted::Last));
        assert_eq!(
            selecting.admit_login_step(&unlocking, &[A], None),
            Err("login step does not match its baseline record".into())
        );
        assert_eq!(
            selecting.admit_login_step(&unlocking, &[B, A], None),
            Ok(Admitted::Last)
        );
        // Whatever the shape refuses, root refuses. This holds by
        // construction, since root's admission begins with the shape; the
        // loop keeps a later split from losing it.
        for operation in every_login_operation() {
            let current = Request::new([9; 32], 1000, operation).unwrap();
            for next in every_login_operation() {
                let next = login(next);
                if current.login_successor(&next).is_err() {
                    for created in [None, Some(A), Some(N)] {
                        for baseline in [&[][..], &[B, A], &[A, B, A]] {
                            assert!(current.admit_login_step(&next, baseline, created).is_err());
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn a_login_start_is_each_operations_first_step_without_the_record() {
        let first = "a login operation begins at its first step";
        for operation in every_login_operation() {
            let request = login(operation.clone());
            let expected = match (&operation, place(&request)) {
                (Operation::LoginEnroll { .. }, ("connect", 1)) => true,
                (Operation::LoginEnroll { .. }, _) => false,
                (_, ("identify", 1)) => true,
                _ => false,
            };
            assert_eq!(
                request.login_start(),
                if expected { Ok(()) } else { Err(first.into()) },
                "{operation:?}"
            );
        }
        for operation in [
            // An enrollment's second key, an unlock at its unlock step, an
            // addition's mid-operation connect, a removal at its authorize.
            enroll(2, 2, LoginStep::Connect),
            unlock(LoginStep::Unlock { key: A, retries: 8 }),
            add(LoginStep::Connect),
            remove(
                vec![slot(1, B)],
                LoginStep::Authorize { key: B, retries: 8 },
            ),
        ] {
            assert_eq!(login(operation).login_start(), Err(first.into()));
        }
        for operation in [
            enroll(1, 1, LoginStep::Connect),
            enroll(2, 1, LoginStep::Connect),
            unlock(LoginStep::Identify),
            add(LoginStep::Identify),
            remove(vec![slot(1, B)], LoginStep::Identify),
        ] {
            assert_eq!(login(operation).login_start(), Ok(()));
        }
        let store = Request::new(
            [9; 32],
            1000,
            Operation::Unlock {
                role: Role::Primary,
            },
        )
        .unwrap();
        assert_eq!(
            store.login_start(),
            Err("not a login operation step".into())
        );
        // Root's beginning is the start's shape, then the record.
        let begin = |operation, baseline: &[Fingerprint]| {
            Request::begin_login([9; 32], 1000, operation, baseline)
        };
        assert_eq!(
            begin(enroll(2, 2, LoginStep::Connect), &[]).err().unwrap(),
            first
        );
        assert_eq!(
            begin(unlock(LoginStep::Identify), &[A]).err().unwrap(),
            "login step does not match its baseline record"
        );
        assert!(begin(unlock(LoginStep::Identify), &[B, A]).is_ok());
    }

    #[test]
    fn login_accessors_name_the_pin_steps_the_kind_and_the_ceiling() {
        for (step, pin) in [
            (LoginStep::Identify, false),
            (LoginStep::Authorize { key: A, retries: 1 }, true),
            (LoginStep::Create { retries: 1 }, true),
            (LoginStep::Prove { key: A, retries: 1 }, true),
            (LoginStep::Repeat { key: A, retries: 1 }, true),
            (LoginStep::Probe { key: A }, false),
            (LoginStep::Unlock { key: A, retries: 1 }, true),
            (LoginStep::Connect, false),
            // The recovery key is the secret the PIN field takes there.
            (LoginStep::RecoveryKey, true),
            (LoginStep::DiskIdentify, false),
            (LoginStep::DiskAuthorize { key: A, retries: 1 }, true),
        ] {
            assert_eq!(step.asks_pin(), pin, "{step:?}");
            assert_eq!(
                step.proves(),
                matches!(
                    step,
                    LoginStep::RecoveryKey
                        | LoginStep::DiskIdentify
                        | LoginStep::DiskAuthorize { .. }
                ),
                "{step:?}"
            );
        }
        assert_eq!(LOGIN_CEREMONY, Duration::from_secs(120));
        assert_eq!(LOGIN_TWO_CEREMONIES, Duration::from_secs(240));
        assert_eq!(LOGIN_READING, Duration::from_secs(120));
        assert_eq!(
            LOGIN_TWO_KEY_ENROLLMENT,
            LOGIN_TWO_CEREMONIES + LOGIN_READING
        );
        // A two-key enrollment by disk proof: three ceremonies and reading.
        assert_eq!(
            LOGIN_LONGEST,
            LOGIN_TWO_CEREMONIES + LOGIN_CEREMONY + LOGIN_READING
        );
        assert_eq!(LOGIN_LONGEST, Duration::from_secs(480));
        let ceremony = Some(Duration::from_secs(120));
        let two = Some(Duration::from_secs(240));
        // A disclosing operation's ceiling adds the reading allowance to
        // its ceremonies, on every step.
        let read = Some(Duration::from_secs(240));
        let two_read = Some(Duration::from_secs(360));
        let mut seen = [false; 5];
        for operation in every_login_operation() {
            let request = login(operation.clone());
            assert!(request.is_login());
            let (index, expected, step) = match operation {
                Operation::LoginUnlock { step, .. } => Some((0, ceremony, step)),
                Operation::LoginRemove {
                    after: 2.., step, ..
                } => Some((1, ceremony, step)),
                Operation::LoginRemove { step, .. }
                | Operation::LoginEnroll { after: 1, step, .. } => Some((2, read, step)),
                Operation::LoginEnroll { step, .. } => Some((3, two_read, step)),
                Operation::LoginAdd { step, .. } => Some((4, two, step)),
                _ => None,
            }
            .unwrap();
            seen[index] = true;
            assert_eq!(request.login_ceiling(), expected, "{operation:?}");
            assert_eq!(request.login_step(), Some(step));
        }
        assert_eq!(seen, [true; 5]);
        for operation in [
            Operation::Unlock {
                role: Role::Primary,
            },
            Operation::Enroll {
                platform: Platform::TpmPcr7,
                recovery: Recovery::SecondToken,
                step: Enrollment::CreatePrimary,
            },
        ] {
            let request = Request::new([9; 32], 1000, operation).unwrap();
            assert!(!request.is_login());
            assert_eq!(request.login_ceiling(), None);
            assert_eq!(request.login_step(), None);
        }
    }

    #[test]
    fn login_prompts_show_counts_slots_and_device_retries() {
        let rows = |operation| login(operation).lines();
        assert_eq!(
            rows(unlock(LoginStep::Identify)),
            [
                "TD SECURE ATTENTION",
                "SESSION USER 1000",
                "UNLOCK SESSION WITH A LOGIN KEY",
                "ACCOUNT UID 1000",
                "CONNECT ONLY ONE ENROLLED KEY",
                "ESC TO CANCEL",
            ]
        );
        assert_eq!(
            rows(unlock(LoginStep::Unlock { key: A, retries: 7 }))[4..],
            [
                "UNLOCK WITH KEY 3fa2c1d0",
                "7 PIN ATTEMPTS LEFT ON THIS KEY",
                "ENTER ITS PIN, THEN TOUCH THE KEY",
                "ESC TO CANCEL",
            ]
        );
        assert_eq!(
            rows(add(LoginStep::Connect))[2..],
            [
                "ADD A LOGIN KEY (2 -> 3 KEYS)",
                "ACCOUNT UID 1000",
                "REMOVE THE AUTHORIZING KEY",
                "CONNECT ONLY THE NEW KEY",
                "ESC TO CANCEL",
            ]
        );
        assert_eq!(
            rows(add(LoginStep::Authorize { key: B, retries: 8 }))[4..],
            [
                "AUTHORIZE WITH KEY 01020304",
                "8 PIN ATTEMPTS LEFT ON THIS KEY",
                "ENTER ITS PIN, THEN TOUCH THE KEY",
                "ESC TO CANCEL",
            ]
        );
        assert_eq!(
            rows(enroll(2, 1, LoginStep::Connect))[2..6],
            [
                "ENROLL LOGIN KEYS (0 -> 2 KEYS)",
                "NEW KEY 1 OF 2",
                "ACCOUNT UID 1000",
                "CONNECT ONLY THE NEW KEY",
            ]
        );
        assert_eq!(
            rows(enroll(2, 2, LoginStep::Connect))[5..],
            [
                "REMOVE THE PREVIOUS KEY",
                "CONNECT ONLY THE NEW KEY",
                "ESC TO CANCEL",
            ]
        );
        assert_eq!(
            rows(enroll(1, 1, LoginStep::Create { retries: 8 }))[2..],
            [
                "ENROLL LOGIN KEYS (0 -> 1 KEY)",
                "NEW KEY 1 OF 1",
                "ACCOUNT UID 1000",
                "CREATE A LOGIN CREDENTIAL",
                "8 PIN ATTEMPTS LEFT ON THIS KEY",
                "ENTER ITS PIN, THEN TOUCH THE KEY",
                "ESC TO CANCEL",
            ]
        );
        for (step, row) in [
            (
                LoginStep::Prove { key: N, retries: 1 },
                "VERIFY NEW KEY deadbeef",
            ),
            (
                LoginStep::Repeat { key: N, retries: 1 },
                "VERIFY NEW KEY deadbeef AGAIN",
            ),
        ] {
            assert_eq!(
                rows(enroll(1, 1, step))[5..],
                [
                    row,
                    "1 PIN ATTEMPTS LEFT ON THIS KEY",
                    "ENTER ITS PIN, THEN TOUCH THE KEY",
                    "ESC TO CANCEL",
                ]
            );
        }
        assert_eq!(
            rows(enroll(1, 1, LoginStep::Probe { key: N }))[5..],
            [
                "CHECKING NEW KEY deadbeef",
                "KEEP IT CONNECTED AND TOUCH IT",
                "ESC TO CANCEL"
            ]
        );
        assert_eq!(
            rows(remove(vec![slot(2, A)], LoginStep::Identify))[2..],
            [
                "REMOVE A LOGIN KEY (3 -> 2 KEYS)",
                "ACCOUNT UID 1000",
                "REMOVE: 2:3fa2c1d0",
                "CONNECT ONLY ONE ENROLLED KEY",
                "ESC TO CANCEL",
            ]
        );
        assert_eq!(
            rows(remove(vec![slot(1, B), slot(3, A)], LoginStep::Identify))[2..6],
            [
                "REMOVE 2 LOGIN KEYS (3 -> 1 KEY)",
                "ACCOUNT UID 1000",
                "REMOVE: 1:01020304 3:3fa2c1d0",
                "CONNECT ONLY ONE ENROLLED KEY",
            ]
        );
        assert_eq!(
            rows(Operation::LoginRemove {
                account: 1000,
                before: 8,
                after: 0,
                removed: (1..=8)
                    .map(|position| slot(position, [position; 4]))
                    .collect(),
                step: LoginStep::Authorize {
                    key: [6; 4],
                    retries: 2
                },
                approval: None,
                proof: None,
            })[2..],
            [
                "REMOVE 8 LOGIN KEYS (8 -> 0 KEYS)",
                "ACCOUNT UID 1000",
                "REMOVE: 1:01010101 2:02020202",
                "        3:03030303 4:04040404",
                "        5:05050505 6:06060606",
                "        7:07070707 8:08080808",
                "LOGIN WILL NOT NEED A KEY",
                "AUTHORIZE WITH KEY 06060606",
                "2 PIN ATTEMPTS LEFT ON THIS KEY",
                "ENTER ITS PIN, THEN TOUCH THE KEY",
                "ESC TO CANCEL",
            ]
        );
    }

    /// A disk proof's operation: `operation` with `proof` set, then the
    /// test disclosure key where its step discloses.
    fn proven(operation: Operation, proof: Proof) -> Operation {
        match operation {
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
                approval: None,
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
                approval: None,
                proof: Some(proof),
            },
            other => other,
        }
        .disclosed(disclosure())
    }

    const PROOFS: &[Proof] = &[Proof::RecoveryKey, Proof::DiskKey];

    fn proof_steps(key: Fingerprint) -> [LoginStep; 3] {
        [
            LoginStep::RecoveryKey,
            LoginStep::DiskIdentify,
            LoginStep::DiskAuthorize { key, retries: 8 },
        ]
    }

    /// Every proof, count, key ordinal, removal set and step a disk proof's
    /// operation may carry: tags 13 to 15 over every step.
    fn every_proven_operation() -> Vec<Operation> {
        let mut operations = Vec::new();
        for operation in every_login_operation() {
            for proof in PROOFS {
                for step in every_step(A).into_iter().chain(proof_steps(A)) {
                    let with = match operation.clone() {
                        Operation::LoginEnroll { .. }
                        | Operation::LoginAdd { .. }
                        | Operation::LoginRemove { .. } => proven(operation.clone(), *proof),
                        _ => continue,
                    };
                    let stepped = match with {
                        Operation::LoginEnroll {
                            account,
                            before,
                            after,
                            key,
                            proof,
                            ..
                        } => Operation::LoginEnroll {
                            account,
                            before,
                            after,
                            key,
                            step,
                            approval: None,
                            proof,
                        },
                        Operation::LoginAdd {
                            account,
                            before,
                            after,
                            proof,
                            ..
                        } => Operation::LoginAdd {
                            account,
                            before,
                            after,
                            step,
                            proof,
                        },
                        Operation::LoginRemove {
                            account,
                            before,
                            after,
                            removed,
                            proof,
                            ..
                        } => Operation::LoginRemove {
                            account,
                            before,
                            after,
                            removed,
                            step,
                            approval: None,
                            proof,
                        },
                        other => other,
                    };
                    operations.push(stepped.disclosed(disclosure()));
                }
            }
        }
        operations.retain(|operation| Request::new([1; 32], 1000, operation.clone()).is_ok());
        operations.sort_by_key(|operation| format!("{operation:?}"));
        operations.dedup();
        operations
    }

    #[test]
    fn each_disk_proof_has_an_independent_literal_encoding_and_rows() {
        // Account 1000, the proof byte, the counts, the operation's own
        // field, the step, and a disclosure step's key.
        let cases: Vec<(Operation, u8, Vec<u8>, &[&str])> = vec![
            (
                proven(enroll(2, 1, LoginStep::RecoveryKey), Proof::RecoveryKey),
                13,
                vec![1, 0, 2, 1, 9, b'3', b'6'],
                &[
                    "ENROLL LOGIN KEYS (0 -> 2 KEYS)",
                    "NEW KEY 1 OF 2",
                    "ACCOUNT UID 1000",
                    "AUTHORIZED BY DISK RECOVERY KEY",
                    "TYPE THE DISK RECOVERY KEY",
                    "BOOT AND LOCK NEED A KEY AND PIN",
                    "CONSOLE AND SSH SESSIONS END NOW",
                    "SSH ONLY FOR THE PRIMARY ACCOUNT",
                    "ADMINISTRATION NEEDS A LOGIN",
                    "PIN SET ON ANOTHER LAYOUT MAY FAIL",
                    "APPROVE: TYPE 3 THEN 6",
                    "ESC: CANCEL",
                ],
            ),
            (
                proven(enroll(1, 1, LoginStep::Connect), Proof::DiskKey),
                13,
                vec![2, 0, 1, 1, 8],
                &[
                    "ENROLL LOGIN KEYS (0 -> 1 KEY)",
                    "NEW KEY 1 OF 1",
                    "ACCOUNT UID 1000",
                    "AUTHORIZED BY A DISK SECURITY KEY",
                    "CONNECT ONLY THE NEW KEY",
                    "ESC TO CANCEL",
                ],
            ),
            (
                proven(add(LoginStep::DiskIdentify), Proof::DiskKey),
                14,
                vec![2, 2, 3, 10],
                &[
                    "ADD A LOGIN KEY (2 -> 3 KEYS)",
                    "ACCOUNT UID 1000",
                    "AUTHORIZED BY A DISK SECURITY KEY",
                    "CONNECT ONE DISK SECURITY KEY ONLY",
                    "ESC TO CANCEL",
                ],
            ),
            (
                proven(
                    add(LoginStep::DiskAuthorize { key: A, retries: 8 }),
                    Proof::DiskKey,
                ),
                14,
                vec![2, 2, 3, 11, 0x3f, 0xa2, 0xc1, 0xd0, 8],
                &[
                    "ADD A LOGIN KEY (2 -> 3 KEYS)",
                    "ACCOUNT UID 1000",
                    "AUTHORIZED BY A DISK SECURITY KEY",
                    "DISK SECURITY KEY 3fa2c1d0",
                    "8 PIN ATTEMPTS LEFT ON THIS KEY",
                    "ENTER ITS PIN, THEN TOUCH THE KEY",
                    "ESC TO CANCEL",
                ],
            ),
            (
                proven(
                    remove(vec![slot(2, A)], LoginStep::RecoveryKey),
                    Proof::RecoveryKey,
                ),
                15,
                vec![1, 3, 2, 1, 2, 0x3f, 0xa2, 0xc1, 0xd0, 9],
                &[
                    "REMOVE A LOGIN KEY (3 -> 2 KEYS)",
                    "ACCOUNT UID 1000",
                    "REMOVE: 2:3fa2c1d0",
                    "AUTHORIZED BY DISK RECOVERY KEY",
                    "TYPE THE DISK RECOVERY KEY",
                    "ESC TO CANCEL",
                ],
            ),
            (
                proven(
                    remove(vec![slot(1, B), slot(3, A)], LoginStep::DiskIdentify),
                    Proof::DiskKey,
                ),
                15,
                vec![
                    2, 3, 1, 2, 1, 1, 2, 3, 4, 3, 0x3f, 0xa2, 0xc1, 0xd0, 10, b'3', b'6',
                ],
                &[
                    "REMOVE 2 LOGIN KEYS (3 -> 1 KEY)",
                    "ACCOUNT UID 1000",
                    "REMOVE: 1:01020304 3:3fa2c1d0",
                    "AUTHORIZED BY A DISK SECURITY KEY",
                    "CONNECT ONE DISK SECURITY KEY ONLY",
                    "LOST KEY OR BLOCKED PIN: NO LOGIN",
                    "RECOVERY NEEDS BOOTING OTHER CODE",
                    "HERE, WHICH ALSO BYPASSES THE LOCK",
                    "A BACKUP KEY CAN BE ADDED LATER",
                    "APPROVE: TYPE 3 THEN 6",
                    "ESC: CANCEL",
                ],
            ),
        ];
        for (operation, tag, tail, rows) in cases {
            let request = login(operation);
            let mut literal = header(tag);
            literal.extend(&tail);
            assert_eq!(request.encode(), literal, "{request:?}");
            assert_eq!(Request::decode(&literal).unwrap(), request);
            let lines = request.lines();
            assert_eq!(lines[..2], ["TD SECURE ATTENTION", "SESSION USER 1000"]);
            assert_eq!(lines[2..], *rows, "{request:?}");
        }
    }

    #[test]
    fn a_disk_proof_keeps_its_own_tags_steps_and_proof_byte() {
        let decode = |tag, tail: &[u8]| {
            let mut bytes = header(tag);
            bytes.extend(tail);
            Request::decode(&bytes)
        };
        assert!(decode(14, &[1, 2, 3, 9]).is_ok());
        assert!(decode(14, &[2, 2, 3, 10]).is_ok());
        for proof in [0, 3, 255] {
            assert_eq!(
                decode(14, &[proof, 2, 3, 9]).err().unwrap(),
                "invalid consent disk proof"
            );
        }
        // Tags 8 to 10 never carry a proof step, 13 to 15 never identify,
        // authorize or another proof's step, and an unlock has no proof.
        for (tag, tail) in [
            (9, &[2, 3, 9][..]),
            (9, &[2, 3, 10]),
            (9, &[2, 3, 11, 1, 2, 3, 4, 8]),
            (8, &[0, 1, 1, 9, b'3', b'6']),
            (10, &[3, 2, 1, 2, 1, 2, 3, 4, 10]),
            (7, &[2, 2, 9]),
            (14, &[1, 2, 3, 1]),
            (14, &[1, 2, 3, 2, 1, 2, 3, 4, 8]),
            (14, &[1, 2, 3, 10]),
            (14, &[2, 2, 3, 9]),
            (13, &[2, 0, 1, 1, 1, b'3', b'6']),
            (15, &[1, 3, 2, 1, 2, 1, 2, 3, 4, 1]),
            (15, &[2, 3, 2, 1, 2, 1, 2, 3, 4, 2, 1, 2, 3, 4, 8]),
            // A proof is made once, at the first key.
            (13, &[1, 0, 2, 2, 9]),
            (13, &[2, 0, 2, 2, 10]),
        ] {
            assert!(decode(tag, tail).is_err(), "{tag} {tail:?}");
        }
    }

    #[test]
    fn disk_proofs_roundtrip_and_refuse_every_cut_and_trailing_byte() {
        let operations = every_proven_operation();
        // Each proof's enrollment: 6 or 7 steps at each two-key count's
        // first key, connect to probe at the second; addition's 6 or 7 at
        // 7 counts; removal's 1 or 2 over each set's valid counts.
        assert_eq!(
            operations.len(),
            (6 + 6 + 5) + (7 + 7 + 5) + 6 * 7 + 7 * 7 + (8 + 7 + 6 + 1) * 3
        );
        for operation in operations {
            let request = Request::new([1; 32], 1000, operation).unwrap();
            assert!(request.login_proof().is_some());
            let mut bytes = request.encode();
            assert_eq!(Request::decode(&bytes).unwrap(), request);
            for end in 0..bytes.len() {
                assert!(Request::decode(&bytes[..end]).is_err(), "{request:?} {end}");
            }
            bytes.push(0);
            assert!(Request::decode(&bytes).is_err());
        }
    }

    /// The widest disk proof, a removal of eight keys at its disk authorize
    /// step, is 99 bytes, and no row of any proof wraps.
    #[test]
    fn disk_proofs_fit_the_request_bound_and_the_narrowest_prompt() {
        let widest = every_proven_operation()
            .into_iter()
            .map(|operation| login(operation).encode().len())
            .max();
        assert_eq!(widest, Some(99));
        let eight = login(proven(
            Operation::LoginRemove {
                account: 1000,
                before: LOGIN_KEYS,
                after: 0,
                removed: (1..=LOGIN_KEYS)
                    .map(|position| slot(position, [0xff; 4]))
                    .collect(),
                step: LoginStep::DiskAuthorize {
                    key: [0xff; 4],
                    retries: 255,
                },
                approval: None,
                proof: None,
            },
            Proof::DiskKey,
        ));
        // Header 45, account, proof and counts 7, the set 1 + 8 * 5, the
        // step 1 + 4 + 1.
        assert_eq!(eight.encode().len(), 99);
        assert!(eight.encode().len() <= MAX_BYTES);
        for operation in every_proven_operation() {
            let mut operation = operation;
            if let Operation::LoginEnroll { account, .. }
            | Operation::LoginAdd { account, .. }
            | Operation::LoginRemove { account, .. } = &mut operation
            {
                *account = 65533;
            }
            let request = Request::new([1; 32], 65533, operation).unwrap();
            for line in request.lines() {
                assert!(line.len() <= PROMPT_COLUMNS, "{line:?}");
            }
        }
    }

    /// The proof's first step discloses where the operation does; no other
    /// step of it, a connect included, carries the key.
    #[test]
    fn a_disk_proof_discloses_on_its_first_step_only() {
        for operation in every_proven_operation() {
            let request = login(operation.clone());
            let start = request.login_start().is_ok();
            let reads = match &operation {
                Operation::LoginEnroll { .. } => true,
                Operation::LoginRemove { after, .. } => *after <= 1,
                _ => false,
            };
            assert_eq!(
                request.login_approval().is_some(),
                start && reads,
                "{operation:?}"
            );
            if start {
                assert!(request.login_step().is_some_and(LoginStep::proves));
            }
        }
    }

    #[test]
    fn a_disk_proofs_ceiling_counts_the_proof_as_one_ceremony() {
        let seconds = |operation| {
            login(operation)
                .login_ceiling()
                .map(|ceiling| ceiling.as_secs())
        };
        for proof in PROOFS {
            let first = proof_steps(A)[if *proof == Proof::RecoveryKey { 0 } else { 1 }];
            assert_eq!(seconds(proven(enroll(1, 1, first), *proof)), Some(360));
            assert_eq!(seconds(proven(enroll(2, 1, first), *proof)), Some(480));
            assert_eq!(seconds(proven(add(first), *proof)), Some(240));
            assert_eq!(
                seconds(proven(remove(vec![slot(2, A)], first), *proof)),
                Some(120)
            );
            assert_eq!(
                seconds(proven(remove(vec![slot(1, B), slot(2, A)], first), *proof)),
                Some(240)
            );
        }
        let longest = every_proven_operation()
            .into_iter()
            .chain(every_login_operation())
            .filter_map(|operation| login(operation).login_ceiling())
            .max();
        assert_eq!(longest, Some(LOGIN_LONGEST));
    }

    #[test]
    fn root_begins_a_disk_proof_only_where_the_volume_offers_it() {
        let baseline = [B, A];
        let both = DiskProofs {
            recovery_key: true,
            tokens: &[N, M],
        };
        let begin =
            |operation, disk| Request::begin_proof([9; 32], 1000, operation, &baseline, disk);
        let recovery = proven(add(LoginStep::RecoveryKey), Proof::RecoveryKey);
        let identify = proven(add(LoginStep::DiskIdentify), Proof::DiskKey);
        assert!(begin(recovery.clone(), both).is_ok());
        assert!(begin(identify.clone(), both).is_ok());
        let none = DiskProofs {
            recovery_key: false,
            tokens: &[],
        };
        assert!(begin(recovery.clone(), none).is_err());
        assert!(begin(identify.clone(), none).is_err());
        assert!(begin(
            recovery.clone(),
            DiskProofs {
                recovery_key: false,
                tokens: &[N],
            }
        )
        .is_err());
        assert!(begin(
            identify.clone(),
            DiskProofs {
                recovery_key: true,
                tokens: &[],
            }
        )
        .is_err());
        // An ordinary operation is not a proof, and a proof does not begin
        // as an ordinary one or past its first step.
        assert!(begin(add(LoginStep::Identify), both).is_err());
        assert!(Request::begin_login([9; 32], 1000, recovery.clone(), &baseline).is_err());
        assert!(Request::begin_login([9; 32], 1000, identify.clone(), &baseline).is_err());
        assert!(begin(
            proven(
                add(LoginStep::DiskAuthorize { key: N, retries: 8 }),
                Proof::DiskKey
            ),
            both
        )
        .is_err());
        // The storage worker, without the record, checks the same shape.
        assert!(login(identify.clone()).proof_start(both).is_ok());
        assert!(login(recovery.clone()).proof_start(none).is_err());
        assert!(login(add(LoginStep::Identify)).proof_start(both).is_err());
        assert!(login(proven(add(LoginStep::Connect), Proof::DiskKey))
            .proof_start(both)
            .is_err());
        // The login baseline still fixes the counts.
        assert!(Request::begin_proof(
            [9; 32],
            1000,
            proven(add(LoginStep::RecoveryKey), Proof::RecoveryKey),
            &[A],
            both
        )
        .is_err());
        // Root's login side, before any storage baseline, checks the shape
        // and the record alone: the volume's offer is not yet known.
        let shape = |operation, baseline: &[Fingerprint]| {
            Request::begin_proof_shape([9; 32], 1000, operation, baseline)
        };
        assert_eq!(shape(recovery.clone(), &baseline), begin(recovery, both));
        assert_eq!(shape(identify.clone(), &baseline), begin(identify, both));
        assert!(shape(add(LoginStep::Identify), &baseline).is_err());
        assert!(shape(proven(add(LoginStep::Connect), Proof::DiskKey), &baseline).is_err());
        assert!(shape(
            proven(
                add(LoginStep::DiskAuthorize { key: N, retries: 8 }),
                Proof::DiskKey
            ),
            &baseline
        )
        .is_err());
        assert!(shape(
            proven(add(LoginStep::RecoveryKey), Proof::RecoveryKey),
            &[A]
        )
        .is_err());
    }

    #[test]
    fn root_admits_a_disk_authorize_step_naming_a_header_token() {
        let baseline = [B, A];
        let disk = DiskProofs {
            recovery_key: false,
            tokens: &[N],
        };
        let identify = Request::begin_proof(
            [9; 32],
            1000,
            proven(add(LoginStep::DiskIdentify), Proof::DiskKey),
            &baseline,
            disk,
        )
        .unwrap();
        let authorize = |key, retries| {
            login(proven(
                add(LoginStep::DiskAuthorize { key, retries }),
                Proof::DiskKey,
            ))
        };
        assert_eq!(
            identify.admit_proof_step(&authorize(N, 8), &baseline, disk),
            Ok(Admitted::Next)
        );
        // A login key's fingerprint, an unknown one, or a blocked key.
        for (key, retries) in [(A, 8), (M, 8), (N, 0)] {
            assert!(identify
                .admit_proof_step(&authorize(key, retries), &baseline, disk)
                .is_err());
        }
        // Never the login worker's to present, nor the storage worker's
        // to present anything but its proof.
        assert!(identify
            .admit_login_step(&authorize(N, 8), &baseline, None)
            .is_err());
        let proved = authorize(N, 8);
        assert!(proved
            .admit_proof_step(
                &login(proven(add(LoginStep::Connect), Proof::DiskKey)),
                &baseline,
                disk
            )
            .is_err());
        assert!(identify
            .admit_proof_step(&authorize(N, 8), &[A], disk)
            .is_err());
        // A header token whose fingerprint a login key shares.
        let colliding = DiskProofs {
            recovery_key: false,
            tokens: &[N, A],
        };
        let identify = Request::begin_proof(
            [9; 32],
            1000,
            proven(add(LoginStep::DiskIdentify), Proof::DiskKey),
            &baseline,
            colliding,
        )
        .unwrap();
        assert_eq!(
            identify.admit_proof_step(&authorize(N, 8), &baseline, colliding),
            Ok(Admitted::Next)
        );
        assert_eq!(
            identify.admit_proof_step(&authorize(A, 8), &baseline, colliding),
            Err("disk security key names a login key".into())
        );
    }

    /// The whole sequence of each proof and operation: root's first step,
    /// the storage worker's proof steps, the login worker's first frame at
    /// the proof's last step, then its own steps to the last.
    #[test]
    fn each_disk_proof_walks_from_the_storage_worker_to_the_login_worker() {
        let disk = DiskProofs {
            recovery_key: true,
            tokens: &[M, N],
        };
        let ceremony = |make: &dyn Fn(LoginStep) -> Operation, new| {
            vec![
                (make(LoginStep::Connect), None),
                (make(LoginStep::Create { retries: 8 }), None),
                (
                    make(LoginStep::Prove {
                        key: new,
                        retries: 8,
                    }),
                    Some(new),
                ),
                (
                    make(LoginStep::Repeat {
                        key: new,
                        retries: 8,
                    }),
                    Some(new),
                ),
                (make(LoginStep::Probe { key: new }), Some(new)),
            ]
        };
        for proof in PROOFS {
            let first = match proof {
                Proof::RecoveryKey => LoginStep::RecoveryKey,
                Proof::DiskKey => LoginStep::DiskIdentify,
            };
            let last = match proof {
                Proof::RecoveryKey => LoginStep::RecoveryKey,
                Proof::DiskKey => LoginStep::DiskAuthorize { key: M, retries: 3 },
            };
            let adding = |step| proven(add(step), *proof);
            let removing = |step| proven(remove(vec![slot(1, B)], step), *proof);
            let enrolling = |key, step| proven(enroll(2, key, step), *proof);
            let first_key = |step| enrolling(1, step);
            let mut two = ceremony(&first_key, A);
            two.extend(ceremony(&|step| enrolling(2, step), B));
            /// A step builder, its login baseline and the login worker's steps.
            type Case<'a> = (
                &'a dyn Fn(LoginStep) -> Operation,
                &'a [Fingerprint],
                Vec<(Operation, Option<Fingerprint>)>,
            );
            let cases: Vec<Case> = vec![
                (&adding, &[B, A], ceremony(&adding, N)),
                (&removing, &[B, A, A], vec![]),
                (&first_key, &[], two),
            ];
            for (make, baseline, rest) in cases {
                let mut current =
                    Request::begin_proof([9; 32], 1000, make(first), baseline, disk).unwrap();
                // A removal's last proof step is its last step.
                let proved = if rest.is_empty() {
                    Admitted::Last
                } else {
                    Admitted::Next
                };
                if first != last {
                    let next = login(make(last));
                    assert_eq!(current.admit_proof_step(&next, baseline, disk), Ok(proved));
                    current = next;
                }
                // The login worker starts at exactly the step the storage
                // worker proved.
                let (resumed, admitted) =
                    Request::begin_proven([9; 32], 1000, make(last), baseline).unwrap();
                assert_eq!(resumed, current);
                assert_eq!(admitted, proved);
                for (index, (operation, created)) in rest.iter().enumerate() {
                    let next = login(operation.clone());
                    let expected = if index + 1 == rest.len() {
                        Admitted::Last
                    } else {
                        Admitted::Next
                    };
                    assert_eq!(
                        current.admit_login_step(&next, baseline, *created),
                        Ok(expected),
                        "{operation:?}"
                    );
                    current = next;
                }
            }
        }
    }

    /// The chosen proof is in every step: a successor that drops or
    /// changes it is another operation.
    #[test]
    fn a_disk_proofs_successor_keeps_its_proof() {
        let proved = login(proven(add(LoginStep::RecoveryKey), Proof::RecoveryKey));
        assert_eq!(
            proved.admit_login_step(
                &login(proven(add(LoginStep::Connect), Proof::RecoveryKey)),
                &[B, A],
                None
            ),
            Ok(Admitted::Next)
        );
        for next in [
            add(LoginStep::Connect),
            proven(add(LoginStep::Connect), Proof::DiskKey),
        ] {
            assert_eq!(
                proved.login_successor(&login(next)).err().unwrap(),
                "login step changed its operation"
            );
        }
    }

    #[test]
    fn the_login_worker_resumes_only_at_a_proven_proofs_last_step() {
        let begin = |operation, baseline: &[Fingerprint]| {
            Request::begin_proven([9; 32], 1000, operation, baseline).map(|(request, _)| request)
        };
        assert!(begin(
            proven(add(LoginStep::RecoveryKey), Proof::RecoveryKey),
            &[B, A]
        )
        .is_ok());
        assert!(begin(
            proven(
                add(LoginStep::DiskAuthorize { key: N, retries: 1 }),
                Proof::DiskKey
            ),
            &[B, A]
        )
        .is_ok());
        for (operation, baseline) in [
            // The proof's first step, a blocked key, a step after it, an
            // ordinary operation, and a baseline the counts do not match.
            (
                proven(add(LoginStep::DiskIdentify), Proof::DiskKey),
                &[B, A][..],
            ),
            (
                proven(
                    add(LoginStep::DiskAuthorize { key: N, retries: 0 }),
                    Proof::DiskKey,
                ),
                &[B, A],
            ),
            (proven(add(LoginStep::Connect), Proof::DiskKey), &[B, A]),
            (add(LoginStep::Authorize { key: A, retries: 1 }), &[B, A]),
            (add(LoginStep::Connect), &[B, A]),
            (
                proven(add(LoginStep::RecoveryKey), Proof::RecoveryKey),
                &[A],
            ),
            (
                proven(enroll(1, 1, LoginStep::RecoveryKey), Proof::RecoveryKey),
                &[A],
            ),
        ] {
            assert!(begin(operation.clone(), baseline).is_err(), "{operation:?}");
        }
    }
}

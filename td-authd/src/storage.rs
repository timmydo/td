//! Nonblocking ownership of one root storage worker, `td-install
//! storage-operation` (td-authd/DESIGN.md amendment 10): root's side of
//! its frames for a selector update, the re-encrypting upgrade's phase 1
//! and its cancellation, protector management, or a login operation's
//! disk proof.

use crate::consent::{
    upgrade_ceiling, Admitted, ApprovalKey, DiskProofs, DiskRequest, DiskRole, Fingerprint,
    LoginStep, Managed, Operation, Proof, Protected, Request, SelectorForm, UpgradeShape,
    UpgradeStep, PROTECTOR_CEILING, REQUEST_CEILING, SELECTOR_UPDATE_CEILING,
    UPGRADE_CANCEL_CEILING,
};
use crate::login::{End, Pin, CANCELLED, INTERNAL, TIMEOUT};
use crate::login_status::login_state;
use crate::unlock::{deadline_after, Event, Wire};
use std::fs::File;
use std::io::Read;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// Until td-install/ENCRYPTION.md's item 9 a production build answers
/// every storage operation `a0 00` before starting anything; host
/// fixtures drive them.
pub(crate) const OPERATIONS: bool = cfg!(test);

/// Margin against the worker's five-second acknowledgement window, for
/// its commit invitation, which the client acknowledges at once.
const ACK_TIME: Duration = Duration::from_secs(3);
/// How long a disk proof's worker has, once root closed its channel, to
/// kill and reap its keyslot test's cryptsetup child, which holds the
/// secret, before root kills the worker: it looks for the close every
/// 10 ms.
const GRACE: Duration = Duration::from_secs(1);
/// The secret frame: its tag and at most 63 bytes, a recovery key's 48.
const SECRET_FRAME: usize = 64;
/// A recovery key as `1c` must carry it: its 48 digits, unparsed.
const RECOVERY_DIGITS: usize = 48;

const PRESENT: u8 = 0x10;
const PRESENTED: u8 = 0x11;
const COMMIT: u8 = 0x12;
const COMMITTED: u8 = 0x13;
const SUCCESS: u8 = 0x14;
const FAILURE: u8 = 0x15;
const SECRET: u8 = 0x16;
const BASELINE: u8 = 0x18;

/// The storage worker's own kinds (td-install/DESIGN.md, "Storage
/// operation worker").
pub(crate) const VOLUME: u8 = 0x20;
pub(crate) const STATE: u8 = 0x21;
pub(crate) const NOT_CURRENT: u8 = 0x22;
const DEPLOYMENT: u8 = 0x23;
const ESP: u8 = 0x24;
/// Sent only after root's commit acknowledgement and before any write, so
/// definite once the worker exits successfully.
pub(crate) const CHANGED: u8 = 0x28;
/// A disk proof's: the recovery-key codec refused the entry, and the
/// keyslot refused the key or the derived passphrase.
pub(crate) const MISTYPED: u8 = 0x25;
pub(crate) const WRONG_KEY: u8 = 0x26;
/// The upgrade's: a new key already holding an excluded credential, and
/// the TPM shape's TPM.
const EXCLUDED: u8 = 0x11;
const TPM: u8 = 0x27;
const UNCERTAIN: u8 = 0x0e;
const FAILED: u8 = 0x0f;
/// Root's: an upgrade without an enrolled login state.
pub(crate) const UNENROLLED: u8 = 0x87;

/// The storage operation the paired peer selects; root derives the rest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Selection {
    SelectorUpdate,
    /// `02 S N`: phase 1 of the upgrade to shape `S` with `N` new keys.
    Upgrade {
        shape: UpgradeShape,
        count: u8,
    },
    /// `03`: the upgrade's cancellation.
    UpgradeCancel,
    /// `04` to `09`: protector management with its proof and an
    /// addition's role.
    Protector(Managed),
    /// `0a`: assigned, fixed by its own increment, and refused by this
    /// build.
    Later(u8),
}

impl Selection {
    /// The bytes after request `20`: one operation byte, and an upgrade's
    /// shape and count, which its shape must take.
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        match bytes {
            [1] => Ok(Self::SelectorUpdate),
            [2, shape, count] => match UpgradeShape::from_byte(*shape) {
                Some(shape) if shape.takes(*count) => Ok(Self::Upgrade {
                    shape,
                    count: *count,
                }),
                _ => Err("invalid upgrade selection".into()),
            },
            [3] => Ok(Self::UpgradeCancel),
            [4, proof, role] => match (Proof::from_byte(*proof), DiskRole::from_byte(*role)) {
                (Some(proof), Some(role)) => Ok(Self::Protector(Managed::Add { proof, role })),
                _ => Err("invalid protector addition selection".into()),
            },
            [5, proof] => Proof::from_byte(*proof)
                .map(|proof| Self::Protector(Managed::Remove { proof }))
                .ok_or_else(|| "invalid protector removal selection".into()),
            [6] => Ok(Self::Protector(Managed::RecoveryRemove)),
            [byte @ 7..=9, proof] => {
                match (
                    Proof::from_byte(*proof),
                    DiskRequest::from_byte(byte.saturating_sub(6)),
                ) {
                    (Some(proof), Some(request)) => {
                        Ok(Self::Protector(Managed::Request { proof, request }))
                    }
                    _ => Err("invalid protector request selection".into()),
                }
            }
            [0x0a] => Ok(Self::Later(0x0a)),
            _ => Err("invalid storage operation selection".into()),
        }
    }

    /// Whether this build runs the selection.
    pub fn runs(self) -> bool {
        !matches!(self, Self::Later(_))
    }

    /// Root's ceiling, fixed at the selection.
    fn ceiling(self) -> Duration {
        match self {
            Self::Upgrade { count, .. } => upgrade_ceiling(count),
            Self::UpgradeCancel => UPGRADE_CANCEL_CEILING,
            Self::Protector(Managed::Request { .. }) => REQUEST_CEILING,
            Self::Protector(_) => PROTECTOR_CEILING,
            Self::SelectorUpdate | Self::Later(_) => SELECTOR_UPDATE_CEILING,
        }
    }
}

/// The volume byte of the worker's baseline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Volume {
    None,
    DeviceBound,
    Upgrading,
    TpmPrimary,
    Fido2Primary,
    Refused,
}

/// The worker's baseline (td-install/DESIGN.md, "Storage operation
/// worker"): what root derives an operation's form and refusals from.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Baseline {
    volume: Volume,
    /// Whether a `recovery-key` marker names a keyslot.
    recovery: bool,
    /// The header's fido2 tokens' fingerprints, whatever their role.
    tokens: Vec<Fingerprint>,
    /// The running deployment's manifest ID when it is an acknowledged
    /// `current` with no pending attempt.
    current: Option<[u8; 32]>,
    /// A protected volume's, `03` or `04`, which protector management
    /// reads.
    protected: Option<Protected>,
}

/// `18`, the volume byte, the recovery-key byte, up to four fido2 tokens
/// with their roles, keyslot bytes and fingerprints, the tpm-pin count,
/// the lockout and request bytes, then `00` and the running deployment's
/// manifest ID, or `01`.
fn baseline(frame: &[u8]) -> Option<Baseline> {
    let [BASELINE, volume, recovery, count, rest @ ..] = frame else {
        return None;
    };
    let volume = match volume {
        0 => Volume::None,
        1 => Volume::DeviceBound,
        2 => Volume::Upgrading,
        3 => Volume::TpmPrimary,
        4 => Volume::Fido2Primary,
        5 => Volume::Refused,
        _ => return None,
    };
    if *recovery > 1 || *count > 4 {
        return None;
    }
    let (tokens, rest) = rest.split_at_checked(usize::from(*count) * 6)?;
    let (tokens, []) = tokens.as_chunks::<6>() else {
        return None;
    };
    let tokens = tokens
        .iter()
        .map(|[role, named, a, b, c, d]| {
            let named = match named {
                0 => false,
                1 => true,
                _ => return None,
            };
            Some((DiskRole::from_byte(*role)?, named, [*a, *b, *c, *d]))
        })
        .collect::<Option<Vec<_>>>()?;
    let [tpm_pins @ 0..=2, lockout @ (0 | 1), request @ 0..=3, standing @ ..] = rest else {
        return None;
    };
    let current = match standing {
        [0, id @ ..] => Some(id.try_into().ok()?),
        [1] => None,
        _ => return None,
    };
    let shape = match volume {
        Volume::TpmPrimary => Some(UpgradeShape::Tpm),
        Volume::Fido2Primary => Some(UpgradeShape::Fido2),
        _ => None,
    };
    Some(Baseline {
        volume,
        recovery: *recovery == 1,
        tokens: tokens.iter().map(|(.., key)| *key).collect(),
        current,
        protected: shape.map(|shape| Protected {
            shape,
            marker: *recovery == 1,
            tpm_pin: *tpm_pins > 0,
            lockout: *lockout == 1,
            request: DiskRequest::from_byte(*request),
            tokens,
        }),
    })
}

/// The worker's typed failure and its detail byte. A selector update's
/// carry none, and `0e` and `28` only once root acknowledged the commit,
/// as an upgrade's and a cancellation's. A disk proof's are the login
/// worker's kinds a disk key can end with, with their details, and the
/// proof's own; its `28` comes before any write, so it is definite
/// whenever it comes. An upgrade's and a cancellation's are where they
/// stand (`Stage`).
fn failure(
    frame: &[u8],
    committed: bool,
    proof: bool,
    selection: Selection,
    stage: Stage,
) -> Option<(u8, u8)> {
    match selection {
        Selection::Upgrade { shape, .. } => return upgrade_failure(frame, committed, shape, stage),
        Selection::UpgradeCancel => return cancel_failure(frame, committed, stage),
        Selection::Protector(managed) => {
            return protector_failure(frame, committed, managed, stage)
        }
        Selection::SelectorUpdate | Selection::Later(_) => {}
    }
    if proof {
        return match frame {
            [FAILURE, 0x01, retries] => Some((0x01, *retries)),
            [FAILURE, 0x06, detail @ 1..=6] => Some((0x06, *detail)),
            [FAILURE, kind @ (0x02..=0x05
            | 0x07
            | TIMEOUT
            | FAILED
            | INTERNAL
            | VOLUME
            | MISTYPED
            | WRONG_KEY
            | CHANGED)] => Some((*kind, 0)),
            _ => None,
        };
    }
    match frame {
        [FAILURE, kind @ (TIMEOUT | FAILED | INTERNAL | DEPLOYMENT | ESP)] => Some((*kind, 0)),
        [FAILURE, kind @ (UNCERTAIN | CHANGED)] if committed => Some((*kind, 0)),
        _ => None,
    }
}

/// Where an upgrade or a cancellation stands, which decides the kinds
/// its worker can end it with (td-compositor/DESIGN.md, "The upgrade").
#[derive(Clone, Copy, Debug)]
struct Stage {
    /// Root's description awaits the worker's first presentation: the
    /// worker's reads, whose `28` is definite since nothing is written.
    reading: bool,
    /// A new key's ceremony step is current, before the commit.
    keyed: bool,
    /// Protector management's: a presented step from the proof's first
    /// on is current, before the commit.
    proving: bool,
}

/// An upgrade's: its reads' kinds while it reads, the TPM's for the TPM
/// shape alone, a new key's from its ceremony, and after root's commit
/// acknowledgement `0e` and `28`; `08`, `0f` and `10` anywhere.
fn upgrade_failure(
    frame: &[u8],
    committed: bool,
    shape: UpgradeShape,
    stage: Stage,
) -> Option<(u8, u8)> {
    match frame {
        [FAILURE, kind @ (TIMEOUT | FAILED | INTERNAL)] => Some((*kind, 0)),
        [FAILURE, kind @ (UNCERTAIN | CHANGED)] if committed => Some((*kind, 0)),
        [FAILURE, kind @ (VOLUME | STATE | DEPLOYMENT | ESP | CHANGED)] if stage.reading => {
            Some((*kind, 0))
        }
        [FAILURE, TPM, detail @ 1..=3] if stage.reading && shape == UpgradeShape::Tpm => {
            Some((TPM, *detail))
        }
        [FAILURE, 0x01, retries] if stage.keyed => Some((0x01, *retries)),
        [FAILURE, 0x06, detail @ 1..=6] if stage.keyed => Some((0x06, *detail)),
        [FAILURE, kind @ (0x02 | 0x03 | 0x05 | 0x07 | EXCLUDED)] if stage.keyed => Some((*kind, 0)),
        _ => None,
    }
}

/// Protector management's, where a disk proof's stand: `20` before the
/// commit; the proof's kinds and a removal's chosen key's `04` detail `01`
/// from the proof's first presented step; `11` while a new key's step is
/// current; after root's commit acknowledgement `0e`; `08`, `0f`, `10`
/// and a definite `28` anywhere.
fn protector_failure(
    frame: &[u8],
    committed: bool,
    managed: Managed,
    stage: Stage,
) -> Option<(u8, u8)> {
    match frame {
        [FAILURE, kind @ (TIMEOUT | FAILED | INTERNAL | CHANGED)] => Some((*kind, 0)),
        [FAILURE, UNCERTAIN] if committed => Some((UNCERTAIN, 0)),
        _ if committed => None,
        [FAILURE, VOLUME] => Some((VOLUME, 0)),
        _ if !stage.proving => None,
        [FAILURE, 0x01, retries] => Some((0x01, *retries)),
        [FAILURE, 0x06, detail @ 1..=6] => Some((0x06, *detail)),
        [FAILURE, 0x04, 0x01] if matches!(managed, Managed::Remove { .. }) => Some((0x04, 1)),
        [FAILURE, kind @ (0x02..=0x05 | 0x07 | MISTYPED | WRONG_KEY)] => Some((*kind, 0)),
        [FAILURE, EXCLUDED] if stage.keyed => Some((EXCLUDED, 0)),
        _ => None,
    }
}

/// A cancellation's: its read's kinds while it reads, and after root's
/// commit acknowledgement `0e` and `28`; `08`, `0f` and `10` anywhere.
fn cancel_failure(frame: &[u8], committed: bool, stage: Stage) -> Option<(u8, u8)> {
    match frame {
        [FAILURE, kind @ (TIMEOUT | FAILED | INTERNAL)] => Some((*kind, 0)),
        [FAILURE, kind @ (UNCERTAIN | CHANGED)] if committed => Some((*kind, 0)),
        [FAILURE, kind @ (VOLUME | STATE | CHANGED)] if stage.reading => Some((*kind, 0)),
        _ => None,
    }
}

/// A disk proof's state: the login operation's first step as the login
/// record admits it, that record, and once the worker's baseline names
/// them, the proofs the volume offers.
struct DiskProof {
    first: Request,
    login: Vec<Fingerprint>,
    recovery: bool,
    tokens: Vec<Fingerprint>,
    /// The proof's last step's secret reached the worker.
    sent: bool,
}

impl DiskProof {
    fn disk(&self) -> DiskProofs<'_> {
        DiskProofs {
            recovery_key: self.recovery,
            tokens: &self.tokens,
        }
    }
}

/// A recovery key as the compositor's field admits it: exactly 48 ASCII
/// digits, which root forwards unparsed.
fn recovery_digits(pin: &Pin) -> bool {
    pin.bytes().len() == RECOVERY_DIGITS && pin.bytes().iter().all(u8::is_ascii_digit)
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(64), |mut text, byte| {
            // Writing to a String cannot fail.
            let _ = write!(text, "{byte:02x}");
            text
        })
}

/// The selector update's description from the baseline, or the storage
/// worker's kind that ends it before any description.
fn selector_update(baseline: &Baseline, key: ApprovalKey) -> Result<Operation, u8> {
    let form = match baseline.volume {
        Volume::None | Volume::Refused => return Err(VOLUME),
        Volume::Upgrading => return Err(STATE),
        Volume::DeviceBound => SelectorForm::DeviceBound,
        Volume::TpmPrimary => SelectorForm::Staged,
        Volume::Fido2Primary => SelectorForm::Fido2,
    };
    let current = baseline.current.ok_or(NOT_CURRENT)?;
    Ok(Operation::SelectorUpdate {
        key,
        form,
        current: hex(&current),
    })
}

/// Tag 17's review from the baseline, or the kind that ends the upgrade
/// before any description: it takes a device-bound volume only.
fn upgrade(
    baseline: &Baseline,
    owner: u32,
    shape: UpgradeShape,
    count: u8,
    key: ApprovalKey,
) -> Result<Operation, u8> {
    match baseline.volume {
        Volume::None | Volume::Refused => return Err(VOLUME),
        Volume::Upgrading | Volume::TpmPrimary | Volume::Fido2Primary => return Err(STATE),
        Volume::DeviceBound => {}
    }
    let current = baseline.current.ok_or(NOT_CURRENT)?;
    Ok(Operation::Upgrade {
        account: owner,
        shape,
        count,
        current: hex(&current),
        step: UpgradeStep::Review,
        approval: Some(key),
    })
}

/// Tag 18 from the baseline: it takes an upgrading volume only.
fn upgrade_cancel(baseline: &Baseline, key: ApprovalKey) -> Result<Operation, u8> {
    match baseline.volume {
        Volume::None | Volume::Refused => Err(VOLUME),
        Volume::DeviceBound | Volume::TpmPrimary | Volume::Fido2Primary => Err(STATE),
        Volume::Upgrading => Ok(Operation::UpgradeCancel { key }),
    }
}

/// Each digit `2` plus its own random byte modulo 8, as an elevation's.
fn approval_key(bytes: [u8; 2]) -> Result<ApprovalKey, String> {
    ApprovalKey::new(bytes.map(|byte| b'2' + byte % 8))
}

/// The fixed worker: root's `td-install` verb, with the launch the login
/// worker's has.
fn command(owner: u32) -> Command {
    let mut command = Command::new("/bin/td-install");
    command
        .args(["storage-operation", "--uid", &owner.to_string()])
        .env_clear()
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// Waiting for the worker's baseline.
    Baseline,
    /// The description is queued; waiting for its presentation.
    Description,
    /// The presentation awaits the compositor's receipt.
    Presented,
    /// Waiting for the commit invitation.
    Running,
    /// The commit invitation awaits its acknowledgement.
    Commit,
    /// Commit acknowledged; waiting for success.
    Completion,
    /// A disk proof's presented secret step awaits its secret.
    Pin,
    /// The secret is not yet written to the worker, which may send nothing
    /// but a failure until it is.
    Sending,
    /// Success received; waiting for the worker's exit.
    Exit,
    /// `28` received after the commit; waiting for the worker's exit.
    Changed,
    /// Killed; waiting to reap it.
    Stopping,
    Done,
}

pub(crate) struct Storage {
    owner: u32,
    nonce: [u8; 32],
    approval: ApprovalKey,
    selection: Selection,
    /// The baseline's fido2 tokens, an upgrade's steps are admitted
    /// against, and the credentials earlier new keys' ceremonies created,
    /// which a later key's may not name either.
    tokens: Vec<Fingerprint>,
    made: Vec<Fingerprint>,
    /// The credential the current key's ceremony created, from its prove
    /// step.
    created: Option<Fingerprint>,
    /// The presented step is the operation's last: only the commit follows.
    last: bool,
    child: Option<Child>,
    /// The worker's exit, reaped before its channel ended, and how long
    /// root waits for that end.
    status: Option<(ExitStatus, Instant)>,
    wire: Option<Wire>,
    phase: Phase,
    request: Option<Request>,
    round: Vec<u8>,
    deadline: Instant,
    acknowledgement_deadline: Option<Instant>,
    /// Root acknowledged the commit: a write may have begun.
    committed: bool,
    /// Set for a disk proof's worker.
    proof: Option<DiskProof>,
    /// Protector management's volume, from the baseline, and the
    /// position a removal's choose step named, which its next step must
    /// carry.
    protected: Option<Protected>,
    chosen: Option<u8>,
    /// A disk proof's secret on its way to the worker, zeroed when dropped.
    pin: Option<Pin>,
    /// When a stopped worker holding a header copy, a disk proof's or
    /// protector management's, is killed if it has not exited.
    grace: Option<Instant>,
    end: Option<End>,
    terminal: Option<Event>,
}

impl Storage {
    /// Call only after root startup and paired-session admission, and only
    /// in a build whose `OPERATIONS` is set.
    pub fn start(owner: u32, selection: Selection) -> Result<Self, String> {
        if owner != 1000 {
            return Err("storage supervisor requires the configured graphical session".into());
        }
        if !OPERATIONS || !selection.runs() {
            return Err("storage operations are refused in this build".into());
        }
        // An upgrade makes login-key consent the volume's (amendment 10).
        let enrolled = !matches!(selection, Selection::Upgrade { .. })
            || login_state::state(Path::new("/"), owner) == login_state::State::Enrolled;
        Self::launch(owner, selection, enrolled, command(owner))
    }

    /// `start` with its login-state read and its worker the caller's: an
    /// upgrade without an enrolled login state ends as `87` before any
    /// worker starts.
    fn launch(
        owner: u32,
        selection: Selection,
        enrolled: bool,
        command: Command,
    ) -> Result<Self, String> {
        let mut nonce = [0; 32];
        let mut key = [0; 2];
        File::open("/dev/urandom")
            .and_then(|mut random| {
                random.read_exact(&mut nonce)?;
                random.read_exact(&mut key)
            })
            .map_err(|_| "read storage request randomness")?;
        if nonce == [0; 32] {
            return Err("zero storage request nonce".into());
        }
        let deadline = Instant::now()
            .checked_add(selection.ceiling())
            .ok_or("storage deadline overflow")?;
        if !enrolled {
            return Ok(Self::unstarted(
                owner,
                nonce,
                approval_key(key)?,
                selection,
                deadline,
                UNENROLLED,
            ));
        }
        Self::spawn(
            owner,
            nonce,
            approval_key(key)?,
            selection,
            deadline,
            None,
            command,
        )
    }

    /// A login operation's disk proof (amendment 10): `first`, its first
    /// step as `login`, the login record, admits it, under the login
    /// operation's own nonce and `deadline`. Called only by
    /// `proof::Proving`, once the login worker's baseline is read.
    pub fn start_proof(
        owner: u32,
        nonce: [u8; 32],
        first: &Request,
        login: &[Fingerprint],
        deadline: Instant,
    ) -> Result<Self, String> {
        if owner != 1000 {
            return Err("storage supervisor requires the configured graphical session".into());
        }
        if !OPERATIONS {
            return Err("disk proofs are refused in this build".into());
        }
        Self::spawn_proof(owner, nonce, first, login, deadline, command(owner))
    }

    pub(crate) fn spawn_proof(
        owner: u32,
        nonce: [u8; 32],
        first: &Request,
        login: &[Fingerprint],
        deadline: Instant,
        command: Command,
    ) -> Result<Self, String> {
        if first.login_proof().is_none() || first.nonce() != &nonce {
            return Err("a disk proof begins at its proof's first step".into());
        }
        // The disclosure's key is the login operation's, on `first`.
        let approval = ApprovalKey::new(*b"22")?;
        let proof = DiskProof {
            first: first.clone(),
            login: login.to_vec(),
            recovery: false,
            tokens: Vec::new(),
            sent: false,
        };
        Self::spawn(
            owner,
            nonce,
            approval,
            Selection::SelectorUpdate,
            deadline,
            Some(proof),
            command,
        )
    }

    /// The deadline counts from before the worker starts, so it ends no
    /// later than the worker's, which counts from its own start.
    fn spawn(
        owner: u32,
        nonce: [u8; 32],
        approval: ApprovalKey,
        selection: Selection,
        deadline: Instant,
        proof: Option<DiskProof>,
        mut command: Command,
    ) -> Result<Self, String> {
        let (parent, child) = UnixStream::pair().map_err(|e| e.to_string())?;
        let wire = Wire::new(parent)?;
        let child = command
            .stdin(Stdio::from(OwnedFd::from(child)))
            .spawn()
            .map_err(|e| format!("start storage worker: {e}"))?;
        let mut storage = Self::unstarted(owner, nonce, approval, selection, deadline, INTERNAL);
        storage.child = Some(child);
        storage.wire = Some(wire);
        storage.phase = Phase::Baseline;
        storage.end = None;
        storage.terminal = None;
        storage.proof = proof;
        Ok(storage)
    }

    /// An operation that ends as `kind` with no worker started.
    fn unstarted(
        owner: u32,
        nonce: [u8; 32],
        approval: ApprovalKey,
        selection: Selection,
        deadline: Instant,
        kind: u8,
    ) -> Self {
        let end = End {
            uncertain: false,
            kind,
            detail: 0,
        };
        Self {
            owner,
            nonce,
            approval,
            selection,
            tokens: Vec::new(),
            made: Vec::new(),
            created: None,
            last: true,
            child: None,
            status: None,
            wire: None,
            phase: Phase::Done,
            request: None,
            round: Vec::new(),
            deadline,
            acknowledgement_deadline: None,
            committed: false,
            proof: None,
            protected: None,
            chosen: None,
            pin: None,
            grace: None,
            end: Some(end),
            terminal: Some(Event::Ended(end)),
        }
    }

    pub fn nonce(&self) -> &[u8; 32] {
        &self.nonce
    }

    /// None until the worker's baseline lets root derive the description.
    pub fn request(&self) -> Option<&Request> {
        self.request.as_ref()
    }

    /// The receipt follows the approval key, which the person may type at
    /// any time before root's deadline: one at or after it ends the
    /// operation as TIMEOUT, and one for an operation already ended is
    /// dropped, as a login disclosure's is.
    pub fn presented(&mut self, request: &Request) -> Result<(), String> {
        let now = Instant::now();
        // A step carries a key where it discloses: a proof's first, an
        // upgrade's review, tag 21's identify step, and the selector
        // update's and the cancellation's description.
        let keyed = request.login_approval().is_some()
            || request.upgrade_approval().is_some()
            || request.protector_approval().is_some()
            || request.selector_update().is_some();
        if keyed && self.request.as_ref() == Some(request) {
            if matches!(self.phase, Phase::Stopping | Phase::Done) {
                return Ok(());
            }
            if self.phase == Phase::Presented && now >= self.deadline {
                return self.stop(TIMEOUT);
            }
        }
        self.acknowledge(request, Phase::Presented, PRESENTED, now)
    }

    pub fn commit(&mut self, request: &Request) -> Result<(), String> {
        self.acknowledge(request, Phase::Commit, COMMITTED, Instant::now())
    }

    fn acknowledge(
        &mut self,
        request: &Request,
        expected: Phase,
        tag: u8,
        now: Instant,
    ) -> Result<(), String> {
        if self.phase != expected
            || self.request.as_ref() != Some(request)
            || now >= self.deadline
            || self
                .acknowledgement_deadline
                .is_none_or(|deadline| now >= deadline)
        {
            self.stop(INTERNAL)?;
            return Err("stale storage acknowledgement".into());
        }

        let first = self.round.first_mut().ok_or("missing storage round")?;
        *first = tag;
        self.wire
            .as_mut()
            .ok_or("missing storage endpoint")?
            .queue(&self.round)?;
        self.round.clear();
        self.acknowledgement_deadline = None;
        if tag == COMMITTED {
            // From here a write may begin: anything but success, or a
            // definite `28`, is uncertain.
            self.committed = true;
            self.phase = Phase::Completion;
        } else if request.ceremony_step().is_some_and(LoginStep::asks_pin) {
            self.phase = Phase::Pin;
        } else {
            self.phase = Phase::Running;
        }
        Ok(())
    }

    /// Forwards the secret for a presented secret step, keeping no copy
    /// once it is written: a disk key's or a new key's PIN, the recovery
    /// key, exactly 48 digits, or a choose step's one digit naming a
    /// removable token, which the compositor's fields alone admit, so
    /// anything else ends the generation. False when the operation has
    /// already ended, or its deadline has passed, which ends it: the
    /// secret is dropped.
    pub fn pin(&mut self, request: &Request, pin: Pin) -> Result<bool, String> {
        if matches!(self.phase, Phase::Stopping | Phase::Done) && request.nonce() == &self.nonce {
            return Ok(false);
        }
        if self.phase != Phase::Pin || self.request.as_ref() != Some(request) {
            self.stop(INTERNAL)?;
            return Err("stale storage secret".into());
        }
        if request.asks_recovery_key() && !recovery_digits(&pin) {
            self.stop(INTERNAL)?;
            return Err("recovery key outside its profile".into());
        }
        let chosen = match (request.protector_step(), request.removal()) {
            (Some(LoginStep::Choose), Some(removal)) => match removal.entry(pin.bytes()) {
                Some(position) => Some(position),
                None => {
                    self.stop(INTERNAL)?;
                    return Err("choose entry outside its profile".into());
                }
            },
            _ if !request.asks_recovery_key() && !pin.is_pin() => {
                self.stop(INTERNAL)?;
                return Err("login PIN is outside its profile".into());
            }
            _ => None,
        };
        if Instant::now() >= self.deadline {
            drop(pin);
            self.stop(TIMEOUT)?;
            return Ok(false);
        }
        self.chosen = chosen;
        self.pin = Some(pin);
        self.phase = Phase::Sending;
        Ok(true)
    }

    /// Kills the worker.
    pub fn cancel(&mut self) -> Result<(), String> {
        self.stop(CANCELLED)
    }

    pub fn poll(&mut self) -> Result<Event, String> {
        match self.phase {
            Phase::Done => {
                return self
                    .terminal
                    .clone()
                    .ok_or_else(|| "missing storage terminal result".into())
            }
            Phase::Stopping => return self.reap(),
            _ => {}
        }
        let now = Instant::now();
        let late = now >= self.deadline;
        if !late
            && self
                .acknowledgement_deadline
                .is_some_and(|deadline| now >= deadline)
        {
            self.stop(TIMEOUT)?;
            return self.reap();
        }
        // As a login operation's: one last look past the deadline, which
        // only a completed success or definite end survives.
        if late {
            self.pin = None;
            if let Some(wire) = self.wire.as_mut() {
                wire.discard();
            }
        }
        let event = match self.advance() {
            Ok(event) => event,
            Err(_) => {
                self.stop(if late { TIMEOUT } else { INTERNAL })?;
                return self.reap();
            }
        };
        if late && !matches!(event, Event::Complete | Event::Ended(_)) {
            self.stop(TIMEOUT)?;
            return self.reap();
        }
        Ok(event)
    }

    /// After `14` or a `28`: the worker's successful exit and a clean end
    /// of its channel, with no further byte, end the operation as
    /// `outcome`; any other exit leaves it uncertain. The end may follow
    /// the exit: a process forked meanwhile holds a copy of the worker's
    /// side until it execs, so root waits for it for `GRACE`.
    fn exited(&mut self, outcome: Event) -> Result<Event, String> {
        if self.status.is_none() {
            let status = self
                .child
                .as_mut()
                .ok_or("missing storage worker")?
                .try_wait()
                .map_err(|e| format!("wait for storage worker: {e}"))?;
            if let Some(status) = status {
                self.child = None;
                let until = Instant::now()
                    .checked_add(GRACE)
                    .unwrap_or_else(Instant::now);
                self.status = Some((status, until));
            }
        }
        let ended = self
            .wire
            .as_mut()
            .ok_or("missing storage endpoint")?
            .finished()?;
        let Some((status, until)) = self.status else {
            return Ok(Event::Waiting);
        };
        if status.success() && !ended && Instant::now() < until {
            return Ok(Event::Waiting);
        }
        self.wire = None;
        if status.success() && ended {
            return Ok(self.finish(outcome));
        }
        self.end = Some(self.ended(INTERNAL, 0));
        self.conclude()
    }

    fn advance(&mut self) -> Result<Event, String> {
        match self.phase {
            Phase::Exit => return self.exited(Event::Complete),
            Phase::Changed => {
                return self.exited(Event::Ended(End {
                    uncertain: false,
                    kind: CHANGED,
                    detail: 0,
                }))
            }
            _ => {}
        }
        // A secret queued before this poll is written before it reads.
        let queued = self.phase == Phase::Sending && self.pin.is_none();
        let wire = self.wire.as_mut().ok_or("missing storage endpoint")?;
        let frame = wire.poll()?;
        if queued && wire.idle() {
            self.phase = Phase::Running;
            if let Some(proof) = &mut self.proof {
                proof.sent = true;
            }
        }
        if let Some(frame) = frame {
            return self.receive(frame);
        }
        if wire.idle() {
            if let Some(pin) = self.pin.take() {
                let mut bytes = Vec::with_capacity(SECRET_FRAME);
                bytes.push(SECRET);
                bytes.extend_from_slice(pin.bytes());
                let queued = wire.queue_bounded(&bytes, SECRET_FRAME);
                bytes.fill(0);
                std::hint::black_box(&mut bytes);
                queued?;
            }
        }
        Ok(self.event())
    }

    fn receive(&mut self, frame: Vec<u8>) -> Result<Event, String> {
        let stage = Stage {
            reading: self.phase == Phase::Description,
            keyed: !self.committed
                && self.request.as_ref().is_some_and(|request| {
                    matches!(request.upgrade_step(), Some(UpgradeStep::Key { .. }))
                        || matches!(
                            request.protector_step(),
                            Some(
                                LoginStep::Connect
                                    | LoginStep::Create { .. }
                                    | LoginStep::Prove { .. }
                                    | LoginStep::Repeat { .. }
                                    | LoginStep::Probe { .. }
                            )
                        )
                }),
            proving: !self.committed
                && !matches!(self.phase, Phase::Baseline | Phase::Description)
                && self
                    .request
                    .as_ref()
                    .and_then(Request::protector_step)
                    .is_some_and(|step| step != LoginStep::Choose),
        };
        if let Some((kind, detail)) = failure(
            &frame,
            self.committed,
            self.proof.is_some(),
            self.selection,
            stage,
        ) {
            if kind == CHANGED {
                self.phase = Phase::Changed;
                return self.exited(Event::Ended(End {
                    uncertain: false,
                    kind: CHANGED,
                    detail: 0,
                }));
            }
            self.stop_with(kind, detail)?;
            return self.reap();
        }
        let sent = self.proof.as_ref().map(|proof| proof.sent);
        match (self.phase, frame.first()) {
            (Phase::Baseline, Some(&BASELINE)) => self.begin(&frame),
            (Phase::Description, Some(&PRESENT)) | (Phase::Running, Some(&COMMIT)) => {
                self.invited(frame)
            }
            // A disk proof's next step, before its secret is sent, and an
            // upgrade's or protector management's next step before its
            // last.
            (Phase::Running, Some(&PRESENT)) if sent == Some(false) => self.invited(frame),
            (Phase::Running, Some(&PRESENT))
                if matches!(
                    self.selection,
                    Selection::Upgrade { .. } | Selection::Protector(_)
                ) && !self.last =>
            {
                self.invited(frame)
            }
            // A disk proof's success follows its secret; no commit round.
            (Phase::Running, Some(&SUCCESS)) if sent == Some(true) && frame.len() == 1 => {
                self.phase = Phase::Exit;
                self.exited(Event::Complete)
            }
            (Phase::Completion, Some(&SUCCESS)) if frame.len() == 1 => {
                self.phase = Phase::Exit;
                self.exited(Event::Complete)
            }
            _ => Err("storage worker frame out of order".into()),
        }
    }

    /// The description from the baseline, or the kind that ends the
    /// operation before any description reaches the worker.
    fn begin(&mut self, frame: &[u8]) -> Result<Event, String> {
        let baseline = baseline(frame).ok_or("malformed storage baseline")?;
        if self.proof.is_some() {
            return self.begin_proof(baseline);
        }
        let operation = match self.selection {
            Selection::Protector(managed) => return self.begin_protector(baseline, managed),
            Selection::Upgrade { shape, count } => {
                upgrade(&baseline, self.owner, shape, count, self.approval)
            }
            Selection::UpgradeCancel => upgrade_cancel(&baseline, self.approval),
            Selection::SelectorUpdate | Selection::Later(_) => {
                selector_update(&baseline, self.approval)
            }
        };
        let operation = match operation {
            Ok(operation) => operation,
            Err(kind) => {
                self.stop(kind)?;
                return self.reap();
            }
        };
        let request = Request::new(self.nonce, self.owner, operation)?;
        if request.upgrade_step().is_some() {
            self.last = request.upgrade_start()? == Admitted::Last;
        }
        self.tokens = baseline.tokens;
        self.wire
            .as_mut()
            .ok_or("missing storage endpoint")?
            .queue(&request.encode())?;
        self.request = Some(request);
        self.phase = Phase::Description;
        Ok(Event::Waiting)
    }

    /// Protector management's first description, from the baseline: `20`
    /// for no encrypted td volume or a header td's reader refuses, `21`
    /// for any other volume than a protected one, or `21` and its detail
    /// where the volume cannot take the operation (`Protected::first`),
    /// each before the worker sees any description.
    fn begin_protector(&mut self, baseline: Baseline, managed: Managed) -> Result<Event, String> {
        let first = match (baseline.volume, &baseline.protected) {
            (Volume::None | Volume::Refused, _) => Err((VOLUME, 0)),
            (_, None) => Err((STATE, 0)),
            (_, Some(protected)) => protected
                .first(self.owner, managed, Some(self.approval))
                .map_err(|detail| (STATE, detail)),
        };
        let (operation, protected) = match (first, baseline.protected) {
            (Ok(operation), Some(protected)) => (operation, protected),
            (Err((kind, detail)), _) => {
                self.stop_with(kind, detail)?;
                return self.reap();
            }
            (Ok(_), None) => return Err("missing protected volume".into()),
        };
        let request = Request::new(self.nonce, self.owner, operation)?;
        self.last = protected.admits(&request)? == Admitted::Last;
        self.protected = Some(protected);
        self.wire
            .as_mut()
            .ok_or("missing storage endpoint")?
            .queue(&request.encode())?;
        self.request = Some(request);
        self.phase = Phase::Description;
        Ok(Event::Waiting)
    }

    /// A disk proof's first description, from both baselines: `20` for
    /// no encrypted td volume or a header td's reader refuses, `21` for an
    /// upgrading volume or one that does not offer the chosen proof (no
    /// `recovery-key` marker, or no fido2 token), each before the worker
    /// sees any description.
    fn begin_proof(&mut self, baseline: Baseline) -> Result<Event, String> {
        let proof = self.proof.as_mut().ok_or("missing disk proof")?;
        let refused = match baseline.volume {
            Volume::None | Volume::Refused => Some(VOLUME),
            Volume::Upgrading => Some(STATE),
            Volume::DeviceBound | Volume::TpmPrimary | Volume::Fido2Primary => None,
        };
        proof.recovery = baseline.recovery;
        proof.tokens = baseline.tokens;
        let request = match refused {
            Some(kind) => Err(kind),
            None => Request::begin_proof(
                self.nonce,
                self.owner,
                proof.first.operation().clone(),
                &proof.login,
                proof.disk(),
            )
            .map_err(|_| STATE),
        };
        let request = match request {
            Ok(request) if request == proof.first => request,
            Ok(_) => return Err("disk proof changed its first step".into()),
            Err(kind) => {
                self.stop(kind)?;
                return self.reap();
            }
        };
        self.wire
            .as_mut()
            .ok_or("missing storage endpoint")?
            .queue(&request.encode())?;
        self.request = Some(request);
        self.phase = Phase::Description;
        Ok(Event::Waiting)
    }

    /// The presentation and the commit invitation, each exactly root's
    /// description: the selector update and the cancellation have one
    /// step. An upgrade's later steps are its key ceremonies', each
    /// admitted against the baseline's fido2 tokens and the credential the
    /// ceremony created, and its commit names its last step. A disk
    /// proof's first presentation is root's description, and its next, a
    /// disk key's authorize step, is admitted against both baselines; it
    /// has no commit. Protector management's later steps are admitted
    /// against the baseline's protectors, a removal's chosen position and
    /// an addition's created credential, and its commit names its last
    /// step.
    fn invited(&mut self, frame: Vec<u8>) -> Result<Event, String> {
        let current = self.request.as_ref().ok_or("missing storage request")?;
        let invitation = Request::decode(frame.get(33..).ok_or("short storage invitation")?)?;
        if let Some(proof) = &self.proof {
            if frame.first() == Some(&COMMIT) {
                return Err("a disk proof has no commit".into());
            }
            if self.phase == Phase::Running {
                current.admit_proof_step(&invitation, &proof.login, proof.disk())?;
            } else if &invitation != current {
                return Err("storage worker changed root's description".into());
            }
            // Only root's first step can disclose.
            self.acknowledgement_deadline = Some(if invitation.login_approval().is_some() {
                self.deadline
            } else {
                deadline_after(ACK_TIME)?
            });
            self.request = Some(invitation);
            self.phase = Phase::Presented;
            self.round = frame;
            return Ok(self.event());
        }
        if let (Some(protected), Phase::Running, Some(&PRESENT)) =
            (&self.protected, self.phase, frame.first())
        {
            let keys = protected.protector_keys();
            let disk = DiskProofs {
                recovery_key: protected.marker,
                tokens: &keys,
            };
            let created = match invitation.protector_step() {
                Some(LoginStep::Prove { key, .. }) => Some(key),
                Some(LoginStep::Repeat { .. } | LoginStep::Probe { .. }) => self.created,
                _ => None,
            };
            let admitted =
                current.admit_protector_step(&invitation, disk, created, self.chosen.take())?;
            self.created = created;
            self.last = admitted == Admitted::Last;
            self.request = Some(invitation);
            self.phase = Phase::Presented;
            self.acknowledgement_deadline = Some(deadline_after(ACK_TIME)?);
            self.round = frame;
            return Ok(self.event());
        }
        if self.phase == Phase::Running && frame.first() == Some(&PRESENT) {
            let step = invitation.upgrade_step().and_then(UpgradeStep::login);
            // The next key's connect: the last key's credential joins the
            // ones a new key may not name.
            if step == Some(LoginStep::Connect) {
                self.made.extend(self.created);
            }
            let created = match step {
                Some(LoginStep::Prove { key, .. }) => Some(key),
                Some(LoginStep::Repeat { .. } | LoginStep::Probe { .. }) => self.created,
                _ => None,
            };
            let excluded: Vec<Fingerprint> =
                self.tokens.iter().chain(&self.made).copied().collect();
            let admitted = current.admit_upgrade_step(&invitation, &excluded, created)?;
            self.created = created;
            self.last = admitted == Admitted::Last;
            self.request = Some(invitation);
            self.phase = Phase::Presented;
            self.acknowledgement_deadline = Some(deadline_after(ACK_TIME)?);
            self.round = frame;
            return Ok(self.event());
        }
        if &invitation != current || (frame.first() == Some(&COMMIT) && !self.last) {
            return Err("storage worker changed root's description".into());
        }
        self.acknowledgement_deadline = Some(if self.phase == Phase::Description {
            self.phase = Phase::Presented;
            // The approval key is typed against the operation's deadline;
            // protector management's first step carries one only at tag 21.
            if self.protected.is_some() && invitation.protector_approval().is_none() {
                deadline_after(ACK_TIME)?
            } else {
                self.deadline
            }
        } else {
            self.phase = Phase::Commit;
            deadline_after(ACK_TIME)?
        });
        self.round = frame;
        Ok(self.event())
    }

    fn event(&self) -> Event {
        match (self.phase, &self.request) {
            (Phase::Presented, Some(request)) => Event::Present(request.clone()),
            (Phase::Pin, Some(request)) => Event::Pin(request.clone()),
            (Phase::Commit, Some(request)) => Event::Commit(request.clone()),
            _ => Event::Waiting,
        }
    }

    fn ended(&self, kind: u8, detail: u8) -> End {
        End {
            uncertain: self.committed,
            kind,
            detail,
        }
    }

    fn stop(&mut self, kind: u8) -> Result<(), String> {
        self.stop_with(kind, 0)
    }

    /// Records how the operation ended and stops the worker: a selector
    /// update's is killed at once; a disk proof's or protector
    /// management's channel is closed, which its keyslot test reads as
    /// the operation's end and kills and reaps its cryptsetup child, and
    /// the worker is killed only if it has not exited by the grace's end.
    fn stop_with(&mut self, kind: u8, detail: u8) -> Result<(), String> {
        if matches!(self.phase, Phase::Stopping | Phase::Done) {
            return Ok(());
        }
        self.end = Some(self.ended(kind, detail));
        self.wire = None;
        self.round.clear();
        self.pin = None;
        self.acknowledgement_deadline = None;
        if self.graced() {
            self.grace = Some(
                Instant::now()
                    .checked_add(GRACE)
                    .unwrap_or_else(Instant::now),
            );
        } else if let Some(child) = &mut self.child {
            child
                .kill()
                .map_err(|e| format!("stop storage worker: {e}"))?;
        }
        self.phase = Phase::Stopping;
        Ok(())
    }

    /// Whether the worker may hold a header copy for a keyslot test: a
    /// disk proof's, or protector management's.
    fn graced(&self) -> bool {
        self.proof.is_some() || matches!(self.selection, Selection::Protector(_))
    }

    fn reap(&mut self) -> Result<Event, String> {
        if let Some(child) = &mut self.child {
            if child
                .try_wait()
                .map_err(|e| format!("reap storage worker: {e}"))?
                .is_none()
            {
                if self.grace.is_some_and(|grace| Instant::now() >= grace) {
                    child
                        .kill()
                        .map_err(|e| format!("stop storage worker: {e}"))?;
                    self.grace = None;
                }
                return Ok(Event::Waiting);
            }
            self.child = None;
        }
        self.conclude()
    }

    fn conclude(&mut self) -> Result<Event, String> {
        let end = self.end.ok_or("missing storage end")?;
        Ok(self.finish(Event::Ended(end)))
    }

    fn finish(&mut self, event: Event) -> Event {
        self.phase = Phase::Done;
        self.terminal = Some(event.clone());
        event
    }

    /// After peer failure only; prove this worker cannot write again.
    pub fn reap_for_teardown(mut self) -> Result<(), String> {
        self.wire = None;
        self.pin = None;
        let graced_worker = self.graced();
        if let Some(child) = &mut self.child {
            if graced_worker && graced(child) {
                self.child = None;
                return Ok(());
            }
            let stopped = child.kill();
            if let Err(error) = child.wait() {
                return Err(format!(
                    "reap storage generation worker: {error}; kill result: {stopped:?}"
                ));
            }
            self.child = None;
        }
        Ok(())
    }
}

/// With its channel already closed, waits out a graced worker's grace for
/// `child` to exit: true once it has been reaped.
///
/// This blocks the authority thread for at most GRACE, polling every 10 ms.
/// Only teardown after a peer failure and dropping a live graced worker
/// call it. Neither can defer the reap: teardown must prove the worker
/// cannot write again before the generation ends, and Drop has no later
/// poll. Both are rare, so a bounded one-second block is acceptable there.
/// An ordinary stop instead counts the grace down across `poll`s.
fn graced(child: &mut Child) -> bool {
    let until = Instant::now().checked_add(GRACE);
    loop {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return true;
        }
        if until.is_none_or(|until| Instant::now() >= until) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

impl Drop for Storage {
    fn drop(&mut self) {
        self.wire = None;
        let graced_worker = self.graced();
        if let Some(child) = &mut self.child {
            if graced_worker && graced(child) {
                return;
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
impl Storage {
    /// A scripted worker: `storage::tests::scripted_worker` playing `script`.
    pub(crate) fn scripted(script: &str) -> Result<Self, String> {
        let deadline = Instant::now()
            .checked_add(SELECTOR_UPDATE_CEILING)
            .ok_or("storage deadline overflow")?;
        Self::spawn(
            1000,
            [42; 32],
            tests::approval(),
            Selection::SelectorUpdate,
            deadline,
            None,
            tests::fixture(script),
        )
    }

    /// A scripted worker for `selection`, as `begin` starts one.
    pub(crate) fn scripted_as(
        selection: Selection,
        enrolled: bool,
        script: &str,
    ) -> Result<Self, String> {
        let mut storage = Self::launch(1000, selection, enrolled, tests::fixture(script))?;
        storage.nonce = [42; 32];
        storage.approval = tests::approval();
        Ok(storage)
    }

    pub(crate) fn fixture_pid(&self) -> Option<u32> {
        self.child.as_ref().map(Child::id)
    }

    /// Ends root's operation deadline now.
    pub(crate) fn expire(&mut self) {
        self.deadline = Instant::now();
    }
}

#[cfg(test)]
#[path = "../tests/storage.rs"]
pub(crate) mod tests;

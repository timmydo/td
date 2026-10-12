//! Nonblocking ownership of one root storage worker, `td-install
//! storage-operation` (td-authd/DESIGN.md amendment 10): root's side of
//! its frames for a selector update or a login operation's disk proof.

use crate::consent::{
    ApprovalKey, DiskProofs, Fingerprint, LoginStep, Operation, Request, SelectorForm,
    SELECTOR_UPDATE_CEILING,
};
use crate::login::{End, Pin, CANCELLED, INTERNAL, TIMEOUT};
use crate::unlock::{deadline_after, Event, Wire};
use std::fs::File;
use std::io::Read;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
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
const UNCERTAIN: u8 = 0x0e;
const FAILED: u8 = 0x0f;

/// The storage operation the paired peer selects; root derives the rest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Selection {
    SelectorUpdate,
    /// Bytes `02` to `0a`: assigned, fixed by their own increments, and
    /// refused by this build.
    Later(u8),
}

impl Selection {
    /// The bytes after request `20`: one operation byte.
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        match bytes {
            [1] => Ok(Self::SelectorUpdate),
            [byte @ 2..=0x0a] => Ok(Self::Later(*byte)),
            _ => Err("invalid storage operation selection".into()),
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
}

/// `18`, the volume byte, the recovery-key byte, up to four fido2 tokens
/// with their roles and fingerprints, then `00` and the running
/// deployment's manifest ID, or `01`.
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
    let (tokens, standing) = rest.split_at_checked(usize::from(*count) * 5)?;
    let (tokens, []) = tokens.as_chunks::<5>() else {
        return None;
    };
    if !tokens.iter().all(|[role, ..]| matches!(role, 1 | 2)) {
        return None;
    }
    let current = match standing {
        [0, id @ ..] => Some(id.try_into().ok()?),
        [1] => None,
        _ => return None,
    };
    Some(Baseline {
        volume,
        recovery: *recovery == 1,
        tokens: tokens
            .iter()
            .map(|[_, a, b, c, d]| [*a, *b, *c, *d])
            .collect(),
        current,
    })
}

/// The worker's typed failure and its detail byte. A selector update's
/// carry none, and `0e` and `28` only once root acknowledged the commit. A
/// disk proof's are the login worker's kinds a disk key can end with,
/// with their details, and the proof's own; its `28` comes before any
/// write, so it is definite whenever it comes.
fn failure(frame: &[u8], committed: bool, proof: bool) -> Option<(u8, u8)> {
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
    child: Option<Child>,
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
    /// A disk proof's secret on its way to the worker, zeroed when dropped.
    pin: Option<Pin>,
    /// When a stopped proof's worker is killed if it has not exited.
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
        if !OPERATIONS || selection != Selection::SelectorUpdate {
            return Err("storage operations are refused in this build".into());
        }
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
            .checked_add(SELECTOR_UPDATE_CEILING)
            .ok_or("storage deadline overflow")?;
        Self::spawn(
            owner,
            nonce,
            approval_key(key)?,
            deadline,
            None,
            command(owner),
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
        Self::spawn(owner, nonce, approval, deadline, Some(proof), command)
    }

    /// The deadline counts from before the worker starts, so it ends no
    /// later than the worker's, which counts from its own start.
    fn spawn(
        owner: u32,
        nonce: [u8; 32],
        approval: ApprovalKey,
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
        Ok(Self {
            owner,
            nonce,
            approval,
            child: Some(child),
            wire: Some(wire),
            phase: Phase::Baseline,
            request: None,
            round: Vec::new(),
            deadline,
            acknowledgement_deadline: None,
            committed: false,
            proof,
            pin: None,
            grace: None,
            end: None,
            terminal: None,
        })
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
        // A proof's step carries a key only where it discloses.
        let keyed = self.proof.is_none() || request.login_approval().is_some();
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
        } else if self.proof.is_some() && request.login_step().is_some_and(LoginStep::asks_pin) {
            self.phase = Phase::Pin;
        } else {
            self.phase = Phase::Running;
        }
        Ok(())
    }

    /// Forwards a disk proof's secret for its presented secret step,
    /// keeping no copy once it is written: a disk key's PIN, or the
    /// recovery key, exactly 48 digits, which the compositor's field alone
    /// admits, so anything else ends the generation. False when the
    /// operation has already ended, or its deadline has passed, which ends
    /// it: the secret is dropped.
    pub fn pin(&mut self, request: &Request, pin: Pin) -> Result<bool, String> {
        if matches!(self.phase, Phase::Stopping | Phase::Done) && request.nonce() == &self.nonce {
            return Ok(false);
        }
        if self.proof.is_none()
            || self.phase != Phase::Pin
            || self.request.as_ref() != Some(request)
        {
            self.stop(INTERNAL)?;
            return Err("stale disk proof secret".into());
        }
        if request.login_step() == Some(LoginStep::RecoveryKey) && !recovery_digits(&pin) {
            self.stop(INTERNAL)?;
            return Err("recovery key outside its profile".into());
        }
        if Instant::now() >= self.deadline {
            drop(pin);
            self.stop(TIMEOUT)?;
            return Ok(false);
        }
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

    /// After `14` or a committed `28`: the worker's successful exit and a
    /// clean end of its channel, with no further byte, end the operation
    /// as `outcome`; any other exit leaves it uncertain.
    fn exited(&mut self, outcome: Event) -> Result<Event, String> {
        let status = self
            .child
            .as_mut()
            .ok_or("missing storage worker")?
            .try_wait()
            .map_err(|e| format!("wait for storage worker: {e}"))?;
        if status.is_some() {
            self.child = None;
        }
        let ended = self
            .wire
            .as_mut()
            .ok_or("missing storage endpoint")?
            .finished()?;
        let Some(status) = status else {
            return Ok(Event::Waiting);
        };
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
        if let Some((kind, detail)) = failure(&frame, self.committed, self.proof.is_some()) {
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
            // A disk proof's next step, before its secret is sent.
            (Phase::Running, Some(&PRESENT)) if sent == Some(false) => self.invited(frame),
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
        let operation = match selector_update(&baseline, self.approval) {
            Ok(operation) => operation,
            Err(kind) => {
                self.stop(kind)?;
                return self.reap();
            }
        };
        let request = Request::new(self.nonce, self.owner, operation)?;
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
    /// description: the selector update has one step. A disk proof's first
    /// presentation is root's description, and its next, a disk key's
    /// authorize step, is admitted against both baselines; it has no
    /// commit.
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
        if &invitation != current {
            return Err("storage worker changed root's description".into());
        }
        self.acknowledgement_deadline = Some(if self.phase == Phase::Description {
            // The approval key is typed against the operation's deadline.
            self.phase = Phase::Presented;
            self.deadline
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
    /// update's is killed at once; a disk proof's channel is closed, which
    /// its keyslot test reads as the operation's end and kills and reaps
    /// its cryptsetup child, and the worker is killed only if it has not
    /// exited by the grace's end.
    fn stop_with(&mut self, kind: u8, detail: u8) -> Result<(), String> {
        if matches!(self.phase, Phase::Stopping | Phase::Done) {
            return Ok(());
        }
        self.end = Some(self.ended(kind, detail));
        self.wire = None;
        self.round.clear();
        self.pin = None;
        self.acknowledgement_deadline = None;
        if self.proof.is_some() {
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
        if let Some(child) = &mut self.child {
            if self.proof.is_some() && graced(child) {
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

/// With its channel already closed, waits out a disk proof's grace for
/// `child` to exit: true once it has been reaped.
///
/// This blocks the authority thread for at most GRACE, polling every 10 ms.
/// Only teardown after a peer failure and dropping a live proof's worker
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
        if let Some(child) = &mut self.child {
            if self.proof.is_some() && graced(child) {
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
            deadline,
            None,
            tests::fixture(script),
        )
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

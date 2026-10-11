//! Nonblocking ownership of one root storage worker, `td-install
//! storage-operation` (td-authd/DESIGN.md amendment 10): root's side of
//! its frames for a selector update.

use crate::consent::{ApprovalKey, Operation, Request, SelectorForm, SELECTOR_UPDATE_CEILING};
use crate::login::{End, CANCELLED, INTERNAL, TIMEOUT};
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

const PRESENT: u8 = 0x10;
const PRESENTED: u8 = 0x11;
const COMMIT: u8 = 0x12;
const COMMITTED: u8 = 0x13;
const SUCCESS: u8 = 0x14;
const FAILURE: u8 = 0x15;
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
    Some(Baseline { volume, current })
}

/// The worker's typed failure: a kind with no detail byte. `0e` and `28`
/// are admitted only once root acknowledged the commit.
fn failure(frame: &[u8], committed: bool) -> Option<u8> {
    match frame {
        [FAILURE, kind @ (TIMEOUT | FAILED | INTERNAL | DEPLOYMENT | ESP)] => Some(*kind),
        [FAILURE, kind @ (UNCERTAIN | CHANGED)] if committed => Some(*kind),
        _ => None,
    }
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
        Self::spawn(owner, nonce, approval_key(key)?, command(owner))
    }

    /// The deadline counts from before the worker starts, so it ends no
    /// later than the worker's, which counts from its own start.
    fn spawn(
        owner: u32,
        nonce: [u8; 32],
        approval: ApprovalKey,
        mut command: Command,
    ) -> Result<Self, String> {
        let deadline = Instant::now()
            .checked_add(SELECTOR_UPDATE_CEILING)
            .ok_or("storage deadline overflow")?;
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
        if self.request.as_ref() == Some(request) {
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
        } else {
            self.phase = Phase::Running;
        }
        Ok(())
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
        self.end = Some(self.ended(INTERNAL));
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
        let wire = self.wire.as_mut().ok_or("missing storage endpoint")?;
        match wire.poll()? {
            Some(frame) => self.receive(frame),
            None => Ok(self.event()),
        }
    }

    fn receive(&mut self, frame: Vec<u8>) -> Result<Event, String> {
        if let Some(kind) = failure(&frame, self.committed) {
            if kind == CHANGED {
                self.phase = Phase::Changed;
                return self.exited(Event::Ended(End {
                    uncertain: false,
                    kind: CHANGED,
                    detail: 0,
                }));
            }
            self.stop(kind)?;
            return self.reap();
        }
        match (self.phase, frame.first()) {
            (Phase::Baseline, Some(&BASELINE)) => self.begin(&frame),
            (Phase::Description, Some(&PRESENT)) | (Phase::Running, Some(&COMMIT)) => {
                self.invited(frame)
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

    /// The presentation and the commit invitation, each exactly root's
    /// description: the selector update has one step.
    fn invited(&mut self, frame: Vec<u8>) -> Result<Event, String> {
        let current = self.request.as_ref().ok_or("missing storage request")?;
        let invitation = Request::decode(frame.get(33..).ok_or("short storage invitation")?)?;
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
            (Phase::Commit, Some(request)) => Event::Commit(request.clone()),
            _ => Event::Waiting,
        }
    }

    fn ended(&self, kind: u8) -> End {
        End {
            uncertain: self.committed,
            kind,
            detail: 0,
        }
    }

    fn stop(&mut self, kind: u8) -> Result<(), String> {
        if matches!(self.phase, Phase::Stopping | Phase::Done) {
            return Ok(());
        }
        self.end = Some(self.ended(kind));
        self.wire = None;
        self.round.clear();
        self.acknowledgement_deadline = None;
        if let Some(child) = &mut self.child {
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
        if let Some(child) = &mut self.child {
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

impl Drop for Storage {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
impl Storage {
    /// A scripted worker: `storage::tests::scripted_worker` playing `script`.
    pub(crate) fn scripted(script: &str) -> Result<Self, String> {
        Self::spawn(1000, [42; 32], tests::approval(), tests::fixture(script))
    }

    /// Ends root's operation deadline now.
    pub(crate) fn expire(&mut self) {
        self.deadline = Instant::now();
    }
}

#[cfg(test)]
#[path = "../tests/storage.rs"]
pub(crate) mod tests;

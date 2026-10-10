//! Nonblocking ownership of one root login-key worker, `td-secret
//! login-operation` (td-login/TOKEN-LOGIN.md): root's side of its frames.

use crate::consent::{
    Admitted, ApprovalKey, Fingerprint, LoginStep, Operation, Request, Slot, LOGIN_CEREMONY,
    LOGIN_KEYS, LOGIN_LONGEST, LOGIN_READING, LOGIN_TWO_CEREMONIES,
};
use crate::unlock::{command, deadline_after, Event, Wire};
use std::fs::File;
use std::io::Read;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Until activation (TOKEN-LOGIN.md increment 5) a production build
/// refuses enrollment, addition and removal before any worker starts; host
/// fixtures drive them.
pub(crate) const WRITES: bool = cfg!(test);

/// Margin against the worker's five-second acknowledgement window. A
/// disclosure step's has the operation deadline instead: the person reads
/// it and types its approval key, with no token I/O open.
const ACK_TIME: Duration = Duration::from_secs(3);
/// The worker's PIN frame: its tag and at most 63 PIN bytes.
const PIN_FRAME: usize = 64;

const PRESENT: u8 = 0x10;
const PRESENTED: u8 = 0x11;
const COMMIT: u8 = 0x12;
const COMMITTED: u8 = 0x13;
const SUCCESS: u8 = 0x14;
const FAILURE: u8 = 0x15;
const PIN: u8 = 0x16;
const BASELINE: u8 = 0x18;

/// Failure kinds root reports in the worker's own numbering
/// (td-secret/DESIGN.md, "Login-key worker").
pub(crate) const TIMEOUT: u8 = 0x08;
pub(crate) const NO_RECORD: u8 = 0x09;
/// The worker broke its protocol, its channel was lost or it failed its exit.
pub(crate) const INTERNAL: u8 = 0x10;
/// The worker's report that its write was attempted.
const UNCERTAIN: u8 = 0x0e;
/// Root's own kinds, apart from the worker's.
pub(crate) const CANCELLED: u8 = 0x80;
/// An addition to a record that already holds eight keys.
pub(crate) const FULL: u8 = 0x81;
/// A first enrollment over an enrolled record.
pub(crate) const ENROLLED: u8 = 0x82;
/// A removal whose slots are not the record's.
pub(crate) const SELECTION: u8 = 0x83;

/// The login operation the paired peer selects; root derives the rest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Selection {
    Unlock,
    /// First enrollment of one or two keys.
    Enroll(u8),
    Add,
    /// Positions in the record's canonical order, with the fingerprints the
    /// key-management screen showed for them.
    Remove(Vec<Slot>),
}

impl Selection {
    /// The bytes after request `1b`: consent's operation tag, then a first
    /// enrollment's key count or a removal's slots.
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        match bytes {
            [7] => Ok(Self::Unlock),
            [8, keys @ (1 | 2)] => Ok(Self::Enroll(*keys)),
            [9] => Ok(Self::Add),
            [10, count, rest @ ..]
                if (1..=LOGIN_KEYS).contains(count) && rest.len() == usize::from(*count) * 5 =>
            {
                let (slots, []) = rest.as_chunks::<5>() else {
                    return Err("invalid login removal slots".into());
                };
                let slots: Vec<Slot> = slots
                    .iter()
                    .map(|[position, a, b, c, d]| Slot {
                        position: *position,
                        key: [*a, *b, *c, *d],
                    })
                    .collect();
                let ordered = slots
                    .windows(2)
                    .all(|pair| matches!(pair, [low, high] if low.position < high.position));
                if !ordered
                    || !slots
                        .iter()
                        .all(|slot| (1..=LOGIN_KEYS).contains(&slot.position))
                {
                    return Err("invalid login removal slots".into());
                }
                Ok(Self::Remove(slots))
            }
            _ => Err("invalid login operation selection".into()),
        }
    }

    /// Whether the operation may change the record.
    pub fn writes(&self) -> bool {
        !matches!(self, Self::Unlock)
    }

    /// Root's bound until its first step names the operation, from which
    /// the description's ceiling, the worker's own, narrows it: a removal
    /// may leave at most one key and so disclose.
    fn ceiling(&self) -> Duration {
        match self {
            Self::Unlock => LOGIN_CEREMONY,
            Self::Enroll(1) | Self::Remove(_) => LOGIN_CEREMONY.saturating_add(LOGIN_READING),
            Self::Add => LOGIN_TWO_CEREMONIES,
            Self::Enroll(_) => LOGIN_LONGEST,
        }
    }

    /// The operation at its first step against the worker's baseline, with
    /// `key` when that step carries a disclosure, or the kind that refuses
    /// it before any description reaches the worker.
    fn operation(
        &self,
        owner: u32,
        baseline: &[Fingerprint],
        key: ApprovalKey,
    ) -> Result<Operation, u8> {
        let count = u8::try_from(baseline.len()).map_err(|_| INTERNAL)?;
        let operation = match self {
            Self::Enroll(_) if count != 0 => return Err(ENROLLED),
            Self::Enroll(after) => Operation::LoginEnroll {
                account: owner,
                before: 0,
                after: *after,
                key: 1,
                step: LoginStep::Connect,
                approval: None,
            },
            _ if count == 0 => return Err(NO_RECORD),
            Self::Unlock => Operation::LoginUnlock {
                account: owner,
                before: count,
                after: count,
                step: LoginStep::Identify,
            },
            // Consent has no encoding for a ninth key.
            Self::Add if count >= LOGIN_KEYS => return Err(FULL),
            Self::Add => Operation::LoginAdd {
                account: owner,
                before: count,
                after: count.checked_add(1).ok_or(INTERNAL)?,
                step: LoginStep::Identify,
            },
            Self::Remove(removed) => Operation::LoginRemove {
                account: owner,
                before: count,
                after: usize::from(count)
                    .checked_sub(removed.len())
                    .and_then(|after| u8::try_from(after).ok())
                    .ok_or(SELECTION)?,
                removed: removed.clone(),
                step: LoginStep::Identify,
                approval: None,
            },
        };
        // A first enrollment's first step, and a removal's that leaves at
        // most one key, carries a disclosure; consent says which.
        Ok(operation.disclosed(key))
    }
}

/// Each digit `2` plus its own random byte modulo 8, as an elevation's: 256
/// is a multiple of 8, so every digit is equally likely.
fn approval_key(bytes: [u8; 2]) -> Result<ApprovalKey, String> {
    ApprovalKey::new(bytes.map(|byte| b'2' + byte % 8))
}

/// A PIN on its way to the worker, zeroed when dropped.
#[derive(PartialEq, Eq)]
pub(crate) struct Pin(Vec<u8>);

impl Pin {
    /// The worker's profile: 4 to 63 printable ASCII bytes.
    pub fn new(bytes: &[u8]) -> Result<Self, String> {
        if !(4..=63).contains(&bytes.len()) || !bytes.iter().all(|b| (0x20..=0x7e).contains(b)) {
            return Err("login PIN is outside its profile".into());
        }
        Ok(Self(bytes.to_vec()))
    }
}

impl Drop for Pin {
    fn drop(&mut self) {
        self.0.fill(0);
        std::hint::black_box(&mut self.0);
    }
}

impl std::fmt::Debug for Pin {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Pin(..)")
    }
}

/// How a login operation ended without success: a kind and its detail byte
/// (zero when the kind has none). Uncertain once root acknowledged a
/// write's commit round: the write may have begun, and only a fresh read of
/// the login state tells what it left.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct End {
    pub uncertain: bool,
    pub kind: u8,
    pub detail: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// Waiting for the worker's baseline.
    Baseline,
    /// Root's first step is queued; waiting for its invitation.
    Description,
    /// An invitation awaits its presentation acknowledgement.
    Presented,
    /// A presented PIN step awaits its PIN.
    Pin,
    /// The PIN is not yet written to the worker, which may send nothing
    /// but a failure until it is.
    Sending,
    /// Waiting for the worker's next frame.
    Running,
    /// The commit invitation awaits its acknowledgement.
    Commit,
    /// Commit acknowledged; waiting for success.
    Completion,
    /// Success received; waiting for the worker's exit.
    Exit,
    /// Killed; waiting to reap it.
    Stopping,
    Done,
}

pub(crate) struct Login {
    owner: u32,
    nonce: [u8; 32],
    /// The key a disclosure step carries, drawn beside the nonce.
    approval: ApprovalKey,
    selection: Selection,
    child: Option<Child>,
    wire: Option<Wire>,
    phase: Phase,
    /// The worker's slot fingerprints in canonical order.
    baseline: Vec<Fingerprint>,
    /// The current step's description, from root's first step on.
    request: Option<Request>,
    /// The credential the current key's ceremony created, from its prove step.
    created: Option<Fingerprint>,
    /// Whether the current step is the operation's final one.
    last: bool,
    round: Vec<u8>,
    pin: Option<Pin>,
    /// Before the worker started, so root's deadline ends no later than the
    /// worker's, which counts from its own start.
    started: Instant,
    deadline: Instant,
    acknowledgement_deadline: Option<Instant>,
    /// Root queued a write's commit acknowledgement.
    committed: bool,
    end: Option<End>,
    terminal: Option<Event>,
}

/// `18 00` unenrolled, or `18 01`, the slot count and each fingerprint.
fn baseline(frame: &[u8]) -> Option<Vec<Fingerprint>> {
    match frame {
        [BASELINE, 0] => Some(Vec::new()),
        [BASELINE, 1, count, keys @ ..]
            if (1..=LOGIN_KEYS).contains(count) && keys.len() == usize::from(*count) * 4 =>
        {
            Some(keys.as_chunks::<4>().0.to_vec())
        }
        _ => None,
    }
}

/// The worker's typed failure: a known kind, with a detail byte exactly for
/// WRONG PIN, KEY REFUSED and UNCERTAIN.
fn failure(frame: &[u8]) -> Option<(u8, u8)> {
    match frame {
        [FAILURE, 0x01, retries] => Some((0x01, *retries)),
        [FAILURE, 0x06, detail @ 1..=6] => Some((0x06, *detail)),
        [FAILURE, 0x0e, detail @ 1..=5] => Some((0x0e, *detail)),
        [FAILURE, kind @ (0x02..=0x05 | 0x07..=0x0d | 0x0f..=0x12)] => Some((*kind, 0)),
        _ => None,
    }
}

impl Login {
    /// Call only after root startup and paired-session admission.
    pub fn start(owner: u32, selection: Selection) -> Result<Self, String> {
        if owner != 1000 {
            return Err("login supervisor requires the configured graphical session".into());
        }
        if selection.writes() && !WRITES {
            return Err("login writes are refused until activation".into());
        }
        let mut nonce = [0; 32];
        let mut key = [0; 2];
        File::open("/dev/urandom")
            .and_then(|mut random| {
                random.read_exact(&mut nonce)?;
                random.read_exact(&mut key)
            })
            .map_err(|_| "read login request randomness")?;
        if nonce == [0; 32] {
            return Err("zero login request nonce".into());
        }
        Self::spawn(
            owner,
            nonce,
            approval_key(key)?,
            selection,
            command("login-operation", owner),
        )
    }

    /// The deadline counts from before the worker starts, so it ends no
    /// later than the worker's, whose commit margin leaves the write room.
    fn spawn(
        owner: u32,
        nonce: [u8; 32],
        approval: ApprovalKey,
        selection: Selection,
        mut command: Command,
    ) -> Result<Self, String> {
        let started = Instant::now();
        let deadline = started
            .checked_add(selection.ceiling())
            .ok_or("login deadline overflow")?;
        let (parent, child) = UnixStream::pair().map_err(|e| e.to_string())?;
        let wire = Wire::new(parent)?;
        let child = command
            .stdin(Stdio::from(OwnedFd::from(child)))
            .spawn()
            .map_err(|e| format!("start login worker: {e}"))?;
        Ok(Self {
            owner,
            nonce,
            approval,
            selection,
            child: Some(child),
            wire: Some(wire),
            phase: Phase::Baseline,
            baseline: Vec::new(),
            request: None,
            created: None,
            last: false,
            round: Vec::new(),
            pin: None,
            started,
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

    /// None until the worker's baseline lets root derive its first step.
    pub fn request(&self) -> Option<&Request> {
        self.request.as_ref()
    }

    /// A disclosure step's receipt follows its approval key, which the
    /// person may type at any time before root's deadline: one at or after
    /// it ends the operation as TIMEOUT, and one for an operation already
    /// ended is dropped, each as a late PIN is, so the generation goes on.
    pub fn presented(&mut self, request: &Request) -> Result<(), String> {
        self.presented_at(request, Instant::now())
    }

    /// `presented` with the clock read once, at `now`, for both the
    /// disclosure's deadline and the acknowledgement's: a receipt that
    /// crosses the deadline between two reads would otherwise be stale.
    fn presented_at(&mut self, request: &Request, now: Instant) -> Result<(), String> {
        if request.login_approval().is_some() && self.request.as_ref() == Some(request) {
            if matches!(self.phase, Phase::Stopping | Phase::Done) {
                return Ok(());
            }
            if self.phase == Phase::Presented && now >= self.deadline {
                return self.stop(TIMEOUT, 0);
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
            self.stop(INTERNAL, 0)?;
            return Err("stale login acknowledgement".into());
        }
        let first = self.round.first_mut().ok_or("missing login round")?;
        *first = tag;
        self.wire
            .as_mut()
            .ok_or("missing login endpoint")?
            .queue(&self.round)?;
        self.round.clear();
        self.acknowledgement_deadline = None;
        self.phase = if tag == COMMITTED {
            // From here a write may begin: anything but success is uncertain.
            self.committed = self.selection.writes();
            Phase::Completion
        } else if request.login_step().is_some_and(LoginStep::asks_pin) {
            Phase::Pin
        } else {
            Phase::Running
        };
        Ok(())
    }

    /// Forwards one PIN for the presented PIN step `request`, keeping no
    /// copy once it is written. False when the operation has already ended,
    /// or its deadline has passed, which ends it: the PIN is dropped.
    pub fn pin(&mut self, request: &Request, pin: Pin) -> Result<bool, String> {
        if matches!(self.phase, Phase::Stopping | Phase::Done) && request.nonce() == &self.nonce {
            return Ok(false);
        }
        if self.phase != Phase::Pin || self.request.as_ref() != Some(request) {
            self.stop(INTERNAL, 0)?;
            return Err("stale login PIN".into());
        }
        // The person may submit just after root's last poll.
        if Instant::now() >= self.deadline {
            drop(pin);
            self.stop(TIMEOUT, 0)?;
            return Ok(false);
        }
        self.pin = Some(pin);
        self.phase = Phase::Sending;
        Ok(true)
    }

    /// Kills the worker; nothing relocks, since no login operation releases
    /// anything.
    pub fn cancel(&mut self) -> Result<(), String> {
        self.stop(CANCELLED, 0)
    }

    pub fn poll(&mut self) -> Result<Event, String> {
        match self.phase {
            Phase::Done => {
                return self
                    .terminal
                    .clone()
                    .ok_or_else(|| "missing login terminal result".into())
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
            self.stop(TIMEOUT, 0)?;
            return self.reap();
        }
        // A poll that finds the deadline passed makes one last look: it
        // reads what is already in the socket, without blocking, and checks
        // once for the exit. Only a success that look completes counts, and
        // nothing still queued, a PIN included, reaches the worker.
        if late {
            self.pin = None;
            if let Some(wire) = self.wire.as_mut() {
                wire.discard();
            }
        }
        let event = match self.advance() {
            Ok(event) => event,
            Err(_) => {
                self.stop(if late { TIMEOUT } else { INTERNAL }, 0)?;
                return self.reap();
            }
        };
        if late && !matches!(event, Event::Complete | Event::Ended(_)) {
            self.stop(TIMEOUT, 0)?;
            return self.reap();
        }
        Ok(event)
    }

    /// After `14`: the worker's successful exit and a clean end of its
    /// channel, with no further byte, complete the operation.
    fn exited(&mut self) -> Result<Event, String> {
        let status = self
            .child
            .as_mut()
            .ok_or("missing login worker")?
            .try_wait()
            .map_err(|e| format!("wait for login worker: {e}"))?;
        if status.is_some() {
            self.child = None;
        }
        // Read after the wait: once the worker has exited, what remains is
        // all it wrote.
        let ended = self
            .wire
            .as_mut()
            .ok_or("missing login endpoint")?
            .finished()?;
        let Some(status) = status else {
            return Ok(Event::Waiting);
        };
        self.wire = None;
        if status.success() && ended {
            return Ok(self.finish(Event::Complete));
        }
        self.end = Some(self.ended(INTERNAL, 0));
        self.conclude()
    }

    fn advance(&mut self) -> Result<Event, String> {
        if self.phase == Phase::Exit {
            return self.exited();
        }
        // A PIN queued before this poll is written before it reads.
        let queued = self.phase == Phase::Sending && self.pin.is_none();
        let wire = self.wire.as_mut().ok_or("missing login endpoint")?;
        let frame = wire.poll()?;
        if queued && wire.idle() {
            self.phase = Phase::Running;
        }
        if let Some(frame) = frame {
            return self.receive(frame);
        }
        if wire.idle() {
            if let Some(pin) = self.pin.take() {
                let mut bytes = Vec::with_capacity(PIN_FRAME);
                bytes.push(PIN);
                bytes.extend_from_slice(&pin.0);
                let queued = wire.queue_bounded(&bytes, PIN_FRAME);
                bytes.fill(0);
                queued?;
            }
        }
        Ok(self.event())
    }

    fn receive(&mut self, frame: Vec<u8>) -> Result<Event, String> {
        if let Some((kind, detail)) = failure(&frame) {
            // A write's outcome is uncertain only once root let it begin.
            if kind == UNCERTAIN && !self.committed {
                return Err("login worker reported a write it could not begin".into());
            }
            self.stop(kind, detail)?;
            return self.reap();
        }
        match (self.phase, frame.as_slice()) {
            (Phase::Baseline, [BASELINE, ..]) => self.begin(&frame),
            (Phase::Description | Phase::Running, [PRESENT, ..])
            | (Phase::Running, [COMMIT, ..]) => self.invited(frame),
            (Phase::Completion, [SUCCESS]) => {
                self.phase = Phase::Exit;
                self.exited()
            }
            _ => Err("login worker frame out of order".into()),
        }
    }

    /// Root's first step from the baseline, or a refusal before the worker
    /// sees any description and so before any token I/O.
    fn begin(&mut self, frame: &[u8]) -> Result<Event, String> {
        let baseline = baseline(frame).ok_or("malformed login baseline")?;
        let refused = match self
            .selection
            .operation(self.owner, &baseline, self.approval)
        {
            Err(kind) => Err(kind),
            Ok(operation) => Request::begin_login(self.nonce, self.owner, operation, &baseline)
                .map_err(|_| match self.selection {
                    Selection::Remove(_) => SELECTION,
                    _ => INTERNAL,
                }),
        };
        let request = match refused {
            Ok(request) => request,
            Err(kind) => {
                self.stop(kind, 0)?;
                return self.reap();
            }
        };
        // The description's ceiling, the worker's, now bounds the operation.
        let ceiling = request.login_ceiling().ok_or("missing login ceiling")?;
        self.deadline = self.deadline.min(
            self.started
                .checked_add(ceiling)
                .ok_or("login deadline overflow")?,
        );
        self.wire
            .as_mut()
            .ok_or("missing login endpoint")?
            .queue(&request.encode())?;
        self.baseline = baseline;
        self.request = Some(request);
        self.phase = Phase::Description;
        Ok(Event::Waiting)
    }

    /// The worker's next presentation or commit invitation, admitted only
    /// as root derives it.
    fn invited(&mut self, frame: Vec<u8>) -> Result<Event, String> {
        let current = self.request.as_ref().ok_or("missing login request")?;
        let invitation = Request::decode(frame.get(33..).ok_or("short login invitation")?)?;
        match (frame.first(), self.phase) {
            (Some(&COMMIT), _) => {
                if !self.last || &invitation != current {
                    return Err("login commit before the final step".into());
                }
                self.phase = Phase::Commit;
            }
            (_, Phase::Description) => {
                if &invitation != current {
                    return Err("login worker changed root's first step".into());
                }
                self.phase = Phase::Presented;
            }
            _ => {
                let created = match invitation.login_step() {
                    Some(LoginStep::Prove { key, .. }) => Some(key),
                    Some(LoginStep::Repeat { .. } | LoginStep::Probe { .. }) => self.created,
                    _ => None,
                };
                let admitted = current.admit_login_step(&invitation, &self.baseline, created)?;
                self.created = created;
                self.last = admitted == Admitted::Last;
                self.request = Some(invitation);
                self.phase = Phase::Presented;
            }
        }
        // Only root's own first step can disclose.
        self.acknowledgement_deadline = Some(match &self.request {
            Some(request) if request.login_approval().is_some() => self.deadline,
            _ => deadline_after(ACK_TIME)?,
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

    /// Records how the operation ended and kills the worker; `reap` then
    /// reports it once the worker is reaped.
    fn stop(&mut self, kind: u8, detail: u8) -> Result<(), String> {
        if matches!(self.phase, Phase::Stopping | Phase::Done) {
            return Ok(());
        }
        self.end = Some(self.ended(kind, detail));
        self.wire = None;
        self.round.clear();
        self.pin = None;
        self.acknowledgement_deadline = None;
        if let Some(child) = &mut self.child {
            child
                .kill()
                .map_err(|e| format!("stop login worker: {e}"))?;
        }
        self.phase = Phase::Stopping;
        Ok(())
    }

    fn reap(&mut self) -> Result<Event, String> {
        if let Some(child) = &mut self.child {
            if child
                .try_wait()
                .map_err(|e| format!("reap login worker: {e}"))?
                .is_none()
            {
                return Ok(Event::Waiting);
            }
            self.child = None;
        }
        self.conclude()
    }

    fn conclude(&mut self) -> Result<Event, String> {
        let end = self.end.ok_or("missing login end")?;
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
            let stopped = child.kill();
            if let Err(error) = child.wait() {
                return Err(format!(
                    "reap login generation worker: {error}; kill result: {stopped:?}"
                ));
            }
            self.child = None;
        }
        Ok(())
    }
}

impl Drop for Login {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
impl Login {
    /// A scripted worker: `login::tests::scripted_worker` playing `script`.
    pub(crate) fn scripted(selection: Selection, script: &str) -> Result<Self, String> {
        Self::spawn(
            1000,
            [42; 32],
            tests::approval(),
            selection,
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
#[path = "../tests/login.rs"]
pub(crate) mod tests;

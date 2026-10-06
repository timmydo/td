//! Private session protocol and a physical attention lifetime.

use crate::attention::Notice;
use crate::authority::consent::{
    Admitted, Enrollment, Fingerprint, LoginStep, Operation, Platform, Recovery, Request, Role,
    Slot, LOGIN_CEREMONY, LOGIN_KEYS, LOGIN_TWO_CEREMONIES,
};
use crate::authority::Exchange;
use crate::input::EvdevOrigin;
use crate::runtime::Runtime;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const ACTIVE: u8 = 0;
const CANCELLED: u8 = 1;
const COMMITTED: u8 = 2;

/// A store operation's or an installation review's attention lifetime.
const SECRET_LIFETIME: Duration = Duration::from_secs(120);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Selection {
    Unlock(Role),
    Enroll(Recovery),
    Write,
    Install,
    Login(LoginSelection),
}
impl From<Role> for Selection {
    fn from(role: Role) -> Self {
        Self::Unlock(role)
    }
}
impl Selection {
    fn operation(&self) -> Option<Operation> {
        Some(match self {
            Self::Write | Self::Install | Self::Login(_) => return None,
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
            Self::Write => vec![0x18],
            Self::Install => vec![0x19],
            Self::Unlock(Role::Primary) => vec![0x12, 1],
            Self::Unlock(Role::Recovery) => vec![0x12, 2],
            Self::Enroll(Recovery::Unrecoverable) => vec![0x16, 0],
            Self::Enroll(Recovery::SecondToken) => vec![0x16, 1],
            Self::Login(login) => return login.request(),
        })
    }
    /// The attention lifetime, fixed at the selection and never renewed.
    fn lifetime(&self) -> Duration {
        match self {
            Self::Login(login) => login.ceiling(),
            _ => SECRET_LIFETIME,
        }
    }
}

/// A login-key operation (td-login/TOKEN-LOGIN.md): root's request `1b`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum LoginSelection {
    Unlock,
    /// First enrollment of one or two keys.
    Enroll(u8),
    Add,
    /// The slots to remove, positions strictly increasing.
    Remove(Vec<Slot>),
}

impl LoginSelection {
    /// The bytes td-authd's `login::Selection::decode` reads, after `1b`.
    fn request(&self) -> Result<Vec<u8>, String> {
        Ok(match self {
            Self::Unlock => vec![0x1b, 7],
            Self::Enroll(keys @ (1 | 2)) => vec![0x1b, 8, *keys],
            Self::Enroll(_) => return Err("a first enrollment has one or two keys".into()),
            Self::Add => vec![0x1b, 9],
            Self::Remove(slots) => {
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

    /// td-authd's ceiling: one ceremony per key the person handles in turn.
    fn ceiling(&self) -> Duration {
        match self {
            Self::Unlock | Self::Remove(_) | Self::Enroll(1) => LOGIN_CEREMONY,
            Self::Enroll(_) | Self::Add => LOGIN_TWO_CEREMONIES,
        }
    }

    /// Whether the operation may change the record.
    fn writes(&self) -> bool {
        *self != Self::Unlock
    }

    /// Whether `request` describes this operation for the configured human.
    fn selects(&self, request: &Request) -> bool {
        request.owner() == 1000
            && request.login_ceiling() == Some(self.ceiling())
            && match (self, request.operation()) {
                (Self::Unlock, Operation::LoginUnlock { account, .. })
                | (Self::Add, Operation::LoginAdd { account, .. }) => *account == 1000,
                (Self::Enroll(keys), Operation::LoginEnroll { account, after, .. }) => {
                    *account == 1000 && after == keys
                }
                (
                    Self::Remove(slots),
                    Operation::LoginRemove {
                        account, removed, ..
                    },
                ) => *account == 1000 && removed == slots,
                _ => false,
            }
    }

    fn success(&self) -> &'static [&'static str] {
        match self {
            Self::Unlock => &["SESSION UNLOCKED"],
            Self::Enroll(_) => &["LOGIN KEYS ENROLLED"],
            Self::Add => &["LOGIN KEY ADDED"],
            Self::Remove(_) => &["LOGIN KEYS REMOVED"],
        }
    }
}

/// The key-management screen's removal digits over the enrolled keys in
/// canonical slot order. Nothing supplies that list before request `1a`
/// (td-login/TOKEN-LOGIN.md increment 4), so until then nothing builds one.
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
        Some(LoginSelection::Remove(slots))
    }
}

/// TOKEN-LOGIN.md's text for root's terminal kind and detail, in rows the
/// narrowest output's chrome holds whole, or none for a pair root never
/// sends.
/// Cancellation adds nothing: the person chose it, or this client did and
/// already said why.
fn login_failure(kind: u8, detail: u8) -> Option<&'static [&'static str]> {
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
        _ => return None,
    })
}

pub(crate) struct Attempt {
    origin: EvdevOrigin,
    runtime: Arc<Mutex<Runtime>>,
    selection: Selection,
    state: AtomicU8,
    deadline: Option<Instant>,
    presentation: Mutex<Option<(Request, u128)>>,
    confirmed: AtomicBool,
}

impl Attempt {
    pub fn new(
        origin: EvdevOrigin,
        runtime: Arc<Mutex<Runtime>>,
        selection: impl Into<Selection>,
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
        })
    }

    /// Input records cancellation before waiting for painting or channel I/O.
    pub fn cancel(&self) {
        self.state.fetch_or(CANCELLED, Ordering::SeqCst);
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
        if !self.active()
            || (self.selection == Selection::Install && !self.confirmed.load(Ordering::SeqCst))
        {
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
        if self.selection == Selection::Install {
            *self
                .presentation
                .lock()
                .map_err(|_| "installation receipt lock poisoned")? =
                Some((request.clone(), completed));
        }
        Ok(request)
    }

    /// Only the physical evdev adapter can offer a confirmation key.
    pub fn confirm_install(&self, _origin: &EvdevOrigin, timestamp: u128) -> Result<(), String> {
        if self.selection != Selection::Install || !self.active() {
            return Ok(());
        }
        let runtime = self.runtime.lock().map_err(|_| "runtime lock poisoned")?;
        let presentation = self
            .presentation
            .lock()
            .map_err(|_| "installation receipt lock poisoned")?;
        if let Some((request, completed)) = &*presentation {
            if timestamp > *completed && self.active() && runtime.attention_request_visible(request)
            {
                self.confirmed.store(true, Ordering::SeqCst);
            }
        }
        Ok(())
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
    /// The current step is the operation's final one: only its commit
    /// follows.
    last: bool,
    /// An addition's authorizing key, which its new key's steps never name.
    authorizer: Option<Fingerprint>,
    committed: bool,
    cancelled: bool,
}

#[derive(Default)]
pub(crate) struct Client {
    pending: Option<Pending>,
    login: Option<LoginPending>,
    inspection: Option<Inspection>,
}

impl Client {
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
        let response = wire.exchange(&selection.request()?)?;
        let nonce = match response.as_slice() {
            [0x9b, 0] => return attempt.notice(Notice::NotAvailable),
            [0x9b, 1, nonce @ ..] => {
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
            last: false,
            authorizer: None,
            committed: false,
            cancelled: false,
        });
        Ok(())
    }

    fn tick_login(&mut self, wire: &mut impl Exchange) -> Result<(), String> {
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
                // uncertain, and from then on nothing else can be reported.
                let uncertain = *status == 0x0e;
                if uncertain != (login.committed && login.selection.writes()) {
                    return Err("login operation reported the wrong kind of end".into());
                }
                if !login.ends(uncertain, *kind, *detail, described) {
                    return Err("login operation ended in a way root cannot".into());
                }
                let rows = login_failure(*kind, *detail).ok_or("invalid login failure kind")?;
                let attempt = Arc::clone(&login.attempt);
                self.login = None;
                return if uncertain {
                    attempt.notice(Notice::Uncertain(if *kind == 0x0e { &[] } else { rows }))
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
                match login.attempt.present(description.clone()) {
                    Ok(receipt) if receipt == description && login.attempt.active() => {
                        let mut bytes = vec![0x13];
                        bytes.extend_from_slice(&receipt.encode());
                        if wire.exchange(&bytes)? != [0x93] {
                            return Err("invalid presentation acknowledgement".into());
                        }
                        login.receipt = Some(receipt);
                    }
                    _ => login.abandon(wire)?,
                }
            }
            // No PIN field yet: nothing in production reaches a PIN step.
            0x0c if presented
                && open
                && description.login_step().is_some_and(LoginStep::asks_pin) =>
            {
                login.cancel(wire)?;
                login.attempt.notice(Notice::NotAvailable)?;
            }
            0x05 if login.last && presented && open => {
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
                let rows = login.selection.success();
                self.login = None;
                attempt.notice(Notice::Login(rows))?;
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
        let response = wire.exchange(&attempt.selection.request()?)?;
        if attempt.selection == Selection::Write && response == [0x98, 0] {
            return attempt.notice(crate::attention::Notice::NoWrite);
        }
        if attempt.selection == Selection::Install && response == [0x99, 0] {
            return attempt.notice(crate::attention::Notice::NoInstall);
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
        });
        Ok(())
    }

    pub fn tick(&mut self, wire: &mut impl Exchange) -> Result<(), String> {
        if self.inspection.is_some() {
            return self.poll_inspection(wire);
        }
        if self.login.is_some() {
            return self.tick_login(wire);
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
                if pending.attempt.selection == Selection::Install
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

impl Pending {
    fn cancel(&mut self, wire: &mut impl Exchange) -> Result<(), String> {
        cancel(wire, self.request.nonce())?;
        self.cancelled = true;
        Ok(())
    }
}

impl LoginPending {
    fn cancel(&mut self, wire: &mut impl Exchange) -> Result<(), String> {
        cancel(wire, &self.nonce)?;
        self.cancelled = true;
        Ok(())
    }

    /// This client's own cancellation, which withdraws any prompt and says
    /// why: the attempt's deadline passed, or a presentation failed. After
    /// Escape the screen is closing and `notice` shows nothing.
    fn abandon(&mut self, wire: &mut impl Exchange) -> Result<(), String> {
        self.cancel(wire)?;
        self.attempt
            .notice(Notice::Login(if self.attempt.expired() {
                &["TIMED OUT"]
            } else {
                &["THE OPERATION FAILED"]
            }))
    }

    /// Whether root can end this operation with `kind` and `detail` here
    /// (td-authd/src/login.rs): any typed worker failure, `0e` only once a
    /// write's commit was acknowledged, cancellation only after this
    /// client's, and each refusal of a selection only before any
    /// description, since root refuses at the worker's baseline.
    fn ends(&self, uncertain: bool, kind: u8, detail: u8, described: bool) -> bool {
        match (kind, detail) {
            (0x01, _) | (0x06, 1..=6) | (0x02..=0x05 | 0x07..=0x0d | 0x0f..=0x12, 0) => true,
            (0x0e, 1..=5) => uncertain,
            (0x80, 0) => self.cancelled,
            (0x81, 0) => !described && self.selection == LoginSelection::Add,
            (0x82, 0) => !described && matches!(self.selection, LoginSelection::Enroll(_)),
            (0x83, 0) => !described && matches!(self.selection, LoginSelection::Remove(_)),
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
            if !matches!(status, 0x03 | 0x04 | 0x0d | 0x0e)
                || description.nonce() != &self.nonce
                || !self.selection.selects(description)
            {
                return Err("root login request changed the selected operation".into());
            }
            description.login_start()?;
            self.request = Some(description.clone());
            return Ok(());
        };
        if current == description {
            return Ok(());
        }
        // `03` too: root that read the next invitation, then stopped at its
        // deadline or a cancellation, describes that step while it reaps.
        // Presenting it still takes `04`, and committing a receipt.
        if !matches!(status, 0x03 | 0x04 | 0x0d | 0x0e) || self.receipt.as_ref() != Some(current) {
            return Err("login status changed a step that was not presented".into());
        }
        let admitted = current.login_successor(description)?;
        match description.login_step() {
            Some(LoginStep::Authorize { key, .. }) => self.authorizer = Some(key),
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
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU64;

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    struct Screen {
        path: PathBuf,
        attempt: Arc<Attempt>,
    }
    impl Screen {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "td-secret-screen-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            let mut runtime = Runtime::new(
                crate::framebuffer::Framebuffer::test_file(&path, 800, 600, 3200).unwrap(),
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

    struct Wire {
        replies: VecDeque<Vec<u8>>,
        calls: Vec<Vec<u8>>,
    }
    impl Exchange for Wire {
        fn exchange(&mut self, bytes: &[u8]) -> Result<Vec<u8>, String> {
            self.calls.push(bytes.to_vec());
            self.replies.pop_front().ok_or("uncertain delivery".into())
        }
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
        }
    }

    #[test]
    fn install_also_admits_a_whole_disk_installation_for_the_session_owner() {
        let disk = |owner: u32| {
            Request::new(
                [42; 32],
                owner,
                Operation::InstallDisk {
                    requester: owner,
                    disk: "vda".into(),
                    capacity: 8 << 30,
                    model: Some(crate::authority::consent::Label::model(b"QEMU HARDDISK")),
                    serial: None,
                    hostname: "td".into(),
                    username: "alice".into(),
                    deployment: [0xab; 8],
                },
            )
            .unwrap()
        };
        let mut screen = Screen::new();
        Arc::get_mut(&mut screen.attempt).unwrap().selection = Selection::Install;
        let mut client = Client::default();
        let reply = [&[0x92][..], disk(1000).encode().as_slice()].concat();
        client
            .start(&mut wire(vec![reply]), Arc::clone(&screen.attempt))
            .unwrap();
        assert_eq!(client.pending.as_ref().unwrap().request, disk(1000));
        let mut screen = Screen::new();
        Arc::get_mut(&mut screen.attempt).unwrap().selection = Selection::Install;
        let reply = [&[0x92][..], disk(1001).encode().as_slice()].concat();
        assert!(Client::default()
            .start(&mut wire(vec![reply]), Arc::clone(&screen.attempt))
            .is_err());
    }

    #[test]
    fn installation_waits_for_fresh_enter_after_complete_presentation() {
        let mut screen = Screen::new();
        Arc::get_mut(&mut screen.attempt).unwrap().selection = Selection::Install;
        let request = Request::new(
            [42; 32],
            1000,
            Operation::Install {
                deployment: "ab".repeat(32),
                requester: 1000,
            },
        )
        .unwrap();
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
            .1;
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

    #[test]
    fn installation_escape_or_hidden_prompt_cannot_be_confirmed() {
        for cancel in [false, true] {
            let mut screen = Screen::new();
            Arc::get_mut(&mut screen.attempt).unwrap().selection = Selection::Install;
            let request = Request::new(
                [42; 32],
                1000,
                Operation::Install {
                    deployment: "ab".repeat(32),
                    requester: 1000,
                },
            )
            .unwrap();
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
            fn exchange(&mut self, bytes: &[u8]) -> Result<Vec<u8>, String> {
                let response = self.wire.exchange(bytes)?;
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
        fn login(selection: LoginSelection) -> Self {
            let mut screen = Self::new();
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
    fn enroll_step(after: u8, key: u8, step: LoginStep) -> Request {
        login(Operation::LoginEnroll {
            account: 1000,
            before: 0,
            after,
            key,
            step,
        })
    }
    fn add_step(step: LoginStep) -> Request {
        login(Operation::LoginAdd {
            account: 1000,
            before: 3,
            after: 4,
            step,
        })
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
    fn remove_step(step: LoginStep) -> Request {
        login(Operation::LoginRemove {
            account: 1000,
            before: 3,
            after: 1,
            removed: removal(),
            step,
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
            LoginSelection::Remove(_) => vec![
                remove_step(LoginStep::Identify),
                remove_step(LoginStep::Authorize {
                    key: ENROLLED[0],
                    retries: 8,
                }),
            ],
        }
    }
    fn selections() -> Vec<LoginSelection> {
        vec![
            LoginSelection::Unlock,
            LoginSelection::Enroll(1),
            LoginSelection::Enroll(2),
            LoginSelection::Add,
            LoginSelection::Remove(removal()),
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

    /// Root's statuses for `presented` steps, from the start: the first
    /// while root waits for the worker to repeat it, then each presented,
    /// acknowledged and worked on, and the client's requests they draw.
    /// A PIN step's `0c` and `1c` arrive with the PIN field; here the
    /// worker goes straight to work.
    fn presented(selection: &LoginSelection, count: usize) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
        let steps = steps(selection);
        let mut replies = vec![started(), vec![0x91, 0x0b], status(3, &steps[0])];
        let mut calls = vec![
            Selection::Login(selection.clone()).request().unwrap(),
            vec![0x11],
            vec![0x11],
        ];
        for step in steps.iter().take(count) {
            replies.extend([status(4, step), vec![0x93], status(3, step)]);
            calls.extend([vec![0x11], description(&[0x13], step), vec![0x11]]);
        }
        (replies, calls)
    }

    fn drive(screen: &Screen, replies: Vec<Vec<u8>>) -> (Client, Wire, Result<(), String>) {
        let mut wire = wire(replies);
        let mut client = Client::default();
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
            let (client, wire, result) = drive(&screen, replies);
            assert_eq!(result, Ok(()), "{selection:?}");
            assert!(client.login.is_none());
            assert_eq!(wire.calls, calls, "{selection:?}");
            assert_eq!(sent(&wire, 0x13), steps.len());
            assert_eq!(screen.shown(), Some(Notice::Login(selection.success())));
        }
    }

    #[test]
    fn root_refusing_a_write_shows_not_available() {
        for selection in [
            LoginSelection::Enroll(1),
            LoginSelection::Enroll(2),
            LoginSelection::Add,
            LoginSelection::Remove(removal()),
        ] {
            let screen = Screen::login(selection.clone());
            let (client, wire, result) = drive(&screen, vec![vec![0x9b, 0]]);
            assert_eq!(result, Ok(()));
            assert!(client.login.is_none());
            assert_eq!(wire.calls.len(), 1);
            assert_eq!(screen.shown(), Some(Notice::NotAvailable));
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
                LoginSelection::Remove(removal()[..1].to_vec()),
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
        fn exchange(&mut self, bytes: &[u8]) -> Result<Vec<u8>, String> {
            let response = self.wire.exchange(bytes)?;
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
        for (replies, at) in [
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
                wire: wire(replies),
                attempt: &screen.attempt,
                at,
            };
            let mut client = Client::default();
            client
                .start(&mut wire, Arc::clone(&screen.attempt))
                .unwrap();
            while client.login.is_some() {
                client.tick(&mut wire).unwrap();
            }
            assert!(wire.wire.replies.is_empty());
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
        }
        // Root wants a PIN, which this build cannot take: the client
        // cancels, says so, and the cancellation's end adds nothing.
        let screen = Screen::login(unlock.clone());
        let mut replies = presented(&unlock, 2).0;
        replies.pop();
        replies.extend([
            status(0x0c, &last),
            vec![0x95, 0],
            status(3, &last),
            end(&last),
        ]);
        let (client, wire, result) = drive(&screen, replies);
        assert_eq!(result, Ok(()));
        assert!(client.login.is_none());
        assert_eq!(sent(&wire, 0x15), 1);
        assert_eq!(sent(&wire, 0x1c), 0);
        assert_eq!(sent(&wire, 0x14), 0);
        assert_eq!(screen.shown(), Some(Notice::NotAvailable));
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
                LoginSelection::Remove(_) => (0x83, 0),
            });
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
            ..Client::default()
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
                    for refusal in [0x81, 0x82, 0x83] {
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
            .chain(selections.iter().map(LoginSelection::success))
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
                    Notice::Menu,
                ),
                0x81 => (LoginSelection::Add, Point::Baseline, Notice::Login(rows)),
                0x82 => (
                    LoginSelection::Enroll(1),
                    Point::Baseline,
                    Notice::Login(rows),
                ),
                0x83 => (
                    LoginSelection::Remove(removal()),
                    Point::Baseline,
                    Notice::Login(rows),
                ),
                0x0e => (
                    LoginSelection::Remove(removal()),
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
                wire: wire(replies),
                attempt: &screen.attempt,
                at,
            };
            let mut client = Client::default();
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

    /// The client's own cancellation says why and withdraws the prompt: its
    /// deadline, or a presentation that failed.
    #[test]
    fn the_clients_own_cancellation_says_why() {
        let unlock = LoginSelection::Unlock;
        let steps = steps(&unlock);
        let end = [&ended(0x0d, 0x80, 0)[..], &steps[0].encode()].concat();
        let mut screen = Screen::login(unlock.clone());
        let deadline = Instant::now() + Duration::from_millis(1500);
        screen.attempt = Arc::new(Attempt {
            origin: crate::input::test_origin(),
            runtime: Arc::clone(&screen.attempt.runtime),
            selection: Selection::Login(unlock.clone()),
            state: AtomicU8::new(ACTIVE),
            deadline: Some(deadline),
            presentation: Mutex::new(None),
            confirmed: AtomicBool::new(false),
        });
        let (mut client, _, result) = drive(&screen, presented(&unlock, 1).0);
        assert_eq!(result, Ok(()));
        // The prompt is up.
        assert_eq!(screen.shown(), None);
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
            fn exchange(&mut self, bytes: &[u8]) -> Result<Vec<u8>, String> {
                let response = self.wire.exchange(bytes)?;
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
        let mut client = Client::default();
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

    #[test]
    fn a_login_attempt_lives_its_operations_ceiling_and_no_longer() {
        for (selection, seconds) in [
            (LoginSelection::Unlock, 120),
            (LoginSelection::Enroll(1), 120),
            (LoginSelection::Remove(removal()), 120),
            (LoginSelection::Enroll(2), 240),
            (LoginSelection::Add, 240),
        ] {
            let before = Instant::now();
            let screen = Screen::login(selection.clone());
            let lifetime = screen.attempt.deadline.unwrap().duration_since(before);
            assert!(lifetime >= Duration::from_secs(seconds));
            assert!(lifetime < Duration::from_secs(seconds + 1));
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
            LoginSelection::Remove(Vec::new()),
            LoginSelection::Remove(removal().into_iter().rev().collect()),
            LoginSelection::Remove(vec![Slot {
                position: 9,
                key: NEW,
            }]),
            LoginSelection::Remove(vec![Slot {
                position: 0,
                key: NEW,
            }]),
            LoginSelection::Remove(
                (1..=9)
                    .map(|position| Slot { position, key: NEW })
                    .collect(),
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
        assert_eq!(chosen, LoginSelection::Remove(removal()));
        assert_eq!(
            wire(chosen),
            Ok([&[0x1b, 10, 2, 1][..], &ENROLLED[0], &[3], &ENROLLED[2]].concat())
        );
        assert!(digits.toggle(1));
        assert_eq!(
            digits.selection(),
            Some(LoginSelection::Remove(vec![Slot {
                position: 3,
                key: ENROLLED[2],
            }]))
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
        let mut client = Client::default();
        client
            .start(&mut wire, Arc::clone(&screen.attempt))
            .unwrap();
        let other = Screen::new();
        client.start(&mut wire, Arc::clone(&other.attempt)).unwrap();
        assert_eq!(wire.calls.len(), 1);
        assert_eq!(other.shown(), Some(Notice::Busy));
    }
}

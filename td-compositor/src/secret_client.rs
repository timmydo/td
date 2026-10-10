//! Private session protocol and a physical attention lifetime.

use crate::attention::{Field, Notice};
use crate::authority::consent::{
    Admitted, ApprovalKey, Enrollment, Fingerprint, LoginStep, Operation, Platform, Recovery,
    Request, Role, Slot, LOGIN_CEREMONY, LOGIN_KEYS, LOGIN_TWO_CEREMONIES,
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
    Erase,
    Submit,
}

impl std::fmt::Debug for FieldKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Byte(_) => "Byte(..)",
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

/// The PIN field's state: open for a presented PIN step root asked a PIN
/// for, until its PIN is submitted and taken, or it closes.
#[derive(Default)]
pub(crate) struct PinField {
    open: bool,
    submitted: bool,
    /// When the field's own paint was seen on glass: only a press made
    /// after it may type.
    shown: Option<u128>,
    pin: PinBuffer,
}

impl PinField {
    pub fn open(&mut self) {
        self.pin.clear();
        self.open = true;
        self.submitted = false;
        self.shown = None;
    }

    pub fn close(&mut self) {
        self.pin.clear();
        self.open = false;
        self.submitted = false;
        self.shown = None;
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// One key while the field is open and not yet submitted: the masked
    /// length to show when the bytes changed. A byte outside printable
    /// ASCII, a 64th byte, Backspace on nothing and an Enter before four
    /// bytes change nothing.
    pub fn key(&mut self, key: FieldKey) -> Option<usize> {
        if !self.open || self.submitted {
            return None;
        }
        let changed = match key {
            FieldKey::Byte(byte) => (0x20..=0x7e).contains(&byte) && self.pin.push(byte),
            FieldKey::Erase => self.pin.pop(),
            FieldKey::Submit => {
                self.submitted = self.pin.length >= PIN_SHORTEST;
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

fn asks_pin(request: &Request) -> bool {
    request.login_step().is_some_and(LoginStep::asks_pin)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Selection {
    Unlock(Role),
    Enroll(Recovery),
    Write,
    Install,
    Login(LoginSelection),
    Elevation(Elevation),
}
impl From<Role> for Selection {
    fn from(role: Role) -> Self {
        Self::Unlock(role)
    }
}
impl Selection {
    fn operation(&self) -> Option<Operation> {
        Some(match self {
            Self::Write | Self::Install | Self::Login(_) | Self::Elevation(_) => return None,
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
    /// before commit: the approval key for an update or an elevation, a
    /// fresh Enter for a live boot's whole-disk installation.
    fn confirms(&self) -> bool {
        matches!(self, Self::Install | Self::Elevation(_))
    }
}

/// An elevation (td-compositor/DESIGN.md, "Elevation consent"): what the
/// attention menu's `B` asks root for, and the queued request its `H`
/// selects. Each description carries the approval key that alone confirms
/// it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Elevation {
    Rollback,
    Hostname,
}

impl Elevation {
    /// The elevation `request` describes, and its approval key.
    fn of(request: &Request) -> Option<(Self, ApprovalKey)> {
        match request.operation() {
            Operation::DeployRollback { key, .. } => Some((Self::Rollback, *key)),
            Operation::SetHostname { key, .. } => Some((Self::Hostname, *key)),
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

/// The approval key `request` carries: an update's (`deploy-publish`) or
/// an elevation's. Every other description, a whole-disk installation's
/// among them, has none.
fn approval_key(request: &Request) -> Option<ApprovalKey> {
    match request.operation() {
        Operation::Install { key, .. } => Some(*key),
        _ => Elevation::of(request).map(|(_, key)| key),
    }
}

/// A presented prompt that waits for its physical confirmation.
struct Presented {
    request: Request,
    /// A press must be stamped strictly later than this: the
    /// CLOCK_MONOTONIC sample taken after the prompt's full-frame
    /// submission, and once an approval key's first digit is typed, that
    /// digit's time.
    after: u128,
    /// How many of the approval key's digits were typed in order.
    typed: usize,
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
        Some(LoginSelection::Remove(slots))
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
        _ => return None,
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
            field.open();
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
        if self.selection.confirms() {
            *self
                .presentation
                .lock()
                .map_err(|_| "presentation receipt lock poisoned")? = Some(Presented {
                request: request.clone(),
                after: completed,
                typed: 0,
            });
        }
        Ok(request)
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
    /// update takes its approval key.
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

    /// One approval-key digit, `2` to `9` in ASCII, pressed at `timestamp`,
    /// which only the physical evdev adapter can offer, for an update or an
    /// elevation: answers whether it ended the request. A press the prompt
    /// cannot answer (stamped before the prompt was on glass or before the
    /// first digit, a withdrawn or replaced prompt, a key already typed)
    /// neither advances nor ends it.
    /// A digit that matches its position advances, and the second confirms;
    /// one that does not cancels the attempt, as Escape does, and the
    /// adapter then drains the screen.
    pub fn approve(
        &self,
        _origin: &EvdevOrigin,
        digit: u8,
        timestamp: u128,
    ) -> Result<bool, String> {
        if !matches!(self.selection, Selection::Install | Selection::Elevation(_))
            || !(b'2'..=b'9').contains(&digit)
            || !self.active()
        {
            return Ok(false);
        }
        let wrong = {
            let runtime = self.runtime.lock().map_err(|_| "runtime lock poisoned")?;
            let mut presentation = self
                .presentation
                .lock()
                .map_err(|_| "presentation receipt lock poisoned")?;
            let Some(presented) = presentation
                .as_mut()
                .filter(|presented| self.answers(&runtime, presented, timestamp))
            else {
                return Ok(false);
            };
            let Some(key) = approval_key(&presented.request) else {
                return Ok(false);
            };
            let digits = key.digits();
            let Some(expected) = digits.get(presented.typed) else {
                return Ok(false);
            };
            let wrong = digit != *expected;
            if !wrong {
                presented.typed += 1;
                presented.after = timestamp;
                if presented.typed == digits.len() {
                    self.confirmed.store(true, Ordering::SeqCst);
                }
            }
            wrong
        };
        // Outside the runtime's guard, since cancellation takes the PIN
        // field's, which every holder takes before the runtime's.
        if wrong {
            self.cancel();
        }
        Ok(wrong)
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
    /// The current step is the operation's final one: only its commit
    /// follows.
    last: bool,
    /// An addition's authorizing key, which its new key's steps never name.
    authorizer: Option<Fingerprint>,
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
            field: false,
            pin_sent: false,
            committed: false,
            cancelled: false,
        });
        Ok(())
    }

    fn tick_login(&mut self, wire: &mut impl Exchange) -> Result<(), String> {
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
                self.login_ended = true;
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
            // Root waits for the presented PIN step's PIN: the field opens,
            // and once the person submits it the PIN goes as `1c`. Nothing
            // in production reaches a PIN step.
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
                let rows = login.selection.success();
                let unlock = login.selection == LoginSelection::Unlock;
                self.login = None;
                self.login_ended = true;
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
        let response = wire.exchange(&attempt.selection.request()?)?;
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
                Selection::Elevation(elevation) => {
                    Elevation::of(&request).is_some_and(|(described, _)| described == elevation)
                }
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
                        crate::attention::Notice::HostnameSaved
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
        // Root moves past a PIN step only once it has the PIN.
        if asks_pin(current) && !self.pin_sent {
            return Err("login status passed a PIN step before its PIN".into());
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
                let empty = {
                    let field = attempt.field.lock().unwrap();
                    field.is_open() && field.typed().is_empty() && !field.submitted()
                };
                if empty {
                    type_pin(attempt, PIN);
                }
            }
            Ok(reply)
        }
    }
    /// The PIN the tests' person types.
    const PIN: &[u8] = b"1234";
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
    /// `pin`'s keys, then Enter, as the evdev adapter offers them.
    fn type_pin(attempt: &Attempt, pin: &[u8]) {
        for byte in pin {
            attempt
                .field_key(
                    &crate::input::test_origin(),
                    FieldKey::Byte(*byte),
                    pressed(attempt),
                )
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
                    storage: crate::authority::consent::Storage::DeviceBound,
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

    /// Root's description of a queued update for the owner, carrying
    /// `key`.
    fn update(key: &[u8; 2]) -> Request {
        Request::new(
            [42; 32],
            1000,
            Operation::Install {
                key: crate::authority::consent::ApprovalKey::new(*key).unwrap(),
                deployment: "ab".repeat(32),
                requester: 1000,
            },
        )
        .unwrap()
    }

    /// An update (`deploy-publish`) commits once, only after its two
    /// digits are typed in order on the presented prompt, and the commit
    /// is root's exact description, so it carries the key typed. Enter,
    /// fresh or not, confirms nothing.
    #[test]
    fn an_update_commits_only_after_its_key_is_typed_never_on_enter() {
        let mut screen = Screen::new();
        Arc::get_mut(&mut screen.attempt).unwrap().selection = Selection::Install;
        let described = update(b"58");
        let invitation = status(5, &described);
        let (mut client, mut wire, shown) = presented_elevation(
            &screen,
            &described,
            vec![
                invitation.clone(),
                invitation.clone(),
                invitation,
                vec![0x94],
                status(6, &described),
            ],
        );
        assert_eq!(wire.calls[0], [0x19]);
        // Invited to commit, the client waits: Enter after presentation,
        // or at any later time, confirms nothing.
        for at in [shown + 1, shown + 2, u128::MAX] {
            screen
                .attempt
                .confirm_install(&crate::input::test_origin(), at)
                .unwrap();
        }
        assert!(!confirmed(&screen) && screen.attempt.active());
        client.tick(&mut wire).unwrap();
        assert_eq!(sent(&wire, 0x14), 0);
        assert!(!press(&screen, b'5', shown + 3));
        client.tick(&mut wire).unwrap();
        assert_eq!(sent(&wire, 0x14), 0);
        assert!(!press(&screen, b'8', shown + 4));
        assert!(confirmed(&screen));
        client.tick(&mut wire).unwrap();
        let commits: Vec<_> = wire
            .calls
            .iter()
            .filter(|call| call.first() == Some(&0x14))
            .collect();
        assert_eq!(commits, [&description(&[0x14], &described)]);
        assert_eq!(commits[0][46..48], *b"58");
        client.tick(&mut wire).unwrap();
        assert!(client.pending.is_none());
        assert_eq!(
            screen.attempt.runtime.lock().unwrap().attention_shown(),
            Some(Notice::Installed)
        );
        assert_eq!(sent(&wire, 0x14), 1);
    }

    /// An update's wrong digit, at either position, ends it unapproved as
    /// an elevation's does; a digit stamped before the prompt was on glass
    /// neither advances nor ends it.
    #[test]
    fn an_updates_wrong_digit_ends_it_and_an_early_one_counts_for_nothing() {
        for typed_first in [false, true] {
            let mut screen = Screen::new();
            Arc::get_mut(&mut screen.attempt).unwrap().selection = Selection::Install;
            let described = update(b"58");
            let (mut client, mut wire, shown) = presented_elevation(
                &screen,
                &described,
                vec![vec![0x95, 0], status(7, &described)],
            );
            for (digit, at) in [(b'5', shown), (b'9', shown - 1), (b'0', shown + 1)] {
                assert!(!press(&screen, digit, at));
            }
            assert!(screen.attempt.active());
            if typed_first {
                assert!(!press(&screen, b'5', shown + 1));
            }
            assert!(press(&screen, b'9', shown + 2));
            assert!(!screen.attempt.active() && !confirmed(&screen));
            assert!(!press(&screen, b'8', shown + 3));
            client.tick(&mut wire).unwrap();
            assert!(client.pending.is_none());
            assert_eq!(sent(&wire, 0x15), 1);
            assert_eq!(sent(&wire, 0x14), 0);
        }
    }

    /// A live boot's whole-disk installation keeps its fresh Enter after
    /// complete presentation, and no digit confirms or ends it.
    #[test]
    fn a_disk_installation_waits_for_fresh_enter_and_takes_no_digit() {
        let mut screen = Screen::new();
        Arc::get_mut(&mut screen.attempt).unwrap().selection = Selection::Install;
        let request = disk(crate::authority::consent::Storage::Unencrypted);
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
        for digit in *b"23456789" {
            assert!(!press(&screen, digit, completed + 1));
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

    fn disk(storage: crate::authority::consent::Storage) -> Request {
        Request::new(
            [42; 32],
            1000,
            Operation::InstallDisk {
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

    // Elevation consent: the approval key.

    const ELEVATIONS: &[Elevation] = &[Elevation::Rollback, Elevation::Hostname];

    /// Root's description of `elevation` for the owner, carrying `key`.
    fn elevation(elevation: Elevation, key: &[u8; 2]) -> Request {
        let key = crate::authority::consent::ApprovalKey::new(*key).unwrap();
        let operation = match elevation {
            Elevation::Rollback => Operation::DeployRollback {
                key,
                current: "a".repeat(64),
                previous: "b".repeat(64),
            },
            Elevation::Hostname => Operation::SetHostname {
                key,
                requester: 1000,
                old: "td".into(),
                new: "td-laptop".into(),
            },
        };
        Request::new([42; 32], 1000, operation).unwrap()
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

    /// The evdev adapter's offer of `digit` pressed at `at`: whether it
    /// ended the request.
    fn press(screen: &Screen, digit: u8, at: u128) -> bool {
        screen
            .attempt
            .approve(&crate::input::test_origin(), digit, at)
            .unwrap()
    }

    fn confirmed(screen: &Screen) -> bool {
        screen.attempt.confirmed.load(Ordering::SeqCst)
    }

    /// Each elevation's prompt commits once, only after its two digits are
    /// typed in order on the presented prompt, and the commit is root's
    /// exact description, so it carries the key typed. The same digit
    /// twice is two presses. Root's success shows each one's own screen.
    #[test]
    fn each_elevation_commits_only_after_its_key_is_typed_in_order() {
        for (elevation, key, request) in [
            (Elevation::Rollback, b"47", vec![0x1d]),
            (Elevation::Hostname, b"44", vec![0x1e]),
        ] {
            let screen = elevation_screen(elevation);
            let described = super::tests::elevation(elevation, key);
            let invitation = status(5, &described);
            let (mut client, mut wire, shown) = presented_elevation(
                &screen,
                &described,
                vec![
                    invitation.clone(),
                    invitation.clone(),
                    invitation,
                    vec![0x94],
                    status(6, &described),
                ],
            );
            assert_eq!(wire.calls[0], request);
            // Invited to commit, the client waits for the key.
            client.tick(&mut wire).unwrap();
            assert!(!press(&screen, key[0], shown + 1));
            assert!(!confirmed(&screen) && screen.attempt.active());
            client.tick(&mut wire).unwrap();
            assert_eq!(sent(&wire, 0x14), 0);
            assert!(!press(&screen, key[1], shown + 2));
            assert!(confirmed(&screen));
            client.tick(&mut wire).unwrap();
            let commits: Vec<_> = wire
                .calls
                .iter()
                .filter(|call| call.first() == Some(&0x14))
                .collect();
            assert_eq!(commits, [&description(&[0x14], &described)]);
            assert_eq!(commits[0][46..48], key[..]);
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

    /// Enter never confirms an elevation, nor a digit anything without a
    /// key: a disk installation, or a write.
    #[test]
    fn enter_never_confirms_an_elevation_nor_a_digit_a_keyless_prompt() {
        for &elevation in ELEVATIONS {
            let screen = elevation_screen(elevation);
            let described = super::tests::elevation(elevation, b"47");
            let (mut client, mut wire, shown) =
                presented_elevation(&screen, &described, vec![status(5, &described)]);
            for at in [shown + 1, shown + 2, u128::MAX] {
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
            (Selection::Write, update(b"47")),
        ] {
            let mut screen = Screen::new();
            Arc::get_mut(&mut screen.attempt).unwrap().selection = selection;
            screen.attempt.present(request).unwrap();
            for digit in *b"23456789" {
                assert!(!press(&screen, digit, u128::MAX));
            }
            assert!(!confirmed(&screen) && screen.attempt.active());
        }
    }

    /// A digit stamped before the prompt was on glass, or the second before
    /// the first, neither advances nor ends the request, right or wrong.
    #[test]
    fn a_digit_stamped_before_its_turn_neither_advances_nor_ends() {
        for &elevation in ELEVATIONS {
            let screen = elevation_screen(elevation);
            // Selected, not yet presented: nothing to answer.
            assert!(!press(&screen, b'4', u128::MAX));
            assert!(!press(&screen, b'9', u128::MAX));
            let described = super::tests::elevation(elevation, b"47");
            let (_client, _wire, shown) = presented_elevation(&screen, &described, vec![]);
            for (digit, at) in [(b'4', shown), (b'4', shown - 1), (b'9', shown), (b'9', 0)] {
                assert!(!press(&screen, digit, at));
            }
            assert!(screen.attempt.active());
            assert_eq!(
                screen
                    .attempt
                    .presentation
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .typed,
                0
            );
            assert!(!press(&screen, b'4', shown + 5));
            // The second digit must be later than the first.
            for (digit, at) in [(b'7', shown + 5), (b'9', shown + 5), (b'7', shown + 4)] {
                assert!(!press(&screen, digit, at));
            }
            assert!(!confirmed(&screen) && screen.attempt.active());
            assert!(!press(&screen, b'7', shown + 6));
            assert!(confirmed(&screen));
            // A third digit has no position, right or wrong.
            assert!(!press(&screen, b'9', shown + 7));
            assert!(screen.attempt.active());
        }
    }

    /// A counted digit from 2 to 9 that does not match, at either position,
    /// ends the request unapproved: the attempt is cancelled, the client
    /// cancels with root and never commits. 0, 1 and anything else are no
    /// digit of the key's alphabet and end nothing.
    #[test]
    fn a_wrong_digit_at_either_position_ends_the_request() {
        for &elevation in ELEVATIONS {
            for typed_first in [false, true] {
                let screen = elevation_screen(elevation);
                let described = super::tests::elevation(elevation, b"47");
                let (mut client, mut wire, shown) = presented_elevation(
                    &screen,
                    &described,
                    vec![vec![0x95, 0], status(7, &described)],
                );
                for digit in [b'0', b'1', b'a', 0x34 + 0x80, b'\n'] {
                    assert!(!press(&screen, digit, shown + 1));
                }
                if typed_first {
                    assert!(!press(&screen, b'4', shown + 1));
                }
                assert!(screen.attempt.active());
                assert!(press(&screen, b'8', shown + 2));
                assert!(!screen.attempt.active() && !confirmed(&screen));
                // The right digits after it change nothing.
                assert!(!press(&screen, b'4', shown + 3));
                assert!(!press(&screen, b'7', shown + 4));
                assert!(!confirmed(&screen));
                client.tick(&mut wire).unwrap();
                assert!(client.pending.is_none());
                assert_eq!(sent(&wire, 0x15), 1);
                assert_eq!(sent(&wire, 0x14), 0);
            }
        }
    }

    /// A prompt replaced by a notice, withdrawn or draining, or a cancelled
    /// attempt, takes no digit: none advances, ends or confirms.
    #[test]
    fn a_hidden_prompt_or_cancelled_attempt_takes_no_digit() {
        for &elevation in ELEVATIONS {
            for case in 0..3 {
                let screen = elevation_screen(elevation);
                let described = super::tests::elevation(elevation, b"47");
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
                for (digit, at) in [(b'4', shown + 1), (b'7', shown + 2), (b'9', shown + 3)] {
                    assert!(!press(&screen, digit, at));
                }
                assert!(!confirmed(&screen) && screen.attempt.active());
                assert!(!screen.attempt.commit());
            }
            let screen = elevation_screen(elevation);
            let described = super::tests::elevation(elevation, b"47");
            let (_client, _wire, shown) = presented_elevation(&screen, &described, vec![]);
            screen.attempt.cancel();
            assert!(!press(&screen, b'4', shown + 1));
            assert!(!press(&screen, b'7', shown + 2));
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
                key: crate::authority::consent::ApprovalKey::new(*b"47").unwrap(),
                current: "a".repeat(64),
                previous: "b".repeat(64),
            },
        )
        .unwrap();
        let update = update(b"47");
        for (selection, described) in [
            (
                Selection::Elevation(Elevation::Rollback),
                elevation(Elevation::Hostname, b"47"),
            ),
            (
                Selection::Elevation(Elevation::Hostname),
                elevation(Elevation::Rollback, b"47"),
            ),
            (Selection::Elevation(Elevation::Rollback), other_owner),
            (Selection::Elevation(Elevation::Rollback), update),
            (Selection::Install, elevation(Elevation::Rollback, b"47")),
            (Selection::Write, elevation(Elevation::Hostname, b"47")),
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
        let mut replies = vec![started(), vec![0x91, 0x0b], status(3, &steps[0])];
        let mut calls = vec![
            Selection::Login(selection.clone()).request().unwrap(),
            vec![0x11],
            vec![0x11],
        ];
        for step in steps.iter().take(count) {
            replies.extend([status(4, step), vec![0x93]]);
            calls.extend([vec![0x11], description(&[0x13], step)]);
            if asks_pin(step) {
                replies.extend([status(0x0c, step), status(0x0c, step), vec![0x9c, 0]]);
                calls.extend([vec![0x11], vec![0x11], pin_request(step, PIN)]);
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
            assert_eq!(screen.shown(), Some(Notice::Login(selection.success())));
            assert_eq!(screen.attempt.field_raw(), [0; PIN_CAPACITY]);
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
            let (mut client, wire, result) = drive(&screen, vec![vec![0x9b, 0]]);
            assert_eq!(result, Ok(()));
            assert!(client.login.is_none());
            // Nothing began, so nothing ended.
            assert!(!client.take_login_end());
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
                    Notice::default(),
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

    /// The client's own cancellation says why and withdraws the prompt: its
    /// deadline, or a presentation that failed.
    #[test]
    fn the_clients_own_cancellation_says_why() {
        let unlock = LoginSelection::Unlock;
        let steps = steps(&unlock);
        let end = [&ended(0x0d, 0x80, 0)[..], &steps[0].encode()].concat();
        let (screen, deadline) = Screen::expiring(unlock.clone(), Duration::from_millis(1500));
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
        while !wire.replies.is_empty() {
            client.tick(&mut wire).unwrap();
        }
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
            let (screen, deadline) =
                Screen::expiring(LoginSelection::Unlock, Duration::from_millis(2000));
            let (mut client, _) = at_pin(&screen, vec![]);
            type_bytes(&screen, b"12");
            assert_ne!(screen.attempt.field_raw(), [0; PIN_CAPACITY]);
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
        let (screen, deadline) =
            Screen::expiring(LoginSelection::Unlock, Duration::from_millis(2000));
        let (mut client, mut wire) = submitted(&screen, b"4321");
        TAKEN.with(|taken| {
            *taken.borrow_mut() = Some(Box::new(move || {
                while Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(20));
                }
            }))
        });
        client.tick(&mut wire).unwrap();
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
    /// presented: at 800x600 the removal of eight keys fails at its prompt,
    /// before any receipt, so no PIN reaches the key.
    #[test]
    fn a_pin_step_without_room_for_its_field_is_never_presented() {
        use crate::authority::consent::{Slot, LOGIN_KEYS};
        let removal = |step| {
            Request::new(
                NONCE,
                1000,
                Operation::LoginRemove {
                    account: 1000,
                    before: LOGIN_KEYS,
                    after: 0,
                    removed: (1..=LOGIN_KEYS)
                        .map(|position| Slot {
                            position,
                            key: [0xa1; 4],
                        })
                        .collect(),
                    step,
                },
            )
            .unwrap()
        };
        let screen = Screen::login(LoginSelection::Unlock);
        let authorize = removal(LoginStep::Authorize {
            key: [0xa1; 4],
            retries: 8,
        });
        assert_eq!(
            screen.attempt.present(authorize).unwrap_err(),
            "output cannot hold the PIN field beneath this prompt"
        );
        // The same operation's silent first step, which asks no PIN, is
        // shown at that size.
        let screen = Screen::login(LoginSelection::Unlock);
        assert!(screen.attempt.present(removal(LoginStep::Identify)).is_ok());
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
        let painted = |pin: &[u8]| {
            let screen = Screen::login(LoginSelection::Unlock);
            let _open = at_pin(&screen, vec![]);
            type_bytes(&screen, pin);
            assert_eq!(
                screen.field(),
                Some(Field::Pin(u8::try_from(pin.len()).unwrap()))
            );
            std::fs::read(&screen.path).unwrap()
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

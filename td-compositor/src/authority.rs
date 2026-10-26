#![deny(unsafe_code)]

//! The compositor owns one private endpoint; its worker alone exchanges frames.

#[cfg_attr(not(feature = "target-recipe"), path = "../../td-authd/src/channel.rs")]
#[cfg_attr(feature = "target-recipe", path = "auth/channel.rs")]
mod channel;
#[cfg_attr(not(feature = "target-recipe"), path = "../../td-authd/src/consent.rs")]
#[cfg_attr(feature = "target-recipe", path = "auth/consent.rs")]
#[allow(
    dead_code,
    reason = "immutable trusted-prompt contract; authority consumer follows"
)]
pub(crate) mod consent;
#[cfg_attr(not(feature = "target-recipe"), path = "../../td-authd/src/sys.rs")]
#[cfg_attr(feature = "target-recipe", path = "auth/sys.rs")]
mod sys;
// `9a`'s names, admitted under the rules root sends them by and drawn on
// the lock surface, and the names a consent hostname description admits.
#[cfg_attr(
    not(feature = "target-recipe"),
    path = "../../td-firstboot/src/hostname.rs"
)]
#[cfg_attr(feature = "target-recipe", path = "auth/hostname.rs")]
mod hostname;
#[cfg_attr(
    not(feature = "target-recipe"),
    path = "../../td-authd/src/primary_account.rs"
)]
#[cfg_attr(feature = "target-recipe", path = "auth/primary_account.rs")]
#[allow(
    dead_code,
    reason = "only the name rule is the compositor's; the account database is td-authd's"
)]
mod primary_account;

use std::collections::VecDeque;
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

const VERSION: &[u8] = b"TDLA003\n";
const CAPACITY: usize = 16;
const QUEUE_CAPACITY: usize = 1;
const TICK: Duration = Duration::from_millis(250);
/// How often `1a` is asked again while the state could not be read or a
/// revocation is pending.
const LOGIN_POLL: Duration = Duration::from_millis(250);
/// TOKEN-LOGIN.md's failure kind for a state that could not be read.
const UNREADABLE: u8 = 0x0c;

enum Work {
    Program(Program),
    /// `22` and a name: td-software, opened at that application.
    SoftwareAt(String),
    Supervised(Supervised),
    Secret(std::sync::Arc<crate::secret_client::Attempt>),
}

/// A control-socket application request and where its answer goes. It
/// waits in a queue of its own, which the worker takes from after the
/// physical queue, so a program on the control socket never holds the
/// slot physical input's requests need.
type CacheWork = (CacheVerb, String, SyncSender<CacheAnswer>);

/// What td-software asks root of one application's cache, as request
/// `24` (td-authd/DESIGN.md, "Application requests"): its status, or one
/// of the four operations td-app runs as the application's account.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CacheVerb {
    Status,
    Install,
    Update,
    Remove,
    DeleteData,
}

/// Each verb with its control-socket word and its byte in `24`.
const CACHE_VERBS: &[(CacheVerb, &str, u8)] = &[
    (CacheVerb::Status, "status", 0),
    (CacheVerb::Install, "install", 1),
    (CacheVerb::Update, "update", 2),
    (CacheVerb::Remove, "remove", 3),
    (CacheVerb::DeleteData, "delete-data", 4),
];

impl CacheVerb {
    pub(crate) fn word(self) -> &'static str {
        CACHE_VERBS
            .iter()
            .find(|(verb, _, _)| *verb == self)
            .map_or("status", |(_, word, _)| word)
    }

    pub(crate) fn from_word(word: &str) -> Option<Self> {
        CACHE_VERBS
            .iter()
            .find(|(_, known, _)| *known == word)
            .map(|(verb, _, _)| *verb)
    }

    fn byte(self) -> u8 {
        CACHE_VERBS
            .iter()
            .find(|(verb, _, _)| *verb == self)
            .map_or(0, |(_, _, byte)| *byte)
    }

    /// An operation's byte in `87`: `00` is none, and status is never one.
    fn operation(byte: u8) -> Result<Option<Self>, String> {
        match CACHE_VERBS.iter().find(|(_, _, known)| *known == byte) {
            Some((CacheVerb::Status, _, _)) => Ok(None),
            Some((verb, _, _)) => Ok(Some(*verb)),
            None => Err("invalid application request answer".into()),
        }
    }
}

/// The most refusal bytes `87` carries.
const CACHE_REASON_BYTES: usize = 200;

/// What root last read of an application's status, as `87` says.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CacheRecord {
    /// No read has finished yet.
    Unread,
    /// td-app's canonical record, read after the last operation ended.
    Current(String),
    /// The same, read before the last operation ended; a read is under way.
    Older(String),
    /// The last read failed.
    Failed,
}

/// `87`'s answer: the operation running, the last one's outcome and its
/// refusal, and the record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CacheStatus {
    pub(crate) operation: Option<CacheVerb>,
    pub(crate) last: Option<(CacheVerb, Option<String>)>,
    pub(crate) record: CacheRecord,
}

/// Root's answer to `24`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CacheAnswer {
    /// `86`: the operation started.
    Started,
    /// `ff 01`: an operation on the application runs already, root's or
    /// one root's last read of its status shows.
    Busy,
    /// `ff 02`: root could not start it.
    Unstarted,
    /// `ff 03`: a live session runs only the installer.
    Live,
    /// `ff 04`: root follows as many applications as it keeps, all of
    /// them in use.
    Full,
    /// `ff 05`: root has no recent read of the application's status to
    /// start an operation on, and has begun one.
    Unread,
    Status(CacheStatus),
}

impl CacheAnswer {
    /// Exactly `87`'s shape for a status, `86` for an operation, or a
    /// refusal; anything else ends the paired generation.
    fn decode(verb: CacheVerb, answer: &[u8]) -> Result<Self, String> {
        let invalid = || "invalid application request answer".to_string();
        match (verb, answer) {
            (_, [0xff, 1]) => return Ok(Self::Busy),
            (_, [0xff, 2]) => return Ok(Self::Unstarted),
            (_, [0xff, 3]) => return Ok(Self::Live),
            (_, [0xff, 4]) => return Ok(Self::Full),
            (_, [0xff, 5]) => return Ok(Self::Unread),
            (CacheVerb::Status, _) => {}
            (_, [0x86]) => return Ok(Self::Started),
            _ => return Err(invalid()),
        }
        let [0x87, operation, last, result, length, rest @ ..] = answer else {
            return Err(invalid());
        };
        let operation = CacheVerb::operation(*operation)?;
        let last = CacheVerb::operation(*last)?;
        let (reason, rest) = rest
            .split_at_checked(usize::from(*length))
            .ok_or_else(invalid)?;
        let printable = |bytes: &[u8]| bytes.iter().all(|b| (b' '..=b'~').contains(b));
        let last = match (last, *result, reason) {
            (None, 0, []) => None,
            (Some(verb), 0, []) => Some((verb, None)),
            (Some(verb), 1, reason)
                if !reason.is_empty()
                    && reason.len() <= CACHE_REASON_BYTES
                    && printable(reason) =>
            {
                let reason = std::str::from_utf8(reason).map_err(|_| invalid())?;
                Some((verb, Some(reason.to_string())))
            }
            _ => return Err(invalid()),
        };
        let [state, record @ ..] = rest else {
            return Err(invalid());
        };
        let text = || -> Result<String, String> {
            let lines = record.ends_with(b"\n")
                && record
                    .iter()
                    .all(|b| *b == b'\n' || *b == b'\t' || (b' '..=b'~').contains(b));
            if !lines {
                return Err(invalid());
            }
            String::from_utf8(record.to_vec()).map_err(|_| invalid())
        };
        let record = match (*state, record) {
            (0, []) => CacheRecord::Unread,
            (1, _) => CacheRecord::Current(text()?),
            (2, _) => CacheRecord::Older(text()?),
            (3, []) => CacheRecord::Failed,
            _ => return Err(invalid()),
        };
        Ok(Self::Status(CacheStatus {
            operation,
            last,
            record,
        }))
    }
}

/// What root may be asked of the applications' units (APPLICATIONS.md
/// §W.9): start an application's, which root checks against its registry.
/// An application updates only through td-software's requests; `0e`, the
/// retired update unit's request, is never sent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Supervised {
    Start(String),
}

/// `0f`'s answer (td-authd/DESIGN.md, "Fixed program launch
/// prerequisite"): the application unit's state as root last read it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UnitState {
    Idle,
    Starting,
    Running,
    Failed,
    Unknown,
}

impl UnitState {
    /// Exactly `85` and a state, or `ff 03` on a live boot, which runs no
    /// application, `ff 04` for a name root's registry lacks or `ff 02`
    /// when root cannot read it, each read as failed; anything else ends
    /// the paired generation.
    fn decode(answer: &[u8]) -> Result<Self, String> {
        match answer {
            [0x85, 0] => Ok(Self::Idle),
            [0x85, 1] => Ok(Self::Starting),
            [0x85, 2] => Ok(Self::Running),
            [0x85, 3] | [0xff, 2..=4] => Ok(Self::Failed),
            [0x85, 4] => Ok(Self::Unknown),
            _ => Err("invalid application unit state answer".into()),
        }
    }
}

/// What became of the start requests the worker sent: how many it sent,
/// whether root refused the last, and the unit state read since.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct StartOutcome {
    pub(crate) sent: u64,
    pub(crate) refused: bool,
    pub(crate) state: Option<UnitState>,
}

#[derive(Default)]
struct ApplicationShared {
    /// Whether the worker asks `0f` on each turn of its loop.
    watching: bool,
    /// The application the last start named, which `0f` asks about.
    name: Option<String>,
    outcome: StartOutcome,
}

/// The configured application's start outcome, shared: the runtime says
/// whether to follow it and reads it, the authority worker writes it.
#[derive(Clone, Default)]
pub(crate) struct Application(Arc<Mutex<ApplicationShared>>);

impl Application {
    pub(crate) fn watch(&self, watching: bool) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .watching = watching;
    }

    pub(crate) fn outcome(&self) -> StartOutcome {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .outcome
    }

    /// The application to ask `0f` about, while watched.
    fn followed(&self) -> Option<String> {
        let shared = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        shared.name.clone().filter(|_| shared.watching)
    }

    /// A `0d` for `name` was answered: a new start, with nothing read
    /// about it yet.
    fn started(&self, name: &str, refused: bool) {
        let mut shared = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        shared.name = Some(name.to_string());
        shared.outcome = StartOutcome {
            sent: shared.outcome.sent.wrapping_add(1),
            refused,
            state: None,
        };
    }

    fn read(&self, state: UnitState) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .outcome
            .state = Some(state);
    }

    /// Asks root's `0f` about the started application while watched and
    /// keeps the answer.
    fn follow(&self, wire: &mut impl Exchange) -> Result<(), String> {
        if let Some(name) = self.followed() {
            let mut request = vec![0x0f];
            request.extend_from_slice(name.as_bytes());
            self.read(UnitState::decode(&wire.exchange(&request)?)?);
        }
        Ok(())
    }

    /// Takes a start and its outcome as the worker would.
    #[cfg(test)]
    pub(crate) fn answer(&self, refused: bool, state: Option<&[u8]>) -> Result<(), String> {
        self.started("firefox", refused);
        if let Some(state) = state {
            self.read(UnitState::decode(state)?);
        }
        Ok(())
    }

    /// A unit state read for the start already taken.
    #[cfg(test)]
    pub(crate) fn answer_state(&self, state: &[u8]) -> Result<(), String> {
        self.read(UnitState::decode(state)?);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn watched(&self) -> bool {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .watching
    }
}

/// How often the worker asks `23` again while root says a row may change
/// soon or could not read its registry.
const INSTALLED_POLL: Duration = Duration::from_secs(1);
/// The most rows `88` carries: the names root follows.
const INSTALLED_ROWS: usize = 16;

/// Root's `23` answer (td-authd/DESIGN.md, "Application requests"): which
/// of the registry's Flathub declarations are installed. A table name the
/// answer leaves out is one the image ships, always installed.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct InstalledSet {
    /// Each declaration and its state: `01` installed, `02` may change.
    rows: Vec<(String, u8)>,
    /// A live boot, which runs no application.
    live: bool,
}

impl InstalledSet {
    /// Whether `name`, a launcher table name, is installed; `None` on a
    /// live boot, where nothing runs and nothing installs.
    pub(crate) fn installed(&self, name: &str) -> Option<bool> {
        (!self.live).then(|| {
            self.rows
                .iter()
                .find(|(row, _)| row == name)
                .is_none_or(|(_, state)| state & 0x01 != 0)
        })
    }

    /// What a control `launch` may do with `name`: start it on
    /// `Some(Some(true))`, leave it to root on a live boot's `Some(None)`,
    /// refuse it on `Some(Some(false))`, and on `None` ask again, its row
    /// settling with root's state not yet installed, as after an install.
    pub(crate) fn launchable(&self, name: &str) -> Option<Option<bool>> {
        let settling = self
            .rows
            .iter()
            .any(|(row, state)| row == name && state & 0x03 == 0x02);
        (self.live || !settling).then(|| self.installed(name))
    }

    fn pending(&self) -> bool {
        self.rows.iter().any(|(_, state)| state & 0x02 != 0)
    }

    /// Exactly `88` and its rows, each a name of `table` once, or `ff 03`
    /// on a live boot; `ff 02`, root unable to read its registry, is no
    /// answer yet. Anything else ends the paired generation.
    fn decode(answer: &[u8], table: &[String]) -> Result<Option<Self>, String> {
        let invalid = || "invalid installed set answer".to_string();
        let (count, mut rest) = match answer {
            [0xff, 2] => return Ok(None),
            [0xff, 3] => {
                return Ok(Some(Self {
                    rows: Vec::new(),
                    live: true,
                }))
            }
            [0x88, count, rest @ ..] => (usize::from(*count), rest),
            _ => return Err(invalid()),
        };
        if count > INSTALLED_ROWS {
            return Err(invalid());
        }
        let mut rows: Vec<(String, u8)> = Vec::with_capacity(count);
        for _ in 0..count {
            let [state, length, tail @ ..] = rest else {
                return Err(invalid());
            };
            let name = tail
                .get(..usize::from(*length))
                .and_then(|name| std::str::from_utf8(name).ok())
                .filter(|name| consent::application_name(name))
                .filter(|name| table.iter().any(|known| known == name))
                .filter(|name| rows.iter().all(|(row, _)| row != name))
                .ok_or_else(invalid)?;
            if state & !0x03 != 0 {
                return Err(invalid());
            }
            rows.push((name.to_string(), *state));
            rest = tail.get(usize::from(*length)..).ok_or_else(invalid)?;
        }
        if !rest.is_empty() {
            return Err(invalid());
        }
        Ok(Some(Self { rows, live: false }))
    }
}

#[derive(Default)]
struct InstalledShared {
    /// The launcher table's names, which an answer may name; nothing is
    /// asked before the runtime gives them.
    table: Option<Vec<String>>,
    due: bool,
    asked: Option<Instant>,
    /// The last answer, and whether root could not read its registry since.
    set: Option<InstalledSet>,
    retry: bool,
    /// Bumped when the set changes, so the runtime redraws only then.
    changes: u64,
}

/// Root's installed set, shared: the runtime says when it is due and reads
/// it, the authority worker asks root and keeps the answer.
#[derive(Clone, Default)]
pub(crate) struct Installed(Arc<Mutex<InstalledShared>>);

impl Installed {
    /// The launcher table's names, and a first ask.
    pub(crate) fn follow_table(&self, names: Vec<String>) {
        let mut shared = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        shared.table = Some(names);
        shared.due = true;
    }

    /// Asks again on the worker's next turn: the launcher opened, or an
    /// operation started.
    pub(crate) fn ask(&self) {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).due = true;
    }

    /// The last answer and how many changes came before it.
    pub(crate) fn snapshot(&self) -> (u64, Option<InstalledSet>) {
        let shared = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        (shared.changes, shared.set.clone())
    }

    /// The table to check an answer against, when one is due at `now`. An
    /// ask is taken here, so one made while the question is out asks again.
    fn wanted(&self, now: Instant) -> Option<Vec<String>> {
        let mut shared = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let again = (shared.retry || shared.set.as_ref().is_some_and(InstalledSet::pending))
            && shared
                .asked
                .is_none_or(|asked| now.saturating_duration_since(asked) >= INSTALLED_POLL);
        if !(shared.due || again) {
            return None;
        }
        let table = shared.table.clone()?;
        shared.due = false;
        Some(table)
    }

    /// Whether an ask is due, taking it as the worker would.
    #[cfg(test)]
    pub(crate) fn take_due(&self) -> bool {
        std::mem::take(&mut self.0.lock().unwrap_or_else(PoisonError::into_inner).due)
    }

    /// Takes `answer` as the worker would have.
    #[cfg(test)]
    pub(crate) fn answer(&self, answer: &[u8]) -> Result<(), String> {
        let table = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .table
            .clone()
            .unwrap_or_default();
        if let Some(set) = InstalledSet::decode(answer, &table)? {
            let mut shared = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            shared.set = Some(set);
            shared.changes = shared.changes.wrapping_add(1);
        }
        Ok(())
    }

    /// Asks root's `23` when due and keeps the answer.
    fn follow(&self, wire: &mut impl Exchange, now: Instant) -> Result<(), String> {
        let Some(table) = self.wanted(now) else {
            return Ok(());
        };
        let answer = InstalledSet::decode(&wire.exchange(&[0x23])?, &table)?;
        let mut shared = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        shared.asked = Some(now);
        match answer {
            Some(set) => {
                shared.retry = false;
                if shared.set.as_ref() != Some(&set) {
                    shared.set = Some(set);
                    shared.changes = shared.changes.wrapping_add(1);
                }
            }
            None => shared.retry = true,
        }
        Ok(())
    }
}

#[derive(Clone)]
pub(crate) struct Launcher {
    send: SyncSender<Work>,
    /// The control socket's application requests, apart from `send`.
    control: SyncSender<CacheWork>,
    login: Login,
    hostnames: Hostnames,
    application: Application,
    installed: Installed,
    /// Root's answer at connect, which the generation's start locks on:
    /// never a later poll's.
    connected: Option<Answer>,
}

/// TOKEN-LOGIN.md's login state, as root's `1a` answer gives it
/// (td-authd/DESIGN.md, login-state amendment 1), with an enrolled
/// record's key list.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum LoginState {
    Unenrolled,
    /// One to eight fingerprints in canonical slot order.
    Enrolled(Vec<consent::Fingerprint>),
    /// The cause as its failure kind, `0a`, `0b` or `0c`, or `0d`, an
    /// unenrolled protected machine (td-authd/DESIGN.md, amendment 9).
    Unavailable(u8),
}

/// `9a`'s revocation byte (td-authd/DESIGN.md, amendment 7): whether the
/// console and SSH were brought to the reduced login state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Revocation {
    /// `00`: they were, or no check runs, as on the live medium.
    Settled,
    /// `01`: a cutover runs; `1a` is asked again every 250 ms.
    Pending,
    /// `02`: it failed and root restarts the machine; no request that
    /// takes the operation slot is sent.
    Restarting,
    /// `03`: it failed again after an automatic restart, or root could not
    /// guard one, so root holds without restarting.
    Held,
}

impl Revocation {
    fn decode(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::Settled),
            1 => Some(Self::Pending),
            2 => Some(Self::Restarting),
            3 => Some(Self::Held),
            _ => None,
        }
    }

    /// The notice the lock surface and the attention screen show, if any.
    pub(crate) fn notice(self) -> Option<&'static str> {
        match self {
            Self::Restarting => Some(crate::attention::RESTARTING_NOTICE),
            Self::Held => Some(crate::attention::HELD_NOTICE),
            Self::Settled | Self::Pending => None,
        }
    }
}

/// Root's `1a` answer: the login state, the primary username and hostname
/// the lock surface draws above the state's rows, the revocation and the
/// admission.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Answer {
    state: LoginState,
    username: String,
    /// Empty where root sent none.
    hostname: String,
    revocation: Revocation,
    /// `01`: a verified account handoff admitted this generation
    /// (td-install/ENCRYPTION.md, "Verified account handoff"). Only the
    /// generation's first answer may carry it.
    admitted: bool,
    /// `01`: root read the protected marker for this answer, or could not
    /// (td-authd/DESIGN.md, amendment 10). A display choice: it offers the
    /// disk proofs and authorizes nothing.
    protected: bool,
}

impl Answer {
    /// Exactly `9a`'s shape, or a protocol violation that ends the paired
    /// generation: a state, an enrolled list or a cause; a primary account
    /// name; an empty hostname or one under td-firstboot's rules; the
    /// revocation byte, `00` to `03`; the admission byte, `00` or `01`;
    /// and the protected byte, `00` or `01`.
    fn decode(answer: &[u8]) -> Result<Self, String> {
        let invalid = || "invalid login state answer".to_string();
        let [0x9a, state, rest @ ..] = answer else {
            return Err(invalid());
        };
        let (state, rest) = match (*state, rest) {
            (0, rest) => (LoginState::Unenrolled, rest),
            (1, [count, rest @ ..]) if (1..=consent::LOGIN_KEYS).contains(count) => {
                let (keys, rest) = rest
                    .split_at_checked(usize::from(*count) * 4)
                    .ok_or_else(invalid)?;
                let (keys, []) = keys.as_chunks::<4>() else {
                    return Err(invalid());
                };
                (LoginState::Enrolled(keys.to_vec()), rest)
            }
            (2, [cause @ 0x0a..=0x0d, rest @ ..]) => (LoginState::Unavailable(*cause), rest),
            _ => return Err(invalid()),
        };
        let [length, rest @ ..] = rest else {
            return Err(invalid());
        };
        let (username, rest) = rest
            .split_at_checked(usize::from(*length))
            .ok_or_else(invalid)?;
        let [length, rest @ ..] = rest else {
            return Err(invalid());
        };
        let (host, rest) = rest
            .split_at_checked(usize::from(*length))
            .ok_or_else(invalid)?;
        let username = std::str::from_utf8(username).map_err(|_| invalid())?;
        let host = std::str::from_utf8(host).map_err(|_| invalid())?;
        let [revocation, admitted @ (0 | 1), protected @ (0 | 1)] = rest else {
            return Err(invalid());
        };
        let revocation = Revocation::decode(*revocation).ok_or_else(invalid)?;
        if primary_account::validate_name(username).is_err() {
            return Err(invalid());
        }
        let hostname = if host.is_empty() {
            String::new()
        } else {
            hostname::Hostname::parse(host)
                .map_err(|_| invalid())?
                .name()
                .to_string()
        };
        Ok(Self {
            state,
            username: username.to_string(),
            hostname,
            revocation,
            admitted: *admitted == 1,
            protected: *protected == 1,
        })
    }

    pub(crate) fn state(&self) -> &LoginState {
        &self.state
    }

    pub(crate) fn username(&self) -> &str {
        &self.username
    }

    pub(crate) fn hostname(&self) -> &str {
        &self.hostname
    }

    pub(crate) fn revocation(&self) -> Revocation {
        self.revocation
    }

    /// Whether a generation starts locked on this answer: enrolled or
    /// unavailable, any cause; never unenrolled, which is also how root
    /// answers on the live medium (TOKEN-LOGIN.md, "Session lock").
    pub(crate) fn locks(&self) -> bool {
        !matches!(self.state, LoginState::Unenrolled)
    }

    /// Whether this, the generation's first answer, says a verified
    /// account handoff admitted the generation: its first paint is then
    /// the session, whatever the state.
    pub(crate) fn admitted(&self) -> bool {
        self.admitted
    }

    /// Whether the key-management screen offers the disk proofs.
    pub(crate) fn protected(&self) -> bool {
        self.protected
    }
}

/// Called with every answer once it is stored.
type Watcher = Arc<dyn Fn(&Answer) + Send + Sync>;

#[derive(Default)]
struct Shared {
    answer: Option<Answer>,
    watcher: Option<Watcher>,
}

/// The last `1a` answer, shared: the authority worker writes it, and the
/// input bindings and the lock surface's watcher read it, each holding
/// nothing else while they do.
#[derive(Clone, Default)]
pub(crate) struct Login(Arc<Mutex<Shared>>);

impl Login {
    /// The last answer's state; none before the first answer, and in the
    /// direct profile, which has no authority.
    pub(crate) fn state(&self) -> Option<LoginState> {
        self.current().map(|answer| answer.state)
    }

    /// The last answer, when `state` has one.
    pub(crate) fn current(&self) -> Option<Answer> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .answer
            .clone()
    }

    /// Calls `watcher` with each later answer, from the authority worker,
    /// which holds no lock then, this handle's included.
    pub(crate) fn watch(&self, watcher: impl Fn(&Answer) + Send + Sync + 'static) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .watcher = Some(Arc::new(watcher));
    }

    fn set(&self, answer: Answer) {
        let watcher = {
            let mut shared = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            shared.answer = Some(answer.clone());
            shared.watcher.clone()
        };
        if let Some(watcher) = watcher {
            watcher(&answer);
        }
    }

    /// Takes `answer` as the worker takes root's.
    #[cfg(test)]
    pub(crate) fn answer(&self, answer: &[u8]) -> Result<(), String> {
        self.set(Answer::decode(answer)?);
        Ok(())
    }
}

/// What root's hostname intake holds (td-authd/DESIGN.md, "Elevation
/// operations"), as its `9f` answer says.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum Queue {
    #[default]
    Empty,
    /// An admitted request waits for `H`.
    Waiting,
    /// Root cannot read or write the backoff, so the intake refuses.
    Refusing,
}

/// Root's `9f` answer, which the attention menu's `H` row shows: the queue
/// and the intake's consecutive unapproved requests.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct HostnameIntake {
    queue: Queue,
    denied: u8,
}

impl HostnameIntake {
    /// Exactly `9f`, the queue and the count, which is `00` while the
    /// intake refuses; anything else ends the paired generation.
    pub(crate) fn decode(answer: &[u8]) -> Result<Self, String> {
        let (queue, denied) = match answer {
            [0x9f, 0, denied] => (Queue::Empty, *denied),
            [0x9f, 1, denied] => (Queue::Waiting, *denied),
            [0x9f, 2, 0] => (Queue::Refusing, 0),
            _ => return Err("invalid hostname intake answer".into()),
        };
        Ok(Self { queue, denied })
    }

    pub(crate) fn queue(self) -> Queue {
        self.queue
    }

    pub(crate) fn denied(self) -> u8 {
        self.denied
    }
}

/// The last `9f` answer, shared: the authority worker writes it after each
/// turn of its loop, and the attention screen reads it as its menu opens.
#[derive(Clone, Default)]
pub(crate) struct Hostnames(Arc<Mutex<HostnameIntake>>);

impl Hostnames {
    /// The last answer; the empty queue before the first, and in the
    /// direct profile, which has no authority.
    pub(crate) fn current(&self) -> HostnameIntake {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn set(&self, intake: HostnameIntake) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = intake;
    }

    /// Takes `answer` as the worker takes root's.
    #[cfg(test)]
    pub(crate) fn answer(&self, answer: &[u8]) -> Result<(), String> {
        self.set(HostnameIntake::decode(answer)?);
        Ok(())
    }

    /// Asks root's `1f` and keeps the answer.
    fn follow(&self, wire: &mut impl Exchange) -> Result<(), String> {
        self.set(HostnameIntake::decode(&wire.exchange(&[0x1f])?)?);
        Ok(())
    }
}

/// When the worker asks `1a`: at connect, after every login operation's
/// end, and every 250 ms while the state could not be read, so a transient
/// helper failure resolves without a reboot, or while a revocation is
/// pending, so its outcome is shown and a failure's refusal of the slot
/// is known.
struct LoginPoll {
    login: Login,
    /// When to ask again; none unless the state could not be read or a
    /// revocation is pending.
    next: Option<Instant>,
    /// Whether the generation's first answer has been read.
    answered: bool,
}

impl LoginPoll {
    fn new(login: Login) -> Self {
        Self {
            login,
            next: None,
            answered: false,
        }
    }

    fn read(&mut self, wire: &mut impl Exchange) -> Result<Answer, String> {
        let answer = Answer::decode(&wire.exchange(&[0x1a])?)?;
        // Only the connect-time answer may admit; a later one that says
        // so is a protocol violation.
        if answer.admitted && self.answered {
            return Err("login state answer admits after the first".into());
        }
        self.answered = true;
        self.next = if answer.state == LoginState::Unavailable(UNREADABLE)
            || answer.revocation == Revocation::Pending
        {
            Some(
                Instant::now()
                    .checked_add(LOGIN_POLL)
                    .ok_or("login state poll overflow")?,
            )
        } else {
            None
        };
        self.login.set(answer.clone());
        Ok(answer)
    }

    /// After the secret client's tick: `1a` again when a login operation
    /// ended in it, or when the poll is due.
    fn follow(
        &mut self,
        wire: &mut impl Exchange,
        secrets: &mut crate::secret_client::Client,
    ) -> Result<(), String> {
        if secrets.take_login_end() || self.next.is_some_and(|next| Instant::now() >= next) {
            return self.read(wire).map(drop);
        }
        Ok(())
    }

    /// The last answer's revocation; settled before the first.
    fn revocation(&self) -> Revocation {
        self.login
            .current()
            .map_or(Revocation::Settled, |answer| answer.revocation)
    }

    /// How long the worker may wait for work before it is due.
    fn wait(&self) -> Duration {
        self.next.map_or(TICK, |next| {
            next.saturating_duration_since(Instant::now()).min(TICK)
        })
    }
}

impl Launcher {
    /// Must precede every compositor thread, child, and descriptor delegation.
    pub fn connect() -> Result<Self, String> {
        startup()?;
        let mut wire = channel::Channel::from_stdin(0).map_err(|e| e.to_string())?;
        if wire.receive().map_err(|e| e.to_string())? != VERSION {
            return Err("unsupported program authority protocol".into());
        }
        wire.send(VERSION).map_err(|e| e.to_string())?;
        if wire.receive().map_err(|e| e.to_string())? != [0x80] {
            return Err("program authority refused session admission".into());
        }
        let login = Login::default();
        let (poll, connected) = open_session(&mut wire, &login)?;
        let hostnames = Hostnames::default();
        let followed = hostnames.clone();
        let application = Application::default();
        let started = application.clone();
        let installed = Installed::default();
        let asked = installed.clone();
        let (send, receive) = mpsc::sync_channel(QUEUE_CAPACITY);
        let (control, requests) = mpsc::sync_channel(QUEUE_CAPACITY);
        std::thread::Builder::new()
            .name("terminal-authority".into())
            .spawn(move || {
                let shared = (followed, started, asked);
                if let Err(error) = worker(wire, receive, requests, poll, shared) {
                    let _ = writeln!(
                        std::io::stderr().lock(),
                        "td-compositor: program authority: {error}"
                    );
                }
                // Losing either peer must end this paired service generation.
                std::process::exit(1);
            })
            .map_err(|e| format!("start program authority worker: {e}"))?;
        Ok(Self {
            send,
            control,
            login,
            hostnames,
            application,
            installed,
            connected: Some(connected),
        })
    }

    /// Root's installed set, which the worker asks for when the runtime
    /// says it is due.
    pub(crate) fn installed(&self) -> Installed {
        self.installed.clone()
    }

    /// What became of the application's start requests, which the worker
    /// keeps current while it is watched.
    pub(crate) fn application(&self) -> Application {
        self.application.clone()
    }

    /// The login state root last answered, which the worker keeps current.
    pub fn login(&self) -> Login {
        self.login.clone()
    }

    /// What root's hostname intake holds, which the worker keeps current.
    pub(crate) fn hostnames(&self) -> Hostnames {
        self.hostnames.clone()
    }

    /// Root's answer at connect, which no later answer replaces.
    pub(crate) fn connected(&self) -> Option<Answer> {
        self.connected.clone()
    }

    pub fn unlock(
        &self,
        attempt: std::sync::Arc<crate::secret_client::Attempt>,
    ) -> Result<(), String> {
        match self.send.try_send(Work::Secret(attempt)) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err("authority request already pending".into()),
            Err(TrySendError::Disconnected(_)) => Err("secret authority unavailable".into()),
        }
    }

    pub fn launch(&self) -> Result<(), String> {
        match self.send.try_send(Work::Program(Program::Home)) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err("program launch is already pending".into()),
            Err(TrySendError::Disconnected(_)) => Err("program authority is unavailable".into()),
        }
    }

    /// Queues an application request for `name`, already in the
    /// registry's grammar; root's answer arrives on the receiver.
    pub(crate) fn cache(
        &self,
        verb: CacheVerb,
        name: &str,
    ) -> Result<Receiver<CacheAnswer>, String> {
        if !consent::application_name(name) {
            return Err("invalid application name".into());
        }
        let (reply, answer) = mpsc::sync_channel(1);
        match self.control.try_send((verb, name.to_string(), reply)) {
            Ok(()) => Ok(answer),
            Err(TrySendError::Full(_)) => Err("an application request is already pending".into()),
            Err(TrySendError::Disconnected(_)) => Err("program authority is unavailable".into()),
        }
    }

    pub fn supervise(&self, request: Supervised) -> Result<(), String> {
        // Root ends the generation on a malformed name; refused here.
        if matches!(&request, Supervised::Start(name) if !consent::application_name(name)) {
            return Err("invalid application name".into());
        }
        match self.send.try_send(Work::Supervised(request)) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err("authority request already pending".into()),
            Err(TrySendError::Disconnected(_)) => Err("program authority is unavailable".into()),
        }
    }

    pub fn launch_selected(&self, terminal: Program) -> Result<(), String> {
        match self.send.try_send(Work::Program(terminal)) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err("program launch is already pending".into()),
            Err(TrySendError::Disconnected(_)) => Err("program authority is unavailable".into()),
        }
    }

    /// Queues td-software, opened at `name`, already in the registry's
    /// grammar.
    pub(crate) fn launch_software_at(&self, name: &str) -> Result<(), String> {
        if !consent::application_name(name) {
            return Err("invalid application name".into());
        }
        match self.send.try_send(Work::SoftwareAt(name.to_string())) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err("program launch is already pending".into()),
            Err(TrySendError::Disconnected(_)) => Err("program authority is unavailable".into()),
        }
    }
}

/// A launcher whose secret attempts wait in a queue a test takes them
/// from, playing the worker itself.
#[cfg(test)]
pub(crate) struct Queued(Receiver<Work>, Receiver<CacheWork>);

#[cfg(test)]
impl Queued {
    pub(crate) fn launcher() -> (Launcher, Self) {
        let (send, receive) = mpsc::sync_channel(QUEUE_CAPACITY);
        let (control, requests) = mpsc::sync_channel(QUEUE_CAPACITY);
        let login = Login::default();
        (
            Launcher {
                send,
                control,
                login,
                hostnames: Hostnames::default(),
                application: Application::default(),
                installed: Installed::default(),
                connected: None,
            },
            Self(receive, requests),
        )
    }

    pub(crate) fn supervised(&self) -> Option<Supervised> {
        match self.0.try_recv() {
            Ok(Work::Supervised(request)) => Some(request),
            _ => None,
        }
    }

    /// The td-software selection the queue holds, if that is what it holds.
    pub(crate) fn software_at(&self) -> Option<String> {
        match self.0.try_recv() {
            Ok(Work::SoftwareAt(name)) => Some(name),
            _ => None,
        }
    }

    /// An application request, waiting up to `wait`, with where its
    /// answer goes.
    pub(crate) fn cache(&self, wait: Duration) -> Option<CacheWork> {
        self.1.recv_timeout(wait).ok()
    }

    pub(crate) fn attempt(&self) -> Option<std::sync::Arc<crate::secret_client::Attempt>> {
        match self.0.try_recv() {
            Ok(Work::Secret(attempt)) => Some(attempt),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Program {
    Home,
    Task,
    TaskManager,
    Editor,
    Photo,
    Review,
    Dua,
    Agent,
    Software,
}

fn startup() -> Result<(), String> {
    let mut status = String::new();
    File::open("/proc/self/status")
        .map_err(|e| e.to_string())?
        .take(8193)
        .read_to_string(&mut status)
        .map_err(|e| e.to_string())?;
    check_status(&status)?;
    audit_descriptors(
        |fd| {
            std::fs::metadata(format!("/proc/self/fd/{fd}"))
                .map(|metadata| (metadata.dev(), metadata.ino()))
                .map_err(|e| format!("missing authority standard descriptor {fd}: {e}"))
        },
        || {
            std::fs::read_dir("/proc/self/fd")
                .map_err(|e| e.to_string())?
                .take(5)
                .map(|entry| {
                    entry
                        .map_err(|e| e.to_string())?
                        .file_name()
                        .to_str()
                        .ok_or("invalid descriptor name")?
                        .parse::<u32>()
                        .map_err(|_| "invalid descriptor number".into())
                })
                .collect()
        },
    )
}

fn audit_descriptors(
    mut identity: impl FnMut(u32) -> Result<(u64, u64), String>,
    enumerate: impl FnOnce() -> Result<Vec<u32>, String>,
) -> Result<(), String> {
    // Check before read_dir can occupy a closed standard descriptor.
    let channel = identity(0)?;
    for fd in [1, 2] {
        if identity(fd)? == channel {
            return Err("compositor log aliases its private authority endpoint".into());
        }
    }
    let mut descriptors = enumerate()?;
    descriptors.sort_unstable();
    if descriptors != [0, 1, 2, 3] {
        return Err("compositor requires only standard authority descriptors".into());
    }
    Ok(())
}

fn check_status(status: &str) -> Result<(), String> {
    let columns = |key: &str| {
        status.lines().find_map(|line| {
            line.strip_prefix(key)
                .map(|value| value.split_whitespace().collect::<Vec<_>>())
        })
    };
    let uid = columns("Uid:").ok_or("missing compositor identity")?;
    let [first, second, third, fourth] = uid.as_slice() else {
        return Err("invalid compositor identity".into());
    };
    let number = first.parse::<u32>().map_err(|_| "invalid compositor uid")?;
    if status.len() > 8192
        || !(1..=999).contains(&number)
        || number.to_string() != *first
        || first != second
        || first != third
        || first != fourth
        || columns("Gid:").as_deref() != Some(uid.as_slice())
        || columns("Threads:").as_deref() != Some(["1"].as_slice())
    {
        return Err("authority compositor requires one dedicated service thread".into());
    }
    Ok(())
}

pub(crate) trait Exchange {
    /// Writes one request and returns once it is written, so a caller
    /// holding a secret in it can zero it before awaiting the answer.
    fn send(&mut self, request: &[u8]) -> Result<(), String>;
    fn receive(&mut self) -> Result<Vec<u8>, String>;

    fn exchange(&mut self, request: &[u8]) -> Result<Vec<u8>, String> {
        self.send(request)?;
        self.receive()
    }
}

impl Exchange for channel::Channel {
    fn send(&mut self, request: &[u8]) -> Result<(), String> {
        channel::Channel::send(self, request).map_err(|e| e.to_string())
    }

    fn receive(&mut self) -> Result<Vec<u8>, String> {
        channel::Channel::receive(self).map_err(|e| e.to_string())
    }
}

/// Complete prior-generation cleanup before any device or input admission.
fn prepare_session(wire: &mut impl Exchange) -> Result<(), String> {
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(5))
        .ok_or("secret session preparation deadline overflow")?;
    if wire.exchange(&[0x10])? != [0x90] {
        return Err("authority refused secret session preparation".into());
    }
    loop {
        let response = wire.exchange(&[0x11])?;
        if Instant::now() >= deadline {
            return Err("secret session preparation expired".into());
        }
        match response.as_slice() {
            [0x91, 2] => return Ok(()),
            [0x91, 1] => std::thread::sleep(Duration::from_millis(10)),
            _ => return Err("invalid secret session preparation response".into()),
        }
    }
}

/// Prepare, then the login state, both before the first repaint and any
/// input admission: the poll, and the answer at connect.
fn open_session(wire: &mut impl Exchange, login: &Login) -> Result<(LoginPoll, Answer), String> {
    prepare_session(wire)?;
    let mut poll = LoginPoll::new(login.clone());
    let connected = poll.read(wire)?;
    Ok((poll, connected))
}

struct Processes {
    handles: VecDeque<u64>,
    latest: u64,
}

impl Processes {
    fn new() -> Self {
        Self {
            handles: VecDeque::with_capacity(CAPACITY),
            latest: 0,
        }
    }

    fn start(
        &mut self,
        wire: &mut impl Exchange,
        terminal: Program,
    ) -> Result<Option<&'static str>, String> {
        let request = match terminal {
            Program::Home => 1,
            Program::Task => 4,
            Program::TaskManager => 7,
            Program::Editor => 8,
            Program::Photo => 9,
            Program::Review => 0x0a,
            Program::Dua => 0x0b,
            Program::Agent => 0x0c,
            Program::Software => 0x22,
        };
        self.request(wire, &[request])
    }

    /// td-software, opened at `name`: `22` and the name.
    fn start_software_at(
        &mut self,
        wire: &mut impl Exchange,
        name: &str,
    ) -> Result<Option<&'static str>, String> {
        self.request(wire, &[&[0x22][..], name.as_bytes()].concat())
    }

    fn request(
        &mut self,
        wire: &mut impl Exchange,
        request: &[u8],
    ) -> Result<Option<&'static str>, String> {
        if self.handles.len() >= CAPACITY {
            return Ok(Some("program launch limit reached"));
        }
        match wire.exchange(request)?.as_slice() {
            [0xff, 1] => Ok(Some("program authority process table is full")),
            [0xff, 2] => Ok(Some("program authority could not start the helper")),
            [0x81, handle @ ..] if handle.len() == 8 => {
                let handle = u64::from_be_bytes(handle.try_into().map_err(|_| "invalid handle")?);
                if handle <= self.latest {
                    return Err("program authority reused a process handle".into());
                }
                self.latest = handle;
                self.handles.push_back(handle);
                Ok(None)
            }
            _ => Err("invalid program authority start response".into()),
        }
    }

    fn poll(&mut self, wire: &mut impl Exchange) -> Result<Option<&'static str>, String> {
        let Some(handle) = self.handles.pop_front() else {
            if wire.exchange(&[3])? != [0x83] {
                return Err("invalid program authority heartbeat".into());
            }
            return Ok(None);
        };
        let mut request = [0u8; 9];
        if let Some(first) = request.first_mut() {
            *first = 2;
        }
        request
            .get_mut(1..)
            .ok_or("invalid poll buffer")?
            .copy_from_slice(&handle.to_be_bytes());
        match wire.exchange(&request)?.as_slice() {
            [0x82, 0] => {
                self.handles.push_back(handle);
                Ok(None)
            }
            [0x82, 1] => Ok(None),
            [0x82, 2] => Ok(Some("launched program failed")),
            _ => Err("invalid program authority process status".into()),
        }
    }
}

/// One supervised request: root starts the unit and answers at once; a
/// start's outcome is followed with `0f` while the runtime watches it.
fn supervise(
    wire: &mut impl Exchange,
    request: Supervised,
    application: &Application,
) -> Result<Option<&'static str>, String> {
    let Supervised::Start(name) = &request;
    let bytes = [&[0x0d][..], name.as_bytes()].concat();
    let (refused, error) = match wire.exchange(&bytes)?.as_slice() {
        [0x84] => (false, None),
        // An earlier start is still being asked, which this one joins.
        [0xff, 1] => (
            false,
            Some("the application's supervisor is already being asked"),
        ),
        [0xff, 2] => (true, Some("program authority could not ask the supervisor")),
        [0xff, 3] => (true, Some("a live session starts only the installer")),
        [0xff, 4] => (true, Some("the image declares no such application")),
        [0xff, 5] => (true, Some("root has as many starts in flight as it allows")),
        _ => return Err("invalid supervised request response".into()),
    };
    application.started(name, refused);
    Ok(error)
}

/// One application request: root answers at once, an operation running on
/// as root's own process.
fn cache(wire: &mut impl Exchange, verb: CacheVerb, name: &str) -> Result<CacheAnswer, String> {
    let mut request = vec![0x24, verb.byte()];
    request.extend_from_slice(name.as_bytes());
    CacheAnswer::decode(verb, &wire.exchange(&request)?)
}

fn worker(
    mut wire: impl Exchange,
    receive: Receiver<Work>,
    requests: Receiver<CacheWork>,
    mut login: LoginPoll,
    (hostnames, application, installed): (Hostnames, Application, Installed),
) -> Result<(), String> {
    let mut processes = Processes::new();
    let mut secrets = crate::secret_client::Client::default();
    loop {
        let work = receive.recv_timeout(login.wait());
        secrets.set_revocation(login.revocation());
        match work {
            Ok(Work::Secret(attempt)) => secrets.start(&mut wire, attempt)?,
            Ok(Work::Supervised(request)) => {
                if let Some(error) = supervise(&mut wire, request, &application)? {
                    let _ = writeln!(std::io::stderr().lock(), "td-compositor: {error}");
                }
            }
            Ok(Work::Program(terminal)) => {
                if let Some(error) = processes.start(&mut wire, terminal)? {
                    let _ = writeln!(std::io::stderr().lock(), "td-compositor: {error}");
                }
            }
            Ok(Work::SoftwareAt(name)) => {
                if let Some(error) = processes.start_software_at(&mut wire, &name)? {
                    let _ = writeln!(std::io::stderr().lock(), "td-compositor: {error}");
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Err("launcher disconnected".into()),
        }
        // At most one control-socket request a turn, after physical work.
        if let Ok((verb, name, reply)) = requests.try_recv() {
            let answer = cache(&mut wire, verb, &name)?;
            // An operation that started changes what is installed.
            if answer == CacheAnswer::Started {
                installed.ask();
            }
            // A caller that stopped waiting is no failure of the channel.
            let _ = reply.try_send(answer);
        }
        if let Some(error) = processes.poll(&mut wire)? {
            let _ = writeln!(std::io::stderr().lock(), "td-compositor: {error}");
        }
        secrets.tick(&mut wire)?;
        login.follow(&mut wire, &mut secrets)?;
        hostnames.follow(&mut wire)?;
        application.follow(&mut wire)?;
        installed.follow(&mut wire, Instant::now())?;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    struct Wire {
        requests: Vec<Vec<u8>>,
        answers: VecDeque<Vec<u8>>,
    }
    impl Exchange for Wire {
        fn send(&mut self, request: &[u8]) -> Result<(), String> {
            self.requests.push(request.to_vec());
            Ok(())
        }
        fn receive(&mut self) -> Result<Vec<u8>, String> {
            self.answers.pop_front().ok_or("unexpected request".into())
        }
    }
    fn wire(answers: Vec<Vec<u8>>) -> Wire {
        Wire {
            requests: Vec::new(),
            answers: answers.into(),
        }
    }
    /// A control queue nothing will ever send on.
    fn idle() -> Receiver<CacheWork> {
        mpsc::sync_channel(QUEUE_CAPACITY).1
    }
    fn handle(number: u64) -> Vec<u8> {
        let mut bytes = vec![0x81];
        bytes.extend_from_slice(&number.to_be_bytes());
        bytes
    }

    #[test]
    fn preparation_waits_for_cleanup_and_never_retries_uncertain_requests() {
        let mut pending = wire(vec![vec![0x90], vec![0x91, 1], vec![0x91, 2]]);
        prepare_session(&mut pending).unwrap();
        assert_eq!(pending.requests, [vec![0x10], vec![0x11], vec![0x11]]);
        for replies in [
            vec![vec![0x90, 0]],
            vec![vec![0x90], vec![0x91, 0]],
            vec![vec![0x90], vec![0x91, 3]],
            vec![vec![0x90], vec![0x91, 2, 0]],
            vec![],
        ] {
            let mut refused = wire(replies);
            assert!(prepare_session(&mut refused).is_err());
            assert_eq!(
                refused
                    .requests
                    .iter()
                    .filter(|request| **request == [0x10])
                    .count(),
                1
            );
        }
    }

    /// `9a` for `state`, with a primary name and a hostname.
    fn login_state(state: &[u8]) -> Vec<u8> {
        revoked(state, 0)
    }

    /// `9a` for `state` with the revocation byte `revocation`, not
    /// admitted.
    fn revoked(state: &[u8], revocation: u8) -> Vec<u8> {
        [
            &[0x9a][..],
            state,
            b"\x06tester\x09td-laptop",
            &[revocation, 0, 0],
        ]
        .concat()
    }

    /// `9a` for `state` with the admission byte `01`.
    fn admitting(state: &[u8]) -> Vec<u8> {
        let mut answer = login_state(state);
        let at = answer.len() - 2;
        answer[at] = 1;
        answer
    }

    /// `9a` for `state` with the protected byte `01`.
    fn protecting(state: &[u8]) -> Vec<u8> {
        let mut answer = login_state(state);
        if let Some(protected) = answer.last_mut() {
            *protected = 1;
        }
        answer
    }

    fn keys(count: u8) -> Vec<u8> {
        let mut bytes = vec![1, count];
        for key in 1..=count {
            bytes.extend_from_slice(&[key; 4]);
        }
        bytes
    }

    #[test]
    fn a_login_state_answer_has_exactly_one_shape() {
        let decode = |bytes: &[u8]| Answer::decode(bytes).map(|answer| answer.state);
        assert_eq!(decode(&login_state(&[0])), Ok(LoginState::Unenrolled));
        for cause in [0x0a, 0x0b, 0x0c, 0x0d] {
            assert_eq!(
                decode(&login_state(&[2, cause])),
                Ok(LoginState::Unavailable(cause))
            );
        }
        for count in 1..=8 {
            assert_eq!(
                decode(&login_state(&keys(count))),
                Ok(LoginState::Enrolled(
                    (1..=count).map(|key| [key; 4]).collect()
                ))
            );
        }
        // Two keys may share a fingerprint; positions keep them apart.
        assert_eq!(
            decode(&login_state(&[&[1, 2][..], &[7; 8]].concat())),
            Ok(LoginState::Enrolled(vec![[7; 4], [7; 4]]))
        );
        // The longest names, and no hostname at all.
        let longest = [
            &[0x9a, 0, 32][..],
            &[b'a'; 32],
            &[63],
            &[b'h'; 63],
            &[0, 0, 0],
        ]
        .concat();
        assert_eq!(decode(&longest), Ok(LoginState::Unenrolled));
        assert_eq!(
            decode(b"\x9a\x00\x01a\x00\x00\x00\x00"),
            Ok(LoginState::Unenrolled)
        );
        // The names are kept for the lock surface, as root sent them.
        let kept = Answer::decode(&login_state(&[0])).unwrap();
        assert_eq!((kept.username(), kept.hostname()), ("tester", "td-laptop"));
        let kept = Answer::decode(&longest).unwrap();
        assert_eq!(kept.username(), "a".repeat(32));
        assert_eq!(kept.hostname(), "h".repeat(63));
        assert_eq!(
            Answer::decode(b"\x9a\x00\x01a\x00\x00\x00\x00")
                .unwrap()
                .hostname(),
            ""
        );
        let refused = [
            vec![],
            vec![0x9a],
            vec![0x9b, 0],
            login_state(&[3]),
            // Enrolled with no keys, nine, or a list cut short or long.
            login_state(&[1, 0]),
            login_state(&keys(9)),
            login_state(&[&[1, 2][..], &[7; 7]].concat()),
            login_state(&[&[1, 1][..], &[7; 5]].concat()),
            // A cause that is not the unavailable state's, or none.
            login_state(&[2, 0x09]),
            login_state(&[2, 0x0e]),
            login_state(&[2]),
            // A state byte with a stray byte after it.
            login_state(&[0, 0]),
            // Usernames: empty, too long, not a primary name, not UTF-8.
            b"\x9a\x00\x00\x00\x00\x00".to_vec(),
            [&[0x9a, 0, 33][..], &[b'a'; 33], &[0, 0, 0, 0]].concat(),
            b"\x9a\x00\x04Root\x00\x00\x00\x00".to_vec(),
            b"\x9a\x00\x041abc\x00\x00\x00\x00".to_vec(),
            b"\x9a\x00\x04a.bc\x00\x00\x00\x00".to_vec(),
            b"\x9a\x00\x02a\xff\x00\x00\x00\x00".to_vec(),
            // Hostnames: too long, against firstboot's rules, not UTF-8.
            [&[0x9a, 0, 1, b'a', 64][..], &[b'h'; 64], &[0, 0, 0]].concat(),
            b"\x9a\x00\x01a\x02TD\x00\x00\x00".to_vec(),
            b"\x9a\x00\x01a\x05host-\x00\x00\x00".to_vec(),
            b"\x9a\x00\x01a\x03a b\x00\x00\x00".to_vec(),
            b"\x9a\x00\x01a\x03td\n\x00\x00\x00".to_vec(),
            b"\x9a\x00\x01a\x02\xc3\xb6\x00\x00\x00".to_vec(),
            // Lengths that overrun the answer.
            b"\x9a\x00\x07tester".to_vec(),
            b"\x9a\x00\x06tester\x09td".to_vec(),
            // The revocation byte: missing, past `03`, or followed by more
            // than the admission and protected bytes.
            b"\x9a\x00\x06tester\x00".to_vec(),
            b"\x9a\x00\x06tester\x00\x04\x00\x00".to_vec(),
            b"\x9a\x00\x06tester\x00\xff\x00\x00".to_vec(),
            b"\x9a\x00\x06tester\x00\x00\x00\x00\x00".to_vec(),
            b"\x9a\x00\x06tester\x00\x02\x01\x00\x00".to_vec(),
            // The admission byte: missing, or past `01`.
            b"\x9a\x00\x06tester\x00\x00".to_vec(),
            b"\x9a\x00\x06tester\x00\x00\x02\x00".to_vec(),
            b"\x9a\x00\x06tester\x00\x00\xff\x00".to_vec(),
            // The protected byte (amendment 10): missing, or past `01`.
            b"\x9a\x00\x06tester\x00\x00\x00".to_vec(),
            b"\x9a\x00\x06tester\x00\x00\x00\x02".to_vec(),
            b"\x9a\x00\x06tester\x00\x00\x01\xff".to_vec(),
        ];
        for answer in refused {
            assert!(decode(&answer).is_err(), "{answer:02x?}");
        }
    }

    /// Each answer after the watch reaches the watcher once stored, with
    /// the handle's own lock released, so the watcher may read it.
    #[test]
    fn every_later_answer_reaches_the_watcher_outside_the_handles_lock() {
        let login = Login::default();
        login.answer(&login_state(&[0])).unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (watched, record) = (login.clone(), Arc::clone(&seen));
        login.watch(move |answer| {
            assert_eq!(watched.current().as_ref(), Some(answer));
            record.lock().unwrap().push(answer.state().clone());
        });
        let mut poll = LoginPoll::new(login.clone());
        let mut w = wire(vec![login_state(&[2, UNREADABLE]), login_state(&keys(1))]);
        poll.read(&mut w).unwrap();
        poll.read(&mut w).unwrap();
        assert_eq!(
            *seen.lock().unwrap(),
            [
                LoginState::Unavailable(UNREADABLE),
                LoginState::Enrolled(vec![[1; 4]])
            ]
        );
    }

    /// Enrolled and unavailable, any cause, start a generation locked;
    /// unenrolled, which is also root's every answer on the live medium,
    /// does not.
    #[test]
    fn enrolled_and_unavailable_answers_lock_and_unenrolled_does_not() {
        let locks = |state: &[u8]| Answer::decode(&login_state(state)).unwrap().locks();
        assert!(!locks(&[0]));
        for count in 1..=8 {
            assert!(locks(&keys(count)));
        }
        for cause in [0x0a, 0x0b, 0x0c, 0x0d] {
            assert!(locks(&[2, cause]));
        }
    }

    /// The admission byte is kept; it changes no state and no lock.
    #[test]
    fn the_admission_byte_is_kept_beside_the_state() {
        for state in [&[0][..], &keys(2), &[2, 0x0d], &[2, UNREADABLE]] {
            let plain = Answer::decode(&login_state(state)).unwrap();
            let admitted = Answer::decode(&admitting(state)).unwrap();
            assert!(!plain.admitted());
            assert!(admitted.admitted());
            assert_eq!(admitted.state(), plain.state());
            assert_eq!(admitted.locks(), plain.locks());
        }
        assert!(Answer::decode(b"\x9a\x00\x06tester\x00\x02\x01\x00")
            .unwrap()
            .admitted());
    }

    /// The protected byte is kept beside the state and the admission, and
    /// changes neither: a display choice.
    #[test]
    fn the_protected_byte_is_kept_beside_the_state() {
        for state in [&[0][..], &keys(2), &[2, 0x0d], &[2, UNREADABLE]] {
            let plain = Answer::decode(&login_state(state)).unwrap();
            let protected = Answer::decode(&protecting(state)).unwrap();
            assert!(!plain.protected() && protected.protected());
            assert!(!protected.admitted());
            assert_eq!(protected.state(), plain.state());
            assert_eq!(protected.locks(), plain.locks());
        }
        let both = Answer::decode(b"\x9a\x00\x06tester\x00\x00\x01\x01").unwrap();
        assert!(both.admitted() && both.protected());
    }

    /// Only the generation's first answer may admit: a later one that says
    /// so ends the paired generation, and is never stored.
    #[test]
    fn only_the_first_answer_may_admit() {
        let login = Login::default();
        let mut poll = LoginPoll::new(login.clone());
        let mut w = wire(vec![
            admitting(&keys(1)),
            login_state(&keys(1)),
            admitting(&keys(1)),
        ]);
        assert!(poll.read(&mut w).unwrap().admitted());
        assert!(!poll.read(&mut w).unwrap().admitted());
        assert!(poll.read(&mut w).is_err());
        assert!(!login.current().unwrap().admitted());
        let mut poll = LoginPoll::new(Login::default());
        let mut w = wire(vec![login_state(&[0]), admitting(&[0])]);
        poll.read(&mut w).unwrap();
        assert!(poll.read(&mut w).is_err());
    }

    /// The answer at connect is kept apart from the handle: an unreadable
    /// state that a poll reads unenrolled before the first paint still
    /// starts the generation locked.
    #[test]
    fn the_connect_answer_is_not_replaced_by_a_later_poll() {
        let login = Login::default();
        let mut w = wire(vec![
            vec![0x90],
            vec![0x91, 2],
            login_state(&[2, UNREADABLE]),
            login_state(&[0]),
        ]);
        let (mut poll, connected) = open_session(&mut w, &login).unwrap();
        poll.next = Some(Instant::now());
        poll.follow(&mut w, &mut crate::secret_client::Client::default())
            .unwrap();
        assert_eq!(login.state(), Some(LoginState::Unenrolled));
        assert_eq!(connected.state(), &LoginState::Unavailable(UNREADABLE));
        assert!(connected.locks());
    }

    #[test]
    fn connect_asks_the_login_state_once_prepared_and_refuses_a_malformed_one() {
        let login = Login::default();
        let mut pending = wire(vec![
            vec![0x90],
            vec![0x91, 1],
            vec![0x91, 2],
            login_state(&keys(2)),
        ]);
        let (poll, connected) = open_session(&mut pending, &login).unwrap();
        assert_eq!(
            pending.requests,
            [vec![0x10], vec![0x11], vec![0x11], vec![0x1a]]
        );
        assert_eq!(
            login.state(),
            Some(LoginState::Enrolled(vec![[1; 4], [2; 4]]))
        );
        assert!(poll.next.is_none());
        assert_eq!(Some(connected), login.current());
        // Nothing is asked before preparation completes.
        let mut unprepared = wire(vec![vec![0x90], vec![0x91, 0]]);
        assert!(open_session(&mut unprepared, &Login::default()).is_err());
        assert!(!unprepared.requests.contains(&vec![0x1a]));
        for answer in [vec![0x9a], revoked(&[0], 4), vec![0x91, 2]] {
            let login = Login::default();
            let mut refused = wire(vec![vec![0x90], vec![0x91, 2], answer]);
            assert!(open_session(&mut refused, &login).is_err());
            assert_eq!(login.state(), None);
        }
    }

    #[test]
    fn an_unreadable_state_is_asked_again_every_250_ms_until_it_resolves() {
        let login = Login::default();
        let mut poll = LoginPoll::new(login.clone());
        let mut client = crate::secret_client::Client::default();
        let mut w = wire(vec![login_state(&[2, UNREADABLE])]);
        poll.read(&mut w).unwrap();
        assert_eq!(login.state(), Some(LoginState::Unavailable(UNREADABLE)));
        let due = poll.next.unwrap();
        assert!(due > Instant::now() && due <= Instant::now() + LOGIN_POLL);
        assert!(poll.wait() <= LOGIN_POLL);
        // Not yet due: nothing is asked.
        poll.follow(&mut w, &mut client).unwrap();
        assert_eq!(w.requests.len(), 1);
        // Due: asked again, still unreadable, and due again later.
        poll.next = Some(Instant::now());
        assert_eq!(poll.wait(), Duration::ZERO);
        w.answers.push_back(login_state(&[2, UNREADABLE]));
        poll.follow(&mut w, &mut client).unwrap();
        assert_eq!(w.requests, [vec![0x1a], vec![0x1a]]);
        assert!(poll.next.unwrap() > Instant::now());
        // Resolved: no more polling.
        poll.next = Some(Instant::now());
        w.answers.push_back(login_state(&keys(1)));
        poll.follow(&mut w, &mut client).unwrap();
        assert_eq!(login.state(), Some(LoginState::Enrolled(vec![[1; 4]])));
        assert_eq!(poll.next, None);
        assert_eq!(poll.wait(), TICK);
        poll.follow(&mut w, &mut client).unwrap();
        assert_eq!(w.requests.len(), 3);
        // Other unavailable causes are not transient: no polling.
        for cause in [0x0a, 0x0b] {
            w.answers.push_back(login_state(&[2, cause]));
            poll.read(&mut w).unwrap();
            assert_eq!(poll.next, None);
        }
    }

    /// The revocation byte, each of `00` to `03` with every state, is kept
    /// with the answer; `02` and `03` carry their notices.
    #[test]
    fn each_revocation_byte_is_kept_with_its_notice() {
        for (byte, revocation, notice) in [
            (0, Revocation::Settled, None),
            (1, Revocation::Pending, None),
            (
                2,
                Revocation::Restarting,
                Some("A CONSOLE OR SSH SESSION COULD NOT BE CLOSED: RESTARTING"),
            ),
            (
                3,
                Revocation::Held,
                Some("SESSION REVOCATION FAILED AFTER A RESTART"),
            ),
        ] {
            for state in [vec![0], keys(2), vec![2, UNREADABLE]] {
                let answer = Answer::decode(&revoked(&state, byte)).unwrap();
                assert_eq!(answer.revocation(), revocation);
                assert_eq!(answer.revocation().notice(), notice);
                assert_eq!(
                    answer.state(),
                    Answer::decode(&login_state(&state)).unwrap().state()
                );
            }
        }
    }

    /// `01` is asked again every 250 ms until it settles, fails or holds;
    /// `02` and `03` are final for the generation and are not polled.
    #[test]
    fn a_pending_revocation_is_asked_again_every_250_ms_until_it_resolves() {
        let login = Login::default();
        let mut poll = LoginPoll::new(login.clone());
        let mut client = crate::secret_client::Client::default();
        assert_eq!(poll.revocation(), Revocation::Settled);
        let mut w = wire(vec![revoked(&[0], 1)]);
        poll.read(&mut w).unwrap();
        assert_eq!(poll.revocation(), Revocation::Pending);
        let due = poll.next.unwrap();
        assert!(due > Instant::now() && due <= Instant::now() + LOGIN_POLL);
        assert!(poll.wait() <= LOGIN_POLL);
        poll.follow(&mut w, &mut client).unwrap();
        assert_eq!(w.requests.len(), 1, "not yet due");
        for (byte, polled) in [(1, true), (0, false), (1, true), (2, false)] {
            poll.next = Some(Instant::now());
            w.answers.push_back(revoked(&[0], byte));
            poll.follow(&mut w, &mut client).unwrap();
            assert_eq!(poll.next.is_some(), polled, "{byte}");
        }
        assert_eq!(poll.revocation(), Revocation::Restarting);
        assert_eq!(poll.wait(), TICK);
        poll.follow(&mut w, &mut client).unwrap();
        assert_eq!(w.requests.len(), 5);
        // Held, and an enrolled state, poll no more either.
        w.answers.push_back(revoked(&keys(1), 3));
        poll.read(&mut w).unwrap();
        assert_eq!((poll.next, poll.revocation()), (None, Revocation::Held));
        // An unreadable state polls whatever the byte.
        w.answers.push_back(revoked(&[2, UNREADABLE], 3));
        poll.read(&mut w).unwrap();
        assert!(poll.next.is_some());
    }

    #[test]
    fn the_login_state_is_asked_after_every_login_operation_ends() {
        let login = Login::default();
        let mut poll = LoginPoll::new(login.clone());
        let mut w = wire(vec![login_state(&[0])]);
        let mut ended = crate::secret_client::Client::with_login_ended();
        poll.follow(&mut w, &mut ended).unwrap();
        assert_eq!(w.requests, [vec![0x1a]]);
        assert_eq!(login.state(), Some(LoginState::Unenrolled));
        // Once only.
        poll.follow(&mut w, &mut ended).unwrap();
        assert_eq!(w.requests.len(), 1);
    }

    #[test]
    fn startup_refuses_human_root_partial_and_multithreaded_identities() {
        let status = "Uid:\t993\t993\t993\t993\nGid:\t993\t993\t993\t993\nThreads:\t1\n";
        assert!(check_status(status).is_ok());
        for invalid in [
            status.replace("993", "1000"),
            status.replace("993", "0"),
            status.replace("Threads:\t1", "Threads:\t2"),
            status.replacen("993", "992", 1),
            status.replace("Gid:", "Absent:"),
            format!("{status}{}", "x".repeat(8192)),
        ] {
            assert!(check_status(&invalid).is_err());
        }
    }

    #[test]
    fn descriptor_admission_checks_holes_and_aliases_before_enumeration() {
        for missing in [0, 1, 2] {
            let enumerated = std::cell::Cell::new(false);
            assert!(audit_descriptors(
                |fd| if fd == missing {
                    Err("closed".into())
                } else {
                    Ok((1, fd as u64))
                },
                || {
                    enumerated.set(true);
                    Ok(vec![0, 1, 2, 3])
                },
            )
            .is_err());
            assert!(!enumerated.get());
        }
        for alias in [1, 2] {
            assert!(audit_descriptors(
                |fd| Ok((1, if fd == alias { 0 } else { fd as u64 })),
                || panic!("alias must fail before enumeration"),
            )
            .is_err());
        }
        assert!(audit_descriptors(|fd| Ok((1, fd as u64)), || Ok(vec![3, 2, 0, 1])).is_ok());
        for descriptors in [vec![0, 1, 2], vec![0, 1, 2, 4], vec![0, 1, 2, 3, 4]] {
            assert!(audit_descriptors(|fd| Ok((1, fd as u64)), || Ok(descriptors)).is_err());
        }
    }

    #[test]
    fn polls_rotate_live_handles_and_retire_each_completion_once() {
        let mut p = Processes::new();
        let mut w = wire(vec![
            handle(1),
            handle(3),
            vec![0x82, 0],
            vec![0x82, 2],
            vec![0x82, 1],
            vec![0x83],
        ]);
        assert_eq!(p.start(&mut w, Program::Home).unwrap(), None);
        assert_eq!(p.start(&mut w, Program::Task).unwrap(), None);
        assert_eq!(w.requests[0], [1]);
        assert_eq!(w.requests[1], [4]);
        assert_eq!(p.poll(&mut w).unwrap(), None);
        assert_eq!(p.poll(&mut w).unwrap(), Some("launched program failed"));
        assert_eq!(p.poll(&mut w).unwrap(), None);
        assert_eq!(p.poll(&mut w).unwrap(), None);
        assert_eq!(w.requests[2], [2, 0, 0, 0, 0, 0, 0, 0, 1]);
        assert_eq!(w.requests[3], [2, 0, 0, 0, 0, 0, 0, 0, 3]);
        assert_eq!(w.requests[4], w.requests[2]);
        assert_eq!(w.requests[5], [3]);
    }

    #[test]
    fn selected_programs_send_only_their_fixed_authority_request() {
        for (terminal, expected) in [
            (Program::TaskManager, 7),
            (Program::Editor, 8),
            (Program::Photo, 9),
            (Program::Review, 0x0a),
            (Program::Dua, 0x0b),
            (Program::Agent, 0x0c),
            (Program::Software, 0x22),
        ] {
            let mut processes = Processes::new();
            let mut wire = wire(vec![handle(1)]);
            assert_eq!(processes.start(&mut wire, terminal).unwrap(), None);
            assert_eq!(wire.requests, [vec![expected]]);
        }
    }

    #[test]
    fn invalid_and_reused_handles_are_fatal_but_capacity_is_bounded() {
        for answer in [
            handle(0),
            vec![0x81],
            vec![0x81, 1],
            vec![0xff, 3],
            vec![0xff, 1, 0],
        ] {
            assert!(Processes::new()
                .start(&mut wire(vec![answer]), Program::Home)
                .is_err());
        }
        let mut p = Processes::new();
        let mut w = wire(vec![handle(1), vec![0x82, 1], handle(1)]);
        p.start(&mut w, Program::Home).unwrap();
        p.poll(&mut w).unwrap();
        assert!(p.start(&mut w, Program::Home).is_err());
        let mut p = Processes::new();
        let mut w = wire((1..=16).map(handle).collect());
        for _ in 0..16 {
            assert_eq!(p.start(&mut w, Program::Home).unwrap(), None);
        }
        assert!(p.start(&mut w, Program::Home).unwrap().is_some());
        assert_eq!(w.requests.len(), 16);
    }

    #[test]
    fn malformed_status_and_heartbeat_answers_are_fatal() {
        for status in [
            vec![],
            vec![0x82],
            vec![0x82, 3],
            vec![0x82, 0, 0],
            vec![0xff, 1],
        ] {
            let mut processes = Processes::new();
            let mut wire = wire(vec![handle(1), status]);
            processes.start(&mut wire, Program::Home).unwrap();
            assert!(processes.poll(&mut wire).is_err());
        }
        for status in [vec![], vec![0x83, 0], vec![0x82, 1]] {
            assert!(Processes::new().poll(&mut wire(vec![status])).is_err());
        }
        for response in [vec![0xff, 1], vec![0xff, 2]] {
            let mut processes = Processes::new();
            assert!(processes
                .start(&mut wire(vec![response]), Program::Home)
                .unwrap()
                .is_some());
            assert!(processes.handles.is_empty());
        }
    }

    #[test]
    fn supervised_requests_are_one_byte_and_one_answer() {
        let application = Application::default();
        let mut started = wire(vec![vec![0x84]]);
        assert_eq!(
            supervise(&mut started, Supervised::Start("mail".into()), &application),
            Ok(None)
        );
        // `0e`, the retired update unit's request, is never sent.
        assert_eq!(started.requests, [b"\x0dmail".to_vec()]);
        // Only a refusal marks the start refused.
        assert_eq!(
            application.outcome(),
            StartOutcome {
                sent: 1,
                refused: false,
                state: None
            }
        );
        for (answer, refused) in [
            (vec![0xff, 1], false),
            (vec![0xff, 2], true),
            (vec![0xff, 3], true),
            (vec![0xff, 4], true),
            (vec![0xff, 5], true),
        ] {
            assert!(supervise(
                &mut wire(vec![answer]),
                Supervised::Start("firefox".into()),
                &application
            )
            .unwrap()
            .is_some());
            assert_eq!(application.outcome().refused, refused);
        }
        assert_eq!(application.outcome().sent, 6);
        for answer in [vec![], vec![0x84, 0], vec![0x81], vec![0xff, 6]] {
            assert!(supervise(
                &mut wire(vec![answer]),
                Supervised::Start("firefox".into()),
                &application
            )
            .is_err());
        }
        assert_eq!(application.outcome().sent, 6);
    }

    /// A watched launch's turn: the start, the heartbeat, the hostname
    /// intake, then `0f`, whose answer is kept for the start just sent.
    #[test]
    fn a_watched_worker_asks_the_unit_state_after_its_start() {
        struct Shared(std::rc::Rc<std::cell::RefCell<Wire>>);
        impl Exchange for Shared {
            fn send(&mut self, request: &[u8]) -> Result<(), String> {
                self.0.borrow_mut().send(request)
            }
            fn receive(&mut self) -> Result<Vec<u8>, String> {
                self.0.borrow_mut().receive()
            }
        }
        let script = std::rc::Rc::new(std::cell::RefCell::new(wire(vec![
            vec![0x84],
            vec![0x83],
            vec![0x9f, 0, 0],
            vec![0x85, 2],
        ])));
        let application = Application::default();
        application.watch(true);
        let (send, receive) = mpsc::sync_channel(QUEUE_CAPACITY);
        assert!(send
            .try_send(Work::Supervised(Supervised::Start("firefox".into())))
            .is_ok());
        drop(send);
        assert!(worker(
            Shared(script.clone()),
            receive,
            idle(),
            LoginPoll::new(Login::default()),
            (
                Hostnames::default(),
                application.clone(),
                Installed::default()
            ),
        )
        .is_err());
        assert_eq!(
            script.borrow().requests,
            [
                b"\x0dfirefox".to_vec(),
                vec![3],
                vec![0x1f],
                b"\x0ffirefox".to_vec()
            ]
        );
        assert_eq!(
            application.outcome(),
            StartOutcome {
                sent: 1,
                refused: false,
                state: Some(UnitState::Running)
            }
        );
    }

    #[test]
    fn the_unit_state_is_asked_only_while_watched_and_has_one_shape() {
        let application = Application::default();
        let mut idle = wire(vec![]);
        application.follow(&mut idle).unwrap();
        assert!(idle.requests.is_empty());
        application.watch(true);
        // Watched, but nothing started: nothing named to ask about.
        application.follow(&mut idle).unwrap();
        assert!(idle.requests.is_empty());
        application.started("mail", false);
        for (answer, state) in [
            (vec![0x85, 0], UnitState::Idle),
            (vec![0x85, 1], UnitState::Starting),
            (vec![0x85, 2], UnitState::Running),
            (vec![0x85, 3], UnitState::Failed),
            (vec![0x85, 4], UnitState::Unknown),
            (vec![0xff, 2], UnitState::Failed),
            (vec![0xff, 3], UnitState::Failed),
            (vec![0xff, 4], UnitState::Failed),
        ] {
            let mut asked = wire(vec![answer]);
            application.follow(&mut asked).unwrap();
            assert_eq!(asked.requests, [b"\x0fmail".to_vec()]);
            assert_eq!(application.outcome().state, Some(state));
        }
        // A new start forgets what was read about the last.
        application.started("firefox", false);
        assert_eq!(application.outcome().state, None);
        let mut asked = wire(vec![vec![0x85, 2]]);
        application.follow(&mut asked).unwrap();
        assert_eq!(asked.requests, [b"\x0ffirefox".to_vec()]);
        for answer in [
            vec![],
            vec![0x85],
            vec![0x85, 5],
            vec![0x85, 2, 0],
            vec![0x84],
            vec![0xff, 1],
            vec![0xff, 5],
        ] {
            assert!(application.follow(&mut wire(vec![answer])).is_err());
        }
        application.watch(false);
        let mut unwatched = wire(vec![]);
        application.follow(&mut unwatched).unwrap();
        assert!(unwatched.requests.is_empty());
    }

    #[test]
    fn worker_never_retries_a_request_after_uncertain_delivery() {
        struct Broken(std::rc::Rc<std::cell::Cell<usize>>);
        impl Exchange for Broken {
            fn send(&mut self, request: &[u8]) -> Result<(), String> {
                assert_eq!(request, [1]);
                self.0.set(self.0.get() + 1);
                Ok(())
            }
            fn receive(&mut self) -> Result<Vec<u8>, String> {
                Err("response lost after delivery".into())
            }
        }
        let (send, receive) = mpsc::sync_channel(QUEUE_CAPACITY);
        assert!(send.try_send(Work::Program(Program::Home)).is_ok());
        drop(send);
        let calls = std::rc::Rc::new(std::cell::Cell::new(0));
        assert!(worker(
            Broken(calls.clone()),
            receive,
            idle(),
            LoginPoll::new(Login::default()),
            (
                Hostnames::default(),
                Application::default(),
                Installed::default()
            ),
        )
        .is_err());
        assert_eq!(calls.get(), 1);
    }

    /// Root's `9f` answer, exactly: the queue, then the count, which is
    /// `00` while the intake refuses. The worker asks `1f` after each turn
    /// and keeps the answer for the menu.
    #[test]
    fn the_hostname_intake_answer_is_exact_and_kept_for_the_menu() {
        for (answer, queue, denied) in [
            (vec![0x9f, 0, 0], Queue::Empty, 0),
            (vec![0x9f, 0, 3], Queue::Empty, 3),
            (vec![0x9f, 1, 0], Queue::Waiting, 0),
            (vec![0x9f, 1, 255], Queue::Waiting, 255),
            (vec![0x9f, 2, 0], Queue::Refusing, 0),
        ] {
            let intake = HostnameIntake::decode(&answer).unwrap();
            assert_eq!((intake.queue(), intake.denied()), (queue, denied));
            let hostnames = Hostnames::default();
            let mut wire = wire(vec![answer]);
            hostnames.follow(&mut wire).unwrap();
            assert_eq!(wire.requests, [vec![0x1f]]);
            assert_eq!(hostnames.current(), intake);
        }
        for answer in [
            vec![],
            vec![0x9f],
            vec![0x9f, 0],
            vec![0x9f, 3, 0],
            vec![0x9f, 2, 1],
            vec![0x9f, 0, 0, 0],
            vec![0x9e, 0, 0],
        ] {
            assert!(HostnameIntake::decode(&answer).is_err(), "{answer:?}");
            let hostnames = Hostnames::default();
            assert!(hostnames.follow(&mut wire(vec![answer])).is_err());
            assert_eq!(hostnames.current(), HostnameIntake::default());
        }
        assert_eq!(
            (
                HostnameIntake::default().queue(),
                HostnameIntake::default().denied()
            ),
            (Queue::Empty, 0)
        );
    }

    fn table() -> Vec<String> {
        vec!["firefox".into(), "mail".into()]
    }

    /// `88`'s rows: each a table name once, its state two bits, the frame
    /// exactly as long as its rows; `ff 03` is a live boot, `ff 02` no
    /// answer yet.
    #[test]
    fn the_installed_set_has_one_shape() {
        let set = InstalledSet::decode(b"\x88\x01\x01\x07firefox", &table())
            .unwrap()
            .unwrap();
        assert_eq!(set.installed("firefox"), Some(true));
        // A name the answer leaves out is one the image ships.
        assert_eq!(set.installed("mail"), Some(true));
        assert!(!set.pending());
        let set = InstalledSet::decode(b"\x88\x01\x02\x07firefox", &table())
            .unwrap()
            .unwrap();
        assert!(set.installed("firefox") == Some(false) && set.pending());
        // Settling uninstalled, a launch asks again rather than refusing.
        assert_eq!(set.launchable("firefox"), None);
        assert_eq!(set.launchable("mail"), Some(Some(true)));
        let set = InstalledSet::decode(b"\x88\x01\x03\x07firefox", &table())
            .unwrap()
            .unwrap();
        assert_eq!(set.launchable("firefox"), Some(Some(true)));
        let set = InstalledSet::decode(b"\x88\x01\x00\x07firefox", &table())
            .unwrap()
            .unwrap();
        assert_eq!(set.launchable("firefox"), Some(Some(false)));
        let live = InstalledSet::decode(&[0xff, 3], &table()).unwrap().unwrap();
        assert_eq!(live.installed("mail"), None);
        assert_eq!(live.launchable("firefox"), Some(None));
        assert_eq!(InstalledSet::decode(&[0xff, 2], &table()), Ok(None));
        assert!(InstalledSet::decode(b"\x88\x00", &table())
            .unwrap()
            .is_some_and(|set| set.rows.is_empty()));
        for bad in [
            &b""[..],
            b"\x88",
            b"\x88\x01",
            b"\x88\x01\x01\x07firefo",
            b"\x88\x01\x01\x07firefoxx",
            b"\x88\x01\x04\x07firefox",
            b"\x88\x01\x01\x04news",
            b"\x88\x01\x01\x07Firefox",
            b"\x88\x01\x01\x00",
            b"\x88\x02\x01\x07firefox\x01\x07firefox",
            b"\x88\x11",
            b"\x87",
            b"\xff\x01",
        ] {
            assert!(InstalledSet::decode(bad, &table()).is_err(), "{bad:?}");
        }
    }

    /// Asked once the table is known and when the runtime says, and again
    /// each second while a row may change or root could not read its
    /// registry; otherwise not at all.
    #[test]
    fn the_installed_set_is_asked_when_due_and_while_pending() {
        let installed = Installed::default();
        let start = Instant::now();
        let mut quiet = wire(vec![]);
        installed.ask();
        installed.follow(&mut quiet, start).unwrap();
        assert!(quiet.requests.is_empty(), "asked before the table");
        installed.follow_table(table());
        let mut asked = wire(vec![b"\x88\x01\x03\x07firefox".to_vec()]);
        installed.follow(&mut asked, start).unwrap();
        assert_eq!(asked.requests, [vec![0x23]]);
        let (changes, set) = installed.snapshot();
        assert!(set.is_some_and(|set| set.installed("firefox") == Some(true)));
        // Pending: not again within the second, then again.
        installed.follow(&mut quiet, start).unwrap();
        assert!(quiet.requests.is_empty());
        let later = start + INSTALLED_POLL;
        let mut settled = wire(vec![b"\x88\x01\x01\x07firefox".to_vec()]);
        installed.follow(&mut settled, later).unwrap();
        assert_eq!(settled.requests.len(), 1);
        assert_eq!(installed.snapshot().0, changes.wrapping_add(1));
        // Settled: nothing more until asked.
        installed
            .follow(&mut quiet, later + INSTALLED_POLL)
            .unwrap();
        assert!(quiet.requests.is_empty());
        // An answer root could not give keeps the last and retries.
        installed.ask();
        let mut unread = wire(vec![vec![0xff, 2], b"\x88\x00".to_vec()]);
        let after = later + INSTALLED_POLL * 2;
        installed.follow(&mut unread, after).unwrap();
        assert!(installed
            .snapshot()
            .1
            .is_some_and(|set| set.rows.len() == 1));
        installed
            .follow(&mut unread, after + INSTALLED_POLL)
            .unwrap();
        assert_eq!(unread.requests.len(), 2);
        assert!(installed
            .snapshot()
            .1
            .is_some_and(|set| set.rows.is_empty()));
        // A malformed answer ends the generation.
        installed.ask();
        assert!(installed
            .follow(&mut wire(vec![vec![0x88]]), after + INSTALLED_POLL * 2)
            .is_err());
    }

    /// An operation that started asks for the set again; td-software opened
    /// at an application sends `22` and its name.
    #[test]
    fn a_started_operation_asks_again_and_software_opens_at_a_name() {
        let installed = Installed::default();
        installed.follow_table(table());
        let start = Instant::now();
        installed
            .follow(&mut wire(vec![b"\x88\x00".to_vec()]), start)
            .unwrap();
        let (send, receive) = mpsc::sync_channel(QUEUE_CAPACITY);
        let (control, requests) = mpsc::sync_channel(QUEUE_CAPACITY);
        let (reply, answer) = mpsc::sync_channel(1);
        assert!(control
            .try_send((CacheVerb::Install, "firefox".into(), reply))
            .is_ok());
        assert!(send
            .try_send(Work::Supervised(Supervised::Start("mail".into())))
            .is_ok());
        drop(send);
        let mut script = wire(vec![
            vec![0x84],
            vec![0x86],
            vec![0x83],
            vec![0x9f, 0, 0],
            b"\x88\x00".to_vec(),
        ]);
        assert!(worker(
            &mut script,
            receive,
            requests,
            LoginPoll::new(Login::default()),
            (
                Hostnames::default(),
                Application::default(),
                installed.clone()
            ),
        )
        .is_err());
        assert_eq!(answer.try_recv(), Ok(CacheAnswer::Started));
        assert_eq!(script.requests.last(), Some(&vec![0x23]));
        drop(control);
        let mut processes = Processes::new();
        let mut opened = wire(vec![handle(1)]);
        assert_eq!(
            processes.start_software_at(&mut opened, "firefox"),
            Ok(None)
        );
        assert_eq!(opened.requests, [b"\x22firefox".to_vec()]);
    }

    /// A start names an application in the registry grammar, or nothing
    /// is queued: root would end the generation on it.
    #[test]
    fn a_start_outside_the_registry_grammar_is_refused_before_root() {
        let (launcher, queued) = Queued::launcher();
        for name in ["Firefox", "", "-x", &"a".repeat(65)] {
            assert!(launcher.supervise(Supervised::Start(name.into())).is_err());
            assert_eq!(queued.supervised(), None);
        }
        assert!(launcher
            .supervise(Supervised::Start("firefox".into()))
            .is_ok());
    }

    #[test]
    fn launcher_queue_never_blocks_input_or_retries_a_delivered_request() {
        let (send, receive) = mpsc::sync_channel(QUEUE_CAPACITY);
        let (control, requests) = mpsc::sync_channel(QUEUE_CAPACITY);
        let launcher = Launcher {
            send,
            control,
            login: Login::default(),
            hostnames: Hostnames::default(),
            application: Application::default(),
            installed: Installed::default(),
            connected: None,
        };
        assert!(launcher.launch().is_ok());
        assert!(launcher.launch().is_err());
        assert!(matches!(
            receive.try_recv(),
            Ok(Work::Program(Program::Home))
        ));
        assert!(launcher.launch_selected(Program::Task).is_ok());
        assert!(matches!(
            receive.try_recv(),
            Ok(Work::Program(Program::Task))
        ));
        assert!(receive.try_recv().is_err());
        // A control-socket request waits apart: a full control queue
        // leaves the physical one free, and the other way about.
        assert!(launcher.cache(CacheVerb::Status, "firefox").is_ok());
        assert!(launcher.cache(CacheVerb::Status, "firefox").is_err());
        assert!(launcher.launch().is_ok());
        assert!(launcher.cache(CacheVerb::Install, "firefox").is_err());
        assert!(requests.try_recv().is_ok());
        assert!(launcher.cache(CacheVerb::Install, "firefox").is_ok());
        assert!(launcher.cache(CacheVerb::Install, "Firefox").is_err());
        drop(receive);
        assert!(launcher.launch().is_err());
    }

    /// A wire the test keeps, lent to a worker that takes its exchange by
    /// value.
    impl<T: Exchange> Exchange for &mut T {
        fn send(&mut self, request: &[u8]) -> Result<(), String> {
            (**self).send(request)
        }
        fn receive(&mut self) -> Result<Vec<u8>, String> {
            (**self).receive()
        }
    }

    const RECORD: &[u8] = b"format=1\nname=firefox\nkind=flathub\ncurrent=none\nprevious=none\n\
checked=never\nrefused=none\nprogress=none\n";

    #[test]
    fn an_application_request_names_its_verb_byte_and_the_name() {
        for (verb, word, byte) in CACHE_VERBS {
            assert_eq!(CacheVerb::from_word(word), Some(*verb));
            assert_eq!(verb.word(), *word);
            assert_eq!(verb.byte(), *byte);
        }
        assert_eq!(CacheVerb::from_word("ready"), None);
        let mut asked = wire(vec![vec![0x86]]);
        assert_eq!(
            cache(&mut asked, CacheVerb::DeleteData, "firefox").unwrap(),
            CacheAnswer::Started
        );
        assert_eq!(asked.requests, [b"\x24\x04firefox".to_vec()]);
        let (launcher, _queued) = Queued::launcher();
        for bad in ["", "Firefox", "../firefox", "fire fox", &"a".repeat(65)] {
            assert!(launcher.cache(CacheVerb::Status, bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn roots_application_answer_has_exactly_one_shape() {
        let status = |head: &[u8], record: &[u8]| [head, record].concat();
        let decode = |verb, bytes: &[u8]| CacheAnswer::decode(verb, bytes);
        for verb in [
            CacheVerb::Install,
            CacheVerb::Update,
            CacheVerb::Remove,
            CacheVerb::DeleteData,
        ] {
            assert_eq!(decode(verb, &[0x86]), Ok(CacheAnswer::Started));
            assert!(decode(verb, &[0x87, 0, 0, 0, 0, 0]).is_err());
            assert!(decode(verb, &[0x86, 0]).is_err());
        }
        for verb in [CacheVerb::Status, CacheVerb::Install] {
            assert_eq!(decode(verb, &[0xff, 1]), Ok(CacheAnswer::Busy));
            assert_eq!(decode(verb, &[0xff, 2]), Ok(CacheAnswer::Unstarted));
            assert_eq!(decode(verb, &[0xff, 3]), Ok(CacheAnswer::Live));
            assert_eq!(decode(verb, &[0xff, 4]), Ok(CacheAnswer::Full));
            assert_eq!(decode(verb, &[0xff, 5]), Ok(CacheAnswer::Unread));
            assert!(decode(verb, &[0xff, 6]).is_err());
        }
        let text = String::from_utf8(RECORD.to_vec()).unwrap();
        for (bytes, operation, last, record) in [
            (
                status(&[0x87, 0, 0, 0, 0, 0], b""),
                None,
                None,
                CacheRecord::Unread,
            ),
            (
                status(&[0x87, 1, 0, 0, 0, 1], RECORD),
                Some(CacheVerb::Install),
                None,
                CacheRecord::Current(text.clone()),
            ),
            (
                status(&[0x87, 0, 2, 0, 0, 2], RECORD),
                None,
                Some((CacheVerb::Update, None)),
                CacheRecord::Older(text.clone()),
            ),
            (
                status(&[0x87, 4, 3, 1, 4], b"lost\x03"),
                Some(CacheVerb::DeleteData),
                Some((CacheVerb::Remove, Some("lost".to_string()))),
                CacheRecord::Failed,
            ),
        ] {
            assert_eq!(
                decode(CacheVerb::Status, &bytes),
                Ok(CacheAnswer::Status(CacheStatus {
                    operation,
                    last,
                    record
                })),
                "{bytes:02x?}"
            );
        }
        let long = [&[0x87, 0, 1, 1, 201][..], &[b'x'; 201], &[0]].concat();
        for bad in [
            vec![],
            vec![0x87],
            vec![0x86],
            vec![0x87, 0, 0, 0, 0],
            // An operation or a last verb outside the four, a status as one.
            vec![0x87, 5, 0, 0, 0, 0],
            vec![0x87, 0, 9, 0, 0, 0],
            // A refusal without a reason, a reason without one, a result
            // with no verb, a result past `01`.
            vec![0x87, 0, 1, 1, 0, 0],
            status(&[0x87, 0, 1, 0, 1], b"x\x00"),
            status(&[0x87, 0, 0, 1, 1], b"x\x00"),
            vec![0x87, 0, 1, 2, 0, 0],
            // A reason with a tab or a control, cut short, or too long.
            status(&[0x87, 0, 1, 1, 2], b"a\t\x00"),
            status(&[0x87, 0, 1, 1, 2], b"a\x1b\x00"),
            status(&[0x87, 0, 1, 1, 9], b"short"),
            long,
            // A state past `03`, a record where none belongs, none where
            // one does, one without its newline or with a control.
            vec![0x87, 0, 0, 0, 0, 4],
            status(&[0x87, 0, 0, 0, 0, 0], RECORD),
            status(&[0x87, 0, 0, 0, 0, 3], RECORD),
            vec![0x87, 0, 0, 0, 0, 1],
            status(&[0x87, 0, 0, 0, 0, 1], b"format=1"),
            status(&[0x87, 0, 0, 0, 0, 1], b"format=1\x1b\n"),
            status(&[0x87, 0, 0, 0, 0, 2], b"\xc3\xa9\n"),
        ] {
            assert!(decode(CacheVerb::Status, &bad).is_err(), "{bad:02x?}");
        }
    }

    #[test]
    fn the_worker_answers_an_application_request_once_and_carries_on() {
        let (send, receive) = mpsc::sync_channel(QUEUE_CAPACITY);
        let (control, requests) = mpsc::sync_channel(QUEUE_CAPACITY);
        let (reply, answer) = mpsc::sync_channel(1);
        assert!(control
            .try_send((CacheVerb::Install, "firefox".into(), reply))
            .is_ok());
        // The physical queue's request goes to root first.
        assert!(send
            .try_send(Work::Supervised(Supervised::Start("mail".into())))
            .is_ok());
        drop(send);
        let mut script = wire(vec![vec![0x84], vec![0x86], vec![0x83], vec![0x9f, 0, 0]]);
        assert!(worker(
            &mut script,
            receive,
            requests,
            LoginPoll::new(Login::default()),
            (
                Hostnames::default(),
                Application::default(),
                Installed::default()
            ),
        )
        .is_err());
        assert_eq!(answer.try_recv(), Ok(CacheAnswer::Started));
        assert_eq!(
            script.requests,
            [
                b"\x0dmail".to_vec(),
                b"\x24\x01firefox".to_vec(),
                vec![3],
                vec![0x1f]
            ]
        );
        // A malformed answer ends the generation.
        let (send, receive) = mpsc::sync_channel::<Work>(QUEUE_CAPACITY);
        let (control, requests) = mpsc::sync_channel(QUEUE_CAPACITY);
        let (reply, _answer) = mpsc::sync_channel(1);
        assert!(control
            .try_send((CacheVerb::Status, "firefox".into(), reply))
            .is_ok());
        let error = worker(
            wire(vec![vec![0x86]]),
            receive,
            requests,
            LoginPoll::new(Login::default()),
            (
                Hostnames::default(),
                Application::default(),
                Installed::default(),
            ),
        )
        .unwrap_err();
        assert!(error.contains("application request answer"), "{error}");
        drop(send);
    }
}

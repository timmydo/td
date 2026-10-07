//! Request `1a`: the human's login state, with root's cache of it
//! (td-authd/DESIGN.md, login-state amendment 1).

use crate::inspection::Inspection;
use std::fs::File;
use std::io::Read;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[path = "../../td-firstboot/src/hostname.rs"]
mod hostname;
#[path = "../../td-secret/src/login_state.rs"]
#[allow(
    dead_code,
    reason = "the shared login-state predicate also serves firstboot and the record store"
)]
mod login_state;

use login_state::{Cause, Owner};

/// The kernel's name for this machine, which every answer reads afresh.
const HOSTNAME: &str = "/proc/sys/kernel/hostname";
/// One byte past the longest valid name and its newline: a longer one is
/// sent empty.
const HOSTNAME_READ: u64 = 65;
/// While the state could not be read, the helper runs again only this long
/// after its last run ended, and an answer in between is the cache's. Each
/// consecutive run the deadline ended past the first doubles it, up to
/// `LONGEST_PAUSE`; a run the helper ended itself resets it.
const HELPER_PAUSE: Duration = Duration::from_secs(2);
const LONGEST_PAUSE: Duration = Duration::from_secs(16);

/// TOKEN-LOGIN.md's login states, as `9a` carries them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum State {
    Unenrolled,
    /// The record's slot fingerprints in canonical slot order, one to eight.
    Enrolled(Vec<[u8; 4]>),
    Unavailable(Cause),
}

impl State {
    /// The state as the helper's result shows it, with its version byte
    /// dropped (`9a` carries none); anything else could not be read.
    fn inspected(result: Option<&[u8]>) -> Self {
        match result {
            Some([0x1a, 0]) => Self::Unavailable(Cause::RecordDamaged),
            Some([0x1a, 1, _version, count, keys @ ..])
                if (1..=8).contains(count) && keys.len() == usize::from(*count) * 4 =>
            {
                let (keys, []) = keys.as_chunks::<4>() else {
                    return Self::Unavailable(Cause::Unreadable);
                };
                Self::Enrolled(keys.to_vec())
            }
            _ => Self::Unavailable(Cause::Unreadable),
        }
    }

    fn unreadable(&self) -> bool {
        *self == Self::Unavailable(Cause::Unreadable)
    }

    fn encode(&self, answer: &mut Vec<u8>) -> Result<(), String> {
        match self {
            Self::Unenrolled => answer.push(0),
            Self::Enrolled(keys) => {
                answer.push(1);
                answer.push(u8::try_from(keys.len()).map_err(|_| "login key list overflow")?);
                answer.extend(keys.iter().flatten());
            }
            Self::Unavailable(cause) => answer.extend_from_slice(&[
                2,
                match cause {
                    Cause::DirectoryDamaged => 0x0a,
                    Cause::RecordDamaged => 0x0b,
                    Cause::Unreadable => 0x0c,
                },
            ]),
        }
        Ok(())
    }
}

/// The helper's launch: the production one, or a test's.
type Helper = Box<dyn FnMut(u32) -> Result<Inspection, String>>;

pub(crate) struct Status {
    owner: u32,
    /// The validated primary account's name, which every answer carries.
    username: String,
    /// What the predicate reads under, `/` in production, and the owner
    /// the directory must have there.
    root: PathBuf,
    directory_owner: Owner,
    hostname: PathBuf,
    helper: Helper,
    /// The last state read, and whether a login operation has ended since.
    cached: Option<State>,
    stale: bool,
    /// When the helper's last run ended, and how many runs in a row the
    /// deadline ended.
    helped: Option<Instant>,
    misses: u32,
    /// A helper killed but not yet reaped: no other starts until it is.
    running: Option<Inspection>,
}

impl Status {
    pub fn new(owner: u32, username: &str) -> Result<Self, String> {
        crate::primary_account::validate_name(username).map_err(|e| e.to_string())?;
        Ok(Self {
            owner,
            username: username.into(),
            root: PathBuf::from("/"),
            directory_owner: Owner::ROOT,
            hostname: PathBuf::from(HOSTNAME),
            helper: Box::new(Inspection::login),
            cached: None,
            stale: false,
            helped: None,
            misses: 0,
            running: None,
        })
    }

    /// Reaps a retained helper that has exited, without waiting.
    pub fn tick(&mut self) {
        if self.running.as_mut().is_some_and(Inspection::reaped) {
            self.running = None;
        }
    }

    /// The pause after the last run: two seconds, doubled for each
    /// consecutive run past the deadline beyond the first, to sixteen.
    fn pause(&self) -> Duration {
        let doublings = self.misses.saturating_sub(1).min(3);
        HELPER_PAUSE
            .checked_mul(1 << doublings)
            .map_or(LONGEST_PAUSE, |pause| pause.min(LONGEST_PAUSE))
    }

    /// A login operation ended, however it ended: the next answer reads
    /// the state afresh.
    pub fn operation_ended(&mut self) {
        self.stale = true;
    }

    /// The `9a` answer. A live boot is unenrolled without any read, so the
    /// live medium never locks.
    pub fn answer(&mut self, live: bool) -> Result<Vec<u8>, String> {
        let state = if live {
            State::Unenrolled
        } else {
            self.current()
        };
        let mut answer = vec![0x9a];
        state.encode(&mut answer)?;
        let username = self.username.as_bytes();
        answer.push(u8::try_from(username.len()).map_err(|_| "username overflow")?);
        answer.extend_from_slice(username);
        let hostname = self.hostname();
        answer.push(u8::try_from(hostname.len()).map_err(|_| "hostname overflow")?);
        answer.extend_from_slice(hostname.as_bytes());
        // Revocation (amendment 7) is reserved until increment 5.
        answer.push(0);
        Ok(answer)
    }

    /// The cached state, refreshed first when none is cached, after a
    /// login operation, and while it reads could-not-be-read.
    fn current(&mut self) -> State {
        match &self.cached {
            Some(state) if !self.stale && !state.unreadable() => state.clone(),
            _ => self.refresh(),
        }
    }

    /// Reads the state and caches it: the predicate, then the helper only
    /// when the record's name exists. Request 19's fresh read (amendment
    /// 8, TOKEN-LOGIN.md increment 4's C4) is this too.
    fn refresh(&mut self) -> State {
        let state = match login_state::state_as(&self.root, self.directory_owner, self.owner) {
            login_state::State::Unenrolled => State::Unenrolled,
            login_state::State::Unavailable(cause) => State::Unavailable(cause),
            login_state::State::Enrolled => self.inspect(),
        };
        if !state.unreadable() {
            self.misses = 0;
        }
        self.cached = Some(state.clone());
        self.stale = false;
        state
    }

    /// The record's state as the helper reads it. A helper that cannot
    /// start, fails, answers malformed bytes or runs out its deadline
    /// leaves it unreadable; while it reads unreadable, the helper waits out
    /// its pause, and a killed one its reaping, before it runs again.
    fn inspect(&mut self) -> State {
        self.tick();
        let unreadable = self.cached.as_ref().is_some_and(State::unreadable);
        let pausing = unreadable
            && self.helped.is_some_and(|ended| {
                ended
                    .checked_add(self.pause())
                    .is_none_or(|resume| Instant::now() < resume)
            });
        if pausing || self.running.is_some() {
            return State::Unavailable(Cause::Unreadable);
        }
        let Ok(mut helper) = (self.helper)(self.owner) else {
            self.helped = Some(Instant::now());
            return State::Unavailable(Cause::Unreadable);
        };
        let result = helper.wait();
        self.helped = Some(Instant::now());
        self.misses = if helper.missed() {
            self.misses.saturating_add(1)
        } else {
            0
        };
        if !helper.reaped() {
            self.running = Some(helper);
        }
        State::inspected(result.as_deref())
    }

    /// The kernel's hostname under td-firstboot's `Hostname::parse`, read
    /// for every answer, or empty.
    fn hostname(&self) -> String {
        let mut bytes = Vec::with_capacity(HOSTNAME_READ as usize);
        let read = File::open(&self.hostname)
            .and_then(|file| file.take(HOSTNAME_READ).read_to_end(&mut bytes));
        if read.is_err() {
            return String::new();
        }
        let name = bytes.strip_suffix(b"\n").unwrap_or(bytes.as_slice());
        std::str::from_utf8(name)
            .ok()
            .and_then(|name| hostname::Hostname::parse(name).ok())
            .map(|name| name.name().to_owned())
            .unwrap_or_default()
    }
}

#[cfg(test)]
#[path = "../tests/login_status.rs"]
pub(crate) mod tests;

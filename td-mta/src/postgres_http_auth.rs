//! Fresh primary authentication for the fixed HTTPS credential worker.
use crate::{
    clock::RuntimeClock,
    device_credentials::Presented,
    http_auth::{Authenticator, Principal},
    ports::{Deadline, Error},
    postgres_input::Loaded,
    postgres_probe::{self, AuthenticationSession, Mode},
    postgres_resolver,
    postgres_runtime_profile::Binding,
};
use std::{
    path::Path,
    sync::{Mutex, TryLockError},
    thread::{self, ThreadId},
    time::{Duration, Instant},
};

/// Preparation adapter, not a serving entry point. The HTTPS service must use
/// this same RuntimeClock. One fixed thread runs calls with no waiter queue.
/// Its retained connection has finite lifetime and byte/frame/job allowances.
pub struct Authentication<'a> {
    loaded: &'a Loaded,
    executable: &'a Path,
    accounts: &'a [(&'a str, &'a Binding)],
    clock: &'a RuntimeClock,
    state: Mutex<State>,
}
struct State {
    owner: Option<ThreadId>,
    session: Option<AuthenticationSession>,
}
impl<'a> Authentication<'a> {
    pub fn new(
        loaded: &'a Loaded,
        executable: &'a Path,
        accounts: &'a [(&'a str, &'a Binding)],
        clock: &'a RuntimeClock,
    ) -> Result<Self, Error> {
        if !executable.is_absolute()
            || accounts.is_empty()
            || accounts.len() > crate::config::identities::MAX_ACCOUNTS
            || accounts
                .iter()
                .enumerate()
                .any(|(index, (username, binding))| {
                    username.is_empty()
                        || username.len() > 254
                        || !username
                            .bytes()
                            .all(|byte| (33..=126).contains(&byte) && byte != b':')
                        || !binding.roles().jmap()
                        || loaded.bootstrap.profile() != binding.profile()
                        || accounts.iter().take(index).any(|(prior, other)| {
                            prior == username || other.account() == binding.account()
                        })
                })
        {
            return Err(Error::Invalid);
        }
        Ok(Self {
            loaded,
            executable,
            accounts,
            clock,
            state: Mutex::new(State {
                owner: None,
                session: None,
            }),
        })
    }
}
impl Authenticator for Authentication<'_> {
    fn authenticate(
        &self,
        username: &str,
        password: &[u8],
        deadline: Deadline,
    ) -> Result<Option<Principal>, Error> {
        let now = Instant::now();
        let deadline = self.clock.deadline_instant(deadline)?.min(
            now.checked_add(Duration::from_secs(10))
                .ok_or(Error::Invalid)?,
        );
        if now >= deadline {
            return Err(Error::Deadline);
        }
        let Some((_, binding)) = self.accounts.iter().find(|(name, _)| *name == username) else {
            return Ok(None);
        };
        if Presented::parse(password).is_none() {
            return Ok(None);
        }
        let mut state = self.state.try_lock().map_err(|error| match error {
            TryLockError::WouldBlock => Error::Busy,
            TryLockError::Poisoned(_) => Error::Invalid,
        })?;
        let owner = thread::current().id();
        if state.owner.is_some_and(|prior| prior != owner) {
            return Err(Error::Invalid);
        }
        state.owner = Some(owner);
        if state
            .session
            .as_ref()
            .is_some_and(|session| !session.reusable(deadline))
        {
            if let Some(session) = state.session.take() {
                session.retire(deadline);
            }
        }
        let reused = state.session.is_some();
        if state.session.is_none() {
            state.session = Some(self.connect(deadline)?);
        }
        let presented = Presented::parse(password).ok_or(Error::Invalid)?;
        let result = state
            .session
            .as_mut()
            .ok_or(Error::Invalid)?
            .authenticate(binding, presented, deadline);
        let result = if Instant::now() >= deadline {
            Err(Error::Deadline)
        } else {
            result
        };
        if reused {
            crate::performance::record(
                crate::performance::Phase::DbReuse,
                Duration::ZERO,
                if result.is_ok() {
                    crate::performance::Outcome::Success
                } else {
                    crate::performance::Outcome::Failed
                },
            );
        }
        if result.is_err() {
            drop(state.session.take());
        }
        match result {
            Ok(principal) => Ok(Some(principal)),
            Err(Error::Forbidden) => Ok(None),
            Err(error) => Err(error),
        }
    }
}
impl Authentication<'_> {
    fn connect(&self, deadline: Instant) -> Result<AuthenticationSession, Error> {
        let dns_deadline = deadline.min(
            Instant::now()
                .checked_add(Duration::from_secs(5))
                .ok_or(Error::Invalid)?,
        );
        let addresses =
            postgres_resolver::resolve(self.executable, &self.loaded.bootstrap, dns_deadline)
                .map_err(|error| match error {
                    postgres_resolver::Error::Deadline => Error::Deadline,
                    _ => Error::Dns,
                })?;
        for address in addresses {
            let mut request = self.loaded.bootstrap.request(address);
            request.mode = Mode::Runtime;
            let result = AuthenticationSession::open(&request, &self.loaded.trust, deadline);
            if Instant::now() >= deadline {
                return Err(Error::Deadline);
            }
            match result {
                Ok(session) => return Ok(session),
                Err(postgres_probe::Error::Transport) => continue,
                Err(postgres_probe::Error::Deadline) => return Err(Error::Deadline),
                Err(_) => return Err(Error::Invalid),
            }
        }
        Err(Error::Invalid)
    }
}

//! Fresh primary authentication for the fixed HTTPS credential worker.
use crate::{
    clock::RuntimeClock,
    device_credentials::Presented,
    http_auth::{Authenticator, EventCredential, EventObservation, Principal},
    ports::{Deadline, Error},
    postgres_input::Loaded,
    postgres_probe::{self, AuthenticationSession, Mode},
    postgres_runtime_profile::Binding,
};
use std::{
    sync::{Mutex, TryLockError},
    thread::{self, ThreadId},
    time::{Duration, Instant},
};

/// Preparation adapter, not a serving entry point. The HTTPS service must use
/// this same RuntimeClock. One fixed thread runs calls with no waiter queue.
/// Its retained connection has finite lifetime and byte/frame/job allowances.
pub struct Authentication<'a> {
    loaded: &'a Loaded,
    accounts: &'a [(&'a str, &'a Binding)],
    clock: &'a RuntimeClock,
    mode: Mode,
    state: Mutex<State>,
}
struct State {
    owner: Option<ThreadId>,
    session: Option<AuthenticationSession>,
}
impl<'a> Authentication<'a> {
    pub fn new(
        loaded: &'a Loaded,
        accounts: &'a [(&'a str, &'a Binding)],
        clock: &'a RuntimeClock,
    ) -> Result<Self, Error> {
        let mode = accounts.first().ok_or(Error::Invalid)?.1.runtime_mode();
        if accounts.is_empty()
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
                        || binding.runtime_mode() != mode
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
            accounts,
            clock,
            mode,
            state: Mutex::new(State {
                owner: None,
                session: None,
            }),
        })
    }
}
impl Authenticator for Authentication<'_> {
    fn maintain(&self) -> Result<(), Error> {
        self.retire_idle(false)
    }
    fn close(&self) -> Result<(), Error> {
        self.retire_idle(true)
    }
    fn authenticate(
        &self,
        username: &str,
        password: &[u8],
        deadline: Deadline,
    ) -> Result<Option<Principal>, Error> {
        let deadline = self.deadline(deadline)?;
        let Some((_, binding)) = self.accounts.iter().find(|(name, _)| *name == username) else {
            return Ok(None);
        };
        let Some(presented) = Presented::parse(password) else {
            return Ok(None);
        };
        match self.with_session(deadline, |session| {
            session.authenticate(binding, presented, deadline)
        }) {
            Ok(principal) => Ok(Some(principal)),
            Err(Error::Forbidden) => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn observe_events(
        &self,
        principal: Principal,
        credential: EventCredential<&[u8]>,
        deadline: Deadline,
    ) -> Result<EventObservation, Error> {
        let deadline = self.deadline(deadline)?;
        let (_, binding) = self
            .accounts
            .iter()
            .find(|(_, binding)| binding.account() == principal.account)
            .ok_or(Error::Forbidden)?;
        let credential = match credential {
            EventCredential::Password(bytes) => {
                EventCredential::Password(Presented::parse(bytes).ok_or(Error::Forbidden)?)
            }
            EventCredential::Continued(stamp) => EventCredential::Continued(stamp),
        };
        self.with_session(deadline, |session| {
            session.observe_events(binding, principal, credential, deadline)
        })
    }
}
impl Authentication<'_> {
    fn deadline(&self, deadline: Deadline) -> Result<Instant, Error> {
        let now = Instant::now();
        let deadline = self.clock.deadline_instant(deadline)?.min(
            now.checked_add(Duration::from_secs(10))
                .ok_or(Error::Invalid)?,
        );
        if now >= deadline {
            return Err(Error::Deadline);
        }
        Ok(deadline)
    }
    fn with_session<T>(
        &self,
        deadline: Instant,
        observe: impl FnOnce(&mut AuthenticationSession) -> Result<T, Error>,
    ) -> Result<T, Error> {
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
        // An idle transport failure precedes this job: replacing it cannot
        // replay SQL. Once authenticate starts, no failure retries the job.
        if let Some(session) = state.session.as_mut() {
            if session.check_idle(deadline).is_err() {
                drop(state.session.take());
            }
        }
        let reused = state.session.is_some();
        if state.session.is_none() {
            state.session = Some(self.connect(deadline)?);
        }
        let result = observe(state.session.as_mut().ok_or(Error::Invalid)?);
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
        if result.is_err()
            && !(matches!(result, Err(Error::Forbidden))
                && state
                    .session
                    .as_ref()
                    .is_some_and(|session| session.reusable(deadline)))
        {
            drop(state.session.take());
        }
        result
    }
    fn retire_idle(&self, closing: bool) -> Result<(), Error> {
        let mut state = self.state.try_lock().map_err(|error| match error {
            TryLockError::WouldBlock => Error::Busy,
            TryLockError::Poisoned(_) => Error::Invalid,
        })?;
        if state
            .owner
            .is_some_and(|owner| owner != thread::current().id())
        {
            return Err(Error::Invalid);
        }
        let now = Instant::now();
        if state
            .session
            .as_ref()
            .is_some_and(|session| closing || session.needs_retirement(now))
        {
            let deadline = now
                .checked_add(Duration::from_millis(100))
                .ok_or(Error::Invalid)?;
            if let Some(session) = state.session.take() {
                session.retire(deadline);
            }
        }
        Ok(())
    }
    fn connect(&self, deadline: Instant) -> Result<AuthenticationSession, Error> {
        let mut request = self.loaded.bootstrap.request();
        request.mode = self.mode;
        let result = AuthenticationSession::open(&request, deadline);
        if Instant::now() >= deadline {
            return Err(Error::Deadline);
        }
        result.map_err(|error| match error {
            postgres_probe::Error::Deadline => Error::Deadline,
            _ => Error::Invalid,
        })
    }
}

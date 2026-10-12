//! A login operation authorized by a disk proof (td-authd/DESIGN.md
//! amendment 10): the login worker reads its baseline, the storage worker
//! presents and verifies the proof, and only after that worker's success
//! and exit does the login worker take the proof's last step as its first
//! frame and carry the operation on.

use crate::consent::{Fingerprint, Request};
use crate::login::{self, End, Login, Pin, Selection, CANCELLED, INTERNAL, NO_PROOF};
use crate::login_status::login_state::Owner;
use crate::storage::Storage;
use crate::unlock::Event;
use std::path::Path;
use std::time::Instant;

/// How the storage worker starts once the login worker's baseline admits
/// the proof's first step: production's fixed verb, or a test's script.
pub(crate) type Start =
    Box<dyn FnMut(u32, [u8; 32], &Request, &[Fingerprint], Instant) -> Result<Storage, String>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    /// Waiting for the login worker's baseline.
    Baseline,
    /// The storage worker runs the proof; the login worker waits.
    Proof,
    /// The login worker carries the operation on from the proof's step.
    Resumed,
    /// Ended: both workers are stopped and being reaped.
    Ending,
    Done,
}

pub(crate) struct Proving {
    owner: u32,
    nonce: [u8; 32],
    login: Option<Login>,
    storage: Option<Storage>,
    start: Start,
    stage: Stage,
    /// The last description the storage worker presented, which an end
    /// during the proof carries.
    shown: Option<Request>,
    end: Option<End>,
    terminal: Option<Event>,
}

impl Proving {
    /// Call only after root startup and paired-session admission, for a
    /// `Selection::Proven`, which only a build whose `storage::OPERATIONS`
    /// is set decodes. On a machine that is not protected it ends as `86`
    /// before any worker starts.
    pub fn start(owner: u32, selection: Selection) -> Result<Self, String> {
        let (nonce, approval) = login::draw()?;
        let protected = login::protected(Path::new("/"), Owner::ROOT);
        Self::begin(owner, nonce, protected, selection, |owner, selection| {
            Login::begin_with(owner, nonce, approval, selection, true)
        })
        .map(|proving| proving.storage_with(Box::new(Storage::start_proof)))
    }

    /// `start` with its draw, its read of the marker and both workers'
    /// starts the caller's.
    pub(crate) fn begin(
        owner: u32,
        nonce: [u8; 32],
        protected: bool,
        selection: Selection,
        login: impl FnOnce(u32, Selection) -> Result<Login, String>,
    ) -> Result<Self, String> {
        if owner != 1000 {
            return Err("login supervisor requires the configured graphical session".into());
        }
        if selection.proof().is_none() {
            return Err("not a disk proof".into());
        }
        let mut proving = Self {
            owner,
            nonce,
            login: None,
            storage: None,
            start: Box::new(|_, _, _, _, _| Err("no storage worker".into())),
            stage: Stage::Done,
            shown: None,
            end: None,
            terminal: None,
        };
        if !protected {
            proving.terminal = Some(Event::Ended(End {
                uncertain: false,
                kind: NO_PROOF,
                detail: 0,
            }));
            return Ok(proving);
        }
        proving.login = Some(login(owner, selection)?);
        proving.stage = Stage::Baseline;
        Ok(proving)
    }

    pub(crate) fn storage_with(mut self, start: Start) -> Self {
        self.start = start;
        self
    }

    pub fn nonce(&self) -> &[u8; 32] {
        &self.nonce
    }

    /// None until the storage worker's baseline lets root derive the
    /// proof's first step; then the storage worker's current step, and
    /// once resumed the login worker's.
    pub fn request(&self) -> Option<&Request> {
        match self.stage {
            Stage::Resumed => self.login.as_ref().and_then(Login::request),
            _ => self.shown.as_ref(),
        }
    }

    pub fn presented(&mut self, request: &Request) -> Result<(), String> {
        match (self.stage, &mut self.storage, &mut self.login) {
            (Stage::Proof, Some(storage), _) => storage.presented(request),
            (Stage::Resumed, _, Some(login)) => login.presented(request),
            // A receipt that crossed the end, as a login operation drops it.
            (Stage::Ending | Stage::Done, ..) if self.shown.as_ref() == Some(request) => Ok(()),
            _ => {
                self.stop(INTERNAL)?;
                Err("stale disk proof acknowledgement".into())
            }
        }
    }

    pub fn commit(&mut self, request: &Request) -> Result<(), String> {
        match (self.stage, &mut self.login) {
            (Stage::Resumed, Some(login)) => login.commit(request),
            _ => {
                self.stop(INTERNAL)?;
                Err("a disk proof has no commit".into())
            }
        }
    }

    pub fn pin(&mut self, request: &Request, pin: Pin) -> Result<bool, String> {
        match (self.stage, &mut self.storage, &mut self.login) {
            (Stage::Proof, Some(storage), _) => storage.pin(request, pin),
            (Stage::Resumed, _, Some(login)) => login.pin(request, pin),
            (Stage::Ending | Stage::Done, ..) if request.nonce() == &self.nonce => Ok(false),
            _ => {
                self.stop(INTERNAL)?;
                Err("stale disk proof secret".into())
            }
        }
    }

    /// Kills both workers, the storage worker after its grace; once
    /// resumed only the login worker remains, and its own cancellation
    /// says whether a write may have begun.
    pub fn cancel(&mut self) -> Result<(), String> {
        match (self.stage, &mut self.login) {
            (Stage::Resumed, Some(login)) => login.cancel(),
            _ => self.stop(CANCELLED),
        }
    }

    pub fn poll(&mut self) -> Result<Event, String> {
        match self.stage {
            Stage::Done => self
                .terminal
                .clone()
                .ok_or_else(|| "missing disk proof terminal result".into()),
            Stage::Ending => self.settle(),
            Stage::Resumed => self.login.as_mut().ok_or("missing login worker")?.poll(),
            Stage::Baseline => {
                if let Some(event) = self.watch()? {
                    return Ok(event);
                }
                let login = self.login.as_ref().ok_or("missing login worker")?;
                let Some((first, baseline, deadline)) = login.proving() else {
                    return Ok(Event::Waiting);
                };
                let (first, baseline) = (first.clone(), baseline.to_vec());
                match (self.start)(self.owner, self.nonce, &first, &baseline, deadline) {
                    Ok(storage) => {
                        self.storage = Some(storage);
                        self.stage = Stage::Proof;
                        Ok(Event::Waiting)
                    }
                    Err(_) => {
                        self.stop(INTERNAL)?;
                        self.settle()
                    }
                }
            }
            Stage::Proof => {
                if let Some(event) = self.watch()? {
                    return Ok(event);
                }
                let storage = self.storage.as_mut().ok_or("missing storage worker")?;
                let event = storage.poll()?;
                if let Some(request) = storage.request() {
                    self.shown = Some(request.clone());
                }
                match event {
                    Event::Complete => self.resume(),
                    Event::Ended(end) => {
                        self.storage_ended(end)?;
                        self.settle()
                    }
                    other => Ok(other),
                }
            }
        }
    }

    /// The login worker before it resumes: it sends nothing, so its end,
    /// whatever ended it, root's deadline included, ends the operation.
    fn watch(&mut self) -> Result<Option<Event>, String> {
        let login = self.login.as_mut().ok_or("missing login worker")?;
        match login.poll()? {
            Event::Waiting => Ok(None),
            Event::Ended(end) => {
                self.end = Some(end);
                if let Some(storage) = &mut self.storage {
                    storage.cancel()?;
                }
                self.stage = Stage::Ending;
                self.settle().map(Some)
            }
            _ => {
                self.stop(INTERNAL)?;
                self.settle().map(Some)
            }
        }
    }

    /// The storage worker's end is the operation's: the login worker,
    /// which wrote nothing, is killed.
    fn storage_ended(&mut self, end: End) -> Result<(), String> {
        self.end = Some(end);
        if let Some(login) = &mut self.login {
            login.cancel()?;
        }
        self.stage = Stage::Ending;
        Ok(())
    }

    /// The storage worker sent `14` and exited: the login worker takes the
    /// proof's last step, the storage worker's last description.
    fn resume(&mut self) -> Result<Event, String> {
        let proved = self
            .storage
            .take()
            .and_then(|storage| storage.request().cloned())
            .ok_or("missing proven step")?;
        let login = self.login.as_mut().ok_or("missing login worker")?;
        if login.resume(&proved).is_err() {
            self.stage = Stage::Resumed;
            return login.poll();
        }
        self.stage = Stage::Resumed;
        Ok(Event::Waiting)
    }

    /// Records `kind` as the end and stops both workers.
    fn stop(&mut self, kind: u8) -> Result<(), String> {
        if matches!(self.stage, Stage::Ending | Stage::Done) {
            return Ok(());
        }
        self.end = Some(End {
            uncertain: false,
            kind,
            detail: 0,
        });
        if let Some(login) = &mut self.login {
            login.cancel()?;
        }
        if let Some(storage) = &mut self.storage {
            storage.cancel()?;
        }
        self.stage = Stage::Ending;
        Ok(())
    }

    /// Reports the end once both workers are reaped.
    fn settle(&mut self) -> Result<Event, String> {
        let done = |event: Event| matches!(event, Event::Ended(_) | Event::Complete);
        if let Some(login) = &mut self.login {
            if !done(login.poll()?) {
                return Ok(Event::Waiting);
            }
        }
        if let Some(storage) = &mut self.storage {
            if !done(storage.poll()?) {
                return Ok(Event::Waiting);
            }
        }
        let end = self.end.ok_or("missing disk proof end")?;
        let event = Event::Ended(end);
        self.stage = Stage::Done;
        self.terminal = Some(event.clone());
        Ok(event)
    }

    /// After peer failure only; prove neither worker can act again.
    pub fn reap_for_teardown(mut self) -> Result<(), String> {
        let storage = self
            .storage
            .take()
            .map_or(Ok(()), Storage::reap_for_teardown);
        let login = self.login.take().map_or(Ok(()), Login::reap_for_teardown);
        storage.and(login)
    }
}

#[cfg(test)]
impl Proving {
    pub(crate) fn stage_is_resumed(&self) -> bool {
        self.stage == Stage::Resumed
    }

    pub(crate) fn storage_pid(&self) -> Option<u32> {
        self.storage.as_ref().and_then(Storage::fixture_pid)
    }

    pub(crate) fn login_pid(&self) -> Option<u32> {
        self.login.as_ref().and_then(Login::fixture_pid)
    }
}

#[cfg(test)]
#[path = "../tests/proof.rs"]
pub(crate) mod tests;

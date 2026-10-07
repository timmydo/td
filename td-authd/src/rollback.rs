//! `deploy-rollback` (td-authd/DESIGN.md, "Elevation operations"): request
//! `1d` describes the pair the two boot selectors name, read through the
//! held volume, with a fresh nonce and approval key; after the person's
//! commit, root runs the one fixed td-boot helper on exactly that pair.

use crate::consent::{ApprovalKey, Operation, Request};
use crate::elevation::{self, Table};
use crate::login_tier;
use crate::unlock::Event;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// How long presentation and commit may take after selection.
const CONSENT: Duration = Duration::from_secs(120);

/// Request `1d`'s refusals before any description, each its `9d` byte.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Refusal {
    /// The operation slot is busy.
    Busy,
    /// The principal table refuses the owner or cannot be read.
    Principal,
    /// The selectors cannot be read or name one deployment.
    Selectors,
}

impl Refusal {
    pub fn answer(self) -> Vec<u8> {
        vec![
            0x9d,
            match self {
                Self::Busy => 0,
                Self::Principal => 1,
                Self::Selectors => 2,
            },
        ]
    }
}

/// The volume held open, and the pair its selectors named when read.
pub(crate) struct Selectors {
    volume: File,
    current: String,
    previous: String,
}

impl Selectors {
    pub fn read(volume: &Path) -> Result<Self, String> {
        let volume = login_tier::open_volume(volume)?;
        let (current, previous) = pair(&volume)?;
        Ok(Self {
            volume,
            current,
            previous,
        })
    }

    /// Whether both selectors, read afresh through the held volume, still
    /// name the pair read at selection.
    fn unchanged(&self) -> bool {
        pair(&self.volume)
            .is_ok_and(|(current, previous)| current == self.current && previous == self.previous)
    }
}

fn pair(volume: &File) -> Result<(String, String), String> {
    let current = login_tier::selected(volume, login_tier::CURRENT)?;
    let previous = login_tier::selected(volume, login_tier::PREVIOUS)?;
    if current == previous {
        return Err("both selectors name one deployment".into());
    }
    Ok((current, previous))
}

/// Before any description: the principal table must grant the owner the
/// operation, then the selectors must name two deployments.
pub(crate) fn admit(
    owner: u32,
    table: Result<Table, String>,
    volume: &Path,
) -> Result<Selectors, Refusal> {
    if !table.is_ok_and(|table| table.grants(owner, elevation::Operation::DeployRollback)) {
        return Err(Refusal::Principal);
    }
    Selectors::read(volume).map_err(|_| Refusal::Selectors)
}

/// Each digit `2` plus its own random byte modulo 8: 256 is a multiple of
/// 8, so every digit is equally likely.
fn approval_key(bytes: [u8; 2]) -> Result<ApprovalKey, String> {
    ApprovalKey::new(bytes.map(|byte| b'2' + byte % 8))
}

pub(crate) struct Rollback {
    request: Request,
    selectors: Selectors,
    presented: bool,
    committed: bool,
    deadline: Instant,
    child: Option<Child>,
    result: Option<bool>,
}

impl Rollback {
    /// The description of `selectors` for `owner`, under a fresh nonce
    /// and approval key, each drawn from `/dev/urandom`.
    pub fn start(owner: u32, selectors: Selectors) -> Result<Self, String> {
        let mut nonce = [0; 32];
        let mut key = [0; 2];
        File::open("/dev/urandom")
            .and_then(|mut random| {
                random.read_exact(&mut nonce)?;
                random.read_exact(&mut key)
            })
            .map_err(|e| format!("draw the rollback's nonce and key: {e}"))?;
        Self::drawn(owner, selectors, nonce, key)
    }

    fn drawn(
        owner: u32,
        selectors: Selectors,
        nonce: [u8; 32],
        key: [u8; 2],
    ) -> Result<Self, String> {
        let request = Request::new(
            nonce,
            owner,
            Operation::DeployRollback {
                key: approval_key(key)?,
                current: selectors.current.clone(),
                previous: selectors.previous.clone(),
            },
        )?;
        Ok(Self {
            request,
            selectors,
            presented: false,
            committed: false,
            deadline: Instant::now()
                .checked_add(CONSENT)
                .ok_or("rollback deadline overflow")?,
            child: None,
            result: None,
        })
    }

    pub fn request(&self) -> &Request {
        &self.request
    }

    fn admit(&self, request: &Request) -> Result<(), String> {
        if request != &self.request
            || self.result.is_some()
            || self.committed
            || Instant::now() >= self.deadline
        {
            return Err("stale rollback consent".into());
        }
        Ok(())
    }

    pub fn presented(&mut self, request: &Request) -> Result<(), String> {
        self.admit(request)?;
        if self.presented {
            return Err("duplicate rollback presentation".into());
        }
        self.presented = true;
        Ok(())
    }

    pub fn commit(&mut self, request: &Request) -> Result<(), String> {
        self.commit_with(request, |current, previous| {
            Command::new("/bin/td-boot")
                .args(["on-volume", "rollback", "/run/td-update", current, previous])
                .env_clear()
                .current_dir("/")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
        })
    }

    /// The commit consumes the confirmation before anything else, so a
    /// retry is a new request; the selectors are then read again, and only
    /// the approved pair still named starts the helper, on that pair.
    fn commit_with(
        &mut self,
        request: &Request,
        spawn: impl FnOnce(&str, &str) -> io::Result<Child>,
    ) -> Result<(), String> {
        self.admit(request)?;
        if !self.presented {
            return Err("rollback was not presented".into());
        }
        let Operation::DeployRollback {
            current, previous, ..
        } = self.request.operation()
        else {
            return Err("invalid rollback description".into());
        };
        self.committed = true;
        if !self.selectors.unchanged() {
            self.result = Some(false);
            return Ok(());
        }
        match spawn(current, previous) {
            Ok(child) => self.child = Some(child),
            Err(_) => self.result = Some(false),
        }
        Ok(())
    }

    pub fn cancel(&mut self) -> Result<(), String> {
        // After commit the helper's transaction runs to its end.
        if !self.committed {
            self.result = Some(false);
        }
        Ok(())
    }

    pub fn poll(&mut self) -> Result<Event, String> {
        if !self.committed && Instant::now() >= self.deadline {
            self.cancel()?;
        }
        if let Some(child) = &mut self.child {
            if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
                self.result = Some(status.success());
                self.child = None;
            }
        }
        Ok(match self.result {
            Some(true) => Event::Complete,
            Some(false) => Event::Failed("rollback cancelled, refused or failed".into()),
            None if self.committed => Event::Waiting,
            None if self.presented => Event::Commit(self.request.clone()),
            None => Event::Present(self.request.clone()),
        })
    }

    pub fn reap_for_teardown(mut self) -> Result<(), String> {
        if let Some(child) = &mut self.child {
            let stopped = child.kill();
            child
                .wait()
                .map_err(|e| format!("reap rollback child: {e}; stop: {stopped:?}"))?;
            self.child = None;
        }
        Ok(())
    }
}

impl Drop for Rollback {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
#[path = "../tests/rollback.rs"]
pub(crate) mod tests;

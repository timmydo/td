//! At most one concurrent secret operation per authenticated generation.

use crate::consent::{Request as Description, Role};
use crate::unlock::{Event, Unlock};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const CLEANUP_TIME: Duration = Duration::from_secs(2);

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Request {
    Prepare,
    Poll,
    Begin(Role),
    Presented(Description),
    Commit(Description),
    Cancel([u8; 32]),
}

impl Request {
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        match bytes {
            [0x10] => Ok(Self::Prepare),
            [0x11] => Ok(Self::Poll),
            [0x12, 1] => Ok(Self::Begin(Role::Primary)),
            [0x12, 2] => Ok(Self::Begin(Role::Recovery)),
            [0x13, rest @ ..] => Ok(Self::Presented(Description::decode(rest)?)),
            [0x14, rest @ ..] => Ok(Self::Commit(Description::decode(rest)?)),
            [0x15, rest @ ..] if rest.len() == 32 => {
                let nonce = rest.try_into().map_err(|_| "invalid cancellation nonce")?;
                if nonce == [0; 32] {
                    return Err("zero cancellation nonce".into());
                }
                Ok(Self::Cancel(nonce))
            }
            _ => Err("invalid secret session request".into()),
        }
    }
}

struct Cleanup {
    child: Option<Child>,
    deadline: Instant,
    expired: bool,
    terminal: Option<Result<bool, String>>,
}

fn cleanup_command(owner: u32) -> Command {
    let mut command = Command::new("/bin/td-secret");
    command
        .args(["lock-session", "--uid", &owner.to_string()])
        .env_clear()
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

impl Cleanup {
    fn start(mut command: Command) -> Result<Self, String> {
        let deadline = Instant::now()
            .checked_add(CLEANUP_TIME)
            .ok_or("session cleanup deadline overflow")?;
        let child = command
            .spawn()
            .map_err(|e| format!("start session cleanup: {e}"))?;
        Ok(Self {
            child: Some(child),
            deadline,
            expired: false,
            terminal: None,
        })
    }

    fn poll(&mut self) -> Result<bool, String> {
        if let Some(result) = &self.terminal {
            return result.clone();
        }
        let child = self.child.as_mut().ok_or("missing session cleanup child")?;
        if Instant::now() >= self.deadline && !self.expired {
            child
                .kill()
                .map_err(|e| format!("stop session cleanup: {e}"))?;
            self.expired = true;
        }
        let Some(status) = child
            .try_wait()
            .map_err(|e| format!("reap session cleanup: {e}"))?
        else {
            return Ok(false);
        };
        self.child = None;
        let result = if !status.success() || self.expired || Instant::now() >= self.deadline {
            Err("session cleanup failed; end authority generation".into())
        } else {
            Ok(true)
        };
        self.terminal = Some(result.clone());
        result
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

pub(crate) struct Session {
    owner: u32,
    activated: bool,
    prepared: bool,
    cleanup: Option<Cleanup>,
    operation: Option<Unlock>,
    event: Option<Event>,
}

impl Session {
    /// Construct only after the root launch admission has succeeded.
    pub fn new(owner: u32) -> Result<Self, String> {
        if owner != 1000 {
            return Err("unsupported secret session owner".into());
        }
        Ok(Self {
            owner,
            activated: false,
            prepared: false,
            cleanup: None,
            operation: None,
            event: None,
        })
    }

    pub fn answer(&mut self, request: Request) -> Result<Vec<u8>, String> {
        self.answer_with(request, cleanup_command, Unlock::start)
    }

    fn answer_with(
        &mut self,
        request: Request,
        cleanup: impl FnOnce(u32) -> Command,
        begin: impl FnOnce(u32, Role) -> Result<Unlock, String>,
    ) -> Result<Vec<u8>, String> {
        match request {
            Request::Prepare => {
                if self.activated {
                    return Err("secret session already prepared or preparing".into());
                }
                // Even a failed spawn leaves generation teardown responsible.
                self.activated = true;
                self.cleanup = Some(Cleanup::start(cleanup(self.owner))?);
                Ok(vec![0x90])
            }
            Request::Poll => {
                self.tick()?;
                self.reply()
            }
            Request::Begin(role) => {
                if !self.prepared || self.cleanup.is_some() || self.operation.is_some() {
                    return Err("secret session is not ready for a new operation".into());
                }
                let operation = begin(self.owner, role)?;
                let mut answer = vec![0x92];
                answer.extend_from_slice(&operation.request().encode());
                self.operation = Some(operation);
                self.event = Some(Event::Waiting);
                Ok(answer)
            }
            Request::Presented(description) => {
                self.operation
                    .as_mut()
                    .ok_or("no secret operation")?
                    .presented(&description)?;
                self.event = Some(Event::Waiting);
                Ok(vec![0x93])
            }
            Request::Commit(description) => {
                self.operation
                    .as_mut()
                    .ok_or("no secret operation")?
                    .commit(&description)?;
                self.event = Some(Event::Waiting);
                Ok(vec![0x94])
            }
            Request::Cancel(nonce) => {
                let operation = self.operation.as_mut().ok_or("no secret operation")?;
                if operation.request().nonce() != &nonce {
                    return Err("stale secret cancellation".into());
                }
                match &self.event {
                    Some(Event::Complete) => Ok(vec![0x95, 1]),
                    Some(Event::Failed(_)) => Ok(vec![0x95, 2]),
                    _ => {
                        operation.cancel("physical attention cancelled")?;
                        self.event = Some(Event::Waiting);
                        Ok(vec![0x95, 0])
                    }
                }
            }
        }
    }

    /// Terminal traffic also advances watchdogs without consuming events.
    pub fn tick(&mut self) -> Result<(), String> {
        if let Some(cleanup) = &mut self.cleanup {
            if cleanup.poll()? {
                self.cleanup = None;
                self.prepared = true;
            }
        }
        if let Some(operation) = &mut self.operation {
            self.event = Some(operation.poll()?);
        }
        Ok(())
    }

    fn reply(&mut self) -> Result<Vec<u8>, String> {
        let Some(operation) = &self.operation else {
            return Ok(vec![
                0x91,
                if self.prepared {
                    2
                } else if self.activated {
                    1
                } else {
                    0
                },
            ]);
        };
        let mut answer = vec![0x91];
        let event = self
            .event
            .as_ref()
            .ok_or("missing secret operation event")?;
        answer.push(match event {
            Event::Waiting => 3,
            Event::Present(_) => 4,
            Event::Commit(_) => 5,
            Event::Complete => 6,
            Event::Failed(_) => 7,
        });
        answer.extend_from_slice(&operation.request().encode());
        if matches!(event, Event::Complete | Event::Failed(_)) {
            self.operation = None;
            self.event = None;
        }
        Ok(answer)
    }

    /// The peer is already failed. Never admit another request during teardown.
    pub fn close(&mut self) -> Result<(), String> {
        self.close_with(cleanup_command)
    }

    fn close_with(&mut self, command: impl FnOnce(u32) -> Command) -> Result<(), String> {
        if !self.activated {
            return Ok(());
        }
        if let Some(operation) = self.operation.take() {
            // The prior operation may retain a fatal cleanup result. Only
            // proven child death, not replaying that result, permits relocking.
            operation.reap_for_teardown()?;
        }
        // Dropping pending generation cleanup also reaps before replacement.
        self.cleanup = None;
        self.prepared = false;
        let mut cleanup = Cleanup::start(command(self.owner))?;
        while !cleanup.poll()? {
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "../tests/session.rs"]
mod tests;

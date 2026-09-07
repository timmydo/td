//! Nonblocking ownership of one private root token child.

use crate::consent::{Enrollment, Operation, Platform, Recovery, Request, Role};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const LIMIT: usize = 289;
const FRAME_TIME: Duration = Duration::from_secs(5);
const ACK_TIME: Duration = Duration::from_secs(3);
const LIFETIME: Duration = Duration::from_secs(120);
const CLEANUP_TIME: Duration = Duration::from_secs(2);

fn deadline_after(duration: Duration) -> Result<Instant, String> {
    Instant::now()
        .checked_add(duration)
        .ok_or_else(|| "unlock deadline overflow".into())
}

struct Wire {
    stream: UnixStream,
    input: Vec<u8>,
    output: Vec<u8>,
    sent: usize,
    deadline: Option<Instant>,
}

impl Wire {
    fn new(stream: UnixStream) -> Result<Self, String> {
        stream.set_nonblocking(true).map_err(|e| e.to_string())?;
        Ok(Self {
            stream,
            input: Vec::with_capacity(LIMIT + 2),
            output: Vec::with_capacity(LIMIT + 2),
            sent: 0,
            deadline: None,
        })
    }

    fn queue(&mut self, bytes: &[u8]) -> Result<(), String> {
        if !self.output.is_empty() || bytes.is_empty() || bytes.len() > LIMIT {
            return Err("invalid private unlock send".into());
        }
        self.output
            .extend_from_slice(&(bytes.len() as u16).to_be_bytes());
        self.output.extend_from_slice(bytes);
        self.sent = 0;
        self.deadline = Some(deadline_after(FRAME_TIME)?);
        Ok(())
    }

    fn expected(&self) -> Result<usize, String> {
        let Some(header) = self.input.get(..2) else {
            return Ok(2);
        };
        let size = usize::from(u16::from_be_bytes(
            header.try_into().map_err(|_| "invalid frame header")?,
        ));
        if size == 0 || size > LIMIT {
            return Err("invalid private unlock frame length".into());
        }
        Ok(size + 2)
    }

    fn take_frame(&mut self, expected: usize) -> Result<Option<Vec<u8>>, String> {
        if self.input.len() != expected || expected <= 2 {
            return Ok(None);
        }
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err("private unlock frame expired".into());
        }
        let result = self.input.get(2..).ok_or("invalid frame body")?.to_vec();
        self.input.clear();
        self.deadline = None;
        Ok(Some(result))
    }

    fn poll(&mut self) -> Result<Option<Vec<u8>>, String> {
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err("private unlock frame expired".into());
        }
        // A ready descriptor or repeated interrupt cannot monopolize the caller.
        for _ in 0..4 {
            if !self.output.is_empty() {
                let pending = self.output.get(self.sent..).ok_or("invalid send cursor")?;
                match self.stream.write(pending) {
                    Ok(0) => return Err("private unlock child stopped reading".into()),
                    Ok(count) => self.sent += count,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(None),
                    Err(error) => return Err(format!("write private unlock child: {error}")),
                }
                if self.sent == self.output.len() {
                    self.output.clear();
                    self.sent = 0;
                    self.deadline = None;
                }
                continue;
            }
            let expected = self.expected()?;
            if let Some(frame) = self.take_frame(expected)? {
                return Ok(Some(frame));
            }
            let mut buffer = [0; LIMIT + 2];
            let remaining = expected
                .checked_sub(self.input.len())
                .ok_or("invalid frame cursor")?;
            let buffer = buffer.get_mut(..remaining).ok_or("oversized frame")?;
            match self.stream.read(buffer) {
                Ok(0) => return Err("private unlock child disconnected".into()),
                Ok(count) => {
                    if self.deadline.is_none() {
                        self.deadline = Some(deadline_after(FRAME_TIME)?);
                    }
                    self.input
                        .extend_from_slice(buffer.get(..count).ok_or("invalid read count")?);
                    if let Some(frame) = self.take_frame(self.expected()?)? {
                        return Ok(Some(frame));
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(None),
                Err(error) => return Err(format!("read private unlock child: {error}")),
            }
        }
        Ok(None)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Event {
    Waiting,
    Present(Request),
    Commit(Request),
    Complete,
    Failed(String),
}

#[derive(PartialEq, Eq)]
enum Phase {
    Presentation,
    Presented,
    Assertion,
    Commit,
    Completion,
    Exit,
    Stopping,
    Locking,
    Done,
}

pub(crate) struct Unlock {
    request: Request,
    child: Option<Child>,
    wire: Option<Wire>,
    phase: Phase,
    round: Vec<u8>,
    deadline: Instant,
    failure: Option<String>,
    terminal: Option<Result<Event, String>>,
    cleanup_expired: bool,
    acknowledgement_deadline: Option<Instant>,
}

fn sanitized(command: &mut Command) -> &mut Command {
    command
        .env_clear()
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
}

fn command(verb: &str, owner: u32) -> Command {
    let mut command = Command::new("/bin/td-secret");
    sanitized(&mut command).args([verb, "--uid", &owner.to_string()]);
    command
}

fn operation_verb(operation: &Operation) -> Result<&'static str, String> {
    match operation {
        Operation::Unlock { .. } => Ok("unlock-operation"),
        Operation::Enroll {
            step: Enrollment::CreatePrimary,
            ..
        } => Ok("enroll-operation"),
        _ => Err("unsupported private token operation".into()),
    }
}

impl Unlock {
    /// Call only after root startup and paired-session admission.
    pub fn start(owner: u32, role: Role) -> Result<Self, String> {
        Self::start_operation(owner, Operation::Unlock { role })
    }

    pub fn start_enrollment(owner: u32, recovery: Recovery) -> Result<Self, String> {
        Self::start_operation(
            owner,
            Operation::Enroll {
                platform: Platform::TpmPcr7,
                recovery,
                step: Enrollment::CreatePrimary,
            },
        )
    }

    fn start_operation(owner: u32, operation: Operation) -> Result<Self, String> {
        let verb = operation_verb(&operation)?;
        if owner != 1000 {
            return Err("token supervisor requires the configured graphical session".into());
        }
        let mut nonce = [0; 32];
        File::open("/dev/urandom")
            .and_then(|mut file| file.read_exact(&mut nonce))
            .map_err(|_| "read token request randomness")?;
        let request = Request::new(nonce, owner, operation)?;
        Self::spawn(request, command(verb, owner))
    }

    fn spawn(request: Request, mut command: Command) -> Result<Self, String> {
        let deadline = deadline_after(LIFETIME)?;
        let (parent, child) = UnixStream::pair().map_err(|e| e.to_string())?;
        let mut wire = Wire::new(parent)?;
        wire.queue(&request.encode())?;
        let child = command
            .stdin(Stdio::from(OwnedFd::from(child)))
            .spawn()
            .map_err(|e| format!("start private unlock child: {e}"))?;
        Ok(Self {
            request,
            child: Some(child),
            wire: Some(wire),
            phase: Phase::Presentation,
            round: Vec::new(),
            deadline,
            failure: None,
            terminal: None,
            cleanup_expired: false,
            acknowledgement_deadline: None,
        })
    }

    /// After peer failure only; prove this child cannot publish again.
    pub fn reap_for_teardown(mut self) -> Result<(), String> {
        self.wire = None;
        if let Some(child) = &mut self.child {
            let stopped = child.kill();
            if let Err(error) = child.wait() {
                return Err(format!(
                    "reap unlock generation child: {error}; kill result: {stopped:?}"
                ));
            }
            self.child = None;
        }
        Ok(())
    }

    pub fn request(&self) -> &Request {
        &self.request
    }

    pub fn presented(&mut self, request: &Request) -> Result<(), String> {
        self.acknowledge(request, Phase::Presented)
    }

    pub fn commit(&mut self, request: &Request) -> Result<(), String> {
        self.acknowledge(request, Phase::Commit)
    }

    fn acknowledge(&mut self, request: &Request, expected: Phase) -> Result<(), String> {
        if self.phase != expected {
            return Err("unlock acknowledgement is out of order".into());
        }
        let tag = match expected {
            Phase::Presented => 0x11,
            Phase::Commit => 0x13,
            _ => return Err("unlock acknowledgement is out of order".into()),
        };
        if request != &self.request
            || Instant::now() >= self.deadline
            || self
                .acknowledgement_deadline
                .is_none_or(|deadline| Instant::now() >= deadline)
        {
            self.cancel("stale unlock acknowledgement")?;
            return Err("stale unlock acknowledgement".into());
        }
        let first = self.round.first_mut().ok_or("missing unlock round")?;
        *first = tag;
        self.wire
            .as_mut()
            .ok_or("missing unlock endpoint")?
            .queue(&self.round)?;
        self.round.clear();
        self.acknowledgement_deadline = None;
        self.phase = if tag == 0x11 {
            Phase::Assertion
        } else {
            Phase::Completion
        };
        Ok(())
    }

    pub fn cancel(&mut self, reason: &str) -> Result<(), String> {
        if matches!(self.phase, Phase::Stopping | Phase::Locking | Phase::Done) {
            return Ok(());
        }
        self.failure = Some(reason.into());
        self.wire = None;
        self.round.clear();
        if let Some(child) = &mut self.child {
            child
                .kill()
                .map_err(|e| format!("stop private unlock child: {e}"))?;
        }
        self.phase = Phase::Stopping;
        Ok(())
    }

    pub fn poll(&mut self) -> Result<Event, String> {
        self.poll_with(|owner| command("lock-session", owner))
    }

    fn poll_with(&mut self, cleanup: impl FnOnce(u32) -> Command) -> Result<Event, String> {
        if self.phase == Phase::Done {
            return self
                .terminal
                .clone()
                .ok_or("missing unlock terminal result")?;
        }
        if matches!(self.phase, Phase::Stopping | Phase::Locking) {
            return self.poll_cleanup(cleanup);
        }
        if Instant::now() >= self.deadline
            || self
                .acknowledgement_deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.cancel("private unlock operation expired")?;
            return Ok(Event::Waiting);
        }
        let result = self.poll_active();
        match result {
            Ok(event) => Ok(event),
            Err(reason) => {
                self.cancel(&reason)?;
                Ok(Event::Waiting)
            }
        }
    }

    fn poll_active(&mut self) -> Result<Event, String> {
        if self.phase == Phase::Presented {
            return Ok(Event::Present(self.request.clone()));
        }
        if self.phase == Phase::Commit {
            return Ok(Event::Commit(self.request.clone()));
        }
        if self.phase == Phase::Exit {
            let status = self
                .child
                .as_mut()
                .ok_or("missing unlock child")?
                .try_wait()
                .map_err(|e| format!("wait for private unlock child: {e}"))?;
            return match status {
                None => Ok(Event::Waiting),
                Some(_) if Instant::now() >= self.deadline => {
                    Err("private unlock operation expired".into())
                }
                Some(status) if status.success() => {
                    self.child = None;
                    self.wire = None;
                    self.finish(Ok(Event::Complete))
                }
                Some(_) => Err("private unlock child failed after completion frame".into()),
            };
        }
        let Some(frame) = self
            .wire
            .as_mut()
            .ok_or("missing unlock endpoint")?
            .poll()?
        else {
            return Ok(Event::Waiting);
        };
        if self.phase == Phase::Completion {
            if frame != [0x14] {
                return Err("invalid unlock completion".into());
            }
            self.phase = Phase::Exit;
            return Ok(Event::Waiting);
        }
        let (tag, expected) = if self.phase == Phase::Presentation {
            (0x10, self.request.clone())
        } else if let Some(next) = self.request.following_enrollment_step()? {
            (0x10, next)
        } else {
            (0x12, self.request.clone())
        };
        if frame.first() != Some(&tag) || frame.get(33..) != Some(expected.encode().as_slice()) {
            return Err("private token prompt changed its request or skipped a step".into());
        }
        self.request = expected;
        self.round = frame;
        self.acknowledgement_deadline = Some(deadline_after(ACK_TIME)?);
        self.phase = if tag == 0x10 {
            Phase::Presented
        } else {
            Phase::Commit
        };
        Ok(if tag == 0x10 {
            Event::Present(self.request.clone())
        } else {
            Event::Commit(self.request.clone())
        })
    }

    fn poll_cleanup(&mut self, cleanup: impl FnOnce(u32) -> Command) -> Result<Event, String> {
        if self.phase == Phase::Locking && Instant::now() >= self.deadline && !self.cleanup_expired
        {
            self.child
                .as_mut()
                .ok_or("missing unlock cleanup child")?
                .kill()
                .map_err(|e| format!("stop expired unlock cleanup: {e}"))?;
            self.cleanup_expired = true;
        }
        let Some(status) = self
            .child
            .as_mut()
            .ok_or("missing stopped unlock child")?
            .try_wait()
            .map_err(|e| format!("reap private unlock child: {e}"))?
        else {
            return Ok(Event::Waiting);
        };
        self.child = None;
        if self.phase == Phase::Stopping {
            let deadline = match deadline_after(CLEANUP_TIME) {
                Ok(deadline) => deadline,
                Err(reason) => return self.cleanup_failed(reason),
            };
            match cleanup(self.request.owner()).spawn() {
                Ok(child) => self.child = Some(child),
                Err(error) => return self.cleanup_failed(format!("start unlock cleanup: {error}")),
            }
            self.deadline = deadline;
            self.phase = Phase::Locking;
            return Ok(Event::Waiting);
        }
        if !status.success() || self.cleanup_expired || Instant::now() >= self.deadline {
            return self
                .cleanup_failed("unlock cleanup did not finish successfully in time".into());
        }
        let reason = self
            .failure
            .take()
            .unwrap_or_else(|| "private unlock cancelled".into());
        self.finish(Ok(Event::Failed(reason)))
    }

    fn cleanup_failed(&mut self, detail: String) -> Result<Event, String> {
        let reason = self
            .failure
            .take()
            .unwrap_or_else(|| "private unlock cancelled".into());
        self.finish(Err(format!(
            "{reason}; {detail}; session cleanup failed; end authority generation"
        )))
    }

    fn finish(&mut self, result: Result<Event, String>) -> Result<Event, String> {
        self.phase = Phase::Done;
        self.terminal = Some(result.clone());
        result
    }
}

impl Drop for Unlock {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
impl Unlock {
    pub(crate) fn fixture_pid(&self) -> Option<u32> {
        self.child.as_ref().map(Child::id)
    }

    pub(crate) fn fixture(request: Request, command: Command) -> Result<Self, String> {
        Self::spawn(request, command)
    }
}

#[cfg(test)]
#[path = "../tests/unlock.rs"]
mod tests;

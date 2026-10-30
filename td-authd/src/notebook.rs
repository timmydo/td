//! One pinned notebook window and dedicated vault worker per generation.

use crate::consent::{Notebook, Operation, Request};
use crate::login::Pin;
use crate::notebook_token::Token;
use crate::notebook_wire::{Bytes, PendingWire, Reader};
use crate::unlock::Event;
use std::fs::File;
use std::io::Read;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Process {
    child: Child,
    wire: PendingWire,
}
impl Process {
    fn start(mut command: Command) -> Result<Self, String> {
        let (root, child) = UnixStream::pair().map_err(|_| "create notebook capability")?;
        let wire = PendingWire::new(root)?;
        let child = command
            .env_clear()
            .current_dir("/")
            .stdin(Stdio::from(OwnedFd::from(child)))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| "start notebook process")?;
        Ok(Self { child, wire })
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Clone, Copy)]
enum Waiting {
    Gui,
    Heartbeat,
}

pub(crate) struct Bridge {
    owner: u32,
    gui: Process,
    service: Process,
    pending: Option<Bytes>,
    waiting: Option<Waiting>,
    heartbeat: Instant,
    was_unlocked: bool,
}

fn action(bytes: &[u8]) -> Option<u8> {
    match bytes {
        [2, 0] => Some(1),
        [2, 1] => Some(2),
        [3, ..] => Some(3),
        [8, ..] => Some(4),
        [9] => Some(5),
        [10, ..] => Some(6),
        _ => None,
    }
}

impl Bridge {
    pub fn start(owner: u32) -> Result<Self, String> {
        if owner != 1000 {
            return Err("unsupported notebook owner".into());
        }
        let registry = crate::secret_intake::principals::Registry::load()?;
        registry.verify_installed_accounts(&registry)?;
        if !registry.notebooks().any(|(account, _)| account == owner) {
            return Err("notebook service account is not provisioned".into());
        }
        let account = crate::primary_account::load().map_err(|_| "read notebook owner")?;
        let mut command = Command::new("/bin/td-login");
        command.args([
            "exec-service-as",
            &format!("tdv{owner}"),
            "--",
            "/bin/td-secret",
            "notebook-service",
            "--uid",
            &owner.to_string(),
        ]);
        let service = Process::start(command)?;
        let mut command = Command::new("/bin/td-login");
        command.args([
            "exec-as",
            account.name(),
            "--",
            "/bin/td-authd",
            "notebook-ui",
            &owner.to_string(),
        ]);
        let gui = Process::start(command)?;
        Ok(Self {
            owner,
            gui,
            service,
            pending: None,
            waiting: None,
            heartbeat: Instant::now(),
            was_unlocked: false,
        })
    }

    pub fn pending(&self) -> bool {
        self.waiting.is_none()
            && self
                .pending
                .as_ref()
                .is_some_and(|p| action(&p.0).is_some())
    }

    pub fn poll(&mut self) -> Result<(), String> {
        if let Some(reply) = self.service.wire.poll()? {
            match self
                .waiting
                .take()
                .ok_or("unsolicited notebook service result")?
            {
                Waiting::Heartbeat if matches!(reply.0.as_slice(), [0, 0..=1]) => {
                    if reply.0 == [0, 0] && self.was_unlocked {
                        return Err("notebook vault expired".into());
                    }
                }
                Waiting::Heartbeat => return Err("invalid notebook heartbeat".into()),
                Waiting::Gui => {
                    if !matches!(reply.0.first(), Some(0 | 1)) {
                        return Err("unexpected notebook service result".into());
                    }
                    self.gui.wire.queue(reply)?;
                }
            }
            self.heartbeat = Instant::now();
        }
        if let Some(request) = self.gui.wire.poll()? {
            if self.pending.is_some() || matches!(self.waiting, Some(Waiting::Gui)) {
                return Err("concurrent notebook window requests".into());
            }
            let tag = request.0.first().copied().ok_or("empty notebook request")?;
            if tag == 11 {
                self.was_unlocked = false;
            }
            if !matches!(tag, 1..=11 | 13 | 14) {
                return Err("notebook window requested another capability".into());
            }
            if action(&request.0).is_some() {
                self.pending = Some(request);
                self.gui.wire.queue(Bytes(vec![0x7e]))?;
            } else {
                // An outstanding heartbeat must finish before the next request.
                if self.waiting.is_some() {
                    self.pending = Some(request);
                } else {
                    self.service.wire.queue(request)?;
                    self.waiting = Some(Waiting::Gui);
                }
            }
        }
        if self.waiting.is_none() {
            if self
                .pending
                .as_ref()
                .is_some_and(|p| action(&p.0).is_none())
            {
                let pending = self.pending.take().ok_or("missing notebook request")?;
                self.service.wire.queue(pending)?;
                self.waiting = Some(Waiting::Gui);
            } else if self.heartbeat.elapsed() >= Duration::from_secs(10) {
                self.service.wire.queue(Bytes(vec![13]))?;
                self.waiting = Some(Waiting::Heartbeat);
            }
        }
        Ok(())
    }

    pub fn begin(mut self) -> Result<Ceremony, String> {
        if self.waiting.is_some() {
            return Err("notebook service is busy".into());
        }
        let pending = self.pending.take().ok_or("no notebook operation waits")?;
        let action = action(&pending.0).ok_or("not a notebook token operation")?;
        let mut hash = crate::sha256::Sha256::new();
        hash.update(&pending.0);
        let digest = hash.finalize();
        let mut nonce = [0; 32];
        File::open("/dev/urandom")
            .and_then(|mut file| file.read_exact(&mut nonce))
            .map_err(|_| "read notebook operation randomness")?;
        self.service.wire.queue(pending)?;
        Ok(Ceremony {
            bridge: Some(self),
            token: None,
            nonce,
            action,
            digest,
            deadline: Instant::now()
                .checked_add(crate::consent::NOTEBOOK_CEILING)
                .ok_or("notebook operation deadline overflow")?,
            event: Event::Waiting,
            description: None,
            complete: false,
        })
    }
}

pub(crate) struct Ceremony {
    bridge: Option<Bridge>,
    token: Option<Token>,
    nonce: [u8; 32],
    action: u8,
    digest: [u8; 32],
    deadline: Instant,
    event: Event,
    description: Option<Request>,
    complete: bool,
}
impl Ceremony {
    pub fn request(&self) -> Option<&Request> {
        self.description.as_ref()
    }
    pub fn nonce(&self) -> &[u8; 32] {
        &self.nonce
    }
    pub fn presented(&mut self, request: &Request) -> Result<(), String> {
        self.token
            .as_mut()
            .ok_or("no notebook token presentation")?
            .presented(request)
    }
    pub fn pin(&mut self, request: &Request, pin: Pin) -> Result<bool, String> {
        self.token
            .as_mut()
            .ok_or("no notebook PIN step")?
            .pin(request, pin)
    }
    pub fn commit(&mut self, _: &Request) -> Result<(), String> {
        Err("notebook does not take an elevation commit".into())
    }
    pub fn cancel(&mut self) -> Result<(), String> {
        if let Some(token) = self.token.take() {
            token.reap_for_teardown()?;
        }
        self.bridge = None;
        self.complete = true;
        self.event = Event::Failed("notebook operation cancelled; vault locked".into());
        Ok(())
    }
    pub fn poll(&mut self) -> Result<Event, String> {
        if self.complete {
            return Ok(self.event.clone());
        }
        if Instant::now() >= self.deadline {
            self.cancel()?;
            return Ok(self.event.clone());
        }
        match self.advance() {
            Ok(event) => {
                self.event = event.clone();
                Ok(event)
            }
            Err(_) => {
                self.cancel()?;
                Ok(self.event.clone())
            }
        }
    }
    fn advance(&mut self) -> Result<Event, String> {
        let bridge = self.bridge.as_mut().ok_or("missing notebook processes")?;
        if bridge.gui.wire.poll()?.is_some() {
            return Err("window changed a pending notebook operation".into());
        }
        if let Some(token) = &mut self.token {
            let event = token.poll()?;
            self.description = Some(token.request().clone());
            match event {
                Event::Complete => {
                    let result = token
                        .take_result()
                        .ok_or("notebook token has no proved result")?;
                    bridge.service.wire.queue(result)?;
                    self.token = None;
                }
                Event::Failed(_) | Event::Ended(_) => return Err("notebook token failed".into()),
                other => return Ok(other),
            }
        }
        if let Some(reply) = bridge.service.wire.poll()? {
            match reply.0.first() {
                Some(0 | 1) => {
                    if reply.0.first() == Some(&0) {
                        bridge.was_unlocked = true;
                    }
                    bridge.gui.wire.queue(reply)?;
                    bridge.heartbeat = Instant::now();
                    self.complete = true;
                    return Ok(Event::Complete);
                }
                Some(0x30 | 0x31) => {
                    let mut input = Reader::new(&reply.0);
                    let method = input.byte()?;
                    let purpose = input.byte()?;
                    let role = input.byte()?;
                    let mut hash = crate::sha256::Sha256::new();
                    hash.update(&reply.0);
                    let request = Request::new(
                        self.nonce,
                        bridge.owner,
                        Operation::Notebook(Notebook {
                            method,
                            action: self.action,
                            purpose,
                            role,
                            step: 0,
                            operation: self.digest,
                            token: hash.finalize(),
                        }),
                    )?;
                    self.description = Some(request.clone());
                    self.token = Some(Token::start(request, &reply.0)?);
                }
                _ => return Err("invalid notebook operation result".into()),
            }
        }
        Ok(Event::Waiting)
    }
    pub fn take_bridge(&mut self) -> Option<Bridge> {
        if self.complete && self.event == Event::Complete {
            self.bridge.take()
        } else {
            None
        }
    }
    pub fn reap_for_teardown(mut self) -> Result<(), String> {
        self.cancel()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    #[ignore = "child process held until the root notebook owner revokes it"]
    fn idle_process() {
        let mut byte = [0];
        let _ = std::io::stdin().read_exact(&mut byte);
    }

    pub(crate) fn ceremony() -> (Ceremony, [u32; 2]) {
        let (mut bridge, pids) = bridge();
        bridge.pending = Some(Bytes(vec![2, 0]));
        (bridge.begin().unwrap(), pids)
    }

    pub(crate) fn bridge() -> (Bridge, [u32; 2]) {
        let process = || {
            let mut command = Command::new(std::env::current_exe().unwrap());
            command.args(["--exact", "notebook::tests::idle_process", "--ignored"]);
            Process::start(command).unwrap()
        };
        let gui = process();
        let service = process();
        let pids = [gui.child.id(), service.child.id()];
        (
            Bridge {
                owner: 1000,
                gui,
                service,
                pending: None,
                waiting: None,
                heartbeat: Instant::now(),
                was_unlocked: true,
            },
            pids,
        )
    }
}

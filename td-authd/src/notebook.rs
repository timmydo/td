//! One pinned notebook window and dedicated vault worker per generation.

use crate::consent::{Notebook, Operation, Request};
use crate::login::Pin;
use crate::notebook_token::Token;
use crate::notebook_wire::{
    put_binding, read_binding, Bytes, MailBinding, PendingWire, Reader, MAIL_PENDING,
};
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
    Mail(u64),
}

pub(crate) struct Bridge {
    owner: u32,
    gui: Process,
    service: Process,
    pending: Option<Bytes>,
    waiting: Option<Waiting>,
    heartbeat: Instant,
    was_unlocked: bool,
    mail_revoked: bool,
    requests: Vec<MailBinding>,
    mail_reply: Option<(u64, Bytes)>,
    binding_update: Option<MailBinding>,
    dismissed: Option<MailBinding>,
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
            mail_revoked: false,
            requests: Vec::new(),
            mail_reply: None,
            binding_update: None,
            dismissed: None,
        })
    }

    pub fn requests(&mut self, requests: &[MailBinding]) {
        if self.requests != requests {
            self.requests = requests.iter().take(MAIL_PENDING).cloned().collect();
        }
    }
    pub fn take_mail_revoked(&mut self) -> bool {
        std::mem::take(&mut self.mail_revoked)
    }
    pub fn take_mail_reply(&mut self) -> Option<(u64, Bytes)> {
        self.mail_reply.take()
    }
    pub fn take_dismissed(&mut self) -> Option<MailBinding> {
        self.dismissed.take()
    }
    pub fn mail(&mut self, id: u64, binding: &MailBinding) -> Result<bool, String> {
        if !self.was_unlocked
            || self.waiting.is_some()
            || self.pending.is_some()
            || self.mail_reply.is_some()
        {
            return Ok(false);
        }
        let mut request = Bytes(vec![12]);
        put_binding(&mut request, binding)?;
        self.service.wire.queue(request)?;
        self.waiting = Some(Waiting::Mail(id));
        Ok(true)
    }
    fn send_gui(&mut self, request: Bytes) -> Result<(), String> {
        self.binding_update = None;
        if request.0.first() == Some(&14) {
            let mut input = Reader::new(&request.0);
            input.byte()?;
            let _: [u8; 16] = input.array()?;
            input.number()?;
            let allow = input.boolean()?;
            let binding = read_binding(&mut input)?;
            input.finish()?;
            if allow {
                self.binding_update = Some(binding);
            }
        }
        self.service.wire.queue(request)?;
        self.waiting = Some(Waiting::Gui);
        Ok(())
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
                Waiting::Mail(id) => {
                    let mut input = Reader::new(&reply.0);
                    if !matches!(input.byte()?, 0 | 1) {
                        return Err("invalid notebook mail result".into());
                    }
                    input.field(4096)?;
                    input.finish()?;
                    if self.was_unlocked {
                        self.mail_reply = Some((id, reply));
                    }
                }
                Waiting::Gui => {
                    if !matches!(reply.0.first(), Some(0 | 1)) {
                        return Err("unexpected notebook service result".into());
                    }
                    if reply.0.first() == Some(&0) {
                        self.dismissed = self.binding_update.take();
                    } else {
                        self.binding_update = None;
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
            if tag == 15 {
                if request.0 != [15] {
                    return Err("invalid notebook request list".into());
                }
                let mut reply = Bytes(vec![0, self.requests.len() as u8]);
                for binding in &self.requests {
                    put_binding(&mut reply, binding)?;
                }
                self.gui.wire.queue(reply)?;
            } else {
                if tag == 11 {
                    self.was_unlocked = false;
                    self.mail_reply = None;
                    self.mail_revoked = true;
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
                        self.send_gui(request)?;
                    }
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
                self.send_gui(pending)?;
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
                mail_revoked: false,
                requests: Vec::new(),
                mail_reply: None,
                binding_update: None,
                dismissed: None,
            },
            pids,
        )
    }
    #[test]
    fn mail_replies_return_only_to_the_admitted_request_and_gui_cannot_read_them() {
        use crate::notebook_wire::{field, Wire};
        let (mut bridge, _) = bridge();
        let (root, peer) = UnixStream::pair().unwrap();
        let _service_hold =
            std::mem::replace(&mut bridge.service.wire, PendingWire::new(root).unwrap());
        let mut service = Wire::new(peer).unwrap();
        let (root, peer) = UnixStream::pair().unwrap();
        let _gui_hold = std::mem::replace(&mut bridge.gui.wire, PendingWire::new(root).unwrap());
        let mut gui = PendingWire::new(peer).unwrap();
        let binding = MailBinding::new(
            "work".into(),
            "https://mail.invalid/jmap".into(),
            "me".into(),
        )
        .unwrap();
        assert!(bridge.mail(42, &binding).unwrap());
        assert!(!bridge.mail(43, &binding).unwrap());
        bridge.poll().unwrap();
        let request = service.receive(Duration::from_secs(1)).unwrap();
        let mut input = Reader::new(&request.0);
        assert_eq!(input.byte().unwrap(), 12);
        assert_eq!(read_binding(&mut input).unwrap(), binding);
        input.finish().unwrap();
        let mut reply = Bytes(vec![0]);
        field(&mut reply, b"secret-token").unwrap();
        service.send(&reply.0).unwrap();
        bridge.poll().unwrap();
        let (id, response) = bridge.take_mail_reply().unwrap();
        assert_eq!(id, 42);
        assert_eq!(response.0, reply.0);
        assert!(gui.poll().unwrap().is_none());
        gui.queue(Bytes(vec![12])).unwrap();
        gui.poll().unwrap();
        assert!(bridge.poll().is_err());
    }

    #[test]
    fn locked_bridge_refuses_mail_without_queuing_a_service_request() {
        let (mut bridge, _) = bridge();
        bridge.was_unlocked = false;
        let binding = MailBinding::new(
            "work".into(),
            "https://mail.invalid/jmap".into(),
            "me".into(),
        )
        .unwrap();
        assert!(!bridge.mail(1, &binding).unwrap());
        assert!(bridge.waiting.is_none());
        assert!(!bridge.service.wire.sending());
    }
    #[test]
    fn gui_lock_discards_an_inflight_mail_reply() {
        use crate::notebook_wire::{field, Wire};
        let (mut bridge, _) = bridge();
        let (root, peer) = UnixStream::pair().unwrap();
        let _service_hold =
            std::mem::replace(&mut bridge.service.wire, PendingWire::new(root).unwrap());
        let mut service = Wire::new(peer).unwrap();
        let (root, peer) = UnixStream::pair().unwrap();
        let _gui_hold = std::mem::replace(&mut bridge.gui.wire, PendingWire::new(root).unwrap());
        let mut gui = Wire::new(peer).unwrap();
        let binding = MailBinding::new(
            "work".into(),
            "https://mail.invalid/jmap".into(),
            "me".into(),
        )
        .unwrap();
        assert!(bridge.mail(42, &binding).unwrap());
        bridge.poll().unwrap();
        service.receive(Duration::from_secs(1)).unwrap();
        gui.send(&[11]).unwrap();
        bridge.poll().unwrap();
        assert!(!bridge.was_unlocked);
        let mut reply = Bytes(vec![0]);
        field(&mut reply, b"secret-token").unwrap();
        service.send(&reply.0).unwrap();
        bridge.poll().unwrap();
        assert!(bridge.take_mail_reply().is_none());
        bridge.poll().unwrap();
        assert_eq!(service.receive(Duration::from_secs(1)).unwrap().0, [11]);
    }
    #[test]
    fn request_list_poll_still_schedules_due_vault_heartbeat() {
        use crate::notebook_wire::Wire;
        let (mut bridge, _) = bridge();
        let (root, peer) = UnixStream::pair().unwrap();
        let _gui_hold = std::mem::replace(&mut bridge.gui.wire, PendingWire::new(root).unwrap());
        let mut gui = Wire::new(peer).unwrap();
        bridge.heartbeat = Instant::now() - Duration::from_secs(11);
        gui.send(&[15]).unwrap();
        bridge.poll().unwrap();
        assert!(matches!(bridge.waiting, Some(Waiting::Heartbeat)));
    }
}

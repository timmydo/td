//! One pinned notebook window and dedicated vault worker per generation.

use crate::consent::{Notebook, Operation, Request};
use crate::login::Pin;
use crate::notebook_token::Token;
use crate::notebook_wire::{
    put_binding, read_binding, Bytes, MailBinding, PendingWire, Reader, MAIL_PENDING,
};
use crate::stderr_tail::Tail;
use crate::unlock::Event;
use std::fs::File;
use std::io::Read;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// How long a failed bridge waits for a process to finish exiting, so its
/// status and last words reach the log rather than the kill's.
const EXIT_GRACE: Duration = Duration::from_millis(250);
/// The wait for an exit before either process is seen to close its stderr:
/// an exiting process closes its descriptors one by one, its endpoint
/// first, so root may see that before its stderr's end.
const EXIT_FLOOR: Duration = Duration::from_millis(20);
const EXIT_STEP: Duration = Duration::from_millis(10);

/// A short frame in a clearing owner, its allocation fallible.
fn frame(bytes: &[u8]) -> Result<Bytes, String> {
    crate::allocation::copy(bytes)
        .map(Bytes)
        .map_err(|_| "notebook frame allocation failed".into())
}

struct Process {
    child: Child,
    wire: PendingWire,
    stderr: Tail,
    exited: Option<ExitStatus>,
}
impl Process {
    fn start(mut command: Command) -> Result<Self, String> {
        let (root, child) = UnixStream::pair().map_err(|_| "create notebook capability")?;
        let wire = PendingWire::new(root)?;
        let (stderr, child_stderr) = Tail::pair()?;
        let child = command
            .env_clear()
            .current_dir("/")
            .stdin(Stdio::from(OwnedFd::from(child)))
            .stdout(Stdio::null())
            .stderr(Stdio::from(child_stderr))
            .spawn()
            .map_err(|_| "start notebook process")?;
        Ok(Self {
            child,
            wire,
            stderr,
            exited: None,
        })
    }

    /// Whether it has exited, reaping it if so.
    fn reap(&mut self) -> Option<ExitStatus> {
        if self.exited.is_none() {
            self.exited = self.child.try_wait().ok().flatten();
        }
        self.exited
    }

    /// Its exit as the log gives it, with its last line of stderr quoted
    /// as its own words.
    fn describe(&mut self) -> String {
        if self.exited.is_some() {
            self.stderr.finish();
        } else {
            self.stderr.drain();
        }
        let state = match self.exited {
            None => "was still running".to_string(),
            Some(status) => match (status.code(), status.signal()) {
                (Some(code), _) => format!("exited {code}"),
                (None, Some(signal)) => format!("was killed by signal {signal}"),
                (None, None) => "exited".to_string(),
            },
        };
        match self.stderr.last_line() {
            Some(line) => format!("{state}, its last words \"{line}\""),
            None => format!("{state}, saying nothing"),
        }
    }
}

/// One line in root's log, which its unit keeps (td-authd/DESIGN.md,
/// "Notebook token descriptions").
pub(crate) fn log(line: &str) {
    use std::io::Write;
    let _ = writeln!(std::io::stderr().lock(), "td-authd: {line}");
}

/// What root says of a notebook bridge it ends.
pub(crate) struct Ending {
    /// The line for root's log; none when the person closed the window.
    pub line: Option<String>,
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

/// The vault service's token request root refuses, and why, as a code
/// it relays unread (td-secret/PORTABLE.md, "Root notebook token
/// exchange"): `3f` and the code.
const DECLINED: u8 = 0x3f;
/// The person declined the step in the window.
const CANCELLED: u8 = 0x10;
/// The operation outlived its ceiling, or its step its deadline.
const EXPIRED: u8 = 0x11;
/// What the window is told of an operation that waited out its ceiling
/// for the token slot: td-secret's words for an expired step.
const WAITED_OUT: &[u8] = b"the security key did not finish in time";
/// The window's PIN is outside a PIN's profile.
const PIN_INPUT: u8 = 0x15;
/// The secure attention screen took the security key.
const SUPERSEDED: u8 = 0x04;
/// A step that ended without the token's own reason.
const FAILED: u8 = 0x03;

/// What the window was last asked in the running step.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Asked {
    /// Step zero: the person connects the key and proceeds, or declines.
    Approval,
    /// A PIN step: the person types the key's PIN, or declines.
    Pin,
}

/// One admitted notebook hardware operation. While nothing has declined
/// it, it holds the token slot; root presents each of its steps in the
/// pinned window and relays to the vault service only a step's verified
/// result or a refusal's code. The service's final reply ends it.
struct Ceremony {
    token: Option<Token>,
    nonce: [u8; 32],
    action: u8,
    digest: [u8; 32],
    deadline: Instant,
    /// The key the running step names, when it names an enrolled one.
    key: Option<[u8; 4]>,
    /// The prompt the window shows: its number, what it asks and the
    /// step it presents, which an answer must name to count.
    asked: Option<(u64, Asked, Request)>,
    /// Once set, the operation releases nothing more: a step running is
    /// reaped and every further token request is refused with this code.
    declined: Option<u8>,
}

pub(crate) struct Bridge {
    owner: u32,
    gui: Process,
    service: Process,
    pending: Option<Bytes>,
    /// A hardware operation's ceiling, from when the window asked for it,
    /// so the wait for the slot counts against it.
    pending_deadline: Option<Instant>,
    waiting: Option<Waiting>,
    ceremony: Option<Ceremony>,
    /// The last prompt's number; each prompt the window is sent has its own.
    prompts: u64,
    /// Starts a step's root worker: `Token::start`, a fixed command.
    worker: fn(Request, &[u8]) -> Result<Token, String>,
    heartbeat: Instant,
    was_unlocked: bool,
    mail_revoked: bool,
    requests: Vec<MailBinding>,
    mail_reply: Option<(u64, Bytes)>,
    binding_update: Option<MailBinding>,
    dismissed: Option<MailBinding>,
}

fn action(bytes: &[u8]) -> Option<u8> {
    // Creation names its backup and each key's kind; adding and replacing
    // name the new key's kind.
    match bytes {
        [2, 0, _, _] => Some(1),
        [2, 1, _, _] => Some(2),
        [3, ..] => Some(3),
        [8, ..] => Some(4),
        [9, _] => Some(5),
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
            pending_deadline: None,
            waiting: None,
            ceremony: None,
            prompts: 0,
            worker: Token::start,
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
            || self.ceremony.is_some()
            || self.mail_reply.is_some()
        {
            return Ok(false);
        }
        let mut request = frame(&[12])?;
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

    /// Whether a notebook operation holds the token slot.
    pub fn busy(&self) -> bool {
        self.ceremony.as_ref().is_some_and(|c| c.declined.is_none())
    }

    /// Gives the token slot to the secure attention screen: the running
    /// step is stopped and reaped, and the operation fails, saying so.
    /// One waiting for the slot keeps waiting.
    pub fn yield_slot(&mut self) -> Result<(), String> {
        self.decline(SUPERSEDED)
    }

    /// Ends the operation's releases with `code`. A step running is
    /// reaped now and its token request refused; a later request is
    /// refused when it comes. One not yet begun is dropped, and the
    /// window told.
    fn decline(&mut self, code: u8) -> Result<(), String> {
        let Some(ceremony) = &mut self.ceremony else {
            if code != SUPERSEDED && self.pending.take_if(|p| action(&p.0).is_some()).is_some() {
                self.pending_deadline = None;
                let mut reply = frame(&[1])?;
                crate::notebook_wire::field(
                    &mut reply,
                    if code == EXPIRED {
                        WAITED_OUT
                    } else {
                        b"cancelled"
                    },
                )?;
                self.gui.wire.queue(reply)?;
            }
            return Ok(());
        };
        if ceremony.declined.is_some() {
            return Ok(());
        }
        ceremony.declined = Some(code);
        ceremony.asked = None;
        if let Some(token) = ceremony.token.take() {
            token.reap_for_teardown()?;
            self.service.wire.queue(frame(&[DECLINED, code])?)?;
        }
        Ok(())
    }

    /// The window's answer to a prompt (`7c`, the prompt's number, then
    /// proceed, a PIN or decline). An answer to a prompt no longer shown
    /// is dropped; a decline ends the operation when it names no prompt
    /// or the one shown.
    fn answer(&mut self, frame: &Bytes) -> Result<(), String> {
        let mut input = Reader::new(&frame.0);
        input.byte()?;
        let number = input.number()?;
        let pin = match input.byte()? {
            0 => None,
            1 => Some(input.field(63)?),
            2 => {
                input.finish()?;
                let shown = self
                    .ceremony
                    .as_ref()
                    .and_then(|c| c.asked.as_ref())
                    .map(|(asked, _, _)| *asked);
                // td-pass numbers a decline only for the prompt it shows
                // and sends zero for a cancel, so a numbered decline with
                // no prompt shown is stale.
                if number != 0 && shown != Some(number) {
                    return Ok(());
                }
                return self.decline(CANCELLED);
            }
            _ => return Err("invalid notebook answer".into()),
        };
        input.finish()?;
        let Some(ceremony) = &mut self.ceremony else {
            return Ok(());
        };
        let Some((asked_number, asked, request)) = ceremony.asked.take() else {
            return Ok(());
        };
        if asked_number != number {
            ceremony.asked = Some((asked_number, asked, request));
            return Ok(());
        }
        let token = ceremony
            .token
            .as_mut()
            .ok_or("notebook prompt without its step")?;
        // A refusal here cancels the step, whose poll then reports it.
        match (asked, pin) {
            (Asked::Approval, None) => {
                let _ = token.presented(&request);
            }
            (Asked::Pin, Some(pin)) => match Pin::entry(pin) {
                Ok(pin) if pin.is_pin() => {
                    let _ = token.pin(&request, pin);
                }
                _ => return self.decline(PIN_INPUT),
            },
            _ => return Err("notebook answer for another step".into()),
        }
        Ok(())
    }

    /// Starts the operation waiting for the token slot.
    fn begin(&mut self) -> Result<(), String> {
        let pending = self.pending.take().ok_or("no notebook operation waits")?;
        let deadline = self
            .pending_deadline
            .take()
            .ok_or("notebook operation has no ceiling")?;
        let action = action(&pending.0).ok_or("not a notebook token operation")?;
        let mut hash = crate::sha256::Sha256::new();
        hash.update(&pending.0);
        let digest = hash.finalize();
        let mut nonce = [0; 32];
        File::open("/dev/urandom")
            .and_then(|mut file| file.read_exact(&mut nonce))
            .map_err(|_| "read notebook operation randomness")?;
        self.service.wire.queue(pending)?;
        self.ceremony = Some(Ceremony {
            token: None,
            nonce,
            action,
            digest,
            deadline,
            key: None,
            asked: None,
            declined: None,
        });
        Ok(())
    }

    /// The vault service's reply within an operation: a token request,
    /// which starts the step's worker or is refused, or the final reply,
    /// which goes to the window and ends it.
    fn service_reply(&mut self, reply: Bytes) -> Result<(), String> {
        let ceremony = self.ceremony.as_mut().ok_or("missing notebook operation")?;
        match reply.0.first() {
            Some(0 | 1) => {
                if ceremony.token.is_some() {
                    return Err("notebook service answered during a token step".into());
                }
                if reply.0.first() == Some(&0) {
                    self.was_unlocked = true;
                }
                self.ceremony = None;
                self.gui.wire.queue(reply)?;
                self.heartbeat = Instant::now();
            }
            Some(0x30..=0x33) => {
                if ceremony.token.is_some() {
                    return Err("notebook service asked for two token steps".into());
                }
                if let Some(code) = ceremony.declined {
                    self.service.wire.queue(frame(&[DECLINED, code])?)?;
                    return Ok(());
                }
                let mut input = Reader::new(&reply.0);
                let method = input.byte()?;
                let purpose = input.byte()?;
                let role = input.byte()?;
                // An assertion or unseal names its key, a security key's
                // credential or a computer's sealed object.
                ceremony.key = if matches!(method, 0x31 | 0x33) {
                    let mut hash = crate::sha256::Sha256::new();
                    hash.update(input.field(1024)?);
                    let digest = hash.finalize();
                    let mut key = [0; 4];
                    key.copy_from_slice(digest.get(..4).ok_or("short credential digest")?);
                    Some(key)
                } else {
                    None
                };
                let mut hash = crate::sha256::Sha256::new();
                hash.update(&reply.0);
                let request = Request::new(
                    ceremony.nonce,
                    self.owner,
                    Operation::Notebook(Notebook {
                        method,
                        action: ceremony.action,
                        purpose,
                        role,
                        step: 0,
                        operation: ceremony.digest,
                        token: hash.finalize(),
                    }),
                )?;
                ceremony.token = Some((self.worker)(request, &reply.0)?);
            }
            _ => return Err("invalid notebook operation result".into()),
        }
        Ok(())
    }

    /// Advances the running step: each new presentation is prompted in
    /// the window, and a step's end relays its result or its refusal.
    fn advance(&mut self) -> Result<(), String> {
        let Some(ceremony) = &mut self.ceremony else {
            return Ok(());
        };
        if ceremony.declined.is_none() && Instant::now() >= ceremony.deadline {
            return self.decline(EXPIRED);
        }
        let Some(token) = &mut ceremony.token else {
            return Ok(());
        };
        match token.poll()? {
            Event::Present(request) => {
                if ceremony
                    .asked
                    .as_ref()
                    .is_some_and(|(_, _, asked)| asked == &request)
                {
                    return Ok(());
                }
                let Operation::Notebook(step) = request.operation() else {
                    return Err("notebook step names another operation".into());
                };
                // A computer key has no device to connect: its PIN, asked
                // next, is the person's approval.
                if step.computer() && step.step == 0 {
                    let _ = token.presented(&request);
                    return Ok(());
                }
                let asked = if request.asks_pin() {
                    Asked::Pin
                } else {
                    Asked::Approval
                };
                self.prompts = self.prompts.wrapping_add(1);
                let mut prompt = frame(&[0x7d])?;
                prompt.extend(&self.prompts.to_be_bytes())?;
                prompt.extend(&[
                    step.purpose,
                    step.role,
                    step.step,
                    u8::from(step.computer()),
                ])?;
                match ceremony.key {
                    Some(key) => {
                        prompt.push(1)?;
                        prompt.extend(&key)?;
                    }
                    None => prompt.push(0)?,
                }
                self.gui.wire.queue(prompt)?;
                // The window's PIN prompt presents the step.
                if asked == Asked::Pin {
                    let _ = token.presented(&request);
                }
                ceremony.asked = Some((self.prompts, asked, request));
            }
            Event::Complete => {
                let result = token
                    .take_result()
                    .ok_or("notebook token has no proved result")?;
                ceremony.token = None;
                ceremony.asked = None;
                self.service.wire.queue(result)?;
            }
            Event::Failed(_) | Event::Ended(_) => {
                let code = token
                    .refusal()
                    .or(token.expired().then_some(EXPIRED))
                    .unwrap_or(FAILED);
                ceremony.token = None;
                ceremony.asked = None;
                self.service.wire.queue(frame(&[DECLINED, code])?)?;
            }
            Event::Waiting | Event::Pin(_) => {}
            Event::Commit(_) => return Err("notebook step asked for a commit".into()),
        }
        Ok(())
    }

    /// What root logs of the bridge it is about to drop for `why`: each
    /// process's exit and last line of stderr. A process that closed its
    /// stderr is exiting, and root waits a short grace for its status;
    /// otherwise it waits for nothing, since the vault service never
    /// exits by itself while root holds its endpoint. The window's
    /// successful exit is the person closing it, which is neither logged
    /// nor a failure.
    pub fn ending(&mut self, why: &str) -> Ending {
        let start = Instant::now();
        loop {
            self.gui.stderr.drain();
            self.service.stderr.drain();
            let exited = self.gui.reap().is_some() | self.service.reap().is_some();
            let grace = if self.gui.stderr.eof() || self.service.stderr.eof() {
                EXIT_GRACE
            } else {
                EXIT_FLOOR
            };
            if exited || start.elapsed() >= grace {
                break;
            }
            std::thread::sleep(EXIT_STEP);
        }
        let failed = [self.gui.exited, self.service.exited]
            .iter()
            .any(|status| status.is_some_and(|status| !status.success()));
        if !failed && self.gui.exited.is_some_and(|status| status.success()) {
            return Ending { line: None };
        }
        Ending {
            line: Some(format!(
                "notebook ended: {why}; its window {}; its vault service {}",
                self.gui.describe(),
                self.service.describe()
            )),
        }
    }

    /// One turn: replies and requests move, a hardware operation starts
    /// when `slot_free` says the token slot is root's to give, and its
    /// step advances. True when an operation began and took the slot.
    pub fn poll(&mut self, slot_free: bool) -> Result<bool, String> {
        // Drained every poll so neither process fills its channel.
        self.gui.stderr.drain();
        self.service.stderr.drain();
        if self.ceremony.is_some() {
            if let Some(reply) = self.service.wire.poll()? {
                self.service_reply(reply)?;
            }
        } else if let Some(reply) = self.service.wire.poll()? {
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
            if request.0.first() == Some(&0x7c) {
                self.answer(&request)?;
            } else {
                self.window_request(request)?;
            }
        }
        let waits = self.ceremony.is_none()
            && self
                .pending
                .as_ref()
                .is_some_and(|p| action(&p.0).is_some());
        let mut began = false;
        if waits && self.pending_deadline.is_some_and(|d| Instant::now() >= d) {
            self.decline(EXPIRED)?;
        } else if waits && self.waiting.is_none() && slot_free {
            self.begin()?;
            began = true;
        }
        self.advance()?;
        if self.waiting.is_none() && self.ceremony.is_none() {
            if self
                .pending
                .as_ref()
                .is_some_and(|p| action(&p.0).is_none())
            {
                let pending = self.pending.take().ok_or("missing notebook request")?;
                self.send_gui(pending)?;
            } else if self.heartbeat.elapsed() >= Duration::from_secs(10) {
                self.service.wire.queue(frame(&[13])?)?;
                self.waiting = Some(Waiting::Heartbeat);
            }
        }
        Ok(began)
    }

    /// A browsing, binding or lifecycle request from the window, which
    /// may not overlap another.
    fn window_request(&mut self, request: Bytes) -> Result<(), String> {
        {
            if self.pending.is_some()
                || self.ceremony.is_some()
                || matches!(self.waiting, Some(Waiting::Gui))
            {
                return Err("concurrent notebook window requests".into());
            }
            let tag = request.0.first().copied().ok_or("empty notebook request")?;
            if tag == 15 {
                if request.0 != [15] {
                    return Err("invalid notebook request list".into());
                }
                let mut reply = frame(&[0, self.requests.len() as u8])?;
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
                    // It waits for the token slot, within its ceiling.
                    self.pending_deadline = Some(
                        Instant::now()
                            .checked_add(crate::consent::NOTEBOOK_CEILING)
                            .ok_or("notebook operation deadline overflow")?,
                    );
                    self.pending = Some(request);
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
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    #[test]
    #[ignore = "child process held until the root notebook owner revokes it"]
    fn idle_process() {
        let mut byte = [0];
        let _ = std::io::stdin().read_exact(&mut byte);
    }

    /// A bridge whose creation holds the token slot.
    pub(crate) fn ceremony() -> (Bridge, [u32; 2]) {
        let (mut bridge, pids) = bridge();
        bridge.pending = Some(Bytes(vec![2, 0, 0, 0]));
        bridge.pending_deadline = Some(Instant::now() + crate::consent::NOTEBOOK_CEILING);
        bridge.begin().unwrap();
        (bridge, pids)
    }

    /// A bridge whose unlock waits for the token slot.
    pub(crate) fn waiting() -> (Bridge, [u32; 2]) {
        let (mut bridge, pids) = bridge();
        bridge.pending = Some(Bytes(unlock_request()));
        bridge.pending_deadline = Some(Instant::now() + crate::consent::NOTEBOOK_CEILING);
        (bridge, pids)
    }

    #[test]
    #[ignore = "child process: a window that refuses to start"]
    fn refusing_process() {
        use std::io::Write;
        let _ = writeln!(
            std::io::stderr(),
            "td-pass: refused\x1b by its startup check"
        );
        std::process::exit(1);
    }

    #[test]
    #[ignore = "child process: a window the person closes"]
    fn closing_process() {
        std::process::exit(0);
    }

    fn process(name: &str) -> Process {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args(["--exact", name, "--ignored", "--nocapture", "-q"]);
        Process::start(command).unwrap()
    }

    pub(crate) fn bridge() -> (Bridge, [u32; 2]) {
        bridge_with(process("notebook::tests::idle_process"))
    }

    /// A bridge whose window refuses to start, as td-pass does when a
    /// startup check fails.
    pub(crate) fn refusing_bridge() -> (Bridge, [u32; 2]) {
        bridge_with(process("notebook::tests::refusing_process"))
    }

    fn bridge_with(gui: Process) -> (Bridge, [u32; 2]) {
        let service = process("notebook::tests::idle_process");
        let pids = [gui.child.id(), service.child.id()];
        (
            Bridge {
                owner: 1000,
                gui,
                service,
                pending: None,
                pending_deadline: None,
                waiting: None,
                ceremony: None,
                prompts: 0,
                worker: crate::notebook_token::tests::succeeding,
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
        bridge.poll(true).unwrap();
        let request = service.receive(Duration::from_secs(1)).unwrap();
        let mut input = Reader::new(&request.0);
        assert_eq!(input.byte().unwrap(), 12);
        assert_eq!(read_binding(&mut input).unwrap(), binding);
        input.finish().unwrap();
        let mut reply = Bytes(vec![0]);
        field(&mut reply, b"secret-token").unwrap();
        service.send(&reply.0).unwrap();
        bridge.poll(true).unwrap();
        let (id, response) = bridge.take_mail_reply().unwrap();
        assert_eq!(id, 42);
        assert_eq!(response.0, reply.0);
        assert!(gui.poll().unwrap().is_none());
        gui.queue(Bytes(vec![12])).unwrap();
        gui.poll().unwrap();
        assert!(bridge.poll(true).is_err());
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
        bridge.poll(true).unwrap();
        service.receive(Duration::from_secs(1)).unwrap();
        gui.send(&[11]).unwrap();
        bridge.poll(true).unwrap();
        assert!(!bridge.was_unlocked);
        let mut reply = Bytes(vec![0]);
        field(&mut reply, b"secret-token").unwrap();
        service.send(&reply.0).unwrap();
        bridge.poll(true).unwrap();
        assert!(bridge.take_mail_reply().is_none());
        bridge.poll(true).unwrap();
        assert_eq!(service.receive(Duration::from_secs(1)).unwrap().0, [11]);
    }
    /// A window that exits unsuccessfully on its own is a failure, its
    /// last line of stderr in root's log line; the vault service, still
    /// running, says so. A window that exits 0 was closed by the person.
    #[test]
    fn a_failed_window_reaches_the_log_and_a_closed_one_does_not() {
        let (mut bridge, _) = refusing_bridge();
        let deadline = Instant::now() + Duration::from_secs(10);
        let why = loop {
            if let Err(why) = bridge.poll(true) {
                break why;
            }
            assert!(Instant::now() < deadline, "the window's end went unseen");
            std::thread::sleep(Duration::from_millis(10));
        };
        let ending = bridge.ending(&why);
        let line = ending.line.unwrap();
        assert!(
            line.starts_with(&format!("notebook ended: {why}; ")),
            "{line}"
        );
        assert!(
            line.contains(
                "its window exited 1, its last words \"td-pass: refused? by its startup check\";"
            ),
            "{line}"
        );
        assert!(
            line.ends_with("its vault service was still running, saying nothing"),
            "{line}"
        );

        let (mut bridge, _) = bridge_with(process("notebook::tests::closing_process"));
        let deadline = Instant::now() + Duration::from_secs(10);
        let why = loop {
            if let Err(why) = bridge.poll(true) {
                break why;
            }
            assert!(Instant::now() < deadline, "the window's end went unseen");
            std::thread::sleep(Duration::from_millis(10));
        };
        let ending = bridge.ending(&why);
        assert!(ending.line.is_none());
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
        bridge.poll(true).unwrap();
        assert!(matches!(bridge.waiting, Some(Waiting::Heartbeat)));
    }

    /// The window's and the vault service's ends of a bridge, and the
    /// processes' own ends, held so neither sees its endpoint close.
    struct Peers {
        gui: PendingWire,
        service: PendingWire,
        _held: [PendingWire; 2],
    }

    fn wired(bridge: &mut Bridge) -> Peers {
        let (root, gui) = UnixStream::pair().unwrap();
        let gui_hold = std::mem::replace(&mut bridge.gui.wire, PendingWire::new(root).unwrap());
        let (root, service) = UnixStream::pair().unwrap();
        let service_hold =
            std::mem::replace(&mut bridge.service.wire, PendingWire::new(root).unwrap());
        Peers {
            gui: PendingWire::new(gui).unwrap(),
            service: PendingWire::new(service).unwrap(),
            _held: [gui_hold, service_hold],
        }
    }

    /// Turns the bridge until `peer` receives a frame, flushing what it
    /// has queued on the way.
    fn next(bridge: &mut Bridge, slot_free: bool, peer: &mut PendingWire) -> Vec<u8> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            bridge.poll(slot_free).unwrap();
            if let Some(frame) = peer.poll().unwrap() {
                return frame.0.clone();
            }
            assert!(Instant::now() < deadline, "nothing arrived");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn send(peer: &mut PendingWire, bytes: &[u8]) {
        peer.queue(Bytes(bytes.to_vec())).unwrap();
        assert!(peer.poll().unwrap().is_none());
    }

    const CREDENTIAL: &[u8] = b"enrolled-credential";

    fn unlock_request() -> Vec<u8> {
        let mut request = Bytes(vec![3]);
        crate::notebook_wire::field(&mut request, CREDENTIAL).unwrap();
        request.0.clone()
    }

    /// The service's `method` request for an unlock with the primary,
    /// naming the enrolled credential.
    fn unlocking(method: u8) -> Bytes {
        let mut request = Bytes(vec![method, 3, 1]);
        crate::notebook_wire::field(&mut request, CREDENTIAL).unwrap();
        request
    }

    /// The service's assertion request for an unlock with the primary.
    fn assertion() -> Vec<u8> {
        let mut request = unlocking(0x31);
        request.extend(&[7; 96]).unwrap();
        request.0.clone()
    }

    fn answer(number: &[u8], kind: u8, pin: Option<&[u8]>) -> Vec<u8> {
        let mut frame = Bytes(vec![0x7c]);
        frame.extend(number).unwrap();
        frame.push(kind).unwrap();
        if let Some(pin) = pin {
            crate::notebook_wire::field(&mut frame, pin).unwrap();
        }
        frame.0.clone()
    }

    /// The window's view of an unlock: root prompts for the key named by
    /// its fingerprint and approval, then its PIN, each prompt numbered;
    /// the step's result reaches only the service, whose reply reaches
    /// the window, and the slot is free again.
    #[test]
    fn an_unlock_is_prompted_in_the_window_and_its_result_goes_to_the_service() {
        let (mut bridge, _) = bridge();
        bridge.was_unlocked = false;
        let mut peers = wired(&mut bridge);
        send(&mut peers.gui, &unlock_request());
        // Another operation holds the slot: the unlock waits.
        for _ in 0..20 {
            bridge.poll(false).unwrap();
            assert!(peers.service.poll().unwrap().is_none());
        }
        assert!(!bridge.busy());
        assert!(bridge.poll(true).unwrap());
        assert!(bridge.busy());
        assert_eq!(
            next(&mut bridge, true, &mut peers.service),
            unlock_request()
        );
        send(&mut peers.service, &assertion());
        let prompt = next(&mut bridge, true, &mut peers.gui);
        let mut hash = crate::sha256::Sha256::new();
        hash.update(CREDENTIAL);
        let fingerprint = hash.finalize();
        assert_eq!(prompt[0], 0x7d);
        assert_eq!(&prompt[9..14], [3, 1, 0, 0, 1]);
        assert_eq!(&prompt[14..], &fingerprint[..4]);
        send(&mut peers.gui, &answer(&prompt[1..9], 0, None));
        let pin = next(&mut bridge, true, &mut peers.gui);
        assert_eq!(&pin[9..12], [3, 1, 1]);
        assert_ne!(pin[1..9], prompt[1..9]);
        // An answer to the earlier prompt is dropped.
        send(&mut peers.gui, &answer(&prompt[1..9], 1, Some(b"999999")));
        for _ in 0..20 {
            bridge.poll(true).unwrap();
            assert!(peers.service.poll().unwrap().is_none());
        }
        send(&mut peers.gui, &answer(&pin[1..9], 1, Some(b"123456")));
        assert_eq!(next(&mut bridge, true, &mut peers.service), [0x31; 37]);
        send(&mut peers.service, &[0]);
        assert_eq!(next(&mut bridge, true, &mut peers.gui), [0]);
        assert!(bridge.ceremony.is_none());
        assert!(!bridge.busy());
        assert!(bridge.was_unlocked);
    }

    /// A computer key's unseal asks for no approval: root acknowledges
    /// step zero itself, and the window's one prompt, for the PIN, names
    /// this computer and the sealed object's fingerprint.
    #[test]
    fn a_computer_unlock_prompts_once_for_its_pin() {
        let (mut bridge, _) = bridge();
        let mut peers = wired(&mut bridge);
        send(&mut peers.gui, &unlock_request());
        assert_eq!(
            next(&mut bridge, true, &mut peers.service),
            unlock_request()
        );
        send(&mut peers.service, &unlocking(0x33).0);
        let prompt = next(&mut bridge, true, &mut peers.gui);
        let mut hash = crate::sha256::Sha256::new();
        hash.update(CREDENTIAL);
        let fingerprint = hash.finalize();
        assert_eq!(prompt[0], 0x7d);
        assert_eq!(&prompt[9..14], [3, 1, 1, 1, 1]);
        assert_eq!(&prompt[14..], &fingerprint[..4]);
        send(&mut peers.gui, &answer(&prompt[1..9], 1, Some(b"123456")));
        assert_eq!(next(&mut bridge, true, &mut peers.service), [0x33; 37]);
        send(&mut peers.service, &[0]);
        assert_eq!(next(&mut bridge, true, &mut peers.gui), [0]);
        assert!(!bridge.busy());
    }

    #[test]
    fn creation_names_each_kind_and_additions_name_theirs() {
        for (request, expected) in [
            (&[2, 0, 1, 0][..], Some(1)),
            (&[2, 1, 1, 0], Some(2)),
            (&[2, 1, 0, 1], Some(2)),
            (&[2, 0], None),
            (&[2, 1], None),
            (&[9, 1], Some(5)),
            (&[9], None),
            (&[10, 1, 1], Some(6)),
        ] {
            assert_eq!(action(request), expected, "{request:?}");
        }
    }

    /// A wrong PIN fails the operation with the token's own code, and
    /// the window and service go on: the next request is served.
    #[test]
    fn a_refused_step_fails_its_operation_and_keeps_the_bridge() {
        let (mut bridge, _) = bridge();
        bridge.worker = crate::notebook_token::tests::refusing;
        let mut peers = wired(&mut bridge);
        send(&mut peers.gui, &unlock_request());
        next(&mut bridge, true, &mut peers.service);
        send(&mut peers.service, &assertion());
        let prompt = next(&mut bridge, true, &mut peers.gui);
        send(&mut peers.gui, &answer(&prompt[1..9], 0, None));
        let pin = next(&mut bridge, true, &mut peers.gui);
        send(&mut peers.gui, &answer(&pin[1..9], 1, Some(b"123456")));
        assert_eq!(
            next(&mut bridge, true, &mut peers.service),
            [DECLINED, 0x25]
        );
        let mut failure = Bytes(vec![1]);
        crate::notebook_wire::field(&mut failure, b"wrong PIN").unwrap();
        send(&mut peers.service, &failure.0);
        assert_eq!(next(&mut bridge, true, &mut peers.gui), failure.0);
        assert!(!bridge.busy());
        send(&mut peers.gui, &[1]);
        assert_eq!(next(&mut bridge, true, &mut peers.service), [1]);
    }

    /// Declining a prompt reaps the step and refuses it as cancelled; a
    /// PIN outside a PIN's profile is refused as such.
    #[test]
    fn a_decline_or_a_malformed_pin_refuses_the_running_step() {
        for (answered, code) in [(None, CANCELLED), (Some(&b"12"[..]), PIN_INPUT)] {
            let (mut bridge, _) = bridge();
            let mut peers = wired(&mut bridge);
            send(&mut peers.gui, &unlock_request());
            next(&mut bridge, true, &mut peers.service);
            send(&mut peers.service, &assertion());
            let prompt = next(&mut bridge, true, &mut peers.gui);
            match answered {
                None => send(&mut peers.gui, &answer(&[0; 8], 2, None)),
                Some(pin) => {
                    send(&mut peers.gui, &answer(&prompt[1..9], 0, None));
                    let asked = next(&mut bridge, true, &mut peers.gui);
                    send(&mut peers.gui, &answer(&asked[1..9], 1, Some(pin)));
                }
            }
            assert_eq!(
                next(&mut bridge, true, &mut peers.service),
                [DECLINED, code]
            );
            assert!(!bridge.busy());
            // A later request of the same operation is refused unasked.
            send(&mut peers.service, &assertion());
            assert_eq!(
                next(&mut bridge, true, &mut peers.service),
                [DECLINED, code]
            );
        }
    }

    /// The secure attention screen takes the slot: the step is reaped
    /// before the answer, and its operation fails as superseded.
    #[test]
    fn attention_takes_the_slot_from_a_running_step() {
        let (mut bridge, _) = bridge();
        let mut peers = wired(&mut bridge);
        send(&mut peers.gui, &unlock_request());
        next(&mut bridge, true, &mut peers.service);
        send(&mut peers.service, &assertion());
        next(&mut bridge, true, &mut peers.gui);
        bridge.yield_slot().unwrap();
        assert!(!bridge.busy());
        assert!(bridge.ceremony.as_ref().is_some_and(|c| c.token.is_none()));
        assert_eq!(
            next(&mut bridge, false, &mut peers.service),
            [DECLINED, SUPERSEDED]
        );
    }

    /// A step whose prompt goes unanswered past its deadline fails as
    /// expired; the service hears the code and both processes go on.
    #[test]
    fn an_unanswered_prompt_expires_its_step_and_keeps_the_bridge() {
        let (mut bridge, _) = bridge();
        bridge.worker = crate::notebook_token::tests::expiring;
        let mut peers = wired(&mut bridge);
        send(&mut peers.gui, &unlock_request());
        next(&mut bridge, true, &mut peers.service);
        send(&mut peers.service, &assertion());
        assert_eq!(next(&mut bridge, true, &mut peers.gui)[0], 0x7d);
        assert_eq!(
            next(&mut bridge, true, &mut peers.service),
            [DECLINED, EXPIRED]
        );
        let mut failure = Bytes(vec![1]);
        crate::notebook_wire::field(&mut failure, b"the security key did not finish in time")
            .unwrap();
        send(&mut peers.service, &failure.0);
        assert_eq!(next(&mut bridge, true, &mut peers.gui), failure.0);
        send(&mut peers.gui, &[1]);
        assert_eq!(next(&mut bridge, true, &mut peers.service), [1]);
    }

    /// A worker that ends without the token's reason fails its step as
    /// root's generic failure.
    #[test]
    fn a_worker_ending_without_a_reason_fails_its_step() {
        let (mut bridge, _) = bridge();
        bridge.worker = crate::notebook_token::tests::failing;
        let mut peers = wired(&mut bridge);
        send(&mut peers.gui, &unlock_request());
        next(&mut bridge, true, &mut peers.service);
        send(&mut peers.service, &assertion());
        assert_eq!(
            next(&mut bridge, true, &mut peers.service),
            [DECLINED, FAILED]
        );
        // The step failed; its operation holds the slot until the
        // service's final reply.
        assert!(bridge.ceremony.as_ref().is_some_and(|c| c.token.is_none()));
    }

    /// An answer naming the shown prompt but another kind of step ends
    /// the bridge; a decline naming another prompt is dropped.
    #[test]
    fn a_wrong_kind_of_answer_ends_the_bridge_and_a_stale_decline_is_dropped() {
        let (mut bridge, _) = bridge();
        let mut peers = wired(&mut bridge);
        send(&mut peers.gui, &unlock_request());
        next(&mut bridge, true, &mut peers.service);
        send(&mut peers.service, &assertion());
        let prompt = next(&mut bridge, true, &mut peers.gui);
        let mut stale = prompt[1..9].to_vec();
        stale[7] ^= 0x40;
        send(&mut peers.gui, &answer(&stale, 2, None));
        for _ in 0..20 {
            bridge.poll(true).unwrap();
            let got = peers.service.poll().unwrap();
            assert!(got.is_none(), "{:?}", got.map(|b| b.0.clone()));
        }
        assert!(bridge.busy());
        send(&mut peers.gui, &answer(&prompt[1..9], 1, Some(b"123456")));
        let deadline = Instant::now() + Duration::from_secs(10);
        while bridge.poll(true).is_ok() {
            assert!(Instant::now() < deadline, "the wrong answer went unseen");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// An operation that waits out its ceiling for the slot is dropped,
    /// the window told, and the service never sees it.
    #[test]
    fn an_operation_that_waits_out_its_ceiling_for_the_slot_is_dropped() {
        let (mut bridge, _) = bridge();
        let mut peers = wired(&mut bridge);
        send(&mut peers.gui, &unlock_request());
        bridge.poll(false).unwrap();
        assert!(bridge.pending.is_some());
        bridge.pending_deadline = Some(Instant::now());
        let mut expired = Bytes(vec![1]);
        crate::notebook_wire::field(&mut expired, WAITED_OUT).unwrap();
        assert_eq!(next(&mut bridge, false, &mut peers.gui), expired.0);
        assert!(bridge.pending.is_none());
        for _ in 0..20 {
            bridge.poll(true).unwrap();
            assert!(peers.service.poll().unwrap().is_none());
        }
    }

    /// An operation still waiting for the slot is dropped by a decline,
    /// and the window told; the service never saw it.
    #[test]
    fn a_decline_before_the_slot_frees_drops_the_waiting_operation() {
        let (mut bridge, _) = bridge();
        let mut peers = wired(&mut bridge);
        send(&mut peers.gui, &unlock_request());
        bridge.poll(false).unwrap();
        send(&mut peers.gui, &answer(&[0; 8], 2, None));
        let mut cancelled = Bytes(vec![1]);
        crate::notebook_wire::field(&mut cancelled, b"cancelled").unwrap();
        assert_eq!(next(&mut bridge, false, &mut peers.gui), cancelled.0);
        assert!(bridge.pending.is_none());
        for _ in 0..20 {
            bridge.poll(true).unwrap();
            assert!(peers.service.poll().unwrap().is_none());
        }
    }
}

//! The live installer's setup intake, the installation service it starts
//! for each installer, and the one physically confirmed whole-disk
//! installation (td-authd/DESIGN.md "Whole-disk installation intake").

use crate::consent::{Label, Operation, Request, Storage};
use crate::installation_consent::{self as wire, Answer, NoConsent, Outcome, Report};
use crate::secret_sys as sys;
use crate::unlock::Event;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const SOCKET: &str = "/run/td-authd/1000/setup";
/// The live wizard's service identity (td-install/INSTALLER.md), owner of
/// the socket and the only peer admitted; a review's consent still names
/// the session owner.
const INSTALLER_UID: u32 = 990;
const CMDLINE: &str = "/proc/cmdline";
/// The key the live selector handed to the live system (td-install/MEDIA.md).
const TRUSTED_KEY: &str = "/run/td-volume/td/trusted.pub";
/// Prompt lifetime, as for an update installation.
const CONSENT_TIME: Duration = Duration::from_secs(120);
/// A frame and its header.
const FRAME_BYTES: usize = 4 + wire::MAX_MESSAGE_BYTES;
/// Nonblocking reads, and writes, per heartbeat: a service cannot hold the
/// authority's loop.
const ATTEMPTS: usize = 4;

fn error(why: impl std::fmt::Display) -> String {
    why.to_string()
}

/// Whether `cmdline` marks a live boot: exactly one `td.live=` token, and
/// it is `td.live=1`. Any other spelling is ambiguous and refused.
pub(crate) fn live_marker(cmdline: &str) -> Result<bool, String> {
    let mut tokens = cmdline
        .split_ascii_whitespace()
        .filter(|token| token.starts_with("td.live="));
    match (tokens.next(), tokens.next()) {
        (None, _) => Ok(false),
        (Some("td.live=1"), None) => Ok(true),
        _ => Err("ambiguous live boot marker".into()),
    }
}

/// Whether this is a live boot whose handed-off trust root is in place.
pub(crate) fn live_boot() -> Result<bool, String> {
    let mut cmdline = String::new();
    File::open(CMDLINE)
        .and_then(|file| file.take(4097).read_to_string(&mut cmdline))
        .map_err(|e| format!("read {CMDLINE}: {e}"))?;
    if cmdline.len() > 4096 {
        return Err(format!("{CMDLINE} exceeds 4096 bytes"));
    }
    if !live_marker(&cmdline)? {
        return Ok(false);
    }
    let key = fs::symlink_metadata(TRUSTED_KEY).map_err(|e| format!("{TRUSTED_KEY}: {e}"))?;
    if !key.is_file() || key.uid() != 0 || key.mode() & 0o022 != 0 {
        return Err("live trust root is not a protected root file".into());
    }
    Ok(true)
}

/// How a review the operation shows ended, as the service reported it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Fate {
    /// Ended before any write.
    Ended,
    Started,
    Finished(Outcome),
}

/// The review a service has open, and how far it has come.
struct Open {
    review: Box<wire::Review>,
    selected: bool,
    /// The answer sent: consent or not.
    answered: Option<bool>,
    started: bool,
}

/// One running `td-install serve` and td-authd's end of its consent channel.
struct Service {
    child: Child,
    channel: UnixStream,
    /// The service closed the channel.
    closed: bool,
    sent_greeting: usize,
    greeting: Vec<u8>,
    inbox: Vec<u8>,
    outbox: Vec<u8>,
    open: Option<Open>,
}

impl Service {
    fn start(installer: UnixStream) -> Result<Self, String> {
        let (ours, theirs) = UnixStream::pair().map_err(error)?;
        let child = Command::new("/bin/td-install")
            .args([
                "serve",
                "/bin/td-boot",
                "/run/td-media",
                TRUSTED_KEY,
                "/",
                "/bin/td-firstboot",
            ])
            .env_clear()
            .current_dir("/")
            .stdin(Stdio::from(OwnedFd::from(installer)))
            .stdout(Stdio::from(OwnedFd::from(theirs)))
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("start installation service: {e}"))?;
        Self::over(child, ours)
    }

    fn over(child: Child, channel: UnixStream) -> Result<Self, String> {
        channel.set_nonblocking(true).map_err(error)?;
        Ok(Self {
            child,
            channel,
            closed: false,
            sent_greeting: 0,
            greeting: Vec::with_capacity(8),
            inbox: Vec::with_capacity(FRAME_BYTES),
            outbox: Vec::new(),
            open: None,
        })
    }

    /// Writes what the channel takes now: the greeting, then answers. A
    /// write that fails breaks the channel; bytes still owed mean the
    /// service has not started, so stopping it interrupts no write.
    fn flush(&mut self) -> Result<(), String> {
        for _ in 0..ATTEMPTS {
            let greeting = self.sent_greeting < wire::GREETING.len();
            let pending = match wire::GREETING.get(self.sent_greeting..) {
                Some(rest) if greeting => rest,
                _ => &self.outbox,
            };
            if pending.is_empty() {
                return Ok(());
            }
            match self.channel.write(pending) {
                Ok(0) => return Err("installation channel takes no more writes".into()),
                Ok(count) if greeting => self.sent_greeting += count,
                Ok(count) => {
                    self.outbox.drain(..count.min(self.outbox.len()));
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(error(e)),
            }
        }
        Ok(())
    }

    /// Moves bytes both ways, adding the fates of reviews to `fates`, and
    /// returns whether everything the service sent was read. An error means
    /// the channel broke; reports already sent are read first.
    fn exchange(&mut self, fates: &mut Vec<([u8; 32], Fate)>) -> Result<bool, String> {
        let written = self.flush();
        let drained = self.read(fates)?;
        written.map(|()| drained)
    }

    fn read(&mut self, fates: &mut Vec<([u8; 32], Fate)>) -> Result<bool, String> {
        let mut buffer = [0; FRAME_BYTES];
        for _ in 0..ATTEMPTS {
            if self.closed {
                return Ok(true);
            }
            match self.channel.read(&mut buffer) {
                // A service that closes with an answer unread resets.
                Ok(0) => self.closed = true,
                Err(e) if e.kind() == io::ErrorKind::ConnectionReset => self.closed = true,
                Ok(count) => {
                    let bytes = buffer
                        .get(..count)
                        .ok_or("installation channel read count")?;
                    self.receive(bytes, fates)?;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(true),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(error(e)),
            }
        }
        Ok(self.closed)
    }

    fn receive(
        &mut self,
        mut bytes: &[u8],
        fates: &mut Vec<([u8; 32], Fate)>,
    ) -> Result<(), String> {
        if self.greeting.len() < 8 {
            let wanted = bytes.len().min(8 - self.greeting.len());
            let (head, tail) = bytes.split_at_checked(wanted).ok_or("greeting split")?;
            self.greeting.extend_from_slice(head);
            bytes = tail;
            if let Ok(greeting) = <[u8; 8]>::try_from(self.greeting.as_slice()) {
                wire::check_greeting(&greeting)?;
            }
        }
        self.inbox.extend_from_slice(bytes);
        loop {
            let Some(header) = self.inbox.first_chunk::<4>() else {
                return Ok(());
            };
            let length = wire::payload_len(*header)?;
            let Some(payload) = self.inbox.get(4..4 + length) else {
                return Ok(());
            };
            let report = Report::decode(payload)?;
            self.inbox.drain(..4 + length);
            self.report(report, fates)?;
        }
    }

    /// Applies the channel's order (td-install/INSTALLER.md "Installation
    /// consent channel"): started only after consent, finished only after
    /// started, ended only before started. A report out of it breaks the
    /// channel.
    fn report(&mut self, report: Report, fates: &mut Vec<([u8; 32], Fate)>) -> Result<(), String> {
        let idle = self.open.is_none();
        let open = self
            .open
            .as_mut()
            .filter(|open| open.review.nonce() == report.nonce());
        let fate = match (report, open) {
            (Report::Review(review), None) if idle => {
                self.open = Some(Open {
                    review,
                    selected: false,
                    answered: None,
                    started: false,
                });
                return Ok(());
            }
            (Report::Ended(nonce, _), Some(open)) if !open.started => (nonce, Fate::Ended),
            (Report::Started(nonce), Some(open))
                if open.answered == Some(true) && !open.started =>
            {
                open.started = true;
                fates.push((nonce, Fate::Started));
                return Ok(());
            }
            (Report::Finished(nonce, outcome), Some(open)) if open.started => {
                (nonce, Fate::Finished(outcome))
            }
            _ => return Err("installation service report out of order".into()),
        };
        self.open = None;
        fates.push(fate);
        Ok(())
    }

    fn answer(&mut self, answer: Answer) -> Result<(), String> {
        if let Some(open) = self
            .open
            .as_mut()
            .filter(|open| open.review.nonce() == answer.nonce() && open.answered.is_none())
        {
            open.answered = Some(matches!(answer, Answer::Consent(_)));
        }
        self.outbox
            .extend_from_slice(&wire::frame(&answer.encode())?);
        // A failed write breaks the channel at the next tick.
        let _ = self.flush();
        Ok(())
    }

    /// Whether the child has exited, or cannot be waited for.
    fn exited(&mut self) -> bool {
        !matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for Service {
    /// Ordinary ticks drop only a reaped service; a live one is dropped
    /// only by teardown, which may block.
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The consent summary of a review, for `owner`.
fn summary(review: &wire::Review, owner: u32) -> Result<Request, String> {
    let deployment = review
        .deployment()
        .first_chunk::<8>()
        .copied()
        .ok_or("short deployment digest")?;
    Request::new(
        *review.nonce(),
        owner,
        Operation::InstallDisk {
            requester: owner,
            disk: review.disk().into(),
            capacity: review.capacity(),
            model: review.model().map(|raw| Label::model(raw.as_bytes())),
            serial: review.serial().map(|raw| Label::serial(raw.as_bytes())),
            hostname: review.hostname().into(),
            username: review.username().into(),
            deployment,
            storage: match review.storage() {
                wire::Storage::Unencrypted => Storage::Unencrypted,
                wire::Storage::DeviceBound => Storage::DeviceBound,
            },
        },
    )
}

/// The setup intake and the service it runs for the current installer.
pub(crate) struct Intake {
    listener: UnixListener,
    owner: u32,
    installer: u32,
    identity: (u64, u64),
    service: Option<Service>,
    /// A retired service, reaped without blocking before another starts.
    stopping: Option<Service>,
    /// This generation installs once.
    complete: bool,
}

impl Intake {
    /// Credential preparation has already admitted the root-owned parents.
    pub fn bind(owner: u32) -> Result<Self, String> {
        if owner != 1000 {
            return Err("unsupported setup requester".into());
        }
        let (listener, identity) = crate::deployment::bind_intake(SOCKET, INSTALLER_UID)?;
        Ok(Self {
            listener,
            owner,
            installer: INSTALLER_UID,
            identity,
            service: None,
            stopping: None,
            complete: false,
        })
    }

    /// Whether a new peer gets a service: none runs or awaits reaping, this
    /// generation has not installed, and the peer is the wizard's identity.
    fn admits(&self, peer: &UnixStream) -> bool {
        self.service.is_none()
            && self.stopping.is_none()
            && !self.complete
            && sys::peer_uid(peer).is_ok_and(|uid| uid == self.installer)
    }

    /// Starts a service for a new installer, and returns the fates of
    /// reviews. A service that closes or breaks the channel, or exits, is
    /// retired and its open review ends; one that broke it is killed.
    pub fn tick(&mut self) -> Vec<([u8; 32], Fate)> {
        if self.stopping.as_mut().is_some_and(Service::exited) {
            self.stopping = None;
        }
        if let Ok((installer, _)) = self.listener.accept() {
            // Busy, done or not the wizard: the peer sees its channel close.
            if self.admits(&installer) {
                match Service::start(installer) {
                    Ok(service) => self.service = Some(service),
                    Err(why) => {
                        let _ = writeln!(io::stderr(), "td-authd: {why}");
                    }
                }
            }
        }
        let Some(service) = &mut self.service else {
            return Vec::new();
        };
        let mut fates = Vec::new();
        // Sampled before draining: a child already dead writes no more.
        let exited = service.exited();
        // Reports read before the channel ended still count.
        let (broken, drained) = match service.exchange(&mut fates) {
            Ok(drained) => (false, drained),
            Err(why) => {
                let _ = writeln!(io::stderr(), "td-authd: installation service: {why}");
                (true, true)
            }
        };
        if fates
            .iter()
            .any(|(_, fate)| *fate == Fate::Finished(Outcome::Complete))
        {
            self.complete = true;
        }
        if broken || service.closed || (drained && exited) {
            // A review the service can no longer report on ends here; one
            // that started fails, its outcome unknown.
            if let Some(open) = service.open.take() {
                fates.push((*open.review.nonce(), Fate::Ended));
            }
            if broken {
                let _ = service.child.kill();
            }
            self.stopping = self.service.take();
            if self.stopping.as_mut().is_some_and(Service::exited) {
                self.stopping = None;
            }
        }
        fates
    }

    /// The open review, once, as a consent request. A review consent cannot
    /// show is declined as unavailable.
    pub fn select(&mut self) -> Result<Request, String> {
        self.tick();
        let service = self.service.as_mut().ok_or("no installation service")?;
        let open = service.open.as_mut().ok_or("no installation review")?;
        if open.selected {
            return Err("installation review already selected".into());
        }
        open.selected = true;
        match summary(&open.review, self.owner) {
            Ok(request) => Ok(request),
            Err(why) => {
                let nonce = *open.review.nonce();
                service.answer(Answer::Declined(nonce, NoConsent::Unavailable))?;
                Err(why)
            }
        }
    }

    pub fn answer(&mut self, answer: Answer) -> Result<(), String> {
        self.service
            .as_mut()
            .ok_or("no installation service")?
            .answer(answer)
    }
}

impl Drop for Intake {
    fn drop(&mut self) {
        crate::deployment::unlink_intake(SOCKET, self.identity);
    }
}

/// One whole-disk installation the person confirms on the trusted path.
pub(crate) struct Installation {
    request: Request,
    presented: bool,
    committed: bool,
    deadline: Instant,
    result: Option<bool>,
    answer: Option<Answer>,
}

impl Installation {
    pub fn start(request: Request) -> Result<Self, String> {
        if !matches!(request.operation(), Operation::InstallDisk { .. }) {
            return Err("not a whole-disk installation".into());
        }
        Ok(Self {
            request,
            presented: false,
            committed: false,
            deadline: Instant::now()
                .checked_add(CONSENT_TIME)
                .ok_or("installation deadline overflow")?,
            result: None,
            answer: None,
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
            return Err("stale installation consent".into());
        }
        Ok(())
    }
    pub fn presented(&mut self, request: &Request) -> Result<(), String> {
        // The review ended unconsented (the service ended it, or Escape or
        // expiry); poll reports it.
        if self.ended() && request == &self.request {
            return Ok(());
        }
        self.admit(request)?;
        if self.presented {
            return Err("duplicate installation presentation".into());
        }
        self.presented = true;
        Ok(())
    }
    pub fn commit(&mut self, request: &Request) -> Result<(), String> {
        self.admit(request)?;
        if !self.presented {
            return Err("installation was not presented".into());
        }
        self.committed = true;
        self.answer = Some(Answer::Consent(*self.request.nonce()));
        Ok(())
    }
    fn decline(&mut self, why: NoConsent) {
        if !self.committed && self.result.is_none() {
            self.result = Some(false);
            self.answer = Some(Answer::Declined(*self.request.nonce(), why));
        }
    }
    /// Consent is one answer, not a renewable approval: after commit only
    /// the service's reports end the operation.
    pub fn cancel(&mut self, _reason: &str) -> Result<(), String> {
        self.decline(NoConsent::Declined);
        Ok(())
    }
    /// The service's report on this review.
    pub fn fate(&mut self, nonce: &[u8; 32], fate: Fate) {
        if nonce != self.request.nonce() || self.result.is_some() {
            return;
        }
        self.result = match fate {
            Fate::Started => return,
            Fate::Ended => Some(false),
            // Only committed consent completes.
            Fate::Finished(outcome) => Some(self.committed && outcome == Outcome::Complete),
        };
    }
    /// Whether this review ended before commit.
    pub fn ended(&self) -> bool {
        self.result.is_some() && !self.committed
    }
    /// The answer owed to the service, once.
    pub fn take_answer(&mut self) -> Option<Answer> {
        self.answer.take()
    }
    pub fn poll(&mut self) -> Result<Event, String> {
        if !self.committed && Instant::now() >= self.deadline {
            self.decline(NoConsent::Expired);
        }
        Ok(match self.result {
            Some(true) => Event::Complete,
            Some(false) => Event::Failed("disk installation failed or was cancelled".into()),
            None if self.committed => Event::Waiting,
            None if self.presented => Event::Commit(self.request.clone()),
            None => Event::Present(self.request.clone()),
        })
    }
}

#[cfg(test)]
#[path = "../tests/disk_install.rs"]
pub(crate) mod tests;

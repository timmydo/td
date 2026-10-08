//! `set-hostname` (td-authd/DESIGN.md, "Elevation operations"): the public
//! intake at `/run/td-authd/1000/hostname` queues one requested name; request
//! `1e` describes it beside the saved name under a fresh nonce and approval
//! key; after the person's commit root saves the new name canonically.

use crate::backoff::{self, Backoff, Entry, Refused, Row, BACKING_OFF, REFUSED};
use crate::consent::{ApprovalKey, Operation, Request};
use crate::deployment::{bind_intake, unlink_intake};
use crate::elevation::{self, Table};
use crate::hostname::Hostname;
use crate::saved;
use crate::secret_sys as sys;
use crate::unlock::Event;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const SOCKET: &str = "/run/td-authd/1000/hostname";
const GREETING: &[u8; 8] = b"TDHST01\n";
/// The reply to a whole frame admitted; `backoff::Refused` gives a
/// refusal's.
const ADMITTED: u8 = 2;
/// The saved name, which `/etc/hostname` links to (td-install/INSTALLER.md).
const SAVED: &str = "/var/lib/td/hostname";
/// How long presentation and commit may take after selection.
const CONSENT: Duration = Duration::from_secs(120);

fn error(why: impl std::fmt::Display) -> io::Error {
    io::Error::other(why.to_string())
}
fn expires(seconds: u64) -> io::Result<Instant> {
    Instant::now()
        .checked_add(Duration::from_secs(seconds))
        .ok_or_else(|| error("hostname request deadline overflow"))
}

/// Request `1e`'s refusals before any description, each its `9e` byte.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Refusal {
    /// Nothing is queued, or the operation slot is busy.
    Nothing,
    /// The principal table refuses the owner or the requester, or cannot
    /// be read.
    Principal,
}

impl Refusal {
    pub fn answer(self) -> Vec<u8> {
        vec![
            0x9e,
            match self {
                Self::Nothing => 0,
                Self::Principal => 1,
            },
        ]
    }
}

/// The backoff as the intake last read it, and the last write that
/// failed, which refuses the intake until a later write succeeds.
struct Status {
    entry: Result<Entry, String>,
    unwritten: Option<String>,
}

impl Status {
    /// A write's result: its entry, or a refusal said on standard error
    /// that stays until a write succeeds.
    fn written(&mut self, result: Result<Entry, String>) -> Result<Entry, Refused> {
        match result {
            Ok(entry) => {
                self.entry = Ok(entry);
                self.unwritten = None;
                Ok(entry)
            }
            Err(why) => {
                diagnose(&why);
                self.unwritten = Some(why.clone());
                Err(Refused::Other(why))
            }
        }
    }
}

/// What the intake reads and writes: the saved name, its owner, the
/// backoff and the principal table.
pub(crate) struct Places {
    saved: PathBuf,
    owner: u32,
    backoff: Backoff,
    table: fn() -> Result<Table, String>,
}

impl Places {
    fn system() -> Self {
        Self {
            saved: PathBuf::from(SAVED),
            owner: 0,
            backoff: Backoff::system(Row::Hostname),
            table: Table::load,
        }
    }

    fn saved(&self) -> Result<Hostname, String> {
        saved::read_hostname(&self.saved, self.owner)?.ok_or_else(|| "no saved hostname".into())
    }
}

/// An admitted request: the name and the requester's UID.
pub(crate) struct Ready {
    name: Hostname,
    requester: u32,
}

impl Ready {
    pub fn requester(&self) -> u32 {
        self.requester
    }
}

struct Peer {
    pidfd: File,
    credentials: sys::Credentials,
    device: u64,
    inode: u64,
}

struct Pending {
    stream: UnixStream,
    owner: u32,
    greeting: usize,
    bytes: Vec<u8>,
    peer: Option<Peer>,
    name: Option<Hostname>,
    acknowledged: bool,
    selected: bool,
    deadline: Option<Instant>,
}

impl Pending {
    fn new(stream: UnixStream, owner: u32) -> io::Result<Self> {
        if sys::peer_uid(&stream)? != owner {
            return Err(error("hostname requester has the wrong UID"));
        }
        stream.set_nonblocking(true)?;
        sys::prepare_receiver(&stream)?;
        Ok(Self {
            stream,
            owner,
            greeting: 0,
            bytes: Vec::with_capacity(64),
            peer: None,
            name: None,
            acknowledged: false,
            selected: false,
            deadline: Some(expires(5)?),
        })
    }

    fn sender(&mut self, sender: sys::Sender) -> io::Result<()> {
        if sender.credentials.uid != self.owner || sender.descriptor.is_some() {
            return Err(error("hostname request has a foreign sender or descriptor"));
        }
        sys::alive(sender.pidfd.as_fd())?;
        let pidfd = File::from(sender.pidfd);
        let metadata = pidfd.metadata()?;
        if let Some(peer) = &self.peer {
            if sender.credentials != peer.credentials
                || metadata.dev() != peer.device
                || metadata.ino() != peer.inode
            {
                return Err(error("hostname sender changed"));
            }
            sys::alive(peer.pidfd.as_fd())?;
        } else {
            self.peer = Some(Peer {
                pidfd,
                credentials: sender.credentials,
                device: metadata.dev(),
                inode: metadata.ino(),
            });
        }
        Ok(())
    }

    fn live(&self) -> io::Result<()> {
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(error("hostname request expired"));
        }
        if let Some(peer) = &self.peer {
            sys::alive(peer.pidfd.as_fd())?;
        }
        Ok(())
    }

    /// One length byte, then 1 to 63 name bytes `Hostname::parse` admits;
    /// `admit` then decides: its success sends admission byte 02, and its
    /// refusal that refusal's byte before the connection closes.
    fn poll(&mut self, admit: impl FnOnce(&Hostname) -> Result<(), Refused>) -> io::Result<()> {
        let mut admit = Some(admit);
        self.live()?;
        for _ in 0..4 {
            if self.greeting < GREETING.len() {
                match self.stream.write(
                    GREETING
                        .get(self.greeting..)
                        .ok_or_else(|| error("hostname greeting cursor"))?,
                ) {
                    Ok(0) => return Err(error("hostname requester disconnected")),
                    Ok(count) => self.greeting += count,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                    Err(e) => return Err(e),
                }
                continue;
            }
            if self.name.is_some() {
                if !self.acknowledged {
                    match self.stream.write(&[ADMITTED]) {
                        Ok(1) => {
                            self.acknowledged = true;
                            self.deadline = Some(expires(60)?);
                        }
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                        _ => return Err(error("hostname admission reply failed")),
                    }
                    continue;
                }
                let mut byte = [0];
                return match sys::receive(&self.stream, &mut byte) {
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                        ) =>
                    {
                        Ok(())
                    }
                    _ => Err(error("hostname requester disconnected or sent extra bytes")),
                };
            }
            let expected = match self.bytes.first() {
                Some(&length) if (1..=63).contains(&length) => usize::from(length) + 1,
                Some(_) => return Err(error("invalid hostname request length")),
                None => 1,
            };
            if self.bytes.len() == expected && expected > 1 {
                let text = std::str::from_utf8(
                    self.bytes
                        .get(1..)
                        .ok_or_else(|| error("missing hostname"))?,
                )
                .map_err(error)?;
                let name = Hostname::parse(text).map_err(error)?;
                let admit = admit
                    .take()
                    .ok_or_else(|| error("hostname admitted twice"))?;
                if let Err(refused) = admit(&name) {
                    let _ = self.stream.write(&[refused.byte()]);
                    return Err(error(refused));
                }
                self.name = Some(name);
                return self.live();
            }
            let mut bytes = [0; 64];
            let remaining = expected
                .checked_sub(self.bytes.len())
                .ok_or_else(|| error("invalid hostname cursor"))?;
            let bytes = bytes
                .get_mut(..remaining)
                .ok_or_else(|| error("hostname frame overflow"))?;
            match sys::receive(&self.stream, bytes) {
                Ok((count, sender)) => {
                    self.sender(sender)?;
                    self.bytes.extend_from_slice(
                        bytes
                            .get(..count)
                            .ok_or_else(|| error("hostname receive count"))?,
                    );
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) => return Err(e),
            }
        }
        self.live()
    }
}

/// A requested name is admitted only outside the backoff, when it is not
/// the saved name and the table grants its requester `set-hostname`, and
/// only once the backoff counts it, so a request that later ends any way
/// but approval, a teardown or crash included, has counted.
fn admit(
    places: &Places,
    status: &mut Status,
    requester: u32,
    name: &Hostname,
) -> Result<(), Refused> {
    let now = backoff::now();
    status.entry = places.backoff.read();
    let entry = match &status.entry {
        Ok(entry) => *entry,
        Err(why) => {
            diagnose(why);
            return Err(Refused::Other(why.clone()));
        }
    };
    // Only a deadline cut to its bound is written back.
    let entry = if entry.clamped(now) == entry {
        entry
    } else {
        status.written(places.backoff.clamp(entry, now))?
    };
    if entry.refuses(now) {
        return Err(Refused::Backoff);
    }
    if &places.saved().map_err(Refused::Other)? == name {
        return Err(Refused::Other(
            "the requested name is the saved name".into(),
        ));
    }
    if !(places.table)()
        .map_err(Refused::Other)?
        .grants(requester, elevation::Operation::SetHostname)
    {
        return Err(Refused::Other(
            "the principal table does not grant the requester".into(),
        ));
    }
    status.written(places.backoff.admitted(entry, now))?;
    Ok(())
}

/// A backoff that cannot be read or written refuses the intake, said on
/// the authority's standard error.
fn diagnose(why: &str) {
    let _ = writeln!(io::stderr(), "td-authd: hostname intake refused: {why}");
}

pub(crate) struct Intake {
    listener: UnixListener,
    owner: u32,
    places: Places,
    pending: Option<Pending>,
    in_flight: bool,
    identity: (u64, u64),
    status: Status,
}

impl Intake {
    /// Credential preparation has already admitted these root-owned parents.
    pub fn bind(owner: u32) -> Result<Self, String> {
        if owner != 1000 {
            return Err("unsupported hostname requester".into());
        }
        let (listener, identity) = bind_intake(SOCKET, owner)?;
        let places = Places::system();
        let status = Status {
            entry: places.backoff.read(),
            unwritten: None,
        };
        Ok(Self {
            listener,
            owner,
            places,
            pending: None,
            in_flight: false,
            identity,
            status,
        })
    }

    pub fn tick(&mut self) {
        if let Ok((stream, _)) = self.listener.accept() {
            if self.pending.is_none() && !self.in_flight {
                self.pending = Pending::new(stream, self.owner).ok();
            }
        }
        let Self {
            pending,
            places,
            status,
            owner,
            ..
        } = self;
        let Some(request) = pending.as_mut() else {
            return;
        };
        // An admitted request counted at admission, however it ends.
        if request
            .poll(|name| admit(places, status, *owner, name))
            .is_err()
        {
            *pending = None;
        }
    }

    /// Root's `9f` answer: `01` while an admitted request waits, else `00`,
    /// or `02` while the backoff cannot be read or its last write failed;
    /// then the count of requests that ended unapproved, at most 255, which
    /// leaves out the one admitted and not yet ended. A backoff that could
    /// not be read is read again first.
    pub fn state(&mut self) -> Vec<u8> {
        if self.status.entry.is_err() {
            self.status.entry = self.places.backoff.read();
        }
        let waiting = self.pending.as_ref().is_some_and(|pending| {
            pending.acknowledged && !pending.selected && pending.live().is_ok()
        });
        let open = self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.name.is_some());
        match (&self.status.entry, &self.status.unwritten) {
            (Ok(entry), None) => vec![
                0x9f,
                u8::from(waiting),
                u8::try_from(entry.count().saturating_sub(u32::from(open))).unwrap_or(u8::MAX),
            ],
            _ => vec![0x9f, 2, 0],
        }
    }

    pub fn select(&mut self) -> Result<Ready, String> {
        self.tick();
        let pending = self.pending.as_mut().ok_or("no pending hostname change")?;
        if !pending.acknowledged || pending.selected {
            return Err("hostname change is not selectable".into());
        }
        pending.live().map_err(|e| e.to_string())?;
        let name = pending.name.as_ref().ok_or("incomplete hostname request")?;
        let requester = pending
            .peer
            .as_ref()
            .map(|peer| peer.credentials.uid)
            .ok_or("hostname request without a sender")?;
        let ready = Ready {
            name: Hostname::parse(name.name())?,
            requester,
        };
        pending.selected = true;
        pending.deadline = Some(expires(120).map_err(|e| e.to_string())?);
        self.in_flight = true;
        Ok(ready)
    }

    /// The description of `ready` beside the saved name; the backoff
    /// counted it at admission.
    pub fn describe(&self, owner: u32, ready: Ready) -> Result<Change, String> {
        let old = self.places.saved()?;
        Change::start(owner, ready, old, &self.places)
    }

    pub fn selected_alive(&self) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|pending| pending.selected && pending.live().is_ok())
    }

    pub fn committed(&mut self) {
        if let Some(pending) = &mut self.pending {
            pending.deadline = None;
        }
    }

    pub fn finish(&mut self, success: bool) {
        self.in_flight = false;
        if let Some(mut pending) = self.pending.take() {
            let _ = pending.stream.write(&[u8::from(success)]);
        }
        // An approval cleared the backoff; a failed write stays.
        self.status.entry = self.places.backoff.read();
    }
}

impl Drop for Intake {
    fn drop(&mut self) {
        unlink_intake(SOCKET, self.identity);
    }
}

pub(crate) fn request(name: &str) -> Result<(), String> {
    let name = Hostname::parse(name)?;
    let mut stream = UnixStream::connect(SOCKET)
        .map_err(|e| format!("connect to the hostname authority: {e}"))?;
    if sys::peer_uid(&stream).map_err(|e| e.to_string())? != 0 {
        return Err("hostname authority is not root".into());
    }
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    let mut greeting = [0; 8];
    stream
        .read_exact(&mut greeting)
        .map_err(|e| format!("hostname admission unavailable (busy or disconnected): {e}"))?;
    if &greeting != GREETING {
        return Err("unsupported hostname authority".into());
    }
    let mut bytes = Vec::with_capacity(64);
    bytes.push(u8::try_from(name.name().len()).map_err(|e| e.to_string())?);
    bytes.extend_from_slice(name.name().as_bytes());
    stream.write_all(&bytes).map_err(|e| e.to_string())?;
    let mut reply = [0];
    stream
        .read_exact(&mut reply)
        .map_err(|e| format!("hostname change was not admitted (disconnected): {e}"))?;
    match reply {
        [ADMITTED] => (),
        [BACKING_OFF] => {
            return Err(
                "hostname change was not admitted: the intake is backing off \
                 after unapproved requests"
                    .into(),
            )
        }
        [REFUSED] => {
            return Err(
                "hostname change was not admitted: the name is the current one, \
                 or the principal table or an unusable backoff refuses it"
                    .into(),
            )
        }
        _ => return Err("hostname change was not admitted".into()),
    }
    writeln!(
        io::stdout(),
        "A hostname change to {} waits. Press Ctrl+Alt+Escape, then H to review it.",
        name.name()
    )
    .map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(3600)))
        .map_err(|e| e.to_string())?;
    stream
        .read_exact(&mut reply)
        .map_err(|e| format!("hostname change ended without a completion receipt: {e}"))?;
    if reply == [1] {
        writeln!(
            io::stdout(),
            "Hostname saved as {}. A restart completes the change.",
            name.name()
        )
        .map_err(|e| e.to_string())
    } else {
        Err("hostname change was declined, expired or failed".into())
    }
}

/// Each digit `2` plus its own random byte modulo 8, as the rollback's.
fn approval_key(bytes: [u8; 2]) -> Result<ApprovalKey, String> {
    ApprovalKey::new(bytes.map(|byte| b'2' + byte % 8))
}

pub(crate) struct Change {
    request: Request,
    saved: PathBuf,
    owner: u32,
    backoff: Backoff,
    presented: bool,
    committed: bool,
    deadline: Instant,
    result: Option<bool>,
}

impl Change {
    /// Tag 12 for `ready` beside `old`, under a fresh nonce and approval
    /// key, each drawn from `/dev/urandom`.
    fn start(owner: u32, ready: Ready, old: Hostname, places: &Places) -> Result<Self, String> {
        let mut nonce = [0; 32];
        let mut key = [0; 2];
        File::open("/dev/urandom")
            .and_then(|mut random| {
                random.read_exact(&mut nonce)?;
                random.read_exact(&mut key)
            })
            .map_err(|e| format!("draw the hostname change's nonce and key: {e}"))?;
        Self::drawn(owner, ready, old, places, nonce, key)
    }

    fn drawn(
        owner: u32,
        ready: Ready,
        old: Hostname,
        places: &Places,
        nonce: [u8; 32],
        key: [u8; 2],
    ) -> Result<Self, String> {
        let request = Request::new(
            nonce,
            owner,
            Operation::SetHostname {
                key: approval_key(key)?,
                requester: ready.requester,
                old: old.name().into(),
                new: ready.name.name().into(),
            },
        )?;
        Ok(Self {
            request,
            saved: places.saved.clone(),
            owner: places.owner,
            backoff: places.backoff.clone(),
            presented: false,
            committed: false,
            deadline: Instant::now()
                .checked_add(CONSENT)
                .ok_or("hostname change deadline overflow")?,
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
            return Err("stale hostname consent".into());
        }
        Ok(())
    }

    pub fn presented(&mut self, request: &Request) -> Result<(), String> {
        self.admit(request)?;
        if self.presented {
            return Err("duplicate hostname presentation".into());
        }
        self.presented = true;
        Ok(())
    }

    /// The commit consumes the confirmation before anything else, so a
    /// retry is a new request; the approval then clears the backoff, and
    /// only a saved name still the old one is replaced.
    pub fn commit(&mut self, request: &Request) -> Result<(), String> {
        self.admit(request)?;
        if !self.presented {
            return Err("hostname change was not presented".into());
        }
        let Operation::SetHostname { old, new, .. } = self.request.operation() else {
            return Err("invalid hostname description".into());
        };
        self.committed = true;
        self.result = Some(save(&self.saved, self.owner, &self.backoff, old, new).is_ok());
        Ok(())
    }

    pub fn cancel(&mut self) -> Result<(), String> {
        if !self.committed {
            self.result = Some(false);
        }
        Ok(())
    }

    pub fn poll(&mut self) -> Result<Event, String> {
        if !self.committed && Instant::now() >= self.deadline {
            self.cancel()?;
        }
        Ok(match self.result {
            Some(true) => Event::Complete,
            Some(false) => Event::Failed("hostname change cancelled, refused or failed".into()),
            None if self.presented => Event::Commit(self.request.clone()),
            None => Event::Present(self.request.clone()),
        })
    }
}

fn save(path: &Path, owner: u32, backoff: &Backoff, old: &str, new: &str) -> Result<(), String> {
    backoff.approved()?;
    let saved = saved::read_hostname(path, owner)?.ok_or("no saved hostname")?;
    if saved.name() != old {
        return Err("the saved hostname changed since it was shown".into());
    }
    saved::write_hostname(path, &Hostname::parse(new)?)
}

#[cfg(test)]
#[path = "../tests/set_hostname.rs"]
pub(crate) mod tests;

//! td-agent's end of a workspace's network links (DESIGN.md §10): the
//! tool host's proxy sends up each connection's destination as a link
//! (`host::Link`), and here it is judged against the workspace's policy
//! and, when admitted, opened through the egress relay, td-net's
//! `td-egressd`, whose socket is under the runtime directory td-net's
//! launch gives td-agent; then its bytes are carried both ways, each
//! direction at most `LINK_WINDOW` ahead of the other end's `Took`.
//!
//! **Every frame from the tool host is jail-controlled.** A link's
//! destination is a request, judged here and again by the relay; ids
//! must rise; at most `MAX_CARRIERS` links hold a thread at once, each until
//! its thread ends; a link that sends past its window is shut; and no
//! write down waits on a tool host that has stopped reading (`Pipe`).

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use crate::config::{Destination, Network};
use crate::frame;
use crate::host::{Credit, Down, Link, LINK_CHUNK, LINK_WINDOW, MAX_LINKS, MAX_LINK_WHY};
use crate::rules::{Sourced, Verdict};

/// The relay's protocol line (net/src/egress.rs).
const RELAY_PROTOCOL: &str = "td-egress 1";
/// The longest answer the relay gives before a connection is its
/// destination's.
const MAX_ANSWER: usize = 2048;
/// How long the relay may take to answer: its lookup's and its
/// connections' deadlines, and room.
const ANSWER_TIME: Duration = Duration::from_secs(60);
/// How long one frame's write down a tool host's pipe may take, however
/// slowly the tool host reads, before it is taken as having stopped.
pub const PIPE_WRITE: Duration = Duration::from_secs(30);
/// The jailed pipe's write timeout: how long one write waits before the
/// frame's deadline is looked at again.
pub const PIPE_STEP: Duration = Duration::from_secs(1);
/// How many links' threads td-agent holds: the proxy's places, and room
/// for threads whose links have ended but which are still finishing, so
/// a link the proxy has a place for is not refused for them.
const MAX_CARRIERS: usize = MAX_LINKS + 8;

/// What a workspace's links are judged by and opened through: its
/// policy, its allowlist, the network rules that apply to it and the
/// rules files that could not be read (DESIGN.md §11), and the relay.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Egress {
    pub network: Network,
    pub allowlist: Vec<Destination>,
    pub rules: Vec<Sourced>,
    pub unread: Vec<(String, String)>,
    /// The relay's socket; none when td-agent was not launched with one.
    pub relay: Option<PathBuf>,
}

impl Default for Egress {
    fn default() -> Self {
        Self {
            network: Network::Off,
            allowlist: Vec::new(),
            rules: Vec::new(),
            unread: Vec::new(),
            relay: None,
        }
    }
}

/// A conversation's `Egress`, shared by every instance it launches with
/// a proxy and changed in place as its policy and rules change, so a
/// running background process's next connection is judged by the
/// current one.
pub type Judge = Arc<Mutex<Egress>>;

/// What the policy says of one destination.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Judgment {
    /// Opened without asking.
    Open,
    /// Refused, and why.
    Refuse(String),
    /// A crossing (§11): the conversation decides, why it is asked, and
    /// whether an "always allow" may be offered, which it may not when a
    /// rule asks or a rules file could not be read.
    Ask { why: String, allow: bool },
}

/// A link of an instance's that waits for the conversation, or that
/// waited and is gone.
/// Each names the call whose command made it, which outlives the
/// instance.
pub enum Asked {
    Waits {
        links: Weak<Links>,
        link: u64,
        call: u64,
        destination: Destination,
        why: String,
        allow: bool,
    },
    Gone {
        links: Weak<Links>,
        link: u64,
        call: u64,
    },
}

/// The most destinations one instance may ask the conversation about
/// over its life, so a command cannot keep the person's cards coming.
pub const MAX_ASKED: usize = 64;

/// Links taken out of waiting, each with its destination, to be given
/// their answer.
pub type Taken = Vec<(u64, Destination)>;

/// How an instance's links reach the conversation that decides them.
pub type Asker = Arc<dyn Fn(Asked) + Send + Sync>;

/// What an instance with a proxy is launched with: the conversation's
/// judge, whom to ask, and the call it runs, which approvals name.
#[derive(Clone)]
pub struct Linked {
    pub judge: Judge,
    pub asker: Option<Asker>,
    pub call: u64,
}

/// `host` and `port` as a destination, as an allowlist names it.
pub fn destination(host: &str, port: u16) -> Result<Destination, String> {
    let named = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    Destination::parse(&named)
}

/// The relay's socket under `runtime`, td-net launch's runtime directory.
pub fn relay_under(runtime: &Path) -> PathBuf {
    runtime.join("td-egress").join("socket")
}

/// The relay's socket as this process's environment names it: under
/// `XDG_RUNTIME_DIR`, when that is absolute and the socket is there.
pub fn relay_here() -> Option<PathBuf> {
    let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR")?);
    let relay = relay_under(&runtime);
    (runtime.is_absolute() && relay.exists()).then_some(relay)
}

impl Egress {
    /// What may become of a connection to `destination` (§10, §11): none
    /// under `off`; then a network deny refuses it, an ask or an unread
    /// rules file asks, and an allow opens it; else `open` opens any, and
    /// `allowlist` one on the allowlist and asks of the rest.
    pub fn judge(&self, destination: &Destination) -> Judgment {
        if self.network == Network::Off {
            return Judgment::Refuse("this workspace's network policy is off".into());
        }
        match crate::rules::judge_network(&self.rules, &self.unread, destination) {
            Verdict::Deny(why) => Judgment::Refuse(why),
            Verdict::Ask(why) => Judgment::Ask { why, allow: false },
            Verdict::Allow(_) => Judgment::Open,
            Verdict::Table => match self.network {
                Network::Open => Judgment::Open,
                Network::Allowlist if self.allowlist.contains(destination) => Judgment::Open,
                _ => Judgment::Ask {
                    why: format!(
                        "{} is not on this workspace's allowlist",
                        destination.text()
                    ),
                    allow: true,
                },
            },
        }
    }
}

/// The frames a conversation writes to its tool host, shared by its
/// calls and its links: one at a time, each within `deadline` however
/// slowly the tool host reads (where the writer's own timeout lets a
/// write return, `PIPE_STEP` for a jailed host); once one fails, every
/// later one fails at once. A call's or a cancel's frame goes before any
/// link's waiting, so it waits at most for the one frame being written.
pub struct Pipe {
    writer: Mutex<Box<dyn Write + Send>>,
    broken: AtomicBool,
    /// Calls' and cancels' frames waiting for their turn.
    urgent: AtomicUsize,
    deadline: Duration,
}

fn stopped() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "the tool host stopped reading")
}

impl Pipe {
    pub fn new(writer: impl Write + Send + 'static, deadline: Duration) -> Arc<Self> {
        Arc::new(Self {
            writer: Mutex::new(Box::new(writer)),
            broken: AtomicBool::new(false),
            urgent: AtomicUsize::new(0),
            deadline,
        })
    }

    /// Writes a call's or a cancel's frame, ahead of any link's, or says
    /// the tool host is not reading.
    pub fn frame(&self, bytes: &[u8]) -> io::Result<()> {
        let framed = frame::encode(bytes)?;
        self.urgent.fetch_add(1, Ordering::AcqRel);
        let written = self.lock().and_then(|writer| self.write(writer, &framed));
        self.urgent.fetch_sub(1, Ordering::AcqRel);
        written
    }

    /// Writes a link's frame once no call's or cancel's waits. One kept
    /// waiting past the deadline breaks the pipe, as a frame written too
    /// slowly does: a link's frame is never dropped and the link left
    /// waiting on it.
    pub fn link(&self, bytes: &[u8]) -> io::Result<()> {
        let framed = frame::encode(bytes)?;
        let began = Instant::now();
        loop {
            if self.broken.load(Ordering::Acquire) {
                return Err(stopped());
            }
            if self.urgent.load(Ordering::Acquire) == 0 {
                let writer = self.lock()?;
                if self.urgent.load(Ordering::Acquire) == 0 {
                    return self.write(writer, &framed);
                }
            }
            if began.elapsed() >= self.deadline {
                self.broken.store(true, Ordering::Release);
                return Err(stopped());
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn lock(&self) -> io::Result<MutexGuard<'_, Box<dyn Write + Send>>> {
        if self.broken.load(Ordering::Acquire) {
            return Err(stopped());
        }
        let writer = self.writer.lock().map_err(|_| stopped())?;
        if self.broken.load(Ordering::Acquire) {
            return Err(stopped());
        }
        Ok(writer)
    }

    /// Writes `framed` whole by the deadline, or breaks the pipe.
    fn write(
        &self,
        mut writer: MutexGuard<'_, Box<dyn Write + Send>>,
        framed: &[u8],
    ) -> io::Result<()> {
        let began = Instant::now();
        let mut rest = framed;
        let written = loop {
            if rest.is_empty() {
                break writer.flush();
            }
            if began.elapsed() >= self.deadline {
                break Err(stopped());
            }
            match writer.write(rest) {
                Ok(0) => break Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => rest = rest.get(n..).unwrap_or_default(),
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::Interrupted
                            | io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                    ) => {}
                Err(e) => break Err(e),
            }
        };
        if written.is_err() {
            self.broken.store(true, Ordering::Release);
        }
        written
    }
}

/// One tool host's links: what each is owed, and how its frames go down.
pub struct Links {
    /// None for a tool host with no proxy, whose links are refused.
    linked: Option<Linked>,
    pipe: Arc<Pipe>,
    open: Mutex<Open>,
}

/// The links open, the threads that carry them, and the last id taken.
#[derive(Default)]
struct Open {
    entries: BTreeMap<u64, Entry>,
    /// Carrying threads, each counted until it ends, however its link
    /// ended before it.
    live: usize,
    last: u64,
    /// Links waiting for the conversation, each its destination.
    waiting: BTreeMap<u64, Destination>,
    /// The person's answers for the rest of this instance, by
    /// destination: opened, or refused and why. Only a destination the
    /// allowlist alone asks about takes one, so a rule, or a rules file
    /// not read, still decides.
    answered: BTreeMap<Destination, Result<(), String>>,
    /// The destinations this instance has asked about, at most
    /// `MAX_ASKED`.
    asked: std::collections::BTreeSet<Destination>,
    /// Links taken out of waiting and not yet given their answer; one
    /// the tool host shuts meanwhile is given none.
    giving: std::collections::BTreeSet<u64>,
}

/// A link's place here: the one sender of the bytes for its relay
/// connection, so removing it ends the thread that writes them, and what
/// its threads share.
struct Entry {
    toward: Sender<Vec<u8>>,
    peer: Arc<Peer>,
}

/// How many of a link's bytes wait for its relay connection, and how far
/// ahead its relay's bytes may go down.
#[derive(Default)]
struct Peer {
    queued: AtomicU64,
    credit: Credit,
}

/// A carrying thread's count, given back when it ends.
struct Live(Arc<Links>);

impl Drop for Live {
    fn drop(&mut self) {
        if let Ok(mut open) = self.0.open.lock() {
            open.live = open.live.saturating_sub(1);
        }
    }
}

/// `why`, cut on a character boundary to what a refusal carries.
fn bounded(why: String) -> String {
    if why.len() <= MAX_LINK_WHY {
        return why;
    }
    let mut end = MAX_LINK_WHY;
    while !why.is_char_boundary(end) {
        end -= 1;
    }
    why.get(..end).unwrap_or_default().to_string()
}

impl Links {
    pub fn new(linked: Option<Linked>, pipe: Arc<Pipe>) -> Arc<Self> {
        Arc::new(Self {
            linked,
            pipe,
            open: Mutex::new(Open::default()),
        })
    }

    /// The call whose instance these links are, which approvals name.
    pub fn call(&self) -> Option<u64> {
        self.linked.as_ref().map(|linked| linked.call)
    }

    /// Whether link `link` still waits for the conversation.
    pub fn waits(&self, link: u64) -> bool {
        self.open
            .lock()
            .is_ok_and(|open| open.waiting.contains_key(&link))
    }

    /// The conversation's answer for `destination`, for every link of
    /// this instance's waiting on it and for the rest of the instance:
    /// opened, or refused and why.
    /// Link `link`, if it still waits, opened or refused, the answer kept
    /// for no other: a rule's, which the rules decide again next time.
    pub fn settle(self: &Arc<Self>, link: u64, answer: Result<(), String>) {
        let taken = self.take(link);
        self.give(taken, &answer);
    }

    /// Link `link` taken out of waiting, with its destination, if it
    /// still waited; nothing written, so the conversation may take it on
    /// its own thread and `give` it on another.
    pub fn take(&self, link: u64) -> Taken {
        self.open
            .lock()
            .ok()
            .and_then(|mut open| {
                let destination = open.waiting.remove(&link)?;
                open.giving.insert(link);
                Some(vec![(link, destination)])
            })
            .unwrap_or_default()
    }

    /// The person's `answer` for `destination` kept for the rest of this
    /// instance, and every link waiting on it taken out of waiting, with
    /// nothing written.
    pub fn take_answer(&self, destination: &Destination, answer: &Result<(), String>) -> Taken {
        self.open
            .lock()
            .map(|mut open| {
                open.answered.insert(destination.clone(), answer.clone());
                let taken: Vec<u64> = open
                    .waiting
                    .iter()
                    .filter(|(_, to)| *to == destination)
                    .map(|(link, _)| *link)
                    .collect();
                for link in &taken {
                    open.waiting.remove(link);
                    open.giving.insert(*link);
                }
                taken
                    .into_iter()
                    .map(|link| (link, destination.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Links taken, opened or refused: what goes down to the tool host.
    pub fn give(self: &Arc<Self>, taken: Taken, answer: &Result<(), String>) {
        for (link, destination) in taken {
            let still = self
                .open
                .lock()
                .is_ok_and(|mut open| open.giving.remove(&link));
            if !still {
                continue;
            }
            match answer {
                Ok(()) => self.start(link, &destination),
                Err(why) => self.refuse(link, why.clone()),
            }
        }
    }

    /// The call these links' instance runs, for what it asks.
    fn call_or_none(&self) -> u64 {
        self.call().unwrap_or(0)
    }

    pub fn answer(self: &Arc<Self>, destination: &Destination, answer: Result<(), String>) {
        let taken = self.take_answer(destination, &answer);
        self.give(taken, &answer);
    }

    /// Tells the conversation link `link` no longer waits, if it did.
    fn unwait(self: &Arc<Self>, link: u64) {
        let waited = self
            .open
            .lock()
            .is_ok_and(|mut open| open.waiting.remove(&link).is_some());
        if waited {
            if let Some(asker) = self.linked.as_ref().and_then(|l| l.asker.as_ref()) {
                asker(Asked::Gone {
                    links: Arc::downgrade(self),
                    link,
                    call: self.call_or_none(),
                });
            }
        }
    }

    fn send(&self, link: Link) -> bool {
        self.pipe.link(&Down::Link(link).encode()).is_ok()
    }

    fn refuse(&self, id: u64, why: String) {
        self.send(Link::Refused {
            link: id,
            why: bounded(why),
        });
    }

    fn entry(&self, id: u64) -> Option<(Sender<Vec<u8>>, Arc<Peer>)> {
        let open = self.open.lock().ok()?;
        let entry = open.entries.get(&id)?;
        Some((entry.toward.clone(), Arc::clone(&entry.peer)))
    }

    /// Link `id`'s place given up: its writing thread ends, its sender
    /// is let go, and whether it was there.
    fn forget(&self, id: u64) -> bool {
        let removed = self
            .open
            .lock()
            .ok()
            .and_then(|mut open| open.entries.remove(&id));
        match removed {
            Some(entry) => {
                entry.peer.credit.close();
                true
            }
            None => false,
        }
    }

    /// Link `id` ended here: given up, and the tool host told, once.
    fn shut(&self, id: u64) {
        if self.forget(id) {
            self.send(Link::Shut { link: id });
        }
    }

    /// A link frame from the tool host, on its reader's thread: nothing
    /// here waits on a relay.
    pub fn up(self: &Arc<Self>, link: Link) {
        match link {
            Link::Open { link, host, port } => self.open_link(link, host, port),
            Link::Bytes { link, data } => {
                let Some((toward, peer)) = self.entry(link) else {
                    return;
                };
                let n = u64::try_from(data.len()).unwrap_or(u64::MAX);
                let queued = peer.queued.fetch_add(n, Ordering::AcqRel).saturating_add(n);
                if queued > LINK_WINDOW || toward.send(data).is_err() {
                    self.shut(link);
                }
            }
            Link::Took { link, bytes } => {
                if let Some((_, peer)) = self.entry(link) {
                    peer.credit.give(bytes);
                }
            }
            Link::Shut { link } => {
                self.forget(link);
                self.unwait(link);
                if let Ok(mut open) = self.open.lock() {
                    open.giving.remove(&link);
                }
            }
            Link::Opened { .. } | Link::Refused { .. } => {}
        }
    }

    fn open_link(self: &Arc<Self>, id: u64, host: String, port: u16) {
        let fresh = self.open.lock().is_ok_and(|mut open| {
            let fresh = id > open.last;
            open.last = open.last.max(id);
            fresh
        });
        if !fresh {
            return self.refuse(id, "a link's id is one already used".into());
        }
        let destination = match destination(&host, port) {
            Ok(destination) => destination,
            Err(why) => return self.refuse(id, why),
        };
        // Judged here, so a refusal costs no thread, and asking waits on
        // no thread either: the link is put aside for the conversation.
        let Some(linked) = &self.linked else {
            return self.refuse(id, "this instance has no network".into());
        };
        let judged = linked.judge.lock().map_or_else(
            |_| Judgment::Refuse("the policy is not readable".into()),
            |e| e.judge(&destination),
        );
        let (why, allow) = match judged {
            Judgment::Open => return self.start(id, &destination),
            Judgment::Refuse(why) => return self.refuse(id, why),
            Judgment::Ask { why, allow } => (why, allow),
        };
        let Some(asker) = &linked.asker else {
            return self.refuse(id, why);
        };
        let waits = self
            .open
            .lock()
            .map(|mut open| {
                let answered = if allow {
                    open.answered.get(&destination).cloned()
                } else {
                    None
                };
                if let Some(answered) = answered {
                    return Err(answered);
                }
                if open.waiting.len() >= MAX_CARRIERS {
                    return Err(Err(format!(
                        "{MAX_CARRIERS} connections already wait for a decision"
                    )));
                }
                if !open.asked.contains(&destination) && open.asked.len() >= MAX_ASKED {
                    return Err(Err(format!(
                        "this command has asked about {MAX_ASKED} destinations; ask the person to admit the ones it needs"
                    )));
                }
                open.asked.insert(destination.clone());
                open.waiting.insert(id, destination.clone());
                Ok(())
            });
        match waits {
            Ok(Ok(())) => asker(Asked::Waits {
                links: Arc::downgrade(self),
                link: id,
                call: linked.call,
                destination,
                why,
                allow,
            }),
            Ok(Err(Ok(()))) => self.start(id, &destination),
            Ok(Err(Err(why))) => self.refuse(id, why),
            Err(_) => self.refuse(id, why),
        }
    }

    /// Opens link `id` to `destination` on a thread of its own, if there
    /// is room for one.
    fn start(self: &Arc<Self>, id: u64, destination: &Destination) {
        // An IPv6 host as the relay is named it by `connect`: bare.
        let host = destination
            .host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(&destination.host)
            .to_string();
        let port = destination.port;
        let (toward, from) = mpsc::channel();
        let peer = Arc::new(Peer::default());
        let taken = self.open.lock().is_ok_and(|mut open| {
            if open.live >= MAX_CARRIERS {
                return false;
            }
            open.live += 1;
            open.entries.insert(
                id,
                Entry {
                    toward,
                    peer: Arc::clone(&peer),
                },
            );
            true
        });
        if !taken {
            return self.refuse(
                id,
                format!("{MAX_CARRIERS} connections are already open; wait for one to end"),
            );
        }
        let live = Live(Arc::clone(self));
        let links = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("egress-link".into())
            .spawn(move || {
                let _live = live;
                links.carry(id, &host, port, &peer, &from);
            });
        if spawned.is_err() {
            self.forget(id);
            self.refuse(id, "no thread for the connection".into());
        }
    }

    /// Opens link `id` through the relay and writes the tool host's bytes
    /// to it until either end shuts.
    fn carry(
        self: Arc<Self>,
        id: u64,
        host: &str,
        port: u16,
        peer: &Arc<Peer>,
        from: &Receiver<Vec<u8>>,
    ) {
        let relay = match self.connect(host, port) {
            Ok(relay) => relay,
            Err(why) => {
                if self.forget(id) {
                    self.refuse(id, why);
                }
                return;
            }
        };
        let Ok(reading) = relay.try_clone() else {
            self.shut(id);
            return;
        };
        // Shut while it was being opened: nothing more to do.
        if self.entry(id).is_none() || !self.send(Link::Opened { link: id }) {
            let _ = relay.shutdown(Shutdown::Both);
            self.shut(id);
            return;
        }
        let links = Arc::clone(&self);
        let pumping = Arc::clone(peer);
        let pumped = std::thread::Builder::new()
            .name("egress-down".into())
            .spawn(move || links.pump(id, &pumping, reading));
        if pumped.is_err() {
            let _ = relay.shutdown(Shutdown::Both);
            self.shut(id);
            return;
        }
        let mut relay = relay;
        while let Ok(data) = from.recv() {
            if relay.write_all(&data).is_err() {
                break;
            }
            let n = u64::try_from(data.len()).unwrap_or(u64::MAX);
            peer.queued.fetch_sub(n, Ordering::AcqRel);
            if !self.send(Link::Took { link: id, bytes: n }) {
                break;
            }
        }
        let _ = relay.shutdown(Shutdown::Both);
        self.shut(id);
    }

    /// The relay's bytes, down, as far ahead as the tool host allows.
    fn pump(&self, id: u64, peer: &Peer, mut reading: UnixStream) {
        let mut buffer = vec![0u8; LINK_CHUNK];
        loop {
            let read = match reading.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => read,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            };
            let n = u64::try_from(read).unwrap_or(u64::MAX);
            let data = buffer.get(..read).unwrap_or_default().to_vec();
            if !peer.credit.take(n) || !self.send(Link::Bytes { link: id, data }) {
                break;
            }
        }
        let _ = reading.shutdown(Shutdown::Both);
        self.shut(id);
    }

    /// A connection through the relay to `host:port`, or why there is
    /// none: the relay's own refusal, as it gives it.
    fn connect(&self, host: &str, port: u16) -> Result<UnixStream, String> {
        let socket = self
            .linked
            .as_ref()
            .and_then(|linked| linked.judge.lock().ok()?.relay.clone())
            .ok_or(
                "there is no egress relay: td-agent was not launched by td-net, which serves one",
            )?;
        let fault = |e: io::Error| format!("the egress relay: {e}");
        let mut relay = UnixStream::connect(socket).map_err(fault)?;
        relay.set_read_timeout(Some(ANSWER_TIME)).map_err(fault)?;
        relay.set_write_timeout(Some(ANSWER_TIME)).map_err(fault)?;
        let named = if host.contains(':') {
            format!("[{host}]")
        } else {
            host.to_string()
        };
        relay
            .write_all(format!("{RELAY_PROTOCOL}\nconnect {named} {port}\n\n").as_bytes())
            .map_err(fault)?;
        let answer = read_answer(&mut relay).map_err(fault)?;
        let mut lines = answer.lines();
        if lines.next() != Some(RELAY_PROTOCOL) {
            return Err("the egress relay answered in another protocol".into());
        }
        match lines.next() {
            Some("ok") => {
                relay.set_read_timeout(None).map_err(fault)?;
                relay.set_write_timeout(None).map_err(fault)?;
                Ok(relay)
            }
            Some(error) => Err(format!(
                "the egress relay: {}",
                error.strip_prefix("error ").unwrap_or(error)
            )),
            None => Err("the egress relay gave no answer".into()),
        }
    }

    /// Every link ended: the tool host is gone.
    pub fn close_all(self: &Arc<Self>) {
        let (entries, waiting) = self
            .open
            .lock()
            .map(|mut open| {
                (
                    std::mem::take(&mut open.entries),
                    std::mem::take(&mut open.waiting),
                )
            })
            .unwrap_or_default();
        for entry in entries.values() {
            entry.peer.credit.close();
        }
        if let Some(asker) = self.linked.as_ref().and_then(|l| l.asker.as_ref()) {
            for link in waiting.into_keys() {
                asker(Asked::Gone {
                    links: Arc::downgrade(self),
                    link,
                    call: self.call_or_none(),
                });
            }
        }
    }
}

/// The relay's answer, read a byte at a time up to its blank line so
/// nothing of the destination's is taken.
fn read_answer(relay: &mut UnixStream) -> io::Result<String> {
    let mut answer = Vec::new();
    let mut byte = [0u8; 1];
    while !answer.ends_with(b"\n\n") {
        if answer.len() >= MAX_ANSWER {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "an answer too long",
            ));
        }
        match relay.read(&mut byte) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(_) => answer.extend_from_slice(&byte),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    String::from_utf8(answer).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "not text"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
    use super::*;
    use std::os::unix::net::UnixListener;

    fn egress(network: Network, allowlist: &[&str], relay: Option<PathBuf>) -> Egress {
        Egress {
            network,
            allowlist: allowlist
                .iter()
                .map(|d| Destination::parse(d).unwrap())
                .collect(),
            rules: Vec::new(),
            unread: Vec::new(),
            relay,
        }
    }

    fn judged(egress: &Egress, host: &str, port: u16) -> Judgment {
        match destination(host, port) {
            Ok(destination) => egress.judge(&destination),
            Err(why) => Judgment::Refuse(why),
        }
    }

    fn sourced(lines: &[&str]) -> Vec<Sourced> {
        lines
            .iter()
            .map(|line| Sourced {
                rule: crate::rules::Rule::parse(line).unwrap(),
                from: "here".into(),
            })
            .collect()
    }

    /// `open` admits any host, `allowlist` its own by host and port, 443
    /// unless named, however the host is written; `off` none; and what
    /// is not a host is refused under any.
    #[test]
    fn links_are_judged_by_the_policy() {
        let allow = egress(
            Network::Allowlist,
            &[
                "static.crates.io",
                "git.example.org:8443",
                "[2001:db8::1]:22",
            ],
            None,
        );
        assert_eq!(judged(&allow, "static.crates.io", 443), Judgment::Open);
        assert_eq!(judged(&allow, "Static.Crates.IO.", 443), Judgment::Open);
        assert_eq!(judged(&allow, "git.example.org", 8443), Judgment::Open);
        assert_eq!(judged(&allow, "2001:db8:0::1", 22), Judgment::Open);
        assert_eq!(
            judged(&allow, "static.crates.io", 80),
            Judgment::Ask {
                why: "static.crates.io:80 is not on this workspace's allowlist".into(),
                allow: true
            }
        );
        assert!(matches!(
            judged(&allow, "crates.io", 443),
            Judgment::Ask { .. }
        ));
        let open = egress(Network::Open, &[], None);
        assert_eq!(judged(&open, "anything.example", 22), Judgment::Open);
        assert!(matches!(judged(&open, "a_b", 443), Judgment::Refuse(_)));
        assert!(matches!(judged(&open, "10.1", 443), Judgment::Refuse(_)));
        let off = egress(Network::Off, &["static.crates.io"], None);
        assert!(matches!(
            judged(&off, "static.crates.io", 443),
            Judgment::Refuse(_)
        ));
    }

    /// The rules come before the policy: a deny refuses even what the
    /// allowlist or `open` admits, an ask or an unread file asks with no
    /// allow to offer, an allow opens what the allowlist would ask of,
    /// and nothing opens under `off`.
    #[test]
    fn links_are_judged_by_the_rules_first() {
        let mut allow = egress(Network::Allowlist, &["static.crates.io"], None);
        allow.rules = sourced(&[
            "deny network static.crates.io",
            "ask network ask.example",
            "allow network extra.example:8443",
        ]);
        assert!(matches!(
            judged(&allow, "static.crates.io", 443),
            Judgment::Refuse(why) if why.contains("deny network static.crates.io")
        ));
        assert!(matches!(
            judged(&allow, "ask.example", 443),
            Judgment::Ask { allow: false, .. }
        ));
        assert_eq!(judged(&allow, "extra.example", 8443), Judgment::Open);
        let mut open = egress(Network::Open, &[], None);
        open.rules = sourced(&["deny network"]);
        assert!(matches!(
            judged(&open, "any.example", 443),
            Judgment::Refuse(_)
        ));
        open.rules = Vec::new();
        open.unread = vec![("theirs".into(), "broken".into())];
        assert!(matches!(
            judged(&open, "any.example", 443),
            Judgment::Ask { allow: false, .. }
        ));
        let mut off = egress(Network::Off, &[], None);
        off.rules = sourced(&["allow network extra.example"]);
        assert!(matches!(
            judged(&off, "extra.example", 443),
            Judgment::Refuse(_)
        ));
    }

    /// A stand-in relay: each request's head sent on `heads`, then
    /// `refused.test` refused as the relay would, and any other host
    /// echoed back with `>` before each read until the client ends.
    fn relay(tag: &str) -> (PathBuf, mpsc::Receiver<String>) {
        let dir =
            std::env::temp_dir().join(format!("td-agent-egress-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("socket");
        let listener = UnixListener::bind(&socket).unwrap();
        let (tell, heads) = mpsc::channel();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let tell = tell.clone();
                std::thread::spawn(move || {
                    let head = read_answer(&mut stream).unwrap();
                    let _ = tell.send(head.clone());
                    if head.contains("refused.test") {
                        let _ = stream.write_all(
                            b"td-egress 1\nerror refused: 10.0.0.1 is private (RFC 1918)\n\n",
                        );
                        return;
                    }
                    if head.contains("long.test") {
                        let why = "x".repeat(1500);
                        let _ = stream.write_all(
                            format!("td-egress 1\nerror transport: {why}\n\n").as_bytes(),
                        );
                        return;
                    }
                    if head.contains("hung.test") {
                        std::thread::sleep(Duration::from_secs(90));
                        return;
                    }
                    if head.contains("silent.test") {
                        let _ = stream.write_all(b"td-egress 1\nok\n\n");
                        std::thread::sleep(Duration::from_secs(20));
                        return;
                    }
                    stream.write_all(b"td-egress 1\nok\n\n").unwrap();
                    let mut buffer = [0u8; 4096];
                    loop {
                        let read = match stream.read(&mut buffer) {
                            Ok(0) | Err(_) => return,
                            Ok(read) => read,
                        };
                        let mut back = b">".to_vec();
                        back.extend_from_slice(&buffer[..read]);
                        if stream.write_all(&back).is_err() {
                            return;
                        }
                    }
                });
            }
        });
        (socket, heads)
    }

    /// Links over `egress`, and the tool host's end of their frames.
    fn links(egress: Option<Egress>) -> (Arc<Links>, UnixStream) {
        asking(egress, None)
    }

    fn asking(egress: Option<Egress>, asker: Option<Asker>) -> (Arc<Links>, UnixStream) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        theirs
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let linked = egress.map(|egress| Linked {
            judge: Arc::new(Mutex::new(egress)),
            asker,
            call: 7,
        });
        (Links::new(linked, Pipe::new(ours, PIPE_WRITE)), theirs)
    }

    /// An asker that hands what it is asked to the test.
    fn asker() -> (Asker, mpsc::Receiver<Asked>) {
        let (tell, asked) = mpsc::channel();
        let tell = Mutex::new(tell);
        let asker: Asker = Arc::new(move |one| {
            let _ = tell.lock().unwrap().send(one);
        });
        (asker, asked)
    }

    fn down(theirs: &mut UnixStream) -> Link {
        let bytes = frame::read(theirs).unwrap().unwrap();
        match Down::decode(&bytes).unwrap() {
            Down::Link(link) => link,
            other => panic!("{other:?}"),
        }
    }

    /// An admitted link is opened through the relay, named as asked; its
    /// bytes go to the relay and are taken back, the relay's come down,
    /// and the tool host's end shuts the relay's connection.
    #[test]
    fn an_admitted_link_is_carried_through_the_relay() {
        let (socket, heads) = relay("carried");
        let (links, mut theirs) = links(Some(egress(
            Network::Allowlist,
            &["example.test", "[2001:db8::1]"],
            Some(socket),
        )));
        links.up(Link::Open {
            link: 1,
            host: "example.test".into(),
            port: 443,
        });
        assert_eq!(down(&mut theirs), Link::Opened { link: 1 });
        assert_eq!(
            heads.recv_timeout(Duration::from_secs(5)).unwrap(),
            "td-egress 1\nconnect example.test 443\n\n"
        );
        links.up(Link::Bytes {
            link: 1,
            data: b"hi".to_vec(),
        });
        let mut seen = Vec::new();
        for _ in 0..2 {
            seen.push(down(&mut theirs));
        }
        assert!(seen.contains(&Link::Took { link: 1, bytes: 2 }), "{seen:?}");
        assert!(
            seen.contains(&Link::Bytes {
                link: 1,
                data: b">hi".to_vec()
            }),
            "{seen:?}"
        );
        links.up(Link::Took { link: 1, bytes: 3 });
        links.up(Link::Shut { link: 1 });
        // An IPv6 host goes to the relay in brackets.
        links.up(Link::Open {
            link: 2,
            host: "2001:db8::1".into(),
            port: 443,
        });
        assert_eq!(down(&mut theirs), Link::Opened { link: 2 });
        assert_eq!(
            heads.recv_timeout(Duration::from_secs(5)).unwrap(),
            "td-egress 1\nconnect [2001:db8::1] 443\n\n"
        );
    }

    /// A link the policy refuses never reaches the relay; one the relay
    /// refuses says why; without a relay or an egress, every link is
    /// refused, saying why.
    #[test]
    fn a_refused_link_says_why() {
        let (socket, heads) = relay("refused");
        let (links, mut theirs) = links(Some(egress(
            Network::Allowlist,
            &["refused.test"],
            Some(socket),
        )));
        links.up(Link::Open {
            link: 1,
            host: "other.test".into(),
            port: 443,
        });
        let Link::Refused { link: 1, why } = down(&mut theirs) else {
            panic!("not refused");
        };
        assert!(why.contains("not on this workspace's allowlist"), "{why}");
        assert!(heads.recv_timeout(Duration::from_millis(200)).is_err());
        links.up(Link::Open {
            link: 2,
            host: "refused.test".into(),
            port: 443,
        });
        let Link::Refused { link: 2, why } = down(&mut theirs) else {
            panic!("not refused");
        };
        assert_eq!(
            why,
            "the egress relay: refused: 10.0.0.1 is private (RFC 1918)"
        );
        let (links, mut theirs) = self::links(Some(egress(Network::Open, &[], None)));
        links.up(Link::Open {
            link: 3,
            host: "example.test".into(),
            port: 443,
        });
        let Link::Refused { why, .. } = down(&mut theirs) else {
            panic!("not refused");
        };
        assert!(why.contains("not launched by td-net"), "{why}");
        let (links, mut theirs) = self::links(None);
        links.up(Link::Open {
            link: 4,
            host: "example.test".into(),
            port: 443,
        });
        let Link::Refused { why, .. } = down(&mut theirs) else {
            panic!("not refused");
        };
        assert!(why.contains("no network"), "{why}");
    }

    /// A tool host that sends past its window, while the relay takes
    /// nothing, has its link shut.
    #[test]
    fn a_link_past_its_window_is_shut() {
        let (socket, _heads) = relay("window");
        let (links, mut theirs) = links(Some(egress(Network::Open, &[], Some(socket))));
        links.up(Link::Open {
            link: 1,
            host: "silent.test".into(),
            port: 443,
        });
        assert_eq!(down(&mut theirs), Link::Opened { link: 1 });
        let chunk = vec![0u8; LINK_CHUNK];
        let chunks = LINK_WINDOW / LINK_CHUNK as u64 + 64;
        for _ in 0..chunks {
            links.up(Link::Bytes {
                link: 1,
                data: chunk.clone(),
            });
        }
        loop {
            match down(&mut theirs) {
                Link::Shut { link: 1 } => break,
                Link::Took { .. } => {}
                other => panic!("{other:?}"),
            }
        }
    }

    /// Ids must rise: one used, or below one used, is refused, whatever
    /// became of its link.
    #[test]
    fn a_links_id_is_never_used_twice() {
        let (socket, _heads) = relay("ids");
        let (links, mut theirs) = links(Some(egress(Network::Open, &[], Some(socket))));
        links.up(Link::Open {
            link: 5,
            host: "example.test".into(),
            port: 443,
        });
        assert_eq!(down(&mut theirs), Link::Opened { link: 5 });
        for again in [5, 3] {
            links.up(Link::Open {
                link: again,
                host: "example.test".into(),
                port: 443,
            });
            let Link::Refused { link, why } = down(&mut theirs) else {
                panic!("not refused");
            };
            assert_eq!(link, again);
            assert!(why.contains("already used"), "{why}");
        }
    }

    /// A link shut while it is being opened keeps its thread's place
    /// until the thread ends, so opening and shutting links at any pace
    /// holds no more than `MAX_CARRIERS` threads.
    #[test]
    fn links_shut_while_opening_keep_their_places() {
        let (socket, _heads) = relay("churn");
        let (links, mut theirs) = links(Some(egress(Network::Open, &[], Some(socket))));
        for id in 1..=(MAX_CARRIERS as u64 + 8) {
            links.up(Link::Open {
                link: id,
                host: "hung.test".into(),
                port: 443,
            });
            links.up(Link::Shut { link: id });
        }
        let mut refused = 0;
        for _ in 0..8 {
            let Link::Refused { why, .. } = down(&mut theirs) else {
                panic!("not refused");
            };
            assert!(why.contains("already open"), "{why}");
            refused += 1;
        }
        assert_eq!(refused, 8);
        assert_eq!(links.open.lock().unwrap().live, MAX_CARRIERS);
    }

    /// A reason past what a refusal carries is cut, so the tool host
    /// still reads it.
    #[test]
    fn a_long_refusal_is_cut_to_the_bound() {
        let (socket, _heads) = relay("long");
        let (links, mut theirs) = links(Some(egress(Network::Open, &[], Some(socket))));
        links.up(Link::Open {
            link: 1,
            host: "long.test".into(),
            port: 443,
        });
        let Link::Refused { why, .. } = down(&mut theirs) else {
            panic!("not refused");
        };
        assert_eq!(why.len(), MAX_LINK_WHY);
        assert_eq!(bounded("é".repeat(MAX_LINK_WHY)).len(), MAX_LINK_WHY);
        assert_eq!(
            bounded(format!("a{}", "é".repeat(MAX_LINK_WHY))).len(),
            MAX_LINK_WHY - 1
        );
    }

    /// A write that times out breaks the pipe, and every later write fails
    /// at once rather than waiting its turn.
    #[test]
    fn a_tool_host_that_stops_reading_fails_writes_at_once() {
        let (ours, _theirs) = UnixStream::pair().unwrap();
        ours.set_write_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let pipe = Pipe::new(ours, Duration::from_millis(300));
        let chunk = vec![0u8; 64 * 1024];
        let mut written = 0;
        while pipe.frame(&chunk).is_ok() {
            written += 1;
            assert!(written < 10_000, "the pipe never filled");
        }
        let began = std::time::Instant::now();
        assert!(pipe.frame(b"x").is_err());
        assert!(began.elapsed() < Duration::from_millis(50));
    }

    /// A tool host that reads a little now and then never stretches a
    /// frame past its deadline.
    #[test]
    fn a_tool_host_reading_a_trickle_fails_the_frame_by_its_deadline() {
        let (ours, mut theirs) = UnixStream::pair().unwrap();
        ours.set_write_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        let done = Arc::new(AtomicBool::new(false));
        let reading = Arc::clone(&done);
        std::thread::spawn(move || {
            let mut byte = [0u8; 1];
            while !reading.load(Ordering::Acquire) && theirs.read(&mut byte).is_ok() {
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        let pipe = Pipe::new(ours, Duration::from_millis(300));
        let (said, outcome) = mpsc::channel();
        std::thread::spawn(move || {
            let began = Instant::now();
            let written = pipe.frame(&vec![0u8; frame::MAX_FRAME]);
            let _ = said.send((written.is_err(), began.elapsed()));
        });
        let (failed, took) = outcome.recv_timeout(Duration::from_secs(10)).unwrap();
        done.store(true, Ordering::Release);
        assert!(failed);
        assert!(took < Duration::from_secs(2), "{took:?}");
    }

    /// Writes each frame whole once given a turn, recording them in order.
    struct Gate {
        written: Arc<Mutex<Vec<Vec<u8>>>>,
        turns: Receiver<()>,
    }

    impl Write for Gate {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.turns
                .recv()
                .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
            self.written.lock().unwrap().push(bytes.to_vec());
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A call's frame goes next after the one being written, ahead of
    /// every link's waiting.
    #[test]
    fn a_calls_frame_goes_before_links_waiting() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let (turn, turns) = mpsc::channel();
        let pipe = Pipe::new(
            Gate {
                written: Arc::clone(&written),
                turns,
            },
            Duration::from_secs(10),
        );
        let spawn = |pipe: &Arc<Pipe>, bytes: &'static [u8], call: bool| {
            let pipe = Arc::clone(pipe);
            std::thread::spawn(move || {
                if call {
                    pipe.frame(bytes)
                } else {
                    pipe.link(bytes)
                }
            })
        };
        let mut threads = vec![spawn(&pipe, b"first", false)];
        std::thread::sleep(Duration::from_millis(50));
        for _ in 0..4 {
            threads.push(spawn(&pipe, b"link", false));
        }
        std::thread::sleep(Duration::from_millis(50));
        threads.push(spawn(&pipe, b"call", true));
        std::thread::sleep(Duration::from_millis(50));
        for _ in 0..6 {
            turn.send(()).unwrap();
        }
        for thread in threads {
            thread.join().unwrap().unwrap();
        }
        let written = written.lock().unwrap();
        assert_eq!(written.len(), 6);
        assert!(written[0].ends_with(b"first"));
        assert!(written[1].ends_with(b"call"), "{:?}", written[1]);
    }

    /// A link's frame kept waiting by calls past the deadline breaks the
    /// pipe rather than being dropped.
    #[test]
    fn a_links_frame_kept_waiting_breaks_the_pipe() {
        let (turn, turns) = mpsc::channel();
        let pipe = Pipe::new(
            Gate {
                written: Arc::new(Mutex::new(Vec::new())),
                turns,
            },
            Duration::from_millis(200),
        );
        pipe.urgent.fetch_add(1, Ordering::AcqRel);
        assert!(pipe.link(b"link").is_err());
        pipe.urgent.fetch_sub(1, Ordering::AcqRel);
        turn.send(()).unwrap();
        assert!(pipe.frame(b"call").is_err());
    }

    /// A link the policy asks of waits, nothing sent down, until the
    /// conversation answers: opened, it goes through the relay; and the
    /// answer holds for the rest of the instance, a later link to the
    /// destination taking it without asking.
    #[test]
    fn a_link_asked_of_waits_for_the_answer_and_keeps_it() {
        let (socket, heads) = relay("asked");
        let (asker, asked) = asker();
        let (links, mut theirs) = asking(
            Some(egress(Network::Allowlist, &[], Some(socket))),
            Some(asker),
        );
        assert_eq!(links.call(), Some(7));
        links.up(Link::Open {
            link: 1,
            host: "Elsewhere.example".into(),
            port: 443,
        });
        let Ok(Asked::Waits {
            link,
            destination,
            why,
            allow,
            ..
        }) = asked.recv_timeout(Duration::from_secs(5))
        else {
            panic!("not asked");
        };
        assert_eq!(
            (link, destination.text()),
            (1, "elsewhere.example".to_string())
        );
        assert!(why.contains("not on this workspace's allowlist"), "{why}");
        assert!(allow);
        assert!(links.waits(1));
        theirs
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        assert!(frame::read(&mut theirs).is_err());
        theirs
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        links.answer(&destination, Ok(()));
        assert!(!links.waits(1));
        assert_eq!(down(&mut theirs), Link::Opened { link: 1 });
        assert_eq!(
            heads.recv_timeout(Duration::from_secs(5)).unwrap(),
            "td-egress 1\nconnect elsewhere.example 443\n\n"
        );
        links.up(Link::Open {
            link: 2,
            host: "elsewhere.example".into(),
            port: 443,
        });
        assert_eq!(down(&mut theirs), Link::Opened { link: 2 });
        assert!(asked.recv_timeout(Duration::from_millis(200)).is_err());
    }

    /// A refusal is given to every link waiting on its destination and
    /// kept; a link that waits and is shut, or whose tool host goes, is
    /// said to be gone.
    #[test]
    fn a_refused_or_gone_link_is_said() {
        let (asker, asked) = asker();
        let (links, mut theirs) = asking(Some(egress(Network::Allowlist, &[], None)), Some(asker));
        for link in [1, 2] {
            links.up(Link::Open {
                link,
                host: "elsewhere.example".into(),
                port: 443,
            });
        }
        links.up(Link::Open {
            link: 3,
            host: "third.example".into(),
            port: 443,
        });
        let mut waiting = Vec::new();
        for _ in 0..3 {
            let Ok(Asked::Waits { destination, .. }) = asked.recv_timeout(Duration::from_secs(5))
            else {
                panic!("not asked");
            };
            waiting.push(destination);
        }
        links.answer(&waiting[0], Err("no".into()));
        for link in [1, 2] {
            assert_eq!(
                down(&mut theirs),
                Link::Refused {
                    link,
                    why: "no".into()
                }
            );
        }
        links.up(Link::Open {
            link: 4,
            host: "elsewhere.example".into(),
            port: 443,
        });
        assert_eq!(
            down(&mut theirs),
            Link::Refused {
                link: 4,
                why: "no".into()
            }
        );
        links.up(Link::Shut { link: 3 });
        assert!(matches!(
            asked.recv_timeout(Duration::from_secs(5)),
            Ok(Asked::Gone { link: 3, .. })
        ));
        links.up(Link::Open {
            link: 5,
            host: "fifth.example".into(),
            port: 443,
        });
        assert!(matches!(
            asked.recv_timeout(Duration::from_secs(5)),
            Ok(Asked::Waits { link: 5, .. })
        ));
        links.close_all();
        assert!(matches!(
            asked.recv_timeout(Duration::from_secs(5)),
            Ok(Asked::Gone { link: 5, .. })
        ));
    }

    /// A judge changed in place reaches the next link of an instance
    /// already running.
    #[test]
    fn a_changed_judge_reaches_a_running_instance() {
        let (socket, _heads) = relay("changed");
        let judge: Judge = Arc::new(Mutex::new(egress(Network::Open, &[], Some(socket))));
        let (ours, mut theirs) = UnixStream::pair().unwrap();
        theirs
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let links = Links::new(
            Some(Linked {
                judge: Arc::clone(&judge),
                asker: None,
                call: 1,
            }),
            Pipe::new(ours, PIPE_WRITE),
        );
        links.up(Link::Open {
            link: 1,
            host: "a.example".into(),
            port: 443,
        });
        assert_eq!(down(&mut theirs), Link::Opened { link: 1 });
        judge.lock().unwrap().network = Network::Off;
        links.up(Link::Open {
            link: 2,
            host: "a.example".into(),
            port: 443,
        });
        assert!(matches!(down(&mut theirs), Link::Refused { link: 2, why } if why.contains("off")));
    }

    /// A rule's answer is kept for no other link; the person's is kept
    /// only where the allowlist alone asks, so a rule that asks later
    /// still asks; and an instance asks about at most `MAX_ASKED`
    /// destinations, each said with its call, which a gone link names
    /// too.
    #[test]
    fn what_is_kept_and_how_much_is_asked() {
        let (asker, asked) = asker();
        let judge = egress(Network::Allowlist, &[], None);
        let (links, mut theirs) = asking(Some(judge), Some(asker));
        let open = |link: u64, host: &str| {
            links.up(Link::Open {
                link,
                host: host.into(),
                port: 443,
            })
        };
        let waits = || match asked.recv_timeout(Duration::from_secs(5)) {
            Ok(Asked::Waits {
                link,
                call,
                destination,
                ..
            }) => (link, call, destination),
            _ => panic!("not asked"),
        };
        open(1, "a.example");
        let (link, call, a) = waits();
        assert_eq!((link, call), (1, 7));
        links.settle(1, Err("a rule".into()));
        assert_eq!(
            down(&mut theirs),
            Link::Refused {
                link: 1,
                why: "a rule".into()
            }
        );
        open(2, "a.example");
        assert_eq!(waits().0, 2);
        links.answer(&a, Err("the person".into()));
        assert!(matches!(down(&mut theirs), Link::Refused { link: 2, .. }));
        open(3, "a.example");
        assert!(matches!(down(&mut theirs), Link::Refused { link: 3, why } if why == "the person"));
        // A rule that asks now outranks the person's kept answer.
        links.linked.as_ref().unwrap().judge.lock().unwrap().rules =
            sourced(&["ask network a.example"]);
        open(4, "a.example");
        assert_eq!(waits().0, 4);
        links.up(Link::Shut { link: 4 });
        assert!(matches!(
            asked.recv_timeout(Duration::from_secs(5)),
            Ok(Asked::Gone {
                link: 4,
                call: 7,
                ..
            })
        ));
        // One destination asked about so far, however many links; the
        // rest of the budget, then refusals of a new one alone.
        for n in 0..(MAX_ASKED as u64 - 1) {
            open(10 + n, &format!("h{n}.example"));
            assert_eq!(waits().0, 10 + n);
            links.settle(10 + n, Err("no".into()));
            assert!(matches!(down(&mut theirs), Link::Refused { .. }));
        }
        open(1000, "over.example");
        assert!(matches!(
            down(&mut theirs),
            Link::Refused { link: 1000, why } if why.contains("asked about 64 destinations")
        ));
        open(1001, "h0.example");
        assert_eq!(waits().0, 1001);
    }

    /// A link the tool host shuts between being taken and given its
    /// answer is given none: nothing is opened for it.
    #[test]
    fn a_link_shut_while_its_answer_is_given_gets_none() {
        let (socket, heads) = relay("shut-taken");
        let (asker, asked) = asker();
        let (links, mut theirs) = asking(
            Some(egress(Network::Allowlist, &[], Some(socket))),
            Some(asker),
        );
        links.up(Link::Open {
            link: 1,
            host: "a.example".into(),
            port: 443,
        });
        let Ok(Asked::Waits { destination, .. }) = asked.recv_timeout(Duration::from_secs(5))
        else {
            panic!("not asked");
        };
        let taken = links.take_answer(&destination, &Ok(()));
        assert_eq!(taken.len(), 1);
        links.up(Link::Shut { link: 1 });
        links.give(taken, &Ok(()));
        assert!(heads.recv_timeout(Duration::from_millis(300)).is_err());
        theirs
            .set_read_timeout(Some(Duration::from_millis(300)))
            .unwrap();
        assert!(frame::read(&mut theirs).is_err());
    }
}

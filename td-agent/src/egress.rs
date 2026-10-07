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
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::config::{Destination, Network};
use crate::frame;
use crate::host::{Credit, Down, Link, LINK_CHUNK, LINK_WINDOW, MAX_LINKS, MAX_LINK_WHY};

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

/// What a workspace's links are judged by and opened through.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Egress {
    pub network: Network,
    pub allowlist: Vec<Destination>,
    /// The relay's socket; none when td-agent was not launched with one.
    pub relay: Option<PathBuf>,
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
    /// Whether `host:port` may be opened, and why not: any under `open`;
    /// one on the allowlist under `allowlist`; none under `off`, whose
    /// instances have no proxy to ask.
    pub fn judge(&self, host: &str, port: u16) -> Result<(), String> {
        let named = if host.contains(':') {
            format!("[{host}]:{port}")
        } else {
            format!("{host}:{port}")
        };
        let destination = Destination::parse(&named)?;
        match self.network {
            Network::Off => Err("this workspace's network policy is off".into()),
            Network::Open => Ok(()),
            Network::Allowlist if self.allowlist.contains(&destination) => Ok(()),
            Network::Allowlist => Err(format!(
                "{} is not on this workspace's allowlist",
                destination.text()
            )),
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
    egress: Option<Egress>,
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
    pub fn new(egress: Option<Egress>, pipe: Arc<Pipe>) -> Arc<Self> {
        Arc::new(Self {
            egress,
            pipe,
            open: Mutex::new(Open::default()),
        })
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
        // Judged here, so a refusal costs no thread.
        let judged = match &self.egress {
            Some(egress) => egress.judge(&host, port),
            None => Err("this instance has no network".into()),
        };
        if let Err(why) = judged {
            return self.refuse(id, why);
        }
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
        let socket = self.egress.as_ref().and_then(|e| e.relay.as_ref()).ok_or(
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
    pub fn close_all(&self) {
        let entries = self
            .open
            .lock()
            .map(|mut open| std::mem::take(&mut open.entries))
            .unwrap_or_default();
        for entry in entries.values() {
            entry.peer.credit.close();
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
            relay,
        }
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
        assert_eq!(allow.judge("static.crates.io", 443), Ok(()));
        assert_eq!(allow.judge("Static.Crates.IO.", 443), Ok(()));
        assert_eq!(allow.judge("git.example.org", 8443), Ok(()));
        assert_eq!(allow.judge("2001:db8:0::1", 22), Ok(()));
        let said = allow.judge("static.crates.io", 80).unwrap_err();
        assert!(
            said.contains("static.crates.io:80 is not on this workspace's allowlist"),
            "{said}"
        );
        assert!(allow.judge("git.example.org", 443).is_err());
        assert!(allow.judge("crates.io", 443).is_err());
        let open = egress(Network::Open, &[], None);
        assert_eq!(open.judge("anything.example", 22), Ok(()));
        assert!(open.judge("a_b", 443).is_err());
        assert!(open.judge("10.1", 443).is_err());
        let off = egress(Network::Off, &["static.crates.io"], None);
        assert!(off.judge("static.crates.io", 443).is_err());
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
        let (ours, theirs) = UnixStream::pair().unwrap();
        theirs
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        (Links::new(egress, Pipe::new(ours, PIPE_WRITE)), theirs)
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
}

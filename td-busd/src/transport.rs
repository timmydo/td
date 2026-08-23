//! The socket, and one connection's life on it.
//!
//! Everything here is safe `std` but for what it borrows from `sys`: the
//! listener, the accept loop and the byte buffers are ordinary Rust, and the
//! three rostered syscalls appear only through `sys`'s wrappers.
//!
//! **One thread per connection**, which is a decision rather than a default. A
//! single-threaded broker would `poll(2)` or `epoll(7)`, and neither is in
//! stable `std` — so the design that looks cheaper would cost a fourth syscall
//! on surface #10, for a session bus whose peer count is the number of
//! applications a person has open. Threads are `std`, so they cost nothing on
//! the roster, and rung 14's routing gets a registry behind a lock rather than
//! a readiness set to rebuild on every wakeup.

use std::fs;
use std::io;
use std::os::fd::{OwnedFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::auth::{Guid, Handshake, PeerIdentity, GUID_LEN};
use crate::message;
use crate::registry::{Bus, Outbox, Overflow, Rejected};
use crate::sys::{self, PeerCredential};
use crate::wire::{WireError, Writer};

/// The bus's own name, path and interface. A message addressed here is for the
/// broker itself; anything else is for another peer.
pub const BUS_NAME: &str = "org.freedesktop.DBus";
pub const BUS_PATH: &str = "/org/freedesktop/DBus";
pub const PEER_INTERFACE: &str = "org.freedesktop.DBus.Peer";

/// The largest frame `message::frame_len` can return, and so the most this
/// will ever hold for one message.
///
/// DERIVED from the bounds §D states — a 16 MiB body and 64 KiB of header
/// fields — rather than chosen. A first draft picked a round 8 MiB, which is
/// not a smaller version of §D's bound but a DIFFERENT one: it refused legal
/// messages between 8 and 16 MiB that the codec had already accepted.
///
/// The correction had its own bug, and it is worth recording because it is the
/// same shape twice. The derivation carried a `+ 8` for the padding between
/// the header fields and the body — padding that `pad8(16 + 65536)` never
/// needs, since 65552 is already 8-aligned. So the constant sat exactly EIGHT
/// bytes above any frame `frame_len` can produce, and the `length >
/// MAX_MESSAGE` refusal it was written for could not fire. That refusal is
/// gone: what bounds a message is the codec refusing an oversized body or
/// field array, and a second check that cannot fire is a dead branch shaped
/// like a safety check.
pub const MAX_MESSAGE: usize = (message::HEADER_LEN
    + message::MAX_HEADER_FIELDS_BYTES as usize)
    .next_multiple_of(8)
    + message::MAX_BODY_BYTES as usize;

/// §D's connection ceiling. A bus that accepted without bound would hand one
/// silent client a thread and a descriptor apiece until it had neither left
/// for the portal, the compositor or anything else.
pub const MAX_CONNECTIONS: usize = 64;

/// The same ceiling's other half. §D asks for 64 "per instance as well as
/// globally", and says why in terms: a GLOBAL cap alone is the denial of
/// service, since one jailed app reaches it by opening 64 sockets and saying
/// nothing, and every other application is then locked off the bus.
///
/// §D maps a peer to its instance by `SO_PEERCRED.pid` through the registered
/// jail instance, and that registry lands with the integration rung. Until it
/// does, the pid IS the finest identity this broker has, so the share is
/// counted per pid: an approximation that is wrong only where one instance
/// spans several processes, and right about the case §D names. The number is
/// a quarter of the ceiling, so four busy peers still fit and no single one
/// can lock the rest out.
pub const MAX_CONNECTIONS_PER_PEER: usize = MAX_CONNECTIONS / 4;

/// Descriptors queued across the WHOLE bus, not per connection.
///
/// A per-connection cap alone does not do what it was added for. `MAX_FDS` is
/// 64 because one legal message may carry that many, and `MAX_CONNECTIONS` is
/// 64, so the product is 4096 descriptors — well past a 1024 `RLIMIT_NOFILE`,
/// and reachable by one client with one descriptor to resend. The refusal that
/// was meant to keep this "a disconnect rather than an `EMFILE` somewhere
/// unrelated" would have produced exactly that `EMFILE`. Four messages' worth
/// across the bus leaves the sockets, the listener and stdio their room.
pub const MAX_QUEUED_FDS_TOTAL: usize = sys::MAX_FDS * 4;

/// One read's worth. Large enough that an ordinary method call arrives whole,
/// small enough that a connection's idle cost is a page rather than a message.
const READ_CHUNK: usize = 8192;

/// How long `probe` waits for a bus to say anything. A bus that accepts and
/// then stalls — a wedged thread, or a non-td process squatting the path —
/// would otherwise block it for ever, and `APPLICATIONS.md` §A's `ready=` line
/// is what calls it. A supervision check that can hang is one that reports
/// nothing rather than failure, which is the shape of mistake `guid_text`
/// above already records.
/// How long a connection that is ending waits for what it has already
/// queued to reach the socket. Bounded: a peer that is alive and not reading
/// would otherwise hold the reader thread for as long as it liked, which is a
/// denial of service dressed as politeness.
const FAREWELL: std::time::Duration = std::time::Duration::from_secs(2);

const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// How long the selftest's one-peer listener waits to be probed before giving
/// up. Longer than the probe's own wait, so the probe is what reports.
const LOOPBACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// The most descriptors that may be queued for a connection before it is
/// refused. A peer that sends descriptors and never the messages claiming them
/// is a peer filling this process's descriptor table, and the refusal is what
/// keeps that a disconnect rather than an `EMFILE` somewhere unrelated.
const MAX_QUEUED_FDS: usize = sys::MAX_FDS;

/// What the bus may hand out at once, and to whom. One per `run`, shared by
/// every connection thread.
///
/// Both counts are the bus's, not a connection's: a per-connection limit
/// cannot see the peer that opens many connections, which is the shape of
/// every abuse §D describes here.
#[derive(Default)]
pub struct Quota {
    /// One entry per live connection, holding its peer's pid. A `Vec` scanned
    /// linearly rather than a map: it holds at most `MAX_CONNECTIONS`, and the
    /// scan happens once per accept.
    live: std::sync::Mutex<Vec<i32>>,
    /// Descriptors queued and unclaimed across every connection.
    queued_fds: std::sync::atomic::AtomicUsize,
}

/// A live connection's place in the quota, given back when it ends — however
/// it ends. Counted by hand at each exit, this is a ceiling that only falls.
///
/// It OWNS its share of the quota rather than borrowing it, because the thread
/// it travels to outlives the frame that admitted it: connection threads are
/// detached, so there is no scope whose lifetime a borrow could use.
pub struct Admitted {
    quota: std::sync::Arc<Quota>,
    pid: i32,
}

impl Admitted {
    pub fn quota(&self) -> &Quota {
        &self.quota
    }
}

impl Drop for Admitted {
    fn drop(&mut self) {
        if let Ok(mut live) = self.quota.live.lock() {
            if let Some(at) = live.iter().position(|pid| *pid == self.pid) {
                live.swap_remove(at);
            }
        }
    }
}

/// The kernel's account of who is on the other end. `main` asks through here
/// rather than reaching into `sys`, which the roster says only this module
/// does — and the confinement test enforces.
pub fn peer_of(stream: &UnixStream) -> io::Result<PeerCredential> {
    sys::peer_credential(stream)
}

impl Quota {
    pub fn new() -> Self {
        Self::default()
    }

    /// Take a place for a peer, or say why there is none. A poisoned lock is
    /// treated as a full bus rather than unwrapped: this is the accept path,
    /// and refusing a peer is always available where panicking is not.
    pub fn try_admit(self: &std::sync::Arc<Self>, pid: i32) -> Result<Admitted, String> {
        let mut live = self
            .live
            .lock()
            .map_err(|_| "the connection table is poisoned".to_string())?;
        if live.len() >= MAX_CONNECTIONS {
            return Err(format!("already serving {}", live.len()));
        }
        let mine = live.iter().filter(|held| **held == pid).count();
        if mine >= MAX_CONNECTIONS_PER_PEER {
            return Err(format!("pid {pid} already holds {mine}"));
        }
        live.push(pid);
        drop(live);
        Ok(Admitted {
            quota: std::sync::Arc::clone(self),
            pid,
        })
    }

    #[cfg(test)]
    fn live_count(&self) -> usize {
        self.live.lock().map(|live| live.len()).unwrap_or(0)
    }

    /// Charge `count` descriptors against the bus's budget, or refuse.
    fn take_fds(&self, count: usize) -> Result<(), String> {
        use std::sync::atomic::Ordering;
        let mut held = self.queued_fds.load(Ordering::Acquire);
        loop {
            let wanted = held.saturating_add(count);
            if wanted > MAX_QUEUED_FDS_TOTAL {
                return Err(format!(
                    "{wanted} descriptors queued across the bus, over {MAX_QUEUED_FDS_TOTAL}"
                ));
            }
            // Compare-and-swap rather than fetch_add-then-check: an add that
            // overshoots and backs off is briefly visible to another thread as
            // a full budget, which refuses a peer that should have been taken.
            match self.queued_fds.compare_exchange_weak(
                held,
                wanted,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(()),
                Err(seen) => held = seen,
            }
        }
    }

    fn release_fds(&self, count: usize) {
        use std::sync::atomic::Ordering;
        // An update rather than `fetch_sub`: a subtraction below zero wraps to
        // an enormous budget, which is the failure that hides itself.
        let _ = self
            .queued_fds
            .try_update(Ordering::AcqRel, Ordering::Acquire, |held| {
                Some(held.saturating_sub(count))
            });
    }
}

/// Where the bus listens, and the directory it needs first.
pub struct Bound {
    listener: UnixListener,
    path: PathBuf,
}

impl Bound {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn listener(&self) -> &UnixListener {
        &self.listener
    }
}

/// Prepare the socket. §D puts the session bus at `/run/user/1000/bus` with its
/// parent at 0700 and the socket itself at 0600.
///
/// BOTH, and the second is set explicitly rather than left to `bind`: a bind
/// creates the socket fresh and umask decides what it lands as, so under the
/// 022 umask `td-login` preserves it lands 0755. The parent is what actually
/// keeps another uid out, and the socket mode is defence behind it — but a
/// design document that says 0600 and a broker that produces 0755 disagree,
/// and that disagreement is the kind this file exists to prevent.
pub fn bind(path: &Path) -> io::Result<Bound> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} has no parent directory", path.display()),
        )
    })?;
    // Only a directory this call CREATES gets its mode set. `bind` takes its
    // path from argv, so an unconditional chmod means `run --socket /tmp/bus`
    // makes `/tmp` private to this uid — a broker reaching outside its own
    // business to break every other user of a directory it was merely passed.
    //
    // An existing parent is left exactly as it is, rather than narrowed OR
    // refused. Refusing a parent that is not already private was the first
    // attempt and is wrong twice over: `/run/user/1000` is td-init's to make
    // (§D's boot table sets it 0700), and any caller that creates its own
    // directory first — the selftest below does — would be turned away from a
    // path it owns. The socket's own 0600 is what defends it where the parent
    // was somebody else's to make.
    let existed = parent.exists();
    fs::create_dir_all(parent)?;
    if !existed {
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }

    // A stale socket from a previous boot would make `bind` fail with
    // EADDRINUSE though nothing is listening. Only a socket is removed: if
    // something else is at that path, that is a misconfiguration to report
    // rather than a file to delete.
    match fs::symlink_metadata(path) {
        Ok(existing) => {
            if !is_socket(existing.permissions().mode()) {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} exists and is not a socket", path.display()),
                ));
            }
            // A socket at this path is not necessarily STALE. Unlinking one
            // that still has a listener leaves the running broker alive and
            // unreachable — every client that connects afterwards reaches this
            // process instead, and the first one's peers are stranded on a
            // socket with no name. Connecting is the only way to tell the two
            // apart: a stale socket refuses with ECONNREFUSED, a live one
            // accepts. The connection made to find out is dropped at once.
            if UnixStream::connect(path).is_ok() {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!("{} already has a listener", path.display()),
                ));
            }
            fs::remove_file(path)?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let listener = UnixListener::bind(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(Bound {
        listener,
        path: path.to_path_buf(),
    })
}

fn is_socket(mode: u32) -> bool {
    mode & 0o170_000 == 0o140_000
}

/// The bus's GUID, one per `run`, as the specification requires: it identifies
/// this bus instance so a client that reconnects can tell it is the same one.
///
/// Read from `/dev/urandom` as a FILE. The kernel's `getrandom(2)` would be a
/// fourth syscall on the roster to obtain 16 bytes that a read already gives.
///
/// `read_exact` of a fixed buffer, never `fs::read`: that reads to EOF, and a
/// character device has none. The first draft did, and the test below hung
/// rather than failed — which is the useful shape of that mistake, since a
/// broker that hangs at startup is one `ready=` waits out.
pub fn guid_text() -> io::Result<String> {
    use std::io::Read;
    let mut file = fs::File::open("/dev/urandom")?;
    let mut bytes = [0u8; GUID_LEN / 2];
    file.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// What a connection is doing with a peer.
pub struct Connection<'a> {
    stream: UnixStream,
    shake: Handshake<'a>,
    /// The GUID this bus advertised, kept for `GetId`: a client that asks must
    /// be told the same one the handshake agreed, not a fresh one.
    guid: &'a str,
    credential: PeerCredential,
    /// The bus's budget, which this connection's queued descriptors are
    /// charged against and returned to.
    quota: &'a Quota,
    /// The directory this connection is routed through.
    bus: &'a Bus,
    /// Where this connection's outgoing frames go. Written by a thread of its
    /// own, so a peer that will not read cannot stall whoever sent to it.
    outbox: Arc<Outbox>,
    /// This connection's unique name, once it has said `Hello`. §D: anything
    /// before `Hello` disconnects, so `None` here is a peer that has not
    /// earned the right to send anything else.
    unique: Option<String>,
    /// The serial of the NEXT message this side sends. The specification
    /// requires a sender's serials to be unique per connection, and a broker
    /// that answered every call with serial 1 would be telling a client that
    /// every reply is the same message. It starts at 1 because zero is not a
    /// legal serial.
    next_serial: u32,
    /// Bytes read and not yet consumed — by the handshake before BEGIN, by the
    /// framer after it.
    inbox: Vec<u8>,
    /// Descriptors received and not yet claimed by a message's `UNIX_FDS`.
    freight: Vec<OwnedFd>,
    /// One read's bytes, reused. `[0u8; READ_CHUNK]` as a local zeroes eight
    /// kilobytes of stack on EVERY read — the per-read hot loop, and a larger
    /// cost than the per-message allocation below that was fixed first.
    chunk: Vec<u8>,
    /// One message's bytes, reused. Framing used to `drain(..len).collect()`
    /// into a fresh `Vec` per message, which puts an allocation in the one
    /// loop on this connection that runs per message rather than per
    /// connection. It is swapped out to satisfy the borrow checker and swapped
    /// back with its capacity, so a busy connection allocates once.
    frame: Vec<u8>,
}

/// Why a connection ended. Every one of these closes the socket; they differ in
/// what gets logged, and in whether the peer did something or merely left.
#[derive(Debug)]
pub enum Ended {
    /// The peer closed its end. The ordinary case.
    PeerLeft,
    /// The peer broke the protocol. §D's rule is that every auth error and
    /// every malformed message ends the connection.
    Refused(String),
    /// This side could not go on.
    Failed(String),
}

impl Drop for Connection<'_> {
    fn drop(&mut self) {
        // Off the directory before anything else: a name that outlived its
        // connection would route a message to a socket nobody is reading.
        if let Some(unique) = self.unique.take() {
            self.bus.leave(&unique);
        }
        self.outbox.close();
        // Whatever is still queued was charged and is about to be closed by
        // `OwnedFd`. Without this the bus's budget only ever falls, and a
        // broker that has served enough connections refuses descriptors it has
        // the room for.
        self.quota.release_fds(self.freight.len());
    }
}

impl<'a> Connection<'a> {
    /// Take a peer the listener accepted. The credential is read HERE, once, at
    /// accept, and is what the handshake admits by — never anything the peer
    /// says about itself.
    pub fn accept(
        stream: UnixStream,
        guid: Guid<'a>,
        quota: &'a Quota,
        bus: &'a Bus,
    ) -> io::Result<Self> {
        let credential = sys::peer_credential(&stream)?;
        let guid_text = guid.as_str();
        // The writer's half of the socket. Cloned here rather than later
        // because a connection without an outbox has no way to be answered,
        // and the failure to make one belongs at accept where it can be seen.
        let outbox = bus.outbox_for(stream.try_clone()?);
        Ok(Connection {
            stream,
            shake: Handshake::new(PeerIdentity::unmapped(credential.uid), guid),
            guid: guid_text,
            credential,
            quota,
            bus,
            outbox,
            unique: None,
            next_serial: 1,
            chunk: vec![0u8; READ_CHUNK],
            inbox: Vec::new(),
            freight: Vec::new(),
            frame: Vec::new(),
        })
    }

    pub fn credential(&self) -> PeerCredential {
        self.credential
    }

    pub fn authenticated_uid(&self) -> Option<u32> {
        self.shake.uid()
    }

    /// Serve this peer until it leaves or breaks the protocol.
    ///
    /// Two threads: this one reads, and the writer below drains the outbox.
    /// EVERY outgoing byte goes through that one writer, the handshake's OK
    /// line included. Writing the handshake here and routed messages there
    /// would be two writers on one socket, and nothing but the order things
    /// happen to occur in would stop them interleaving halfway through a
    /// frame — a bug that would appear only once a peer was busy.
    pub fn serve(&mut self) -> Ended {
        let writing = Arc::clone(&self.outbox);
        let writer = std::thread::Builder::new().spawn(move || {
            while let Some(frame) = writing.take() {
                let size = frame.len();
                let outcome = write_frame(writing.stream(), &frame, &[]);
                // Dropped BEFORE the bytes are given back, so the moment the
                // budget says this connection is holding nothing is a moment
                // at which it really is.
                drop(frame);
                writing.finished(size);
                if let Err(why) = outcome {
                    // The peer is unreachable. Closing is what tells everyone
                    // queuing to it to stop, and what wakes the reader.
                    eprintln!("td-busd: {why}");
                    writing.close();
                    return;
                }
            }
        });
        let writer = match writer {
            Ok(handle) => handle,
            Err(error) => return Ended::Failed(format!("cannot spawn a writer: {error}")),
        };
        let ended = loop {
            match self.pump() {
                Ok(true) => {}
                Ok(false) => break Ended::PeerLeft,
                Err(ended) => break ended,
            }
        };
        // Seal, then flush, then close. The last thing a connection is told
        // is usually WHY it is ending, and closing on the spot throws that
        // away: a peer that makes a bad call and then a fatal one would get a
        // bare EOF rather than the error reply the broker had already written
        // for it. The flush is bounded because the peer may be alive and
        // simply not reading, in which case the writer is blocked in `sendmsg`
        // and would never finish on its own.
        self.outbox.seal();
        self.outbox.flush_within(FAREWELL);
        self.outbox.close();
        let _ = writer.join();
        ended
    }

    /// One read, and everything that read makes possible. `Ok(false)` means the
    /// peer closed its end.
    fn pump(&mut self) -> Result<bool, Ended> {
        let mut chunk = std::mem::take(&mut self.chunk);
        let read = sys::receive(&self.stream, &mut chunk);
        self.chunk = chunk;
        let received = read.map_err(|error| {
            // `InvalidData` is this layer's word for "the peer sent something
            // the protocol does not allow" — a truncated control buffer is
            // the peer over-sending descriptors, not this side failing. Logged
            // as `Failed` it would read as our fault in the one log an
            // operator has, which is how an attack goes down as a glitch.
            if error.kind() == io::ErrorKind::InvalidData {
                Ended::Refused(format!("read: {error}"))
            } else {
                Ended::Failed(format!("read: {error}"))
            }
        })?;
        if received.count == 0 {
            return Ok(false);
        }
        // §D: a client that skips `NEGOTIATE_UNIX_FD` is refused any message
        // carrying a descriptor, per the specification. `AGREE_UNIX_FD` is the
        // server's half of that exchange, and a peer that never asked has no
        // business sending one.
        //
        // The gate is the AGREEMENT and deliberately not `begun()` as well,
        // though a descriptor before BEGIN carries no message that could claim
        // it. A client may pipeline `BEGIN` and its first message into one
        // write — libdbus does — and the kernel attaches that write's
        // descriptors to the read this loop is in the middle of, where BEGIN
        // has not been parsed yet. Requiring `begun()` here would refuse the
        // ordinary case. Descriptors arriving before BEGIN are held as freight
        // and bounded like any other.
        //
        // The descriptors are already owned by `received`, so this refusal
        // CLOSES them on the way out: the ordering rule holds through this
        // path exactly as it does through a truncated control buffer.
        if !received.fds.is_empty() && !self.shake.unix_fd() {
            return Err(Ended::Refused(format!(
                "{} descriptors arrived without AGREE_UNIX_FD",
                received.fds.len()
            )));
        }
        // Descriptors are queued the moment they arrive, already owned. They
        // are claimed later by the message whose UNIX_FDS says so — which may
        // be in this read or a later one, since the kernel attaches them to
        // whichever write carried them and a peer may split a message anywhere.
        let arrived = received.fds.len();
        // Charged BEFORE they join the queue, so a refusal drops them here
        // rather than leaving the bus's count and this connection's disagreeing.
        self.quota
            .take_fds(arrived)
            .map_err(Ended::Refused)?;
        self.freight.extend(received.fds);
        if self.freight.len() > MAX_QUEUED_FDS {
            return Err(Ended::Refused(format!(
                "{} descriptors queued and unclaimed",
                self.freight.len()
            )));
        }
        // Disjoint fields, so this borrows `inbox` mutably and `chunk`
        // immutably without a copy in between.
        let read = received.count.min(self.chunk.len());
        self.inbox
            .extend_from_slice(self.chunk.get(..read).unwrap_or(&[]));
        if !self.shake.begun() {
            self.advance_handshake()?;
        }
        if self.shake.begun() {
            self.advance_messages()?;
        }
        // AFTER the drain, never before it. What bounds this buffer is
        // `message::frame_len` refusing a body past `MAX_BODY_BYTES` or a
        // field array past `MAX_HEADER_FIELDS_BYTES` — not anything in this
        // module — and, before BEGIN, `auth`'s own line and command caps. So
        // what remains here is always an incomplete message and always
        // smaller than the largest complete one. Checked ahead of the drain
        // instead — where the first draft had it — the only thing it can
        // catch is a legitimate maximum-sized message whose last read
        // straddled the end of it, which is a refusal of a peer that did
        // nothing wrong. It stays as a backstop against those codec ceilings
        // changing, and is deliberately NOT the thing relied on.
        if self.inbox.len() > MAX_MESSAGE {
            return Err(Ended::Refused(format!(
                "buffered {} bytes without a complete message",
                self.inbox.len()
            )));
        }
        Ok(true)
    }

    fn advance_handshake(&mut self) -> Result<(), Ended> {
        let fed = self
            .shake
            .feed(&self.inbox)
            .map_err(|error| Ended::Refused(format!("authentication: {error:?}")))?;
        if !fed.reply.is_empty() {
            self.queue_own(fed.reply)?;
        }
        // `consumed` stops at BEGIN, so whatever a client pipelined behind it
        // stays in the inbox and is framed below rather than swallowed here.
        self.inbox.drain(..fed.consumed.min(self.inbox.len()));
        Ok(())
    }

    fn advance_messages(&mut self) -> Result<(), Ended> {
        loop {
            let length = match message::frame_len(&self.inbox) {
                Ok(Some(length)) => length,
                Ok(None) => return Ok(()),
                Err(error) => {
                    return Err(Ended::Refused(format!("frame: {error}")));
                }
            };
            if self.inbox.len() < length {
                return Ok(());
            }
            let mut frame = std::mem::take(&mut self.frame);
            frame.clear();
            frame.extend(self.inbox.drain(..length));
            let dispatched = self.dispatch(&frame);
            // Back before the `?`, so a refused message still returns the
            // buffer rather than leaving the connection to allocate a new one
            // for a message it will never read.
            self.frame = frame;
            dispatched?;
        }
    }

    /// One complete message. Routing is rung 14; what this landing owes is that
    /// the message DECODES against the descriptors that actually arrived, that
    /// the ones it claims are taken off the queue, and that the peer is told
    /// something rather than left waiting.
    fn dispatch(&mut self, frame: &[u8]) -> Result<(), Ended> {
        let available = u32::try_from(self.freight.len()).unwrap_or(u32::MAX);
        let (message, _) = message::decode_from_client(frame, available)
            .map_err(|error| Ended::Refused(format!("message: {error}")))?;
        // The count is already settled: `decode_from_client` was handed
        // `available` and refuses any message whose UNIX_FDS disagrees with
        // it, so by here `wanted` and the queue length are the same number. An
        // earlier draft re-checked `wanted > self.freight.len()` here, which
        // reads like the load-bearing check and is in fact unreachable — the
        // red-check found it by reverting it and watching every test stay
        // green. A dead branch shaped like a safety check is worse than none,
        // because the next reader trusts it.
        let wanted = usize::try_from(message.fields.unix_fds.unwrap_or(0)).unwrap_or(0);
        // Claimed descriptors leave the queue with the message that named them.
        // Forwarding them is the descriptor half of rung 14; this increment
        // routes bytes, and a message carrying descriptors is refused rather
        // than delivered without them — silently dropping an `h` a recipient
        // will index is worse than saying no.
        let claimed: Vec<OwnedFd> = self.freight.drain(..wanted.min(self.freight.len())).collect();
        self.quota.release_fds(claimed.len());
        drop(claimed);

        // A message type this version does not know is IGNORED, which the
        // specification requires and which has to be decided HERE rather than
        // in the relay. A draft refused it in `restamp`, so the disposition
        // depended on whether the destination happened to exist: the same
        // message was silently dropped when addressed to a name nobody owned
        // and disconnected the sender when addressed to a live peer. One rule,
        // one place, before anything looks at where it was going.
        //
        // Below the Hello gate, so an unknown type from a nameless connection
        // still disconnects: §D's rule is that ANYTHING before `Hello` does.
        //
        // §D: `Hello` first, and anything before it disconnects. The rule is
        // the specification's and it is load-bearing here rather than
        // ceremonial — a peer with no unique name has no SENDER to stamp on
        // what it sends, so routing it would mean delivering a message whose
        // origin the recipient cannot check.
        //
        // A `Hello` with no DESTINATION counts. Real clients set one and the
        // specification says to, but there is nothing else a connection with
        // no name could be addressing: the broker is the only peer it can
        // reach. Requiring the field made the `None` alternative in the match
        // below unreachable — a dead arm shaped like a live case, which is the
        // same fault as the dead branch this file removed above — and it turned
        // a lenient case into a disconnect whose reason read "Hello before
        // Hello". The INTERFACE is not checked for the same reason.
        let is_hello = message.kind == message::MessageType::MethodCall
            && message.fields.member == Some("Hello")
            && matches!(message.fields.destination, None | Some(BUS_NAME))
            && matches!(message.fields.interface, None | Some(BUS_NAME))
            && on_the_bus_object(&message);
        if self.unique.is_none() && !is_hello {
            // The reason is spelled out rather than reported as "Hello before
            // Hello", which is what a peer got when it sent a `Hello` this
            // broker did not recognise AS one.
            let what = match message.fields.member {
                Some("Hello") => "a Hello that is not org.freedesktop.DBus.Hello \
                                  at /org/freedesktop/DBus",
                Some(member) => member,
                None => "a message",
            };
            return Err(Ended::Refused(format!("{what} before Hello")));
        }

        // What may be answered at all. Replying to a signal, a method return
        // or an error is itself a protocol violation — the specification
        // reserves replies for METHOD_CALL — and a message type this version
        // does not know must be IGNORED rather than answered or refused.
        // `NO_REPLY_EXPECTED` withdraws the reply from a call that would
        // otherwise take one, and a sender that set it is not waiting.
        //
        // The first draft answered everything, which is the more obvious
        // reading of "never leave a caller waiting" and the wrong one: a
        // caller that is not waiting cannot be left waiting, and an error
        // addressed to a signal is a message its sender has no serial for.
        let wants_reply = message.kind == message::MessageType::MethodCall
            && message.flags & message::FLAG_NO_REPLY_EXPECTED == 0;

        // Stated once, here, rather than left to emerge from three places
        // that each happen to drop it: `wants_reply` is false for any type
        // that is not a method call, so the bus path and both routing paths
        // would all fall through to `Ok(())` on their own. That makes this
        // early return invisible to behaviour — the red-check records it as
        // uncoverable for exactly that reason — and keeping it is still right.
        // A rule that holds because three unrelated branches agree is a rule
        // that stops holding when one of them changes. It also keeps a peer
        // from writing one relay-failure line to the journal per message.
        if matches!(message.kind, message::MessageType::Unknown(_)) {
            return Ok(());
        }

        // Addressed to the broker, or to a peer?
        match message.fields.destination {
            Some(BUS_NAME) | None if is_hello => self.say_hello(&message, wants_reply),
            Some(BUS_NAME) => self.bus_method(&message, wants_reply),
            Some(destination) => self.route(&message, destination, wants_reply, wanted),
            // No DESTINATION and not `Hello`. A SIGNAL without one is a
            // broadcast, which needs the match rules that land next, so it is
            // accepted and goes nowhere rather than being refused — a signal
            // has no reply to be missing, and erroring one would be answering
            // a message that was addressed to nobody. A method CALL without
            // one is the opposite case and gets the opposite treatment: the
            // caller is waiting on a serial, and until match rules land there
            // is no route that could ever produce the reply, so it is told
            // rather than left to time out.
            None if wants_reply => self.refuse(
                &message,
                "org.freedesktop.DBus.Error.NotSupported",
                "a method call must name a destination on this bus; \
                 undirected delivery lands with match rules",
            ),
            None => Ok(()),
        }
    }

    /// `Hello`: the connection earns its unique name and is told what it is.
    ///
    /// `wants_reply` is honoured here as everywhere else. A `Hello` carrying
    /// `NO_REPLY_EXPECTED` is a strange thing for a client to send, but it is
    /// the one bus method that changes state, so it still earns the name and
    /// simply is not answered — a draft answered it regardless, which made
    /// this the single method that ignored the flag.
    fn say_hello(
        &mut self,
        message: &message::Message<'_>,
        wants_reply: bool,
    ) -> Result<(), Ended> {
        if self.unique.is_some() {
            // The specification says a second `Hello` is an error, and §D says
            // the connection ends. A peer that asks twice has lost track of
            // its own identity, which is not a state to keep serving.
            return Err(Ended::Refused("Hello twice".into()));
        }
        let unique = self
            .bus
            .reserve()
            .map_err(|why| Ended::Failed(format!("cannot name a peer: {why}")))?;
        // Recorded before anything that can fail, so `Drop` leaves the bus
        // under this name whatever happens next. A draft set it only after the
        // reply encoded, which leaked the directory entry on any failure in
        // between — and once `publish` moved below the reply, it would have
        // been a name reserved and never released.
        self.unique = Some(unique.clone());
        if wants_reply {
            let reply = message::Builder::method_return(message.endian, message.serial)
                .sender(BUS_NAME)
                .destination(&unique)
                .serial(self.take_serial())
                .body("s", |writer| writer.string(&unique))
                .map_err(|error| Ended::Failed(format!("Hello body: {error}")))?
                .encode()
                .map_err(|error| Ended::Failed(format!("Hello reply: {error}")))?;
            self.queue_own(reply)?;
        }
        // Routable only now: the reply is already ahead of anything another
        // peer can aim at this name.
        self.bus
            .publish(&unique, &self.outbox, self.credential.uid, self.credential.pid)
            .map_err(|why| Ended::Failed(format!("cannot name a peer: {why}")))
    }

    /// The bus's own interface: what a peer may ask the broker itself.
    ///
    /// Everything here is answered from the directory, and none of it reaches
    /// another peer. §D's rule for the rest is `UnknownMethod` rather than
    /// silence — a client that calls a method this bus does not have should
    /// find out now, not on a reply that never comes. `RequestName`,
    /// `AddMatch` and their neighbours land with match rules; until then they
    /// are honestly absent rather than quietly accepted, because a client told
    /// its match rule was installed and then never signalled is worse off than
    /// one told there is no such method.
    fn bus_method(
        &mut self,
        message: &message::Message<'_>,
        wants_reply: bool,
    ) -> Result<(), Ended> {
        let member = message.fields.member.unwrap_or("");
        let interface = message.fields.interface;
        // INTERFACE is optional in a method call, so a message that omits it is
        // taken at its member. One that names a DIFFERENT interface is not: a
        // `Ping` sent to the bus on some application's interface is that
        // application's method, and answering it here would be the broker
        // impersonating a peer.
        let on = |name: &str| interface.is_none() || interface == Some(name);
        // `org.freedesktop.DBus`'s methods live on ONE object. `Peer` does
        // not — the specification puts it on every object a connection
        // exposes — so `Ping` is answered wherever it is addressed and the
        // rest is answered only at the broker's own path. Without this, a
        // call to `/org/example/Thing` addressed to the bus name would be
        // answered by the broker as though it were that object.
        let here = on_the_bus_object(message);
        // A peer that is not waiting gets no answer, but the work above still
        // has to happen for the ones that change state. None of these do.
        if !wants_reply {
            return Ok(());
        }
        match member {
            "Ping" if on(PEER_INTERFACE) => {
                if !self.takes(message, "")? {
                    return Ok(());
                }
                self.answer(message, "", |_| Ok(()))
            }
            "GetId" if here && on(BUS_NAME) => {
                if !self.takes(message, "")? {
                    return Ok(());
                }
                let guid = self.guid.to_string();
                self.answer(message, "s", move |writer| writer.string(&guid))
            }
            "ListNames" if here && on(BUS_NAME) => {
                if !self.takes(message, "")? {
                    return Ok(());
                }
                let mut names = vec![BUS_NAME.to_string()];
                names.extend(self.bus.names());
                self.answer(message, "as", move |writer| {
                    writer.array("s", |inner| {
                        for name in &names {
                            inner.string(name)?;
                        }
                        Ok(())
                    })
                })
            }
            // td has no service activation: nothing on this bus starts because
            // it was called. An empty list is the true answer, and it is the
            // one that lets a client get on with its life.
            "ListActivatableNames" if here && on(BUS_NAME) => {
                if !self.takes(message, "")? {
                    return Ok(());
                }
                self.answer(message, "as", |writer| writer.array("s", |_| Ok(())))
            }
            "NameHasOwner" if here && on(BUS_NAME) => {
                let Some(asked) = self.bus_name_argument(message)? else {
                    return Ok(());
                };
                let held = asked == BUS_NAME || self.bus.route(&asked).is_some();
                self.answer(message, "b", move |writer| {
                    writer.bool(held);
                    Ok(())
                })
            }
            "GetNameOwner" if here && on(BUS_NAME) => {
                let Some(asked) = self.bus_name_argument(message)? else {
                    return Ok(());
                };
                // The bus owns its own name, and says so: a client that asks
                // who `org.freedesktop.DBus` is should not be told nobody.
                if asked == BUS_NAME || self.bus.route(&asked).is_some() {
                    self.answer(message, "s", move |writer| writer.string(&asked))
                } else {
                    self.refuse(
                        message,
                        "org.freedesktop.DBus.Error.NameHasNoOwner",
                        "no such connection on this bus",
                    )
                }
            }
            // What the kernel says about whoever is behind a name. td's
            // confinement story rests on this being the broker's answer rather
            // than the peer's: an application that asks who called it is asking
            // `SO_PEERCRED`, taken once at accept and not re-derived from
            // anything the caller sent.
            "GetConnectionUnixUser" if here && on(BUS_NAME) => {
                let Some(asked) = self.bus_name_argument(message)? else {
                    return Ok(());
                };
                match self.credentials_for(&asked) {
                    // A uid of 0 is root, not "unknown", so this needs no
                    // guard where the pid below does.
                    Some((uid, _)) => self.answer(message, "u", move |writer| {
                        writer.uint32(uid);
                        Ok(())
                    }),
                    None => self.no_such_owner(message),
                }
            }
            "GetConnectionUnixProcessID" if here && on(BUS_NAME) => {
                let Some(asked) = self.bus_name_argument(message)? else {
                    return Ok(());
                };
                match self.credentials_for(&asked) {
                    None => self.no_such_owner(message),
                    Some((_, pid)) => match usable_pid(pid) {
                        Some(pid) => self.answer(message, "u", move |writer| {
                            writer.uint32(pid);
                            Ok(())
                        }),
                        // The name IS here; its pid is not knowable. A draft
                        // answered `NameHasNoOwner`, which `ListNames` and
                        // `GetNameOwner` contradict one call later. The
                        // specification has an error for exactly this case.
                        None => self.refuse(
                            message,
                            "org.freedesktop.DBus.Error.UnixProcessIdUnknown",
                            "the kernel did not report a pid for that connection",
                        ),
                    },
                }
            }
            "GetConnectionCredentials" if here && on(BUS_NAME) => {
                let Some(asked) = self.bus_name_argument(message)? else {
                    return Ok(());
                };
                // As many credentials as are known, which is what the
                // `a{sv}` shape is for. Two drafts got this wrong in opposite
                // directions: the first reported `ProcessID: 0` for a peer
                // whose pid was not knowable, and the second refused the whole
                // call and threw away a uid the kernel HAD reported. An entry
                // that is absent says "not known"; a zero says "pid zero".
                match self.credentials_for(&asked) {
                    Some((uid, pid)) => {
                        let pid = usable_pid(pid);
                        self.answer(message, "a{sv}", move |writer| {
                            writer.array("{sv}", |array| {
                                array.dict_entry(|entry| {
                                    entry.string("UnixUserID")?;
                                    entry.variant("u", |value| {
                                        value.uint32(uid);
                                        Ok(())
                                    })
                                })?;
                                if let Some(pid) = pid {
                                    array.dict_entry(|entry| {
                                        entry.string("ProcessID")?;
                                        entry.variant("u", |value| {
                                            value.uint32(pid);
                                            Ok(())
                                        })
                                    })?;
                                }
                                Ok(())
                            })
                        })
                    }
                    None => self.no_such_owner(message),
                }
            }
            _ => self.refuse(
                message,
                "org.freedesktop.DBus.Error.UnknownMethod",
                "td-busd serves Hello, the name and credential lookups and \
                 directed routing; the rest of org.freedesktop.DBus lands with \
                 match rules",
            ),
        }
    }

    /// The kernel's word on whoever owns `name`, or `None` for a name that is
    /// not here. The broker's own name is answered from this process, which is
    /// the truthful answer: `org.freedesktop.DBus` IS td-busd.
    fn credentials_for(&self, name: &str) -> Option<(u32, i32)> {
        if name == BUS_NAME {
            // The bus is this process. Reading `/proc/self` rather than calling
            // `getuid` keeps the roster at three syscalls, for the reason
            // `main`'s `current_uid` gives.
            let entry = fs::metadata("/proc/self").ok()?;
            let uid = std::os::unix::fs::MetadataExt::uid(&entry);
            let pid = fs::read_link("/proc/self").ok()?;
            let pid = pid.file_name()?.to_str()?.parse().ok()?;
            return Some((uid, pid));
        }
        self.bus.credentials(name)
    }

    /// The one refusal every name lookup shares.
    fn no_such_owner(&mut self, message: &message::Message<'_>) -> Result<(), Ended> {
        self.refuse(
            message,
            "org.freedesktop.DBus.Error.NameHasNoOwner",
            "no such connection on this bus",
        )
    }

    /// This connection's unique name.
    ///
    /// Every path that builds an outgoing message runs after `dispatch`'s
    /// `Hello` gate, so the name is always there. Saying so in the signature
    /// rather than writing `unwrap_or_default` is the difference between an
    /// impossible case that fails loudly and a silent fallback shaped like a
    /// default: an empty SENDER is not a legal bus name, so the "default"
    /// would have produced a message the codec then refused, one layer away
    /// from where the mistake was.
    fn named(&self) -> Result<&str, Ended> {
        self.unique
            .as_deref()
            .ok_or_else(|| Ended::Failed("a reply was built before Hello".into()))
    }

    /// Check a call's arguments against the signature the method takes, and
    /// answer `InvalidArgs` if they disagree.
    ///
    /// The SIGNATURE is the whole check, not the first argument's type. A
    /// draft read `args()[0]` and ignored everything else, so `GetNameOwner`
    /// with a spare argument, or `Ping` with a body, ran as though it had been
    /// called correctly. Comparing signatures rejects both, and rejects a
    /// wrongly typed argument as a side effect rather than as a special case.
    ///
    /// `Hello` is not checked here and cannot be: an error reply has to be
    /// ADDRESSED, and a connection that has not yet said `Hello` has no name
    /// to address it to. Its arguments are ignored, which is what every other
    /// implementation does with them.
    fn takes(
        &mut self,
        message: &message::Message<'_>,
        signature: &str,
    ) -> Result<bool, Ended> {
        if message.fields.signature.unwrap_or("") == signature {
            return Ok(true);
        }
        let wanted = if signature.is_empty() {
            "no arguments".to_string()
        } else {
            format!("arguments of signature '{signature}'")
        };
        self.refuse(
            message,
            "org.freedesktop.DBus.Error.InvalidArgs",
            &format!("this method takes {wanted}"),
        )?;
        Ok(false)
    }

    /// The one bus name a name lookup was called with, or an `InvalidArgs`
    /// reply for the caller.
    ///
    /// A missing, wrongly typed, or syntactically invalid argument is a
    /// MALFORMED CALL, not a broken connection: the specification has an error
    /// name for exactly this, and a draft disconnected instead — which its own
    /// comment said it did not do. It is also not a lookup of the empty
    /// string, which would be answered truthfully and uselessly with "nobody
    /// owns that".
    fn bus_name_argument(
        &mut self,
        message: &message::Message<'_>,
    ) -> Result<Option<String>, Ended> {
        if !self.takes(message, "s")? {
            return Ok(None);
        }
        let asked = message.args().first().and_then(crate::wire::Value::as_str);
        match asked {
            // Validated rather than looked up: a string that is not a bus name
            // cannot be owned by anyone, so answering "nobody owns it" is true
            // and unhelpful — it reads as a fact about the bus rather than
            // about the question.
            Some(text) if crate::name::valid_bus_name(text) => Ok(Some(text.to_string())),
            _ => {
                self.refuse(
                    message,
                    "org.freedesktop.DBus.Error.InvalidArgs",
                    "that is not a bus name",
                )?;
                Ok(None)
            }
        }
    }

    /// A method return to whoever sent `message`, with a body this bus writes.
    fn answer<F>(
        &mut self,
        message: &message::Message<'_>,
        signature: &str,
        fill: F,
    ) -> Result<(), Ended>
    where
        F: FnOnce(&mut Writer) -> Result<(), WireError>,
    {
        // Cloned so the builder borrows a local: `destination` holds a
        // reference for as long as the builder lives, and `take_serial` needs
        // `&mut self` in the middle of it.
        let mine = self.named()?.to_string();
        let serial = self.take_serial();
        let mut builder = message::Builder::method_return(message.endian, message.serial)
            .sender(BUS_NAME)
            .destination(&mine)
            .serial(serial);
        if !signature.is_empty() {
            builder = builder
                .body(signature, fill)
                .map_err(|error| Ended::Failed(format!("reply body: {error}")))?;
        }
        let reply = builder
            .encode()
            .map_err(|error| Ended::Failed(format!("reply: {error}")))?;
        self.queue_own(reply)
    }

    /// Deliver to another peer, with this connection's name stamped on it.
    fn route(
        &mut self,
        message: &message::Message<'_>,
        destination: &str,
        wants_reply: bool,
        descriptors: usize,
    ) -> Result<(), Ended> {
        // A message carrying descriptors is not forwarded yet, and saying so
        // is the point: delivering it WITHOUT them would hand the recipient a
        // body whose `h` values index nothing.
        if descriptors > 0 {
            return if wants_reply {
                self.refuse(
                    message,
                    "org.freedesktop.DBus.Error.NotSupported",
                    "descriptor passing between peers lands with the rest of rung 14",
                )
            } else {
                Ok(())
            };
        }
        let Some(outbox) = self.bus.route(destination) else {
            // §D's one consistent story: a name with no owner is absent, and
            // the caller is told so rather than left waiting.
            return if wants_reply {
                self.refuse(
                    message,
                    "org.freedesktop.DBus.Error.NameHasNoOwner",
                    "no such connection on this bus",
                )
            } else {
                Ok(())
            };
        };
        let sender = self.named()?.to_string();
        // Re-encoding can FAIL on a message the broker itself accepted, and
        // the sender must be told rather than torn down for it. The broker
        // adds a SENDER field, and the cap on an incoming header-fields array
        // is the same number as the cap on an outgoing one — object paths have
        // no length bound of their own — so a legal message whose fields are
        // within a couple of dozen bytes of the ceiling cannot be relayed. A
        // draft propagated that as `Ended::Failed`, which disconnected the
        // sender with NO reply at all and logged it as a BROKER fault. Peer
        // input must not produce a broker fault.
        // Rebuilt rather than relayed: the SENDER field is the broker's word
        // about who sent this, and §D refuses a client-supplied one. The BODY
        // is copied byte for byte — a broker has no business re-marshalling a
        // payload it does not read.
        let forwarded = match self.restamp(message, &sender) {
            Ok(forwarded) => forwarded,
            Err(why) => {
                eprintln!("td-busd: cannot relay from {sender}: {why}");
                return if wants_reply {
                    self.refuse(
                        message,
                        "org.freedesktop.DBus.Error.LimitsExceeded",
                        &format!("this message cannot be relayed: {why}"),
                    )
                } else {
                    Ok(())
                };
            }
        };
        match self.deliver(&outbox, forwarded) {
            Ok(()) => Ok(()),
            // The recipient's queue is full, and the SENDER is told. The
            // recipient is NOT disconnected: it did not choose when this
            // fired. See `registry::MAX_OUTGOING_BYTES` — two maximum messages
            // back to back exceed the ceiling by construction, so a draft that
            // disconnected here let any peer evict any other with two frames.
            // A peer that genuinely never reads is removed by the BUS ceiling's
            // remedy instead, which picks the largest consumer.
            Err(Overflow::Connection { bytes, frames }) => {
                if wants_reply {
                    self.refuse(
                        message,
                        "org.freedesktop.DBus.Error.LimitsExceeded",
                        &format!(
                            "the recipient is {bytes} bytes behind in {frames} frames"
                        ),
                    )
                } else {
                    Ok(())
                }
            }
            Err(Overflow::Bus(_)) => {
                if wants_reply {
                    self.refuse(
                        message,
                        "org.freedesktop.DBus.Error.LimitsExceeded",
                        "the bus is over its queue ceiling",
                    )
                } else {
                    Ok(())
                }
            }
            Err(Overflow::Closed) => {
                if wants_reply {
                    self.refuse(
                        message,
                        "org.freedesktop.DBus.Error.NameHasNoOwner",
                        "that connection has gone",
                    )
                } else {
                    Ok(())
                }
            }
        }
    }

    /// The same message with the broker's SENDER on it and nothing else
    /// changed.
    fn restamp(
        &self,
        message: &message::Message<'_>,
        sender: &str,
    ) -> Result<Vec<u8>, String> {
        // Every field taken here is one `message.rs` already refused the
        // message for lacking, so none of these can be absent. A draft wrote
        // `unwrap_or` at each of them, which reads as a default and is not
        // one: `path.unwrap_or(BUS_PATH)` would have relabelled a peer's call
        // as addressed to the BROKER's own object. An impossible case gets a
        // loud error, not a plausible substitute.
        let missing = || "a relayed message lost a mandatory field".to_string();
        let mut builder = match message.kind {
            message::MessageType::MethodCall => {
                let (Some(path), Some(member)) = (message.fields.path, message.fields.member)
                else {
                    return Err(missing());
                };
                message::Builder::method_call(message.endian, path, message.fields.interface, member)
            }
            message::MessageType::Signal => {
                let (Some(path), Some(interface), Some(member)) = (
                    message.fields.path,
                    message.fields.interface,
                    message.fields.member,
                ) else {
                    return Err(missing());
                };
                message::Builder::signal(message.endian, path, interface, member)
            }
            message::MessageType::MethodReturn => {
                let Some(reply_serial) = message.fields.reply_serial else {
                    return Err(missing());
                };
                message::Builder::method_return(message.endian, reply_serial)
            }
            message::MessageType::Error => {
                let (Some(name), Some(reply_serial)) =
                    (message.fields.error_name, message.fields.reply_serial)
                else {
                    return Err(missing());
                };
                message::Builder::error(message.endian, name, reply_serial)
            }
            message::MessageType::Unknown(_) => {
                // Unreachable: `dispatch` ignores an unknown type before it
                // looks at the destination. Named rather than silent, because a
                // silent fallback here would forward it as something else.
                return Err("an unknown message type reached the relay".into());
            }
        };
        builder = builder.sender(sender).serial(message.serial).flags(message.flags);
        if let Some(destination) = message.fields.destination {
            builder = builder.destination(destination);
        }
        let body = message.body_bytes().to_vec();
        if !body.is_empty() || message.fields.signature.is_some() {
            builder = builder
                .body_raw(message.fields.signature.unwrap_or(""), body)
                .map_err(|error| format!("relay body: {error}"))?;
        }
        builder.encode().map_err(|error| format!("relay: {error}"))
    }

    /// An error reply to the peer that sent this, queued like any other.
    fn refuse(
        &mut self,
        message: &message::Message<'_>,
        name: &str,
        why: &str,
    ) -> Result<(), Ended> {
        let mine = self.named()?.to_string();
        let serial = self.take_serial();
        let reply = message::Builder::error(message.endian, name, message.serial)
            .serial(serial)
            .sender(BUS_NAME)
            .destination(&mine)
            .body("s", |writer| writer.string(why))
            .map_err(|error| Ended::Failed(format!("refusal body: {error}")))?
            .encode()
            .map_err(|error| Ended::Failed(format!("refusal: {error}")))?;
        self.queue_own(reply)
    }

    /// Append a frame to an outbox, applying §D's remedy if the BUS is full.
    ///
    /// The remedy belongs wherever the condition is seen, not only on the
    /// routing path. A draft ran it only in `route` and turned a bus overflow
    /// into `Ended::Failed` everywhere else, which made the ceiling a weapon
    /// rather than a bound: four peers that stop reading fill the budget, and
    /// the next INNOCENT peer to call `GetId` — or the next connection to
    /// reach its `Hello` reply, or even its `OK` line — is the one
    /// disconnected, while the four sit there untouched.
    ///
    /// One retry. If the bus is still over its ceiling after its largest
    /// consumer has gone, the answer really is that the bus is full.
    fn deliver(&self, outbox: &Arc<Outbox>, frame: Vec<u8>) -> Result<(), Overflow> {
        let rejected = match outbox.push(frame) {
            Ok(()) => return Ok(()),
            Err(rejected) => rejected,
        };
        let Rejected {
            why: Overflow::Bus(bytes),
            frame,
        } = rejected
        else {
            return Err(rejected.why);
        };
        // §D: a broker-level condition, logged apart from an ordinary refusal
        // because reaching it means a policy elsewhere is wrong.
        let relieved = self.bus.relieve_largest();
        eprintln!(
            "td-busd: the bus is {bytes} bytes behind; disconnected {}",
            relieved.as_deref().unwrap_or("nobody")
        );
        outbox.push(frame).map_err(|again| again.why)
    }

    /// Queue a frame this broker generated for its own peer.
    fn queue_own(&mut self, frame: Vec<u8>) -> Result<(), Ended> {
        match self.deliver(&Arc::clone(&self.outbox), frame) {
            Ok(()) => Ok(()),
            Err(Overflow::Closed) => Err(Ended::PeerLeft),
            Err(Overflow::Connection { bytes, frames }) => Err(Ended::Refused(format!(
                "{bytes} bytes in {frames} frames queued and unread"
            ))),
            Err(Overflow::Bus(bytes)) => {
                Err(Ended::Failed(format!("the bus is {bytes} bytes behind")))
            }
        }
    }

    /// This connection's next outgoing serial. Unique per connection, as the
    /// specification requires, and never zero.
    fn take_serial(&mut self) -> u32 {
        let mine = self.next_serial;
        self.next_serial = self.next_serial.checked_add(1).unwrap_or(1);
        mine
    }

}

/// Is this message addressed to the broker's own object?
///
/// PATH is mandatory on a method call, so an absent one is a message the codec
/// has already refused; it is spelled out rather than defaulted for the reason
/// `restamp` gives.
fn on_the_bus_object(message: &message::Message<'_>) -> bool {
    message.fields.path == Some(BUS_PATH)
}

/// A pid this broker is willing to report, or `None` for one it cannot.
///
/// `SO_PEERCRED` answers 0 for a peer whose pid does not exist in the reader's
/// namespace, and 0 is not a process. Reporting it would hand a caller a
/// number that looks like an answer and names nothing, and `/proc/0` is a
/// lookup that fails much later and somewhere else. `None` means "not
/// knowable" and NOT "no such connection" — the two are different answers and
/// a draft gave the second for the first.
fn usable_pid(pid: i32) -> Option<u32> {
    if pid <= 0 {
        return None;
    }
    u32::try_from(pid).ok()
}

/// Write a whole frame, attaching descriptors to the FIRST write only: a
/// partial `sendmsg` that re-attached them would deliver each one twice.
///
/// Free rather than a method on `Connection` because the writer thread outlives
/// every borrow of one: the reader owns the `Connection`, the writer owns only
/// an `Arc<Outbox>` and the socket it hands over.
fn write_frame(stream: &UnixStream, bytes: &[u8], fds: &[RawFd]) -> Result<(), String> {
    if bytes.is_empty() {
        // The loop below is `while sent < bytes.len()`, so an empty frame
        // returns `Ok(())` without one `sendmsg` — and any descriptors handed
        // with it are dropped having gone nowhere. No caller does this today;
        // the guard is here so that none can start.
        return Err(format!(
            "refusing to write an empty frame carrying {} descriptors",
            fds.len()
        ));
    }
    let mut sent = 0usize;
    let mut attach = fds;
    while sent < bytes.len() {
        let rest = bytes.get(sent..).unwrap_or(&[]);
        match sys::send(stream, rest, attach) {
            Ok(0) => return Err("write made no progress".into()),
            Ok(count) => {
                sent = sent.saturating_add(count);
                attach = &[];
            }
            // No `Interrupted` arm: `sys::send` retries `EINTR` itself, so
            // it never surfaces one. A draft added it here by symmetry with
            // the READ path, where it is live — a dead branch shaped like a
            // safety check, which `dispatch`'s own comment two hundred lines
            // above warns against, and which the red-check could not catch
            // because reverting it changes nothing.
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {
                return Err("peer closed while being written to".into())
            }
            Err(error) => return Err(format!("write: {error}")),
        }
    }
    Ok(())
}

/// Connect to a running bus and complete the client half of `EXTERNAL`. This is
/// what `td-busd probe` does, and what §A's `ready=` line calls: a bus that has
/// bound its socket but cannot authenticate is not ready, and a readiness check
/// that only connected would not know the difference.
pub fn probe(path: &Path, uid: u32) -> Result<String, String> {
    probe_within(path, uid, PROBE_TIMEOUT)
}

/// `UnixStream::connect` with a deadline, which `std` does not offer for a
/// unix socket the way it does for TCP.
///
/// The connect runs on a thread of its own and the caller waits on a channel.
/// A thread blocked in `connect` is NOT joined and cannot be: the call it is
/// in has no timeout, which is the whole reason this exists. It is left
/// running, and that is sound only because of what this function is for —
/// `probe` is one short-lived process whose next act is to exit, so the thread
/// is reclaimed by process teardown a moment later. Do not lift this into a
/// long-lived process without giving the thread a way to be cancelled.
///
/// The `send` on the far side is deliberately ignored: after a timeout nobody
/// is receiving, and a connect that succeeds late has its stream dropped by
/// the channel, which closes it.
fn connect_within(path: &Path, timeout: std::time::Duration) -> Result<UnixStream, String> {
    let owned = path.to_path_buf();
    connect_by(path, timeout, move || {
        UnixStream::connect(&owned)
            .map_err(|error| format!("connect {}: {error}", owned.display()))
    })
}

/// `connect_within` with the connect itself as an argument.
///
/// The seam is here for the same reason `probe_within` takes its wait as an
/// argument: the branch worth testing is the one where the connect NEVER
/// returns, and provoking that through a real socket means filling a listen
/// backlog whose depth is a property of the std version and the host. A test
/// that needs several thousand threads to be reliable is not a test of this
/// logic. So the timeout is proven against a connect that is defined not to
/// return, and what stays outside the test is the kernel fact that a real one
/// can behave that way — `unix_stream_connect` waits on the connecting
/// socket's `SO_SNDTIMEO`, which is unset until after `connect` returns.
fn connect_by<F>(
    path: &Path,
    timeout: std::time::Duration,
    connect: F,
) -> Result<UnixStream, String>
where
    F: FnOnce() -> Result<UnixStream, String> + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("td-busd-probe-connect".into())
        .spawn(move || {
            let _ = tx.send(connect());
        })
        .map_err(|error| format!("cannot start the connect thread: {error}"))?;
    match rx.recv_timeout(timeout) {
        Ok(outcome) => outcome,
        // The listener exists — otherwise `connect` would have refused at once
        // — and is not accepting. Said as its own sentence because it is a
        // different fault from a bus that accepts and then says nothing, and
        // the console line is all the operator gets.
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(format!(
            "connect {}: the bus did not accept in time",
            path.display()
        )),
        // The thread died without sending, which it has no path to do.
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(format!(
            "connect {}: the connect thread ended without an answer",
            path.display()
        )),
    }
}

/// `probe` with the wait as an argument, so a test can prove the timeout
/// exists without spending `PROBE_TIMEOUT` to do it.
fn probe_within(
    path: &Path,
    uid: u32,
    timeout: std::time::Duration,
) -> Result<String, String> {
    use std::io::{Read, Write};

    // ONE deadline for the whole probe, and it starts HERE rather than after
    // the connect. `connect(2)` on a unix socket whose accept queue is full
    // blocks against the SENDER's `SO_SNDTIMEO`, which is unset until the
    // socket exists — so a timeout installed after the call cannot bound the
    // call. A listener that binds and never accepts therefore held this for
    // ever, and once `/etc/bootsuccess` calls the probe with no wrapper of its
    // own, "for ever" is the health target and then the boot: the host kills
    // the VM on its own ceiling and reports a bare timeout with no guest-side
    // reason in it, which is the one failure the whole timeout chain exists to
    // prevent. td-svc's `ready=` survived it by group-killing each attempt;
    // nothing else did.
    let deadline = std::time::Instant::now() + timeout;
    let stream = connect_within(path, timeout)?;
    // Safe `std` on a descriptor this process owns, so nothing joins the
    // roster for it.
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|error| format!("set read timeout: {error}"))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|error| format!("set write timeout: {error}"))?;
    let mut stream = stream;
    let hex: String = uid
        .to_string()
        .bytes()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    stream
        .write_all(format!("\0AUTH EXTERNAL {hex}\r\n").as_bytes())
        .map_err(|error| format!("write AUTH: {error}"))?;

    // The same deadline the connect was held to, so the answer gets what the
    // connect did not spend rather than a fresh allowance. A socket timeout is
    // an INACTIVITY timeout: a wedged or squatting listener that dribbles a
    // byte just inside it holds the probe for `MAX_LINE` times the timeout —
    // hours, on a check `ready=` is waiting for. The per-read timeout is
    // narrowed to what is left of the deadline as it goes.
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while !line.ends_with(b"\r\n") {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return Err("read: the bus did not answer in time".into());
        }
        stream
            .set_read_timeout(Some(left))
            .map_err(|error| format!("set read timeout: {error}"))?;
        match stream.read(&mut byte) {
            Ok(0) => return Err("the bus closed the connection during AUTH".into()),
            Ok(_) => line.push(byte[0]),
            // A signal is not an answer and not a failure. Every other read on
            // this surface retries through `sys::receive`; this one is plain
            // `std`, so it must say so itself.
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(format!("read: {error}")),
        }
        if line.len() > crate::auth::MAX_LINE {
            return Err("the bus answered with an overlong line".into());
        }
    }
    let text = String::from_utf8_lossy(&line).trim_end().to_string();
    let guid = text
        .strip_prefix("OK ")
        .ok_or_else(|| format!("the bus refused this uid: {text}"))?;
    if guid.len() != GUID_LEN || !guid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!("the bus answered OK with a bad guid: {guid}"));
    }
    stream
        .write_all(b"BEGIN\r\n")
        .map_err(|error| format!("write BEGIN: {error}"))?;
    Ok(format!("bus at {} answered OK {guid}", path.display()))
}

/// Bind a socket, serve one peer on it, and probe it — end to end, through
/// all three rostered syscalls, on the machine that will run the result.
///
/// This is what puts the transport inside the recipe's realized-output check.
/// `td-busd selftest` is the step the recipe runs, and until this existed that
/// step exercised the wire format alone: the socket, `SO_PEERCRED` and the
/// handshake were covered by host-side tests and by nothing the target build
/// ever ran. A codec that round-trips on a machine whose broker cannot bind is
/// not the thing the recipe means to be asserting.
pub fn loopback(uid: u32) -> Result<String, String> {
    // Unique per CALL, not per process. Keyed on the pid alone, two concurrent
    // selftests share a path: the second `bind` finds the first still
    // listening, refuses, and its probe never connects — leaving the accept
    // below waiting for a peer that will never come. Which is how this was
    // found: two tests calling `selftest` deadlocked the suite while passing
    // one at a time.
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let nth = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "td-busd-selftest-{}-{nth}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).map_err(|error| format!("cannot make {}: {error}", dir.display()))?;
    let path = dir.join("bus");
    let outcome = loopback_at(&path, uid);
    // Best effort: a selftest that passed and then failed to tidy up has still
    // answered the question it was asked.
    fs::remove_dir_all(&dir).ok();
    outcome
}

fn loopback_at(path: &Path, uid: u32) -> Result<String, String> {
    let bound = bind(path).map_err(|error| format!("cannot listen on {}: {error}", path.display()))?;
    let text = guid_text().map_err(|error| format!("cannot make a guid: {error}"))?;
    let guid = Guid::new(&text).map_err(|error| format!("bad guid: {error:?}"))?;

    // A blocking `accept` inside a `scope` is a deadlock waiting for a reason:
    // `scope` will not return until the thread does, and the thread returns
    // only when a peer arrives. Any failure that stops the probe reaching the
    // listener therefore hangs the selftest — and a hang is the one failure
    // that reports nothing. A deadline removes the whole class rather than the
    // one instance that was found.
    bound
        .listener()
        .set_nonblocking(true)
        .map_err(|error| format!("cannot poll the listener: {error}"))?;
    let quota = Quota::new();
    std::thread::scope(|scope| {
        // `Builder::spawn_scoped`, not `scope.spawn`: the latter PANICS when
        // the OS refuses a thread, and this binary is built `panic=abort`, so
        // it would abort `td-busd selftest` — the recipe's realized-output
        // check. `run` was changed for exactly this reason and this site was
        // missed, which is what a second review cycle is for.
        let listening = std::thread::Builder::new().spawn_scoped(scope, || {
            let deadline = std::time::Instant::now() + LOOPBACK_TIMEOUT;
            loop {
                match bound.listener().accept() {
                    // Accepted sockets do not inherit the listener's
                    // non-blocking flag on Linux, so the connection below is
                    // an ordinary blocking one.
                    Ok((stream, _)) => {
                        let bus = Bus::new();
                        let accepted = Connection::accept(stream, guid, &quota, &bus);
                        if let Ok(mut connection) = accepted {
                            let _ = connection.serve();
                        }
                        break;
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        if std::time::Instant::now() >= deadline {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });
        if let Err(error) = listening {
            return Err(format!("cannot spawn the selftest listener: {error}"));
        }
        probe(path, uid)
    })?;
    Ok(format!(
        "td-busd: a bus bound at {}, authenticated a peer by its kernel credential, and answered the handshake with its guid",
        path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::sync::mpsc;
    use std::thread;

    const GUID: &str = "00112233445566778899aabbccddeeff";

    fn scratch(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "td-busd-{}-{}-{name}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        path
    }

    fn this_uid() -> u32 {
        let entry = fs::metadata("/proc/self").expect("procfs");
        std::os::unix::fs::MetadataExt::uid(&entry)
    }

    /// What a served connection reports when it is over.
    #[derive(Debug)]
    struct Outcome {
        ended: Ended,
        credential: PeerCredential,
        /// What the connection was CHARGED to, which is the credential's uid
        /// whatever the peer claimed.
        uid: Option<u32>,
    }

    /// Wait for a connection to end, but not forever. A bare `recv()` here
    /// turns "the bus failed to end this connection" — which is what a
    /// regression in any of the refusals below looks like — into a test run
    /// that hangs instead of one that fails, and a hang reports nothing.
    fn ended(hear: &mpsc::Receiver<Outcome>) -> Outcome {
        match hear.recv_timeout(std::time::Duration::from_secs(20)) {
            Ok(outcome) => outcome,
            Err(_) => panic!("the connection never ended"),
        }
    }

    /// A served connection, and the client end of it.
    ///
    /// The client carries a read timeout for the reason `ended` above carries
    /// one: a regression that stops the bus answering should fail this suite,
    /// not stop it. Two mutations in the red-check hung here before it did.
    fn serving() -> (UnixStream, mpsc::Receiver<Outcome>) {
        let (client, server) = UnixStream::pair().expect("socketpair");
        client
            .set_read_timeout(Some(std::time::Duration::from_secs(20)))
            .expect("client read timeout");
        let (tell, hear) = mpsc::channel();
        thread::spawn(move || {
            let guid = Guid::new(GUID).expect("guid");
            let quota = Quota::new();
            let bus = Bus::new();
            let mut connection = Connection::accept(server, guid, &quota, &bus).expect("accept");
            let ended = connection.serve();
            let _ = tell.send(Outcome {
                ended,
                credential: connection.credential(),
                uid: connection.authenticated_uid(),
            });
        });
        (client, hear)
    }

    /// A bus with several served connections on it, and the client end of
    /// each.
    ///
    /// ONE `Bus` behind all of them, which is the whole point: routing is a
    /// property of the directory, and a harness that gave each connection its
    /// own directory would prove only that a message goes nowhere.
    fn bus_of(peers: usize) -> (Arc<Bus>, Vec<UnixStream>) {
        let mut clients = Vec::new();
        let mut servers = Vec::new();
        for _ in 0..peers {
            let (client, server) = UnixStream::pair().expect("socketpair");
            client
                .set_read_timeout(Some(std::time::Duration::from_secs(20)))
                .expect("client read timeout");
            clients.push(client);
            servers.push(server);
        }
        // Handed back so a test can reach the directory the connections share
        // — filling the bus's budget, say, which no client can do cheaply.
        let bus = Arc::new(Bus::new());
        let serving = Arc::clone(&bus);
        thread::spawn(move || {
            let quota = Quota::new();
            let bus = serving;
            thread::scope(|scope| {
                for server in servers {
                    let quota = &quota;
                    let bus = &*bus;
                    let spawned = thread::Builder::new().spawn_scoped(scope, move || {
                        let guid = Guid::new(GUID).expect("guid");
                        if let Ok(mut connection) = Connection::accept(server, guid, quota, bus) {
                            let _ = connection.serve();
                        }
                    });
                    spawned.expect("spawn a connection thread");
                }
            });
        });
        (bus, clients)
    }

    /// One client's side of a connection: writes what a peer writes, and hands
    /// back whole frames.
    struct Peer {
        stream: UnixStream,
        held: Vec<u8>,
        /// How many handshake lines are still to be stepped over. One for the
        /// `OK`, two when `AGREE_UNIX_FD` follows it.
        lines: usize,
    }

    impl Peer {
        /// Authenticate, BEGIN and say `Hello`, all in one write, and return
        /// the unique name the bus handed out.
        fn arrive(stream: UnixStream) -> (Self, String) {
            Self::arriving(stream, false)
        }

        /// The same, having negotiated descriptor passing.
        fn arrive_with_fds(stream: UnixStream) -> (Self, String) {
            Self::arriving(stream, true)
        }

        fn arriving(stream: UnixStream, descriptors: bool) -> (Self, String) {
            let mut peer = Peer {
                stream,
                held: Vec::new(),
                lines: if descriptors { 2 } else { 1 },
            };
            let negotiate = if descriptors { "NEGOTIATE_UNIX_FD\r\n" } else { "" };
            let mut opening =
                format!("\0AUTH EXTERNAL {}\r\n{negotiate}BEGIN\r\n", uid_hex()).into_bytes();
            opening.extend_from_slice(&bus_call("Hello", 1));
            peer.send(&opening);
            let frame = peer.frame();
            let (reply, _) = message::decode(&frame, 0).expect("decode Hello's reply");
            assert_eq!(
                reply.kind,
                message::MessageType::MethodReturn,
                "Hello was not answered with a name"
            );
            let name = reply
                .args()
                .first()
                .and_then(crate::wire::Value::as_str)
                .expect("Hello answers with a name")
                .to_string();
            (peer, name)
        }

        fn send(&mut self, bytes: &[u8]) {
            self.stream.write_all(bytes).expect("write");
        }

        /// Send a frame with a descriptor attached, the way a client that
        /// negotiated `UNIX_FD` does.
        fn send_with_fd(&mut self, frame: &[u8], fd: RawFd) {
            let sent = sys::send(&self.stream, frame, &[fd]).expect("sendmsg");
            assert_eq!(sent, frame.len(), "the test wrote only part of a frame");
        }

        /// The next whole frame, reading until there is one. The handshake's
        /// lines are stepped over on the way past.
        fn frame(&mut self) -> Vec<u8> {
            let mut chunk = [0u8; 1024];
            loop {
                while self.lines > 0 {
                    match self.held.windows(2).position(|pair| pair == b"\r\n") {
                        Some(at) => {
                            self.held.drain(..at + 2);
                            self.lines -= 1;
                        }
                        None => break,
                    }
                }
                if self.lines == 0 {
                    if let Ok(Some(length)) = message::frame_len(&self.held) {
                        if self.held.len() >= length {
                            return self.held.drain(..length).collect();
                        }
                    }
                }
                let read = self.stream.read(&mut chunk).expect("read");
                assert_ne!(read, 0, "the bus closed without answering");
                self.held.extend_from_slice(&chunk[..read]);
            }
        }

        /// Nothing more arrives. The wait is short because the assertion is
        /// that nothing comes, and a long one would only make the suite slow
        /// at proving it.
        fn expect_silence(&mut self) {
            assert!(self.held.is_empty(), "a frame was already waiting");
            self.stream
                .set_read_timeout(Some(std::time::Duration::from_millis(200)))
                .expect("read timeout");
            let mut chunk = [0u8; 64];
            match self.stream.read(&mut chunk) {
                Ok(0) => {}
                Ok(read) => panic!("{read} unexpected bytes: {:?}", &chunk[..read]),
                Err(error) => assert!(
                    matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ),
                    "read failed for the wrong reason: {error}"
                ),
            }
        }

        /// The connection is gone: the far end has hung up.
        fn expect_disconnect(&mut self) {
            self.stream
                .set_read_timeout(Some(std::time::Duration::from_secs(20)))
                .expect("read timeout");
            let mut chunk = [0u8; 256];
            loop {
                match self.stream.read(&mut chunk) {
                    Ok(0) => return,
                    // Bytes already in flight when the bus decided are not a
                    // failure — the disconnect is what is being asserted.
                    Ok(_) => {}
                    Err(error) if error.kind() == io::ErrorKind::ConnectionReset => return,
                    Err(error) => panic!("read failed: {error}"),
                }
            }
        }
    }

    /// A method call addressed to the broker itself.
    fn bus_call(member: &str, serial: u32) -> Vec<u8> {
        message::Builder::method_call(crate::wire::Endian::Little, BUS_PATH, Some(BUS_NAME), member)
            .destination(BUS_NAME)
            .serial(serial)
            .encode()
            .expect("encode a bus call")
    }

    /// A method call addressed to another peer, carrying a string so the test
    /// can tell a forwarded body from a rebuilt one.
    fn peer_call(destination: &str, serial: u32, text: &str) -> Vec<u8> {
        message::Builder::method_call(
            crate::wire::Endian::Little,
            "/org/example/Thing",
            Some("org.example.Thing"),
            "Do",
        )
        .destination(destination)
        .serial(serial)
        .body("s", |writer| writer.string(text))
        .expect("body")
        .encode()
        .expect("encode a peer call")
    }

    /// A directed message reaches the connection its DESTINATION names, and
    /// arrives with the broker's word about who sent it.
    #[test]
    fn a_message_reaches_the_peer_it_names() {
        let (_bus, mut clients) = bus_of(2);
        let second = clients.pop().expect("two clients");
        let first = clients.pop().expect("two clients");
        let (mut one, name_one) = Peer::arrive(first);
        let (mut two, name_two) = Peer::arrive(second);
        assert_ne!(name_one, name_two, "two connections shared a name");

        one.send(&peer_call(&name_two, 7, "over here"));
        let frame = two.frame();
        let (got, _) = message::decode(&frame, 0).expect("decode what arrived");
        assert_eq!(got.kind, message::MessageType::MethodCall);
        assert_eq!(got.fields.member, Some("Do"));
        assert_eq!(got.fields.path, Some("/org/example/Thing"));
        assert_eq!(got.fields.interface, Some("org.example.Thing"));
        assert_eq!(got.fields.destination, Some(name_two.as_str()));
        // The serial is the SENDER's, unchanged: it is what the sender will
        // match a reply against.
        assert_eq!(got.serial, 7);
        assert_eq!(
            got.fields.sender,
            Some(name_one.as_str()),
            "the broker did not stamp the sender"
        );
        assert_eq!(
            got.args().first().and_then(crate::wire::Value::as_str),
            Some("over here"),
            "the body did not survive the relay"
        );
        // The sender is told nothing: a delivered call is answered by the
        // peer that received it, not by the broker.
        one.expect_silence();
    }

    /// A client cannot forge who a message came from. §D refuses a
    /// client-supplied SENDER outright rather than overwriting it, because a
    /// client that sets one is either broken or lying and neither should be
    /// quietly corrected.
    #[test]
    fn a_client_may_not_supply_its_own_sender() {
        let (_bus, mut clients) = bus_of(2);
        let second = clients.pop().expect("two clients");
        let first = clients.pop().expect("two clients");
        let (mut one, _) = Peer::arrive(first);
        let (mut two, name_two) = Peer::arrive(second);

        let forged = message::Builder::method_call(
            crate::wire::Endian::Little,
            "/org/example/Thing",
            Some("org.example.Thing"),
            "Do",
        )
        .destination(&name_two)
        .sender(":1.99")
        .serial(7)
        .encode()
        .expect("encode");
        one.send(&forged);
        one.expect_disconnect();
        // And nothing reached the peer it was addressed to.
        two.expect_silence();
    }

    /// A call to a name nobody owns is REFUSED rather than dropped. A caller
    /// left waiting on a serial hangs; a caller told `NameHasNoOwner` fails.
    #[test]
    fn a_call_to_a_name_nobody_owns_is_refused() {
        let (_bus, mut clients) = bus_of(1);
        let only = clients.pop().expect("one client");
        let (mut peer, _) = Peer::arrive(only);

        peer.send(&peer_call(":1.404", 5, "anyone there"));
        let frame = peer.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode the refusal");
        assert_eq!(reply.kind, message::MessageType::Error);
        assert_eq!(
            reply.fields.error_name,
            Some("org.freedesktop.DBus.Error.NameHasNoOwner")
        );
        assert_eq!(reply.fields.reply_serial, Some(5));
        assert_eq!(reply.fields.sender, Some(BUS_NAME));
    }

    /// The same call with NO_REPLY_EXPECTED gets no error either: a sender
    /// that is not waiting must not be sent a message it has no serial for.
    #[test]
    fn an_undeliverable_message_that_wants_no_reply_is_silently_dropped() {
        let (_bus, mut clients) = bus_of(1);
        let only = clients.pop().expect("one client");
        let (mut peer, _) = Peer::arrive(only);

        let quiet = message::Builder::method_call(
            crate::wire::Endian::Little,
            "/org/example/Thing",
            Some("org.example.Thing"),
            "Do",
        )
        .destination(":1.404")
        .flags(message::FLAG_NO_REPLY_EXPECTED)
        .serial(5)
        .encode()
        .expect("encode");
        peer.send(&quiet);
        peer.expect_silence();
    }

    /// Anything before `Hello` ends the connection. A peer with no unique name
    /// has no SENDER to be stamped with, so routing it would mean delivering a
    /// message whose origin the recipient cannot check.
    #[test]
    fn nothing_may_be_sent_before_hello() {
        let (mut client, hear) = serving();
        let mut opening =
            format!("\0AUTH EXTERNAL {}\r\nBEGIN\r\n", uid_hex()).into_bytes();
        opening.extend_from_slice(&bus_call("ListNames", 1));
        client.write_all(&opening).expect("write");
        match ended(&hear).ended {
            Ended::Refused(why) => assert!(why.contains("before Hello"), "{why}"),
            other => panic!("a call before Hello gave {other:?}"),
        }
    }

    /// A SECOND `Hello` ends it too. A peer that asks twice has lost track of
    /// its own identity, which is not a state to keep serving.
    #[test]
    fn a_second_hello_ends_the_connection() {
        let (client, hear) = serving();
        let (mut peer, _) = Peer::arrive(client);
        peer.send(&bus_call("Hello", 2));
        match ended(&hear).ended {
            Ended::Refused(why) => assert!(why.contains("Hello twice"), "{why}"),
            other => panic!("a second Hello gave {other:?}"),
        }
    }

    /// The bus answers for itself: who is here, who owns a name, and what this
    /// bus is called. `busctl list` is these three and little else.
    #[test]
    fn the_bus_answers_for_its_own_names() {
        let (_bus, mut clients) = bus_of(2);
        let second = clients.pop().expect("two clients");
        let first = clients.pop().expect("two clients");
        let (mut one, name_one) = Peer::arrive(first);
        let (_two, name_two) = Peer::arrive(second);

        one.send(&bus_call("ListNames", 2));
        let frame = one.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode ListNames");
        assert_eq!(reply.kind, message::MessageType::MethodReturn);
        let listed: Vec<String> = reply
            .args()
            .first()
            .and_then(crate::wire::Value::as_seq)
            .expect("an array came back")
            .values(64)
            .expect("read the array")
            .iter()
            .filter_map(crate::wire::Value::as_str)
            .map(str::to_string)
            .collect();
        assert!(listed.contains(&BUS_NAME.to_string()), "{listed:?}");
        assert!(listed.contains(&name_one), "{listed:?}");
        assert!(listed.contains(&name_two), "{listed:?}");

        // A name that is here.
        one.send(&name_query("GetNameOwner", &name_two, 3));
        let frame = one.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode GetNameOwner");
        assert_eq!(reply.kind, message::MessageType::MethodReturn);
        assert_eq!(
            reply.args().first().and_then(crate::wire::Value::as_str),
            Some(name_two.as_str())
        );

        // And one that is not.
        one.send(&name_query("GetNameOwner", ":1.404", 4));
        let frame = one.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode the refusal");
        assert_eq!(reply.kind, message::MessageType::Error);
        assert_eq!(
            reply.fields.error_name,
            Some("org.freedesktop.DBus.Error.NameHasNoOwner")
        );

        // `GetId` answers with the GUID the handshake agreed, not a fresh one.
        one.send(&bus_call("GetId", 5));
        let frame = one.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode GetId");
        assert_eq!(
            reply.args().first().and_then(crate::wire::Value::as_str),
            Some(GUID),
            "GetId answered with a different GUID than the handshake"
        );
    }

    /// The broker answers who is behind a name from the KERNEL, not from
    /// anything the caller said. td's confinement story rests on this: an
    /// application asking who called it is asking `SO_PEERCRED`, taken once at
    /// accept, and a peer cannot describe itself into a different answer.
    #[test]
    fn the_bus_reports_the_kernels_word_on_who_owns_a_name() {
        let (_bus, mut clients) = bus_of(2);
        let second = clients.pop().expect("two clients");
        let first = clients.pop().expect("two clients");
        let (mut one, _) = Peer::arrive(first);
        let (_two, name_two) = Peer::arrive(second);

        one.send(&name_query("GetConnectionUnixUser", &name_two, 2));
        let frame = one.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode the uid");
        assert_eq!(reply.kind, message::MessageType::MethodReturn);
        assert_eq!(
            reply.args().first().and_then(crate::wire::Value::as_u32),
            Some(this_uid())
        );

        // Both peers are this test process, so the pid is known exactly.
        one.send(&name_query("GetConnectionUnixProcessID", &name_two, 3));
        let frame = one.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode the pid");
        assert_eq!(
            reply.args().first().and_then(crate::wire::Value::as_u32),
            Some(std::process::id())
        );

        // The dictionary form carries the same two numbers.
        one.send(&name_query("GetConnectionCredentials", &name_two, 4));
        let frame = one.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode the credentials");
        assert_eq!(reply.fields.signature, Some("a{sv}"));
        let mut found = Vec::new();
        let entries = reply
            .args()
            .first()
            .and_then(crate::wire::Value::as_seq)
            .expect("a dictionary came back")
            .values(16)
            .expect("read the dictionary");
        for entry in &entries {
            let pair = entry
                .as_seq()
                .expect("a dict entry")
                .values(2)
                .expect("read the entry");
            let key = pair.first().and_then(crate::wire::Value::as_str);
            let value = pair
                .get(1)
                .and_then(crate::wire::Value::as_seq)
                .and_then(|variant| variant.values(1).ok())
                .and_then(|held| held.first().and_then(crate::wire::Value::as_u32));
            if let (Some(key), Some(value)) = (key, value) {
                found.push((key.to_string(), value));
            }
        }
        assert!(
            found.contains(&("UnixUserID".to_string(), this_uid())),
            "{found:?}"
        );
        assert!(
            found.contains(&("ProcessID".to_string(), std::process::id())),
            "{found:?}"
        );

        // A name nobody owns has no credentials, and says so rather than
        // answering with a zero that reads like root and pid 0. All THREE
        // lookups: a draft covered only the first, and the red-check found
        // that the other two could each be made to answer zero with every
        // test still green.
        for (which, member) in [
            "GetConnectionUnixUser",
            "GetConnectionUnixProcessID",
            "GetConnectionCredentials",
        ]
        .into_iter()
        .enumerate()
        {
            let serial = 5u32.saturating_add(u32::try_from(which).unwrap_or(0));
            one.send(&name_query(member, ":1.404", serial));
            let frame = one.frame();
            let (reply, _) = message::decode(&frame, 0).expect("decode the refusal");
            assert_eq!(reply.kind, message::MessageType::Error, "{member} answered");
            assert_eq!(
                reply.fields.error_name,
                Some("org.freedesktop.DBus.Error.NameHasNoOwner"),
                "{member} refused for the wrong reason"
            );
            assert_eq!(reply.fields.reply_serial, Some(serial));
        }
    }

    /// A message with no DESTINATION splits by type. A signal is a broadcast,
    /// which needs the match rules that have not landed, and is accepted and
    /// goes nowhere — it has no reply to be missing. A method CALL is the
    /// opposite: its sender is waiting on a serial no route can currently
    /// answer, so it is told rather than left to time out.
    #[test]
    fn an_undirected_message_is_dropped_or_refused_by_type() {
        let (_bus, mut clients) = bus_of(1);
        let only = clients.pop().expect("one client");
        let (mut peer, _) = Peer::arrive(only);

        let broadcast = message::Builder::signal(
            crate::wire::Endian::Little,
            "/org/example/Thing",
            "org.example.Thing",
            "Happened",
        )
        .serial(2)
        .encode()
        .expect("encode the signal");
        peer.send(&broadcast);
        peer.expect_silence();

        let undirected = message::Builder::method_call(
            crate::wire::Endian::Little,
            "/org/example/Thing",
            Some("org.example.Thing"),
            "Do",
        )
        .serial(3)
        .encode()
        .expect("encode the call");
        peer.send(&undirected);
        let frame = peer.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode the refusal");
        assert_eq!(reply.kind, message::MessageType::Error);
        assert_eq!(
            reply.fields.error_name,
            Some("org.freedesktop.DBus.Error.NotSupported")
        );
        assert_eq!(reply.fields.reply_serial, Some(3));
    }

    /// A bus method this version does not have is `UnknownMethod`, not
    /// silence. `RequestName` and `AddMatch` land with match rules; a client
    /// told its match rule was installed and then never signalled is worse off
    /// than one told there is no such method.
    #[test]
    fn a_bus_method_this_version_lacks_is_an_error_rather_than_silence() {
        let (_bus, mut clients) = bus_of(1);
        let only = clients.pop().expect("one client");
        let (mut peer, _) = Peer::arrive(only);

        peer.send(&bus_call("AddMatch", 2));
        let frame = peer.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode the refusal");
        assert_eq!(reply.kind, message::MessageType::Error);
        assert_eq!(
            reply.fields.error_name,
            Some("org.freedesktop.DBus.Error.UnknownMethod")
        );
        assert_eq!(reply.fields.reply_serial, Some(2));
    }

    /// A message carrying descriptors to a PEER is refused, not stripped.
    /// Delivering it without them would hand the recipient a body whose `h`
    /// values index nothing — a corruption the recipient cannot detect,
    /// because the message it receives is well formed and simply wrong.
    #[test]
    fn a_message_carrying_descriptors_to_a_peer_is_refused_rather_than_stripped() {
        let (_bus, mut clients) = bus_of(2);
        let second = clients.pop().expect("two clients");
        let first = clients.pop().expect("two clients");
        let (mut one, _) = Peer::arrive_with_fds(first);
        let (mut two, name_two) = Peer::arrive(second);

        let spare = fs::File::open("/dev/null").expect("/dev/null");
        let carrying = message::Builder::method_call(
            crate::wire::Endian::Little,
            "/org/example/Thing",
            Some("org.example.Thing"),
            "Take",
        )
        .destination(&name_two)
        .serial(7)
        .unix_fds(1)
        .body("h", |writer| {
            writer.unix_fd(0);
            Ok(())
        })
        .expect("body")
        .encode()
        .expect("encode");
        one.send_with_fd(&carrying, spare.as_raw_fd());

        let frame = one.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode the refusal");
        assert_eq!(reply.kind, message::MessageType::Error);
        assert_eq!(
            reply.fields.error_name,
            Some("org.freedesktop.DBus.Error.NotSupported")
        );
        assert_eq!(reply.fields.reply_serial, Some(7));
        // And nothing at all reached the peer it was addressed to.
        two.expect_silence();
    }

    /// A name leaves the bus with its connection. A unique name that outlived
    /// the socket behind it would send a message to nobody, and the directory
    /// would grow for the life of the broker.
    #[test]
    fn a_name_leaves_the_bus_with_its_connection() {
        let (_bus, mut clients) = bus_of(2);
        let second = clients.pop().expect("two clients");
        let first = clients.pop().expect("two clients");
        let (mut one, _) = Peer::arrive(first);
        let (two, name_two) = Peer::arrive(second);

        // While it is here, it is findable.
        one.send(&name_query("GetNameOwner", &name_two, 2));
        let frame = one.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode");
        assert_eq!(reply.kind, message::MessageType::MethodReturn);

        drop(two);

        // The bus notices asynchronously: the reading thread has to come back
        // from `recvmsg` before it can leave the directory. So this polls to a
        // DEADLINE rather than sleeping a guessed interval — a fixed sleep is
        // either flaky or slow, and usually both.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let mut serial = 3u32;
        loop {
            one.send(&name_query("GetNameOwner", &name_two, serial));
            let frame = one.frame();
            let (reply, _) = message::decode(&frame, 0).expect("decode");
            if reply.kind == message::MessageType::Error {
                assert_eq!(
                    reply.fields.error_name,
                    Some("org.freedesktop.DBus.Error.NameHasNoOwner")
                );
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "{name_two} is still on the bus after its connection ended"
            );
            serial = serial.saturating_add(1);
        }

        // And a message to it is refused rather than queued for a socket
        // nobody is reading.
        one.send(&peer_call(&name_two, 99, "anyone"));
        let frame = one.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode");
        assert_eq!(reply.kind, message::MessageType::Error);
        assert_eq!(
            reply.fields.error_name,
            Some("org.freedesktop.DBus.Error.NameHasNoOwner")
        );
    }

    /// A name lookup with no name to look up is a malformed CALL, answered
    /// with `InvalidArgs`. It is not a question about the empty string, which
    /// would be answered truthfully and uselessly with "nobody owns that", and
    /// it is not a broken connection: the specification has an error name for
    /// exactly this, and a draft disconnected instead — which its own comment
    /// said it did not do.
    #[test]
    fn a_name_lookup_without_a_name_is_refused_rather_than_answered() {
        let (_bus, mut clients) = bus_of(1);
        let only = clients.pop().expect("one client");
        let (mut peer, _) = Peer::arrive(only);

        peer.send(&bus_call("GetNameOwner", 2));
        let frame = peer.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode the refusal");
        assert_eq!(reply.kind, message::MessageType::Error);
        assert_eq!(
            reply.fields.error_name,
            Some("org.freedesktop.DBus.Error.InvalidArgs")
        );
        assert_eq!(reply.fields.reply_serial, Some(2));

        // And the connection is still usable afterwards, which is the whole
        // difference between a bad call and a bad peer.
        peer.send(&bus_call("GetId", 3));
        let frame = peer.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode");
        assert_eq!(reply.kind, message::MessageType::MethodReturn);
        assert_eq!(reply.fields.reply_serial, Some(3));
    }

    /// A member the broker knows, on an interface that is not the broker's, is
    /// not the broker's method. Answering it would be td-busd impersonating a
    /// peer — and `Ping` is the one every implementation puts on every
    /// connection, so it is the one this is most likely to get wrong.
    #[test]
    fn a_known_member_on_a_foreign_interface_is_not_the_brokers() {
        let (_bus, mut clients) = bus_of(1);
        let only = clients.pop().expect("one client");
        let (mut peer, _) = Peer::arrive(only);

        let call = message::Builder::method_call(
            crate::wire::Endian::Little,
            BUS_PATH,
            Some("org.example.Thing"),
            "Ping",
        )
        .destination(BUS_NAME)
        .serial(2)
        .encode()
        .expect("encode");
        peer.send(&call);
        let frame = peer.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode");
        assert_eq!(reply.kind, message::MessageType::Error);
        assert_eq!(
            reply.fields.error_name,
            Some("org.freedesktop.DBus.Error.UnknownMethod")
        );
    }

    /// `Hello` with no DESTINATION is still `Hello`. There is nothing else a
    /// connection with no name could be addressing, and the alternative — the
    /// `None` arm of the destination match being unreachable — is a dead case
    /// dressed as a live one.
    #[test]
    fn hello_without_a_destination_is_still_hello() {
        let (mut client, _hear) = serving();
        let bare = message::Builder::method_call(
            crate::wire::Endian::Little,
            BUS_PATH,
            Some(BUS_NAME),
            "Hello",
        )
        .serial(1)
        .encode()
        .expect("encode");
        let mut opening = format!("\0AUTH EXTERNAL {}\r\nBEGIN\r\n", uid_hex()).into_bytes();
        opening.extend_from_slice(&bare);
        client.write_all(&opening).expect("write");

        let mut peer = Peer {
            stream: client,
            held: Vec::new(),
            lines: 1,
        };
        let frame = peer.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode");
        assert_eq!(reply.kind, message::MessageType::MethodReturn);
        assert_eq!(
            reply.args().first().and_then(crate::wire::Value::as_str),
            Some(":1.1")
        );
    }

    /// A full BUS does not disconnect whoever calls next. §D's remedy is that
    /// the largest consumer goes; a draft ran it only on the routing path and
    /// turned a bus overflow into `Ended::Failed` everywhere else, which made
    /// the ceiling a weapon aimed at every peer except the ones responsible.
    #[test]
    fn a_full_bus_relieves_its_largest_consumer_rather_than_the_caller() {
        let (bus, mut clients) = bus_of(1);
        let only = clients.pop().expect("one client");
        let (mut peer, _) = Peer::arrive(only);

        // Four hoarders fill the whole budget between them. They are not real
        // connections — they do not need to be. What is being tested is what
        // happens to the peer that calls NEXT.
        let mut kept = Vec::new();
        for which in 0..4i32 {
            let (client, server) = UnixStream::pair().expect("socketpair");
            kept.push(client);
            let outbox = bus.outbox_for(server);
            bus.join(&outbox, 1000, 9100 + which).expect("join");
            outbox
                .push(vec![0u8; crate::registry::MAX_OUTGOING_BYTES])
                .expect("push");
        }
        assert_eq!(bus.queued_bytes(), crate::registry::MAX_OUTGOING_BYTES_TOTAL);

        // An ordinary call from an innocent peer. It is answered.
        peer.send(&bus_call("GetId", 2));
        let frame = peer.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode");
        assert_eq!(
            reply.kind,
            message::MessageType::MethodReturn,
            "a full bus disconnected the peer that asked rather than the one holding it"
        );
        assert_eq!(reply.fields.reply_serial, Some(2));
        // And the budget really did come down, so a hoarder went.
        assert!(
            bus.queued_bytes() < crate::registry::MAX_OUTGOING_BYTES_TOTAL,
            "nothing was relieved"
        );
    }

    /// What is already queued survives the connection ending. The last thing a
    /// peer is told is usually WHY it is ending, and a draft threw it away:
    /// `close` cleared the queue on every exit path, so a peer that made a bad
    /// call and then a fatal one got a bare EOF instead of the reply the
    /// broker had already written for it.
    #[test]
    fn the_reply_before_a_fatal_error_still_reaches_the_peer() {
        let (client, hear) = serving();
        let (mut peer, _) = Peer::arrive(client);

        // Both in ONE write, so the reader dispatches the call and then hits
        // the fatal frame without ever going back to the socket in between.
        let mut together = bus_call("GetId", 2);
        together.extend_from_slice(&[0u8; 16]);
        peer.send(&together);

        // The connection is ENDED first, and only then is anything read. A
        // draft read first, which let the test pass whenever the writer
        // happened to be scheduled before the teardown — it detected the
        // regression sometimes, which for a race is the same as not at all.
        // Waiting means the reply has to have survived seal-flush-close, not
        // merely raced it.
        match ended(&hear).ended {
            Ended::Refused(_) => {}
            other => panic!("a malformed frame gave {other:?}"),
        }
        let frame = peer.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode the answer");
        assert_eq!(reply.kind, message::MessageType::MethodReturn);
        assert_eq!(reply.fields.reply_serial, Some(2));
    }

    /// A message type this version does not know is IGNORED, and the same way
    /// whoever it was addressed to. A draft refused it inside the relay, so
    /// the disposition depended on whether the destination happened to exist:
    /// silently dropped for a name nobody owned, a disconnect for a live peer.
    #[test]
    fn an_unknown_message_type_is_ignored_wherever_it_is_addressed() {
        let (bus, mut clients) = bus_of(2);
        let second = clients.pop().expect("two clients");
        let first = clients.pop().expect("two clients");
        let (mut one, _) = Peer::arrive(first);
        let (mut two, name_two) = Peer::arrive(second);
        let _ = &bus;

        // The SAME message to a live peer and to a name nobody owns. Both
        // must be ignored; the draft disconnected for the first and dropped
        // the second, so the disposition turned on a third party.
        for (which, destination) in [name_two.as_str(), ":1.404"].into_iter().enumerate() {
            let serial = 2u32.saturating_add(u32::try_from(which).unwrap_or(0));
            let mut frame = peer_call(destination, serial, "unknown to you");
            // Type 9 is not a type this version knows. The header's type byte
            // is at offset 1, and nothing else about the frame changes.
            if let Some(kind) = frame.get_mut(1) {
                *kind = 9;
            }
            one.send(&frame);
        }
        // Nothing comes back, and the connection is still alive: an ordinary
        // call after them is answered.
        one.send(&bus_call("GetId", 9));
        let frame = one.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode");
        assert_eq!(reply.kind, message::MessageType::MethodReturn);
        assert_eq!(
            reply.fields.reply_serial,
            Some(9),
            "an unknown type was answered rather than ignored"
        );
        two.expect_silence();
    }

    /// `Hello` honours `NO_REPLY_EXPECTED` like every other method. It still
    /// earns the name — it is the one bus method that changes state — but a
    /// draft answered it regardless, which made it the single method that
    /// ignored the flag.
    #[test]
    fn a_hello_that_wants_no_reply_still_earns_a_name() {
        let (bus, mut clients) = bus_of(1);
        let mut client = clients.pop().expect("one client");
        let quiet = message::Builder::method_call(
            crate::wire::Endian::Little,
            BUS_PATH,
            Some(BUS_NAME),
            "Hello",
        )
        .destination(BUS_NAME)
        .flags(message::FLAG_NO_REPLY_EXPECTED)
        .serial(1)
        .encode()
        .expect("encode");
        let mut opening = format!("\0AUTH EXTERNAL {}\r\nBEGIN\r\n", uid_hex()).into_bytes();
        opening.extend_from_slice(&quiet);
        client.write_all(&opening).expect("write");

        let mut peer = Peer {
            stream: client,
            held: Vec::new(),
            lines: 1,
        };
        // No Hello reply — but the name exists, which the directory can say.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while !bus.names().iter().any(|name| name == ":1.1") {
            assert!(deadline > std::time::Instant::now(), "no name was assigned");
            std::thread::yield_now();
        }
        // And the connection is usable, which proves it got past the gate.
        peer.send(&bus_call("GetId", 2));
        let frame = peer.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode");
        assert_eq!(reply.fields.reply_serial, Some(2), "the Hello was answered");
    }

    /// The bus's own methods live on ONE object; `Peer` lives on every object.
    /// Without the distinction, a call to some application's path addressed to
    /// the bus name would be answered by the broker as though it were that
    /// application's object.
    #[test]
    fn the_brokers_methods_are_at_the_brokers_path_and_ping_is_everywhere() {
        let (_bus, mut clients) = bus_of(1);
        let only = clients.pop().expect("one client");
        let (mut peer, _) = Peer::arrive(only);

        let elsewhere = message::Builder::method_call(
            crate::wire::Endian::Little,
            "/org/example/Thing",
            Some(BUS_NAME),
            "ListNames",
        )
        .destination(BUS_NAME)
        .serial(2)
        .encode()
        .expect("encode");
        peer.send(&elsewhere);
        let frame = peer.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode");
        assert_eq!(reply.kind, message::MessageType::Error);
        assert_eq!(
            reply.fields.error_name,
            Some("org.freedesktop.DBus.Error.UnknownMethod")
        );

        // `Peer.Ping` is answered wherever it is sent.
        let ping = message::Builder::method_call(
            crate::wire::Endian::Little,
            "/org/example/Thing",
            Some(PEER_INTERFACE),
            "Ping",
        )
        .destination(BUS_NAME)
        .serial(3)
        .encode()
        .expect("encode");
        peer.send(&ping);
        let frame = peer.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode");
        assert_eq!(reply.kind, message::MessageType::MethodReturn);
        assert_eq!(reply.fields.reply_serial, Some(3));
    }

    /// `Hello` is held to the same rule as the rest of the broker's
    /// interface: the broker's name, at the broker's object. One sent to
    /// another path is not this broker's `Hello`, and a connection that sends
    /// it has still not said `Hello` — so it is disconnected, and told why in
    /// those words rather than the "Hello before Hello" a draft reported.
    #[test]
    fn a_hello_at_another_object_is_not_hello() {
        let (mut client, hear) = serving();
        let elsewhere = message::Builder::method_call(
            crate::wire::Endian::Little,
            "/org/example/Thing",
            Some(BUS_NAME),
            "Hello",
        )
        .destination(BUS_NAME)
        .serial(1)
        .encode()
        .expect("encode");
        let mut opening = format!("\0AUTH EXTERNAL {}\r\nBEGIN\r\n", uid_hex()).into_bytes();
        opening.extend_from_slice(&elsewhere);
        client.write_all(&opening).expect("write");
        match ended(&hear).ended {
            Ended::Refused(why) => {
                assert!(why.contains("before Hello"), "{why}");
                assert!(
                    why.contains("/org/freedesktop/DBus"),
                    "the reason does not say what was wrong with it: {why}"
                );
            }
            other => panic!("a Hello at another object gave {other:?}"),
        }
    }

    /// A pid of zero is not a process. `SO_PEERCRED` answers 0 for a peer
    /// whose pid does not exist in the reader's namespace, and reporting it
    /// would hand a caller a number that looks like an answer and names
    /// nothing — worse than "no such name", because `/proc/0` fails much later
    /// and somewhere else.
    ///
    /// Pinned on the function rather than through a connection: a test cannot
    /// make a live peer have an unmappable pid, and the rule is worth having
    /// anyway.
    #[test]
    fn a_pid_that_names_nothing_is_not_reported() {
        assert_eq!(usable_pid(42), Some(42));
        assert_eq!(usable_pid(0), None, "pid 0 is not a process");
        assert_eq!(usable_pid(-1), None, "a negative pid is not one");
    }

    /// A message the broker cannot RE-ENCODE is refused, and the sender stays.
    ///
    /// The broker adds a SENDER field, and the cap on an incoming header-field
    /// array is the same number as the cap on an outgoing one — object paths
    /// have no length bound of their own — so a legal message whose fields sit
    /// within a couple of dozen bytes of the ceiling cannot be relayed. A
    /// draft propagated that as a broker FAULT: the sender was torn down with
    /// no reply at all, for a message this broker had itself accepted.
    #[test]
    fn a_message_that_cannot_be_re_encoded_is_refused_not_fatal() {
        let (_bus, mut clients) = bus_of(2);
        let second = clients.pop().expect("two clients");
        let first = clients.pop().expect("two clients");
        let (mut one, _) = Peer::arrive(first);
        let (mut two, name_two) = Peer::arrive(second);

        // The longest object path this codec will accept in a message with
        // these fields — found by asking it, so the test tracks the constant
        // rather than restating it.
        let mut brimful = None;
        for len in (65_000..65_500).rev() {
            let path = format!("/{}", "a".repeat(len));
            let built = message::Builder::method_call(
                crate::wire::Endian::Little,
                &path,
                Some("org.example.Thing"),
                "Do",
            )
            .destination(&name_two)
            .serial(7)
            .encode();
            if let Ok(frame) = built {
                brimful = Some(frame);
                break;
            }
        }
        let brimful = brimful.expect("no path length fits inside the field cap");
        one.send(&brimful);

        let frame = one.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode the refusal");
        assert_eq!(reply.kind, message::MessageType::Error, "the sender was not told");
        assert_eq!(
            reply.fields.error_name,
            Some("org.freedesktop.DBus.Error.LimitsExceeded")
        );
        assert_eq!(reply.fields.reply_serial, Some(7));

        // The sender is still here — that is the half a draft got wrong.
        one.send(&bus_call("GetId", 8));
        let frame = one.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode");
        assert_eq!(reply.kind, message::MessageType::MethodReturn);
        assert_eq!(reply.fields.reply_serial, Some(8));
        two.expect_silence();
    }

    /// A SENDER cannot evict a RECIPIENT by writing at it.
    ///
    /// The per-connection ceiling is one whole maximum message, so two large
    /// messages back to back exceed it however promptly the recipient reads —
    /// the first is still in the writer's hands when the second arrives. A
    /// draft disconnected the recipient for that, and `ListNames` hands every
    /// name to every caller, so one application could walk the bus and evict
    /// every other application two frames at a time, at no cost to itself.
    #[test]
    fn a_sender_cannot_evict_the_peer_it_writes_to() {
        let (_bus, mut clients) = bus_of(2);
        let second = clients.pop().expect("two clients");
        let first = clients.pop().expect("two clients");
        let (mut one, _) = Peer::arrive(first);
        let (mut two, name_two) = Peer::arrive(second);

        // Two of these are over the recipient's ceiling; one is not. The
        // recipient never reads either of them.
        let big = 9 * 1024 * 1024;
        for serial in 1..=2u32 {
            let call = message::Builder::method_call(
                crate::wire::Endian::Little,
                "/org/example/Thing",
                Some("org.example.Thing"),
                "Do",
            )
            .destination(&name_two)
            .serial(serial)
            .body("ay", |writer| {
                writer.array("y", |inner| {
                    inner.append(&vec![0u8; big]);
                    Ok(())
                })
            })
            .expect("body")
            .encode()
            .expect("encode");
            one.send(&call);
        }

        let frame = one.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode");
        assert_eq!(reply.kind, message::MessageType::Error);
        assert_eq!(
            reply.fields.error_name,
            Some("org.freedesktop.DBus.Error.LimitsExceeded"),
            "the sender was not the one refused"
        );

        // And the recipient is still THERE — reading proves it, where the
        // directory does not: `close` shuts the socket down without leaving
        // the bus, so a name stays listed for as long as it takes the victim's
        // own thread to notice. A draft of this test asserted on `names()` and
        // so passed against the very behaviour it was written to forbid.
        let frame = two.frame();
        let (got, _) = message::decode(&frame, 0).expect("decode what arrived");
        assert_eq!(got.kind, message::MessageType::MethodCall);
        assert_eq!(
            got.serial, 1,
            "the recipient got something other than the first message"
        );
    }

    /// A bus method checks the SIGNATURE it was called with, not just the
    /// first argument's type. A draft read `args()[0]` and ignored the rest,
    /// so a call with a spare argument, or a no-argument method called with a
    /// body, ran as though it had been called correctly.
    #[test]
    fn a_bus_method_checks_the_signature_it_was_called_with() {
        let (_bus, mut clients) = bus_of(1);
        let only = clients.pop().expect("one client");
        let (mut peer, _) = Peer::arrive(only);

        // A no-argument method, called with one.
        let noisy = message::Builder::method_call(
            crate::wire::Endian::Little,
            BUS_PATH,
            Some(PEER_INTERFACE),
            "Ping",
        )
        .destination(BUS_NAME)
        .serial(2)
        .body("s", |writer| writer.string("unasked for"))
        .expect("body")
        .encode()
        .expect("encode");
        peer.send(&noisy);
        let frame = peer.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode");
        assert_eq!(reply.kind, message::MessageType::Error, "Ping took a body");
        assert_eq!(
            reply.fields.error_name,
            Some("org.freedesktop.DBus.Error.InvalidArgs")
        );

        // A one-argument method, called with two.
        let extra = message::Builder::method_call(
            crate::wire::Endian::Little,
            BUS_PATH,
            Some(BUS_NAME),
            "GetNameOwner",
        )
        .destination(BUS_NAME)
        .serial(3)
        .body("ss", |writer| {
            writer.string(BUS_NAME)?;
            writer.string("and one more")
        })
        .expect("body")
        .encode()
        .expect("encode");
        peer.send(&extra);
        let frame = peer.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode");
        assert_eq!(reply.kind, message::MessageType::Error, "a spare argument was ignored");
        assert_eq!(
            reply.fields.error_name,
            Some("org.freedesktop.DBus.Error.InvalidArgs")
        );

        // A string that is not a bus name at all.
        peer.send(&name_query("GetNameOwner", "not a bus name", 4));
        let frame = peer.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode");
        assert_eq!(reply.kind, message::MessageType::Error);
        assert_eq!(
            reply.fields.error_name,
            Some("org.freedesktop.DBus.Error.InvalidArgs"),
            "a string that cannot be owned was looked up rather than refused"
        );

        // And the connection survives all three.
        peer.send(&bus_call("GetId", 5));
        let frame = peer.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode");
        assert_eq!(reply.fields.reply_serial, Some(5));
    }

    /// `Hello` on a foreign INTERFACE is not this broker's `Hello`, on the
    /// same rule that puts the rest of the interface at one object. A draft
    /// checked the destination and the path and left the interface out, so
    /// `org.example.Thing.Hello` earned a unique name.
    #[test]
    fn a_hello_on_a_foreign_interface_is_not_hello() {
        let (mut client, hear) = serving();
        let foreign = message::Builder::method_call(
            crate::wire::Endian::Little,
            BUS_PATH,
            Some("org.example.Thing"),
            "Hello",
        )
        .destination(BUS_NAME)
        .serial(1)
        .encode()
        .expect("encode");
        let mut opening = format!("\0AUTH EXTERNAL {}\r\nBEGIN\r\n", uid_hex()).into_bytes();
        opening.extend_from_slice(&foreign);
        client.write_all(&opening).expect("write");
        match ended(&hear).ended {
            Ended::Refused(why) => assert!(why.contains("before Hello"), "{why}"),
            other => panic!("a Hello on a foreign interface gave {other:?}"),
        }
    }

    /// A peer whose pid is not knowable is not a peer that is absent.
    ///
    /// `SO_PEERCRED` answers 0 for a pid that does not exist in the reader's
    /// namespace — the case td's jails will produce. A draft answered
    /// `NameHasNoOwner` for it, which `ListNames` and `GetNameOwner`
    /// contradict one call later, and made `GetConnectionCredentials` throw
    /// away a uid the kernel HAD reported. An absent dictionary entry says
    /// "not known"; a zero says "pid zero"; and "no such name" says something
    /// else entirely.
    ///
    /// A socketpair cannot be made to have such a peer, so the connection is
    /// put into the directory directly. What is under test is the LOOKUP.
    #[test]
    fn a_peer_whose_pid_is_unknown_is_not_a_peer_that_is_absent() {
        let (bus, mut clients) = bus_of(1);
        let only = clients.pop().expect("one client");
        let (mut peer, _) = Peer::arrive(only);

        let (_held, server) = UnixStream::pair().expect("socketpair");
        let outbox = bus.outbox_for(server);
        let placeless = bus.join(&outbox, 1000, 0).expect("join");

        // The name is here.
        peer.send(&name_query("NameHasOwner", &placeless, 2));
        let frame = peer.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode");
        assert_eq!(reply.kind, message::MessageType::MethodReturn);

        // Its uid is knowable.
        peer.send(&name_query("GetConnectionUnixUser", &placeless, 3));
        let frame = peer.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode");
        assert_eq!(
            reply.args().first().and_then(crate::wire::Value::as_u32),
            Some(1000)
        );

        // Its pid is not, and that is its own answer.
        peer.send(&name_query("GetConnectionUnixProcessID", &placeless, 4));
        let frame = peer.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode");
        assert_eq!(reply.kind, message::MessageType::Error);
        assert_eq!(
            reply.fields.error_name,
            Some("org.freedesktop.DBus.Error.UnixProcessIdUnknown"),
            "an unknowable pid was reported as an unknown name"
        );

        // And the dictionary reports what it can rather than nothing.
        peer.send(&name_query("GetConnectionCredentials", &placeless, 5));
        let frame = peer.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode");
        assert_eq!(reply.kind, message::MessageType::MethodReturn);
        let entries = reply
            .args()
            .first()
            .and_then(crate::wire::Value::as_seq)
            .expect("a dictionary")
            .values(16)
            .expect("read it");
        let mut keys = Vec::new();
        for entry in &entries {
            let pair = entry.as_seq().expect("an entry").values(2).expect("read it");
            if let Some(key) = pair.first().and_then(crate::wire::Value::as_str) {
                keys.push(key.to_string());
            }
        }
        assert!(keys.contains(&"UnixUserID".to_string()), "{keys:?}");
        assert!(
            !keys.contains(&"ProcessID".to_string()),
            "an unknowable pid was reported anyway: {keys:?}"
        );
    }

    /// A name lookup called with one string argument.
    fn name_query(member: &str, name: &str, serial: u32) -> Vec<u8> {
        message::Builder::method_call(crate::wire::Endian::Little, BUS_PATH, Some(BUS_NAME), member)
            .destination(BUS_NAME)
            .serial(serial)
            .body("s", |writer| writer.string(name))
            .expect("body")
            .encode()
            .expect("encode a name query")
    }

    #[test]
    fn the_parent_directory_is_private_and_a_stale_socket_is_replaced() {
        let dir = scratch("dir");
        let path = dir.join("bus");
        let bound = bind(&path).expect("bind");
        assert_eq!(bound.path(), path.as_path());
        let mode = fs::metadata(&dir).expect("parent").permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "parent is {mode:o}");

        // Binding again over a socket whose listener is GONE succeeds: the
        // stale one is removed, which is what a restart after an unclean exit
        // needs. The live case is the opposite and is its own test below —
        // this comment named it for a while, which made the pair read as one
        // contradiction rather than two rules.
        drop(bound);
        let again = bind(&path).expect("rebind");
        assert!(again.path().exists());
        drop(again);

        // Something that is not a socket is reported, not deleted.
        fs::remove_file(&path).ok();
        fs::write(&path, b"not a socket").expect("write");
        let refusal = match bind(&path) {
            Ok(_) => panic!("bound over a regular file"),
            Err(error) => error,
        };
        assert_eq!(refusal.kind(), io::ErrorKind::AlreadyExists);
        assert!(path.exists(), "the file was deleted");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_guid_is_thirty_two_hex_digits_and_differs_per_bus() {
        let one = guid_text().expect("guid");
        let two = guid_text().expect("guid");
        assert_eq!(one.len(), GUID_LEN);
        assert!(one.bytes().all(|byte| byte.is_ascii_hexdigit()), "{one}");
        assert!(Guid::new(&one).is_ok(), "{one} is not a guid");
        assert_ne!(one, two, "two buses would claim the same identity");
    }

    #[test]
    fn a_connection_authenticates_by_the_kernels_uid() {
        let (mut client, hear) = serving();
        let hex: String = this_uid()
            .to_string()
            .bytes()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        client
            .write_all(format!("\0AUTH EXTERNAL {hex}\r\n").as_bytes())
            .expect("write");
        let mut reply = [0u8; 64];
        let read = client.read(&mut reply).expect("read");
        let text = String::from_utf8_lossy(&reply[..read]).to_string();
        assert_eq!(text, format!("OK {GUID}\r\n"), "got {text:?}");
        drop(client);
        let outcome = ended(&hear);
        assert!(matches!(outcome.ended, Ended::PeerLeft), "{outcome:?}");
        // The kernel's answer, and what the connection is charged to.
        assert_eq!(outcome.credential.uid, this_uid());
        assert_eq!(outcome.credential.pid, std::process::id() as i32);
        assert_eq!(outcome.uid, Some(this_uid()));
    }

    /// The claim is checked against what the KERNEL says, not against what the
    /// peer would like to be. A peer on a socketpair is this process, so a
    /// claim of somebody else must be refused however plausible it looks.
    #[test]
    fn a_claim_the_kernel_does_not_support_is_rejected() {
        let (mut client, _hear) = serving();
        let other = this_uid().wrapping_add(1);
        let hex: String = other
            .to_string()
            .bytes()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        client
            .write_all(format!("\0AUTH EXTERNAL {hex}\r\n").as_bytes())
            .expect("write");
        let mut reply = [0u8; 64];
        let read = client.read(&mut reply).expect("read");
        assert_eq!(
            String::from_utf8_lossy(&reply[..read]),
            "REJECTED EXTERNAL\r\n"
        );
    }

    #[test]
    fn a_probe_completes_the_handshake_against_a_real_socket() {
        let dir = scratch("probe");
        let path = dir.join("bus");
        let bound = bind(&path).expect("bind");
        let listener = bound.listener().try_clone().expect("clone");
        let served = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let guid = Guid::new(GUID).expect("guid");
            let quota = Quota::new();
            let bus = Bus::new();
            let mut connection = Connection::accept(stream, guid, &quota, &bus).expect("connect");
            connection.serve()
        });
        let summary = probe(&path, this_uid()).expect("probe");
        assert!(summary.contains(GUID), "{summary}");
        let ended = served.join().expect("thread");
        assert!(matches!(ended, Ended::PeerLeft), "{ended:?}");
        fs::remove_dir_all(&dir).ok();
    }

    /// A probe against a bus that refuses it must FAIL, or `ready=` would pass
    /// for a bus nobody can use.
    #[test]
    fn a_probe_that_is_refused_reports_failure() {
        let dir = scratch("refused");
        let path = dir.join("bus");
        let bound = bind(&path).expect("bind");
        let listener = bound.listener().try_clone().expect("clone");
        thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let guid = Guid::new(GUID).expect("guid");
            let quota = Quota::new();
            // Bound to a local rather than matched inline: the scrutinee's
            // `Result` temporary otherwise lives to the end of this block,
            // which is after `quota` is dropped.
            let bus = Bus::new();
            let accepted = Connection::accept(stream, guid, &quota, &bus);
            if let Ok(mut connection) = accepted {
                let _ = connection.serve();
            }
        });
        let refusal = probe(&path, this_uid().wrapping_add(1)).expect_err("probe passed");
        assert!(refusal.contains("refused this uid"), "{refusal}");
        fs::remove_dir_all(&dir).ok();
    }

    /// A message arriving after BEGIN is framed, decoded and answered, and the
    /// first one a connection is allowed to send is `Hello`. Its answer is the
    /// name every other peer on this bus will address it by, so the reply is
    /// checked field by field here rather than through the `Peer` helper the
    /// routing tests use: this is where the shape of it is pinned.
    #[test]
    fn a_message_after_begin_is_decoded_and_answered() {
        let (mut client, _hear) = serving();
        // The handshake and the first message in ONE write, which is what
        // libdbus does.
        let mut opening = format!("\0AUTH EXTERNAL {}\r\nBEGIN\r\n", uid_hex()).into_bytes();
        opening.extend_from_slice(&bus_call("Hello", 1));
        client.write_all(&opening).expect("write");

        let mut got = Vec::new();
        let mut chunk = [0u8; 512];
        // The OK line, then the reply; read until a whole frame is there.
        while message::frame_len(after_ok(&got)).ok().flatten().is_none() {
            let read = client.read(&mut chunk).expect("read");
            assert_ne!(read, 0, "the bus closed without answering");
            got.extend_from_slice(&chunk[..read]);
        }
        let body = after_ok(&got);
        let (reply, _) = message::decode(body, 0).expect("decode the reply");
        assert_eq!(reply.kind, message::MessageType::MethodReturn);
        assert_eq!(reply.fields.reply_serial, Some(1));
        // The broker says who it is. A reply with no SENDER would leave a
        // client unable to tell the bus's word from a peer's.
        assert_eq!(reply.fields.sender, Some(BUS_NAME));
        assert_eq!(reply.fields.signature, Some("s"));
        let name = reply
            .args()
            .first()
            .and_then(crate::wire::Value::as_str)
            .expect("Hello answers with a name");
        assert_eq!(name, ":1.1", "the first connection on a bus is :1.1");
        // And it is addressed to the name it just handed out.
        assert_eq!(reply.fields.destination, Some(":1.1"));
    }

    /// Split the OK line off the front of what the bus sent.
    fn after_ok(all: &[u8]) -> &[u8] {
        match all.windows(2).position(|pair| pair == b"\r\n") {
            Some(at) => all.get(at + 2..).unwrap_or(&[]),
            None => &[],
        }
    }

    /// The hex the EXTERNAL mechanism wants for this process's uid.
    fn uid_hex() -> String {
        this_uid()
            .to_string()
            .bytes()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    /// Descriptors that arrive and are never claimed are BOUNDED. A peer can
    /// attach descriptors to bytes that never complete a message, and each one
    /// costs the broker a slot in its own descriptor table — so a queue that
    /// grew without limit is an `EMFILE` on some unrelated later connection,
    /// reachable by a client that only has to keep sending.
    #[test]
    fn unclaimed_descriptors_do_not_queue_without_bound() {
        let (client, hear) = serving();
        let opening = format!(
            "\0AUTH EXTERNAL {}\r\nNEGOTIATE_UNIX_FD\r\nBEGIN\r\n",
            uid_hex()
        );
        sys::send(&client, opening.as_bytes(), &[]).expect("write the opening");
        // The server must have AGREED before a descriptor may be sent, and it
        // answers on the same socket; reading its OK/AGREE lines here also
        // orders this write after the handshake it depends on.
        let mut answer = [0u8; 128];
        let mut got = Vec::new();
        while !got.windows(13).any(|w| w == b"AGREE_UNIX_FD") {
            let read = (&client).read(&mut answer).expect("read the agreement");
            assert_ne!(read, 0, "the bus closed before agreeing");
            got.extend_from_slice(&answer[..read]);
        }
        let spare = fs::File::open("/dev/null").expect("/dev/null");
        // One byte at a time so nothing ever frames a message that could claim
        // them, and more descriptors than the queue is allowed to hold.
        let batch = vec![spare.as_raw_fd(); 16];
        let mut sent = 0usize;
        while sent <= MAX_QUEUED_FDS {
            if sys::send(&client, b"l", &batch).is_err() {
                break;
            }
            sent += batch.len();
        }
        let outcome = ended(&hear);
        match outcome.ended {
            Ended::Refused(why) => assert!(
                why.contains("unclaimed"),
                "ended for the wrong reason: {why}"
            ),
            other => panic!("{sent} descriptors queued and got {other:?}"),
        }
    }

    /// A message may not claim descriptors that did not arrive with it. The
    /// count in `UNIX_FDS` is the peer's word about its own message; the queue
    /// is the kernel's account of what crossed. Where they disagree the peer
    /// is wrong, and the refusal comes from the message layer rather than from
    /// here — which is the point of checking it at the layer that owns it.
    #[test]
    fn a_message_claiming_descriptors_that_did_not_arrive_is_refused() {
        let (mut client, hear) = serving();
        let hello = message::Builder::method_call(
            crate::wire::Endian::Little,
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "Hello",
        )
        .destination("org.freedesktop.DBus")
        .unix_fds(3)
        .serial(1)
        .encode()
        .expect("encode");
        let mut opening = format!("\0AUTH EXTERNAL {}\r\nBEGIN\r\n", uid_hex()).into_bytes();
        opening.extend_from_slice(&hello);
        client.write_all(&opening).expect("write");
        let outcome = ended(&hear);
        match outcome.ended {
            Ended::Refused(why) => assert!(
                why.contains("UNIX_FDS disagrees"),
                "ended for the wrong reason: {why}"
            ),
            other => panic!("a claim of 3 against 0 gave {other:?}"),
        }
    }

    /// The bound that actually bounds the buffer: a message is refused on the
    /// length it DECLARES, before a byte of its body is buffered. A broker
    /// that waited to see whether the body arrived would have agreed to hold
    /// it, which is the whole attack — the declaration is free to make and
    /// the memory is not.
    #[test]
    fn a_message_declaring_more_than_the_cap_is_refused_before_it_arrives() {
        let (mut client, hear) = serving();
        let mut opening = format!("\0AUTH EXTERNAL {}\r\nBEGIN\r\n", uid_hex()).into_bytes();
        // A well-formed little-endian header whose body length is past the
        // cap, and nothing after it. Fields array empty, body enormous.
        let mut header = vec![b'l', 1, 0, 1];
        header.extend_from_slice(&(message::MAX_BODY_BYTES + 1).to_le_bytes());
        header.extend_from_slice(&1u32.to_le_bytes());
        header.extend_from_slice(&0u32.to_le_bytes());
        opening.extend_from_slice(&header);
        client.write_all(&opening).expect("write");
        let outcome = ended(&hear);
        match outcome.ended {
            Ended::Refused(why) => assert!(
                why.contains("frame") && why.contains("ceiling"),
                "ended for the wrong reason: {why}"
            ),
            other => panic!("an oversized declaration gave {other:?}"),
        }
        // And the body was never buffered: the refusal came from the header.
        assert!(
            outcome.credential.uid == this_uid(),
            "the peer was misidentified"
        );
    }

    /// §D's ceiling has two halves and the second is the one that matters:
    /// a global cap alone is the denial of service, because one app reaches
    /// it by opening 64 sockets and every other application is then locked
    /// off the bus. Both halves are checked here, since a quota that enforced
    /// only the global one would pass a test that only counted.
    #[test]
    fn the_quota_holds_a_peer_to_its_share_and_the_bus_to_its_ceiling() {
        let quota = std::sync::Arc::new(Quota::new());
        // One peer may take its share and no more.
        let mut mine = Vec::new();
        for which in 0..MAX_CONNECTIONS_PER_PEER {
            match quota.try_admit(7) {
                Ok(place) => mine.push(place),
                Err(why) => panic!("pid 7 refused at {which}: {why}"),
            }
        }
        match quota.try_admit(7) {
            Ok(_) => panic!("pid 7 took more than its share"),
            Err(why) => assert!(why.contains("pid 7"), "{why}"),
        }
        // And another peer is unaffected, which is the whole point.
        let other = quota.try_admit(8).expect("a different peer is refused");
        assert_eq!(quota.live_count(), MAX_CONNECTIONS_PER_PEER + 1);

        // Filling the bus from enough distinct peers reaches the global cap.
        let mut rest = Vec::new();
        let mut pid = 9;
        while quota.live_count() < MAX_CONNECTIONS {
            match quota.try_admit(pid) {
                Ok(place) => rest.push(place),
                Err(_) => pid += 1,
            }
        }
        match quota.try_admit(4242) {
            Ok(_) => panic!("the bus went past its ceiling"),
            Err(why) => assert!(why.contains("already serving"), "{why}"),
        }

        // A place is given back when it is dropped, however it is dropped.
        drop(other);
        drop(mine.pop());
        assert_eq!(quota.live_count(), MAX_CONNECTIONS - 2);
        quota.try_admit(4242).expect("room after two left");
    }

    /// The descriptor budget is the BUS's, not a connection's. A per-connection
    /// cap of 64 across 64 connections is 4096 descriptors — past a 1024
    /// `RLIMIT_NOFILE`, and so exactly the `EMFILE` the per-connection cap was
    /// added to prevent.
    #[test]
    fn queued_descriptors_are_bounded_across_the_whole_bus() {
        let quota = Quota::new();
        // One message's worth at a time, up to the bus's budget.
        let batches = MAX_QUEUED_FDS_TOTAL / sys::MAX_FDS;
        for which in 0..batches {
            if let Err(why) = quota.take_fds(sys::MAX_FDS) {
                panic!("batch {which} refused: {why}");
            }
        }
        match quota.take_fds(1) {
            Ok(()) => panic!("the bus went past its descriptor budget"),
            Err(why) => assert!(why.contains("across the bus"), "{why}"),
        }
        // Claimed descriptors come back.
        quota.release_fds(sys::MAX_FDS);
        quota.take_fds(sys::MAX_FDS).expect("room after a claim");
        // And a release of more than is held floors at zero rather than
        // wrapping into an enormous budget.
        quota.release_fds(MAX_QUEUED_FDS_TOTAL * 4);
        quota
            .take_fds(MAX_QUEUED_FDS_TOTAL)
            .expect("an emptied budget is the whole budget");
        assert!(quota.take_fds(1).is_err(), "the budget wrapped");
    }

    /// A connection that ends returns whatever it was still holding. Without
    /// it the bus's budget only ever falls, and a broker that has served
    /// enough peers refuses descriptors it has the room for.
    #[test]
    fn a_connection_returns_its_freight_to_the_bus_when_it_ends() {
        let quota = Quota::new();
        quota.take_fds(sys::MAX_FDS).expect("charge");
        {
            let (_client, server) = UnixStream::pair().expect("socketpair");
            let guid = Guid::new(GUID).expect("guid");
            let bus = Bus::new();
            let accepted = Connection::accept(server, guid, &quota, &bus);
            let mut connection = accepted.expect("accept");
            let spare = fs::File::open("/dev/null").expect("/dev/null");
            let owned: OwnedFd = spare.into();
            connection.freight.push(owned);
        }
        // The connection is gone; what it held is back.
        quota
            .take_fds(MAX_QUEUED_FDS_TOTAL - sys::MAX_FDS + 1)
            .expect("the freight was returned");
    }

    /// §D asks for the socket at 0600 as well as the parent at 0700. A bind
    /// creates the socket fresh and umask decides its mode, so under the 022
    /// umask `td-login` preserves it lands 0755 unless something says
    /// otherwise. The parent is the real boundary; this is the second lock,
    /// and a normative document that says 0600 against a broker that produces
    /// 0755 is a disagreement worth failing over.
    #[test]
    fn the_socket_itself_is_private_whatever_the_umask_is() {
        let dir = scratch("mode");
        let path = dir.join("bus");
        let bound = bind(&path).expect("bind");
        let mode = fs::metadata(bound.path()).expect("socket").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "socket is {mode:o}");
        fs::remove_dir_all(&dir).ok();
    }

    /// A socket with a LIVE listener is not stolen. Removing it would leave
    /// the running broker alive and unreachable — its peers stranded on a
    /// socket with no name, every later client reaching the usurper — and
    /// nothing would report it, because both processes are working.
    #[test]
    fn a_bus_that_is_already_listening_is_not_displaced() {
        let dir = scratch("live");
        let path = dir.join("bus");
        let first = bind(&path).expect("the first bind");
        match bind(&path) {
            Ok(_) => panic!("the second bind displaced a live listener"),
            Err(error) => assert_eq!(error.kind(), io::ErrorKind::AddrInUse),
        }
        // And the original is still the one listening.
        drop(first);
        // With it gone the path is stale rather than live, and a bind may
        // take it — which is what a restart after an unclean exit needs.
        bind(&path).expect("rebinding a stale socket");
        fs::remove_dir_all(&dir).ok();
    }

    /// §D: a client that skips `NEGOTIATE_UNIX_FD` is refused any message
    /// carrying a descriptor, per the specification. The negotiation exists so
    /// that a peer which cannot handle descriptors never receives one; a
    /// server that accepted them from a peer that never asked would be
    /// enforcing half of a two-sided agreement.
    #[test]
    fn descriptors_before_the_agreement_are_refused() {
        let (client, hear) = serving();
        // Authenticated and begun, but never negotiated.
        let opening = format!("\0AUTH EXTERNAL {}\r\nBEGIN\r\n", uid_hex());
        sys::send(&client, opening.as_bytes(), &[]).expect("write the opening");
        let spare = fs::File::open("/dev/null").expect("/dev/null");
        // The send may fail if the server refuses and closes first; either way
        // the connection must end, and end as the PEER's fault.
        let _ = sys::send(&client, b"x", &[spare.as_raw_fd()]);
        let outcome = ended(&hear);
        match outcome.ended {
            Ended::Refused(why) => assert!(
                why.contains("AGREE_UNIX_FD"),
                "ended for the wrong reason: {why}"
            ),
            other => panic!("a descriptor without the agreement gave {other:?}"),
        }
    }

    /// Only a method call that wants a reply gets one. A reply to a signal is
    /// a protocol violation in its own right, a message type this version does
    /// not know must be ignored rather than answered, and `NO_REPLY_EXPECTED`
    /// withdraws the reply from a call that would take one.
    ///
    /// All three are sent in ONE write ahead of an ordinary call, so the test
    /// distinguishes "answered nothing" from "answered late": exactly one
    /// reply must come back, and it must name the ordinary call's serial.
    #[test]
    fn only_a_method_call_expecting_a_reply_is_answered() {
        let (client, _hear) = serving();
        let (mut peer, name) = Peer::arrive(client);
        assert_eq!(name, ":1.1");

        let signal = message::Builder::signal(
            crate::wire::Endian::Little,
            "/org/example",
            "org.example.Thing",
            "Happened",
        )
        .serial(2)
        .encode()
        .expect("encode the signal");
        let quiet = message::Builder::method_call(
            crate::wire::Endian::Little,
            BUS_PATH,
            Some(BUS_NAME),
            "GetId",
        )
        .destination(BUS_NAME)
        .flags(message::FLAG_NO_REPLY_EXPECTED)
        .serial(3)
        .encode()
        .expect("encode the quiet call");
        let asking = bus_call("GetId", 4);

        // All three in ONE write, so the test distinguishes "answered nothing"
        // from "answered late": exactly one reply must come back, and it must
        // name the ordinary call's serial.
        let mut together = signal;
        together.extend_from_slice(&quiet);
        together.extend_from_slice(&asking);
        peer.send(&together);

        let frame = peer.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode the reply");
        assert_eq!(reply.kind, message::MessageType::MethodReturn);
        assert_eq!(
            reply.fields.reply_serial,
            Some(4),
            "the answer belongs to the call that asked for one"
        );
        // And that is the ONLY answer: nothing follows it.
        peer.expect_silence();
    }

    /// A CONNECT that never returns is given up on, which is a different
    /// fault from the silent bus below and was not bounded at all before.
    ///
    /// `connect(2)` on a unix socket whose accept queue is full blocks against
    /// the connecting socket's `SO_SNDTIMEO`, and a timeout installed after
    /// `UnixStream::connect` has returned cannot bound a call that has not
    /// returned. `ready=` survived that because td-svc group-kills each probe
    /// attempt; `/etc/bootsuccess` calls the probe with no wrapper of its own,
    /// so an unbounded connect there hangs the health target until the HOST
    /// kills the VM — a bare timeout with no guest-side reason in it, which is
    /// the failure the whole boot budget exists to prevent.
    #[test]
    fn a_connect_that_never_returns_is_given_up_on() {
        let (tell, hear) = mpsc::channel();
        thread::spawn(move || {
            // Run it on its own thread so that a `connect_by` which does NOT
            // give up fails this test rather than hanging it.
            let outcome = connect_by(
                Path::new("/nonexistent/bus"),
                std::time::Duration::from_millis(150),
                || {
                    thread::sleep(std::time::Duration::from_secs(30));
                    Err("waited for a connect that should have been abandoned".into())
                },
            );
            let _ = tell.send(outcome);
        });
        match hear.recv_timeout(std::time::Duration::from_secs(10)) {
            Ok(Ok(_)) => panic!("a connect that never returned produced a stream"),
            Ok(Err(why)) => assert!(
                why.contains("did not accept in time"),
                "gave up for another reason: {why}"
            ),
            Err(_) => panic!("the connect was never given up on"),
        }
    }

    /// The probe's wait is ONE deadline over connect and answer together, not
    /// one each. Two deadlines would let a bus that is slow to accept AND slow
    /// to answer hold the probe for twice `PROBE_TIMEOUT`, and the boot budget
    /// is derived from the single figure.
    #[test]
    fn the_probes_deadline_covers_the_connect_and_the_answer_together() {
        let dir = scratch("one-deadline");
        let path = dir.join("bus");
        let bound = bind(&path).expect("bind");
        // Accept, and then never answer: the connect is instant, so all of the
        // wait is spent on the read.
        let held = thread::spawn(move || bound.listener().accept().map(|(stream, _)| stream));

        let wait = std::time::Duration::from_millis(300);
        let started = std::time::Instant::now();
        let outcome = probe_within(&path, this_uid(), wait);
        let spent = started.elapsed();
        assert!(outcome.is_err(), "a silent bus answered");
        assert!(
            spent < wait.saturating_mul(2),
            "the probe spent {spent:?} against a {wait:?} deadline, so the connect \
             and the answer are being given a deadline each"
        );
        drop(held.join());
        fs::remove_dir_all(&dir).ok();
    }

    /// A bus that accepts and then says nothing does not hold `probe` for
    /// ever. `APPLICATIONS.md` §A's `ready=` line calls this, so a probe that
    /// can hang is a readiness check that reports neither ready nor failed —
    /// the supervisor waits on it instead of restarting anything.
    ///
    /// The wait is a parameter so this costs milliseconds rather than the five
    /// seconds the shipped constant would.
    #[test]
    fn a_probe_against_a_silent_bus_gives_up_rather_than_hanging() {
        let dir = scratch("silent");
        let path = dir.join("bus");
        let bound = bind(&path).expect("bind");
        // Accept, and then never answer.
        let held = thread::spawn(move || bound.listener().accept().map(|(stream, _)| stream));
        // Run the probe on its own thread so that a probe which does NOT
        // give up fails this test instead of hanging it. Asserting on elapsed
        // time cannot do that: the assertion is never reached.
        let (tell, hear) = mpsc::channel();
        let probing = path.clone();
        let uid = this_uid();
        thread::spawn(move || {
            let outcome = probe_within(&probing, uid, std::time::Duration::from_millis(200));
            let _ = tell.send(outcome);
        });
        match hear.recv_timeout(std::time::Duration::from_secs(10)) {
            Ok(Ok(summary)) => panic!("a silent bus answered: {summary}"),
            Ok(Err(why)) => assert!(why.contains("read"), "gave up for another reason: {why}"),
            Err(_) => panic!("the probe never gave up"),
        }
        drop(held.join());
        fs::remove_dir_all(&dir).ok();
    }

    /// A sender's serials are unique per connection, per the specification. A
    /// broker answering every call with serial 1 tells a client that every
    /// reply it will ever receive is the same message — harmless while clients
    /// match on `reply_serial`, and a conformance gap in a commit whose
    /// subject is conformance.
    #[test]
    fn each_reply_carries_its_own_serial() {
        let (mut client, _hear) = serving();
        let mut opening = format!("\0AUTH EXTERNAL {}\r\nBEGIN\r\n", uid_hex()).into_bytes();
        // `Hello` once — a second one ends the connection — and then three
        // calls that can be repeated, which is what this is really about.
        opening.extend_from_slice(&bus_call("Hello", 1));
        for serial in 2..=4u32 {
            opening.extend_from_slice(&bus_call("GetId", serial));
        }
        client.write_all(&opening).expect("write");

        let mut got = Vec::new();
        let mut chunk = [0u8; 1024];
        let mut seen = Vec::new();
        while seen.len() < 4 {
            let read = client.read(&mut chunk).expect("read");
            assert_ne!(read, 0, "the bus closed after {} replies", seen.len());
            got.extend_from_slice(&chunk[..read]);
            let mut rest = after_ok(&got);
            seen.clear();
            while let Ok(Some(length)) = message::frame_len(rest) {
                if rest.len() < length {
                    break;
                }
                let (frame, tail) = rest.split_at(length);
                let (reply, _) = message::decode(frame, 0).expect("decode");
                seen.push((reply.serial, reply.fields.reply_serial));
                rest = tail;
            }
        }
        let serials: Vec<u32> = seen.iter().map(|(serial, _)| *serial).collect();
        let answered: Vec<Option<u32>> = seen.iter().map(|(_, to)| *to).collect();
        assert_eq!(
            answered,
            vec![Some(1), Some(2), Some(3), Some(4)],
            "wrong calls answered"
        );
        let mut unique = serials.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(
            unique.len(),
            serials.len(),
            "the bus reused a serial: {serials:?}"
        );
        assert!(!serials.contains(&0), "zero is not a legal serial");
    }

    /// The probe's wait is an OVERALL deadline, not a per-read one. A socket
    /// timeout is an inactivity timeout, so a bus that dribbles one byte just
    /// inside it holds `ready=` for `MAX_LINE` times the timeout — hours, on a
    /// check a supervisor is waiting for.
    #[test]
    fn a_probe_against_a_dribbling_bus_still_gives_up() {
        let dir = scratch("dribble");
        let path = dir.join("bus");
        let bound = bind(&path).expect("bind");
        let dribbling = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let stop = std::sync::Arc::clone(&dribbling);
        // A bus that answers, slowly, for ever — and never ends a line.
        let held = thread::spawn(move || {
            if let Ok((mut stream, _)) = bound.listener().accept() {
                while stop.load(std::sync::atomic::Ordering::Acquire) {
                    if stream.write_all(b"O").is_err() {
                        break;
                    }
                    thread::sleep(std::time::Duration::from_millis(20));
                }
            }
        });

        let (tell, hear) = mpsc::channel();
        let probing = path.clone();
        let uid = this_uid();
        thread::spawn(move || {
            // Each read finishes well inside this, so only an overall deadline
            // can end the exchange.
            let outcome = probe_within(&probing, uid, std::time::Duration::from_millis(300));
            let _ = tell.send(outcome);
        });
        let outcome = hear.recv_timeout(std::time::Duration::from_secs(15));
        dribbling.store(false, std::sync::atomic::Ordering::Release);
        drop(held.join());
        match outcome {
            Ok(Ok(summary)) => panic!("a dribbling bus was accepted: {summary}"),
            Ok(Err(why)) => assert!(why.contains("read"), "gave up for another reason: {why}"),
            Err(_) => panic!("the probe never gave up on a dribbling bus"),
        }
        fs::remove_dir_all(&dir).ok();
    }

    /// A parent this call did not create is left exactly as it was. `bind`
    /// takes its path from argv, so a broker that chmodded whatever it was
    /// handed would make `/tmp` private to one uid on `run --socket /tmp/bus`.
    #[test]
    fn an_existing_parent_directory_keeps_its_mode() {
        let dir = scratch("kept");
        fs::create_dir_all(&dir).expect("scratch");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).expect("chmod");
        let path = dir.join("bus");
        let bound = bind(&path).expect("bind");
        let mode = fs::metadata(&dir).expect("parent").permissions().mode();
        assert_eq!(mode & 0o777, 0o755, "the parent was chmodded to {mode:o}");
        // The socket itself is still private, which is what defends it when
        // the parent belongs to somebody else.
        let socket = fs::metadata(bound.path()).expect("socket").permissions().mode();
        assert_eq!(socket & 0o777, 0o600, "socket is {socket:o}");
        fs::remove_dir_all(&dir).ok();
    }

    /// A peer that sends rubbish after BEGIN is disconnected rather than
    /// tolerated, and the refusal says what was wrong with it.
    #[test]
    fn a_malformed_message_ends_the_connection() {
        let (mut client, hear) = serving();
        let hex: String = this_uid()
            .to_string()
            .bytes()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let mut opening = format!("\0AUTH EXTERNAL {hex}\r\nBEGIN\r\n").into_bytes();
        // A plausible header with an impossible endianness byte.
        opening.extend_from_slice(&[b'Q'; 16]);
        client.write_all(&opening).expect("write");
        let outcome = ended(&hear);
        match outcome.ended {
            Ended::Refused(why) => {
                assert!(why.contains("frame") || why.contains("message"), "{why}")
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }
}

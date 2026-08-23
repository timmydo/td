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

use crate::auth::{Guid, Handshake, PeerIdentity, GUID_LEN};
use crate::message;
use crate::sys::{self, PeerCredential};

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
    credential: PeerCredential,
    /// The bus's budget, which this connection's queued descriptors are
    /// charged against and returned to.
    quota: &'a Quota,
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
    pub fn accept(stream: UnixStream, guid: Guid<'a>, quota: &'a Quota) -> io::Result<Self> {
        let credential = sys::peer_credential(&stream)?;
        Ok(Connection {
            stream,
            shake: Handshake::new(PeerIdentity::unmapped(credential.uid), guid),
            credential,
            quota,
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
    pub fn serve(&mut self) -> Ended {
        loop {
            match self.pump() {
                Ok(true) => {}
                Ok(false) => return Ended::PeerLeft,
                Err(ended) => return ended,
            }
        }
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
            self.write_all(&fed.reply, &[])?;
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
        // Dropping them here is what this increment does with freight it cannot
        // yet route; rung 14 forwards them instead, and the queue discipline is
        // the same either way.
        let claimed: Vec<OwnedFd> = self.freight.drain(..wanted.min(self.freight.len())).collect();
        self.quota.release_fds(claimed.len());
        drop(claimed);

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
        if message.kind != message::MessageType::MethodCall
            || message.flags & message::FLAG_NO_REPLY_EXPECTED != 0
        {
            return Ok(());
        }

        let serial = message.serial;
        let mine = self.next_serial;
        // Wrapping, skipping zero: a serial is a u32 and zero is not legal.
        // No connection will reach this, but a counter that silently becomes
        // an illegal value is worse than one that says what it does.
        self.next_serial = self.next_serial.checked_add(1).unwrap_or(1);
        let reply = message::Builder::error(
            message.endian,
            "org.freedesktop.DBus.Error.NotSupported",
            serial,
        )
        .serial(mine)
        .body("s", |writer| {
            writer.string(
                "td-busd serves the handshake and the wire format; names, \
                 routing and match rules land with rung 14",
            )
        })
        .map_err(|error| Ended::Failed(format!("reply body: {error}")))?
        .encode()
        .map_err(|error| Ended::Failed(format!("reply: {error}")))?;
        self.write_all(&reply, &[])
    }

    /// Write a whole frame, attaching descriptors to the FIRST write only: a
    /// partial `sendmsg` that re-attached them would deliver each one twice.
    fn write_all(&mut self, bytes: &[u8], fds: &[RawFd]) -> Result<(), Ended> {
        if bytes.is_empty() {
            // The loop below is `while sent < bytes.len()`, so an empty frame
            // returns `Ok(())` without one `sendmsg` — and any descriptors
            // handed with it are dropped having gone nowhere. No caller does
            // this today; the guard is here so that none can start.
            return Err(Ended::Failed(format!(
                "refusing to write an empty frame carrying {} descriptors",
                fds.len()
            )));
        }
        let mut sent = 0usize;
        let mut attach = fds;
        while sent < bytes.len() {
            let rest = bytes.get(sent..).unwrap_or(&[]);
            match sys::send(&self.stream, rest, attach) {
                Ok(0) => return Err(Ended::Failed("write made no progress".into())),
                Ok(count) => {
                    sent = sent.saturating_add(count);
                    attach = &[];
                }
                Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {
                    return Err(Ended::Failed("peer closed while being written to".into()))
                }
                Err(error) => return Err(Ended::Failed(format!("write: {error}"))),
            }
        }
        Ok(())
    }
}

/// Connect to a running bus and complete the client half of `EXTERNAL`. This is
/// what `td-busd probe` does, and what §A's `ready=` line calls: a bus that has
/// bound its socket but cannot authenticate is not ready, and a readiness check
/// that only connected would not know the difference.
pub fn probe(path: &Path, uid: u32) -> Result<String, String> {
    probe_within(path, uid, PROBE_TIMEOUT)
}

/// `probe` with the wait as an argument, so a test can prove the timeout
/// exists without spending `PROBE_TIMEOUT` to do it.
fn probe_within(
    path: &Path,
    uid: u32,
    timeout: std::time::Duration,
) -> Result<String, String> {
    use std::io::{Read, Write};

    let stream = UnixStream::connect(path)
        .map_err(|error| format!("connect {}: {error}", path.display()))?;
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

    // ONE deadline for the whole answer, not one per read. A socket timeout
    // is an INACTIVITY timeout: a wedged or squatting listener that dribbles a
    // byte just inside it holds the probe for `MAX_LINE` times the timeout —
    // hours, on a check `ready=` is waiting for. The per-read timeout is
    // narrowed to what is left of the deadline as it goes.
    let deadline = std::time::Instant::now() + timeout;
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
                        if let Ok(mut connection) = Connection::accept(stream, guid, &quota) {
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
            let mut connection = Connection::accept(server, guid, &quota).expect("accept");
            let ended = connection.serve();
            let _ = tell.send(Outcome {
                ended,
                credential: connection.credential(),
                uid: connection.authenticated_uid(),
            });
        });
        (client, hear)
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
            let mut connection = Connection::accept(stream, guid, &quota).expect("connect");
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
            let accepted = Connection::accept(stream, guid, &quota);
            if let Ok(mut connection) = accepted {
                let _ = connection.serve();
            }
        });
        let refusal = probe(&path, this_uid().wrapping_add(1)).expect_err("probe passed");
        assert!(refusal.contains("refused this uid"), "{refusal}");
        fs::remove_dir_all(&dir).ok();
    }

    /// A message arriving after BEGIN is framed, decoded and answered. Routing
    /// is rung 14; being ANSWERED is this landing's job, because a caller left
    /// waiting on a serial hangs rather than fails.
    #[test]
    fn a_message_after_begin_is_decoded_and_answered() {
        let (mut client, _hear) = serving();
        let hex: String = this_uid()
            .to_string()
            .bytes()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        // The handshake and the first message in ONE write, which is what
        // libdbus does.
        let hello = message::Builder::method_call(
            crate::wire::Endian::Little,
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "Hello",
        )
        .destination("org.freedesktop.DBus")
        .serial(1)
        .encode()
        .expect("encode");
        let mut opening = format!("\0AUTH EXTERNAL {hex}\r\nBEGIN\r\n").into_bytes();
        opening.extend_from_slice(&hello);
        client.write_all(&opening).expect("write");

        let mut got = Vec::new();
        let mut chunk = [0u8; 512];
        // The OK line, then the error reply; read until a whole frame is there.
        while message::frame_len(after_ok(&got)).ok().flatten().is_none() {
            let read = client.read(&mut chunk).expect("read");
            assert_ne!(read, 0, "the bus closed without answering");
            got.extend_from_slice(&chunk[..read]);
        }
        let body = after_ok(&got);
        let (reply, _) = message::decode(body, 0).expect("decode the reply");
        assert_eq!(reply.kind, message::MessageType::Error);
        assert_eq!(
            reply.fields.error_name,
            Some("org.freedesktop.DBus.Error.NotSupported")
        );
        assert_eq!(reply.fields.reply_serial, Some(1));
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
            let accepted = Connection::accept(server, guid, &quota);
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
        let (mut client, _hear) = serving();
        let signal = message::Builder::signal(
            crate::wire::Endian::Little,
            "/org/example",
            "org.example.Thing",
            "Happened",
        )
        .serial(1)
        .encode()
        .expect("encode the signal");
        let quiet = message::Builder::method_call(
            crate::wire::Endian::Little,
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "Hello",
        )
        .destination("org.freedesktop.DBus")
        .flags(message::FLAG_NO_REPLY_EXPECTED)
        .serial(2)
        .encode()
        .expect("encode the quiet call");
        let asking = message::Builder::method_call(
            crate::wire::Endian::Little,
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "Hello",
        )
        .destination("org.freedesktop.DBus")
        .serial(3)
        .encode()
        .expect("encode the asking call");

        let mut opening = format!("\0AUTH EXTERNAL {}\r\nBEGIN\r\n", uid_hex()).into_bytes();
        opening.extend_from_slice(&signal);
        opening.extend_from_slice(&quiet);
        opening.extend_from_slice(&asking);
        client.write_all(&opening).expect("write");

        let mut got = Vec::new();
        let mut chunk = [0u8; 512];
        while message::frame_len(after_ok(&got)).ok().flatten().is_none() {
            let read = client.read(&mut chunk).expect("read");
            assert_ne!(read, 0, "the bus closed without answering");
            got.extend_from_slice(&chunk[..read]);
        }
        let body = after_ok(&got);
        let (reply, used) = message::decode(body, 0).expect("decode the reply");
        assert_eq!(reply.kind, message::MessageType::Error);
        assert_eq!(
            reply.fields.reply_serial,
            Some(3),
            "the answer belongs to the call that asked for one"
        );
        // And that is the ONLY answer: nothing follows it.
        assert_eq!(
            body.len(),
            used,
            "the bus answered a message that did not ask"
        );
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
        for serial in 1..=3u32 {
            let call = message::Builder::method_call(
                crate::wire::Endian::Little,
                "/org/freedesktop/DBus",
                Some("org.freedesktop.DBus"),
                "Hello",
            )
            .destination("org.freedesktop.DBus")
            .serial(serial)
            .encode()
            .expect("encode");
            opening.extend_from_slice(&call);
        }
        client.write_all(&opening).expect("write");

        let mut got = Vec::new();
        let mut chunk = [0u8; 1024];
        let mut seen = Vec::new();
        while seen.len() < 3 {
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
        assert_eq!(answered, vec![Some(1), Some(2), Some(3)], "wrong calls answered");
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

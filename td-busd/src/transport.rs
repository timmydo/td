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
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::auth::{Guid, Handshake, PeerIdentity, GUID_LEN};
use crate::lineage::{Caller, Identity, Instances, Named, Procfs, Reading, RealProcfs};
use crate::message;
use crate::policy;
use crate::registry::{Bus, Outbox, Overflow, Rejected, Released, Routing};
use crate::sys::{self, PeerCredential};
use crate::wire::{WireError, Writer};

/// The bus's own name, path and interface. A message addressed here is for the
/// broker itself; anything else is for another peer.
pub const BUS_NAME: &str = "org.freedesktop.DBus";
pub const BUS_PATH: &str = "/org/freedesktop/DBus";
pub const PEER_INTERFACE: &str = "org.freedesktop.DBus.Peer";
/// td's own interface and object, for the jail registration §D specifies.
///
/// Versioned in the name from the start, the way `org.freedesktop.systemd1`
/// is: this is a private protocol between two td programs, and the cost of
/// being able to run an old registrant against a new broker later is one digit
/// now. It is deliberately NOT hung off `org.freedesktop.DBus` — that
/// interface belongs to the specification, and adding private methods to it
/// would make td's extension indistinguishable from standard surface to
/// anything that introspects the bus.
pub const JAIL_INTERFACE: &str = "td.Jail1";
pub const JAIL_PATH: &str = "/td/Jail1";
/// The most bytes an instance name may carry.
///
/// It is a registry key and it reaches diagnostics, so it is bounded and
/// spelled from a closed character set rather than trusted: `open` stores it,
/// `resolve` reports it, and an unbounded string from a peer would be a way to
/// put arbitrary bytes into the broker's own log lines.
pub const MAX_INSTANCE_NAME: usize = 64;

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
/// §D maps a peer to its instance through the registered jail instance, and
/// does it from the pidfd rather than from this number — see `lineage`. The
/// QUOTA still counts per `SO_PEERCRED.pid`, which is the right trade for a
/// rate limit: the cost of a recycled number here is a miscounted share, and
/// the accept path already refuses a peer whose pid cannot be proved. So the
/// share is counted per pid: an approximation that is wrong only where one instance
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
    /// Every jail instance the broker knows, for the registration methods and
    /// for the identity below.
    instances: &'a Instances,
    /// Which jailed instance this peer belongs to, decided ONCE at accept.
    ///
    /// §D says "at accept", and the timing is the point rather than an
    /// optimisation: a lineage is a statement about a process tree at an
    /// instant, and the instant that matters is the one the kernel attached
    /// this socket to a pid. Resolving later would let an application change
    /// what it is by outliving its own ancestors.
    identity: Identity,
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
            // §D: a caller whose callee disconnects mid-call otherwise waits
            // for ever, which is the "left waiting on a serial" failure it
            // argues against everywhere else. Best effort, because this is
            // `Drop` and there is nowhere to report to: a caller that cannot
            // be told is a caller that was already leaving.
            // Serialized with every other ownership change, so a peer's
            // `NameLost` and the next holder's `NameAcquired` cannot
            // interleave with a third transition's.
            let ordering = self.bus.ordering();
            let departure = self.bus.leave(&unique);
            // Names first, and the order is immaterial: the announcements
            // and the `NoReply`s go to disjoint sets of connections, since a
            // peer waiting on a call to this one cannot also be the peer
            // inheriting a name from it without being told two separate
            // things it can read in either order.
            for handover in &departure.handovers {
                self.announce(handover);
            }
            drop(ordering);
            for call in departure.abandoned {
                // Numbered from the CALLER's stream, not this one. The sender
                // stamped below is the broker, which is one sender across the
                // whole bus, so two peers departing at the same counter value
                // would otherwise hand one caller two messages from
                // `org.freedesktop.DBus` carrying the same serial — and that
                // caller's ordinary broker replies are numbered from the same
                // place, so the collision does not need two departures.
                let serial = call.outbox.take_serial();
                let built = message::Builder::error(
                    crate::wire::Endian::Little,
                    "org.freedesktop.DBus.Error.NoReply",
                    call.serial,
                )
                .sender(BUS_NAME)
                .destination(&call.caller)
                .serial(serial)
                .body("s", |writer| {
                    writer.string("the peer this call was sent to disconnected")
                });
                if let Ok(frame) = built.map_err(|_| ()).and_then(|built| {
                    built.encode().map_err(|_| ())
                }) {
                    // Through `deliver`, so §D's bus remedy runs here too. A
                    // draft pushed straight to the outbox, which is the same
                    // mistake `deliver` is documented against: during a mass
                    // disconnect a full bus would bin every `NoReply` in
                    // silence and never relieve the peer responsible.
                    let _ = self.deliver(&call.outbox, frame);
                }
            }
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
        instances: &'a Instances,
    ) -> io::Result<Self> {
        let credential = sys::peer_credential(&stream)?;
        let guid_text = guid.as_str();
        // Before the handshake, because this is the kernel's account of the
        // peer and the handshake is the peer's account of itself. It is taken
        // once, here, so that every later answer is the one that was true at
        // accept rather than one taken whenever it was first needed: a
        // lineage is a statement about a process tree at an instant, and
        // re-walking it later would answer about a tree that has moved on.
        //
        // The pidfd rather than `credential.pid`: the credential carries a
        // NUMBER sampled at connect, and a peer that has since been reaped can
        // have had that number handed to somebody else. `lineage` says why the
        // handle answers it and the number cannot.
        //
        // A pidfd this kernel will not give is an identity this broker cannot
        // establish, so it is `Unknown` — which `policy` denies every peer to
        // — rather than an accept failure. The connection is still SERVED
        // because a denial a peer can read beats a socket that closed without
        // saying why; it is not served because it is trusted. Below Linux 6.5
        // no peer can be identified at all, so on such a host every
        // connection lands here and this bus routes nothing between peers.
        // That is fail-closed working as intended rather than a fallback, and
        // it is why the image pins 7.x.
        let identity = match sys::peer_pidfd(&stream) {
            // Dropped at the end of this expression, and deliberately: the
            // answer is taken once, here, and never recomputed, so holding one
            // descriptor per connection would spend an fd on a question
            // nothing asks again.
            Ok(pidfd) => instances.resolve(&RealProcfs, pidfd.as_raw_fd()),
            Err(why) => Self::unidentifiable(&why),
        };
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
            instances,
            identity,
            outbox,
            unique: None,
            chunk: vec![0u8; READ_CHUNK],
            inbox: Vec::new(),
            freight: Vec::new(),
            frame: Vec::new(),
        })
    }

    /// Which process is on this connection, proved rather than sampled.
    ///
    /// **What the pidfd buys is liveness, not a different number**, and a
    /// review corrected an earlier draft of this comment that implied
    /// otherwise. `SO_PEERCRED` and `SO_PEERPIDFD` render the same
    /// `sk->sk_peer_pid`, so whenever `named_by` answers a pid at all it is
    /// the pid `credential.pid` already held. The difference is the question
    /// each can answer: the socket's number is a value the kernel sampled at
    /// `connect(2)` and will keep reporting after the process behind it has
    /// been reaped and the allocator has handed the number on, at which point
    /// a registry keyed on it attributes the registration to whoever holds it
    /// now. The descriptor answers whether that has happened. So this is
    /// `credential.pid` plus the kernel's word that it has not been freed --
    /// and the word is the whole of the value.
    ///
    /// Taken fresh at each registry call rather than kept from accept. Kept,
    /// it would be one descriptor per connection held for the whole of its
    /// life to answer a question two methods ask at most twice per launch --
    /// and `Complete` arrives on a DIFFERENT connection from `Register`
    /// anyway, because td-jail closes every descriptor above stderr between
    /// the two, so a handle cached at accept would not span the pair that
    /// needs it.
    ///
    /// `None` means the peer could not be identified, and both callers refuse
    /// on it. What this proves is exactly one thing: the pair names a process
    /// that is this connection's peer and has NOT BEEN REAPED, so its number
    /// is still exclusively its own. Not "is running" -- a zombie's `fdinfo`
    /// still reports its pid, which is right, because an unreaped pid is an
    /// unavailable pid and availability is the only property at issue.
    /// Proving the two PHASES are the same process is the registry's job, and
    /// it uses the start time.
    ///
    /// `Unreadable` is `None` as firmly as `Reaped` is, and that arm is why
    /// this takes an injected `/proc` rather than reaching for `RealProcfs`
    /// itself. It means the broker could not read the descriptor's `fdinfo` —
    /// `EMFILE`, or a format it does not recognise — which is precisely the
    /// state in which falling back to the sampled number would be worst. No
    /// live kernel can be made to produce it on demand, so a version that
    /// answered `Some(self.credential.pid)` there survived the whole suite
    /// until the seam was here.
    ///
    /// The `/proc` read is BRACKETED by two reads of the pidfd, exactly as
    /// `resolve`'s walk is, and a review found the version without it. One
    /// read is not enough: the peer can be reaped and its number reused
    /// between the pidfd read and the `stat`, and phase one would then record
    /// the impostor's start time — while the peer that transferred its socket
    /// away before dying still has a controller on the other end to take
    /// delivery of the token. Both later checks would agree with each other
    /// about a process that was never this connection's peer. With the second
    /// read the argument is the walk's: if the pidfd names the same pid
    /// afterwards, the peer was never reaped in between, so its number was
    /// never free, so the `/proc` entry read while it ran was the peer's.
    ///
    /// The descriptor is held across both reads. Dropping it in between would
    /// return its NUMBER to this process's descriptor table, and the second
    /// read could then be of something else entirely.
    fn caller(&self, procfs: &dyn Procfs) -> Option<Caller> {
        let pidfd = sys::peer_pidfd(&self.stream).ok()?;
        let Named::Pid(pid) = procfs.named_by(pidfd.as_raw_fd()) else {
            return None;
        };
        let Reading::Of(stat) = procfs.stat(pid) else {
            return None;
        };
        match procfs.named_by(pidfd.as_raw_fd()) {
            Named::Pid(again) if again == pid => Some(Caller {
                pid,
                starttime: stat.starttime,
            }),
            _ => None,
        }
    }

    /// A peer whose pidfd the kernel would not give.
    ///
    /// Split out of `accept` so the arm can be redded at all: every kernel td
    /// runs on answers `SO_PEERPIDFD`, so nothing a test can do reaches this
    /// branch through the real call, and a mutation of it to `Unconfined` —
    /// which is the whole failure this design exists to prevent — survived the
    /// entire suite before the seam was here.
    fn unidentifiable(why: &io::Error) -> Identity {
        Identity::Unknown(format!("the peer's pidfd could not be taken: {why}"))
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
    /// `NO_REPLY_EXPECTED` is a strange thing for a client to send, but it
    /// changes state, so it still earns the name and simply is not answered —
    /// a draft answered it regardless, which made this the single method that
    /// ignored the flag. The `td.Jail1` pair follows the same rule from the
    /// other side of `bus_method`'s guard.
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
            .publish(
                &unique,
                &self.outbox,
                self.credential.uid,
                self.credential.pid,
                self.identity.app_id().map(str::to_string),
            )
            .map_err(|why| Ended::Failed(format!("cannot name a peer: {why}")))?;
        // A unique name is a name this connection now OWNS, and the reference
        // daemon announces it like any other. The `Hello` reply carries the
        // same fact, so this is compatibility rather than news: a client that
        // waits for `NameAcquired` before it considers itself connected would
        // otherwise wait for ever. After `publish`, because the announcement
        // is routed through the directory to reach this connection's outbox.
        self.announce(&crate::registry::Handover {
            name: unique.clone(),
            lost: None,
            gained: Some(unique),
        });
        Ok(())
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
        // td's own interface, at td's own object. It is NOT on
        // `org.freedesktop.DBus`: that interface is the specification's, and a
        // broker that hangs private methods off it is inventing standard
        // surface. A separate object keeps the rule that §D's "the broker's
        // own methods are at the broker's own object" states — each interface
        // answers where it lives, and nothing else.
        //
        // These two sit ABOVE the no-reply guard, which a draft did not do.
        // `NO_REPLY_EXPECTED` means "do not send me the reply", not "do not do
        // the work", and together with `Hello` these are the only bus methods
        // that change anything. A `Complete` dropped for want of a reply would
        // leave a jailed application with no registration on record, which
        // resolves `Unconfined` — the one answer §E exists to prevent.
        //
        // Which makes the TYPE guard explicit rather than incidental. The
        // no-reply guard was doing that job by accident: `wants_reply` is
        // false for every message type that is not a method call, so nothing
        // below it could ever run for a signal. Moving these two above it took
        // them out from behind that, and a review found a SIGNAL named
        // `Register` at this path registering an instance. `say_hello`'s
        // caller has always tested the kind explicitly; these two now do too.
        let is_call = message.kind == message::MessageType::MethodCall;
        let jail_method = matches!(member, "Register" | "Complete")
            && is_call
            && on_the_jail_object(message)
            && on(JAIL_INTERFACE);
        if jail_method {
            // The one live `AccessDenied` on this bus. Registration is
            // authenticated by uid and in v1 every session peer shares one,
            // so a confined application reaching this interface could name
            // its own instance and app id — the record every later answer
            // about it is derived from. Denied rather than reported absent,
            // which is the opposite of the rule for names: the interface is
            // not a secret, and a peer that may not use it is better told so
            // than left calling a method that seems not to exist.
            if !policy::may_register(&self.identity) {
                return self.refuse_if_wanted(
                    message,
                    "org.freedesktop.DBus.Error.AccessDenied",
                    "td.Jail1 is for callers this broker has placed outside a jail",
                    wants_reply,
                );
            }
            return match member {
                "Register" => self.jail_register(message, wants_reply),
                "Complete" => self.jail_complete(message, wants_reply),
                // Unreachable through `jail_method`, which names the same two.
                // Spelled out anyway: a third member added to that list would
                // otherwise arrive here and be COMPLETED, twenty lines away
                // from the edit that caused it.
                _ => Ok(()),
            };
        }
        // `RequestName` and `ReleaseName` CHANGE something, so they belong up
        // here with the jail methods rather than below the gate: a caller that
        // set `NO_REPLY_EXPECTED` has withdrawn the REPLY, not the work, and a
        // draft that left them below silently did nothing at all for it.
        let name_method =
            matches!(member, "RequestName" | "ReleaseName") && is_call && here && on(BUS_NAME);
        if name_method {
            return match member {
                "RequestName" => self.acquire_name(message, wants_reply),
                "ReleaseName" => self.give_up_name(message, wants_reply),
                // Unreachable through `name_method`, which names the same two,
                // and spelled out for the reason the jail dispatch above is.
                _ => Ok(()),
            };
        }
        // Everything below is a question rather than a change, so a peer that
        // is not waiting for an answer simply gets none.
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
                // One lock, not two: a connection joining or a name
                // changing hands between two acquisitions would produce a
                // list that never described the bus at any instant.
                let mut names = vec![BUS_NAME.to_string()];
                names.extend(self.bus.all_names());
                names.retain(|name| self.may_see(name));
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
                let held = self.may_see(&asked)
                    && (asked == BUS_NAME || self.bus.owner_of(&asked).is_some());
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
                //
                // The answer is the OWNER's unique name, which for a unique
                // name is itself and for a well-known one is whoever holds
                // it. That is the whole use of the method: a client resolves
                // a name once and then matches the sender of what arrives.
                //
                // The policy is asked BEFORE the directory, as everywhere
                // else here: `route` gives the reason — deciding after the
                // lookup leaves the refusal's TIMING dependent on the fact
                // the refusal exists to withhold — and a draft of this arm
                // resolved first and filtered the result.
                let owner = if !self.may_see(&asked) {
                    None
                } else if asked == BUS_NAME {
                    Some(asked.clone())
                } else {
                    self.bus.owner_of(&asked)
                };
                if let Some(owner) = owner {
                    self.answer(message, "s", move |writer| writer.string(&owner))
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
                match self.about(&asked) {
                    Some((uid, pid, app_id)) => {
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
                                // §D's td-owned extension, present only for a
                                // peer whose lineage PROVED an instance. An
                                // `Unconfined` or `Unknown` peer has no entry
                                // rather than an empty one: this key means
                                // "the broker established that this
                                // connection is this application", and a
                                // sentinel would invite a reader to treat a
                                // failure to establish that as an answer.
                                if let Some(app_id) = app_id {
                                    array.dict_entry(|entry| {
                                        entry.string("td.AppId")?;
                                        entry.variant("s", |value| value.string(&app_id))
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
                "td-busd serves Hello, the name and credential lookups, \
                 directed routing and td.Jail1 registration; the rest of \
                 org.freedesktop.DBus lands with match rules",
            ),
        }
    }

    /// Phase one: `Register(s instance, s app_id, as services) -> s token`.
    ///
    /// Called by stage 0 before it unshares anything, because the pid the
    /// record needs does not exist yet. §D is explicit that this is
    /// authenticated by uid and that in v1 every session peer is uid 1000 — so
    /// the app id is a string the registrant supplies, the walk is sound about
    /// WHICH instance a connection belongs to and says nothing about whether
    /// that instance is what it calls itself, and per-app uids are the fix.
    /// That is recorded in §D rather than papered over here.
    fn jail_register(
        &mut self,
        message: &message::Message<'_>,
        wants_reply: bool,
    ) -> Result<(), Ended> {
        let Some((instance, app_id, services)) =
            self.registration_arguments(message, wants_reply)?
        else {
            return Ok(());
        };
        let Some(registrant) = self.caller(&RealProcfs) else {
            return self.refuse_if_wanted(
                message,
                "td.Jail1.Error.Refused",
                "the registering process could not be identified",
                wants_reply,
            );
        };
        match self.instances.open(
            &RealProcfs,
            &instance,
            &app_id,
            services,
            self.credential.uid,
            registrant,
        ) {
            Ok(token) if wants_reply => {
                self.answer(message, "s", move |writer| writer.string(&token))
            }
            // A registrant that asked for no reply has still registered; it
            // simply cannot use what it did not wait for.
            Ok(_) => Ok(()),
            Err(why) => self.refuse_if_wanted(
                message,
                "td.Jail1.Error.Refused",
                &why,
                wants_reply,
            ),
        }
    }

    /// Phase two: `Complete(s token, u pid) -> ()`.
    ///
    /// The start time is not an argument. It is the field every later reuse
    /// check rests on, so the broker reads it out of `/proc` for itself rather
    /// than accepting the one number of the record that the registrant would
    /// otherwise choose.
    fn jail_complete(
        &mut self,
        message: &message::Message<'_>,
        wants_reply: bool,
    ) -> Result<(), Ended> {
        let Some((token, pid)) = self.completion_arguments(message, wants_reply)? else {
            return Ok(());
        };
        // The completing connection's own caller is an argument the registry
        // needs and the caller cannot supply: it is what makes "a registrant
        // may bind its own child and nothing else" checkable. Proved rather
        // than sampled, and this is the only reader of the completer's
        // `/proc` entry now -- the registry stopped reading it, because a read
        // outside the pidfd bracket cannot say whose entry it read. A
        // completer already REAPED (not merely exited: a zombie still has an
        // entry and an unavailable number) is refused here.
        let Some(completer) = self.caller(&RealProcfs) else {
            return self.refuse_if_wanted(
                message,
                "td.Jail1.Error.Refused",
                "the completing process could not be identified",
                wants_reply,
            );
        };
        match self.instances.complete(
            &RealProcfs,
            &token,
            pid,
            self.credential.uid,
            completer,
        ) {
            Ok(()) if wants_reply => self.answer(message, "", |_| Ok(())),
            Ok(()) => Ok(()),
            Err(why) => self.refuse_if_wanted(
                message,
                "td.Jail1.Error.Refused",
                &why,
                wants_reply,
            ),
        }
    }

    /// The kernel's word on whoever owns `name`, or `None` for a name that is
    /// not here. The broker's own name is answered from this process, which is
    /// the truthful answer: `org.freedesktop.DBus` IS td-busd.
    fn credentials_for(&self, name: &str) -> Option<(u32, i32)> {
        // Gated here rather than at each of the three callers: §D singles
        // these out because another instance's host pid is both an identifier
        // for `/proc` spelunking outside the jail and the input to the
        // lineage walk this broker's identity story rests on. `None` is the
        // same answer the callers already give for a name that is not here,
        // which is what makes an invisible peer indistinguishable from an
        // absent one.
        if !self.may_see(name) {
            return None;
        }
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

    /// Everything `GetConnectionCredentials` reports, gated as
    /// `credentials_for` is and taken under ONE lock.
    ///
    /// Two lookups would let the name change hands between them and report
    /// one peer's uid and pid beside another peer's application id, which is
    /// the claim `td.AppId` exists to make truthfully.
    fn about(&self, name: &str) -> Option<(u32, i32, Option<String>)> {
        if !self.may_see(name) {
            return None;
        }
        if name == BUS_NAME {
            // The broker has no application id of its own; its uid and pid
            // come from the same place `credentials_for` reads them.
            let (uid, pid) = self.credentials_for(name)?;
            return Some((uid, pid, None));
        }
        self.bus.about(name)
    }

    /// Whether this caller may learn that `name` exists.
    ///
    /// Every answer the broker gives about a name goes through here, so a
    /// name a caller may not see is absent in the same way whichever way it
    /// asks.
    fn may_see(&self, name: &str) -> bool {
        policy::may_see(&self.identity, self.unique.as_deref(), name)
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

    /// `takes`, for a method that runs whether or not its caller is waiting.
    ///
    /// The check is the same; only the complaint is conditional. A caller that
    /// set `NO_REPLY_EXPECTED` gets no `InvalidArgs` — sending one would be the
    /// unsolicited reply the flag forbids — but its message is still graded,
    /// because the answer decides whether the state change below happens.
    fn takes_if_wanted(
        &mut self,
        message: &message::Message<'_>,
        signature: &str,
        wants_reply: bool,
    ) -> Result<bool, Ended> {
        if !wants_reply {
            return Ok(message.fields.signature.unwrap_or("") == signature);
        }
        self.takes(message, signature)
    }

    /// `refuse`, suppressed for a caller that said it does not want a reply.
    fn refuse_if_wanted(
        &mut self,
        message: &message::Message<'_>,
        name: &str,
        text: &str,
        wants_reply: bool,
    ) -> Result<(), Ended> {
        if !wants_reply {
            return Ok(());
        }
        self.refuse(message, name, text)
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

    /// §D's `RequestName`, with the specification's three flags and four
    /// answers.
    ///
    /// The whole operation is serialized against every other ownership
    /// change on this bus, announcements included. Without that, two peers
    /// racing for one name can have their directed signals interleave with
    /// somebody else's: B replaces A, C replaces B, and if C's `NameLost` for
    /// B is queued before B's own `NameAcquired`, B is told it lost the name
    /// and then that it holds it, while C is the holder. The signals are a
    /// state machine, so their ORDER is the state.
    fn acquire_name(
        &mut self,
        message: &message::Message<'_>,
        wants_reply: bool,
    ) -> Result<(), Ended> {
        let Some((asked, flags)) = self.name_and_flags(message, wants_reply)? else {
            return Ok(());
        };
        if !policy::may_own(&self.identity, &asked) {
            return self.refuse_if_wanted(
                message,
                "org.freedesktop.DBus.Error.AccessDenied",
                if policy::is_reserved_name(&asked) {
                    "that name is reserved for this broker and the portal"
                } else {
                    "this connection may own no name on this bus"
                },
                wants_reply,
            );
        }
        let mine = self.named()?.to_string();
        let ordering = self.bus.ordering();
        let (outcome, handover) = self.bus.request_name(&mine, &asked, flags);
        // The news BEFORE the answer, which is the reference daemon's order
        // and not `say_hello`'s. `say_hello` puts its reply first because the
        // peer has no name until it arrives, so anything ahead of it reaches a
        // connection that does not yet know who it is; here the connection is
        // established and the question is only which of two of the broker's
        // own messages a client library sees first. Matching `dbus-daemon` is
        // worth more than an argument of our own on a point where clients are
        // tested against it.
        //
        // Neither order closes the window this leaves: the name is routable
        // the moment `request_name` returns, so another peer can land a call
        // for it ahead of both. That is the shape `say_hello` was restructured
        // to remove, and it is much less serious here — the recipient has a
        // name, knows it, and a client library queues an incoming call while
        // it waits for a reply — but it is a deviation and not a claim to the
        // contrary.
        if let Some(handover) = handover {
            self.announce(&handover);
        }
        let code = outcome.code();
        if wants_reply {
            self.answer(message, "u", move |writer| {
                writer.uint32(code);
                Ok(())
            })?;
        }
        drop(ordering);
        Ok(())
    }

    /// §D's `ReleaseName`, serialized with the rest for the same reason.
    fn give_up_name(
        &mut self,
        message: &message::Message<'_>,
        wants_reply: bool,
    ) -> Result<(), Ended> {
        let Some(asked) = self.well_known_argument(message, wants_reply)? else {
            return Ok(());
        };
        let mine = self.named()?.to_string();
        let ordering = self.bus.ordering();
        let (outcome, handover) = self.bus.release_name(&mine, &asked);
        // `NON_EXISTENT` and `NOT_OWNER` are a two-valued oracle for "does
        // anybody hold this name", and without this any peer could ask it
        // about any name — straight past the filter the last two landings
        // built, and past this file's own rule that a name the caller may not
        // see is reported ABSENT rather than as an error confirming it
        // exists. Answered as nobody's, which is indistinguishable from the
        // truth for a name this caller may not see.
        //
        // Only the refusal is rewritten. A caller that HELD the name or was
        // queued for it gets `Done` either way, so nothing legitimate is
        // touched: a peer cannot be holding a name it may not see without
        // this broker having granted it one.
        let outcome = match outcome {
            Released::NotOwner if !self.may_see(&asked) => Released::NonExistent,
            outcome => outcome,
        };
        let code = outcome.code();
        if let Some(handover) = handover {
            self.announce(&handover);
        }
        if wants_reply {
            self.answer(message, "u", move |writer| {
                writer.uint32(code);
                Ok(())
            })?;
        }
        drop(ordering);
        Ok(())
    }

    /// A well-known name argument, validated, or a refusal already sent.
    ///
    /// Well-known rather than any bus name: a unique name is the broker's to
    /// hand out and never a peer's to ask for, so a request for one is
    /// refused as an argument rather than answered as a name nobody holds.
    fn well_known_argument(
        &mut self,
        message: &message::Message<'_>,
        wants_reply: bool,
    ) -> Result<Option<String>, Ended> {
        if !self.takes_if_wanted(message, "s", wants_reply)? {
            return Ok(None);
        }
        match message.args().first().and_then(crate::wire::Value::as_str) {
            Some(text) if crate::name::valid_well_known_name(text) => {
                Ok(Some(text.to_string()))
            }
            _ => {
                self.refuse_if_wanted(
                    message,
                    "org.freedesktop.DBus.Error.InvalidArgs",
                    "that is not a well-known bus name",
                    wants_reply,
                )?;
                Ok(None)
            }
        }
    }

    /// `RequestName`'s name and flags, validated, or a refusal already sent.
    ///
    /// Unknown flag bits are IGNORED rather than refused, which is what the
    /// specification says and what a client library built against a later
    /// version needs: a bit this broker does not implement is a request it
    /// does not honour, not a message it cannot read.
    fn name_and_flags(
        &mut self,
        message: &message::Message<'_>,
        wants_reply: bool,
    ) -> Result<Option<(String, u32)>, Ended> {
        if !self.takes_if_wanted(message, "su", wants_reply)? {
            return Ok(None);
        }
        let args = message.args();
        let name = args.first().and_then(crate::wire::Value::as_str);
        let flags = args.get(1).and_then(crate::wire::Value::as_u32);
        let (Some(name), Some(flags)) = (name, flags) else {
            self.refuse_if_wanted(
                message,
                "org.freedesktop.DBus.Error.InvalidArgs",
                "RequestName takes a name and flags",
                wants_reply,
            )?;
            return Ok(None);
        };
        if !crate::name::valid_well_known_name(name) {
            self.refuse_if_wanted(
                message,
                "org.freedesktop.DBus.Error.InvalidArgs",
                "that is not a well-known bus name",
                wants_reply,
            )?;
            return Ok(None);
        }
        Ok(Some((name.to_string(), flags)))
    }

    /// `Register`'s three arguments, validated, or a refusal already sent.
    ///
    /// Every one of them is checked here rather than in the registry, for the
    /// reason every other wire argument in this file is: the registry's job is
    /// to say what may be registered, and the wire's job is to make sure what
    /// reached it is the shape it claims. A registry that also had to defend
    /// against malformed strings would be two graders for one rule.
    fn registration_arguments(
        &mut self,
        message: &message::Message<'_>,
        wants_reply: bool,
    ) -> Result<Option<(String, String, Vec<String>)>, Ended> {
        if !self.takes_if_wanted(message, "ssas", wants_reply)? {
            return Ok(None);
        }
        let args = message.args();
        let instance = args.first().and_then(crate::wire::Value::as_str);
        let app_id = args.get(1).and_then(crate::wire::Value::as_str);
        let names = args.get(2).and_then(|value| value.as_seq());
        let (Some(instance), Some(app_id), Some(names)) = (instance, app_id, names) else {
            self.refuse_if_wanted(
                message,
                "org.freedesktop.DBus.Error.InvalidArgs",
                "Register takes an instance name, an application id and a list \
                 of service names",
                wants_reply,
            )?;
            return Ok(None);
        };
        if !valid_instance_name(instance) {
            self.refuse_if_wanted(
                message,
                "org.freedesktop.DBus.Error.InvalidArgs",
                "that is not an instance name",
                wants_reply,
            )?;
            return Ok(None);
        }
        if !valid_application_id(app_id) {
            self.refuse_if_wanted(
                message,
                "org.freedesktop.DBus.Error.InvalidArgs",
                "that is not an application id",
                wants_reply,
            )?;
            return Ok(None);
        }
        let Ok(values) = names.values(crate::lineage::MAX_SERVICES) else {
            self.refuse_if_wanted(
                message,
                "org.freedesktop.DBus.Error.InvalidArgs",
                "that service list cannot be read",
                wants_reply,
            )?;
            return Ok(None);
        };
        let mut services = Vec::with_capacity(values.len());
        for value in &values {
            match value.as_str() {
                // A WELL-KNOWN name. A predeclared service is a name the
                // instance intends to own, and a unique name — `:1.7` — is
                // the broker's to hand out and nobody's to claim, so
                // `valid_bus_name` was the wrong grader here too.
                Some(name) if crate::name::valid_well_known_name(name) => {
                    services.push(name.to_string());
                }
                _ => {
                    self.refuse_if_wanted(
                        message,
                        "org.freedesktop.DBus.Error.InvalidArgs",
                        "a predeclared service must be a bus name",
                        wants_reply,
                    )?;
                    return Ok(None);
                }
            }
        }
        Ok(Some((instance.to_string(), app_id.to_string(), services)))
    }

    /// `Complete`'s two arguments, validated, or a refusal already sent.
    fn completion_arguments(
        &mut self,
        message: &message::Message<'_>,
        wants_reply: bool,
    ) -> Result<Option<(String, i32)>, Ended> {
        if !self.takes_if_wanted(message, "su", wants_reply)? {
            return Ok(None);
        }
        let args = message.args();
        let token = args.first().and_then(crate::wire::Value::as_str);
        let pid = args.get(1).and_then(crate::wire::Value::as_u32);
        let (Some(token), Some(pid)) = (token, pid) else {
            self.refuse_if_wanted(
                message,
                "org.freedesktop.DBus.Error.InvalidArgs",
                "Complete takes a token and a pid",
                wants_reply,
            )?;
            return Ok(None);
        };
        // `u` on the wire and `i32` in `/proc`: a pid that does not survive the
        // conversion is not a pid this kernel ever issued, and refusing it here
        // keeps the registry from having to have an opinion about it.
        let Ok(pid) = i32::try_from(pid) else {
            self.refuse_if_wanted(
                message,
                "org.freedesktop.DBus.Error.InvalidArgs",
                "that is not a pid",
                wants_reply,
            )?;
            return Ok(None);
        };
        Ok(Some((token.to_string(), pid)))
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

    /// Tell the connections a name changed hands about.
    ///
    /// `NameAcquired` and `NameLost` are DIRECTED signals -- each goes to the
    /// one connection it is about -- so they need no subscription and no match
    /// rule. `NameOwnerChanged` is the broadcast form of the same news and
    /// waits for match rules to land; §D filters it per caller, which is a
    /// subscription-shaped version of the `see` question and belongs with the
    /// machinery that answers it.
    ///
    /// Best effort, and the cost of that is worth stating: these are
    /// STATE-MACHINE signals, not news. A peer that misses `NameLost` goes on
    /// believing it holds a name that now routes elsewhere. There is nothing
    /// better available — this runs from `Drop` as well, so there is nowhere
    /// to report to, and disconnecting a recipient for being behind is the
    /// thing §D's budget remedy exists to avoid, since it did not choose when
    /// this fired. So it is LOGGED: a broker-level condition, apart from an
    /// ordinary refusal, because reaching it means a peer's view of the bus
    /// and the bus's view of it have parted.
    fn announce(&self, handover: &crate::registry::Handover) {
        for (unique, member) in [
            (handover.lost.as_deref(), "NameLost"),
            (handover.gained.as_deref(), "NameAcquired"),
        ] {
            let Some(unique) = unique else {
                continue;
            };
            let Some(outbox) = self.bus.route(unique) else {
                continue;
            };
            let serial = outbox.take_serial();
            let built = message::Builder::signal(
                crate::wire::Endian::Little,
                BUS_PATH,
                BUS_NAME,
                member,
            )
            .sender(BUS_NAME)
            .destination(unique)
            .serial(serial)
            .body("s", |writer| writer.string(&handover.name));
            let Ok(frame) = built
                .map_err(|_| ())
                .and_then(|built| built.encode().map_err(|_| ()))
            else {
                eprintln!(
                    "td-busd: cannot build {member} for {unique}: {} is unsayable",
                    handover.name
                );
                continue;
            };
            if self.deliver(&outbox, frame).is_err() {
                eprintln!(
                    "td-busd: {unique} did not get {member} for {}; \
                     its view of what it owns is now behind this bus's",
                    handover.name
                );
            }
        }
    }

    /// Un-record a call this connection recorded and then could not send.
    ///
    /// Nameless is not a case: a connection cannot route anything before
    /// `Hello`, so nothing it recorded can be waiting when `self.unique` is
    /// `None`.
    fn forget(&self, recorded: bool, serial: u32) {
        if !recorded {
            return;
        }
        if let Some(caller) = self.unique.as_deref() {
            self.bus.forget_reply(caller, serial);
        }
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
        // A destination this caller may not reach is reported ABSENT, not
        // denied, and that is deliberate. §D asks for `AccessDenied` for what
        // the default policy does not permit, and separately that a name the
        // caller may not see is reported as absent "rather than as an error
        // that confirms it exists". For a DIRECTED SEND the two rules meet,
        // and the second one governs: a sandboxed peer's talk set and see set
        // are the same set, so anything it may not send to is also something
        // it may not know is there, and answering `AccessDenied` would
        // announce the peer it was refused. `AccessDenied` is reached where a
        // caller may see the interface and still not use it, which is
        // `td.Jail1`.
        //
        // Asked BEFORE the directory: `route` walks the peer list and stops
        // early when it finds the name, so deciding after it would leave the
        // refusal's TIMING dependent on the fact the refusal exists to
        // withhold. The lookups above short-circuit for the same reason.
        //
        // A REPLY is not filtered by the TALK SET, and that is the
        // difference between a policy on who may be addressed and one on what
        // may be sent. A method return or an error is addressed by
        // `reply_serial` to a caller that already reached this connection, so
        // filtering it by the sender's talk set drops the answer to a call the
        // broker itself delivered — the caller times out and the callee is
        // told nothing. §D grants a sandbox the portal's REPLIES, so the
        // symmetric direction cannot be a denial.
        //
        // It is filtered by OWNERSHIP instead, which is the stricter test: a
        // reply is carried only if this connection is the one that call was
        // routed to. §D wants that for the integrity half — libdbus and GDBus
        // both match a pending call by serial without checking who answered,
        // so without this any peer can answer a call another peer made to a
        // third, and the client library hands it over as the answer.
        let reply = match message.kind {
            message::MessageType::MethodReturn | message::MessageType::Error => {
                message.fields.reply_serial
            }
            _ => None,
        };
        let permitted = match reply {
            // A method return or an error with no `reply_serial` answers
            // nothing, and the codec refuses one on those two types, so this
            // arm is dead. It falls through to the rules below rather than
            // being refused here: writing a second refusal for a case the
            // decoder has already taken would be a branch shaped like a check
            // that never runs, and `restamp` rejects the message afterwards
            // in any event.
            Some(serial) => self
                .bus
                .claim_reply(self.named()?, destination, serial),
            None => {
                // Who may be addressed, and then what may be sent to them. A
                // confined peer may CALL the portal; a directed signal at it
                // is a channel §D does not grant.
                policy::may_talk(&self.identity, self.unique.as_deref(), destination)
                    && (message.kind != message::MessageType::Signal
                        || policy::may_signal(&self.identity))
            }
        };
        // Recorded BEFORE the message is queued, so the answer cannot reach
        // a table that does not know the question yet: the callee's reader is
        // a different thread and is free to run the moment the frame lands in
        // its outbox.
        //
        // Resolved and recorded as ONE act, for the symmetric reason. A
        // lookup here and a record afterwards leave a window in which the
        // callee departs: its `leave` sweeps a table that does not mention
        // this call yet, the record lands behind the sweep, and the caller
        // waits for ever on a connection that has gone.
        //
        // Only a call that WANTS a reply is recorded, because only that has a
        // serial anybody is waiting on. `NO_REPLY_EXPECTED` says the caller
        // is not listening, so a "reply" to one is forged by construction and
        // the ownership test above refuses it without a rule of its own.
        // `wants_reply` alone: `dispatch` sets it only for a method call
        // that did not withdraw its reply, and states so once rather than
        // leaving three places to each happen to drop it. A second test here
        // would be a dead branch shaped like a safety check, which this file
        // argues is worse than none because the next reader trusts it.
        let recorded = wants_reply;
        let found = if !permitted {
            None
        } else if recorded {
            match self
                .bus
                .route_expecting(destination, self.named()?, message.serial)
            {
                Routing::Ready(outbox) => Some(outbox),
                Routing::Absent => None,
                Routing::TooMany => {
                    return self.refuse(
                        message,
                        "org.freedesktop.DBus.Error.LimitsExceeded",
                        "this connection has too many calls outstanding",
                    )
                }
                // The specification already requires a serial to be unique
                // among a connection's undelivered messages. The table needs
                // it too: with a duplicate allowed, two answers carry one
                // `reply_serial` to a client that cannot tell them apart.
                Routing::Repeated => {
                    return self.refuse(
                        message,
                        "org.freedesktop.DBus.Error.InvalidArgs",
                        "this connection already has a call outstanding on that serial",
                    )
                }
            }
        } else {
            self.bus.route(destination)
        };
        let Some(outbox) = found else {
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
                self.forget(recorded, message.serial);
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
                self.forget(recorded, message.serial);
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
                self.forget(recorded, message.serial);
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
                self.forget(recorded, message.serial);
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
    /// The serial for a message the broker ORIGINATES to this peer.
    ///
    /// Kept on the outbox rather than here, because the sender it numbers for
    /// is `org.freedesktop.DBus` and the stream those numbers must stay
    /// distinct in is the recipient's. A departing peer writes a `NoReply`
    /// into a caller's outbox from its own thread, so a counter kept per
    /// connection would number two of the broker's messages to one caller
    /// from two different places.
    fn take_serial(&self) -> u32 {
        self.outbox.take_serial()
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

fn on_the_jail_object(message: &message::Message<'_>) -> bool {
    message.fields.path == Some(JAIL_PATH)
}

/// The most bytes in a td application identity.
const MAX_APPLICATION_ID: usize = 32;

/// An application identity, in the language §D's identity section defines.
///
/// NOT a bus name, which a draft used and a review caught. A td application's
/// identity is a short flat name — `firefox`, `darktable` — and reverse DNS is
/// an alias for wires td does not own, not the identity. `valid_bus_name` is
/// wrong in both directions here: it refuses every real td identity, because
/// a bus name needs a `.` and these do not have one, and it accepts `:1.7`,
/// which is a unique connection name the broker hands out. td-jail carries
/// this same grammar in `validate_application_name`; the crates are separate
/// dependency-free locks, so this is a second copy of one normative rule
/// rather than a second rule.
fn valid_application_id(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_APPLICATION_ID
        && !name.starts_with('-')
        && name != "."
        && !name.contains("..")
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
}

/// An instance name: non-empty, bounded, and drawn from a closed set.
///
/// Not a bus name, because it is not one — it names a launch rather than
/// something ownable, and borrowing that grammar would imply it could be
/// owned. It is the application-id language with a longer ceiling and no
/// leading-dash rule, for the ordinary reason: this string is compared,
/// logged and reported, and a name that can look like a path invites
/// somebody later to treat it as one. `/` is excluded by the character set;
/// `..` anywhere and a bare `.` are excluded by name, which is broader than
/// "a `.` run and nothing else" and deliberately so — `a..b` is refused.
fn valid_instance_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_INSTANCE_NAME
        && name != "."
        && !name.contains("..")
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
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
                        let instances = Instances::new();
                        let accepted =
                            Connection::accept(stream, guid, &quota, &bus, &instances);
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
    use crate::lineage::Procfs;
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::sync::mpsc;
    use std::thread;

    const GUID: &str = "00112233445566778899aabbccddeeff";

    /// The ignored helper's full test path, and the variable that tells it
    /// where to connect.
    const REAPED_HELPER: &str = "transport::tests::connects_to_the_socket_named_in_the_environment";
    const REAPED_SOCKET: &str = "TD_BUSD_TEST_REAPED_SOCKET";

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

    /// Whether this kernel answers `SO_PEERPIDFD` at all.
    ///
    /// Below Linux 6.5 it does not, and on such a host NO peer can be
    /// identified — so the tests that are about identity have nothing to
    /// assert rather than something to fail. The image pins 7.x, so on the
    /// target none of these guards is taken; they are for developer and CI
    /// hosts, which `cargo test` also runs on.
    fn pidfd_available() -> bool {
        match UnixStream::pair() {
            Ok((_client, server)) => sys::peer_pidfd(&server).is_ok(),
            Err(_) => false,
        }
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

    /// A `td.Jail1` call, at td's own object.
    fn jail_call<F>(member: &str, serial: u32, signature: &str, fill: F) -> Vec<u8>
    where
        F: FnOnce(&mut Writer) -> Result<(), WireError>,
    {
        message::Builder::method_call(
            crate::wire::Endian::Little,
            JAIL_PATH,
            Some(JAIL_INTERFACE),
            member,
        )
        .destination(BUS_NAME)
        .serial(serial)
        .body(signature, fill)
        .expect("encode a jail call")
        .encode()
        .expect("encode a jail call")
    }

    fn register_call(instance: &str, app_id: &str, serial: u32) -> Vec<u8> {
        let instance = instance.to_string();
        let app_id = app_id.to_string();
        jail_call("Register", serial, "ssas", move |writer| {
            writer.string(&instance)?;
            writer.string(&app_id)?;
            writer.array("s", |_| Ok(()))
        })
    }

    /// The reply's error name, or `None` for a method return.
    fn error_of(frame: &[u8]) -> Option<String> {
        let (reply, _) = message::decode(frame, 0).expect("decode a reply");
        reply.fields.error_name.map(str::to_string)
    }

    /// A served connection whose PEER is already a registered instance.
    ///
    /// The client end of a socketpair made in this process has this process's
    /// own pid, so registering that pid as an instance's stage-2 pid is what
    /// makes the accepted connection resolve `Jailed` — the same thing a real
    /// jailed application achieves by descending from its stage 2. Registering
    /// through the library rather than over the wire is deliberate: the
    /// identity is taken AT ACCEPT, so a registration that arrived on this
    /// same connection would be too late to describe it.
    ///
    /// The registrant is this process's PARENT rather than this process,
    /// because that is the shape the registry insists on: a registrant may
    /// bind its own child and nothing else. The test harness stands in for
    /// stage 1 and this process stands in for the stage 2 it spawned.
    fn serving_as(app_id: &str) -> (UnixStream, mpsc::Receiver<Outcome>) {
        let (client, server) = UnixStream::pair().expect("socketpair");
        client
            .set_read_timeout(Some(std::time::Duration::from_secs(20)))
            .expect("client read timeout");
        let app_id = app_id.to_string();
        let (tell, hear) = mpsc::channel();
        thread::spawn(move || {
            let guid = Guid::new(GUID).expect("guid");
            let quota = Quota::new();
            let bus = Bus::new();
            let instances = Instances::new();
            let pid = i32::try_from(std::process::id()).expect("a pid fits");
            let crate::lineage::Reading::Of(mine) = RealProcfs.stat(pid) else {
                panic!("this process has a /proc entry");
            };
            let parent = mine.ppid;
            // The registrant is this process's PARENT, which is what makes
            // the accepted connection a strict descendant of a live instance.
            // A pidfd for an arbitrary process is unobtainable here --
            // `pidfd_open` is not on surface #10's roster — so the pair the
            // transport would have proved is stated instead.
            let crate::lineage::Reading::Of(theirs) = RealProcfs.stat(parent) else {
                panic!("this process's parent has a /proc entry");
            };
            let registrant = crate::lineage::Caller {
                pid: parent,
                starttime: theirs.starttime,
            };
            let token = instances
                .open(&RealProcfs, "fixture", &app_id, Vec::new(), this_uid(), registrant)
                .expect("phase one");
            instances
                .complete(&RealProcfs, &token, pid, this_uid(), registrant)
                .expect("phase two");
            let mut connection =
                Connection::accept(server, guid, &quota, &bus, &instances).expect("accept");
            let ended = connection.serve();
            let _ = tell.send(Outcome {
                ended,
                credential: connection.credential(),
                uid: connection.authenticated_uid(),
            });
        });
        (client, hear)
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
            let instances = Instances::new();
            let mut connection =
                Connection::accept(server, guid, &quota, &bus, &instances).expect("accept");
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
            let instances = Instances::new();
            thread::scope(|scope| {
                for server in servers {
                    let quota = &quota;
                    let bus = &*bus;
                    let instances = &instances;
                    let spawned = thread::Builder::new().spawn_scoped(scope, move || {
                        let guid = Guid::new(GUID).expect("guid");
                        if let Ok(mut connection) =
                            Connection::accept(server, guid, quota, bus, instances)
                        {
                            let _ = connection.serve();
                        }
                    });
                    spawned.expect("spawn a connection thread");
                }
            });
        });
        (bus, clients)
    }

    /// A bus whose peers are all CONFINED, which is what makes the filter
    /// observable.
    ///
    /// Both ends of a socketpair are this test process, so every connection
    /// resolves to the same identity — register the fixture instance against
    /// this process and every peer on the bus is `Jailed`. That is the shape
    /// the filter is about: a sandboxed peer looking at another peer it has
    /// no business seeing. `bus_of` registers nothing and so is the
    /// unconfined control for the same tests.
    fn confined_bus_of(peers: usize) -> (Arc<Bus>, Vec<UnixStream>) {
        let (bus, _instances, clients) = confined_bus_watching(peers);
        (bus, clients)
    }

    /// A bus with one UNCONFINED peer and one CONFINED one, in that order.
    ///
    /// Every other helper here gives every peer the same identity, because
    /// both ends of a socketpair are this process and `Instances` answers
    /// about a process. The way out is that identity is resolved ONCE, at
    /// accept, and never recomputed: accepting one connection while no
    /// instance record exists places it outside a jail, and registering the
    /// fixture before accepting the second places that one inside. Neither
    /// accept reads the socket — the answer comes from `SO_PEERPIDFD` — so
    /// the order is this thread's to choose and does not depend on when a
    /// client speaks.
    ///
    /// It is the only shape in this suite where a message crosses a policy
    /// boundary, which is what the reply rules are about.
    fn mixed_bus() -> (Arc<Bus>, UnixStream, UnixStream) {
        let (free_client, free_server) = UnixStream::pair().expect("socketpair");
        let (jailed_client, jailed_server) = UnixStream::pair().expect("socketpair");
        for client in [&free_client, &jailed_client] {
            client
                .set_read_timeout(Some(std::time::Duration::from_secs(20)))
                .expect("client read timeout");
        }
        let bus = Arc::new(Bus::new());
        let serving = Arc::clone(&bus);
        thread::spawn(move || {
            let quota = Quota::new();
            let bus = serving;
            let instances = Instances::new();
            // Accepted with the registry EMPTY, so this process has no
            // instance and this connection is placed outside a jail.
            let free = Connection::accept(
                free_server,
                Guid::new(GUID).expect("guid"),
                &quota,
                &bus,
                &instances,
            );

            let pid = i32::try_from(std::process::id()).expect("a pid fits");
            let crate::lineage::Reading::Of(mine) = RealProcfs.stat(pid) else {
                panic!("this process has a /proc entry");
            };
            let crate::lineage::Reading::Of(theirs) = RealProcfs.stat(mine.ppid) else {
                panic!("this process's parent has a /proc entry");
            };
            let registrant = crate::lineage::Caller {
                pid: mine.ppid,
                starttime: theirs.starttime,
            };
            let token = instances
                .open(&RealProcfs, "fixture", "fixture", Vec::new(), this_uid(), registrant)
                .expect("phase one");
            instances
                .complete(&RealProcfs, &token, pid, this_uid(), registrant)
                .expect("phase two");

            // And this one with the record in place.
            let jailed = Connection::accept(
                jailed_server,
                Guid::new(GUID).expect("guid"),
                &quota,
                &bus,
                &instances,
            );
            thread::scope(|scope| {
                for connection in [free, jailed] {
                    let spawned = thread::Builder::new().spawn_scoped(scope, move || {
                        if let Ok(mut connection) = connection {
                            let _ = connection.serve();
                        }
                    });
                    spawned.expect("spawn a connection thread");
                }
            });
        });
        (bus, free_client, jailed_client)
    }

    /// The same, handing back the registry. A refusal that nevertheless did
    /// the work is invisible from the wire, so the test that cares reads the
    /// registry instead.
    fn confined_bus_watching(peers: usize) -> (Arc<Bus>, Arc<Instances>, Vec<UnixStream>) {
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
        let bus = Arc::new(Bus::new());
        let serving = Arc::clone(&bus);
        let instances = Arc::new(Instances::new());
        let registry = Arc::clone(&instances);
        thread::spawn(move || {
            let quota = Quota::new();
            let bus = serving;
            let instances = registry;
            let pid = i32::try_from(std::process::id()).expect("a pid fits");
            let crate::lineage::Reading::Of(mine) = RealProcfs.stat(pid) else {
                panic!("this process has a /proc entry");
            };
            let crate::lineage::Reading::Of(theirs) = RealProcfs.stat(mine.ppid) else {
                panic!("this process's parent has a /proc entry");
            };
            let registrant = crate::lineage::Caller {
                pid: mine.ppid,
                starttime: theirs.starttime,
            };
            let token = instances
                .open(&RealProcfs, "fixture", "fixture", Vec::new(), this_uid(), registrant)
                .expect("phase one");
            instances
                .complete(&RealProcfs, &token, pid, this_uid(), registrant)
                .expect("phase two");
            thread::scope(|scope| {
                for server in servers {
                    let quota = &quota;
                    let bus = &*bus;
                    let instances = &*instances;
                    let spawned = thread::Builder::new().spawn_scoped(scope, move || {
                        let guid = Guid::new(GUID).expect("guid");
                        if let Ok(mut connection) =
                            Connection::accept(server, guid, quota, bus, instances)
                        {
                            let _ = connection.serve();
                        }
                    });
                    spawned.expect("spawn a connection thread");
                }
            });
        });
        (bus, instances, clients)
    }

    /// A bus whose peers all resolve `Unknown`, which nothing else here can
    /// produce.
    ///
    /// Phase one is opened against this process's parent and never completed.
    /// §E refuses a strict descendant of a PENDING registrant — deliberately,
    /// since a connection arriving between the two phases is ambiguous — so
    /// every connection from this process resolves `Unknown` for as long as
    /// the registration stays open. That is the only way to reach the arm
    /// from a test, and without it the `Unknown` policy is asserted only
    /// where it is written.
    fn unknown_bus_of(peers: usize) -> (Arc<Bus>, Vec<UnixStream>) {
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
        let bus = Arc::new(Bus::new());
        let serving = Arc::clone(&bus);
        thread::spawn(move || {
            let quota = Quota::new();
            let bus = serving;
            let instances = Instances::new();
            let pid = i32::try_from(std::process::id()).expect("a pid fits");
            let crate::lineage::Reading::Of(mine) = RealProcfs.stat(pid) else {
                panic!("this process has a /proc entry");
            };
            let crate::lineage::Reading::Of(theirs) = RealProcfs.stat(mine.ppid) else {
                panic!("this process's parent has a /proc entry");
            };
            let registrant = crate::lineage::Caller {
                pid: mine.ppid,
                starttime: theirs.starttime,
            };
            // Opened and deliberately NOT completed.
            instances
                .open(&RealProcfs, "pending", "pending", Vec::new(), this_uid(), registrant)
                .expect("phase one");
            thread::scope(|scope| {
                for server in servers {
                    let quota = &quota;
                    let bus = &*bus;
                    let instances = &instances;
                    let spawned = thread::Builder::new().spawn_scoped(scope, move || {
                        let guid = Guid::new(GUID).expect("guid");
                        if let Ok(mut connection) =
                            Connection::accept(server, guid, quota, bus, instances)
                        {
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
        /// Frames `answer` stepped over, in arrival order, waiting for a
        /// `frame` that wants them.
        ///
        /// The broker announces a name changing hands BEFORE it answers the
        /// call that changed it, which is the reference daemon's order. A test
        /// that asks for the ANSWER should not have to know that, and a test
        /// that asks for the announcement should still find it, so the one
        /// that arrives early waits here rather than being discarded.
        deferred: Vec<Vec<u8>>,
    }

    /// The serial `arriving` spends on its barrier call. Far above anything a
    /// test writes by hand, so a reply to it can never be mistaken for a reply
    /// to the test's own first message.
    const ARRIVAL_BARRIER: u32 = 0xa55e_d000;

    impl Peer {
        /// Authenticate, BEGIN, say `Hello`, and return the unique name the
        /// bus handed out — once that name is actually reachable.
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
                deferred: Vec::new(),
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
            // The unique name is announced like any other name gained, and
            // it arrives after the `Hello` reply because `publish` — which is
            // what makes this connection routable enough to be told anything
            // — happens after that reply is queued.
            let frame = peer.frame();
            let (gained, _) =
                message::decode(&frame, 0).expect("decode the arrival announcement");
            assert_eq!(
                (
                    gained.kind,
                    gained.fields.sender,
                    gained.fields.member,
                    gained.args().first().and_then(crate::wire::Value::as_str),
                ),
                (
                    message::MessageType::Signal,
                    Some(BUS_NAME),
                    Some("NameAcquired"),
                    Some(name.as_str()),
                ),
                "a peer was not told the name it had just been given"
            );
            // `say_hello` queues this reply BEFORE `publish` makes the name
            // routable, so a peer that has read its Hello can still be
            // invisible to the peer about to look for it. Neither a sleep nor
            // a poll: the broker serves one connection's messages in order and
            // `publish` is the last thing `say_hello` does, so a call made
            // here is necessarily handled after it. Asking for this
            // connection's own owner both waits for that and asserts it.
            peer.send(&name_query("GetNameOwner", &name, ARRIVAL_BARRIER));
            let frame = peer.frame();
            let (settled, _) =
                message::decode(&frame, 0).expect("decode the arrival barrier");
            // Type and sender as well as serial: a reply serial names nothing
            // on its own, since any peer may route a method return carrying
            // one.
            assert_eq!(
                (
                    settled.kind,
                    settled.fields.sender,
                    settled.fields.reply_serial
                ),
                (
                    message::MessageType::MethodReturn,
                    Some(BUS_NAME),
                    Some(ARRIVAL_BARRIER)
                ),
                "the arrival barrier was answered by another message"
            );
            assert_eq!(
                settled.args().first().and_then(crate::wire::Value::as_str),
                Some(name.as_str()),
                "{name} was not on the bus when its own Hello came back"
            );
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
            if !self.deferred.is_empty() {
                return self.deferred.remove(0);
            }
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
        /// The next frame that ANSWERS a call, holding aside any signal that
        /// arrives ahead of it.
        ///
        /// A name changing hands is announced before the call that changed it
        /// is answered. Tests that care about the announcement read it with
        /// `frame`; this is for the ones that care about the answer, and what
        /// it steps over is kept rather than dropped so a later `frame` still
        /// sees it.
        fn answer(&mut self) -> Vec<u8> {
            let mut stepped: Vec<Vec<u8>> = Vec::new();
            let found = loop {
                let frame = self.frame();
                let Ok((message, _)) = message::decode(&frame, 0) else {
                    break frame;
                };
                if message.kind != message::MessageType::Signal {
                    break frame;
                }
                stepped.push(frame);
            };
            // Put back AFTER the loop, never during it: `frame` drains this
            // list, so a frame deferred inside the loop would be handed
            // straight back and deferred again, for ever.
            //
            // Stepped-over frames first, because `frame` drains from the
            // front: anything still deferred arrived later than everything
            // this call passed.
            stepped.append(&mut self.deferred);
            self.deferred = stepped;
            found
        }

        fn expect_silence(&mut self) {
            assert!(
                self.deferred.is_empty(),
                "a frame this test stepped over is still waiting"
            );
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
            // Put it back. The short wait belongs to this assertion and to
            // nothing after it: a test that proves silence and then waits for
            // a frame was waiting 200ms for it, which under load is a
            // "read: timed out" in place of whatever the test meant to say.
            self.stream
                .set_read_timeout(Some(std::time::Duration::from_secs(20)))
                .expect("read timeout");
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

    /// A method call whose sender is not listening for the answer.
    fn unanswerable_call(destination: &str, serial: u32) -> Vec<u8> {
        message::Builder::method_call(
            crate::wire::Endian::Little,
            "/org/example/Thing",
            Some("org.example.Thing"),
            "Do",
        )
        .destination(destination)
        .serial(serial)
        .flags(message::FLAG_NO_REPLY_EXPECTED)
        .encode()
        .expect("encode a call that wants no reply")
    }

    /// A signal aimed at one connection rather than broadcast.
    fn directed_signal(destination: &str, serial: u32) -> Vec<u8> {
        message::Builder::signal(
            crate::wire::Endian::Little,
            "/org/example/Thing",
            "org.example.Thing",
            "Happened",
        )
        .destination(destination)
        .serial(serial)
        .encode()
        .expect("encode a directed signal")
    }

    /// A method return addressed to `destination`, answering `answers`.
    fn reply_to(destination: &str, answers: u32, serial: u32) -> Vec<u8> {
        message::Builder::method_return(crate::wire::Endian::Little, answers)
            .destination(destination)
            .serial(serial)
            .encode()
            .expect("encode a reply")
    }

    /// Read the `NameAcquired` a connection is sent for its own unique name.
    ///
    /// `Peer::arrive` accounts for it; the tests that lay out their own
    /// opening bytes have to as well. Asserted rather than discarded, so each
    /// of them keeps checking that the announcement is where this says it is.
    fn takes_its_name(peer: &mut Peer) -> String {
        let frame = peer.frame();
        let (gained, _) =
            message::decode(&frame, 0).expect("decode the arrival announcement");
        assert_eq!(
            (gained.kind, gained.fields.sender, gained.fields.member),
            (
                message::MessageType::Signal,
                Some(BUS_NAME),
                Some("NameAcquired")
            ),
            "a peer was not told the name it had just been given"
        );
        gained
            .args()
            .first()
            .and_then(crate::wire::Value::as_str)
            .unwrap_or("")
            .to_string()
    }

    /// `RequestName(name, flags)`.
    fn request_name(name: &str, flags: u32, serial: u32) -> Vec<u8> {
        message::Builder::method_call(crate::wire::Endian::Little, BUS_PATH, Some(BUS_NAME), "RequestName")
            .destination(BUS_NAME)
            .serial(serial)
            .body("su", |writer| {
                writer.string(name)?;
                writer.uint32(flags);
                Ok(())
            })
            .expect("body")
            .encode()
            .expect("encode a RequestName")
    }

    /// `ReleaseName(name)`.
    fn release_name(name: &str, serial: u32) -> Vec<u8> {
        name_query("ReleaseName", name, serial)
    }

    /// The `u` a name method answered with, and the serial it answers.
    fn name_code(frame: &[u8]) -> (u32, Option<u32>) {
        let (reply, _) = message::decode(frame, 0).expect("decode a name reply");
        assert_eq!(
            reply.kind,
            message::MessageType::MethodReturn,
            "the broker refused rather than answering: {:?}",
            reply.fields.error_name
        );
        (
            reply.args().first().and_then(crate::wire::Value::as_u32).unwrap_or(0),
            reply.fields.reply_serial,
        )
    }

    /// The name a `NameAcquired` or `NameLost` signal carries.
    fn announced(frame: &[u8]) -> (String, String) {
        let (signal, _) = message::decode(frame, 0).expect("decode an announcement");
        assert_eq!(signal.kind, message::MessageType::Signal);
        assert_eq!(signal.fields.sender, Some(BUS_NAME));
        (
            signal.fields.member.unwrap_or("").to_string(),
            signal
                .args()
                .first()
                .and_then(crate::wire::Value::as_str)
                .unwrap_or("")
                .to_string(),
        )
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

    /// Registration is two calls, and the second is what makes the instance
    /// resolvable. §D splits it because the pid the record needs does not
    /// exist when the instance does.
    #[test]
    fn registration_takes_two_calls_over_the_bus() {
        let (mut client, _hear) = serving();
        let mut opening =
            format!("\0AUTH EXTERNAL {}\r\nBEGIN\r\n", uid_hex()).into_bytes();
        opening.extend_from_slice(&bus_call("Hello", 1));
        opening.extend_from_slice(&register_call("fixture", "fixture", 2));
        client.write_all(&opening).expect("write");

        let mut peer = Peer {
            stream: client,
            held: Vec::new(),
            lines: 1,
            deferred: Vec::new(),
        };
        let _hello = peer.frame();
        let _gained = takes_its_name(&mut peer);
        let frame = peer.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode Register's reply");
        assert_eq!(
            reply.kind,
            message::MessageType::MethodReturn,
            "Register was refused: {:?}",
            reply.fields.error_name
        );
        // The SENDER on this reply is a cross-crate contract, not decoration.
        // td-jail authenticates the broker's answers by it — a peer on a
        // shared session bus can otherwise forge a `Complete` success and free
        // an unregistered jail — and `answer()` is the path that produces both
        // of its replies. Deleting `.sender(BUS_NAME)` from it left this
        // crate's 214 tests and td-jail's 96 all green, and hung the real
        // client for a full deadline per launch, because a reply this client
        // does not recognise is SKIPPED rather than refused. That is the
        // "fails at boot and nowhere earlier" shape, so it is pinned here.
        assert_eq!(
            reply.fields.sender,
            Some(BUS_NAME),
            "a jail reply lost its SENDER, which is what td-jail authenticates \
             the broker by"
        );
        let token = reply
            .args()
            .first()
            .and_then(crate::wire::Value::as_str)
            .expect("Register answers with a token")
            .to_string();

        // Phase two, with this process's own pid — and it is REFUSED, which
        // is the rule rather than a limitation of the harness. A registrant
        // may bind its own CHILD, and this connection's process is not its own
        // child. A test living in one process cannot stage a real stage 2
        // without spawning a binary it would then have to declare as an input,
        // so the successful path is pinned in `lineage`'s tests where `/proc`
        // is injected. What the wire has to show is that both calls travel,
        // that phase one's token comes back, and that the registry's refusal
        // reaches the caller as an error rather than being swallowed.
        let pid = std::process::id();
        let token_again = token.clone();
        peer.send(&jail_call("Complete", 3, "su", move |writer| {
            writer.string(&token_again)?;
            writer.uint32(pid);
            Ok(())
        }));
        let frame = peer.frame();
        assert_eq!(
            error_of(&frame).as_deref(),
            Some("td.Jail1.Error.Refused"),
            "a pid the registrant did not spawn was bound to its instance"
        );

        // And the token survives that refusal. A registration burned by a
        // failed attempt would be a way for one bad argument — or one hostile
        // guess — to kill a launch that is still in progress.
        peer.send(&jail_call("Complete", 4, "su", move |writer| {
            writer.string(&token)?;
            writer.uint32(1);
            Ok(())
        }));
        let frame = peer.frame();
        assert_eq!(
            error_of(&frame).as_deref(),
            Some("td.Jail1.Error.Refused"),
            "the second attempt failed for the wrong reason"
        );
    }

    /// A SIGNAL named `Register` is not a call to `Register`.
    ///
    /// The specification reserves method dispatch for `METHOD_CALL`, and until
    /// these two arms moved above the no-reply guard that was enforced by
    /// accident: `wants_reply` is false for every other type, so nothing below
    /// the guard could run for a signal. A review found the gap by sending
    /// one, and it registered.
    #[test]
    fn a_signal_named_register_does_not_register() {
        let (mut client, _hear) = serving();
        let shout = message::Builder::signal(
            crate::wire::Endian::Little,
            JAIL_PATH,
            JAIL_INTERFACE,
            "Register",
        )
        .destination(BUS_NAME)
        .serial(2)
        .body("ssas", |writer| {
            writer.string("shouted")?;
            writer.string("fixture")?;
            writer.array("s", |_| Ok(()))
        })
        .expect("encode")
        .encode()
        .expect("encode");

        let mut opening =
            format!("\0AUTH EXTERNAL {}\r\nBEGIN\r\n", uid_hex()).into_bytes();
        opening.extend_from_slice(&bus_call("Hello", 1));
        opening.extend_from_slice(&shout);
        client.write_all(&opening).expect("write");

        let mut peer = Peer {
            stream: client,
            held: Vec::new(),
            lines: 1,
            deferred: Vec::new(),
        };
        let _hello = peer.frame();
        let _gained = takes_its_name(&mut peer);

        // If the signal had registered, this would be refused as a duplicate.
        peer.send(&register_call("shouted", "fixture", 3));
        assert_eq!(
            error_of(&peer.frame()),
            None,
            "a signal registered the instance"
        );
    }

    /// `NO_REPLY_EXPECTED` withdraws the REPLY, not the work.
    ///
    /// A draft returned from `bus_method`'s no-reply guard before reaching the
    /// two `td.Jail1` arms, on the strength of a comment saying no bus method
    /// changed state — true when it was written and false once these landed.
    /// A `Complete` dropped that way is the dangerous one: stage 2 is already
    /// running, and an application with no registration on record resolves
    /// `Unconfined`.
    #[test]
    fn a_registration_that_wants_no_reply_still_registers() {
        let (mut client, _hear) = serving();
        let quiet = message::Builder::method_call(
            crate::wire::Endian::Little,
            JAIL_PATH,
            Some(JAIL_INTERFACE),
            "Register",
        )
        .destination(BUS_NAME)
        .flags(message::FLAG_NO_REPLY_EXPECTED)
        .serial(2)
        .body("ssas", |writer| {
            writer.string("quiet")?;
            writer.string("fixture")?;
            writer.array("s", |_| Ok(()))
        })
        .expect("encode")
        .encode()
        .expect("encode");

        let mut opening =
            format!("\0AUTH EXTERNAL {}\r\nBEGIN\r\n", uid_hex()).into_bytes();
        opening.extend_from_slice(&bus_call("Hello", 1));
        opening.extend_from_slice(&quiet);
        client.write_all(&opening).expect("write");

        let mut peer = Peer {
            stream: client,
            held: Vec::new(),
            lines: 1,
            deferred: Vec::new(),
        };
        let _hello = peer.frame();
        let _gained = takes_its_name(&mut peer);

        // Nothing comes back for the quiet call, so the only way to see it is
        // its effect: the instance name is taken, and a second registration
        // under the same name is refused.
        peer.send(&register_call("quiet", "fixture", 3));
        let frame = peer.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode");
        assert_eq!(
            reply.fields.reply_serial,
            Some(3),
            "the quiet Register was answered after all"
        );
        assert_eq!(
            error_of(&frame).as_deref(),
            Some("td.Jail1.Error.Refused"),
            "the quiet Register did not register"
        );

        // And the same for `Complete`, which is the one that matters: a
        // dropped completion leaves a running stage 2 with no record. Sent
        // quietly with a token that was never issued, the refusal it would
        // have produced is suppressed and the connection carries on — so what
        // this leg shows is that the message was consumed rather than
        // disconnecting the peer, and the arm above it that the registry
        // reached at all.
        let quiet_complete = message::Builder::method_call(
            crate::wire::Endian::Little,
            JAIL_PATH,
            Some(JAIL_INTERFACE),
            "Complete",
        )
        .destination(BUS_NAME)
        .flags(message::FLAG_NO_REPLY_EXPECTED)
        .serial(4)
        .body("su", |writer| {
            writer.string("not-a-token")?;
            writer.uint32(1);
            Ok(())
        })
        .expect("encode")
        .encode()
        .expect("encode");
        peer.send(&quiet_complete);
        peer.send(&bus_call("GetId", 5));
        let frame = peer.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode");
        assert_eq!(
            reply.fields.reply_serial,
            Some(5),
            "the quiet Complete was answered"
        );
    }

    /// A helper that exists to be somebody's child.
    ///
    /// It blocks until its stdin closes, so the test that spawned it decides
    /// when it ends, and it does nothing at all unless that test spawned it.
    #[test]
    fn a_child_that_waits_for_its_parent() {
        if std::env::var_os("TD_BUSD_TEST_CHILD").is_none() {
            return;
        }
        let mut ignored = Vec::new();
        let _ = std::io::stdin().read_to_end(&mut ignored);
    }

    /// Phase two, completed for real, over the wire.
    ///
    /// The registry's rule is that a registrant may bind its own CHILD, so a
    /// test in one process needs an actual child to reach the successful path
    /// at all. A draft asserted only the refusal and claimed a real child
    /// would mean declaring a binary as a build input; a review pointed out
    /// that `current_exe()` is this test binary, already a build output, and
    /// that td-jail's own suite re-enters it exactly this way. Without this,
    /// a broker that refused every completion would pass the wire suite.
    #[test]
    fn a_registration_completes_over_the_bus_for_a_real_child() {
        let Ok(executable) = std::env::current_exe() else {
            return;
        };
        let child = std::process::Command::new(executable)
            .args([
                "--exact",
                "transport::tests::a_child_that_waits_for_its_parent",
                "--nocapture",
            ])
            .env("TD_BUSD_TEST_CHILD", "1")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        let Ok(mut child) = child else {
            return;
        };
        let held = child.stdin.take();

        let (mut client, _hear) = serving();
        let mut opening =
            format!("\0AUTH EXTERNAL {}\r\nBEGIN\r\n", uid_hex()).into_bytes();
        opening.extend_from_slice(&bus_call("Hello", 1));
        opening.extend_from_slice(&register_call("real", "fixture", 2));
        client.write_all(&opening).expect("write");
        let mut peer = Peer {
            stream: client,
            held: Vec::new(),
            lines: 1,
            deferred: Vec::new(),
        };
        let _hello = peer.frame();
        let _gained = takes_its_name(&mut peer);
        let frame = peer.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode Register's reply");
        let token = reply
            .args()
            .first()
            .and_then(crate::wire::Value::as_str)
            .map(str::to_string);

        let outcome = token.map(|token| {
            let pid = child.id();
            peer.send(&jail_call("Complete", 3, "su", move |writer| {
                writer.string(&token)?;
                writer.uint32(pid);
                Ok(())
            }));
            error_of(&peer.frame())
        });

        // Release the child before asserting, so a failure does not leave it
        // running.
        drop(held);
        let _ = child.wait();

        assert_eq!(
            outcome,
            Some(None),
            "completing with the registrant's own child was refused"
        );
    }

    /// Every argument is checked at the wire rather than in the registry.
    #[test]
    fn registration_refuses_arguments_that_are_not_what_they_claim() {
        let (mut client, _hear) = serving();
        let mut opening =
            format!("\0AUTH EXTERNAL {}\r\nBEGIN\r\n", uid_hex()).into_bytes();
        opening.extend_from_slice(&bus_call("Hello", 1));
        client.write_all(&opening).expect("write");
        let mut peer = Peer {
            stream: client,
            held: Vec::new(),
            lines: 1,
            deferred: Vec::new(),
        };
        let _hello = peer.frame();
        let _gained = takes_its_name(&mut peer);

        // A name that is a `.` run and nothing else, which the grammar's
        // comment claims to exclude and a draft did not: `/` was refused and
        // `..` — the one the comment names — was not.
        for path_shaped in ["..", ".", "a..b"] {
            peer.send(&register_call(path_shaped, "fixture", 2));
            assert_eq!(
                error_of(&peer.frame()).as_deref(),
                Some("org.freedesktop.DBus.Error.InvalidArgs"),
                "instance name {path_shaped:?} was accepted"
            );
        }

        // An instance name that could be read as a path.
        peer.send(&register_call("../elsewhere", "fixture", 2));
        assert_eq!(
            error_of(&peer.frame()).as_deref(),
            Some("org.freedesktop.DBus.Error.InvalidArgs"),
            "a path-shaped instance name was accepted"
        );

        // An application id that could never be a bus name, which is what §D
        // reports it as.
        peer.send(&register_call("fixture", "not a name", 3));
        assert_eq!(
            error_of(&peer.frame()).as_deref(),
            Some("org.freedesktop.DBus.Error.InvalidArgs"),
            "an unnameable application id was accepted"
        );

        // A predeclared service that is not a bus name either.
        peer.send(&jail_call("Register", 4, "ssas", |writer| {
            writer.string("fixture")?;
            writer.string("fixture")?;
            writer.array("s", |array| array.string("nope"))
        }));
        assert_eq!(
            error_of(&peer.frame()).as_deref(),
            Some("org.freedesktop.DBus.Error.InvalidArgs"),
            "an unnameable service was accepted"
        );

        // A predeclared service that IS a bus name and still cannot be one:
        // `:1.7` is a unique name, which the broker hands out and nobody may
        // claim. A draft graded services with `valid_bus_name`, which accepts
        // it.
        peer.send(&jail_call("Register", 5, "ssas", |writer| {
            writer.string("fixture")?;
            writer.string("fixture")?;
            writer.array("s", |array| array.string(":1.7"))
        }));
        assert_eq!(
            error_of(&peer.frame()).as_deref(),
            Some("org.freedesktop.DBus.Error.InvalidArgs"),
            "a unique name was accepted as a predeclared service"
        );

        // A token nobody issued.
        peer.send(&jail_call("Complete", 6, "su", |writer| {
            writer.string("not-a-token")?;
            writer.uint32(std::process::id());
            Ok(())
        }));
        assert_eq!(
            error_of(&peer.frame()).as_deref(),
            Some("td.Jail1.Error.Refused"),
            "an unissued token completed a registration"
        );
    }

    /// The app id is a td identity, not a bus name.
    ///
    /// A draft graded it with `valid_bus_name`, which a review showed is wrong
    /// in both directions: §D's identity section defines a td application's
    /// identity as a short FLAT name — `firefox`, `darktable` — with reverse
    /// DNS reserved as an alias for wires td does not own. A bus name requires
    /// an interior `.`, so every real td identity would have been refused, and
    /// `:1.7` would have been accepted although it is a unique connection name
    /// the broker hands out and nobody can be.
    #[test]
    fn the_application_id_is_a_flat_td_name() {
        let (mut client, _hear) = serving();
        let mut opening =
            format!("\0AUTH EXTERNAL {}\r\nBEGIN\r\n", uid_hex()).into_bytes();
        opening.extend_from_slice(&bus_call("Hello", 1));
        client.write_all(&opening).expect("write");
        let mut peer = Peer {
            stream: client,
            held: Vec::new(),
            lines: 1,
            deferred: Vec::new(),
        };
        let _hello = peer.frame();
        let _gained = takes_its_name(&mut peer);

        let mut serial = 2;
        for (which, good) in ["fixture", "firefox", "td-jail-fixture", "org.td.Alias"]
            .into_iter()
            .enumerate()
        {
            // A fresh instance name each time, or the second one is refused
            // for the name rather than graded on its id.
            peer.send(&register_call(&format!("i{which}"), good, serial));
            assert_eq!(
                error_of(&peer.frame()),
                None,
                "application id {good:?} was refused"
            );
            serial += 1;
        }

        for bad in [
            ":1.7",                              // the broker's to hand out
            "org.td..alias",                     // a `.` run
            "-leading",                          // an id that reads as a flag
            ".",
            "",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", // 33 bytes
        ] {
            peer.send(&register_call("rejected", bad, serial));
            assert_eq!(
                error_of(&peer.frame()).as_deref(),
                Some("org.freedesktop.DBus.Error.InvalidArgs"),
                "application id {bad:?} was accepted"
            );
            serial += 1;
        }
    }

    /// `td.Jail1` answers at td's own object and nowhere else.
    ///
    /// The same rule §D states for the broker's own interface, applied to
    /// td's: a `Register` addressed to `/org/freedesktop/DBus` is not this
    /// method, and answering it there would put private surface on the
    /// specification's object.
    #[test]
    fn jail_registration_is_not_answered_on_the_bus_object() {
        let (mut client, _hear) = serving();
        let misplaced = message::Builder::method_call(
            crate::wire::Endian::Little,
            BUS_PATH,
            Some(JAIL_INTERFACE),
            "Register",
        )
        .destination(BUS_NAME)
        .serial(2)
        .body("ssas", |writer| {
            writer.string("fixture")?;
            writer.string("fixture")?;
            writer.array("s", |_| Ok(()))
        })
        .expect("body")
        .encode()
        .expect("encode");
        let mut opening =
            format!("\0AUTH EXTERNAL {}\r\nBEGIN\r\n", uid_hex()).into_bytes();
        opening.extend_from_slice(&bus_call("Hello", 1));
        opening.extend_from_slice(&misplaced);
        client.write_all(&opening).expect("write");
        let mut peer = Peer {
            stream: client,
            held: Vec::new(),
            lines: 1,
            deferred: Vec::new(),
        };
        let _hello = peer.frame();
        let _gained = takes_its_name(&mut peer);
        assert_eq!(
            error_of(&peer.frame()).as_deref(),
            Some("org.freedesktop.DBus.Error.UnknownMethod"),
            "td.Jail1 answered on the specification's own object"
        );
    }

    /// A confined peer's credentials carry `td.AppId`; an unconfined one's do
    /// not.
    ///
    /// The absence is as load-bearing as the presence: the key means "the
    /// broker established that this connection is this application", and an
    /// empty or sentinel value would invite a reader to treat a failure to
    /// establish that as an answer.
    #[test]
    fn a_confined_peers_credentials_carry_its_app_id() {
        // The `Jailed` half of this rests on the accept path identifying the
        // peer, which needs the option.
        if !pidfd_available() {
            return;
        }
        for (harness, expected) in [
            (serving_as("fixture"), Some("fixture")),
            (serving(), None),
        ] {
            let (mut client, _hear) = harness;
            let mut opening =
                format!("\0AUTH EXTERNAL {}\r\nBEGIN\r\n", uid_hex()).into_bytes();
            opening.extend_from_slice(&bus_call("Hello", 1));
            client.write_all(&opening).expect("write");
            let mut peer = Peer {
                stream: client,
                held: Vec::new(),
                lines: 1,
                deferred: Vec::new(),
            };
            let frame = peer.frame();
            let (hello, _) = message::decode(&frame, 0).expect("decode Hello");
            let me = hello
                .args()
                .first()
                .and_then(crate::wire::Value::as_str)
                .expect("a unique name")
                .to_string();
            assert_eq!(takes_its_name(&mut peer), me);

            peer.send(&name_query("GetConnectionCredentials", &me, 2));
            let frame = peer.frame();
            let (reply, _) = message::decode(&frame, 0).expect("decode credentials");
            let entries = reply
                .args()
                .first()
                .and_then(crate::wire::Value::as_seq)
                .expect("a dictionary came back")
                .values(16)
                .expect("read the dictionary");
            let mut app_id = None;
            for entry in &entries {
                let pair = entry
                    .as_seq()
                    .expect("a dict entry")
                    .values(2)
                    .expect("read the entry");
                if pair.first().and_then(crate::wire::Value::as_str) != Some("td.AppId") {
                    continue;
                }
                app_id = pair
                    .get(1)
                    .and_then(crate::wire::Value::as_seq)
                    .and_then(|variant| variant.values(1).ok())
                    .and_then(|held| held.first().and_then(crate::wire::Value::as_str))
                    .map(str::to_string);
            }
            assert_eq!(app_id.as_deref(), expected, "td.AppId was {app_id:?}");
        }
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
            deferred: Vec::new(),
        };
        let frame = peer.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode");
        assert_eq!(reply.kind, message::MessageType::MethodReturn);
        assert_eq!(
            reply.args().first().and_then(crate::wire::Value::as_str),
            Some(":1.1")
        );
    }

    /// The filter, end to end: a confined peer is not told about a peer it
    /// may not see, whichever way it asks.
    ///
    /// Four questions with one answer between them. §D's objection to doing
    /// this piecemeal is that an error which confirms a name exists is not a
    /// filter, so the interesting assertion is not that any one call refuses
    /// — it is that all four agree, and that the unconfined control below
    /// disagrees with all four.
    #[test]
    fn a_confined_peer_is_told_nothing_about_a_peer_it_may_not_see() {
        if !pidfd_available() {
            return;
        }
        let (_bus, mut clients) = confined_bus_of(2);
        let second = clients.pop().expect("two clients");
        let first = clients.pop().expect("two clients");
        let (mut one, name_one) = Peer::arrive(first);
        let (_two, name_two) = Peer::arrive(second);

        // ListNames: the broker and itself, and nothing else.
        one.send(&bus_call("ListNames", 2));
        let frame = one.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode ListNames");
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
        assert!(
            listed.contains(&name_one),
            "a peer cannot see itself: {listed:?}"
        );
        assert!(
            !listed.contains(&name_two),
            "the filter leaked a name: {listed:?}"
        );

        // GetNameOwner: absent.
        one.send(&name_query("GetNameOwner", &name_two, 3));
        let frame = one.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode GetNameOwner");
        assert_eq!(reply.kind, message::MessageType::Error);
        assert_eq!(
            reply.fields.error_name,
            Some("org.freedesktop.DBus.Error.NameHasNoOwner")
        );

        // NameHasOwner: false, rather than an error that admits it.
        one.send(&name_query("NameHasOwner", &name_two, 4));
        let frame = one.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode NameHasOwner");
        assert_eq!(reply.kind, message::MessageType::MethodReturn);
        assert!(
            matches!(reply.args().first(), Some(crate::wire::Value::Bool(false))),
            "the filter admitted a name it had just hidden: {:?}",
            reply.args().first()
        );

        // And the credentials, which are what §D singles out: another
        // instance's host pid is the input to the lineage walk itself.
        one.send(&name_query("GetConnectionUnixProcessID", &name_two, 5));
        let frame = one.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode the pid lookup");
        assert_eq!(reply.kind, message::MessageType::Error);
        assert_eq!(
            reply.fields.error_name,
            Some("org.freedesktop.DBus.Error.NameHasNoOwner"),
            "a sandbox was handed another peer's host pid"
        );
    }

    /// The control. The same questions on a bus with no registered instance
    /// are answered in full, because an unconfined peer is a positive grant
    /// rather than a peer the broker failed to place.
    #[test]
    fn an_unconfined_peer_is_still_told_everything() {
        let (_bus, mut clients) = bus_of(2);
        let second = clients.pop().expect("two clients");
        let first = clients.pop().expect("two clients");
        let (mut one, _) = Peer::arrive(first);
        let (_two, name_two) = Peer::arrive(second);

        one.send(&name_query("GetNameOwner", &name_two, 2));
        let frame = one.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode");
        assert_eq!(reply.kind, message::MessageType::MethodReturn);

        one.send(&name_query("GetConnectionUnixProcessID", &name_two, 3));
        let frame = one.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode");
        assert_eq!(
            reply.args().first().and_then(crate::wire::Value::as_u32),
            Some(std::process::id())
        );

        // The POSITIVE half of `NameHasOwner`, which nothing else asserts: a
        // reviewer replaced the whole expression with `false` and the entire
        // suite stayed green, because the confined test wants `false` and no
        // other test reads the boolean at all. Without this the one assertion
        // that is ABOUT "false rather than an error" cannot fail.
        one.send(&name_query("NameHasOwner", &name_two, 4));
        let frame = one.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode");
        assert!(
            matches!(reply.args().first(), Some(crate::wire::Value::Bool(true))),
            "a visible name was reported absent: {:?}",
            reply.args().first()
        );

        one.send(&bus_call("ListNames", 5));
        let frame = one.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode ListNames");
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
        assert!(listed.contains(&name_two), "{listed:?}");
    }

    /// A confined peer cannot reach a peer it cannot see, and is told the
    /// same story on the send path as on the lookup path.
    #[test]
    fn a_confined_peer_cannot_send_to_a_peer_it_may_not_see() {
        if !pidfd_available() {
            return;
        }
        let (_bus, mut clients) = confined_bus_of(2);
        let second = clients.pop().expect("two clients");
        let first = clients.pop().expect("two clients");
        let (mut one, _) = Peer::arrive(first);
        let (mut two, name_two) = Peer::arrive(second);

        one.send(&peer_call(&name_two, 2, "anyone"));
        let frame = one.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode the refusal");
        assert_eq!(reply.kind, message::MessageType::Error);
        assert_eq!(
            reply.fields.error_name,
            Some("org.freedesktop.DBus.Error.NameHasNoOwner"),
            "a refusal that names the peer is not a filter"
        );
        // And it did not arrive anyway, which the error alone does not say.
        two.expect_silence();
    }

    /// `td.Jail1` is the one place a confined caller is DENIED rather than
    /// told nothing is there. It is the interface that decides what a
    /// confined application is, and registration is authenticated only by
    /// uid — which in v1 every session peer shares.
    #[test]
    fn a_confined_peer_may_not_register_an_instance() {
        if !pidfd_available() {
            return;
        }
        let (client, _hear) = serving_as("fixture");
        let (mut peer, _) = Peer::arrive(client);
        let call = message::Builder::method_call(
            crate::wire::Endian::Little,
            JAIL_PATH,
            Some(JAIL_INTERFACE),
            "Register",
        )
        .destination(BUS_NAME)
        .serial(2)
        .body("ssas", |writer| {
            writer.string("mine")?;
            writer.string("mine")?;
            writer.array("s", |_| Ok(()))
        })
        .expect("body")
        .encode()
        .expect("encode");
        peer.send(&call);
        let frame = peer.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode the refusal");
        assert_eq!(reply.kind, message::MessageType::Error);
        assert_eq!(
            reply.fields.error_name,
            Some("org.freedesktop.DBus.Error.AccessDenied"),
            "a confined peer registered an instance"
        );
    }

    /// A peer the broker could not place gets less than a sandboxed one, and
    /// gets it consistently.
    ///
    /// The consistency is the point and a draft failed it: `Unknown` denied
    /// everything including the caller's OWN name, while `Hello` had just
    /// handed that name over and the broker went on answering `GetId` and
    /// `Ping` — so the bus both told a peer its name and denied it had one.
    /// The rule is that an unplaceable peer keeps what it has already been
    /// told and gets nothing else, portal included.
    #[test]
    fn an_unplaceable_peer_keeps_only_what_it_has_been_told() {
        if !pidfd_available() {
            return;
        }
        let (_bus, mut clients) = unknown_bus_of(2);
        let second = clients.pop().expect("two clients");
        let first = clients.pop().expect("two clients");
        let (mut one, name_one) = Peer::arrive(first);
        let (_two, name_two) = Peer::arrive(second);

        // Its own name resolves — `Peer::arrive`'s barrier already proved
        // that, since the barrier IS a GetNameOwner for the caller's own
        // name and it would have failed for a peer denied its own name.
        one.send(&name_query("GetNameOwner", &name_one, 2));
        let frame = one.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode own name");
        assert_eq!(
            reply.kind,
            message::MessageType::MethodReturn,
            "the bus denied a peer the name it had just given it"
        );

        // And the broker still answers it, which is what makes the above a
        // consistency requirement rather than a courtesy.
        one.send(&bus_call("GetId", 3));
        let frame = one.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode GetId");
        assert_eq!(reply.kind, message::MessageType::MethodReturn);

        // Another peer: absent.
        one.send(&name_query("GetNameOwner", &name_two, 4));
        let frame = one.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode");
        assert_eq!(
            reply.fields.error_name,
            Some("org.freedesktop.DBus.Error.NameHasNoOwner")
        );

        // That this peer really is UNPLACEABLE rather than confined, which
        // the assertions above do not settle on their own: a `Jailed` peer
        // would answer them identically. `td.AppId` is present exactly when
        // the lineage proved an application, so its absence rules out
        // `Jailed` — and the hidden peer above rules out `Unconfined`, which
        // would have been told everything. Between them the arm is pinned.
        //
        // The remaining difference between this arm and `Jailed` — the
        // portal grant — is NOT observable here and is not asserted here:
        // nothing can own a portal name until td-portal exists, so both arms
        // answer "no owner". It is pinned in `policy`'s own tests instead.
        one.send(&name_query("GetConnectionCredentials", &name_one, 5));
        let frame = one.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode credentials");
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
            assert_ne!(
                pair.first().and_then(crate::wire::Value::as_str),
                Some("td.AppId"),
                "an unplaceable peer was reported as a confined application"
            );
        }

        // And ListNames shows it the broker and itself, and no one else.
        one.send(&bus_call("ListNames", 6));
        let frame = one.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode ListNames");
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
        assert!(listed.contains(&name_one), "{listed:?}");
        assert!(!listed.contains(&name_two), "the filter leaked: {listed:?}");
    }

    /// The registration gate covers BOTH phases and does not depend on
    /// anybody waiting for the answer.
    ///
    /// `Complete` matters at least as much as `Register` — it is the call
    /// that binds a pid to an instance — and `NO_REPLY_EXPECTED` matters
    /// because §D is explicit that withdrawing the reply withdraws the reply
    /// and NOT the work, so a gate placed after that check would do nothing
    /// for a caller that simply did not ask for an answer.
    #[test]
    fn the_registration_gate_holds_for_both_phases_and_without_a_reply() {
        if !pidfd_available() {
            return;
        }
        let (_bus, instances, mut clients) = confined_bus_watching(1);
        let only = clients.pop().expect("one client");
        let (mut peer, name) = Peer::arrive(only);

        // That this peer is JAILED rather than merely unplaceable, which the
        // refusals below cannot say for themselves: `may_register` denies
        // both arms with the same error, so a regression that made every peer
        // `Unknown` would leave this test green while proving nothing about
        // confinement. `td.AppId` is present exactly when a lineage proved an
        // application.
        peer.send(&name_query("GetConnectionCredentials", &name, 9));
        let frame = peer.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode credentials");
        let entries = reply
            .args()
            .first()
            .and_then(crate::wire::Value::as_seq)
            .expect("a dictionary came back")
            .values(16)
            .expect("read the dictionary");
        let confined = entries.iter().any(|entry| {
            entry
                .as_seq()
                .and_then(|pair| pair.values(2).ok())
                .and_then(|pair| pair.first().and_then(crate::wire::Value::as_str))
                == Some("td.AppId")
        });
        assert!(confined, "this peer is not a confined application");
        let before = instances.pending_count();

        let register = |serial: u32, flags: u8| {
            let mut call = message::Builder::method_call(
                crate::wire::Endian::Little,
                JAIL_PATH,
                Some(JAIL_INTERFACE),
                "Register",
            )
            .destination(BUS_NAME)
            .serial(serial);
            if flags != 0 {
                call = call.flags(flags);
            }
            call.body("ssas", |writer| {
                writer.string("mine")?;
                writer.string("mine")?;
                writer.array("s", |_| Ok(()))
            })
            .expect("body")
            .encode()
            .expect("encode")
        };

        // Phase one, refused.
        peer.send(&register(2, 0));
        let frame = peer.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode");
        assert_eq!(
            reply.fields.error_name,
            Some("org.freedesktop.DBus.Error.AccessDenied")
        );

        // Phase two, refused — and named separately because it is the call
        // that binds a pid to an instance.
        let complete = message::Builder::method_call(
            crate::wire::Endian::Little,
            JAIL_PATH,
            Some(JAIL_INTERFACE),
            "Complete",
        )
        .destination(BUS_NAME)
        .serial(3)
        .body("su", |writer| {
            writer.string("a token this peer should never get to spend")?;
            writer.uint32(1);
            Ok(())
        })
        .expect("body")
        .encode()
        .expect("encode");
        peer.send(&complete);
        let frame = peer.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode");
        assert_eq!(
            reply.fields.error_name,
            Some("org.freedesktop.DBus.Error.AccessDenied"),
            "a confined peer completed a registration"
        );

        // And with the reply withdrawn there is no answer — but the WORK
        // must not have happened either, which no answer can report. §D is
        // explicit that withdrawing the reply withdraws the reply and not the
        // work, so a gate that sat below the no-reply check would register
        // this instance and say nothing. The registry is what says so: a
        // successful phase one leaves a pending registration behind.
        peer.send(&register(4, message::FLAG_NO_REPLY_EXPECTED));
        peer.expect_silence();
        peer.send(&bus_call("GetId", 5));
        let frame = peer.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode");
        assert_eq!(reply.fields.reply_serial, Some(5), "the connection broke");
        assert_eq!(
            instances.pending_count(),
            before,
            "a refused Register opened a registration anyway"
        );
    }

    /// A reply reaches the caller that asked for it.
    ///
    /// The ordinary case, and the one every other test here is the negative
    /// of: the broker carries an answer because it routed the question.
    #[test]
    fn a_reply_reaches_the_caller_that_asked_for_it() {
        let (_bus, mut clients) = bus_of(2);
        let second = clients.pop().expect("two clients");
        let first = clients.pop().expect("two clients");
        let (mut one, name_one) = Peer::arrive(first);
        let (mut two, name_two) = Peer::arrive(second);

        one.send(&peer_call(&name_two, 5, "a question"));
        let frame = two.frame();
        let (call, _) = message::decode(&frame, 0).expect("decode the call");
        assert_eq!(call.fields.sender, Some(name_one.as_str()));

        two.send(&reply_to(&name_one, call.serial, 2));
        let frame = one.frame();
        let (answer, _) = message::decode(&frame, 0).expect("decode the answer");
        assert_eq!(answer.kind, message::MessageType::MethodReturn);
        assert_eq!(answer.fields.reply_serial, Some(5));
        assert_eq!(answer.fields.sender, Some(name_two.as_str()));
    }

    /// A reply nobody asked for is not carried.
    ///
    /// libdbus and GDBus both match a pending call by serial without checking
    /// who answered, so a peer that can place a `METHOD_RETURN` in front of a
    /// client is answering on somebody else's behalf. Before the table there
    /// was nothing to check it against; a draft of the previous commit even
    /// asserted the delivery, to record the gap honestly.
    #[test]
    fn a_reply_nobody_asked_for_is_not_delivered() {
        let (_bus, mut clients) = bus_of(2);
        let second = clients.pop().expect("two clients");
        let first = clients.pop().expect("two clients");
        let (mut one, name_one) = Peer::arrive(first);
        let (mut two, _) = Peer::arrive(second);

        two.send(&reply_to(&name_one, 4321, 2));
        one.expect_silence();

        // And the connection that tried it is still serving, because a
        // forged reply is a bad MESSAGE and not a bad peer: there is no reply
        // to refuse it with, so it is dropped rather than made fatal.
        two.send(&bus_call("GetId", 3));
        let frame = two.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode");
        assert_eq!(reply.fields.reply_serial, Some(3));
    }

    /// The answer must come from the peer the question went to.
    ///
    /// This is the case the SENDER field alone cannot fix: the broker stamps
    /// a truthful sender, and the client library never looks at it. Three
    /// peers, because two cannot express "somebody else answered".
    #[test]
    fn a_reply_from_a_peer_that_was_not_asked_is_not_delivered() {
        let (_bus, mut clients) = bus_of(3);
        let third = clients.pop().expect("three clients");
        let second = clients.pop().expect("three clients");
        let first = clients.pop().expect("three clients");
        let (mut one, name_one) = Peer::arrive(first);
        let (mut two, name_two) = Peer::arrive(second);
        let (mut three, _) = Peer::arrive(third);

        one.send(&peer_call(&name_two, 9, "a question for two"));
        let frame = two.frame();
        let (call, _) = message::decode(&frame, 0).expect("decode the call");

        // Three answers a question it was never asked, with the right serial.
        three.send(&reply_to(&name_one, 9, 2));
        one.expect_silence();

        // And the peer that WAS asked can still answer, so the refusal above
        // did not consume the record it was checked against.
        two.send(&reply_to(&name_one, call.serial, 2));
        let frame = one.frame();
        let (answer, _) = message::decode(&frame, 0).expect("decode the answer");
        assert_eq!(answer.fields.reply_serial, Some(9));
        assert_eq!(answer.fields.sender, Some(name_two.as_str()));
    }

    /// An answer goes to the peer that ASKED, and nowhere else.
    ///
    /// The conjunct this pins is `call.caller == to`. Without it a peer that
    /// was legitimately called by one peer could address its answer to a
    /// THIRD, whose client library would match it against whatever that peer
    /// happens to have outstanding on the same serial.
    #[test]
    fn a_reply_may_not_be_readdressed_to_a_peer_that_did_not_ask() {
        let (_bus, mut clients) = bus_of(3);
        let third = clients.pop().expect("three clients");
        let second = clients.pop().expect("three clients");
        let first = clients.pop().expect("three clients");
        let (mut one, name_one) = Peer::arrive(first);
        let (mut two, name_two) = Peer::arrive(second);
        let (mut three, name_three) = Peer::arrive(third);

        one.send(&peer_call(&name_two, 31, "a question from one"));
        let frame = two.frame();
        let (call, _) = message::decode(&frame, 0).expect("decode the call");

        // Two was asked, and answers THREE with the serial one is waiting on.
        two.send(&reply_to(&name_three, call.serial, 2));
        three.expect_silence();

        // The record was not consumed by the attempt, so the peer that asked
        // is still answerable.
        two.send(&reply_to(&name_one, call.serial, 3));
        let frame = one.frame();
        let (answer, _) = message::decode(&frame, 0).expect("decode the answer");
        assert_eq!(answer.fields.reply_serial, Some(31));
        assert_eq!(answer.fields.sender, Some(name_two.as_str()));
    }

    /// An answer carries the serial of the question it answers.
    ///
    /// The conjunct this pins is `call.serial == reply_serial`. Without it a
    /// peer that was genuinely called could answer with any serial it liked,
    /// and the caller's library would match it against a DIFFERENT call it
    /// has outstanding — the same confusion as an answer from a stranger,
    /// arriving from a peer the caller really is talking to.
    #[test]
    fn a_reply_may_not_answer_a_serial_that_was_never_asked() {
        let (_bus, mut clients) = bus_of(2);
        let second = clients.pop().expect("two clients");
        let first = clients.pop().expect("two clients");
        let (mut one, name_one) = Peer::arrive(first);
        let (mut two, name_two) = Peer::arrive(second);

        one.send(&peer_call(&name_two, 41, "a question from one"));
        let frame = two.frame();
        let (call, _) = message::decode(&frame, 0).expect("decode the call");
        assert_eq!(call.serial, 41);

        // A serial one never sent, from the peer one really did call.
        two.send(&reply_to(&name_one, 4141, 2));
        one.expect_silence();

        two.send(&reply_to(&name_one, 41, 3));
        let frame = one.frame();
        let (answer, _) = message::decode(&frame, 0).expect("decode the answer");
        assert_eq!(answer.fields.reply_serial, Some(41));
        assert_eq!(answer.fields.sender, Some(name_two.as_str()));
    }

    /// Two callees departing hand one caller two DIFFERENT serials.
    ///
    /// The broker is ONE sender, `org.freedesktop.DBus`, and a serial has to
    /// be unique among a sender's undelivered messages. A sweep is written
    /// into the caller's outbox by the DEPARTING connection's thread, so a
    /// counter kept per connection numbered these from two places: two peers
    /// leaving at the same counter value handed one caller two messages from
    /// one sender carrying one serial. The counter belongs to the stream the
    /// messages land in.
    #[test]
    fn a_swept_call_is_numbered_from_the_stream_it_lands_in() {
        let (_bus, mut clients) = bus_of(3);
        let third = clients.pop().expect("three clients");
        let second = clients.pop().expect("three clients");
        let first = clients.pop().expect("three clients");
        let (mut one, _) = Peer::arrive(first);
        let (mut two, name_two) = Peer::arrive(second);
        let (mut three, name_three) = Peer::arrive(third);

        one.send(&peer_call(&name_two, 81, "a question for two"));
        let _ = two.frame();
        one.send(&peer_call(&name_three, 82, "a question for three"));
        let _ = three.frame();

        drop(two);
        drop(three);

        let mut serials = Vec::new();
        let mut answered = Vec::new();
        for _ in 0..2 {
            let frame = one.frame();
            let (told, _) = message::decode(&frame, 0).expect("decode the error");
            assert_eq!(told.fields.sender, Some(BUS_NAME));
            assert_eq!(
                told.fields.error_name,
                Some("org.freedesktop.DBus.Error.NoReply")
            );
            serials.push(told.serial);
            answered.push(told.fields.reply_serial);
        }
        assert_ne!(
            serials.first(), serials.get(1),
            "two peers left at the same counter value and collided"
        );
        answered.sort_unstable();
        assert_eq!(answered, vec![Some(81), Some(82)]);
    }

    /// A departing CALLER takes its unanswered questions with it.
    ///
    /// The other half of the sweep, and the one nothing was pinning. Without
    /// it the table only ever grows: a caller that leaves mid-call would hold
    /// its share of the bound against a name that no longer exists, for as
    /// long as the broker runs.
    #[test]
    fn a_departing_caller_releases_what_it_was_waiting_on() {
        let (bus, mut clients) = bus_of(2);
        let second = clients.pop().expect("two clients");
        let first = clients.pop().expect("two clients");
        let (mut one, _) = Peer::arrive(first);
        let (mut two, name_two) = Peer::arrive(second);

        one.send(&peer_call(&name_two, 51, "a question never answered"));
        let _ = two.frame();
        assert_eq!(bus.pending_replies(), 1);

        drop(one);
        // The departure is another thread's work, so this waits for it rather
        // than assuming it has happened.
        let mut left = false;
        for _ in 0..200 {
            if bus.pending_replies() == 0 {
                left = true;
                break;
            }
            thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(left, "a departed caller is still holding its share");
    }

    /// A caller that withdrew its reply is not recorded, and cannot be
    /// answered.
    ///
    /// `NO_REPLY_EXPECTED` says the sender is not listening, so there is no
    /// serial anybody waits on. That makes a "reply" to one forged by
    /// construction: the ownership test refuses it without needing a rule of
    /// its own, which is the argument for recording only calls that want one.
    #[test]
    fn a_call_that_wants_no_reply_is_neither_recorded_nor_answerable() {
        let (bus, mut clients) = bus_of(2);
        let second = clients.pop().expect("two clients");
        let first = clients.pop().expect("two clients");
        let (mut one, name_one) = Peer::arrive(first);
        let (mut two, name_two) = Peer::arrive(second);

        one.send(&unanswerable_call(&name_two, 61));
        let frame = two.frame();
        let (call, _) = message::decode(&frame, 0).expect("decode the call");
        assert_eq!(call.serial, 61, "the call was delivered all the same");
        assert_eq!(
            bus.pending_replies(),
            0,
            "a call nobody is waiting on took a place in the table"
        );

        two.send(&reply_to(&name_one, 61, 2));
        one.expect_silence();
    }

    /// A caller may not have two calls outstanding on one serial.
    ///
    /// The specification asks for this anyway — a serial is unique among a
    /// connection's undelivered messages — and the table needs it: with a
    /// duplicate allowed, `(caller, serial)` stops naming ONE call, two
    /// answers carry the same `reply_serial` to a client that cannot tell
    /// them apart, and un-recording an undelivered call could remove the
    /// wrong entry and leave a live one untracked.
    #[test]
    fn a_caller_may_not_reuse_a_serial_it_is_still_waiting_on() {
        let (bus, mut clients) = bus_of(3);
        let third = clients.pop().expect("three clients");
        let second = clients.pop().expect("three clients");
        let first = clients.pop().expect("three clients");
        let (mut one, name_one) = Peer::arrive(first);
        let (mut two, name_two) = Peer::arrive(second);
        let (mut three, name_three) = Peer::arrive(third);

        one.send(&peer_call(&name_two, 71, "a question"));
        let _ = two.frame();
        // The same serial, to a different peer.
        one.send(&peer_call(&name_three, 71, "the same serial"));
        let frame = one.frame();
        let (refusal, _) = message::decode(&frame, 0).expect("decode the refusal");
        assert_eq!(refusal.kind, message::MessageType::Error);
        assert_eq!(
            refusal.fields.error_name,
            Some("org.freedesktop.DBus.Error.InvalidArgs")
        );
        assert_eq!(refusal.fields.reply_serial, Some(71));
        three.expect_silence();
        assert_eq!(bus.pending_replies(), 1, "the first call was disturbed");

        // Once the first is answered the serial is free again.
        two.send(&reply_to(&name_one, 71, 2));
        let frame = one.frame();
        let (answer, _) = message::decode(&frame, 0).expect("decode the answer");
        assert_eq!(answer.fields.reply_serial, Some(71));
        one.send(&peer_call(&name_three, 71, "free again"));
        let frame = three.frame();
        let (call, _) = message::decode(&frame, 0).expect("decode the call");
        assert_eq!(call.fields.sender, Some(name_one.as_str()));
    }

    /// One question, one answer. A second reply carrying the same serial is
    /// as forged as a first from the wrong peer.
    #[test]
    fn a_serial_is_answered_once() {
        let (_bus, mut clients) = bus_of(2);
        let second = clients.pop().expect("two clients");
        let first = clients.pop().expect("two clients");
        let (mut one, name_one) = Peer::arrive(first);
        let (mut two, name_two) = Peer::arrive(second);

        one.send(&peer_call(&name_two, 11, "a question"));
        let frame = two.frame();
        let (call, _) = message::decode(&frame, 0).expect("decode the call");

        two.send(&reply_to(&name_one, call.serial, 2));
        let frame = one.frame();
        let (answer, _) = message::decode(&frame, 0).expect("decode");
        assert_eq!(answer.fields.reply_serial, Some(11));

        two.send(&reply_to(&name_one, call.serial, 3));
        one.expect_silence();
    }

    /// A caller whose callee disconnects is TOLD, rather than left waiting on
    /// a serial that will never come.
    ///
    /// §D argues against the "left waiting" failure everywhere else and this
    /// was the one place the broker still committed it.
    #[test]
    fn a_caller_is_told_when_the_peer_it_called_disconnects() {
        let (_bus, mut clients) = bus_of(2);
        let second = clients.pop().expect("two clients");
        let first = clients.pop().expect("two clients");
        let (mut one, _) = Peer::arrive(first);
        let (mut two, name_two) = Peer::arrive(second);

        one.send(&peer_call(&name_two, 13, "a question never answered"));
        // Read at the callee before dropping it, or the call races the
        // departure and is refused `NameHasNoOwner` by the route instead --
        // which is the right answer to a different question.
        let _ = two.frame();
        drop(two);

        let frame = one.frame();
        let (told, _) = message::decode(&frame, 0).expect("decode the error");
        assert_eq!(told.kind, message::MessageType::Error);
        assert_eq!(
            told.fields.error_name,
            Some("org.freedesktop.DBus.Error.NoReply")
        );
        assert_eq!(
            told.fields.reply_serial,
            Some(13),
            "the error answers the call that was lost"
        );
        assert_eq!(told.fields.sender, Some(BUS_NAME));
    }

    /// A confined peer may ANSWER a call it was sent, even though it could
    /// not have placed that call itself.
    ///
    /// The talk set governs who may be ADDRESSED; a reply is governed by
    /// whether it answers a real question. This is the case that separates
    /// them, and it needs two peers with DIFFERENT identities: the caller is
    /// outside a jail and may address anybody, the callee is inside one and
    /// may address only the broker, the portal and itself. Its answer reaches
    /// a peer it could not have called.
    #[test]
    fn a_confined_peer_may_answer_a_call_it_was_sent() {
        if !pidfd_available() {
            return;
        }
        let (_bus, free, jailed) = mixed_bus();
        let (mut caller, name_caller) = Peer::arrive(free);
        let (mut callee, name_callee) = Peer::arrive(jailed);

        // The callee cannot place a call to the caller: it cannot see it.
        callee.send(&peer_call(&name_caller, 2, "refused"));
        let frame = callee.frame();
        let (refusal, _) = message::decode(&frame, 0).expect("decode");
        assert_eq!(
            refusal.fields.error_name,
            Some("org.freedesktop.DBus.Error.NameHasNoOwner"),
            "the two peers are not on opposite sides of the policy"
        );

        // The caller can, because it is outside the jail.
        caller.send(&peer_call(&name_callee, 3, "a question across the boundary"));
        let frame = callee.frame();
        let (call, _) = message::decode(&frame, 0).expect("decode the call");
        assert_eq!(call.fields.sender, Some(name_caller.as_str()));

        // And the answer crosses back, though the address it is going to is
        // one the callee may not send anything else to.
        callee.send(&reply_to(&name_caller, call.serial, 4));
        let frame = caller.frame();
        let (answer, _) = message::decode(&frame, 0).expect("decode the answer");
        assert_eq!(answer.kind, message::MessageType::MethodReturn);
        assert_eq!(answer.fields.reply_serial, Some(3));
        assert_eq!(answer.fields.sender, Some(name_callee.as_str()));
    }

    /// A confined peer may call, and may not signal.
    ///
    /// Addressed to its OWN name, which is the one destination a confined
    /// peer may reach — so the talk set is satisfied and the only thing left
    /// to refuse the signal is the rule about what may be SENT. Any other
    /// destination would be refused for being unseeable and would prove
    /// nothing about message type.
    #[test]
    fn a_confined_peer_may_call_but_may_not_signal() {
        if !pidfd_available() {
            return;
        }
        let (_bus, mut clients) = confined_bus_of(1);
        let only = clients.pop().expect("one client");
        let (mut peer, name) = Peer::arrive(only);

        peer.send(&directed_signal(&name, 2));
        peer.expect_silence();

        // A CALL to the same destination is carried, so the refusal above is
        // about the message type and not about the address.
        peer.send(&peer_call(&name, 3, "to myself"));
        let frame = peer.frame();
        let (call, _) = message::decode(&frame, 0).expect("decode the call");
        assert_eq!(call.kind, message::MessageType::MethodCall);
        assert_eq!(call.fields.sender, Some(name.as_str()));
    }

    /// The control: an unconfined peer's directed signal is carried.
    #[test]
    fn an_unconfined_peer_may_signal() {
        let (_bus, mut clients) = bus_of(1);
        let only = clients.pop().expect("one client");
        let (mut peer, name) = Peer::arrive(only);

        peer.send(&directed_signal(&name, 2));
        let frame = peer.frame();
        let (signal, _) = message::decode(&frame, 0).expect("decode the signal");
        assert_eq!(signal.kind, message::MessageType::Signal);
        assert_eq!(signal.fields.sender, Some(name.as_str()));
    }

    /// A call the broker could not relay is not left outstanding.
    ///
    /// The record is written BEFORE the queue push, so every path that fails
    /// after it has to undo it. Left in place the entry is a call nobody will
    /// ever answer, and it holds the caller's share of the bound until the
    /// connection ends — and since a relay can fail for reasons a THIRD peer
    /// caused, one peer could otherwise spend another's whole allowance.
    ///
    /// Provoked through re-encoding, which is the one relay failure a test can
    /// cause deterministically: the broker adds a SENDER field, so a legal
    /// message whose header fields sit just under the cap cannot be rebuilt.
    #[test]
    fn a_call_that_was_not_relayed_is_not_left_outstanding() {
        let (bus, mut clients) = bus_of(2);
        let second = clients.pop().expect("two clients");
        let first = clients.pop().expect("two clients");
        let (mut one, _) = Peer::arrive(first);
        let (mut two, name_two) = Peer::arrive(second);

        // The longest object path this codec accepts in a message with these
        // fields — asked rather than restated, as the re-encoding test does.
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
            .serial(21)
            .encode();
            if let Ok(frame) = built {
                brimful = Some(frame);
                break;
            }
        }
        let brimful = brimful.expect("no path length fits inside the field cap");

        one.send(&brimful);
        let frame = one.frame();
        let (refusal, _) = message::decode(&frame, 0).expect("decode the refusal");
        assert_eq!(
            refusal.fields.error_name,
            Some("org.freedesktop.DBus.Error.LimitsExceeded")
        );
        assert_eq!(
            bus.pending_replies(),
            0,
            "a call that never reached its callee is still holding a place"
        );

        // And the caller can still use the place it did not spend: a call
        // that CAN be relayed is recorded and delivered as usual.
        one.send(&peer_call(&name_two, 22, "a question that fits"));
        let frame = two.frame();
        let (call, _) = message::decode(&frame, 0).expect("decode the call");
        assert_eq!(call.kind, message::MessageType::MethodCall);
        assert_eq!(bus.pending_replies(), 1);
    }

    /// A name goes to whoever asks first, and the asking is announced.
    #[test]
    fn a_name_goes_to_whoever_asks_for_it_first() {
        let (_bus, mut clients) = bus_of(2);
        let second = clients.pop().expect("two clients");
        let first = clients.pop().expect("two clients");
        let (mut one, _) = Peer::arrive(first);
        let (mut two, _) = Peer::arrive(second);

        one.send(&request_name("org.example.Thing", 0, 2));
        assert_eq!(name_code(&one.answer()), (1, Some(2)), "not the primary owner");
        // The answer comes first, then the news, in the order `say_hello`
        // uses: what this peer asked about goes ahead of what it did not.
        assert_eq!(
            announced(&one.frame()),
            ("NameAcquired".to_string(), "org.example.Thing".to_string())
        );

        // A second asker without DO_NOT_QUEUE waits.
        two.send(&request_name("org.example.Thing", 0, 2));
        assert_eq!(name_code(&two.answer()), (2, Some(2)), "not queued");
        two.expect_silence();

        // Asking again updates the flags and says which place it holds.
        one.send(&request_name("org.example.Thing", 0, 3));
        assert_eq!(name_code(&one.answer()), (4, Some(3)), "not already owner");
        two.send(&request_name("org.example.Thing", 0, 3));
        assert_eq!(name_code(&two.answer()), (2, Some(3)), "not still queued");
    }

    /// A caller that will not wait is told the name exists.
    #[test]
    fn a_caller_that_will_not_queue_is_told_the_name_exists() {
        let (bus, mut clients) = bus_of(2);
        let second = clients.pop().expect("two clients");
        let first = clients.pop().expect("two clients");
        let (mut one, name_one) = Peer::arrive(first);
        let (mut two, _) = Peer::arrive(second);

        one.send(&request_name("org.example.Thing", 0, 2));
        assert_eq!(name_code(&one.answer()).0, 1);
        let _ = one.frame();

        two.send(&request_name(
            "org.example.Thing",
            crate::registry::NAME_FLAG_DO_NOT_QUEUE,
            2,
        ));
        assert_eq!(name_code(&two.answer()), (3, Some(2)), "not told it exists");
        assert!(
            bus.holds(&name_one, "org.example.Thing"),
            "the refused caller took the name anyway"
        );
    }

    /// A well-known name ROUTES, which is the whole use of holding one.
    ///
    /// And the reply comes back, which is what the previous rung could not
    /// test: it recorded the callee as the connection it RESOLVED rather than
    /// the name the caller wrote, precisely so that a reply from the holder
    /// would still claim its record. Nothing could observe that while only
    /// unique names routed.
    #[test]
    fn a_call_to_a_well_known_name_reaches_its_holder_and_is_answered() {
        let (_bus, mut clients) = bus_of(2);
        let second = clients.pop().expect("two clients");
        let first = clients.pop().expect("two clients");
        let (mut one, name_one) = Peer::arrive(first);
        let (mut two, name_two) = Peer::arrive(second);

        two.send(&request_name("org.example.Thing", 0, 2));
        assert_eq!(name_code(&two.answer()).0, 1);
        let _ = two.frame();

        one.send(&peer_call("org.example.Thing", 5, "a question by name"));
        let frame = two.frame();
        let (call, _) = message::decode(&frame, 0).expect("decode the call");
        assert_eq!(call.fields.sender, Some(name_one.as_str()));
        assert_eq!(
            call.fields.destination,
            Some("org.example.Thing"),
            "the destination a caller wrote is not rewritten"
        );

        // The holder answers under its UNIQUE name, and the record was
        // written against that, so the answer is carried.
        two.send(&reply_to(&name_one, call.serial, 3));
        let frame = one.frame();
        let (answer, _) = message::decode(&frame, 0).expect("decode the answer");
        assert_eq!(answer.fields.reply_serial, Some(5));
        assert_eq!(answer.fields.sender, Some(name_two.as_str()));
    }

    /// A caller waiting on a well-known name is told when its holder goes.
    ///
    /// The other half of the same guard: the departure sweep looks for the
    /// departing peer's UNIQUE name, so a call recorded against the written
    /// name would be invisible to it and its caller would wait for ever.
    #[test]
    fn a_call_by_name_is_answered_when_the_holder_departs() {
        let (_bus, mut clients) = bus_of(2);
        let second = clients.pop().expect("two clients");
        let first = clients.pop().expect("two clients");
        let (mut one, _) = Peer::arrive(first);
        let (mut two, _) = Peer::arrive(second);

        two.send(&request_name("org.example.Thing", 0, 2));
        assert_eq!(name_code(&two.answer()).0, 1);
        let _ = two.frame();

        one.send(&peer_call("org.example.Thing", 7, "a question by name"));
        let _ = two.frame();
        drop(two);

        let frame = one.frame();
        let (told, _) = message::decode(&frame, 0).expect("decode the error");
        assert_eq!(
            told.fields.error_name,
            Some("org.freedesktop.DBus.Error.NoReply")
        );
        assert_eq!(told.fields.reply_serial, Some(7));
    }

    /// Releasing a name hands it to whoever was waiting.
    #[test]
    fn releasing_a_name_advances_the_queue() {
        let (bus, mut clients) = bus_of(2);
        let second = clients.pop().expect("two clients");
        let first = clients.pop().expect("two clients");
        let (mut one, name_one) = Peer::arrive(first);
        let (mut two, name_two) = Peer::arrive(second);

        one.send(&request_name("org.example.Thing", 0, 2));
        assert_eq!(name_code(&one.answer()).0, 1);
        let _ = one.frame();
        two.send(&request_name("org.example.Thing", 0, 2));
        assert_eq!(name_code(&two.answer()).0, 2);

        one.send(&release_name("org.example.Thing", 3));
        assert_eq!(name_code(&one.answer()), (1, Some(3)), "not released");
        assert_eq!(
            announced(&one.frame()),
            ("NameLost".to_string(), "org.example.Thing".to_string())
        );
        assert_eq!(
            announced(&two.frame()),
            ("NameAcquired".to_string(), "org.example.Thing".to_string())
        );
        assert!(bus.holds(&name_two, "org.example.Thing"));
        assert!(!bus.holds(&name_one, "org.example.Thing"));

        // And it routes to the new holder.
        one.send(&peer_call("org.example.Thing", 8, "for the new holder"));
        let frame = two.frame();
        let (call, _) = message::decode(&frame, 0).expect("decode the call");
        assert_eq!(call.fields.sender, Some(name_one.as_str()));
    }

    /// A departing holder hands the name on in the same act as its departure.
    #[test]
    fn a_departing_holder_hands_the_name_on() {
        let (bus, mut clients) = bus_of(2);
        let second = clients.pop().expect("two clients");
        let first = clients.pop().expect("two clients");
        let (mut one, _) = Peer::arrive(first);
        let (mut two, name_two) = Peer::arrive(second);

        one.send(&request_name("org.example.Thing", 0, 2));
        assert_eq!(name_code(&one.answer()).0, 1);
        let _ = one.frame();
        two.send(&request_name("org.example.Thing", 0, 2));
        assert_eq!(name_code(&two.answer()).0, 2);

        drop(one);
        assert_eq!(
            announced(&two.frame()),
            ("NameAcquired".to_string(), "org.example.Thing".to_string())
        );
        assert!(bus.holds(&name_two, "org.example.Thing"));
    }

    /// A holder that consents may be replaced, and goes to the FRONT of the
    /// queue rather than the back.
    #[test]
    fn a_consenting_holder_is_replaced_and_keeps_its_place() {
        let (bus, mut clients) = bus_of(2);
        let second = clients.pop().expect("two clients");
        let first = clients.pop().expect("two clients");
        let (mut one, name_one) = Peer::arrive(first);
        let (mut two, name_two) = Peer::arrive(second);

        one.send(&request_name(
            "org.example.Thing",
            crate::registry::NAME_FLAG_ALLOW_REPLACEMENT,
            2,
        ));
        assert_eq!(name_code(&one.answer()).0, 1);
        let _ = one.frame();

        two.send(&request_name(
            "org.example.Thing",
            crate::registry::NAME_FLAG_REPLACE_EXISTING,
            2,
        ));
        assert_eq!(name_code(&two.answer()), (1, Some(2)), "not the new owner");
        assert_eq!(
            announced(&two.frame()),
            ("NameAcquired".to_string(), "org.example.Thing".to_string())
        );
        assert_eq!(
            announced(&one.frame()),
            ("NameLost".to_string(), "org.example.Thing".to_string())
        );
        assert!(bus.holds(&name_two, "org.example.Thing"));

        // The displaced holder is still waiting, and at the front: when the
        // replacement leaves, the name comes back to it.
        drop(two);
        assert_eq!(
            announced(&one.frame()),
            ("NameAcquired".to_string(), "org.example.Thing".to_string())
        );
        assert!(bus.holds(&name_one, "org.example.Thing"));
    }

    /// The other half of "both sides": a holder that CONSENTS is still not
    /// replaced by a caller that did not ask to replace it.
    #[test]
    fn a_caller_that_did_not_ask_to_replace_does_not() {
        let (bus, mut clients) = bus_of(2);
        let second = clients.pop().expect("two clients");
        let first = clients.pop().expect("two clients");
        let (mut one, name_one) = Peer::arrive(first);
        let (mut two, _) = Peer::arrive(second);

        one.send(&request_name(
            "org.example.Thing",
            crate::registry::NAME_FLAG_ALLOW_REPLACEMENT,
            2,
        ));
        assert_eq!(name_code(&one.answer()).0, 1);
        let _ = one.frame();

        two.send(&request_name("org.example.Thing", 0, 2));
        assert_eq!(name_code(&two.answer()), (2, Some(2)), "it took the name");
        assert!(
            bus.holds(&name_one, "org.example.Thing"),
            "a consenting holder was replaced by a caller that did not ask"
        );
        one.expect_silence();
    }

    /// A displaced holder goes to the FRONT of the queue, ahead of anyone who
    /// was already waiting.
    ///
    /// Three peers, because with two the front and the back of the queue are
    /// the same place and the rule cannot be told from its opposite.
    #[test]
    fn a_displaced_holder_goes_ahead_of_whoever_was_waiting() {
        let (bus, mut clients) = bus_of(3);
        let third = clients.pop().expect("three clients");
        let second = clients.pop().expect("three clients");
        let first = clients.pop().expect("three clients");
        let (mut one, name_one) = Peer::arrive(first);
        let (mut two, _) = Peer::arrive(second);
        let (mut three, _) = Peer::arrive(third);

        one.send(&request_name(
            "org.example.Thing",
            crate::registry::NAME_FLAG_ALLOW_REPLACEMENT,
            2,
        ));
        assert_eq!(name_code(&one.answer()).0, 1);
        let _ = one.frame();

        // Two is waiting BEFORE three arrives, so a queue that appended the
        // displaced holder would hand the name to two next.
        two.send(&request_name("org.example.Thing", 0, 2));
        assert_eq!(name_code(&two.answer()).0, 2);

        three.send(&request_name(
            "org.example.Thing",
            crate::registry::NAME_FLAG_REPLACE_EXISTING,
            2,
        ));
        assert_eq!(name_code(&three.answer()).0, 1);
        let _ = three.frame();
        assert_eq!(
            announced(&one.frame()),
            ("NameLost".to_string(), "org.example.Thing".to_string())
        );

        drop(three);
        assert_eq!(
            announced(&one.frame()),
            ("NameAcquired".to_string(), "org.example.Thing".to_string()),
            "the name went to whoever asked longest ago rather than back"
        );
        assert!(bus.holds(&name_one, "org.example.Thing"));
        two.expect_silence();
    }

    /// A displaced holder that asked NOT to be queued is not queued.
    ///
    /// The flag says what happens when the name is taken away as well as
    /// whether to wait for it in the first place.
    #[test]
    fn a_displaced_holder_that_will_not_queue_does_not_come_back() {
        let (bus, mut clients) = bus_of(2);
        let second = clients.pop().expect("two clients");
        let first = clients.pop().expect("two clients");
        let (mut one, name_one) = Peer::arrive(first);
        let (mut two, name_two) = Peer::arrive(second);

        one.send(&request_name(
            "org.example.Thing",
            crate::registry::NAME_FLAG_ALLOW_REPLACEMENT
                | crate::registry::NAME_FLAG_DO_NOT_QUEUE,
            2,
        ));
        assert_eq!(name_code(&one.answer()).0, 1);
        let _ = one.frame();

        two.send(&request_name(
            "org.example.Thing",
            crate::registry::NAME_FLAG_REPLACE_EXISTING,
            2,
        ));
        assert_eq!(name_code(&two.answer()).0, 1);
        let _ = two.frame();
        assert_eq!(
            announced(&one.frame()),
            ("NameLost".to_string(), "org.example.Thing".to_string())
        );

        // And when the new holder goes, the name goes with it rather than
        // back to the peer that said it would not wait.
        drop(two);
        one.expect_silence();
        assert!(!bus.holds(&name_one, "org.example.Thing"));
        assert!(!bus.holds(&name_two, "org.example.Thing"));
    }

    /// Asking again with different flags CHANGES them.
    ///
    /// Nothing pinned this: the one test that re-requested did so with the
    /// same flags it started with, so an implementation that dropped the
    /// update on the floor answered identically.
    #[test]
    fn asking_again_changes_the_flags_it_carries() {
        let (bus, mut clients) = bus_of(2);
        let second = clients.pop().expect("two clients");
        let first = clients.pop().expect("two clients");
        let (mut one, name_one) = Peer::arrive(first);
        let (mut two, name_two) = Peer::arrive(second);

        // Held WITHOUT consent to replacement, so a replacement now fails.
        one.send(&request_name("org.example.Thing", 0, 2));
        assert_eq!(name_code(&one.answer()).0, 1);
        let _ = one.frame();
        two.send(&request_name(
            "org.example.Thing",
            crate::registry::NAME_FLAG_REPLACE_EXISTING,
            2,
        ));
        assert_eq!(name_code(&two.answer()).0, 2, "it was replaced too early");

        // The same peer asks again and now consents.
        one.send(&request_name(
            "org.example.Thing",
            crate::registry::NAME_FLAG_ALLOW_REPLACEMENT,
            3,
        ));
        assert_eq!(name_code(&one.answer()), (4, Some(3)), "not already owner");

        // Which the next replacement can now use.
        two.send(&request_name(
            "org.example.Thing",
            crate::registry::NAME_FLAG_REPLACE_EXISTING,
            4,
        ));
        assert_eq!(
            name_code(&two.answer()),
            (1, Some(4)),
            "the flags never changed"
        );
        assert!(bus.holds(&name_two, "org.example.Thing"));
        assert!(!bus.holds(&name_one, "org.example.Thing"));
    }

    /// A departing connection gives up EVERY name it held, not the first.
    ///
    /// Three at once, because the cleanup removes emptied entries by index
    /// with `swap_remove` — which moves the last entry into the hole — and
    /// with one name that arithmetic cannot be told from its opposite. One of
    /// the three has a peer waiting behind it, so the walk has to both hand a
    /// name on and drop two entries in the same pass.
    #[test]
    fn a_departing_holder_gives_up_every_name_it_held() {
        let (bus, mut clients) = bus_of(2);
        let second = clients.pop().expect("two clients");
        let first = clients.pop().expect("two clients");
        let (mut one, _) = Peer::arrive(first);
        let (mut two, name_two) = Peer::arrive(second);

        let mut serial = 2u32;
        for name in [
            "org.example.First",
            "org.example.Second",
            "org.example.Third",
        ] {
            one.send(&request_name(name, 0, serial));
            assert_eq!(name_code(&one.answer()).0, 1, "{name} was refused");
            let _ = one.frame();
            serial = serial.saturating_add(1);
        }
        // Two waits behind the middle one only.
        two.send(&request_name("org.example.Second", 0, 2));
        assert_eq!(name_code(&two.answer()).0, 2);

        drop(one);

        assert_eq!(
            announced(&two.frame()),
            ("NameAcquired".to_string(), "org.example.Second".to_string())
        );
        assert!(bus.holds(&name_two, "org.example.Second"));

        // The other two are nobody's, and can be taken. `DO_NOT_QUEUE`, so an
        // entry that survived its holder would answer `EXISTS` rather than
        // quietly queueing behind a connection that has gone.
        let mut serial = 3u32;
        for name in ["org.example.First", "org.example.Third"] {
            two.send(&request_name(
                name,
                crate::registry::NAME_FLAG_DO_NOT_QUEUE,
                serial,
            ));
            assert_eq!(
                name_code(&two.answer()),
                (1, Some(serial)),
                "{name} outlived the connection that held it"
            );
            let _ = two.frame();
            serial = serial.saturating_add(1);
        }
    }

    /// A handover does not grow the queue past its bound.
    ///
    /// The replacement itself is not refused for want of room — a queue full
    /// of bystanders must not be able to freeze a handover two peers have
    /// agreed on — so what gives way is the courtesy of re-queueing the
    /// displaced holder. The bound holds either way, and the queue's length
    /// is invisible from the wire, so this asks the directory.
    #[test]
    fn a_handover_does_not_grow_the_queue_past_its_bound() {
        let peers = crate::registry::MAX_NAME_QUEUE + 1;
        let (bus, clients) = bus_of(peers);
        let mut arrived: Vec<Peer> = clients
            .into_iter()
            .map(|client| Peer::arrive(client).0)
            .collect();

        // The holder consents, and the queue fills behind it.
        for (which, peer) in arrived.iter_mut().enumerate() {
            if which == crate::registry::MAX_NAME_QUEUE {
                break;
            }
            let flags = if which == 0 {
                crate::registry::NAME_FLAG_ALLOW_REPLACEMENT
            } else {
                0
            };
            peer.send(&request_name("org.example.Thing", flags, 2));
            let expected = if which == 0 { 1 } else { 2 };
            assert_eq!(name_code(&peer.answer()).0, expected, "peer {which}");
        }
        assert_eq!(
            bus.wanting("org.example.Thing"),
            crate::registry::MAX_NAME_QUEUE
        );

        // The last one takes the name from a queue with no room to put the
        // displaced holder back into.
        let Some(last) = arrived.get_mut(crate::registry::MAX_NAME_QUEUE) else {
            panic!("a peer for every place and one more");
        };
        last.send(&request_name(
            "org.example.Thing",
            crate::registry::NAME_FLAG_REPLACE_EXISTING,
            2,
        ));
        assert_eq!(name_code(&last.answer()).0, 1, "the handover was refused");
        assert!(
            bus.wanting("org.example.Thing") <= crate::registry::MAX_NAME_QUEUE,
            "a handover grew the queue to {}",
            bus.wanting("org.example.Thing")
        );
    }

    /// A WAITING caller's flags are updated too, not only a holder's.
    ///
    /// They matter when it reaches the front: a peer that queued without
    /// consenting to replacement and then changed its mind has to be
    /// replaceable once it holds the name.
    #[test]
    fn asking_again_changes_the_flags_of_a_caller_that_is_waiting() {
        let (bus, mut clients) = bus_of(3);
        let third = clients.pop().expect("three clients");
        let second = clients.pop().expect("three clients");
        let first = clients.pop().expect("three clients");
        let (mut one, _) = Peer::arrive(first);
        let (mut two, _) = Peer::arrive(second);
        let (mut three, name_three) = Peer::arrive(third);

        one.send(&request_name("org.example.Thing", 0, 2));
        assert_eq!(name_code(&one.answer()).0, 1);
        let _ = one.frame();

        // Two queues WITHOUT consenting, then changes its mind.
        two.send(&request_name("org.example.Thing", 0, 2));
        assert_eq!(name_code(&two.answer()).0, 2);
        two.send(&request_name(
            "org.example.Thing",
            crate::registry::NAME_FLAG_ALLOW_REPLACEMENT,
            3,
        ));
        assert_eq!(name_code(&two.answer()), (2, Some(3)), "not still queued");

        // It reaches the front, carrying the flags it asked for second.
        one.send(&release_name("org.example.Thing", 4));
        assert_eq!(name_code(&one.answer()).0, 1);
        let _ = one.frame();
        assert_eq!(
            announced(&two.frame()),
            ("NameAcquired".to_string(), "org.example.Thing".to_string())
        );

        three.send(&request_name(
            "org.example.Thing",
            crate::registry::NAME_FLAG_REPLACE_EXISTING,
            2,
        ));
        assert_eq!(
            name_code(&three.answer()),
            (1, Some(2)),
            "a waiting caller's change of mind was dropped"
        );
        assert!(bus.holds(&name_three, "org.example.Thing"));
    }

    /// A name given up completely can be taken again.
    ///
    /// The entry has to GO when its queue empties, not linger holding a name
    /// nobody owns: a lingering entry answers `EXISTS` to a caller that will
    /// not queue, and queues a caller that will — behind nobody, for ever.
    #[test]
    fn a_name_given_up_completely_can_be_taken_again() {
        let (bus, mut clients) = bus_of(2);
        let second = clients.pop().expect("two clients");
        let first = clients.pop().expect("two clients");
        let (mut one, _) = Peer::arrive(first);
        let (mut two, name_two) = Peer::arrive(second);

        one.send(&request_name("org.example.Thing", 0, 2));
        assert_eq!(name_code(&one.answer()).0, 1);
        let _ = one.frame();
        one.send(&release_name("org.example.Thing", 3));
        assert_eq!(name_code(&one.answer()).0, 1);
        let _ = one.frame();

        // With DO_NOT_QUEUE, so a lingering entry would answer `EXISTS`
        // rather than quietly queueing behind nobody.
        two.send(&request_name(
            "org.example.Thing",
            crate::registry::NAME_FLAG_DO_NOT_QUEUE,
            2,
        ));
        assert_eq!(
            name_code(&two.answer()),
            (1, Some(2)),
            "a name nobody holds was not free"
        );
        assert!(bus.holds(&name_two, "org.example.Thing"));
    }

    /// A departing peer leaves the QUEUE as well as the ownership.
    ///
    /// Otherwise the name is handed to a connection that has gone: it routes
    /// nowhere, the announcement reaches nobody, and the name is held by a
    /// socket that is closed.
    #[test]
    fn a_departing_peer_leaves_the_queue_it_was_waiting_in() {
        let (bus, mut clients) = bus_of(3);
        let third = clients.pop().expect("three clients");
        let second = clients.pop().expect("three clients");
        let first = clients.pop().expect("three clients");
        let (mut one, _) = Peer::arrive(first);
        let (mut two, name_two) = Peer::arrive(second);
        let (mut three, name_three) = Peer::arrive(third);

        one.send(&request_name("org.example.Thing", 0, 2));
        assert_eq!(name_code(&one.answer()).0, 1);
        let _ = one.frame();
        two.send(&request_name("org.example.Thing", 0, 2));
        assert_eq!(name_code(&two.answer()).0, 2);

        // The waiter goes before the holder does.
        drop(two);
        // The departure is another thread's work; wait for it rather than
        // assuming it has happened.
        let mut gone = false;
        for _ in 0..200 {
            if bus.route(&name_two).is_none() {
                gone = true;
                break;
            }
            thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(gone, "the waiter never left");

        one.send(&release_name("org.example.Thing", 3));
        assert_eq!(name_code(&one.answer()).0, 1);
        let _ = one.frame();

        // Nobody holds it now, so it is there to be taken.
        three.send(&request_name(
            "org.example.Thing",
            crate::registry::NAME_FLAG_DO_NOT_QUEUE,
            2,
        ));
        assert_eq!(
            name_code(&three.answer()),
            (1, Some(2)),
            "the name was handed to a connection that had gone"
        );
        assert!(bus.holds(&name_three, "org.example.Thing"));
    }

    /// Replacement needs BOTH sides to agree.
    #[test]
    fn a_holder_that_did_not_consent_is_not_replaced() {
        let (bus, mut clients) = bus_of(2);
        let second = clients.pop().expect("two clients");
        let first = clients.pop().expect("two clients");
        let (mut one, name_one) = Peer::arrive(first);
        let (mut two, _) = Peer::arrive(second);

        one.send(&request_name("org.example.Thing", 0, 2));
        assert_eq!(name_code(&one.answer()).0, 1);
        let _ = one.frame();

        two.send(&request_name(
            "org.example.Thing",
            crate::registry::NAME_FLAG_REPLACE_EXISTING,
            2,
        ));
        assert_eq!(name_code(&two.answer()), (2, Some(2)), "it was replaced");
        assert!(bus.holds(&name_one, "org.example.Thing"));
        one.expect_silence();
    }

    /// Asking again with `DO_NOT_QUEUE` is how a client LEAVES a queue.
    ///
    /// A draft updated the flags of a caller already in the queue and
    /// returned `IN_QUEUE` whatever they said, so a client that changed its
    /// mind stayed down as a future owner and was told `EXISTS` by a broker
    /// that still had it waiting.
    #[test]
    fn asking_again_without_queueing_leaves_the_queue() {
        let (bus, mut clients) = bus_of(3);
        let third = clients.pop().expect("three clients");
        let second = clients.pop().expect("three clients");
        let first = clients.pop().expect("three clients");
        let (mut one, _) = Peer::arrive(first);
        let (mut two, _) = Peer::arrive(second);
        let (mut three, name_three) = Peer::arrive(third);

        one.send(&request_name("org.example.Thing", 0, 2));
        assert_eq!(name_code(&one.answer()).0, 1);
        let _ = one.frame();
        two.send(&request_name("org.example.Thing", 0, 2));
        assert_eq!(name_code(&two.answer()).0, 2);
        three.send(&request_name("org.example.Thing", 0, 2));
        assert_eq!(name_code(&three.answer()).0, 2);

        // Two changes its mind, and is told the name exists rather than that
        // it is still queued.
        two.send(&request_name(
            "org.example.Thing",
            crate::registry::NAME_FLAG_DO_NOT_QUEUE,
            3,
        ));
        assert_eq!(name_code(&two.answer()), (3, Some(3)), "it stayed in the queue");

        // And it really left: the holder's release goes past it to three.
        one.send(&release_name("org.example.Thing", 4));
        assert_eq!(name_code(&one.answer()).0, 1);
        let _ = one.frame();
        assert_eq!(
            announced(&three.frame()),
            ("NameAcquired".to_string(), "org.example.Thing".to_string())
        );
        assert!(bus.holds(&name_three, "org.example.Thing"));
        two.expect_silence();
    }

    /// A caller already in the queue may still REPLACE a consenting holder.
    ///
    /// A draft answered "still queued" to a client that had queued and then
    /// asked again with `REPLACE_EXISTING`, leaving it waiting on a holder
    /// that had agreed to be replaced.
    #[test]
    fn a_queued_caller_may_still_replace_a_consenting_holder() {
        let (bus, mut clients) = bus_of(2);
        let second = clients.pop().expect("two clients");
        let first = clients.pop().expect("two clients");
        let (mut one, name_one) = Peer::arrive(first);
        let (mut two, name_two) = Peer::arrive(second);

        one.send(&request_name(
            "org.example.Thing",
            crate::registry::NAME_FLAG_ALLOW_REPLACEMENT,
            2,
        ));
        assert_eq!(name_code(&one.answer()).0, 1);
        let _ = one.frame();

        two.send(&request_name("org.example.Thing", 0, 2));
        assert_eq!(name_code(&two.answer()).0, 2, "not queued to begin with");

        two.send(&request_name(
            "org.example.Thing",
            crate::registry::NAME_FLAG_REPLACE_EXISTING,
            3,
        ));
        assert_eq!(
            name_code(&two.answer()),
            (1, Some(3)),
            "a queued caller could not take a name offered to it"
        );
        assert!(bus.holds(&name_two, "org.example.Thing"));
        assert_eq!(
            announced(&two.frame()),
            ("NameAcquired".to_string(), "org.example.Thing".to_string())
        );
        assert_eq!(
            announced(&one.frame()),
            ("NameLost".to_string(), "org.example.Thing".to_string())
        );
        // And it is in the queue ONCE: when the new holder goes, the name
        // comes back to the peer that consented, not to a stale second copy
        // of the peer that took it.
        drop(two);
        assert_eq!(
            announced(&one.frame()),
            ("NameAcquired".to_string(), "org.example.Thing".to_string())
        );
        assert!(bus.holds(&name_one, "org.example.Thing"));
    }

    /// The per-connection bound is charged when a name is TAKEN as well as
    /// when it is queued for.
    ///
    /// A draft checked it only on the queueing path, so a peer at its bound
    /// could go on collecting names by displacing consenting owners.
    #[test]
    fn taking_a_name_from_its_holder_is_charged_to_the_bound() {
        let (bus, mut clients) = bus_of(2);
        let second = clients.pop().expect("two clients");
        let first = clients.pop().expect("two clients");
        let (mut one, name_one) = Peer::arrive(first);
        let (mut two, _) = Peer::arrive(second);

        // Two fills its allowance with names of its own.
        let mut serial = 2u32;
        for which in 0..crate::registry::MAX_NAMES_PER_CONNECTION {
            two.send(&request_name(&format!("org.example.N{which}"), 0, serial));
            assert_eq!(name_code(&two.answer()).0, 1);
            let _ = two.frame();
            serial = serial.saturating_add(1);
        }

        // One offers a name up.
        one.send(&request_name(
            "org.example.Offered",
            crate::registry::NAME_FLAG_ALLOW_REPLACEMENT,
            2,
        ));
        assert_eq!(name_code(&one.answer()).0, 1);
        let _ = one.frame();

        two.send(&request_name(
            "org.example.Offered",
            crate::registry::NAME_FLAG_REPLACE_EXISTING,
            serial,
        ));
        assert_eq!(
            name_code(&two.answer()),
            (3, Some(serial)),
            "the bound did not hold against a replacement"
        );
        assert!(
            bus.holds(&name_one, "org.example.Offered"),
            "a peer at its bound took another name anyway"
        );
        one.expect_silence();
    }

    /// `ReleaseName`'s three answers.
    #[test]
    fn releasing_says_which_of_the_three_things_happened() {
        let (_bus, mut clients) = bus_of(2);
        let second = clients.pop().expect("two clients");
        let first = clients.pop().expect("two clients");
        let (mut one, _) = Peer::arrive(first);
        let (mut two, _) = Peer::arrive(second);

        // Nobody holds it.
        one.send(&release_name("org.example.Nothing", 2));
        assert_eq!(name_code(&one.answer()), (2, Some(2)), "not non-existent");

        one.send(&request_name("org.example.Thing", 0, 3));
        assert_eq!(name_code(&one.answer()).0, 1);
        let _ = one.frame();

        // Somebody holds it and it is not this caller.
        two.send(&release_name("org.example.Thing", 4));
        assert_eq!(name_code(&two.answer()), (3, Some(4)), "not refused");

        // A caller that was only WAITING releases too: it no longer wants it.
        two.send(&request_name("org.example.Thing", 0, 5));
        assert_eq!(name_code(&two.answer()).0, 2);
        two.send(&release_name("org.example.Thing", 6));
        assert_eq!(name_code(&two.answer()), (1, Some(6)), "a waiter cannot leave");
        // And leaving the queue is not a handover: the holder keeps it and is
        // told nothing.
        one.expect_silence();
    }

    /// `NO_REPLY_EXPECTED` withdraws the REPLY, not the work.
    ///
    /// Both of these CHANGE something, so they sit above the gate that answers
    /// nothing to a caller that is not waiting. A draft left them below it and
    /// silently did nothing at all — the same shape of mistake the jail
    /// registration made, and caught the same way: by reading the effect
    /// rather than the wire, since a call that wants no reply has no wire to
    /// read.
    #[test]
    fn a_name_call_that_wants_no_reply_is_still_performed() {
        let (bus, mut clients) = bus_of(1);
        let only = clients.pop().expect("one client");
        let (mut peer, name) = Peer::arrive(only);

        fn quiet(member: &str, serial: u32) -> message::Builder<'_> {
            message::Builder::method_call(
                crate::wire::Endian::Little,
                BUS_PATH,
                Some(BUS_NAME),
                member,
            )
            .destination(BUS_NAME)
            .flags(message::FLAG_NO_REPLY_EXPECTED)
            .serial(serial)
        }

        let taken = quiet("RequestName", 2)
            .body("su", |writer| {
                writer.string("org.example.Quiet")?;
                writer.uint32(0);
                Ok(())
            })
            .expect("body")
            .encode()
            .expect("encode");
        peer.send(&taken);

        // No reply — but the announcement still comes, and the directory
        // agrees.
        assert_eq!(
            announced(&peer.frame()),
            ("NameAcquired".to_string(), "org.example.Quiet".to_string())
        );
        assert!(
            bus.holds(&name, "org.example.Quiet"),
            "a quiet RequestName did nothing"
        );

        let given = quiet("ReleaseName", 3)
            .body("s", |writer| writer.string("org.example.Quiet"))
            .expect("body")
            .encode()
            .expect("encode");
        peer.send(&given);
        assert_eq!(
            announced(&peer.frame()),
            ("NameLost".to_string(), "org.example.Quiet".to_string())
        );
        assert!(
            !bus.holds(&name, "org.example.Quiet"),
            "a quiet ReleaseName did nothing"
        );

        // And a quiet call with the WRONG arguments earns no error, because
        // the caller said it is not listening for one.
        let malformed = quiet("RequestName", 4)
            .body("s", |writer| writer.string("org.example.Quiet"))
            .expect("body")
            .encode()
            .expect("encode");
        peer.send(&malformed);
        peer.expect_silence();
        assert!(!bus.holds(&name, "org.example.Quiet"));
    }

    /// A reserved name is refused to everyone, including a caller this design
    /// otherwise leaves unrestricted.
    #[test]
    fn a_reserved_name_is_refused_to_an_unconfined_caller() {
        let (_bus, mut clients) = bus_of(1);
        let only = clients.pop().expect("one client");
        let (mut peer, _) = Peer::arrive(only);

        let mut serial = 2u32;
        for name in [
            "org.freedesktop.DBus",
            "org.freedesktop.portal.Desktop",
            "org.freedesktop.impl.portal.Access",
        ] {
            peer.send(&request_name(name, 0, serial));
            let frame = peer.frame();
            let (refusal, _) = message::decode(&frame, 0).expect("decode");
            assert_eq!(
                refusal.fields.error_name,
                Some("org.freedesktop.DBus.Error.AccessDenied"),
                "{name} was not refused"
            );
            assert_eq!(refusal.fields.reply_serial, Some(serial));
            serial = serial.saturating_add(1);
        }
    }

    /// §D's default sandboxed policy owns no name.
    #[test]
    fn a_confined_peer_may_own_no_name() {
        if !pidfd_available() {
            return;
        }
        let (bus, mut clients) = confined_bus_of(1);
        let only = clients.pop().expect("one client");
        let (mut peer, name) = Peer::arrive(only);

        peer.send(&request_name("org.example.Thing", 0, 2));
        let frame = peer.frame();
        let (refusal, _) = message::decode(&frame, 0).expect("decode");
        assert_eq!(
            refusal.fields.error_name,
            Some("org.freedesktop.DBus.Error.AccessDenied")
        );
        assert!(!bus.holds(&name, "org.example.Thing"));
    }

    /// `ReleaseName` tells a caller nothing the filter would withhold.
    ///
    /// Its `NON_EXISTENT` and `NOT_OWNER` answers are a two-valued oracle for
    /// "does anybody hold this name", and a draft let any peer ask it about
    /// any name — straight past the filter, and past this file's rule that a
    /// name the caller may not see is reported ABSENT rather than as an error
    /// confirming it exists. A confined caller now gets the same answer for a
    /// name that is held as for one that is not.
    ///
    /// The mixed bus, because the two answers have to differ for a caller
    /// that MAY see the name or the test proves only that the method is
    /// broken.
    #[test]
    fn releasing_a_name_it_may_not_see_tells_a_confined_caller_nothing() {
        if !pidfd_available() {
            return;
        }
        let (bus, free, jailed) = mixed_bus();
        let (mut owner, name_owner) = Peer::arrive(free);
        let (mut confined, _) = Peer::arrive(jailed);

        owner.send(&request_name("org.example.Held", 0, 2));
        assert_eq!(name_code(&owner.answer()).0, 1);
        let _ = owner.frame();
        assert!(bus.holds(&name_owner, "org.example.Held"));

        // Held and unheld are the same answer to a caller that may see
        // neither.
        confined.send(&release_name("org.example.Held", 2));
        let held = name_code(&confined.answer());
        confined.send(&release_name("org.example.NotHeld", 3));
        let unheld = name_code(&confined.answer());
        assert_eq!(
            (held.0, unheld.0),
            (2, 2),
            "a confined caller learned which names are taken"
        );

        // And the name is still held: the refusal did not release it.
        assert!(bus.holds(&name_owner, "org.example.Held"));

        // A caller that MAY see the name is still told the truth, so the
        // answers above are a withholding rather than a method that never
        // works. `NOT_OWNER` for a visible name is the third answer and is
        // covered by `releasing_says_which_of_the_three_things_happened`;
        // what this adds is that the holder can still give its own name up.
        owner.send(&release_name("org.example.Held", 3));
        assert_eq!(
            name_code(&owner.answer()).0,
            1,
            "the holder could not release its own name"
        );
        assert!(!bus.holds(&name_owner, "org.example.Held"));
    }

    /// A unique name is the broker's to hand out, never a peer's to ask for.
    #[test]
    fn a_unique_name_may_not_be_requested() {
        let (_bus, mut clients) = bus_of(1);
        let only = clients.pop().expect("one client");
        let (mut peer, name) = Peer::arrive(only);

        for asked in [name.as_str(), ":1.999", "not a name"] {
            peer.send(&request_name(asked, 0, 2));
            let frame = peer.frame();
            let (refusal, _) = message::decode(&frame, 0).expect("decode");
            assert_eq!(
                refusal.fields.error_name,
                Some("org.freedesktop.DBus.Error.InvalidArgs"),
                "{asked} was accepted"
            );
        }
    }

    /// The lookups answer about a well-known name, and about its HOLDER.
    #[test]
    fn the_lookups_answer_about_a_name_and_name_its_holder() {
        let (_bus, mut clients) = bus_of(2);
        let second = clients.pop().expect("two clients");
        let first = clients.pop().expect("two clients");
        let (mut one, _) = Peer::arrive(first);
        let (mut two, name_two) = Peer::arrive(second);

        one.send(&name_query("NameHasOwner", "org.example.Thing", 2));
        let frame = one.frame();
        let (before, _) = message::decode(&frame, 0).expect("decode");
        assert!(
            matches!(before.args().first(), Some(crate::wire::Value::Bool(false))),
            "an unheld name has an owner"
        );

        two.send(&request_name("org.example.Thing", 0, 2));
        assert_eq!(name_code(&two.answer()).0, 1);
        let _ = two.frame();

        one.send(&name_query("NameHasOwner", "org.example.Thing", 3));
        let frame = one.frame();
        let (after, _) = message::decode(&frame, 0).expect("decode");
        assert!(matches!(
            after.args().first(),
            Some(crate::wire::Value::Bool(true))
        ));

        // The OWNER's unique name, not the name that was asked about.
        one.send(&name_query("GetNameOwner", "org.example.Thing", 4));
        let frame = one.frame();
        let (owner, _) = message::decode(&frame, 0).expect("decode");
        assert_eq!(
            owner.args().first().and_then(crate::wire::Value::as_str),
            Some(name_two.as_str())
        );

        one.send(&bus_call("ListNames", 5));
        let frame = one.frame();
        let (reply, _) = message::decode(&frame, 0).expect("decode");
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
        assert!(
            listed.contains(&"org.example.Thing".to_string()),
            "a held name is missing from ListNames: {listed:?}"
        );
        assert!(listed.contains(&name_two), "{listed:?}");
    }

    /// The queue is bounded, and a caller refused for want of room is told
    /// the same thing a caller that declined to wait is told.
    #[test]
    fn the_owner_queue_is_bounded() {
        let peers = crate::registry::MAX_NAME_QUEUE + 1;
        let (_bus, clients) = bus_of(peers);
        let mut arrived: Vec<Peer> = clients
            .into_iter()
            .map(|client| Peer::arrive(client).0)
            .collect();

        for (which, peer) in arrived.iter_mut().enumerate() {
            peer.send(&request_name("org.example.Thing", 0, 2));
            let expected = if which == 0 {
                1
            } else if which < crate::registry::MAX_NAME_QUEUE {
                2
            } else {
                3
            };
            assert_eq!(
                name_code(&peer.answer()).0,
                expected,
                "peer {which} got the wrong answer"
            );
        }
    }

    /// One connection may not hold more names than its bound.
    #[test]
    fn a_connection_may_not_hold_more_names_than_its_bound() {
        let (_bus, mut clients) = bus_of(1);
        let only = clients.pop().expect("one client");
        let (mut peer, _) = Peer::arrive(only);

        let mut serial = 2u32;
        for which in 0..crate::registry::MAX_NAMES_PER_CONNECTION {
            peer.send(&request_name(&format!("org.example.N{which}"), 0, serial));
            assert_eq!(name_code(&peer.answer()).0, 1, "name {which} was refused");
            let _ = peer.frame();
            serial = serial.saturating_add(1);
        }
        peer.send(&request_name("org.example.OneTooMany", 0, serial));
        assert_eq!(
            name_code(&peer.answer()),
            (3, Some(serial)),
            "the bound did not hold"
        );
    }

    /// §D's bound, charged per CALLER and refused rather than trimmed.
    ///
    /// Trimming an old entry to make room would un-track a call that is still
    /// outstanding, sending its caller back to waiting for ever — the failure
    /// the table exists to remove, reintroduced by the mechanism meant to
    /// bound it.
    ///
    /// Three peers, because two cannot tell a per-caller bound from a global
    /// one: an implementation counting the whole table would pass a test that
    /// only ever fills it from one connection, and one peer could then stop
    /// every other peer on the bus from calling anything.
    #[test]
    fn a_caller_may_not_outrun_the_pending_reply_bound() {
        let (bus, mut clients) = bus_of(3);
        let third = clients.pop().expect("three clients");
        let second = clients.pop().expect("three clients");
        let first = clients.pop().expect("three clients");
        let (mut one, _) = Peer::arrive(first);
        let (mut two, name_two) = Peer::arrive(second);
        let (mut three, _) = Peer::arrive(third);

        // Two never answers, so every call stays outstanding.
        let mut serial = 2u32;
        for _ in 0..crate::registry::MAX_PENDING_REPLIES {
            one.send(&peer_call(&name_two, serial, "unanswered"));
            let _ = two.frame();
            serial = serial.saturating_add(1);
        }
        assert_eq!(bus.pending_replies(), crate::registry::MAX_PENDING_REPLIES);

        one.send(&peer_call(&name_two, serial, "one too many"));
        let frame = one.frame();
        let (refusal, _) = message::decode(&frame, 0).expect("decode the refusal");
        assert_eq!(refusal.kind, message::MessageType::Error);
        assert_eq!(
            refusal.fields.error_name,
            Some("org.freedesktop.DBus.Error.LimitsExceeded")
        );
        // The refused call was not recorded, and none of the others was
        // dropped to make room for it.
        assert_eq!(bus.pending_replies(), crate::registry::MAX_PENDING_REPLIES);
        two.expect_silence();

        // And ANOTHER caller is unaffected: the bound is one peer's share of
        // the table, not the table.
        three.send(&peer_call(&name_two, 2, "from a caller at nobody's bound"));
        let frame = two.frame();
        let (call, _) = message::decode(&frame, 0).expect("decode the call");
        assert_eq!(call.kind, message::MessageType::MethodCall);
        assert_eq!(
            bus.pending_replies(),
            crate::registry::MAX_PENDING_REPLIES.saturating_add(1),
            "a second caller's call was charged to the first caller's share"
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

        // The four pushes below need the WHOLE budget, and this peer's
        // arrival replies are still charged for it after the peer has read
        // them: an in-flight frame's bytes come back only from the writer's
        // `finished`, once the frame is written AND dropped, because the
        // ceiling bounds live memory rather than queue depth. So the empty bus
        // this test always assumed is waited for rather than presumed.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while bus.queued_bytes() != 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "this peer's arrival is still charged {} bytes",
                bus.queued_bytes()
            );
            std::thread::yield_now();
        }

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
    /// earns the name, because it changes state, but a draft answered it
    /// regardless, which made it the single method that ignored the flag.
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
            deferred: Vec::new(),
        };
        // No Hello REPLY — but the name is still announced, because a name
        // gained is announced whether or not the call that gained it was
        // answered, and the directory says the same thing.
        assert_eq!(takes_its_name(&mut peer), ":1.1");
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

    /// A kernel that will not give a pidfd leaves the peer UNIDENTIFIED, not
    /// unconfined.
    ///
    /// `ENOPROTOOPT` is what a kernel below 6.5 answers, and the tempting
    /// reading of it is "no jails here, carry on" — which hands full portal
    /// access to every confined process on the machine. There is no way to
    /// provoke this branch on a kernel that has the option, so the mapping is
    /// asserted directly.
    #[test]
    fn a_kernel_that_gives_no_pidfd_leaves_the_peer_unidentified() {
        // ENOPROTOOPT, which is what the option's absence looks like.
        let refusal = io::Error::from_raw_os_error(92);
        match Connection::unidentifiable(&refusal) {
            Identity::Unknown(why) => assert!(why.contains("pidfd"), "{why}"),
            other => panic!("a missing pidfd resolved {other:?}"),
        }
    }

    /// The socket a reaped peer left behind, staged against the live kernel.
    ///
    /// This is the attack the commit exists for, so it is worth STAGING
    /// rather than describing. A peer connects, exits, and is reaped while
    /// its connection sits in the listen backlog — a delay a real peer
    /// controls by filling that backlog. `SO_PEERCRED` still reports its
    /// number, which the allocator has already taken back; the pidfd reports
    /// that there is no longer a process there.
    ///
    /// Everywhere else this transition is a fixture the author wrote, which
    /// proves the parser agrees with the author rather than with the kernel.
    /// Three reviews said so independently.
    ///
    /// The connector has to be a separate PROCESS, and this crate has no
    /// `fork`: surface #10's roster is three syscalls and `fork` is not one
    /// of them. So the test binary re-runs ITSELF with a filter naming the
    /// ignored helper below. Waiting for that child is what reaps it.
    #[test]
    fn a_peer_reaped_before_accept_is_not_unconfined() {
        let Some(stream) = a_reaped_peers_connection("reaped") else {
            return;
        };

        let credential = sys::peer_credential(&stream).expect("peercred");
        assert!(
            credential.pid > 0,
            "SO_PEERCRED reported no pid for a peer it sampled at connect"
        );

        let Ok(pidfd) = sys::peer_pidfd(&stream) else {
            // A kernel that refuses outright for a reaped peer is the other
            // denial, and accept maps it to the same answer.
            let refused = Connection::unidentifiable(&io::Error::from_raw_os_error(3));
            assert!(matches!(refused, Identity::Unknown(_)));
            return;
        };
        assert_eq!(
            RealProcfs.named_by(pidfd.as_raw_fd()),
            crate::lineage::Named::Reaped,
            "a reaped peer's pidfd still named a live process"
        );
        // The registry is empty, so a walk from `credential.pid` would answer
        // `Unconfined` — full portal access for a connection whose owner is
        // gone and whose number may already be somebody else's.
        let instances = Instances::new();
        match instances.resolve(&RealProcfs, pidfd.as_raw_fd()) {
            Identity::Unknown(why) => assert!(why.contains("reaped"), "{why}"),
            other => panic!("a reaped peer resolved {other:?}"),
        }
    }

    /// A connection whose peer is gone, or `None` where one cannot be staged.
    ///
    /// The connector has to be a separate PROCESS, and this crate has no
    /// `fork`: surface #10's roster is three syscalls and `fork` is not one
    /// of them. So the test binary re-runs ITSELF with a filter naming the
    /// ignored helper below. Waiting for that child is what reaps it, and the
    /// connection it left sits in the backlog until the caller accepts it.
    fn a_reaped_peers_connection(tag: &str) -> Option<UnixStream> {
        if !pidfd_available() {
            return None;
        }
        let path = scratch(tag);
        let listener = UnixListener::bind(&path).ok()?;
        let exe = std::env::current_exe().ok()?;
        let ran = std::process::Command::new(exe)
            .args([
                "--exact",
                "--ignored",
                "--quiet",
                "--test-threads=1",
                REAPED_HELPER,
            ])
            .env(REAPED_SOCKET, &path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        let _ = fs::remove_file(&path);
        let status = ran.ok()?;
        assert!(status.success(), "the helper process did not connect");
        // `status()` waited, so the connector is reaped. Whatever is in the
        // backlog now belongs to a process that does not exist.
        let (stream, _) = listener.accept().expect("the helper's connection");
        Some(stream)
    }

    /// A connection whose owner is gone REGISTERS NOTHING.
    ///
    /// The identity walk stopped trusting `SO_PEERCRED`'s number one commit
    /// ago; the registry went on trusting it, and `Register` is where trusting
    /// it costs something — the number names whoever holds it now, and the
    /// instance's name and services would be recorded against them. The peer
    /// here is genuinely reaped, so the sampled number is stale by
    /// construction, and it is still positive and plausible, which is why the
    /// contrast is asserted rather than described.
    ///
    /// The recycled-number half of the same attack cannot be staged — a test
    /// cannot make the allocator wrap — so it is pinned in `lineage`'s tests
    /// against an injected `/proc`. Between them the two halves cover the pair
    /// this commit adds.
    #[test]
    fn a_reaped_peer_registers_nothing() {
        let Some(stream) = a_reaped_peers_connection("reaped-register") else {
            return;
        };
        let guid = Guid::new(GUID).expect("guid");
        let quota = Quota::new();
        let bus = Bus::new();
        let instances = Instances::new();
        let mut connection =
            Connection::accept(stream, guid, &quota, &bus, &instances).expect("accept");

        // The number the registry used to be handed is right there, and it
        // looks like every other pid.
        assert!(
            connection.credential.pid > 0,
            "the sampled number this refusal has to beat was not there"
        );
        assert_eq!(
            connection.caller(&RealProcfs),
            None,
            "a peer that no longer exists was identified as the caller"
        );

        // This connection never said `Hello` and never will: its peer was gone
        // before the socket was accepted. A refusal has to be ADDRESSED, so
        // the name that Hello would have assigned is set here directly —
        // otherwise this arm reports "a reply was built before Hello" and the
        // refusal under test never gets built.
        connection.unique = Some(":1.1".to_string());
        let call = register_call("one", "org.td.One", 2);
        let (message, _) = message::decode(&call, 0).expect("decode a Register");
        connection.jail_register(&message, true).expect("the arm ran");

        let frame = connection.outbox.take().expect("a refusal was queued");
        assert_eq!(
            error_of(&frame).as_deref(),
            Some("td.Jail1.Error.Refused"),
            "a registration was answered for a process that does not exist"
        );
        let (reply, _) = message::decode(&frame, 0).expect("decode the refusal");
        let why = reply
            .args()
            .first()
            .and_then(crate::wire::Value::as_str)
            .unwrap_or("")
            .to_string();
        assert!(why.contains("could not be identified"), "{why}");
        assert_eq!(
            instances.pending_count(),
            0,
            "a registration was opened for a process that does not exist"
        );
    }

    /// A `/proc` the test drives, because the states this seam has to refuse
    /// are ones no live kernel produces on demand.
    ///
    /// `after_stat` is what the SECOND pidfd read answers, and it is keyed on
    /// a `stat` having happened rather than on a count of pidfd reads. That
    /// distinction is the whole point of the knob: a count says "the second
    /// one" wherever the second one sits, so it cannot tell a read taken after
    /// the `/proc` read from one taken immediately after the first — and the
    /// mutation that deletes the bracket's second read entirely has to fail
    /// this fake, not satisfy it. The same lesson, and the same shape, as
    /// `lineage`'s own table.
    struct Staged {
        first: Named,
        after_stat: Option<Named>,
        stat: Reading,
        read: std::cell::Cell<bool>,
    }

    impl Staged {
        fn new(first: Named, stat: Reading) -> Self {
            Staged {
                first,
                after_stat: None,
                stat,
                read: std::cell::Cell::new(false),
            }
        }

        fn then(mut self, after_stat: Named) -> Self {
            self.after_stat = Some(after_stat);
            self
        }
    }

    fn a_stat(starttime: u64) -> Reading {
        Reading::Of(crate::lineage::Stat { ppid: 1, starttime })
    }

    impl Procfs for Staged {
        fn stat(&self, _pid: i32) -> Reading {
            self.read.set(true);
            self.stat
        }

        fn named_by(&self, _pidfd: std::os::fd::RawFd) -> Named {
            match (self.read.get(), self.after_stat) {
                (true, Some(after)) => after,
                _ => self.first,
            }
        }
    }

    /// Every state in which this seam must prove nothing, against a real
    /// socket whose peer is alive and whose sampled number is CORRECT.
    ///
    /// That last part is what makes the table sharp rather than decorative: a
    /// fallback to `self.credential.pid` would be right about the number in
    /// every row here, and still wrong, because a broker that cannot read its
    /// oracle has no business preferring the value the oracle exists to
    /// qualify.
    #[test]
    fn a_caller_the_broker_cannot_prove_is_not_a_caller() {
        if !pidfd_available() {
            return;
        }
        let (client, server) = UnixStream::pair().expect("socketpair");
        let guid = Guid::new(GUID).expect("guid");
        let quota = Quota::new();
        let bus = Bus::new();
        let instances = Instances::new();
        let connection =
            Connection::accept(server, guid, &quota, &bus, &instances).expect("accept");
        let me = i32::try_from(std::process::id()).expect("a pid fits");
        assert_eq!(
            connection.credential.pid, me,
            "the number a fallback would have used was not even wrong"
        );

        let cases: [(&str, Staged); 6] = [
            // The oracle cannot be read: EMFILE, or an `fdinfo` this broker
            // does not recognise.
            ("an unreadable pidfd", Staged::new(Named::Unreadable, a_stat(7))),
            // ...and it is refused even when the /proc read would succeed,
            // which is the case a fake answering `Unreadable` to BOTH
            // questions cannot distinguish. A mutation lived here.
            (
                "an unreadable pidfd over a readable entry",
                Staged::new(Named::Unreadable, a_stat(7)).then(Named::Pid(me)),
            ),
            // The peer is gone before the walk starts.
            ("a reaped pidfd", Staged::new(Named::Reaped, a_stat(7))),
            // The entry behind a named pid is not there.
            ("no /proc entry", Staged::new(Named::Pid(me), Reading::Gone)),
            (
                "an unreadable /proc entry",
                Staged::new(Named::Pid(me), Reading::Unreadable),
            ),
            // And the bracket's own case: the peer was named, its entry was
            // read, and by the second read it had been reaped -- so the entry
            // that was read may have been somebody else's.
            (
                "reaped during the lookup",
                Staged::new(Named::Pid(me), a_stat(7)).then(Named::Reaped),
            ),
        ];
        for (what, procfs) in cases {
            assert_eq!(connection.caller(&procfs), None, "{what} proved a caller");
        }

        // A pid that CHANGED between the two reads is the same refusal: the
        // number was free in between, so the entry read while it was free
        // describes nobody in particular.
        let moved = Staged::new(Named::Pid(me), a_stat(7)).then(Named::Pid(me + 1));
        assert_eq!(connection.caller(&moved), None, "the bracket accepted a new pid");

        // The control: both reads agree and the entry is readable, so the
        // pair is proved and carries the start time that was read.
        let good = Staged::new(Named::Pid(me), a_stat(7)).then(Named::Pid(me));
        assert_eq!(
            connection.caller(&good),
            Some(Caller {
                pid: me,
                starttime: 7
            })
        );
        drop(client);
    }

    /// The seam against the LIVE kernel, which is the half no fake can prove.
    ///
    /// Every other test of `caller` drives an injected `/proc`, so together
    /// they establish that the code agrees with the fakes. This one
    /// establishes that the fakes are describing the kernel: a socketpair's
    /// peer is this process, so the pair that comes back is one the test can
    /// check rather than merely print.
    #[test]
    fn the_real_proc_path_proves_this_connections_own_peer() {
        if !pidfd_available() {
            return;
        }
        let (client, server) = UnixStream::pair().expect("socketpair");
        let guid = Guid::new(GUID).expect("guid");
        let quota = Quota::new();
        let bus = Bus::new();
        let instances = Instances::new();
        let connection =
            Connection::accept(server, guid, &quota, &bus, &instances).expect("accept");
        assert_eq!(
            connection.credential.pid,
            i32::try_from(std::process::id()).expect("a pid fits"),
            "the number the fallback would have used was not even the right one"
        );
        // Through the REAL /proc, on a socketpair whose peer is this process:
        // the pair proves out and names us. Every other test of this seam
        // drives an injected `/proc`, so without this one the whole seam could
        // agree with the fakes and disagree with the kernel.
        assert_eq!(
            connection.caller(&RealProcfs).map(|caller| caller.pid),
            Some(i32::try_from(std::process::id()).expect("a pid fits"))
        );
        drop(client);
    }

    /// A connection whose owner is gone COMPLETES nothing either.
    ///
    /// The twin of the test above, and it exists because two reviewers found
    /// the same hole independently: `Complete`'s guard had only a source-level
    /// pin, and a mutation that commented the guard out — or aliased the
    /// sampled credential past it — left all 236 tests passing. `Register`
    /// had the staged test; phase two, which is the call that actually binds
    /// a pid to an instance, had none.
    ///
    /// The token is a fabrication, and that is what makes the assertion
    /// sharp: if the guard is gone the registry answers "no registration is
    /// open under that token", and if it is there the call never reaches the
    /// registry at all.
    #[test]
    fn a_reaped_peer_completes_nothing() {
        let Some(stream) = a_reaped_peers_connection("reaped-complete") else {
            return;
        };
        let guid = Guid::new(GUID).expect("guid");
        let quota = Quota::new();
        let bus = Bus::new();
        let instances = Instances::new();
        let mut connection =
            Connection::accept(stream, guid, &quota, &bus, &instances).expect("accept");
        assert!(connection.credential.pid > 0);
        assert_eq!(connection.caller(&RealProcfs), None);

        connection.unique = Some(":1.1".to_string());
        let token = "0123456789abcdef0123456789abcdef".to_string();
        let call = jail_call("Complete", 2, "su", move |writer| {
            writer.string(&token)?;
            writer.uint32(1);
            Ok(())
        });
        let (message, _) = message::decode(&call, 0).expect("decode a Complete");
        connection.jail_complete(&message, true).expect("the arm ran");

        let frame = connection.outbox.take().expect("a refusal was queued");
        assert_eq!(
            error_of(&frame).as_deref(),
            Some("td.Jail1.Error.Refused"),
            "a completion was answered for a process that does not exist"
        );
        let (reply, _) = message::decode(&frame, 0).expect("decode the refusal");
        let why = reply
            .args()
            .first()
            .and_then(crate::wire::Value::as_str)
            .unwrap_or("")
            .to_string();
        assert!(
            why.contains("could not be identified"),
            "the guard was skipped and the registry answered instead: {why}"
        );
    }

    /// Connect to the socket named in the environment, then exit. A helper
    /// process for the tests above and nothing else; without the variable it
    /// does nothing, so a plain `--ignored` run of the suite is harmless.
    #[test]
    #[ignore = "a helper process for the reaped-peer tests"]
    fn connects_to_the_socket_named_in_the_environment() {
        let Ok(path) = std::env::var(REAPED_SOCKET) else {
            return;
        };
        UnixStream::connect(path).expect("the helper could not connect");
    }

    /// `SO_PEERPIDFD` against the live kernel, which is the half no fixture
    /// can prove: that the option number is the right one, that the kernel
    /// installs a real descriptor for it, and that the descriptor's `fdinfo`
    /// names the process on the other end of the socket.
    ///
    /// A socketpair's peer is this process, so the pid it names is one this
    /// test can check rather than merely print.
    #[test]
    fn the_peers_pidfd_names_the_process_on_the_other_end() {
        if !pidfd_available() {
            return;
        }
        let (client, server) = UnixStream::pair().expect("socketpair");
        let pidfd = sys::peer_pidfd(&server).expect("the kernel gave a pidfd");
        let me = i32::try_from(std::process::id()).expect("a pid fits");
        assert_eq!(
            RealProcfs.named_by(pidfd.as_raw_fd()),
            crate::lineage::Named::Pid(me),
            "the peer's pidfd named some other process"
        );
        // And it agrees with the credential the same socket carries. That is
        // the only cross-check available while both ends are alive: the two
        // can differ ONLY once the peer has been reaped, which is precisely
        // the case the pidfd exists to catch and this test cannot stage.
        assert_eq!(sys::peer_credential(&server).expect("peercred").pid, me);
        drop(client);
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
            let instances = Instances::new();
            let mut connection =
                Connection::accept(stream, guid, &quota, &bus, &instances).expect("connect");
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
            let instances = Instances::new();
            let accepted = Connection::accept(stream, guid, &quota, &bus, &instances);
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
            let instances = Instances::new();
            let accepted = Connection::accept(server, guid, &quota, &bus, &instances);
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

        // Five frames, not four: the `NameAcquired` for the unique name is
        // one of the broker's messages too, and it has to carry a serial of
        // its own like any other. Counting it is the point rather than an
        // inconvenience — it comes from the same counter.
        let mut got = Vec::new();
        let mut chunk = [0u8; 1024];
        let mut seen = Vec::new();
        while seen.len() < 5 {
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
            vec![Some(1), None, Some(2), Some(3), Some(4)],
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

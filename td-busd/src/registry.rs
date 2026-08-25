//! Who is on the bus, and how a message reaches them.
//!
//! Routing is what makes a broker a broker, and with one thread per connection
//! it is also where the design has to be careful: the thread that RECEIVES a
//! message is not the thread that owns the socket it is going to. Writing
//! straight to the recipient from the sender's thread would be simpler and is
//! wrong — a peer that never reads would block whoever sent to it, so any
//! application could stall any other by declining to drain its own socket.
//!
//! So every connection has an OUTBOX and a thread that drains it. A sender
//! appends and moves on; a recipient that will not read fills its own queue and
//! is disconnected for it. That is the shape §D asks for when it says the queue
//! ceiling is in bytes: a queue is only a queue if somebody else owns the far
//! end of it.

use std::collections::VecDeque;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::transport::MAX_MESSAGE;

/// The most bytes one connection may have waiting to be written.
///
/// **Reaching it refuses the SENDER; it does not disconnect the recipient.**
/// That is a change from what a draft of §D specified, and review found why it
/// had to change: this ceiling is one whole maximum message, so two maximum
/// messages back to back exceed it BY CONSTRUCTION — the first is still in the
/// writer's hands when the second arrives, however promptly the recipient
/// reads. Under the old rule any peer could evict any other with two frames,
/// and `ListNames` hands every name to every caller, so one application could
/// walk the bus and evict every other application at no cost to itself. A
/// ceiling an attacker aims at someone else is not a bound — the same argument
/// the bus ceiling below is written from, applied to the case that is aimed at
/// someone else by definition.
///
/// The recipient's memory is still bounded, by this number. A peer that never
/// reads accumulates to it and then everyone writing to it is told
/// `LimitsExceeded`; it is removed when it becomes the BUS's largest consumer,
/// which is the right test for "this peer is the problem" because it compares
/// peers rather than trusting whoever happened to write last.
///
/// The ceiling is a whole message, and `push` accepts unconditionally into a
/// queue that is EMPTY AND HAS NOTHING IN FLIGHT, so a legal 16 MiB message is
/// not permanently undeliverable to a peer whose queue happened to be
/// non-empty. Without the ceiling the queue is the denial of service §D
/// describes. The exception is per-connection only, and it is not an absolute
/// guarantee that a maximum message is always deliverable: the bus ceiling
/// below has no exception, so a maximum message is refused when the BUS is
/// near its bound. An earlier wording of this comment promised the guarantee
/// outright, which the code has never made true.
pub const MAX_OUTGOING_BYTES: usize = MAX_MESSAGE;

/// The same bound across the whole bus. §D is explicit that this memory is
/// charged to `td-busd` rather than to any application's cgroup, so §P's caps
/// do not reach it and only this does.
pub const MAX_OUTGOING_BYTES_TOTAL: usize = MAX_MESSAGE * 4;

/// A secondary guard against many tiny messages, which the byte ceilings above
/// do not bound: a million one-byte frames cost far more in per-frame
/// bookkeeping than in bytes. §D asks for the count to be exactly this — a
/// backstop beside the byte limit rather than the limit itself, which is the
/// mistake it records a draft having made.
pub const MAX_OUTGOING_MESSAGES: usize = 4096;

/// How many calls one connection may have outstanding at once, per §D's
/// bounds list.
///
/// Charged to the CALLER. The table is the broker's memory and a peer that
/// could fill it without bound would be spending the broker's, but the peer
/// to charge is the one that chose to call: a callee cannot decline to be
/// called, so charging the callee would let one peer exhaust another's share
/// by calling it.
pub const MAX_PENDING_REPLIES: usize = 128;

/// How long the bus's remedy waits for a relieved connection's writer to let
/// go of the frame it is holding. Short because `shutdown` is what releases
/// it: a `sendmsg` on a shut-down socket returns at once, so this is a bound
/// on a pathological writer rather than a wait anything normal reaches.
const SETTLE: Duration = Duration::from_millis(500);

/// A frame that did not fit, and why.
///
/// The frame comes BACK. §D's remedy for the bus ceiling is to disconnect the
/// largest consumer and carry on, which means the caller wants to try again —
/// and retrying with a clone would double a 16 MiB message at the exact moment
/// the broker is short of memory.
#[derive(Debug)]
pub struct Rejected {
    pub why: Overflow,
    pub frame: Vec<u8>,
}

/// Why an append failed.
#[derive(Debug, PartialEq, Eq)]
pub enum Overflow {
    /// This connection is gone; nothing more will be written to it.
    Closed,
    /// This connection's own ceiling. It is disconnected for it. Both
    /// numbers are carried because either can be what fired: a draft reported
    /// every refusal as a byte overflow, so a flood of 4096 tiny frames was
    /// diagnosed as "bytes behind" with a byte count far under the ceiling.
    Connection { bytes: usize, frames: usize },
    /// The bus's ceiling. §D calls this a broker-level condition: the fault is
    /// a policy elsewhere, so it is logged apart from an ordinary refusal.
    Bus(usize),
}

struct Queue {
    frames: VecDeque<Vec<u8>>,
    /// Bytes sitting in `frames`.
    bytes: usize,
    /// Bytes the writer has taken and not yet finished writing.
    ///
    /// This is the difference between bounding a QUEUE and bounding MEMORY,
    /// and a draft got it wrong in the way that matters: `take` uncharged the
    /// frame before handing it over, so a writer blocked in `sendmsg` against
    /// a peer that had stopped reading held a whole 16 MiB frame that no
    /// counter knew about — and the queue it came from was now empty, so the
    /// per-connection exception admitted another. That is the same multiplier
    /// the bus ceiling exists to prevent, rebuilt one frame lower down: eight
    /// blocked writers hold eight maximum messages whatever the counters say.
    /// The charge now lasts until the bytes are on the wire.
    in_flight: usize,
    /// Whether the writer is holding a frame, for the COUNT ceiling. A draft
    /// counted `frames.len()` alone, so taking one tiny frame off the deque
    /// let a peer hold one more than the backstop allows — the byte ceiling
    /// learned to count the writer's frame and the count ceiling did not.
    in_flight_frames: usize,
    /// Refuses new frames. Set by `seal` and by `close`.
    accepting: bool,
    /// Nothing more will be written at all; `take` gives up rather than
    /// waiting. `seal` does NOT set this — that is the whole point of it.
    closed: bool,
}

impl Queue {
    /// Everything this connection is holding, queued or in flight.
    fn held(&self) -> usize {
        self.bytes.saturating_add(self.in_flight)
    }

    /// How many frames it is holding, on the same principle.
    fn frames_held(&self) -> usize {
        self.frames.len().saturating_add(self.in_flight_frames)
    }
}

/// One connection's pending writes, and the socket they go to.
pub struct Outbox {
    /// A clone of the connection's socket. The reader thread keeps the
    /// original; this half is written by the writer thread and shut down by
    /// whichever notices the connection is over.
    stream: UnixStream,
    queue: Mutex<Queue>,
    /// Woken when there is something to write, or nothing ever will be.
    ready: Condvar,
    /// Woken when a frame has finished being written. `flush_within` waits on
    /// this so a connection that is ending can put its last words on the wire
    /// before the socket goes down.
    written: Condvar,
    total: Arc<AtomicUsize>,
    /// The serial the broker stamps on the next message it ORIGINATES to this
    /// connection.
    ///
    /// It lives here rather than on the connection because the sender it
    /// numbers for is `org.freedesktop.DBus`, which is one sender across the
    /// whole bus, while the stream those numbers have to stay distinct in is
    /// this one. A departing peer's sweep writes into a caller's outbox from
    /// the DEPARTING connection's thread, so a counter kept per connection
    /// would let two of them hand the same caller two messages from the same
    /// sender carrying the same serial.
    next_serial: AtomicU32,
}

impl Outbox {
    fn new(stream: UnixStream, total: Arc<AtomicUsize>) -> Self {
        Self {
            stream,
            queue: Mutex::new(Queue {
                frames: VecDeque::new(),
                bytes: 0,
                in_flight: 0,
                in_flight_frames: 0,
                accepting: true,
                closed: false,
            }),
            ready: Condvar::new(),
            written: Condvar::new(),
            total,
            next_serial: AtomicU32::new(1),
        }
    }

    /// The next serial for a message the broker originates to this
    /// connection. Wraps to 1 rather than 0: zero is not a legal serial.
    pub fn take_serial(&self) -> u32 {
        let mut mine = self.next_serial.fetch_add(1, Ordering::Relaxed);
        if mine == 0 {
            mine = self.next_serial.fetch_add(1, Ordering::Relaxed);
        }
        mine
    }

    /// Append a frame for the writer thread. Never blocks on the recipient.
    pub fn push(&self, frame: Vec<u8>) -> Result<(), Rejected> {
        let mut queue = match self.queue.lock() {
            Ok(queue) => queue,
            // A poisoned queue belongs to a connection whose thread died
            // holding it. Treating that as closed is right and is also the
            // only option that does not panic on a broker's routing path.
            Err(_) => return Err(Rejected { why: Overflow::Closed, frame }),
        };
        if !queue.accepting {
            return Err(Rejected { why: Overflow::Closed, frame });
        }
        let size = frame.len();
        // The empty-queue exception, so one legal message fits a connection
        // that is holding nothing. IN FLIGHT counts as holding: a frame the
        // writer is blocked on is this connection's memory just as much as one
        // still in the deque, and treating an emptied queue as empty while its
        // frame is still in the writer's hands is exactly the hole a draft
        // left here.
        if queue.held() > 0
            && (queue.held().saturating_add(size) > MAX_OUTGOING_BYTES
                || queue.frames_held() >= MAX_OUTGOING_MESSAGES)
        {
            return Err(Rejected {
                why: Overflow::Connection {
                    bytes: queue.held(),
                    frames: queue.frames_held(),
                },
                frame,
            });
        }
        // The bus's budget, taken before the frame joins the queue so the two
        // counts cannot disagree.
        //
        // NO empty-queue exception here, and that asymmetry is the point. The
        // exception exists so one legal message is always deliverable to one
        // peer; applied to the bus as well it stops being an exception and
        // becomes a multiplier, since every connection's queue is empty at
        // some moment and 64 of them each landing a maximum message is 64
        // times the ceiling. Measured on the first draft: eight connections
        // took 134 MiB against a 67 MiB bound. When the BUS is full the right
        // answer is that the bus is full — §D's remedy is to disconnect the
        // largest consumer, which the caller does with `relieve_largest`.
        let mut held = self.total.load(Ordering::Acquire);
        loop {
            let wanted = held.saturating_add(size);
            if wanted > MAX_OUTGOING_BYTES_TOTAL {
                return Err(Rejected {
                    why: Overflow::Bus(held),
                    frame,
                });
            }
            match self.total.compare_exchange_weak(
                held,
                wanted,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(seen) => held = seen,
            }
        }
        queue.bytes = queue.bytes.saturating_add(size);
        queue.frames.push_back(frame);
        drop(queue);
        self.ready.notify_one();
        Ok(())
    }

    /// How many bytes this connection is holding, queued or in flight. Used
    /// to pick the largest consumer when the bus's ceiling is reached — and it
    /// must count the in-flight frame, or the connection actually hoarding a
    /// whole message looks like the SMALLEST consumer and the remedy
    /// disconnects an innocent peer instead.
    pub fn pending(&self) -> usize {
        self.queue.lock().map(|queue| queue.held()).unwrap_or(0)
    }

    /// Stop accepting, wake the writer, and take the socket down so a reader
    /// blocked in `recvmsg` returns instead of waiting for a peer that will
    /// never be answered.
    pub fn close(&self) {
        if let Ok(mut queue) = self.queue.lock() {
            queue.accepting = false;
            queue.closed = true;
            // What is dropped here was charged to the bus and must be handed
            // back, or a broker that has served enough connections believes it
            // is full of messages nobody is waiting for.
            //
            // The IN-FLIGHT bytes are NOT reclaimed here. The writer still has
            // that frame; the shutdown below is what will release it, and the
            // writer's own `finished` is the only thing that knows when the
            // memory is actually gone. A caller that needs the budget back
            // before it carries on wants `close_and_settle`.
            let dropped = queue.bytes;
            queue.frames.clear();
            queue.bytes = 0;
            let _ = self
                .total
                .try_update(Ordering::AcqRel, Ordering::Acquire, |held| {
                    Some(held.saturating_sub(dropped))
                });
        }
        self.ready.notify_all();
        self.written.notify_all();
        let _ = self.stream.shutdown(Shutdown::Both);
    }

    /// The next frame to write, waiting until there is one.
    ///
    /// The bytes move from `bytes` to `in_flight` rather than being given
    /// back: this connection still holds them, and the bus's count must say
    /// so until they are on the wire. The caller MUST call `finished` when the
    /// write completes, or this connection's budget never comes back.
    ///
    /// Returns `None` when nothing more will ever be written: the outbox is
    /// closed, or it is sealed and the queue has run dry.
    pub fn take(&self) -> Option<Vec<u8>> {
        let mut queue = self.queue.lock().ok()?;
        loop {
            if queue.closed {
                return None;
            }
            if let Some(frame) = queue.frames.pop_front() {
                queue.bytes = queue.bytes.saturating_sub(frame.len());
                queue.in_flight = queue.in_flight.saturating_add(frame.len());
                queue.in_flight_frames = queue.in_flight_frames.saturating_add(1);
                return Some(frame);
            }
            if !queue.accepting {
                // Sealed and drained: the last words are out.
                return None;
            }
            queue = self.ready.wait(queue).ok()?;
        }
    }

    /// The frame `take` handed over is written AND DROPPED. Give its bytes
    /// back to the bus.
    ///
    /// The caller must have dropped the frame first. This is the whole
    /// difference between charging for a queue and charging for memory: a
    /// draft had `close` reclaim the in-flight bytes itself, which is earlier
    /// than the writer lets go — `close` only shuts the socket down, and the
    /// blocked `sendmsg` still has to return before the `Vec` is freed. The
    /// budget freed in that window is real budget against memory that is still
    /// live, so a caller retrying straight after a relief could admit a second
    /// maximum message beside the first. Nothing but the writer knows when the
    /// bytes are gone, so nothing but the writer gives them back.
    ///
    /// `min` guards the double-release anyway: a settle that gave up waiting
    /// may have written the frame off, and giving it back twice would drift
    /// the bus's count DOWN, which is the direction that silently removes the
    /// ceiling.
    pub fn finished(&self, size: usize) {
        if let Ok(mut queue) = self.queue.lock() {
            let give = size.min(queue.in_flight);
            queue.in_flight = queue.in_flight.saturating_sub(give);
            queue.in_flight_frames = queue.in_flight_frames.saturating_sub(1);
            let _ = self
                .total
                .try_update(Ordering::AcqRel, Ordering::Acquire, |held| {
                    Some(held.saturating_sub(give))
                });
        }
        self.written.notify_all();
    }

    /// Close, and wait for the writer to let go of the frame it is holding.
    ///
    /// `close` alone is not enough for a caller that is about to reuse the
    /// budget: it releases the socket, not the allocation. This waits for the
    /// writer's `finished`, which is bounded because `shutdown` is what
    /// unblocks the writer and a `sendmsg` on a shut-down socket returns at
    /// once. On timeout the bytes stay CHARGED rather than being written off:
    /// a writer that is not making progress really is still holding them, and
    /// over-counting tightens the ceiling where under-counting removes it.
    pub fn close_and_settle(&self, patience: Duration) {
        self.close();
        // `Instant + Duration` PANICS on overflow, and this crate does not
        // panic. A patience that cannot be added to now is treated as no
        // patience at all, which is the safe direction: the caller closes
        // either way.
        let Some(deadline) = Instant::now().checked_add(patience) else {
            return;
        };
        let Ok(mut queue) = self.queue.lock() else {
            return;
        };
        while queue.in_flight > 0 {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                return;
            };
            let Ok((held, timeout)) = self.written.wait_timeout(queue, left) else {
                return;
            };
            if timeout.timed_out() {
                return;
            }
            queue = held;
        }
    }

    /// Stop accepting new frames, but keep and write what is already queued.
    ///
    /// This is what makes a diagnosis survive the connection it explains. A
    /// draft called `close` on every exit path from `serve`, which cleared the
    /// queue and shut the socket down — so a peer that made a bad call and
    /// then a fatal one got a bare EOF instead of the error reply the broker
    /// had already written for it, depending only on whether the writer thread
    /// happened to be scheduled in between.
    pub fn seal(&self) {
        if let Ok(mut queue) = self.queue.lock() {
            queue.accepting = false;
        }
        self.ready.notify_all();
    }

    /// Wait, to a deadline, until everything queued has been written.
    ///
    /// Bounded because the peer may be alive and simply not reading, in which
    /// case the writer is blocked in `sendmsg` and would never finish. The
    /// caller closes afterwards either way; this only buys the frames that
    /// CAN go out the chance to.
    pub fn flush_within(&self, patience: Duration) {
        // `Instant + Duration` PANICS on overflow, and this crate does not
        // panic. A patience that cannot be added to now is treated as no
        // patience at all, which is the safe direction: the caller closes
        // either way.
        let Some(deadline) = Instant::now().checked_add(patience) else {
            return;
        };
        let Ok(mut queue) = self.queue.lock() else {
            return;
        };
        while queue.held() > 0 && !queue.closed {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                return;
            };
            let Ok((held, timeout)) = self.written.wait_timeout(queue, left) else {
                return;
            };
            if timeout.timed_out() {
                return;
            }
            queue = held;
        }
    }

    /// Bytes sitting in the deque, not counting the one the writer holds.
    #[cfg(test)]
    fn queued(&self) -> usize {
        self.queue.lock().map(|queue| queue.bytes).unwrap_or(0)
    }

    /// Bytes the writer has taken and not yet finished writing.
    #[cfg(test)]
    fn in_flight(&self) -> usize {
        self.queue.lock().map(|queue| queue.in_flight).unwrap_or(0)
    }

    /// `push`, reduced to why it failed. Tests care about the reason; the
    /// frame that comes back with it is for the retry the broker does.
    #[cfg(test)]
    fn offer(&self, frame: Vec<u8>) -> Result<(), Overflow> {
        self.push(frame).map_err(|rejected| rejected.why)
    }

    /// The socket this outbox writes to.
    ///
    /// The caller writes; this type does not. `sendmsg` lives behind the one
    /// raw-syscall surface UNSAFE.md §10 rosters, and the transport is its
    /// only caller, so the queue hands the socket over rather than reaching
    /// for the syscall itself. The split also puts descriptor attachment where
    /// descriptors already are when the rest of rung 14 lands.
    pub fn stream(&self) -> &UnixStream {
        &self.stream
    }
}

/// One connection as the bus knows it.
struct Registered {
    unique: String,
    outbox: Arc<Outbox>,
    /// What the kernel said about the peer behind this name, taken once at
    /// accept. Two plain numbers rather than the syscall layer's type: the
    /// directory answers questions about a name, and it should not have to
    /// name the raw-syscall module to do it.
    uid: u32,
    pid: i32,
    /// The application this peer's lineage proved it belongs to, or `None` for
    /// one that is not a confined application. Decided at accept and stored,
    /// never recomputed — see `app_id` below.
    app_id: Option<String>,
}

/// One call that has been routed and not yet answered.
///
/// §D asks for this table for two reasons and the second is the sharper one.
/// A caller whose callee disconnects mid-call otherwise waits for ever, which
/// is the "left waiting on a serial" failure §D argues against everywhere
/// else. And with nothing recording which call is outstanding to whom, ANY
/// peer may send a `METHOD_RETURN` carrying an arbitrary `REPLY_SERIAL` to
/// any other peer — libdbus and GDBus both match a pending call by serial
/// without checking who answered — so one peer can answer a call another made
/// to a third, and the client library hands it to the caller as the answer.
/// The broker stamps a truthful SENDER, which makes that detectable; this is
/// what detects it.
struct Pending {
    /// Who is waiting, and the serial it is waiting on. The pair is the
    /// identity of the call: serials are unique per connection, not globally.
    caller: String,
    serial: u32,
    /// Who was asked, and therefore the only connection whose answer counts.
    callee: String,
}

/// What resolving a destination and recording a call to it found.
pub enum Routing {
    /// Nothing on this bus answers to that name.
    Absent,
    /// The caller already holds `MAX_PENDING_REPLIES` unanswered calls.
    TooMany,
    /// The caller already has a call outstanding on that serial.
    Repeated,
    /// The callee's outbox, with the call recorded against the connection
    /// this lookup actually resolved.
    Ready(Arc<Outbox>),
}

/// A call left unanswered because the peer it was routed to has gone.
pub struct Abandoned {
    pub outbox: Arc<Outbox>,
    pub caller: String,
    pub serial: u32,
}

struct Directory {
    /// The N in `:1.N`. Monotonic and never reused: a unique name that came
    /// back would let a message written for one connection reach its
    /// successor, which is the one thing a unique name exists to prevent.
    next: u32,
    peers: Vec<Registered>,
    /// Under the SAME lock as `peers`, deliberately. A departing connection
    /// has to leave the directory and sweep its outstanding calls as one act:
    /// with two locks a caller could be handed a `NoReply` for a callee that
    /// a concurrent `route` had just found still present, or the reverse.
    pending: Vec<Pending>,
}

/// The bus's directory.
pub struct Bus {
    directory: Mutex<Directory>,
    total_outgoing: Arc<AtomicUsize>,
}

impl Default for Bus {
    fn default() -> Self {
        Self::new()
    }
}

impl Bus {
    pub fn new() -> Self {
        Self {
            directory: Mutex::new(Directory {
                next: 1,
                peers: Vec::new(),
                pending: Vec::new(),
            }),
            total_outgoing: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Make an outbox for a connection, before it has a name. A connection has
    /// a socket and a queue from the moment it is accepted; what `Hello` adds
    /// is the NAME, and until then nothing can address it.
    pub fn outbox_for(&self, stream: UnixStream) -> Arc<Outbox> {
        Arc::new(Outbox::new(stream, Arc::clone(&self.total_outgoing)))
    }

    /// `Hello`: assign this connection its unique name and publish it.
    /// Take the next unique name WITHOUT becoming routable under it.
    ///
    /// Split from `publish` because the order matters: a draft registered the
    /// outbox and then built the `Hello` reply, which leaves a window in which
    /// the connection is addressable and has not yet been told its own name.
    /// Names are sequential and `ListNames` discloses the highest, so a peer
    /// can aim at the next one and land a method call ahead of the reply — and
    /// the first frame a client library reads is then a stranger's call rather
    /// than the answer to the only message it has sent.
    pub fn reserve(&self) -> Result<String, String> {
        let mut directory = self
            .directory
            .lock()
            .map_err(|_| "the bus directory is poisoned".to_string())?;
        let unique = format!(":1.{}", directory.next);
        directory.next = directory
            .next
            .checked_add(1)
            .ok_or_else(|| "the bus has run out of unique names".to_string())?;
        Ok(unique)
    }

    /// Become routable under a name `reserve` handed out.
    pub fn publish(
        &self,
        unique: &str,
        outbox: &Arc<Outbox>,
        uid: u32,
        pid: i32,
        app_id: Option<String>,
    ) -> Result<(), String> {
        let mut directory = self
            .directory
            .lock()
            .map_err(|_| "the bus directory is poisoned".to_string())?;
        directory.peers.push(Registered {
            unique: unique.to_string(),
            outbox: Arc::clone(outbox),
            uid,
            pid,
            app_id,
        });
        Ok(())
    }

    /// `reserve` and `publish` together, for callers with nothing to say in
    /// between. Tests use this; `say_hello` deliberately does not.
    #[cfg(test)]
    pub(crate) fn join(
        &self,
        outbox: &Arc<Outbox>,
        uid: u32,
        pid: i32,
    ) -> Result<String, String> {
        let unique = self.reserve()?;
        self.publish(&unique, outbox, uid, pid, None)?;
        Ok(unique)
    }

    /// Resolve `name` and record `caller`'s call to it as ONE act.
    ///
    /// One act because two leave a window. Between a lookup that found the
    /// callee and a record written afterwards, the callee can depart: `leave`
    /// sweeps a table that does not mention the call yet, the record lands
    /// after the sweep, and the caller is left waiting for ever on a
    /// connection that has gone — while the entry holds the caller's share of
    /// the bound until the caller itself departs. Both are the failure this
    /// table exists to remove, so the lookup and the record share the lock
    /// that `leave` also takes.
    ///
    /// The callee is recorded as the unique name this lookup RESOLVED, never
    /// as the name the caller wrote. Today the two are the same string
    /// because only a unique name routes. When a well-known name can be
    /// owned they are not, and recording the written name would test a
    /// reply from the owner against a name the owner does not answer to:
    /// every genuine reply dropped, and every one of those calls leaked past
    /// `leave`, which looks for the departing peer's unique name.
    ///
    /// Refused rather than trimmed at the bound: dropping an OLD entry to
    /// make room would silently un-track a call that is still outstanding, so
    /// that caller would go back to waiting for ever — the failure this table
    /// exists to remove, reintroduced by the mechanism meant to bound it.
    ///
    /// A serial this caller already has outstanding is refused too, which the
    /// specification asks for anyway — a serial is unique among a
    /// connection's undelivered messages — and which this table needs. With a
    /// duplicate allowed, `(caller, serial)` stops naming ONE call: two
    /// answers carry the same `reply_serial` to a client that cannot tell
    /// them apart, and `forget_reply`, which un-records a call that could not
    /// be sent, could remove the wrong one and leave a live call untracked.
    pub fn route_expecting(&self, name: &str, caller: &str, serial: u32) -> Routing {
        let Ok(mut directory) = self.directory.lock() else {
            return Routing::Absent;
        };
        let Some((outbox, callee)) = directory
            .peers
            .iter()
            .find(|peer| peer.unique == name)
            .map(|peer| (Arc::clone(&peer.outbox), peer.unique.clone()))
        else {
            return Routing::Absent;
        };
        let mut held = 0_usize;
        for call in directory.pending.iter() {
            if call.caller != caller {
                continue;
            }
            if call.serial == serial {
                return Routing::Repeated;
            }
            held = held.saturating_add(1);
        }
        if held >= MAX_PENDING_REPLIES {
            return Routing::TooMany;
        }
        directory.pending.push(Pending {
            caller: caller.to_string(),
            serial,
            callee,
        });
        Routing::Ready(outbox)
    }

    /// Un-record a call that was recorded and then never delivered.
    ///
    /// Recording precedes the queue push so that an answer cannot reach the
    /// table before the question does, which means every path that fails
    /// AFTER it has to undo it. Left in place the entry is a call nobody will
    /// ever answer: it holds the caller's share of the bound for the life of
    /// the connection, and since a relay can fail because a THIRD peer filled
    /// the callee's queue, one peer could otherwise spend another's whole
    /// allowance and leave it unable to call anyone.
    pub fn forget_reply(&self, caller: &str, serial: u32) {
        if let Ok(mut directory) = self.directory.lock() {
            if let Some(at) = directory
                .pending
                .iter()
                .position(|call| call.caller == caller && call.serial == serial)
            {
                directory.pending.swap_remove(at);
            }
        }
    }

    /// Whether `replier` is the peer `to` is waiting on for `reply_serial`,
    /// consuming the record if so.
    ///
    /// Consumed, so a serial answers ONCE: a second reply carrying the same
    /// serial is as forged as a first one from the wrong peer, and a caller
    /// that has already been answered has nothing outstanding to confuse.
    pub fn claim_reply(&self, replier: &str, to: &str, reply_serial: u32) -> bool {
        let Ok(mut directory) = self.directory.lock() else {
            return false;
        };
        let found = directory.pending.iter().position(|call| {
            call.caller == to && call.serial == reply_serial && call.callee == replier
        });
        match found {
            Some(at) => {
                directory.pending.swap_remove(at);
                true
            }
            None => false,
        }
    }

    /// Remove a connection, and report the calls that will now never be
    /// answered.
    ///
    /// Silent if it never said `Hello`. The departing connection's own
    /// outstanding calls are dropped without ceremony — there is nobody left
    /// to deliver an error to — while the calls made TO it are what comes
    /// back, each with the outbox its error has to reach.
    #[must_use = "the abandoned calls have to be answered, or their callers wait for ever"]
    pub fn leave(&self, unique: &str) -> Vec<Abandoned> {
        let mut abandoned = Vec::new();
        if let Ok(mut directory) = self.directory.lock() {
            if let Some(at) = directory
                .peers
                .iter()
                .position(|peer| peer.unique == unique)
            {
                directory.peers.swap_remove(at);
            }
            // Both directions in one pass under the one lock. The list is
            // taken out first so the peer lookup below can borrow the
            // directory while this walks what used to be in it.
            let outstanding = std::mem::take(&mut directory.pending);
            let mut kept = Vec::with_capacity(outstanding.len());
            for call in outstanding {
                if call.caller == unique {
                    continue;
                }
                if call.callee != unique {
                    kept.push(call);
                    continue;
                }
                let outbox = directory
                    .peers
                    .iter()
                    .find(|peer| peer.unique == call.caller)
                    .map(|peer| Arc::clone(&peer.outbox));
                if let Some(outbox) = outbox {
                    abandoned.push(Abandoned {
                        outbox,
                        caller: call.caller,
                        serial: call.serial,
                    });
                }
            }
            directory.pending = kept;
        }
        abandoned
    }

    #[cfg(test)]
    pub(crate) fn pending_replies(&self) -> usize {
        self.directory
            .lock()
            .map(|directory| directory.pending.len())
            .unwrap_or(0)
    }

    /// The outbox a message addressed to `name` should go to.
    pub fn route(&self, name: &str) -> Option<Arc<Outbox>> {
        let directory = self.directory.lock().ok()?;
        directory
            .peers
            .iter()
            .find(|peer| peer.unique == name)
            .map(|peer| Arc::clone(&peer.outbox))
    }

    /// The uid and pid behind a name, as the KERNEL reported them when the
    /// connection was accepted — not as the peer describes itself. That is the
    /// whole value of the answer: a confined application asking who is on the
    /// other end of a call is asking the kernel, through the broker.
    pub fn credentials(&self, name: &str) -> Option<(u32, i32)> {
        let directory = self.directory.lock().ok()?;
        directory
            .peers
            .iter()
            .find(|peer| peer.unique == name)
            .map(|peer| (peer.uid, peer.pid))
    }

    /// The application id behind a name, for peers that have one.
    ///
    /// Resolved once at accept and stored, not recomputed per query. That is
    /// deliberate rather than a cache: the lineage §D proves is the one that
    /// existed when the connection was made, and re-walking it later would
    /// answer about a process tree that has moved on — an application whose
    /// intermediate ancestors have since exited would stop being itself.
    pub fn app_id(&self, name: &str) -> Option<String> {
        let directory = self.directory.lock().ok()?;
        directory
            .peers
            .iter()
            .find(|peer| peer.unique == name)
            .and_then(|peer| peer.app_id.clone())
    }

    /// Every unique name currently on the bus, in the order they joined.
    pub fn names(&self) -> Vec<String> {
        let mut names = match self.directory.lock() {
            Ok(directory) => directory
                .peers
                .iter()
                .map(|peer| peer.unique.clone())
                .collect::<Vec<String>>(),
            Err(_) => Vec::new(),
        };
        names.sort_by_key(|name| unique_index(name).unwrap_or(u32::MAX));
        names
    }

    /// Disconnect whoever is holding the most, which is what §D asks for when
    /// the BUS's ceiling is reached: refusing everyone would punish the peers
    /// a policy failure elsewhere has already inconvenienced.
    pub fn relieve_largest(&self) -> Option<String> {
        // The victim is chosen under the directory lock and dealt with
        // WITHOUT it. `close_and_settle` waits, briefly, for the victim's
        // writer; doing that with the directory held would stop every route,
        // join and leave on the bus for the duration — a global stall taken
        // in exactly the degraded state that most needs the bus to keep
        // moving.
        let (name, outbox) = {
            let directory = self.directory.lock().ok()?;
        // Each `pending()` takes its own outbox's lock, so these are samples
        // taken at slightly different instants rather than one consistent
        // snapshot — making them consistent would mean holding every queue
        // lock at once, which is a lock-ordering problem far worse than the
        // one it would solve. The residual is that a peer which is no longer
        // THE largest can still be chosen, which is unfairness rather than
        // unsoundness. The one outcome worth ruling out is disconnecting a
        // peer that is holding NOTHING, so the choice is re-read before it is
        // acted on: if the pressure resolved itself in the meantime, nobody is
        // relieved and the caller's retry finds the room anyway.
            let worst = directory
                .peers
                .iter()
                .max_by_key(|peer| peer.outbox.pending())?;
            if worst.outbox.pending() == 0 {
                return None;
            }
            (worst.unique.clone(), Arc::clone(&worst.outbox))
        };
        outbox.close_and_settle(SETTLE);
        Some(name)
    }

    #[cfg(test)]
    pub(crate) fn queued_bytes(&self) -> usize {
        self.total_outgoing.load(Ordering::Acquire)
    }
}

/// The N of a `:1.N`, for ordering names the way they were handed out. A name
/// this does not understand sorts last rather than panicking a listing.
fn unique_index(name: &str) -> Option<u32> {
    name.strip_prefix(":1.")?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (UnixStream, UnixStream) {
        UnixStream::pair().expect("socketpair")
    }

    /// Names are handed out in order and never reused. A recycled name would
    /// let a message addressed to one connection arrive at whoever took its
    /// place — the exact confusion a unique name exists to prevent.
    #[test]
    fn unique_names_count_up_and_are_never_reused() {
        let bus = Bus::new();
        let (_a, sa) = pair();
        let (_b, sb) = pair();
        let first = bus.outbox_for(sa);
        let second = bus.outbox_for(sb);
        assert_eq!(bus.join(&first, 1000, 4001).expect("join"), ":1.1");
        assert_eq!(bus.join(&second, 1000, 4002).expect("join"), ":1.2");
        assert_eq!(bus.names(), vec![":1.1".to_string(), ":1.2".to_string()]);

        assert!(
            bus.leave(":1.1").is_empty(),
            "a peer nobody called abandoned somebody"
        );
        assert_eq!(bus.names(), vec![":1.2".to_string()]);
        assert!(bus.route(":1.1").is_none(), "a departed name still routes");

        let (_c, sc) = pair();
        let third = bus.outbox_for(sc);
        assert_eq!(
            bus.join(&third, 1000, 4003).expect("join"),
            ":1.3",
            "a unique name was reused"
        );
    }

    /// A queue is bounded in BYTES, and one maximum-sized message always fits.
    /// Both halves matter: the ceiling alone makes a legal large message
    /// undeliverable, and the exception alone is not a bound.
    #[test]
    fn one_connection_queues_to_its_ceiling_and_no_further() {
        let bus = Bus::new();
        let (_client, server) = pair();
        let outbox = bus.outbox_for(server);
        // An empty queue takes anything, however large.
        outbox
            .push(vec![0u8; MAX_OUTGOING_BYTES + 1])
            .expect("an empty queue refused a whole message");
        // And now it is over its ceiling, so the next is refused.
        match outbox.offer(vec![0u8; 1]) {
            Err(Overflow::Connection { .. }) => {}
            other => panic!("a queue past its ceiling accepted more: {other:?}"),
        }
    }

    /// Many tiny messages are bounded by count, which the byte ceiling does
    /// not reach.
    #[test]
    fn a_flood_of_tiny_messages_is_bounded_by_count() {
        let bus = Bus::new();
        let (_client, server) = pair();
        let outbox = bus.outbox_for(server);
        for which in 0..MAX_OUTGOING_MESSAGES {
            if let Err(why) = outbox.offer(vec![0u8; 1]) {
                panic!("frame {which} refused: {why:?}");
            }
        }
        match outbox.offer(vec![0u8; 1]) {
            Err(Overflow::Connection { bytes, frames }) => {
                assert!(
                    bytes < MAX_OUTGOING_BYTES,
                    "the count guard did not fire; the byte ceiling did"
                );
                // The refusal has to say which guard fired, or a flood of tiny
                // frames is diagnosed as "bytes behind" with a byte count far
                // under the ceiling — which reads as a broker miscounting its
                // own bound.
                assert_eq!(
                    frames, MAX_OUTGOING_MESSAGES,
                    "the refusal did not report the count that fired"
                );
            }
            other => panic!("the count guard did not fire: {other:?}"),
        }
    }

    /// The COUNT ceiling counts the writer's frame too.
    ///
    /// A draft counted `frames.len()` alone, so taking one frame off the deque
    /// made room for one more than the backstop allows — the byte ceiling had
    /// learned to count the writer's frame and the count ceiling had not.
    #[test]
    fn the_count_ceiling_counts_the_frame_in_the_writers_hands() {
        let bus = Bus::new();
        let (_client, server) = pair();
        let outbox = bus.outbox_for(server);
        for which in 0..MAX_OUTGOING_MESSAGES {
            if let Err(why) = outbox.offer(vec![0u8; 1]) {
                panic!("frame {which} refused: {why:?}");
            }
        }
        // The writer takes one. The deque is one shorter; what the connection
        // is holding is not.
        let taken = outbox.take().expect("a frame to write");
        assert_eq!(taken.len(), 1);
        assert!(
            matches!(
                outbox.offer(vec![0u8; 1]),
                Err(Overflow::Connection { .. })
            ),
            "taking a frame off the deque made room past the count ceiling"
        );
        outbox.finished(1);
        // And once it really is written, there is room again.
        assert!(outbox.offer(vec![0u8; 1]).is_ok());
    }

    /// The bus's own ceiling, which no single connection's reaches.
    #[test]
    fn the_bus_bounds_what_every_connection_holds_together() {
        let bus = Bus::new();
        let mut held = Vec::new();
        let mut boxes = Vec::new();
        for _ in 0..8 {
            let (client, server) = pair();
            held.push(client);
            boxes.push(bus.outbox_for(server));
        }
        // Each takes one whole message, which its OWN ceiling allows. The
        // bus's does not, and it is the bus's that has to bind.
        let mut refused = 0;
        for outbox in &boxes {
            match outbox.offer(vec![0u8; MAX_OUTGOING_BYTES]) {
                Ok(()) => {}
                Err(Overflow::Bus(_)) => refused += 1,
                other => panic!("refused for the wrong reason: {other:?}"),
            }
        }
        assert!(refused > 0, "the bus ceiling never fired");
        assert!(
            bus.queued_bytes() <= MAX_OUTGOING_BYTES_TOTAL,
            "the bus went past its ceiling: {} > {MAX_OUTGOING_BYTES_TOTAL}",
            bus.queued_bytes()
        );

        // NOT the remedy working — the remedy declining to act. Nothing here
        // has said Hello, so the directory is empty and there is no addressable
        // consumer to relieve. A draft headed this "the largest consumer goes,
        // and its bytes come back", which is the opposite of what the two
        // assertions below check, and would have let a reader credit this test
        // with coverage it does not have.
        let before = bus.queued_bytes();
        let relieved = bus.relieve_largest();
        assert!(
            relieved.is_none(),
            "nothing has said Hello, so nothing is addressable to relieve"
        );
        assert_eq!(bus.queued_bytes(), before, "an unnamed peer was relieved");
    }

    /// §D's remedy when the BUS ceiling is reached: the largest consumer goes,
    /// rather than everyone being refused. The test above proves the ceiling
    /// binds; this one proves the remedy picks the right connection and gives
    /// its bytes back, which is what makes the bus usable again afterwards.
    #[test]
    fn the_bus_relieves_whoever_is_holding_the_most() {
        let bus = Bus::new();
        let mut kept = Vec::new();
        let mut boxes = Vec::new();
        let mut names = Vec::new();
        for which in 0..3usize {
            let (client, server) = pair();
            kept.push(client);
            let outbox = bus.outbox_for(server);
            let name = bus
                .join(&outbox, 1000, 5000 + which as i32)
                .expect("join");
            names.push(name);
            boxes.push(outbox);
        }

        // The middle one holds twice what its neighbours do.
        let small = MAX_OUTGOING_BYTES / 8;
        for (which, outbox) in boxes.iter().enumerate() {
            let size = if which == 1 { small * 2 } else { small };
            outbox.offer(vec![0u8; size]).expect("push");
        }

        let before = bus.queued_bytes();
        let relieved = bus.relieve_largest().expect("somebody was relieved");
        assert_eq!(
            relieved,
            names.get(1).cloned().unwrap_or_default(),
            "the wrong connection was relieved"
        );
        assert!(
            bus.queued_bytes() < before,
            "relieving a connection did not give its bytes back: {} vs {before}",
            bus.queued_bytes()
        );
        // The relieved connection is closed, so nothing more is queued to it.
        let closed = boxes.get(1).expect("three outboxes");
        assert_eq!(closed.offer(vec![0u8; 8]), Err(Overflow::Closed));
        // Its neighbours are untouched and still take messages.
        let neighbour = boxes.first().expect("three outboxes");
        assert_eq!(neighbour.offer(vec![0u8; 8]), Ok(()));
    }

    /// A frame the writer is blocked on is STILL this connection's memory.
    ///
    /// The bound is on memory, not on a deque. A draft uncharged the frame in
    /// `take`, before the write, so a writer stuck against a peer that had
    /// stopped reading held a whole 16 MiB message that no counter knew about
    /// — and the queue it came from now looked empty, so the per-connection
    /// exception admitted another. That is the bus multiplier rebuilt one
    /// frame lower down, and no test could see it because the tests that
    /// measured the ceilings never started a writer.
    #[test]
    fn a_frame_in_the_writers_hands_is_still_this_connections_memory() {
        use std::io::Write;

        let bus = Bus::new();
        let (client, server) = pair();
        let outbox = bus.outbox_for(server);
        let writing = Arc::clone(&outbox);
        let writer = std::thread::spawn(move || {
            while let Some(frame) = writing.take() {
                let size = frame.len();
                let mut stream = writing.stream();
                let outcome = stream.write_all(&frame);
                drop(frame);
                writing.finished(size);
                if outcome.is_err() {
                    return;
                }
            }
        });

        outbox.push(vec![0u8; MAX_OUTGOING_BYTES]).expect("push");
        // The client never reads, so the socket buffer fills and the writer
        // blocks part-way through the frame — with the frame in its hands.
        let deadline = Instant::now() + Duration::from_secs(20);
        while outbox.in_flight() == 0 {
            assert!(Instant::now() < deadline, "the writer never took the frame");
            std::thread::yield_now();
        }
        assert_eq!(outbox.queued(), 0, "the deque should be empty by now");
        assert_eq!(
            outbox.pending(),
            MAX_OUTGOING_BYTES,
            "an in-flight frame vanished from this connection's account"
        );
        assert_eq!(
            bus.queued_bytes(),
            MAX_OUTGOING_BYTES,
            "an in-flight frame vanished from the bus's account"
        );
        // So a second maximum message does NOT fit: the connection is holding
        // one already, whoever happens to be holding it.
        assert!(
            matches!(
                outbox.offer(vec![0u8; MAX_OUTGOING_BYTES]),
                Err(Overflow::Connection { .. })
            ),
            "an emptied deque let a second maximum message in"
        );

        drop(client);
        outbox.close();
        let _ = writer.join();
        assert_eq!(
            bus.queued_bytes(),
            0,
            "closing did not give the in-flight bytes back"
        );
    }

    /// An in-flight frame is given back ONCE.
    ///
    /// Both `close` and the writer's `finished` reclaim, and they can both
    /// fire for the same frame: `close` shuts the socket down, which is what
    /// releases the writer to call `finished`. If neither guards against the
    /// other the bus's count walks DOWN past what is really held — the
    /// direction that silently removes the ceiling rather than tightening it,
    /// and the one a `saturating_sub` floor at zero hides completely unless
    /// somebody else is holding bytes at the time.
    #[test]
    fn an_in_flight_frame_is_given_back_once() {
        use std::io::Write;

        let bus = Bus::new();
        // A bystander holding a frame, so the count has somewhere to fall to
        // that is not zero.
        let (_watching, watched) = pair();
        let bystander = bus.outbox_for(watched);
        bystander
            .push(vec![0u8; MAX_OUTGOING_BYTES])
            .expect("the bystander's frame");

        let (client, server) = pair();
        let outbox = bus.outbox_for(server);
        let writing = Arc::clone(&outbox);
        let writer = std::thread::spawn(move || {
            while let Some(frame) = writing.take() {
                let size = frame.len();
                let mut stream = writing.stream();
                let outcome = stream.write_all(&frame);
                drop(frame);
                writing.finished(size);
                if outcome.is_err() {
                    return;
                }
            }
        });
        outbox.push(vec![0u8; MAX_OUTGOING_BYTES]).expect("push");
        let deadline = Instant::now() + Duration::from_secs(20);
        while outbox.in_flight() == 0 {
            assert!(Instant::now() < deadline, "the writer never took the frame");
            std::thread::yield_now();
        }
        assert_eq!(bus.queued_bytes(), MAX_OUTGOING_BYTES * 2);

        // Closing reclaims the in-flight frame AND releases the writer, which
        // then reports the same frame finished.
        outbox.close();
        drop(client);
        let _ = writer.join();
        assert_eq!(
            bus.queued_bytes(),
            MAX_OUTGOING_BYTES,
            "one frame was given back twice; the bystander's bytes vanished with it"
        );
    }

    /// `close` releases the SOCKET, not the ALLOCATION.
    ///
    /// A draft had `close` reclaim the in-flight bytes itself. That is earlier
    /// than the writer lets go: `close` shuts the socket down, and the blocked
    /// `sendmsg` still has to return before the `Vec` is freed. Budget freed
    /// in that window is real budget against memory that is still live, so a
    /// caller retrying straight after a relief could admit a second maximum
    /// message beside the first. Only the writer knows when the bytes are
    /// gone, so only the writer gives them back.
    ///
    /// No writer thread here: `take` stands in for one, which is what makes
    /// the window observable at all rather than a microsecond wide.
    #[test]
    fn closing_does_not_free_bytes_the_writer_still_holds() {
        let bus = Bus::new();
        let (_client, server) = pair();
        let outbox = bus.outbox_for(server);
        outbox.push(vec![0u8; 4096]).expect("push");

        let frame = outbox.take().expect("a frame to write");
        assert_eq!(bus.queued_bytes(), 4096);

        outbox.close();
        assert_eq!(
            bus.queued_bytes(),
            4096,
            "close freed budget against memory that is still live"
        );

        drop(frame);
        outbox.finished(4096);
        assert_eq!(bus.queued_bytes(), 0, "the writer's release did not land");
    }

    /// Nobody is relieved when nobody is holding anything. The remedy names a
    /// victim and the caller retries on the strength of it, so "the largest
    /// consumer" has to be a consumer.
    #[test]
    fn nothing_is_relieved_when_nothing_is_held() {
        let bus = Bus::new();
        let mut kept = Vec::new();
        for which in 0..3usize {
            let (client, server) = pair();
            kept.push(client);
            let outbox = bus.outbox_for(server);
            bus.join(&outbox, 1000, 9300 + which as i32).expect("join");
        }
        assert_eq!(bus.queued_bytes(), 0);
        assert!(
            bus.relieve_largest().is_none(),
            "a connection holding nothing was disconnected as the largest consumer"
        );
    }

    /// The remedy frees a frame the WRITER is holding, and does it in time
    /// for the retry that follows.
    ///
    /// `close` reclaims the in-flight bytes itself rather than waiting for the
    /// blocked writer to come back from `sendmsg` and hand them over. Without
    /// that, `relieve_largest` returns before the budget has moved and the
    /// broker's one retry fails against a bus it has just made room in — the
    /// remedy would report success and change nothing.
    #[test]
    fn the_remedy_frees_a_frame_the_writer_is_holding() {
        use std::io::Write;

        let bus = Bus::new();
        let mut kept = Vec::new();
        let mut writers = Vec::new();
        for which in 0..4usize {
            let (client, server) = pair();
            kept.push(client);
            let outbox = bus.outbox_for(server);
            bus.join(&outbox, 1000, 9200 + which as i32).expect("join");
            outbox.push(vec![0u8; MAX_OUTGOING_BYTES]).expect("push");
            let writing = Arc::clone(&outbox);
            writers.push(std::thread::spawn(move || {
                while let Some(frame) = writing.take() {
                    let size = frame.len();
                    let mut stream = writing.stream();
                    let outcome = stream.write_all(&frame);
                    drop(frame);
                    writing.finished(size);
                    if outcome.is_err() {
                        return;
                    }
                }
            }));
            // Nobody reads, so the frame ends up in the writer's hands.
            let deadline = Instant::now() + Duration::from_secs(20);
            while outbox.in_flight() == 0 {
                assert!(Instant::now() < deadline, "the writer never took the frame");
                std::thread::yield_now();
            }
        }
        assert_eq!(bus.queued_bytes(), MAX_OUTGOING_BYTES_TOTAL);

        let (client, server) = pair();
        kept.push(client);
        let latecomer = bus.outbox_for(server);
        assert!(matches!(
            latecomer.offer(vec![0u8; 8]),
            Err(Overflow::Bus(_))
        ));
        assert!(bus.relieve_largest().is_some(), "nobody was relieved");
        assert!(
            latecomer.offer(vec![0u8; 8]).is_ok(),
            "the remedy did not free the frame its victim was writing"
        );

        latecomer.close();
        drop(kept);
        for writer in writers {
            let _ = writer.join();
        }
    }

    /// Relieving the largest consumer MAKES ROOM. That is what turns §D's
    /// remedy from a diagnosis into a remedy: the broker retries the append it
    /// just failed, rather than reporting the ceiling to whoever asked.
    #[test]
    fn relieving_the_largest_consumer_makes_room() {
        let bus = Bus::new();
        let mut kept = Vec::new();
        let mut boxes = Vec::new();
        for which in 0..4usize {
            let (client, server) = pair();
            kept.push(client);
            let outbox = bus.outbox_for(server);
            bus.join(&outbox, 1000, 9000 + which as i32).expect("join");
            outbox.push(vec![0u8; MAX_OUTGOING_BYTES]).expect("push");
            boxes.push(outbox);
        }
        assert_eq!(bus.queued_bytes(), MAX_OUTGOING_BYTES_TOTAL);

        // One more byte does not fit anywhere.
        let (client, server) = pair();
        kept.push(client);
        let latecomer = bus.outbox_for(server);
        assert!(matches!(
            latecomer.offer(vec![0u8; 8]),
            Err(Overflow::Bus(_))
        ));

        assert!(bus.relieve_largest().is_some(), "nobody was relieved");
        assert!(
            latecomer.offer(vec![0u8; 8]).is_ok(),
            "the remedy freed nothing"
        );
    }

    /// Closing gives back what was queued. Without it the bus's count only
    /// rises, and a broker that has served enough connections believes it is
    /// full of messages nobody is waiting for.
    #[test]
    fn closing_a_connection_returns_its_queue_to_the_bus() {
        let bus = Bus::new();
        let (_client, server) = pair();
        let outbox = bus.outbox_for(server);
        outbox.offer(vec![0u8; 4096]).expect("push");
        assert_eq!(bus.queued_bytes(), 4096);
        outbox.close();
        assert_eq!(bus.queued_bytes(), 0, "the queue was not given back");
        assert_eq!(
            outbox.offer(vec![0u8; 1]),
            Err(Overflow::Closed),
            "a closed outbox accepted a frame"
        );
    }

    /// The writer thread is what makes this a queue rather than a blocking
    /// write: what goes in comes out of the socket, in order, and the thread
    /// ends when the outbox closes.
    ///
    /// The thread here writes with `std`, where the transport's writes with
    /// `sendmsg`. That is the point of the split: this file's job is that the
    /// frames come out in the order they went in, and it can prove that
    /// without going anywhere near the rostered syscall surface.
    #[test]
    fn what_is_queued_reaches_the_socket_in_order() {
        use std::io::{Read, Write};

        let bus = Bus::new();
        let (mut client, server) = pair();
        let outbox = bus.outbox_for(server);
        let writing = Arc::clone(&outbox);
        let writer = std::thread::spawn(move || {
            while let Some(frame) = writing.take() {
                let mut stream = writing.stream();
                if stream.write_all(&frame).is_err() {
                    return;
                }
            }
        });

        for which in 0u8..8 {
            outbox.offer(vec![which; 4]).expect("push");
        }
        let mut got = [0u8; 32];
        let mut have = 0;
        while have < got.len() {
            match client.read(&mut got[have..]) {
                Ok(0) => break,
                Ok(read) => have += read,
                Err(error) => panic!("read: {error}"),
            }
        }
        assert_eq!(have, 32, "not everything queued was written");
        for which in 0u8..8 {
            let at = usize::from(which) * 4;
            assert_eq!(
                got.get(at..at + 4),
                Some([which; 4].as_slice()),
                "frame {which} arrived out of order or altered"
            );
        }
        outbox.close();
        writer.join().expect("the writer thread did not end");
    }
}

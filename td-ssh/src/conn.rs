//! The post-authentication connection core both roles share: one thread reads
//! packets, one owns the transport and channels and is fed by a channel of
//! events, and each stream blocks on its own thread for window credit, so no
//! `poll` or signal surface is needed (DESIGN.md section 6).

use crate::error::{peer_text, Error, Result};
use crate::kex::{Kex, KexEnvironment, KexOutput};
use crate::msg;
use crate::packet::{Counters, PacketReader, PacketWriter, TransportKey};
use crate::wire::{Reader, Writer};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{
    sync_channel, Receiver, RecvError, RecvTimeoutError, SendError, SyncSender, TrySendError,
};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

pub const CHANNEL_WINDOW: u32 = 2 * 1024 * 1024;
pub const CHANNEL_MAX_PACKET: u32 = 32 * 1024;
/// Counts events, and so bounds the ones with nothing on the heap.
pub(crate) const EVENT_QUEUE: usize = 4096;
/// What queued output may bring the queue to. Sources wait above it, released as
/// the core drains, so a core blocked in a write pins this much and no more, for as
/// long as the write's own limit lets it block.
pub const EVENT_BUDGET: usize = 2 * 1024 * 1024;
/// What the reader may bring it to. Twice as much, or output filling the queue
/// would stop the reads: nothing would stamp a peer's arrivals, and two td peers
/// each blocked in a write would each wait on the other's reader.
pub const READER_BUDGET: usize = 2 * EVENT_BUDGET;
/// What one queued message costs beyond its bytes, so that a flood of tiny ones
/// is charged for their allocations.
const MESSAGE_OVERHEAD: usize = 64;
/// Outgoing traffic a rekey may hold back. Sources stop taking credit during a
/// rekey, so what reaches here is what they had already taken, which the budget
/// and one packet per source bound, and control messages. Only a peer flooding
/// those during a rekey can pass this.
const MAX_HELD_BYTES: usize = 8 * 1024 * 1024;

pub enum Event {
    /// A message and the sequence number `UNIMPLEMENTED` would have to name.
    Packet {
        payload: Vec<u8>,
        seq: u32,
    },
    TransportEnded(Error),
    Output {
        channel: u32,
        stderr: bool,
        data: Vec<u8>,
    },
    OutputEof {
        channel: u32,
        stderr: bool,
    },
    Consumed {
        channel: u32,
        bytes: u32,
    },
    /// A sink that reports its consumer going away found it gone.
    SinkBroken {
        channel: u32,
    },
    ChildExited {
        channel: u32,
        status: Result<ExitStatus>,
    },
    Tick,
    WindowChanged {
        columns: u32,
        rows: u32,
        width: u32,
        height: u32,
    },
    /// The `~.` escape.
    EscapeDisconnect,
}

impl Event {
    /// What this event holds on the heap, charged against the budget.
    fn cost(&self) -> usize {
        match self {
            Event::Packet { payload, .. } => payload.len().saturating_add(MESSAGE_OVERHEAD),
            Event::Output { data, .. } => data.len().saturating_add(MESSAGE_OVERHEAD),
            _ => 0,
        }
    }

    fn budget(&self) -> usize {
        match self {
            Event::Packet { .. } => READER_BUDGET,
            _ => EVENT_BUDGET,
        }
    }
}

/// What the threads of one connection share with its core: whether sources may
/// take credit, how much the queue holds, and how many packets have arrived.
pub struct Flow {
    state: Mutex<FlowState>,
    changed: Condvar,
    arrivals: AtomicU64,
}

struct FlowState {
    /// False from a KEXINIT to the end of that exchange (DESIGN.md section 6).
    transport_open: bool,
    queued: usize,
    receiver_gone: bool,
}

impl Flow {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(FlowState {
                transport_open: true,
                queued: 0,
                receiver_gone: false,
            }),
            changed: Condvar::new(),
            arrivals: AtomicU64::new(0),
        })
    }

    fn set_transport_open(&self, open: bool) {
        if let Ok(mut state) = self.state.lock() {
            if state.transport_open != open {
                state.transport_open = open;
                self.changed.notify_all();
            }
        }
    }

    /// Waits for room up to `limit`. One message always fits an empty queue,
    /// whatever its size. False once nothing will ever drain it.
    fn reserve(&self, cost: usize, limit: usize) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        loop {
            if state.receiver_gone {
                return false;
            }
            if state.queued == 0 || state.queued.saturating_add(cost) <= limit {
                state.queued = state.queued.saturating_add(cost);
                return true;
            }
            state = match self.changed.wait(state) {
                Ok(state) => state,
                Err(_) => return false,
            };
        }
    }

    /// Room up to `limit` now, or none, as `reserve` would grant it.
    fn try_reserve(&self, cost: usize, limit: usize) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        if state.receiver_gone || (state.queued > 0 && state.queued.saturating_add(cost) > limit) {
            return false;
        }
        state.queued = state.queued.saturating_add(cost);
        true
    }

    fn release(&self, cost: usize) {
        if let Ok(mut state) = self.state.lock() {
            state.queued = state.queued.saturating_sub(cost);
            self.changed.notify_all();
        }
    }

    fn receiver_gone(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.receiver_gone = true;
            self.changed.notify_all();
        }
    }

    /// Stamped by the reader as each packet comes off the socket, ahead of any
    /// queue: whatever the core is doing, this says the peer is there.
    pub(crate) fn stamp_arrival(&self) {
        self.arrivals.fetch_add(1, Ordering::Relaxed);
    }

    pub fn arrivals(&self) -> u64 {
        self.arrivals.load(Ordering::Relaxed)
    }
}

/// The sending half of the event queue. `send` waits for room in the budget, so
/// the core itself never sends: it would be waiting on its own draining.
#[derive(Clone)]
pub struct EventSender {
    sender: SyncSender<Event>,
    flow: Arc<Flow>,
}

impl EventSender {
    pub fn send(&self, event: Event) -> std::result::Result<(), SendError<()>> {
        let cost = event.cost();
        if cost > 0 && !self.flow.reserve(cost, event.budget()) {
            return Err(SendError(()));
        }
        self.sender.send(event).map_err(|_| {
            self.flow.release(cost);
            SendError(())
        })
    }

    /// Without waiting. Only for what costs nothing against the budget.
    pub fn try_send(&self, event: Event) -> bool {
        self.sender.try_send(event).is_ok()
    }

    /// Channel data without waiting for budget or a slot in the queue, for a
    /// thread that must not block: the data back if either is short.
    pub fn send_output_now(
        &self,
        channel: u32,
        stderr: bool,
        data: Vec<u8>,
    ) -> std::result::Result<(), Vec<u8>> {
        let cost = data.len().saturating_add(MESSAGE_OVERHEAD);
        if !self.flow.try_reserve(cost, EVENT_BUDGET) {
            return Err(data);
        }
        let event = Event::Output {
            channel,
            stderr,
            data,
        };
        match self.sender.try_send(event) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(event) | TrySendError::Disconnected(event)) => {
                self.flow.release(cost);
                // Only the event built above comes back.
                match event {
                    Event::Output { data, .. } => Err(data),
                    _ => Err(Vec::new()),
                }
            }
        }
    }

    pub fn flow(&self) -> &Arc<Flow> {
        &self.flow
    }
}

/// The core's half. Taking an event releases its share of the budget.
pub struct EventReceiver {
    receiver: Receiver<Event>,
    flow: Arc<Flow>,
}

impl EventReceiver {
    pub fn recv(&self) -> std::result::Result<Event, RecvError> {
        let event = self.receiver.recv()?;
        self.taken(&event);
        Ok(event)
    }

    pub fn recv_timeout(&self, limit: Duration) -> std::result::Result<Event, RecvTimeoutError> {
        let event = self.receiver.recv_timeout(limit)?;
        self.taken(&event);
        Ok(event)
    }

    fn taken(&self, event: &Event) {
        let cost = event.cost();
        if cost > 0 {
            self.flow.release(cost);
        }
    }
}

impl Drop for EventReceiver {
    fn drop(&mut self) {
        self.flow.receiver_gone();
    }
}

pub fn event_channel() -> (EventSender, EventReceiver) {
    let (sender, receiver) = sync_channel(EVENT_QUEUE);
    let flow = Flow::new();
    (
        EventSender {
            sender,
            flow: flow.clone(),
        },
        EventReceiver { receiver, flow },
    )
}

/// Runs the first exchange synchronously, under strict KEX: nothing but the KEX
/// sequence may appear, the first packet included, or the connection ends.
/// Returns the session ID.
pub fn initial_kex<R: Read, W: Write, E: KexEnvironment>(
    reader: &mut PacketReader<R>,
    writer: &mut PacketWriter<W>,
    kex: &mut Kex,
    env: &mut E,
) -> Result<[u8; 32]> {
    writer.write_packet(&kex.start(env)?)?;
    loop {
        let payload = reader.read_packet()?;
        let number = payload.first().copied().unwrap_or(0);
        if number == msg::DISCONNECT {
            return Err(disconnected(&payload));
        }
        if !msg::is_kex(number) {
            return Err(Error::protocol(format!(
                "message {number} during the initial key exchange"
            )));
        }
        if kex_step(reader, writer, kex, env, &payload)? {
            return kex
                .session_id()
                .ok_or_else(|| Error::protocol("key exchange finished without a session"));
        }
    }
}

/// One KEX message on a link this thread alone reads and writes. True when the
/// exchange has finished.
pub fn kex_step<R: Read, W: Write, E: KexEnvironment>(
    reader: &mut PacketReader<R>,
    writer: &mut PacketWriter<W>,
    kex: &mut Kex,
    env: &mut E,
    payload: &[u8],
) -> Result<bool> {
    let mut output = kex.on_message(payload, env)?;
    apply_output(writer, &mut output)?;
    if let Some(key) = output.rx_key {
        reader.install_key(key);
    }
    Ok(output.finished)
}

/// What a peer's `SSH_MSG_DISCONNECT` says, safe to print.
pub fn disconnected(payload: &[u8]) -> Error {
    let mut reader = Reader::new(payload.get(1..).unwrap_or_default());
    let _ = reader.u32();
    Error::Disconnected(peer_text(reader.string().unwrap_or_default()))
}

fn apply_output<W: Write>(writer: &mut PacketWriter<W>, output: &mut KexOutput) -> Result<()> {
    for packet in &output.send {
        writer.write_packet(packet)?;
    }
    if let Some(key) = output.tx_key.take() {
        writer.install_key(key);
    }
    Ok(())
}

/// `thread::spawn` panics when no thread can be had. Here that ends this connection
/// and nothing else.
pub(crate) fn start_thread(
    events: &EventSender,
    body: impl FnOnce() + Send + 'static,
) -> Option<thread::JoinHandle<()>> {
    match thread::Builder::new().spawn(body) {
        Ok(handle) => Some(handle),
        Err(error) => {
            let _ = events.try_send(Event::TransportEnded(Error::Io(error)));
            None
        }
    }
}

/// Starts the reader thread. After handing up `NEWKEYS` it waits for the key
/// that the peer's next packet is sealed under, which only the core can derive.
/// Each packet is stamped as it arrives, before it waits for room in the queue.
/// `at_end` runs when the socket has ended, before that is queued: the core may be
/// blocked in a write and read no queue until something ends that.
pub fn spawn_reader<R: Read + Send + 'static>(
    mut reader: PacketReader<R>,
    events: EventSender,
    at_end: impl FnOnce() + Send + 'static,
) -> SyncSender<TransportKey> {
    let (key_sender, key_receiver) = sync_channel::<TransportKey>(1);
    let starter = events.clone();
    start_thread(&starter, move || loop {
        let payload = match reader.read_packet() {
            Ok(payload) => payload,
            Err(error) => {
                // A protocol error leaves the core a socket to say so on.
                if matches!(error, Error::Io(_)) {
                    at_end();
                }
                let _ = events.send(Event::TransportEnded(error));
                return;
            }
        };
        events.flow().stamp_arrival();
        let is_new_keys = payload.first() == Some(&msg::NEWKEYS);
        let seq = reader.counters().seq.wrapping_sub(1);
        if events.send(Event::Packet { payload, seq }).is_err() {
            return;
        }
        if is_new_keys {
            match key_receiver.recv() {
                Ok(key) => reader.install_key(key),
                Err(_) => return,
            }
        }
    });
    key_sender
}

/// Transport duties for the threaded stage: outgoing queueing while a rekey
/// holds traffic back, rekey decisions, and transport-level messages.
pub struct Conn<W, E> {
    writer: PacketWriter<W>,
    rx_keys: SyncSender<TransportKey>,
    kex: Kex,
    env: E,
    held: VecDeque<Vec<u8>>,
    held_bytes: usize,
    rx_bytes: u64,
    rx_packets: u64,
    rekey_bytes: u64,
    completed_rekeys: u32,
    flow: Arc<Flow>,
}

const REKEY_BYTES: u64 = 1 << 30;
const REKEY_PACKETS: u64 = 1 << 28;

impl<W: Write, E: KexEnvironment> Conn<W, E> {
    pub fn new(
        writer: PacketWriter<W>,
        rx_keys: SyncSender<TransportKey>,
        kex: Kex,
        env: E,
        flow: Arc<Flow>,
    ) -> Self {
        Self {
            writer,
            rx_keys,
            kex,
            env,
            held: VecDeque::new(),
            held_bytes: 0,
            rx_bytes: 0,
            rx_packets: 0,
            rekey_bytes: REKEY_BYTES,
            completed_rekeys: 0,
            flow,
        }
    }

    pub fn completed_rekeys(&self) -> u32 {
        self.completed_rekeys
    }

    /// Lowers the byte limit in either direction. It can never raise it.
    pub fn limit_rekey_bytes(&mut self, limit: u64) {
        self.rekey_bytes = limit.min(REKEY_BYTES);
    }

    /// Credit for a channel of this connection, held back with the rest of them
    /// while a key exchange is under way.
    pub fn new_credit(&self, window: u32) -> Arc<Credit> {
        Credit::new(window, self.flow.clone())
    }

    /// Packets taken off the socket so far, however far behind the core is.
    pub fn arrivals(&self) -> u64 {
        self.flow.arrivals()
    }

    pub fn counters(&self) -> Counters {
        self.writer.counters()
    }

    /// Whether a key exchange is under way, holding traffic back.
    pub fn rekeying(&self) -> bool {
        self.kex.in_progress()
    }

    /// Sends now, or holds until a rekey in progress has finished. RFC 4253
    /// section 7.1 forbids anything but KEX traffic after our KEXINIT.
    pub fn send(&mut self, payload: Vec<u8>) -> Result<()> {
        if self.kex.in_progress() {
            self.held_bytes = self
                .held_bytes
                .saturating_add(payload.len())
                .saturating_add(MESSAGE_OVERHEAD);
            if self.held_bytes > MAX_HELD_BYTES {
                return Err(Error::protocol(
                    "too much traffic held back during a key exchange",
                ));
            }
            self.held.push_back(payload);
            return Ok(());
        }
        self.writer.write_packet(&payload)?;
        self.maybe_rekey()
    }

    fn maybe_rekey(&mut self) -> Result<()> {
        let wanted = self.writer.counters().wants_rekey()
            || self.writer.counters().bytes >= self.rekey_bytes
            || self.rx_bytes >= self.rekey_bytes
            || self.rx_packets >= REKEY_PACKETS;
        if wanted && !self.kex.in_progress() {
            let init = self.kex.start(&mut self.env)?;
            self.flow.set_transport_open(false);
            self.writer.write_packet(&init)?;
        }
        Ok(())
    }

    /// Handles what the transport itself owns. Returns a message for the
    /// connection layer, or None when it was consumed here.
    pub fn on_packet(&mut self, payload: Vec<u8>) -> Result<Option<Vec<u8>>> {
        self.rx_bytes = self.rx_bytes.saturating_add(payload.len() as u64);
        self.rx_packets = self.rx_packets.saturating_add(1);
        let number = payload.first().copied().unwrap_or(0);
        if number == msg::DISCONNECT {
            return Err(disconnected(&payload));
        }
        if msg::is_kex(number) {
            let output = self.kex.on_message(&payload, &mut self.env)?;
            // Open again only once what was held has gone out, or a source
            // would overtake it.
            self.flow.set_transport_open(false);
            self.apply(output)?;
            self.flow.set_transport_open(!self.kex.in_progress());
            return Ok(None);
        }
        // Before any ignoring, so that nothing a peer sends gets past either.
        if self.kex.peer_committed() && !msg::is_generic_transport(number) {
            return Err(Error::protocol(format!(
                "message {number} after the peer's KEXINIT"
            )));
        }
        // Every packet counts: a receive-only connection, or one fed nothing but
        // IGNORE, sends too rarely to notice otherwise.
        self.maybe_rekey()?;
        match number {
            msg::IGNORE | msg::DEBUG | msg::UNIMPLEMENTED | msg::EXT_INFO => Ok(None),
            _ => Ok(Some(payload)),
        }
    }

    fn apply(&mut self, mut output: KexOutput) -> Result<()> {
        apply_output(&mut self.writer, &mut output)?;
        if let Some(key) = output.rx_key {
            self.rx_keys
                .send(key)
                .map_err(|_| Error::protocol("packet reader has gone"))?;
            self.rx_bytes = 0;
            self.rx_packets = 0;
        }
        if output.finished {
            self.completed_rekeys = self.completed_rekeys.saturating_add(1);
            while let Some(packet) = self.held.pop_front() {
                self.writer.write_packet(&packet)?;
            }
            self.held_bytes = 0;
            self.maybe_rekey()?;
        }
        Ok(())
    }

    /// Best effort: the connection is ending either way.
    pub fn disconnect(&mut self, code: u32, text: &str) {
        let mut writer = Writer::message(msg::DISCONNECT);
        writer.u32(code).str(text).str("");
        let _ = self.writer.write_packet(writer.as_slice());
    }

    pub fn env_mut(&mut self) -> &mut E {
        &mut self.env
    }
}

/// Bytes the peer's channel window permits. Stream threads block here, which
/// is what backpressures a child's pipe, and they block here too while the
/// connection's key exchange is under way. The counters change only under the
/// flow's lock, so a waiter cannot miss a change.
pub struct Credit {
    flow: Arc<Flow>,
    bytes: AtomicU64,
    closed: AtomicBool,
}

impl Credit {
    pub fn new(initial: u32, flow: Arc<Flow>) -> Arc<Self> {
        Arc::new(Self {
            flow,
            bytes: AtomicU64::new(u64::from(initial)),
            closed: AtomicBool::new(false),
        })
    }

    /// Nothing polices a peer granting more than the 2^32 - 1 RFC 4254 section
    /// 5.2 allows: a grant only lets us send, and the sum saturates in a `u64`.
    pub fn add(&self, bytes: u32) {
        if let Ok(_state) = self.flow.state.lock() {
            let total = self
                .bytes
                .load(Ordering::Relaxed)
                .saturating_add(u64::from(bytes));
            self.bytes.store(total, Ordering::Relaxed);
            self.flow.changed.notify_all();
        }
    }

    pub fn close(&self) {
        if let Ok(_state) = self.flow.state.lock() {
            self.closed.store(true, Ordering::Relaxed);
            self.flow.changed.notify_all();
        }
    }

    /// Tells one source to stop, wherever it waits for credit.
    pub fn stop_source(&self, stop: &AtomicBool) {
        if let Ok(_state) = self.flow.state.lock() {
            stop.store(true, Ordering::Relaxed);
            self.flow.changed.notify_all();
        }
    }

    /// Exactly `bytes` of credit if the window and the transport have room for it
    /// now, without waiting: for a path that has somewhere else to go if not.
    pub fn take_now(&self, bytes: usize) -> bool {
        let Ok(state) = self.flow.state.lock() else {
            return false;
        };
        let have = self.bytes.load(Ordering::Relaxed);
        let wanted = bytes as u64;
        if self.closed.load(Ordering::Relaxed)
            || state.receiver_gone
            || !state.transport_open
            || wanted == 0
            || have < wanted
        {
            return false;
        }
        self.bytes.store(have - wanted, Ordering::Relaxed);
        true
    }

    /// Waits for at least one byte of credit, taking at most `max`: None once the
    /// channel closed, or once `stop` is set.
    fn take_unless(&self, max: usize, stop: &AtomicBool) -> Option<usize> {
        let mut state = self.flow.state.lock().ok()?;
        loop {
            if self.closed.load(Ordering::Relaxed)
                || state.receiver_gone
                || stop.load(Ordering::Relaxed)
            {
                return None;
            }
            let bytes = self.bytes.load(Ordering::Relaxed);
            if state.transport_open && bytes > 0 {
                let granted = (max as u64).min(bytes);
                self.bytes.store(bytes - granted, Ordering::Relaxed);
                return Some(granted as usize);
            }
            state = self.flow.changed.wait(state).ok()?;
        }
    }
}

/// The most a source sends in one message: the peer's limit, within ours.
pub fn source_chunk(max_packet: usize) -> usize {
    max_packet.clamp(1, CHANNEL_MAX_PACKET as usize)
}

/// Pumps a local stream to a channel, one window-credited piece at a time.
/// Returns what stops it: `Credit::stop_source` makes it close the stream, once
/// its wait for credit or its next read has returned, and report the end. A read
/// cannot be interrupted, so a program writing nothing is not told until it writes.
pub fn spawn_source<R: Read + Send + 'static>(
    mut input: R,
    channel: u32,
    stderr: bool,
    max_packet: usize,
    credit: Arc<Credit>,
    events: EventSender,
) -> Arc<AtomicBool> {
    let chunk = source_chunk(max_packet);
    let starter = events.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let told = stop.clone();
    start_thread(&starter, move || {
        let mut buffer = vec![0u8; chunk];
        'stream: loop {
            let got = match input.read(&mut buffer) {
                Ok(0) => break,
                Ok(got) => got,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                // A PTY master reports its last slave's close as EIO.
                Err(_) => break,
            };
            let mut rest = buffer.get(..got).unwrap_or_default();
            while !rest.is_empty() {
                let Some(granted) = credit.take_unless(rest.len(), &told) else {
                    if told.load(Ordering::Relaxed) {
                        break 'stream;
                    }
                    return;
                };
                let (piece, tail) = rest.split_at(granted.min(rest.len()));
                let event = Event::Output {
                    channel,
                    stderr,
                    data: piece.to_vec(),
                };
                if events.send(event).is_err() {
                    return;
                }
                rest = tail;
            }
        }
        // Closed first: this is what a program still writing to it is told by.
        drop(input);
        let _ = events.send(Event::OutputEof { channel, stderr });
    });
    stop
}

/// What a sink does when its consumer goes away.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WhenBroken {
    /// Goes on crediting the window, discarding the output.
    Discard,
    /// The same, and says so once.
    Report,
}

/// The core's end of a sink: bytes it has charged to the channel's window, for
/// the sink's thread to write. They are appended to one buffer, not queued one
/// message at a time, so what waits is the window's bytes, held in at most twice
/// that between the buffer and what the thread has taken: a peer sending empty or
/// one-byte messages adds nothing a message of its own would cost. Dropping it is
/// the end of the stream, once what it holds is written.
pub struct Sink {
    shared: Arc<SinkShared>,
}

struct SinkShared {
    state: Mutex<SinkState>,
    ready: Condvar,
}

#[derive(Default)]
struct SinkState {
    pending: Vec<u8>,
    /// The `Sink` was dropped: nothing more will come.
    closed: bool,
    /// The thread has ended, or never started, so nothing will write what came.
    gone: bool,
}

impl Sink {
    /// Hands bytes on. False when no write is coming for them and the caller has to
    /// credit the window itself. Empty data is dropped here, being nothing to write.
    pub fn send(&self, data: &[u8]) -> bool {
        let Ok(mut state) = self.shared.state.lock() else {
            return false;
        };
        if state.gone {
            return false;
        }
        if !data.is_empty() {
            state.pending.extend_from_slice(data);
            self.shared.ready.notify_one();
        }
        true
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        if let Ok(mut state) = self.shared.state.lock() {
            state.closed = true;
        }
        self.shared.ready.notify_one();
    }
}

/// Marks the sink gone however its thread ends, or if it never ran.
struct SinkThread(Arc<SinkShared>);

impl Drop for SinkThread {
    fn drop(&mut self) {
        if let Ok(mut state) = self.0.state.lock() {
            state.gone = true;
            state.pending = Vec::new();
        }
    }
}

impl SinkThread {
    /// Everything appended since the last call, or None at the end of the stream.
    fn next(&self) -> Option<Vec<u8>> {
        let mut state = self.0.state.lock().ok()?;
        loop {
            if !state.pending.is_empty() {
                return Some(std::mem::take(&mut state.pending));
            }
            if state.closed {
                return None;
            }
            state = self.0.ready.wait(state).ok()?;
        }
    }
}

/// Drains a channel's inbound data to a local stream on its own thread, so a
/// slow consumer never stalls the core. The window is only reopened as bytes
/// actually land, which is what bounds what waits.
pub fn spawn_sink<W: Write + Send + 'static>(
    mut output: W,
    channel: u32,
    when_broken: WhenBroken,
    events: EventSender,
) -> (Sink, Option<thread::JoinHandle<()>>) {
    let shared = Arc::new(SinkShared {
        state: Mutex::new(SinkState::default()),
        ready: Condvar::new(),
    });
    let sink = Sink {
        shared: shared.clone(),
    };
    let starter = events.clone();
    let worker = SinkThread(shared);
    let handle = start_thread(&starter, move || {
        let mut broken = false;
        // Credit is reported once a packet's worth is owed, not at every write: a
        // keystroke's byte would otherwise wake the core for nothing, which on a
        // busy machine is a wait for a CPU. What is still owed is far inside the
        // window, and is reported when the stream ends.
        let mut owed = 0u32;
        while let Some(data) = worker.next() {
            // A packet's worth at a time, each credited as it lands, so a slow
            // consumer reopens the window as it goes, and not only once all that
            // waited is through.
            for piece in data.chunks(CHANNEL_MAX_PACKET as usize) {
                if !broken && (output.write_all(piece).is_err() || output.flush().is_err()) {
                    broken = true;
                    if when_broken == WhenBroken::Report {
                        let _ = events.send(Event::SinkBroken { channel });
                    }
                }
                owed = owed.saturating_add(u32::try_from(piece.len()).unwrap_or(u32::MAX));
                if owed >= CHANNEL_MAX_PACKET {
                    if events
                        .send(Event::Consumed {
                            channel,
                            bytes: owed,
                        })
                        .is_err()
                    {
                        return;
                    }
                    owed = 0;
                }
            }
        }
        if owed > 0 {
            let _ = events.send(Event::Consumed {
                channel,
                bytes: owed,
            });
        }
    });
    (sink, handle)
}

/// Consumed bytes that bring a grant. A slow link drains the window no faster than
/// it delivers, and until a grant a client's output side sends nothing at all, which
/// a server's keepalive reads as a dead client. Well under the window for that.
pub const WINDOW_GRANT_AT: u32 = 256 * 1024;

/// Inbound flow control: what the peer may still send, and when to grant more.
#[derive(Debug)]
pub struct LocalWindow {
    remaining: u64,
    consumed: u64,
}

impl Default for LocalWindow {
    fn default() -> Self {
        Self {
            remaining: u64::from(CHANNEL_WINDOW),
            consumed: 0,
        }
    }
}

impl LocalWindow {
    /// Charges incoming data. A peer overrunning the window breaks the protocol.
    pub fn charge(&mut self, bytes: usize) -> Result<()> {
        let bytes = bytes as u64;
        if bytes > self.remaining {
            return Err(Error::protocol("peer overran the channel window"));
        }
        self.remaining -= bytes;
        Ok(())
    }

    /// Bytes to grant back once `WINDOW_GRANT_AT` have been used.
    pub fn consumed(&mut self, bytes: u32) -> Option<u32> {
        self.consumed = self.consumed.saturating_add(u64::from(bytes));
        if self.consumed >= u64::from(WINDOW_GRANT_AT) {
            let grant = u32::try_from(self.consumed).unwrap_or(CHANNEL_WINDOW);
            self.consumed -= u64::from(grant);
            self.remaining = self.remaining.saturating_add(u64::from(grant));
            Some(grant)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;
    use crate::kex::Role;
    use std::io::Cursor;
    use std::time::Duration;

    impl Credit {
        /// `take_unless` with nothing to stop it, which no production source is.
        fn take(&self, max: usize) -> Option<usize> {
            self.take_unless(max, &AtomicBool::new(false))
        }
    }

    struct NoEnv;

    impl KexEnvironment for NoEnv {
        fn random(&mut self, out: &mut [u8]) -> Result<()> {
            out.fill(1);
            Ok(())
        }
        fn host_key_blob(&mut self) -> Result<Vec<u8>> {
            Ok(Vec::new())
        }
        fn sign(&mut self, _hash: &[u8; 32]) -> Result<Vec<u8>> {
            Ok(Vec::new())
        }
        fn check_host_key(&mut self, _blob: &[u8]) -> Result<()> {
            Ok(())
        }
    }

    fn engine() -> Kex {
        Kex::new(
            Role::Client,
            "SSH-2.0-a",
            "SSH-2.0-b",
            crate::kex::DEFAULT_KEX.to_vec(),
        )
    }

    fn framed(payload: &[u8]) -> Vec<u8> {
        let mut writer = PacketWriter::new(Vec::new());
        writer.write_packet(payload).unwrap();
        std::mem::take(writer.get_mut())
    }

    fn test_conn() -> Conn<Vec<u8>, NoEnv> {
        let (rx_keys, _unused) = sync_channel(1);
        Conn::new(
            PacketWriter::new(Vec::new()),
            rx_keys,
            engine(),
            NoEnv,
            Flow::new(),
        )
    }

    /// A core part way into a rekey the peer started.
    fn mid_peer_exchange() -> Conn<Vec<u8>, NoEnv> {
        let (rx_keys, _unused) = sync_channel(1);
        let mut conn = Conn::new(
            PacketWriter::new(Vec::new()),
            rx_keys,
            Kex::resume(Role::Client, "SSH-2.0-a", "SSH-2.0-b", [9; 32]),
            NoEnv,
            Flow::new(),
        );
        let mut peer = Kex::resume(Role::Server, "SSH-2.0-a", "SSH-2.0-b", [9; 32]);
        let init = peer.start(&mut NoEnv).unwrap();
        assert_eq!(conn.on_packet(init).unwrap(), None);
        assert!(conn.kex.peer_committed());
        conn
    }

    /// From the peer's KEXINIT to `NEWKEYS` only KEX and RFC 4253 section 7.1's
    /// transport-generic messages pass: not `EXT_INFO`, and nothing of the
    /// connection, however it would otherwise be ignored.
    #[test]
    fn during_the_peer_s_exchange_only_generic_transport_messages_pass() {
        for number in [msg::IGNORE, msg::DEBUG, msg::UNIMPLEMENTED] {
            let mut conn = mid_peer_exchange();
            assert_eq!(conn.on_packet(vec![number]).unwrap(), None, "{number}");
        }
        for number in [msg::EXT_INFO, msg::CHANNEL_DATA, msg::GLOBAL_REQUEST, 200] {
            let mut conn = mid_peer_exchange();
            let error = conn.on_packet(vec![number]).err().unwrap().to_string();
            assert!(
                error.contains("after the peer's KEXINIT"),
                "{number}: {error}"
            );
        }
    }

    /// What is ignored still counts toward a rekey, so a peer sending nothing else
    /// cannot keep a connection on one key past the limit.
    #[test]
    fn ignored_messages_count_toward_a_rekey() {
        for number in [msg::IGNORE, msg::DEBUG, msg::UNIMPLEMENTED, msg::EXT_INFO] {
            let mut conn = test_conn();
            conn.rx_packets = REKEY_PACKETS - 1;
            assert_eq!(conn.on_packet(vec![number]).unwrap(), None);
            assert!(conn.kex.in_progress(), "{number}");
            let mut conn = test_conn();
            conn.limit_rekey_bytes(1024);
            assert_eq!(conn.on_packet(vec![number; 2048]).unwrap(), None);
            assert!(conn.kex.in_progress(), "{number} by bytes");
        }
    }

    #[test]
    fn traffic_held_by_a_rekey_is_bounded_and_released_once_finished() {
        let mut conn = test_conn();
        conn.kex.start(&mut conn.env).unwrap();
        let piece = vec![0u8; 1024 * 1024 - MESSAGE_OVERHEAD];
        for _ in 0..8 {
            conn.send(piece.clone()).unwrap();
        }
        assert_eq!(conn.held_bytes, MAX_HELD_BYTES);
        let error = conn.send(vec![0]).err().unwrap().to_string();
        assert!(error.contains("held back"), "{error}");
        // Finishing an exchange empties the count along with the queue.
        conn.held.clear();
        conn.held_bytes = 1;
        conn.apply(KexOutput {
            send: Vec::new(),
            tx_key: None,
            rx_key: None,
            finished: true,
        })
        .unwrap();
        assert_eq!(conn.held_bytes, 0);
    }

    /// A flood of replies is charged for its allocations, not just its bytes.
    #[test]
    fn a_flood_of_tiny_messages_during_a_rekey_reaches_the_bound() {
        let mut conn = test_conn();
        conn.kex.start(&mut conn.env).unwrap();
        let most = MAX_HELD_BYTES / MESSAGE_OVERHEAD;
        let refused = (0..=most).find(|_| conn.send(vec![msg::REQUEST_FAILURE]).is_err());
        assert!(refused.is_some(), "{} held", conn.held.len());
    }

    /// Everything a healthy peer can cause fits, with room for its control traffic:
    /// the queue, and a packet in the hand of each source of the most channels.
    #[test]
    fn the_held_bound_is_above_all_that_sources_can_have_taken() {
        let sources = 2 * 16;
        let in_hand = sources * (CHANNEL_MAX_PACKET as usize + MESSAGE_OVERHEAD);
        assert!(READER_BUDGET + in_hand < MAX_HELD_BYTES * 3 / 4);
    }

    fn output(bytes: usize) -> Event {
        Event::Output {
            channel: 0,
            stderr: false,
            data: vec![0; bytes],
        }
    }

    /// Everything a test blocks on here has an end, if the code does what it says.
    fn within(limit: Duration, what: &str, done: impl Fn() -> bool) {
        let start = std::time::Instant::now();
        while !done() {
            assert!(start.elapsed() < limit, "never happened: {what}");
            thread::sleep(Duration::from_millis(2));
        }
    }

    const PATIENCE: Duration = Duration::from_secs(20);

    #[test]
    fn the_budget_stops_senders_by_bytes_and_draining_releases_them() {
        let (events, receiver) = event_channel();
        let piece = CHANNEL_MAX_PACKET as usize;
        let fits = EVENT_BUDGET / (piece + MESSAGE_OVERHEAD);
        for _ in 0..fits {
            events.send(output(piece)).unwrap();
        }
        let queued = |events: &EventSender| events.flow.state.lock().unwrap().queued;
        assert_eq!(queued(&events), fits * (piece + MESSAGE_OVERHEAD));
        // Far under the queue's count, and yet the next one waits.
        assert!(fits < EVENT_QUEUE);
        let sent = Arc::new(AtomicBool::new(false));
        let waiter = {
            let (events, sent) = (events.clone(), sent.clone());
            thread::spawn(move || {
                events.send(output(piece)).unwrap();
                sent.store(true, Ordering::SeqCst);
            })
        };
        thread::sleep(Duration::from_millis(50));
        assert!(!sent.load(Ordering::SeqCst), "a send past the budget");
        // What costs nothing is not held up by it, so a core can always be told.
        events.send(Event::Tick).unwrap();
        // Draining one releases it.
        receiver.recv().unwrap();
        waiter.join().unwrap();
        assert!(sent.load(Ordering::SeqCst));
        while queued(&events) > 0 {
            receiver.recv().unwrap();
        }
    }

    #[test]
    fn one_message_fits_an_empty_queue_whatever_its_size() {
        let (events, receiver) = event_channel();
        events.send(output(EVENT_BUDGET * 2)).unwrap();
        assert!(matches!(receiver.recv(), Ok(Event::Output { .. })));
        assert_eq!(events.flow.state.lock().unwrap().queued, 0);
    }

    /// Output filling its budget leaves the reader room, so what the peer says is
    /// still taken off the socket, up to the reader's own limit.
    #[test]
    fn the_reader_has_room_above_what_output_can_fill() {
        let (events, receiver) = event_channel();
        events
            .send(output(EVENT_BUDGET - MESSAGE_OVERHEAD))
            .unwrap();
        let packet = |bytes: usize| Event::Packet {
            payload: vec![0; bytes],
            seq: 0,
        };
        events
            .send(packet(EVENT_BUDGET - MESSAGE_OVERHEAD))
            .unwrap();
        assert_eq!(events.flow.state.lock().unwrap().queued, READER_BUDGET);
        let waiter = {
            let events = events.clone();
            thread::spawn(move || events.send(packet(1)).is_ok())
        };
        thread::sleep(Duration::from_millis(30));
        assert!(!waiter.is_finished(), "a packet past the reader's limit");
        receiver.recv().unwrap();
        assert!(waiter.join().unwrap());
    }

    /// A core that went away must not leave a sender waiting on it for ever.
    #[test]
    fn a_receiver_going_away_releases_every_waiter() {
        let (events, receiver) = event_channel();
        events.send(output(EVENT_BUDGET)).unwrap();
        let waiter = {
            let events = events.clone();
            thread::spawn(move || events.send(output(1)).is_err())
        };
        let credit = Credit::new(0, events.flow.clone());
        let taker = {
            let credit = credit.clone();
            thread::spawn(move || credit.take(1))
        };
        thread::sleep(Duration::from_millis(20));
        drop(receiver);
        assert!(waiter.join().unwrap());
        assert_eq!(taker.join().unwrap(), None);
    }

    #[test]
    fn credit_waits_for_the_transport_as_well_as_the_window() {
        let flow = Flow::new();
        let credit = Credit::new(100, flow.clone());
        assert_eq!(credit.take(10), Some(10));
        flow.set_transport_open(false);
        let got = Arc::new(AtomicU64::new(0));
        let waiter = {
            let (credit, got) = (credit.clone(), got.clone());
            thread::spawn(move || {
                let taken = credit.take(10).unwrap();
                got.store(taken as u64, Ordering::SeqCst);
            })
        };
        thread::sleep(Duration::from_millis(30));
        assert_eq!(got.load(Ordering::SeqCst), 0, "took credit during a rekey");
        // More window is no reason to go on.
        credit.add(100);
        thread::sleep(Duration::from_millis(30));
        assert_eq!(got.load(Ordering::SeqCst), 0, "took credit during a rekey");
        flow.set_transport_open(true);
        waiter.join().unwrap();
        assert_eq!(got.load(Ordering::SeqCst), 10);
        // A close releases a waiter however the transport stands.
        flow.set_transport_open(false);
        let waiter = {
            let credit = credit.clone();
            thread::spawn(move || credit.take(1))
        };
        thread::sleep(Duration::from_millis(20));
        credit.close();
        assert_eq!(waiter.join().unwrap(), None);
    }

    /// A real pair of engines: the flag follows the exchange from each end, ours
    /// started by a limit, and the peer's arriving unasked.
    #[test]
    fn the_core_shuts_the_transport_at_kexinit_and_opens_it_after_newkeys() {
        struct Env(crate::kex::RandomSource<std::fs::File>);
        impl KexEnvironment for Env {
            fn random(&mut self, out: &mut [u8]) -> Result<()> {
                self.0.fill(out)
            }
            fn host_key_blob(&mut self) -> Result<Vec<u8>> {
                Ok(crate::kex::ed25519_public_blob(
                    &td_primitives::curve25519::ed25519::SigningKey::from_seed(&[42; 32])
                        .unwrap()
                        .public_key(),
                ))
            }
            fn sign(&mut self, hash: &[u8; 32]) -> Result<Vec<u8>> {
                let key =
                    td_primitives::curve25519::ed25519::SigningKey::from_seed(&[42; 32]).unwrap();
                Ok(crate::kex::ed25519_signature_blob(&key.sign(hash).unwrap()))
            }
            fn check_host_key(&mut self, _blob: &[u8]) -> Result<()> {
                Ok(())
            }
        }
        let env = || {
            Env(crate::kex::RandomSource(
                std::fs::File::open("/dev/urandom").unwrap(),
            ))
        };
        let open = |flow: &Flow| flow.state.lock().unwrap().transport_open;
        let start = |role: Role| {
            let flow = Flow::new();
            let (rx_keys, keys) = sync_channel(1);
            let kex = Kex::resume(role, "SSH-2.0-a", "SSH-2.0-b", [9; 32]);
            let conn = Conn::new(
                PacketWriter::new(Vec::new()),
                rx_keys,
                kex,
                env(),
                flow.clone(),
            );
            (conn, flow, keys)
        };

        // We start it, by the limit.
        let (mut client, flow, _keys) = start(Role::Client);
        assert!(open(&flow));
        client.limit_rekey_bytes(0);
        client.send(vec![msg::IGNORE]).unwrap();
        assert!(client.rekeying());
        assert!(!open(&flow));

        // The peer starts one.
        let (mut server, flow, _server_keys) = start(Role::Server);
        let mut peer = Kex::resume(Role::Client, "SSH-2.0-a", "SSH-2.0-b", [9; 32]);
        let mut peer_env = env();
        let init = peer.start(&mut peer_env).unwrap();
        server.on_packet(init).unwrap();
        assert!(server.rekeying());
        assert!(!open(&flow));

        // Through the whole of it, and open again only at its end.
        let mut wire = PacketReader::new(Cursor::new(std::mem::take(server.writer.get_mut())));
        let reply = wire.read_packet().unwrap();
        let ecdh = peer.on_message(&reply, &mut peer_env).unwrap();
        server.on_packet(ecdh.send[0].clone()).unwrap();
        assert!(!open(&flow));
        let mut wire = PacketReader::new(Cursor::new(std::mem::take(server.writer.get_mut())));
        let reply = wire.read_packet().unwrap();
        let finished = peer.on_message(&reply, &mut peer_env).unwrap();
        assert!(!open(&flow));
        server.on_packet(finished.send[0].clone()).unwrap();
        let newkeys = wire.read_packet().unwrap();
        assert_eq!(newkeys[0], msg::NEWKEYS);
        assert!(!server.rekeying());
        assert!(open(&flow));
        assert_eq!(server.completed_rekeys(), 1);
    }

    /// Stopped while it waits for window nothing will grant: the stream closes,
    /// which is the program's `SIGPIPE`, and the end is reported.
    #[test]
    fn a_stopped_source_closes_its_stream_and_reports_the_end() {
        struct Tracked(Arc<AtomicBool>);
        impl Read for Tracked {
            fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
                out.fill(1);
                Ok(out.len())
            }
        }
        impl Drop for Tracked {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let (events, receiver) = event_channel();
        let credit = Credit::new(0, events.flow().clone());
        let closed = Arc::new(AtomicBool::new(false));
        let stop = spawn_source(
            Tracked(closed.clone()),
            3,
            false,
            1024,
            credit.clone(),
            events,
        );
        thread::sleep(Duration::from_millis(20));
        assert!(!closed.load(Ordering::SeqCst));
        credit.stop_source(&stop);
        assert!(matches!(
            receiver.recv_timeout(PATIENCE),
            Ok(Event::OutputEof { channel: 3, .. })
        ));
        assert!(closed.load(Ordering::SeqCst));
    }

    /// A consumer that went away: reported once, and the window stays open, since
    /// stderr shares it and a server may have more to finish with.
    #[test]
    fn a_reporting_sink_says_so_once_and_goes_on_crediting() {
        struct Gone;
        impl Write for Gone {
            fn write(&mut self, _data: &[u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let (events, receiver) = event_channel();
        let (sink, handle) = spawn_sink(Gone, 5, WhenBroken::Report, events.clone());
        drop(events);
        for _ in 0..3 {
            assert!(sink.send(&[1; 100]));
        }
        drop(sink);
        handle.unwrap().join().unwrap();
        let mut seen = Vec::new();
        while let Ok(event) = receiver.recv_timeout(Duration::from_millis(50)) {
            seen.push(event);
        }
        assert!(matches!(seen[0], Event::SinkBroken { channel: 5 }));
        let credits: Vec<u32> = seen[1..]
            .iter()
            .filter_map(|event| match event {
                Event::Consumed { channel: 5, bytes } => Some(*bytes),
                _ => None,
            })
            .collect();
        assert_eq!(credits.len(), seen.len() - 1, "only credits follow");
        assert_eq!(credits.iter().sum::<u32>(), 300);

        // The other kind keeps the window moving, as a child's stdin needs.
        let (events, receiver) = event_channel();
        let (sink, handle) = spawn_sink(Gone, 5, WhenBroken::Discard, events.clone());
        drop(events);
        assert!(sink.send(&[1; 100]));
        drop(sink);
        handle.unwrap().join().unwrap();
        assert!(matches!(
            receiver.recv_timeout(PATIENCE),
            Ok(Event::Consumed {
                channel: 5,
                bytes: 100
            })
        ));
    }

    /// Sent now or handed back, with nothing left charged: over the budget, past
    /// a full queue, and once the core is gone.
    #[test]
    fn output_sent_now_waits_for_nothing_and_hands_back_what_it_cannot_send() {
        let (events, receiver) = event_channel();
        assert_eq!(events.send_output_now(1, false, b"a".to_vec()), Ok(()));
        // Past the budget behind what is queued.
        let large = vec![0; EVENT_BUDGET];
        assert_eq!(events.send_output_now(1, false, large.clone()), Err(large));
        assert!(matches!(receiver.recv(), Ok(Event::Output { data, .. }) if data == b"a"));
        // A full queue, of what costs nothing.
        for _ in 0..EVENT_QUEUE {
            assert!(events.try_send(Event::Tick));
        }
        assert_eq!(
            events.send_output_now(1, true, b"b".to_vec()),
            Err(b"b".to_vec())
        );
        assert_eq!(events.flow().state.lock().unwrap().queued, 0, "charged");
        drop(receiver);
        assert_eq!(
            events.send_output_now(1, false, b"c".to_vec()),
            Err(b"c".to_vec())
        );
    }

    /// Credit taken now is all of it or none, and never while a rekey holds the
    /// transport or once the channel has closed.
    #[test]
    fn credit_is_taken_now_whole_or_not_at_all() {
        let flow = Flow::new();
        let credit = Credit::new(10, flow.clone());
        assert!(!credit.take_now(11));
        assert!(!credit.take_now(0));
        assert!(credit.take_now(4));
        assert!(credit.take_now(6));
        assert!(!credit.take_now(1), "spent");
        credit.add(5);
        flow.set_transport_open(false);
        assert!(!credit.take_now(1), "held by a rekey");
        flow.set_transport_open(true);
        assert!(credit.take_now(1));
        credit.close();
        assert!(!credit.take_now(1), "closed");
    }

    /// A consumer that has stopped, a window spent on it, and then a flood of empty
    /// and one-byte messages: what waits is the window's bytes, held in one buffer,
    /// and the empty ones add nothing at all. Before, each was a queued message.
    #[test]
    fn a_flood_of_empty_or_tiny_data_holds_no_more_than_its_bytes() {
        struct Blocked(Receiver<()>);
        impl Write for Blocked {
            fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
                let _ = self.0.recv();
                Ok(data.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let (events, receiver) = event_channel();
        let (release, gate) = std::sync::mpsc::channel();
        let (sink, handle) = spawn_sink(Blocked(gate), 5, WhenBroken::Discard, events.clone());
        assert!(sink.send(&[1; 100]));
        // The thread has taken that and is blocked writing it.
        let deadline = std::time::Instant::now() + PATIENCE;
        while !sink.shared.state.lock().unwrap().pending.is_empty() {
            assert!(std::time::Instant::now() < deadline, "the sink never wrote");
            thread::yield_now();
        }
        for _ in 0..1_000_000 {
            assert!(sink.send(&[]));
        }
        let mut window = LocalWindow::default();
        window.charge(100).unwrap();
        for _ in 0..CHANNEL_WINDOW - 100 {
            window.charge(1).unwrap();
            assert!(sink.send(&[2]));
        }
        assert!(window.charge(1).is_err(), "the window is spent");
        let state = sink.shared.state.lock().unwrap();
        assert_eq!(state.pending.len(), (CHANNEL_WINDOW - 100) as usize);
        assert!(state.pending.capacity() <= 2 * CHANNEL_WINDOW as usize);
        drop(state);
        drop(sink);
        drop(events);
        // One write for the first message and one for each packet of the rest.
        for _ in 0..=CHANNEL_WINDOW / CHANNEL_MAX_PACKET {
            release.send(()).unwrap();
        }
        handle.unwrap().join().unwrap();
        let mut credited = 0;
        while let Ok(Event::Consumed { bytes, .. }) =
            receiver.recv_timeout(Duration::from_millis(50))
        {
            credited += bytes;
        }
        assert_eq!(credited, CHANNEL_WINDOW);
    }

    /// A keystroke's write reports no credit of its own, which would wake the core
    /// for each, and a packet's worth does; what is owed comes at the end.
    #[test]
    fn small_writes_are_credited_a_packet_at_a_time() {
        struct Counted(Arc<AtomicU64>);
        impl Write for Counted {
            fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
                self.0.fetch_add(data.len() as u64, Ordering::SeqCst);
                Ok(data.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let written = Arc::new(AtomicU64::new(0));
        let (events, receiver) = event_channel();
        let (sink, handle) = spawn_sink(
            Counted(written.clone()),
            5,
            WhenBroken::Discard,
            events.clone(),
        );
        let wait_for = |total: u64| {
            let deadline = std::time::Instant::now() + PATIENCE;
            while written.load(Ordering::SeqCst) < total {
                assert!(std::time::Instant::now() < deadline, "the sink never wrote");
                thread::yield_now();
            }
        };
        let step = u64::from(CHANNEL_MAX_PACKET);
        for typed in 1..step {
            assert!(sink.send(b"a"));
            wait_for(typed);
        }
        assert!(
            receiver.recv_timeout(Duration::from_millis(50)).is_err(),
            "a keystroke woke the core"
        );
        assert!(sink.send(b"a"));
        assert!(matches!(
            receiver.recv_timeout(PATIENCE),
            Ok(Event::Consumed { channel: 5, bytes }) if bytes == CHANNEL_MAX_PACKET
        ));
        assert!(sink.send(b"abc"));
        wait_for(step + 3);
        drop(sink);
        drop(events);
        handle.unwrap().join().unwrap();
        assert!(matches!(
            receiver.recv_timeout(PATIENCE),
            Ok(Event::Consumed {
                channel: 5,
                bytes: 3
            })
        ));
    }

    /// What waited is credited a packet at a time as it lands, not once all of it
    /// is through, so a slow consumer keeps the window moving.
    #[test]
    fn a_large_wait_is_credited_as_each_packet_lands() {
        struct Gated(Receiver<()>);
        impl Write for Gated {
            fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
                let _ = self.0.recv();
                Ok(data.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let (events, receiver) = event_channel();
        let (release, gate) = std::sync::mpsc::channel();
        let (sink, handle) = spawn_sink(Gated(gate), 5, WhenBroken::Discard, events.clone());
        let packet = CHANNEL_MAX_PACKET as usize;
        assert!(sink.send(&vec![1; 4 * packet]));
        release.send(()).unwrap();
        assert!(matches!(
            receiver.recv_timeout(PATIENCE),
            Ok(Event::Consumed { channel: 5, bytes }) if bytes == CHANNEL_MAX_PACKET
        ));
        // Three packets are still to be written, so nothing more is credited yet.
        assert!(receiver.recv_timeout(Duration::from_millis(100)).is_err());
        drop(sink);
        drop(events);
        for _ in 0..3 {
            release.send(()).unwrap();
        }
        handle.unwrap().join().unwrap();
        let mut credited = CHANNEL_MAX_PACKET;
        while let Ok(Event::Consumed { bytes, .. }) =
            receiver.recv_timeout(Duration::from_millis(50))
        {
            credited += bytes;
        }
        assert_eq!(credited, 4 * CHANNEL_MAX_PACKET);
    }

    /// A sink whose thread could not start, or has ended, says so, so the caller
    /// credits the window rather than wait on a write that will not come.
    #[test]
    fn a_sink_with_no_thread_left_refuses_and_holds_nothing() {
        let (events, receiver) = event_channel();
        drop(receiver);
        let (sink, handle) = spawn_sink(Vec::new(), 5, WhenBroken::Discard, events);
        assert!(sink.send(&vec![1; CHANNEL_MAX_PACKET as usize]));
        // A packet's credit is due and cannot be delivered, so the thread ends.
        handle.unwrap().join().unwrap();
        assert!(!sink.send(&[1; 10]));
        assert!(sink.shared.state.lock().unwrap().pending.is_empty());
    }

    /// A core blocked in a write is told by the socket, and its queue holds the rest.
    #[test]
    fn the_end_of_the_socket_runs_at_end_before_it_is_queued() {
        let (events, receiver) = event_channel();
        let closed = Arc::new(AtomicBool::new(false));
        let told = closed.clone();
        let wire = framed(&[msg::IGNORE]);
        let _keys = spawn_reader(PacketReader::new(Cursor::new(wire)), events, move || {
            // Long enough that a queued end would be seen before this has run.
            thread::sleep(Duration::from_millis(50));
            told.store(true, Ordering::SeqCst);
        });
        assert!(matches!(
            receiver.recv_timeout(PATIENCE),
            Ok(Event::Packet { .. })
        ));
        assert!(matches!(
            receiver.recv_timeout(PATIENCE),
            Ok(Event::TransportEnded(Error::Io(_)))
        ));
        assert!(closed.load(Ordering::SeqCst));
    }

    /// A protocol error is the core's to send a `DISCONNECT` about.
    #[test]
    fn a_protocol_error_leaves_the_socket_for_the_core() {
        let (events, receiver) = event_channel();
        let closed = Arc::new(AtomicBool::new(false));
        let told = closed.clone();
        let wire = [0xffu8; 64].to_vec();
        let _keys = spawn_reader(PacketReader::new(Cursor::new(wire)), events, move || {
            told.store(true, Ordering::SeqCst);
        });
        assert!(matches!(
            receiver.recv_timeout(PATIENCE),
            Ok(Event::TransportEnded(Error::Protocol(_)))
        ));
        assert!(!closed.load(Ordering::SeqCst));
    }

    /// The reader's stamp does not wait behind a queue that is full.
    #[test]
    fn the_reader_stamps_each_packet_before_it_waits_for_room() {
        let mut wire = Vec::new();
        for _ in 0..3 {
            wire.extend(framed(&vec![msg::IGNORE; 60_000]));
        }
        let (events, receiver) = event_channel();
        events.send(output(READER_BUDGET)).unwrap();
        let flow = events.flow().clone();
        let _keys = spawn_reader(PacketReader::new(Cursor::new(wire)), events, || {});
        within(PATIENCE, "the first packet's stamp", || {
            flow.arrivals() == 1
        });
        thread::sleep(Duration::from_millis(30));
        // Held at the budget with one read: nothing is read on behalf of a queue.
        assert_eq!(flow.arrivals(), 1);
        receiver.recv().unwrap();
        within(PATIENCE, "the rest of the stamps", || flow.arrivals() == 3);
    }

    /// At a slow link's rate something has to come back inside the keepalive's bound
    /// (`server::session`), and half of this window was once over a quarter of an hour
    /// at 1 KB/s.
    #[test]
    fn a_grant_comes_at_a_small_share_of_the_window() {
        assert_eq!(WINDOW_GRANT_AT, 256 * 1024);
        assert!(u64::from(WINDOW_GRANT_AT) * 8 <= u64::from(CHANNEL_WINDOW));
    }

    #[test]
    fn a_disconnect_during_the_initial_exchange_reports_its_text_filtered() {
        let mut message = Writer::message(msg::DISCONNECT);
        message.u32(11).str("no \x1b]0;PWNED\x07 today").str("");
        let mut reader = PacketReader::new(Cursor::new(framed(message.as_slice())));
        let mut writer = PacketWriter::new(Vec::new());
        let error = initial_kex(&mut reader, &mut writer, &mut engine(), &mut NoEnv)
            .err()
            .unwrap();
        assert!(
            matches!(&error, Error::Disconnected(text) if text == "no ?]0;PWNED? today"),
            "{error}"
        );
    }

    #[test]
    fn the_session_core_filters_a_disconnect_too() {
        let mut conn = test_conn();
        let mut message = Writer::message(msg::DISCONNECT);
        message.u32(11).str("a\nb\x1b").str("");
        let error = conn.on_packet(message.into_vec()).err().unwrap();
        assert_eq!(error.to_string(), "disconnected by peer: a?b?");
    }

    #[test]
    fn credit_blocks_until_granted_and_close_releases_waiters() {
        let credit = Credit::new(3, Flow::new());
        assert_eq!(credit.take(10), Some(3));
        let waiter = {
            let credit = credit.clone();
            thread::spawn(move || credit.take(10))
        };
        thread::sleep(Duration::from_millis(20));
        credit.add(4);
        assert_eq!(waiter.join().unwrap(), Some(4));
        let waiter = {
            let credit = credit.clone();
            thread::spawn(move || credit.take(1))
        };
        thread::sleep(Duration::from_millis(20));
        credit.close();
        assert_eq!(waiter.join().unwrap(), None);
    }

    #[test]
    fn the_local_window_refuses_an_overrun_and_regrants_at_its_threshold() {
        let mut window = LocalWindow::default();
        assert!(window.charge(CHANNEL_WINDOW as usize + 1).is_err());
        window.charge(CHANNEL_WINDOW as usize).unwrap();
        assert!(window.charge(1).is_err());
        assert_eq!(window.consumed(1000), None);
        assert_eq!(window.consumed(WINDOW_GRANT_AT - 1001), None);
        assert_eq!(window.consumed(1), Some(WINDOW_GRANT_AT));
        assert_eq!(window.consumed(1), None);
        let grant = window.consumed(CHANNEL_WINDOW / 2).unwrap();
        assert_eq!(grant, CHANNEL_WINDOW / 2 + 1);
        assert!(window.charge(grant as usize).is_ok());
    }

    #[test]
    fn a_source_respects_credit_and_reports_eof() {
        let (events, receiver) = event_channel();
        let credit = Credit::new(2, events.flow().clone());
        spawn_source(&b"abcd"[..], 7, false, 1024, credit.clone(), events);
        let mut seen = Vec::new();
        for _ in 0..1 {
            if let Event::Output { data, .. } = receiver.recv().unwrap() {
                seen.extend(data);
            }
        }
        assert_eq!(seen, b"ab");
        credit.add(10);
        if let Event::Output { data, channel, .. } = receiver.recv().unwrap() {
            assert_eq!(channel, 7);
            seen.extend(data);
        }
        assert_eq!(seen, b"abcd");
        assert!(matches!(
            receiver.recv().unwrap(),
            Event::OutputEof { channel: 7, .. }
        ));
    }
}

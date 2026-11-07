//! The authenticated session: the account's own process, holding no host key.
//! It switches credentials before reading anything from the network, then runs
//! channels until the peer goes away (DESIGN.md sections 4 and 6).

use super::credentials::{self, Credentials};
use super::ipc::{self, SessionGrant, TransportState};
use super::log;
use super::sftp;
use crate::conn::{self, Conn, Credit, Event, EventSender, LocalWindow, WhenBroken};
use crate::conn::{CHANNEL_MAX_PACKET, CHANNEL_WINDOW};
use crate::error::{Error, Result};
use crate::kex::{Kex, KexEnvironment, RandomSource, Role};
use crate::msg;
use crate::packet::{PacketReader, PacketWriter, IDENTIFICATION};
use crate::sys;
use crate::wire::{Reader, Writer};
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::File;
use std::net::{Shutdown, TcpStream};
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const PATH: &str = "/bin";
/// What a PTY's reader may still deliver once the child is gone. Something the
/// child left behind can hold the slave open indefinitely.
const PTY_DRAIN: Duration = Duration::from_millis(200);
/// How often a PTY's waiter looks for an exit between waits for a kill request:
/// std's `Child` cannot be blocked on and killed at the same time.
const PTY_WAIT_POLL: Duration = Duration::from_millis(100);
/// Fixed, not configurable (DESIGN.md section 4): a client that vanished is noticed
/// after this many ticks without an answer, and a rekey that never finishes too.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(300);
const KEEPALIVE_COUNT: u32 = 3;
/// The same total: a write this long without progress ends the connection.
const STALL_LIMIT: Duration = Duration::from_secs(300 * 3);
const KEEPALIVE_REQUEST: &str = "keepalive@openssh.com";
/// RFC 4254 section 5.2: outstanding window never passes 2^32 - 1.
const MAX_WINDOW: u64 = u32::MAX as u64;

struct SessionEnv {
    random: RandomSource<File>,
    signer: UnixStream,
    host_blob: Vec<u8>,
}

impl KexEnvironment for SessionEnv {
    fn random(&mut self, out: &mut [u8]) -> Result<()> {
        self.random.fill(out)
    }

    fn host_key_blob(&mut self) -> Result<Vec<u8>> {
        Ok(self.host_blob.clone())
    }

    fn sign(&mut self, hash: &[u8; 32]) -> Result<Vec<u8>> {
        let mut request = Writer::new();
        request.string(hash);
        ipc::write_frame(&mut self.signer, request.as_slice())?;
        let reply = ipc::read_frame(&mut self.signer)?;
        Ok(Reader::new(&reply).string()?.to_vec())
    }

    fn check_host_key(&mut self, _blob: &[u8]) -> Result<()> {
        Err(Error::protocol("the server checks no host key"))
    }
}

pub fn main(single_user: bool) -> i32 {
    match run(single_user) {
        Ok(()) => 0,
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof => 0,
        Err(Error::Disconnected(_)) => 0,
        Err(error) => {
            log::line(format_args!("td-sshd: session: {error}"));
            1
        }
    }
}

/// The monitor's admission never grants root (DESIGN.md section 5). This is the
/// last point before a program runs, and takes no monitor's word for it.
fn refuse_root_grant(uid: u32) -> Result<()> {
    if uid == 0 {
        return Err(Error::refused("a session is never granted root"));
    }
    Ok(())
}

fn run(single_user: bool) -> Result<()> {
    let socket = TcpStream::from(std::io::stdin().as_fd().try_clone_to_owned()?);
    let mut signer = UnixStream::from(std::io::stdout().as_fd().try_clone_to_owned()?);
    let mut status = credentials::open_status()?;

    let frame = ipc::read_frame(&mut signer)?;
    let mut reader = Reader::new(&frame);
    let state = TransportState::decode(&mut reader)?;
    let grant = SessionGrant::decode(&mut reader)?;
    let host_blob = reader.string()?.to_vec();
    reader.finish()?;
    refuse_root_grant(grant.uid)?;

    if !single_user {
        credentials::switch(
            &Credentials::new(grant.uid, grant.gid, &grant.groups),
            &mut status,
        )
        .map_err(Error::refused)?;
    }
    drop(status);
    sys::set_not_dumpable()?;
    // A home that cannot be entered is reported by the shell it would have run.
    let _ = std::env::set_current_dir(&grant.home);

    let random = RandomSource(File::open("/dev/urandom")?);
    socket.set_write_timeout(Some(STALL_LIMIT))?;
    let writer_half = socket.try_clone()?;
    let ending = socket.try_clone()?;
    let reader_half = socket;
    let packet_reader = PacketReader::resume(reader_half, &state.rx_key, state.rx);
    let packet_writer = PacketWriter::resume(writer_half, &state.tx_key, state.tx);
    let kex_engine = Kex::resume(
        Role::Server,
        &state.client_version,
        IDENTIFICATION,
        state.session_id,
    );
    let env = SessionEnv {
        random,
        signer,
        host_blob,
    };
    let (events, receiver) = conn::event_channel();
    let rx_keys = conn::spawn_reader(packet_reader, events.clone(), move || {
        let _ = ending.shutdown(Shutdown::Both);
    });
    let mut conn = Conn::new(
        packet_writer,
        rx_keys,
        kex_engine,
        env,
        events.flow().clone(),
    );
    // In this order: a tick must not find the first probe not yet due.
    let mut table = Channels::new(grant, events.clone(), Instant::now());
    start_ticker(events)?;
    loop {
        let Ok(event) = receiver.recv() else {
            return Ok(());
        };
        table.on_event(event, &mut conn)?;
    }
}

impl Channel {
    /// The child has gone from a PTY whose reader has not seen the end.
    fn is_draining(&self) -> bool {
        self.master.is_some() && !self.child_running && self.stdout_open && !self.closing
    }

    /// `eow@openssh.com`: nothing at the far end reads this channel's output any
    /// more. Closing our read of a pipe gives the program its `SIGPIPE`. A terminal
    /// has none to give, so the session leader goes, as on the peer's close.
    fn peer_stopped_reading(&mut self) {
        self.peer_reads = false;
        if let Some(stop) = &self.stdout_stop {
            self.credit.stop_source(stop);
        }
        if let Some(kill) = &self.kill {
            let _ = kill.send(());
        }
    }
}

/// One session channel's whole state.
struct Channel {
    peer: u32,
    credit: Arc<Credit>,
    window: LocalWindow,
    max_packet: u32,
    /// Where inbound data goes: a child's stdin, a PTY master or the SFTP thread.
    input: Option<Sender<Vec<u8>>>,
    pty: Option<PtyRequest>,
    master: Option<File>,
    started: bool,
    /// How many of stdout, stderr and the child are still running.
    stdout_open: bool,
    stderr_open: bool,
    child_status: Option<Result<ExitStatus>>,
    child_running: bool,
    subsystem_running: bool,
    /// Our `CHANNEL_CLOSE` has been sent: nothing more goes to the peer for this.
    closing: bool,
    /// The peer's `CHANNEL_CLOSE` arrived. A channel in this state exists only to
    /// hold its `MaxSessions` slot until its program has gone.
    peer_closed: bool,
    /// Ends a PTY's session leader, on the peer's close.
    kill: Option<Sender<()>>,
    /// Ends the source reading the child's stdout, or a PTY's master.
    stdout_stop: Option<Arc<AtomicBool>>,
    /// False once the peer said `eow@openssh.com`: no window will come for this.
    peer_reads: bool,
    /// What the peer has granted less what has been sent, as this thread saw them.
    /// The stream threads' `Credit` can only be behind it, so a compliant peer's
    /// own count is never lower.
    peer_window: u64,
    /// When output last moved or more window arrived, for the PTY drain clock.
    last_activity: Instant,
    /// When the PTY drain timer now running is due.
    drain_due: Option<Instant>,
}

#[derive(Clone)]
struct PtyRequest {
    term: String,
    size: sys::Winsize,
}

/// Keepalive state. An answer counts only against a probe that was sent, so a peer
/// cannot keep a rekey it never finishes alive with replies nobody asked for. Ticks
/// and answers wait behind queued output in one FIFO, so `arrivals`, which the
/// reader stamps as packets come off the socket, says what the queue cannot yet:
/// a probe is unanswered only if nothing at all has arrived since it was sent.
#[derive(Debug, Default)]
struct Keepalive {
    ticks: u32,
    unanswered: u32,
    arrivals: u64,
}

impl Keepalive {
    /// One tick, with the reader's count. Whether to send a probe; the error ends
    /// the connection.
    fn tick(&mut self, rekeying: bool, arrivals: u64) -> Result<bool> {
        let arrived = arrivals != self.arrivals;
        self.arrivals = arrivals;
        if arrived && !rekeying {
            self.ticks = 0;
            self.unanswered = 0;
        }
        self.ticks = self.ticks.saturating_add(1);
        if self.ticks >= KEEPALIVE_COUNT {
            return Err(Error::protocol("the client stopped answering keepalives"));
        }
        if rekeying {
            return Ok(false);
        }
        self.unanswered = self.unanswered.saturating_add(1);
        Ok(true)
    }

    fn on_reply(&mut self) {
        if self.unanswered > 0 {
            self.unanswered -= 1;
            self.ticks = 0;
        }
    }
}

struct Channels {
    grant: SessionGrant,
    channels: HashMap<u32, Channel>,
    next_id: u32,
    events: EventSender,
    keepalive: Keepalive,
    next_probe: Instant,
}

impl Channels {
    fn new(grant: SessionGrant, events: EventSender, now: Instant) -> Self {
        Self {
            grant,
            channels: HashMap::new(),
            next_id: 0,
            events,
            keepalive: Keepalive::default(),
            next_probe: now + KEEPALIVE_INTERVAL,
        }
    }

    /// The channel unless we have closed it, after which the peer's in-flight
    /// messages are dropped and nothing goes back: OpenSSH clients treat traffic
    /// on a channel they have freed as fatal.
    fn open_mut(&mut self, id: u32) -> Option<&mut Channel> {
        self.channels.get_mut(&id).filter(|state| !state.closing)
    }

    /// Sends the probe when one is due, and runs every PTY drain clock. The ticker
    /// and the drain timers share one event, so what is due is decided by time.
    fn on_tick<W: std::io::Write>(
        &mut self,
        now: Instant,
        conn: &mut Conn<W, SessionEnv>,
    ) -> Result<()> {
        self.drain_ptys(now, conn)?;
        if now < self.next_probe {
            return Ok(());
        }
        // From now: ticks that waited behind output must not burn a count each.
        self.next_probe = now + KEEPALIVE_INTERVAL;
        if self.keepalive.tick(conn.rekeying(), conn.arrivals())? {
            let mut probe = Writer::message(msg::GLOBAL_REQUEST);
            probe.str(KEEPALIVE_REQUEST).bool(true);
            conn.send(probe.into_vec())?;
        }
        Ok(())
    }

    /// A PTY's reader may never see EOF, since something the child left behind can
    /// hold the slave open. So once the child has gone the output counts as over
    /// after `PTY_DRAIN` of quiet, but no clock runs while the source may be waiting
    /// for credit: a stalled reader's bytes are not lost, and a grant, or the end of
    /// the rekey that holds every source back, restarts it.
    fn drain_ptys<W: std::io::Write>(
        &mut self,
        now: Instant,
        conn: &mut Conn<W, SessionEnv>,
    ) -> Result<()> {
        let draining: Vec<u32> = self
            .channels
            .iter()
            .filter(|(_, state)| state.is_draining())
            .map(|(id, _)| *id)
            .collect();
        for id in draining {
            let events = self.events.clone();
            let Some(state) = self.channels.get_mut(&id) else {
                continue;
            };
            if state.drain_due.is_some_and(|due| due <= now) {
                state.drain_due = None;
            }
            if (state.peer_window == 0 && state.peer_reads) || conn.rekeying() {
                continue;
            }
            let quiet_at = state.last_activity + PTY_DRAIN;
            if now < quiet_at {
                if state.drain_due.is_none() {
                    // Without a timer the channel would wait for a tick a minute off.
                    if arm_drain_timer(quiet_at - now, events).is_ok() {
                        state.drain_due = Some(quiet_at);
                        continue;
                    }
                } else {
                    continue;
                }
            }
            state.stdout_open = false;
            self.maybe_finish(id, conn)?;
        }
        Ok(())
    }

    /// Sources go on again, so the quiet that counts starts now.
    fn after_rekey<W: std::io::Write>(&mut self, conn: &mut Conn<W, SessionEnv>) -> Result<()> {
        let now = Instant::now();
        for state in self
            .channels
            .values_mut()
            .filter(|state| state.is_draining())
        {
            state.last_activity = now;
            state.drain_due = None;
        }
        self.drain_ptys(now, conn)
    }

    fn on_event<W: std::io::Write>(
        &mut self,
        event: Event,
        conn: &mut Conn<W, SessionEnv>,
    ) -> Result<()> {
        match event {
            Event::Packet { payload, seq } => {
                let was_rekeying = conn.rekeying();
                if let Some(message) = conn.on_packet(payload)? {
                    self.on_message(&message, seq, conn)?;
                }
                if was_rekeying && !conn.rekeying() {
                    self.after_rekey(conn)?;
                }
            }
            Event::TransportEnded(error) => return Err(error),
            Event::Output {
                channel,
                stderr,
                data,
            } => {
                if let Some(state) = self.open_mut(channel) {
                    state.peer_window = state.peer_window.saturating_sub(data.len() as u64);
                    state.last_activity = Instant::now();
                    let message = if stderr {
                        let mut message = Writer::message(msg::CHANNEL_EXTENDED_DATA);
                        message
                            .u32(state.peer)
                            .u32(msg::EXTENDED_DATA_STDERR)
                            .string(&data);
                        message
                    } else {
                        let mut message = Writer::message(msg::CHANNEL_DATA);
                        message.u32(state.peer).string(&data);
                        message
                    };
                    conn.send(message.into_vec())?;
                }
            }
            Event::OutputEof { channel, stderr } => {
                if let Some(state) = self.open_mut(channel) {
                    if stderr {
                        state.stderr_open = false;
                    } else {
                        state.stdout_open = false;
                    }
                }
                self.maybe_finish(channel, conn)?;
            }
            Event::Consumed { channel, bytes } => {
                if let Some(state) = self.open_mut(channel) {
                    if let Some(grant) = state.window.consumed(bytes) {
                        let mut message = Writer::message(msg::CHANNEL_WINDOW_ADJUST);
                        message.u32(state.peer).u32(grant);
                        conn.send(message.into_vec())?;
                    }
                }
            }
            Event::ChildExited { channel, status } => {
                let now = Instant::now();
                if let Some(state) = self.channels.get_mut(&channel) {
                    state.child_running = false;
                    state.child_status = Some(status);
                    state.last_activity = now;
                }
                self.drain_ptys(now, conn)?;
                self.after_program(channel, conn)?;
            }
            Event::SubsystemEnded { channel } => {
                if let Some(state) = self.channels.get_mut(&channel) {
                    state.subsystem_running = false;
                    state.stdout_open = false;
                    state.stderr_open = false;
                    state.child_status = Some(Ok(ExitStatus::from_raw(0)));
                }
                self.after_program(channel, conn)?;
            }
            Event::Tick => self.on_tick(Instant::now(), conn)?,
            Event::SinkBroken { .. } | Event::WindowChanged { .. } | Event::EscapeDisconnect => {}
        }
        Ok(())
    }

    /// A program has gone: a channel the peer has already closed gives its slot
    /// back, and any other may have finished.
    fn after_program<W: std::io::Write>(
        &mut self,
        id: u32,
        conn: &mut Conn<W, SessionEnv>,
    ) -> Result<()> {
        if self
            .channels
            .get(&id)
            .is_some_and(|state| state.peer_closed)
        {
            self.channels.remove(&id);
            return Ok(());
        }
        self.maybe_finish(id, conn)
    }

    fn on_message<W: std::io::Write>(
        &mut self,
        payload: &[u8],
        seq: u32,
        conn: &mut Conn<W, SessionEnv>,
    ) -> Result<()> {
        let number = payload.first().copied().unwrap_or(0);
        let mut reader = Reader::new(payload.get(1..).unwrap_or_default());
        match number {
            msg::GLOBAL_REQUEST => {
                let _name = reader.string()?;
                if reader.bool()? {
                    conn.send(vec![msg::REQUEST_FAILURE])?;
                }
            }
            msg::REQUEST_SUCCESS | msg::REQUEST_FAILURE => self.keepalive.on_reply(),
            msg::CHANNEL_OPEN => self.on_open(&mut reader, conn)?,
            msg::CHANNEL_WINDOW_ADJUST => {
                let id = reader.u32()?;
                let grant = reader.u32()?;
                if let Some(state) = self.open_mut(id) {
                    let window = state.peer_window.saturating_add(u64::from(grant));
                    if window > MAX_WINDOW {
                        return Err(Error::protocol("peer granted more than 2^32 - 1"));
                    }
                    state.peer_window = window;
                    state.credit.add(grant);
                    state.last_activity = Instant::now();
                    self.drain_ptys(Instant::now(), conn)?;
                }
            }
            msg::CHANNEL_DATA => {
                let id = reader.u32()?;
                let data = reader.string()?;
                if let Some(state) = self.open_mut(id) {
                    state.window.charge(data.len())?;
                    match &state.input {
                        Some(input) => {
                            let _ = input.send(data.to_vec());
                        }
                        // Nothing will ever read it: grant the window straight back.
                        None => {
                            let bytes = u32::try_from(data.len()).unwrap_or(u32::MAX);
                            if let Some(grant) = state.window.consumed(bytes) {
                                let mut message = Writer::message(msg::CHANNEL_WINDOW_ADJUST);
                                message.u32(state.peer).u32(grant);
                                conn.send(message.into_vec())?;
                            }
                        }
                    }
                }
            }
            msg::CHANNEL_EXTENDED_DATA => {
                let id = reader.u32()?;
                let _kind = reader.u32()?;
                let data = reader.string()?;
                if let Some(state) = self.open_mut(id) {
                    state.window.charge(data.len())?;
                    let bytes = u32::try_from(data.len()).unwrap_or(u32::MAX);
                    if let Some(grant) = state.window.consumed(bytes) {
                        let mut message = Writer::message(msg::CHANNEL_WINDOW_ADJUST);
                        message.u32(state.peer).u32(grant);
                        conn.send(message.into_vec())?;
                    }
                }
            }
            msg::CHANNEL_EOF => {
                let id = reader.u32()?;
                if let Some(state) = self.open_mut(id) {
                    // Dropping the sender ends the sink, which closes the child's
                    // stdin. A PTY has no such edge: its slave stays open.
                    state.input = None;
                }
            }
            msg::CHANNEL_REQUEST => self.on_request(&mut reader, conn)?,
            msg::CHANNEL_CLOSE => self.on_close(reader.u32()?, conn)?,
            msg::CHANNEL_SUCCESS | msg::CHANNEL_FAILURE => {}
            _ => {
                let mut reply = Writer::message(msg::UNIMPLEMENTED);
                reply.u32(seq);
                conn.send(reply.into_vec())?;
            }
        }
        Ok(())
    }

    /// The peer's close. The slot stays taken until the program has gone, so
    /// closing and reopening cannot run more of them at once than `MaxSessions`.
    /// Pipes close here, and that is all a program without a terminal gets: it owns
    /// its exit. On a PTY the source thread holds a master open in its blocked read,
    /// so nothing would ever hang the terminal up. Killing the session leader does:
    /// the kernel then sends the foreground group `SIGHUP`.
    fn on_close<W: std::io::Write>(
        &mut self,
        id: u32,
        conn: &mut Conn<W, SessionEnv>,
    ) -> Result<()> {
        let Some(state) = self.channels.get_mut(&id) else {
            return Ok(());
        };
        if state.peer_closed {
            return Ok(());
        }
        state.peer_closed = true;
        state.credit.close();
        state.input = None;
        if let Some(kill) = &state.kill {
            let _ = kill.send(());
        }
        let peer = state.peer;
        let close_owed = !state.closing;
        state.closing = true;
        if !state.child_running && !state.subsystem_running {
            self.channels.remove(&id);
        }
        if close_owed {
            let mut close = Writer::message(msg::CHANNEL_CLOSE);
            close.u32(peer);
            conn.send(close.into_vec())?;
        }
        Ok(())
    }

    fn on_open<W: std::io::Write>(
        &mut self,
        reader: &mut Reader<'_>,
        conn: &mut Conn<W, SessionEnv>,
    ) -> Result<()> {
        let kind = reader.str()?;
        let peer = reader.u32()?;
        let window = reader.u32()?;
        let max_packet = reader.u32()?;
        let refusal = if kind != "session" {
            Some((
                msg::OPEN_UNKNOWN_CHANNEL_TYPE,
                "td-sshd only opens session channels",
            ))
        } else if self.channels.len() as u64 >= u64::from(self.grant.max_sessions) {
            Some((msg::OPEN_RESOURCE_SHORTAGE, "too many sessions"))
        } else {
            None
        };
        if let Some((code, text)) = refusal {
            let mut failure = Writer::message(msg::CHANNEL_OPEN_FAILURE);
            failure.u32(peer).u32(code).str(text).str("");
            return conn.send(failure.into_vec());
        }
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.channels.insert(
            id,
            Channel {
                peer,
                credit: conn.new_credit(window),
                window: LocalWindow::default(),
                max_packet: max_packet.min(CHANNEL_MAX_PACKET),
                input: None,
                pty: None,
                master: None,
                started: false,
                stdout_open: false,
                stderr_open: false,
                child_status: None,
                child_running: false,
                subsystem_running: false,
                closing: false,
                peer_closed: false,
                kill: None,
                stdout_stop: None,
                peer_reads: true,
                peer_window: u64::from(window),
                last_activity: Instant::now(),
                drain_due: None,
            },
        );
        let mut confirmation = Writer::message(msg::CHANNEL_OPEN_CONFIRMATION);
        confirmation
            .u32(peer)
            .u32(id)
            .u32(CHANNEL_WINDOW)
            .u32(CHANNEL_MAX_PACKET);
        conn.send(confirmation.into_vec())
    }

    fn on_request<W: std::io::Write>(
        &mut self,
        reader: &mut Reader<'_>,
        conn: &mut Conn<W, SessionEnv>,
    ) -> Result<()> {
        let id = reader.u32()?;
        let request = reader.str()?;
        let want_reply = reader.bool()?;
        let Some(peer) = self.open_mut(id).map(|state| state.peer) else {
            return Ok(());
        };
        let accepted = match request {
            "pty-req" => self.on_pty_request(id, reader)?,
            "window-change" => {
                let size = read_size(reader)?;
                if let Some(state) = self.channels.get(&id) {
                    if let Some(master) = &state.master {
                        let _ = sys::set_window_size(master, &size);
                    }
                }
                // RFC 4254 6.7: no reply is ever sent for this one.
                return Ok(());
            }
            "eow@openssh.com" => {
                if let Some(state) = self.channels.get_mut(&id) {
                    state.peer_stopped_reading();
                }
                true
            }
            "shell" => self.start(id, None, None),
            "exec" => {
                // Bytes, as a file name in a Git path may well be.
                let command = OsString::from_vec(reader.string()?.to_vec());
                self.start(id, Some(command), None)
            }
            "subsystem" => {
                let name = reader.str()?.to_string();
                self.start(id, None, Some(name))
            }
            // env, signal, x11-req, auth-agent-req and anything else.
            _ => false,
        };
        if want_reply {
            let number = if accepted {
                msg::CHANNEL_SUCCESS
            } else {
                msg::CHANNEL_FAILURE
            };
            let mut reply = Writer::message(number);
            reply.u32(peer);
            conn.send(reply.into_vec())?;
        }
        Ok(())
    }

    fn on_pty_request(&mut self, id: u32, reader: &mut Reader<'_>) -> Result<bool> {
        let term = reader.str()?.to_string();
        let size = read_size(reader)?;
        // Encoded modes are read and ignored: a new terminal keeps its defaults.
        let _modes = reader.string()?;
        let permit = self.grant.permit_tty;
        let Some(state) = self.channels.get_mut(&id) else {
            return Ok(false);
        };
        if !permit || state.started || state.pty.is_some() {
            return Ok(false);
        }
        state.pty = Some(PtyRequest { term, size });
        Ok(true)
    }

    /// Starts the channel's one program, or SFTP. A forced command wins over
    /// everything the client asked for, including the subsystem.
    fn start(&mut self, id: u32, command: Option<OsString>, subsystem: Option<String>) -> bool {
        match self.channels.get(&id) {
            Some(state) if !state.started => {}
            _ => return false,
        }
        let forced = self.grant.force_command.clone();
        if forced.is_none() {
            if let Some(name) = &subsystem {
                if name != "sftp" || !self.grant.sftp {
                    return false;
                }
                return self.start_sftp(id);
            }
        }
        let original = command.or_else(|| subsystem.map(OsString::from));
        let (program_command, original_env) = match forced {
            Some(forced) => (Some(OsString::from(forced)), original),
            None => (original, None),
        };
        match self.spawn_child(id, program_command, original_env) {
            Ok(()) => true,
            Err(error) => {
                log::line(format_args!("td-sshd: {}: {error}", self.grant.user));
                false
            }
        }
    }

    fn start_sftp(&mut self, id: u32) -> bool {
        let events = self.events.clone();
        let Some(state) = self.channels.get_mut(&id) else {
            return false;
        };
        let (input, output) = (
            mpsc::channel::<Vec<u8>>(),
            sftp::ChannelWriter::new(
                id,
                state.max_packet as usize,
                state.credit.clone(),
                events.clone(),
            ),
        );
        let home = self.grant.home.clone();
        let reader = sftp::ChannelReader::new(id, input.1, events.clone());
        let started = thread::Builder::new().spawn(move || {
            sftp::serve(reader, output, &home);
            let _ = events.send(Event::SubsystemEnded { channel: id });
        });
        if let Err(error) = started {
            log::line(format_args!("td-sshd: {}: {error}", self.grant.user));
            return false;
        }
        state.started = true;
        state.subsystem_running = true;
        state.input = Some(input.0);
        true
    }

    fn spawn_child(
        &mut self,
        id: u32,
        command: Option<OsString>,
        original: Option<OsString>,
    ) -> std::io::Result<()> {
        let grant = &self.grant;
        let state = self
            .channels
            .get_mut(&id)
            .ok_or_else(|| std::io::Error::other("no such channel"))?;
        let shell_name = std::path::Path::new(&grant.shell)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("sh");
        let mut process = Command::new(&grant.shell);
        match &command {
            Some(text) => {
                process.arg0(shell_name).arg("-c").arg(text);
            }
            None => {
                process.arg0(format!("-{shell_name}"));
            }
        }
        process
            .env_clear()
            .env("HOME", &grant.home)
            .env("USER", &grant.user)
            .env("LOGNAME", &grant.user)
            .env("SHELL", &grant.shell)
            .env("PATH", PATH)
            .env(
                "SSH_CONNECTION",
                format!(
                    "{} {} {} {}",
                    grant.peer_address, grant.peer_port, grant.local_address, grant.local_port
                ),
            )
            .env(
                "SSH_CLIENT",
                format!(
                    "{} {} {}",
                    grant.peer_address, grant.peer_port, grant.local_port
                ),
            );
        if std::path::Path::new(&grant.home).is_dir() {
            process.current_dir(&grant.home);
        } else {
            process.current_dir("/");
        }
        if let Some(original) = &original {
            process.env("SSH_ORIGINAL_COMMAND", original);
        }
        let mut pty_master = None;
        if let Some(pty) = &state.pty {
            let (master, slave_path) = sys::open_pty()?;
            sys::set_window_size(&master, &pty.size)?;
            let slave = sys::open_pty_slave(&slave_path)?;
            process
                .env("TERM", &pty.term)
                .env("SSH_TTY", &slave_path)
                .stdin(Stdio::from(slave.try_clone()?))
                .stdout(Stdio::from(slave.try_clone()?))
                .stderr(Stdio::from(slave));
            sys::lead_session(&mut process, true);
            pty_master = Some(master);
        } else {
            process
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            sys::lead_session(&mut process, false);
        }
        // The waiter exists first, so failing to start one leaves no child behind.
        let waiter = Waiter::start(id, pty_master.is_some(), self.events.clone())?;
        let mut child = process.spawn()?;
        // A copy of the slave here would keep the master from ever reading EOF.
        drop(process);
        let max_packet = state.max_packet as usize;
        if let Some(master) = pty_master {
            let writer = master.try_clone()?;
            let (sender, _handle) =
                conn::spawn_sink(writer, id, WhenBroken::Discard, self.events.clone());
            state.input = Some(sender);
            state.stdout_stop = Some(conn::spawn_source(
                master.try_clone()?,
                id,
                false,
                max_packet,
                state.credit.clone(),
                self.events.clone(),
            ));
            state.master = Some(master);
            state.stdout_open = true;
            state.stderr_open = false;
            state.kill = Some(waiter.kill.clone());
        } else {
            if let Some(stdin) = child.stdin.take() {
                let (sender, _handle) =
                    conn::spawn_sink(stdin, id, WhenBroken::Discard, self.events.clone());
                state.input = Some(sender);
            }
            if let Some(stdout) = child.stdout.take() {
                state.stdout_open = true;
                state.stdout_stop = Some(conn::spawn_source(
                    stdout,
                    id,
                    false,
                    max_packet,
                    state.credit.clone(),
                    self.events.clone(),
                ));
            }
            if let Some(stderr) = child.stderr.take() {
                state.stderr_open = true;
                conn::spawn_source(
                    stderr,
                    id,
                    true,
                    max_packet,
                    state.credit.clone(),
                    self.events.clone(),
                );
            }
        }
        state.started = true;
        state.child_running = true;
        // Its receiver lives until the program has gone.
        let _ = waiter.child.send(child);
        Ok(())
    }

    /// All output delivered and the program gone: EOF, status, close, in order.
    fn maybe_finish<W: std::io::Write>(
        &mut self,
        id: u32,
        conn: &mut Conn<W, SessionEnv>,
    ) -> Result<()> {
        let Some(state) = self.channels.get_mut(&id) else {
            return Ok(());
        };
        if state.closing
            || state.child_running
            || state.subsystem_running
            || state.stdout_open
            || state.stderr_open
        {
            return Ok(());
        }
        let Some(status) = state.child_status.take() else {
            return Ok(());
        };
        state.closing = true;
        // Whatever a source thread still holds has nowhere to go now.
        state.credit.close();
        let peer = state.peer;
        let mut eof = Writer::message(msg::CHANNEL_EOF);
        eof.u32(peer);
        conn.send(eof.into_vec())?;
        match status {
            Ok(status) => match status.code() {
                Some(code) => {
                    let mut request = channel_status(peer, "exit-status");
                    request.u32(code.unsigned_abs());
                    conn.send(request.into_vec())?;
                }
                None => {
                    let mut request = channel_status(peer, "exit-signal");
                    request
                        .str(signal_name(status.signal().unwrap_or(0)))
                        .bool(status.core_dumped())
                        .str("")
                        .str("");
                    conn.send(request.into_vec())?;
                }
            },
            Err(error) => {
                log::line(format_args!("td-sshd: waiting for a child: {error}"));
                let mut request = channel_status(peer, "exit-status");
                request.u32(255);
                conn.send(request.into_vec())?;
            }
        }
        let mut close = Writer::message(msg::CHANNEL_CLOSE);
        close.u32(peer);
        conn.send(close.into_vec())
    }
}

fn channel_status(peer: u32, name: &str) -> Writer {
    let mut request = Writer::message(msg::CHANNEL_REQUEST);
    request.u32(peer).str(name).bool(false);
    request
}

fn read_size(reader: &mut Reader<'_>) -> Result<sys::Winsize> {
    let narrow = |value: u32| u16::try_from(value).unwrap_or(u16::MAX);
    let columns = narrow(reader.u32()?);
    let rows = narrow(reader.u32()?);
    let width = narrow(reader.u32()?);
    let height = narrow(reader.u32()?);
    Ok(sys::Winsize {
        rows,
        columns,
        width,
        height,
    })
}

/// RFC 4254 section 6.10's names, without the `SIG` prefix.
fn signal_name(signal: i32) -> &'static str {
    match signal {
        1 => "HUP",
        2 => "INT",
        3 => "QUIT",
        4 => "ILL",
        6 => "ABRT",
        8 => "FPE",
        9 => "KILL",
        11 => "SEGV",
        13 => "PIPE",
        14 => "ALRM",
        15 => "TERM",
        10 => "USR1",
        12 => "USR2",
        _ => "UNKNOWN",
    }
}

/// The thread that reaps one child, and how to reach it.
struct Waiter {
    child: Sender<Child>,
    kill: Sender<()>,
}

impl Waiter {
    /// Only a PTY's child can ever be asked to die, so only it pays for polling.
    fn start(channel: u32, on_pty: bool, events: EventSender) -> std::io::Result<Self> {
        let (child, child_receiver) = mpsc::channel::<Child>();
        let (kill, kill_receiver) = mpsc::channel::<()>();
        thread::Builder::new().spawn(move || {
            let Ok(mut child) = child_receiver.recv() else {
                return;
            };
            let status = if on_pty {
                wait_or_kill(&mut child, &kill_receiver)
            } else {
                child.wait()
            };
            let status = status.map_err(Error::from);
            let _ = events.send(Event::ChildExited { channel, status });
        })?;
        Ok(Self { child, kill })
    }
}

fn wait_or_kill(child: &mut Child, kill: &Receiver<()>) -> std::io::Result<ExitStatus> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        match kill.recv_timeout(PTY_WAIT_POLL) {
            Ok(()) => {
                let _ = child.kill();
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return child.wait(),
        }
    }
}

fn start_ticker(events: EventSender) -> std::io::Result<()> {
    thread::Builder::new().spawn(move || loop {
        thread::sleep(KEEPALIVE_INTERVAL);
        if events.send(Event::Tick).is_err() {
            return;
        }
    })?;
    Ok(())
}

fn arm_drain_timer(after: Duration, events: EventSender) -> std::io::Result<()> {
    thread::Builder::new().spawn(move || {
        thread::sleep(after);
        let _ = events.send(Event::Tick);
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;
    use td_fs::TempDir;

    #[test]
    fn signals_have_their_rfc_names() {
        assert_eq!(signal_name(15), "TERM");
        assert_eq!(signal_name(9), "KILL");
        assert_eq!(signal_name(99), "UNKNOWN");
    }

    #[test]
    fn oversized_terminal_sizes_saturate() {
        let mut writer = Writer::new();
        writer.u32(u32::MAX).u32(24).u32(0).u32(0);
        let bytes = writer.into_vec();
        let mut reader = Reader::new(&bytes);
        let size = read_size(&mut reader);
        assert!(matches!(
            size,
            Ok(sys::Winsize {
                columns: u16::MAX,
                rows: 24,
                ..
            })
        ));
    }

    /// What the connection wrote, shared with the test reading it.
    #[derive(Clone, Default)]
    struct Wire(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Wire {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(data);
            Ok(data.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    const PEER: u32 = 77;
    const LONG: Duration = Duration::from_secs(20);

    /// One session's channel table with a real `Conn`, unencrypted so the test can
    /// read each packet, and a real channel of events.
    struct Harness {
        table: Channels,
        conn: Conn<Wire, SessionEnv>,
        receiver: conn::EventReceiver,
        wire: Wire,
        read: usize,
        start: Instant,
        _keys: Receiver<[u8; 64]>,
        dir: TempDir,
    }

    fn harness(max_sessions: u32) -> Harness {
        let dir = TempDir::new("td-ssh-session").unwrap();
        let home = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
        let wire = Wire::default();
        let (rx_keys, keys) = std::sync::mpsc::sync_channel(1);
        let (events, receiver) = conn::event_channel();
        let conn = Conn::new(
            PacketWriter::new(wire.clone()),
            rx_keys,
            Kex::resume(Role::Server, "SSH-2.0-x", IDENTIFICATION, [0; 32]),
            SessionEnv {
                random: RandomSource(File::open("/dev/urandom").unwrap()),
                signer: UnixStream::pair().unwrap().0,
                host_blob: Vec::new(),
            },
            events.flow().clone(),
        );
        let start = Instant::now();
        let grant = SessionGrant {
            user: "tester".into(),
            uid: 1000,
            gid: 1000,
            groups: Vec::new(),
            home: home.to_str().unwrap().into(),
            shell: "/bin/sh".into(),
            force_command: None,
            permit_tty: true,
            max_sessions,
            sftp: true,
            peer_address: "127.0.0.1".into(),
            peer_port: 1,
            local_address: "127.0.0.1".into(),
            local_port: 22,
        };
        Harness {
            table: Channels::new(grant, events, start),
            conn,
            receiver,
            wire,
            read: 0,
            start,
            _keys: keys,
            dir,
        }
    }

    impl Harness {
        fn message(&mut self, payload: Writer) {
            self.table
                .on_event(
                    Event::Packet {
                        payload: payload.into_vec(),
                        seq: 0,
                    },
                    &mut self.conn,
                )
                .unwrap();
        }

        /// The packets written since the last call.
        fn written(&mut self) -> Vec<Vec<u8>> {
            let bytes = self.wire.0.lock().unwrap().clone();
            let mut reader =
                PacketReader::new(std::io::Cursor::new(bytes.get(self.read..).unwrap()));
            let mut packets = Vec::new();
            while let Ok(packet) = reader.read_packet() {
                packets.push(packet);
            }
            self.read = bytes.len();
            packets
        }

        /// Opens a channel with a window of `window`; the first has id 0.
        fn open(&mut self, window: u32) -> u8 {
            let mut request = Writer::message(msg::CHANNEL_OPEN);
            request
                .str("session")
                .u32(PEER)
                .u32(window)
                .u32(CHANNEL_MAX_PACKET);
            self.message(request);
            self.written()[0][0]
        }

        fn close(&mut self, id: u32) {
            let mut request = Writer::message(msg::CHANNEL_CLOSE);
            request.u32(id);
            self.message(request);
        }

        fn exec(&mut self, id: u32, command: &str) {
            let mut request = Writer::message(msg::CHANNEL_REQUEST);
            request.u32(id).str("exec").bool(true).str(command);
            self.message(request);
            assert_eq!(self.written()[0][0], msg::CHANNEL_SUCCESS);
        }

        fn request_pty(&mut self, id: u32) {
            let mut request = Writer::message(msg::CHANNEL_REQUEST);
            request.u32(id).str("pty-req").bool(false).str("vt100");
            request.u32(80).u32(24).u32(0).u32(0).string(&[]);
            self.message(request);
        }

        /// Feeds events until `done` says so.
        fn pump(&mut self, done: impl Fn(&Channels) -> bool) {
            let deadline = Instant::now() + LONG;
            while !done(&self.table) {
                assert!(Instant::now() < deadline, "the condition never came");
                match self.receiver.recv_timeout(Duration::from_millis(50)) {
                    Ok(event) => self.table.on_event(event, &mut self.conn).unwrap(),
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => panic!("the event channel ended"),
                }
            }
        }

        fn tick(&mut self, number: u32) -> Result<()> {
            let now = self.start + KEEPALIVE_INTERVAL * number + Duration::from_millis(1);
            self.table.on_tick(now, &mut self.conn)
        }

        fn output(&mut self, id: u32, data: &[u8]) {
            let event = Event::Output {
                channel: id,
                stderr: false,
                data: data.to_vec(),
            };
            self.table.on_event(event, &mut self.conn).unwrap();
        }

        /// A channel whose PTY child has gone, without either being real: `master`
        /// is only what marks the channel as a PTY's.
        fn pty_child_gone(&mut self, id: u32) {
            let state = self.table.channels.get_mut(&id).unwrap();
            state.master = Some(File::open("/dev/null").unwrap());
            state.started = true;
            state.stdout_open = true;
            state.child_running = true;
            let event = Event::ChildExited {
                channel: id,
                status: Ok(ExitStatus::from_raw(0)),
            };
            self.table.on_event(event, &mut self.conn).unwrap();
        }

        fn age_activity(&mut self, id: u32) -> Instant {
            let state = self.table.channels.get_mut(&id).unwrap();
            state.last_activity = Instant::now();
            state.drain_due = None;
            state.last_activity + PTY_DRAIN + Duration::from_millis(1)
        }
    }

    /// What a client's reply to a probe looks like.
    fn reply() -> Writer {
        Writer::message(msg::REQUEST_FAILURE)
    }

    #[test]
    fn the_stall_limit_is_the_keepalive_total() {
        assert_eq!(KEEPALIVE_INTERVAL, Duration::from_secs(300));
        assert_eq!(KEEPALIVE_COUNT, 3);
        assert_eq!(STALL_LIMIT, KEEPALIVE_INTERVAL * KEEPALIVE_COUNT);
    }

    #[test]
    fn each_tick_sends_a_probe_and_the_third_unanswered_ends_the_connection() {
        let mut harness = harness(4);
        harness.tick(1).unwrap();
        harness.tick(2).unwrap();
        let probes = harness.written();
        assert_eq!(probes.len(), 2);
        for probe in &probes {
            let mut reader = Reader::new(&probe[1..]);
            assert_eq!(probe[0], msg::GLOBAL_REQUEST);
            assert_eq!(reader.str().unwrap(), "keepalive@openssh.com");
            assert!(reader.bool().unwrap());
        }
        let error = harness.tick(3).unwrap_err().to_string();
        assert!(error.contains("keepalive"), "{error}");
    }

    #[test]
    fn any_reply_to_a_probe_resets_the_count() {
        let mut harness = harness(4);
        harness.tick(1).unwrap();
        harness.tick(2).unwrap();
        harness.message(reply());
        harness.tick(3).unwrap();
        harness.message(Writer::message(msg::REQUEST_SUCCESS));
        harness.tick(4).unwrap();
        harness.tick(5).unwrap();
        assert!(harness.tick(6).is_err());
    }

    /// The reader stamps what the queue is still holding behind output: a probe
    /// the client answered, or anything else it sent, keeps the connection.
    #[test]
    fn whatever_arrived_since_a_probe_keeps_it_from_counting_as_unanswered() {
        let mut harness = harness(4);
        let flow = harness.table.events.flow().clone();
        for tick in 1..12 {
            harness.tick(tick).unwrap();
            flow.stamp_arrival();
        }
        // Each tick still sent its probe.
        assert_eq!(harness.written().len(), 11);
        // Then nothing more arrives: the first tick sees the last stamp, and the
        // connection ends on the same third tick as ever.
        harness.tick(12).unwrap();
        harness.tick(13).unwrap();
        assert!(harness.tick(14).is_err());
    }

    /// Nothing the peer sends buys a stuck rekey time: it never finishes it.
    #[test]
    fn what_arrives_during_a_rekey_does_not_reset_the_count() {
        let mut harness = harness(4);
        let flow = harness.table.events.flow().clone();
        harness.conn.limit_rekey_bytes(0);
        harness.conn.send(vec![msg::IGNORE]).unwrap();
        assert!(harness.conn.rekeying());
        harness.tick(1).unwrap();
        flow.stamp_arrival();
        harness.tick(2).unwrap();
        flow.stamp_arrival();
        assert!(harness.tick(3).is_err());
    }

    /// Ticks wait behind queued output, so several can come at once. They are one
    /// interval, not three, and the connection is not ended by them.
    #[test]
    fn a_backlog_of_late_ticks_counts_once() {
        let mut harness = harness(4);
        let late = harness.start + KEEPALIVE_INTERVAL * 5;
        for _ in 0..5 {
            harness.table.on_tick(late, &mut harness.conn).unwrap();
        }
        assert_eq!(harness.written().len(), 1);
    }

    #[test]
    fn a_session_refuses_a_grant_of_root() {
        assert!(refuse_root_grant(0).is_err());
        refuse_root_grant(1000).unwrap();
    }

    /// A rekey holds the source back with bytes in its hand, like an empty window,
    /// so the quiet this clock looks for means nothing until the rekey is over.
    #[test]
    fn the_drain_clock_does_not_run_during_a_rekey() {
        let mut harness = harness(4);
        harness.open(1000);
        harness.pty_child_gone(0);
        harness.written();
        harness.conn.limit_rekey_bytes(0);
        harness.conn.send(vec![msg::IGNORE]).unwrap();
        assert!(harness.conn.rekeying());
        let due = Instant::now() + PTY_DRAIN * 5;
        harness.table.on_tick(due, &mut harness.conn).unwrap();
        assert!(harness.table.channels[&0].stdout_open);
    }

    /// The peer reads no more of this channel: a program writing to it hears, and
    /// no window ever coming cannot hold the channel open once it has gone.
    #[test]
    fn eow_closes_the_read_of_a_childs_stdout_with_the_window_shut() {
        let mut harness = harness(4);
        harness.open(1 << 16);
        harness.exec(
            0,
            "while :; do echo xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx; done",
        );
        let mut request = Writer::message(msg::CHANNEL_REQUEST);
        request.u32(0).str("eow@openssh.com").bool(false);
        harness.message(request);
        // The pump's own deadline is the failure: no window comes, so only the program
        // hearing of it and going can end the channel.
        harness.pump(|table| {
            table
                .channels
                .get(&0)
                .is_none_or(|state| state.closing && !state.child_running)
        });
    }

    #[test]
    fn eow_ends_a_program_on_a_pty_which_has_no_sigpipe_to_get() {
        let mut harness = harness(4);
        harness.open(16);
        harness.request_pty(0);
        harness.exec(
            0,
            "while :; do echo xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx; done",
        );
        let mut request = Writer::message(msg::CHANNEL_REQUEST);
        request.u32(0).str("eow@openssh.com").bool(false);
        harness.message(request);
        harness.pump(|table| table.channels.get(&0).is_some_and(|state| state.closing));
    }

    #[test]
    fn a_tick_that_is_not_yet_due_sends_nothing() {
        let mut harness = harness(4);
        let early = harness.start + KEEPALIVE_INTERVAL - Duration::from_secs(1);
        harness.table.on_tick(early, &mut harness.conn).unwrap();
        assert!(harness.written().is_empty());
    }

    /// A rekey that never finishes: nothing may go out, the ticks still count, and
    /// no reply nobody asked for can hold the connection open.
    #[test]
    fn a_rekey_in_flight_counts_ticks_without_sending() {
        let mut harness = harness(4);
        harness.conn.limit_rekey_bytes(0);
        harness.conn.send(vec![msg::IGNORE]).unwrap();
        assert!(harness.conn.rekeying());
        let sent = harness.written();
        harness.tick(1).unwrap();
        harness.message(reply());
        harness.tick(2).unwrap();
        harness.message(reply());
        assert!(harness.written().is_empty(), "{sent:?}");
        assert!(harness.tick(3).is_err());
    }

    #[test]
    fn a_grant_past_the_window_limit_ends_the_connection() {
        let mut harness = harness(4);
        harness.open(1);
        let grant = |amount: u32| {
            let mut request = Writer::message(msg::CHANNEL_WINDOW_ADJUST);
            request.u32(0).u32(amount);
            Event::Packet {
                payload: request.into_vec(),
                seq: 0,
            }
        };
        harness
            .table
            .on_event(grant(u32::MAX - 1), &mut harness.conn)
            .unwrap();
        assert!(harness.table.on_event(grant(1), &mut harness.conn).is_err());
        // Spending it makes room again.
        harness.output(0, b"x");
        harness.table.on_event(grant(1), &mut harness.conn).unwrap();

        let mut harness = self::harness(4);
        harness.open(u32::MAX);
        assert!(harness.table.on_event(grant(1), &mut harness.conn).is_err());
    }

    #[test]
    fn nothing_follows_our_close_for_that_channel() {
        let mut harness = harness(4);
        harness.open(1000);
        harness.pty_child_gone(0);
        let due = harness.age_activity(0);
        harness.table.on_tick(due, &mut harness.conn).unwrap();
        let finish: Vec<u8> = harness.written().iter().map(|packet| packet[0]).collect();
        assert_eq!(
            finish,
            [msg::CHANNEL_EOF, msg::CHANNEL_REQUEST, msg::CHANNEL_CLOSE]
        );
        harness.output(0, b"from something the child left behind");
        let eof = Event::OutputEof {
            channel: 0,
            stderr: false,
        };
        harness.table.on_event(eof, &mut harness.conn).unwrap();
        let mut request = Writer::message(msg::CHANNEL_DATA);
        request.u32(0).string(b"in flight");
        harness.message(request);
        let mut request = Writer::message(msg::CHANNEL_REQUEST);
        request.u32(0).str("env").bool(true).str("A").str("b");
        harness.message(request);
        assert!(harness.written().is_empty());
    }

    #[test]
    fn the_drain_clock_does_not_run_while_the_source_waits_for_credit() {
        let mut harness = harness(4);
        harness.open(5);
        harness.output(0, b"12345");
        harness.written();
        harness.pty_child_gone(0);
        // Well past the drain with no credit left: the source holds bytes it cannot
        // send, so this must not end the channel.
        let due = Instant::now() + PTY_DRAIN * 5;
        harness.table.on_tick(due, &mut harness.conn).unwrap();
        assert!(harness.written().is_empty());
        assert!(harness.table.channels[&0].stdout_open);

        // A grant restarts it, and the quiet that follows does end it.
        let mut request = Writer::message(msg::CHANNEL_WINDOW_ADJUST);
        request.u32(0).u32(10);
        harness.message(request);
        assert!(harness.table.channels[&0].stdout_open);
        assert!(harness.table.channels[&0].drain_due.is_some());
        let later = Instant::now() + PTY_DRAIN * 5;
        harness.table.on_tick(later, &mut harness.conn).unwrap();
        let finish: Vec<u8> = harness.written().iter().map(|packet| packet[0]).collect();
        assert_eq!(finish.last(), Some(&msg::CHANNEL_CLOSE));
    }

    #[test]
    fn output_pushes_the_drain_out() {
        let mut harness = harness(4);
        harness.open(1000);
        harness.pty_child_gone(0);
        harness.written();
        let due = harness.age_activity(0);
        // Output processed now makes `due` no longer quiet enough.
        std::thread::sleep(Duration::from_millis(5));
        harness.output(0, b"late");
        harness.written();
        harness.table.on_tick(due, &mut harness.conn).unwrap();
        assert!(harness.written().is_empty());
        assert!(harness.table.channels[&0].stdout_open);
    }

    #[test]
    fn a_closed_channel_holds_its_slot_until_its_program_has_gone() {
        let mut harness = harness(1);
        assert_eq!(harness.open(1 << 20), msg::CHANNEL_OPEN_CONFIRMATION);
        let stop = harness.dir.path().join("stop");
        harness.exec(
            0,
            &format!("while ! test -e {}; do :; done", stop.display()),
        );
        harness.close(0);
        assert_eq!(harness.written()[0][0], msg::CHANNEL_CLOSE);
        assert_eq!(harness.open(1 << 20), msg::CHANNEL_OPEN_FAILURE);
        std::fs::write(&stop, b"").unwrap();
        harness.pump(|table| table.channels.is_empty());
        assert_eq!(harness.open(1 << 20), msg::CHANNEL_OPEN_CONFIRMATION);
    }

    /// The shell blocks reading its terminal, and only the kill can end it.
    #[test]
    fn closing_a_channel_ends_the_shell_on_its_pty() {
        let mut harness = harness(1);
        harness.open(1 << 20);
        harness.request_pty(0);
        harness.exec(0, "read -r line");
        harness.close(0);
        assert_eq!(harness.written()[0][0], msg::CHANNEL_CLOSE);
        assert_eq!(harness.open(1 << 20), msg::CHANNEL_OPEN_FAILURE);
        harness.pump(|table| table.channels.is_empty());
        assert_eq!(harness.open(1 << 20), msg::CHANNEL_OPEN_CONFIRMATION);
    }

    #[test]
    fn a_channel_with_nothing_running_frees_its_slot_at_once() {
        let mut harness = harness(1);
        harness.open(1000);
        harness.close(0);
        assert_eq!(harness.written()[0][0], msg::CHANNEL_CLOSE);
        assert!(harness.table.channels.is_empty());
        // A second close for it, and one for a channel that never was.
        harness.close(0);
        harness.close(9);
        assert!(harness.written().is_empty());
    }
}

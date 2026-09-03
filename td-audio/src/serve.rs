//! The daemon: a Unix socket, a set of client sessions, and one PCM.
//!
//! §K.5 fixes the shape. The socket is `/run/td-audio/native`, in a directory
//! `td-seatd` creates and this daemon does not: "`td-seatd` **creates the
//! directory only**; it cannot create a listening socket and exit, because a
//! listening socket dies with the process that holds it unless it is passed on,
//! and nothing here passes descriptors between units. The daemon binds its own
//! socket." The directory is 0755 and the socket is 0666, and the gate is
//! `SO_PEERCRED` rather than mode bits, "in code that can say why it refused".
//!
//! One thread. Audio arrives from clients, is summed, and is handed to the
//! device; a second thread would buy nothing but a lock around the mixer. The
//! wait is one `poll(2)` over the listener, every client, and the PCM together,
//! so a client that writes and a device that drains are the same event loop.

use crate::mixer::Mixer;
use crate::session::{Disconnect, Session};
use crate::sink::{is_underrun, AudioSink, Spec};
use crate::sys::{self, Interest, PollSet};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// §K.5: "its socket lives at `/run/td-audio/native`".
pub const SOCKET_PATH: &str = "/run/td-audio/native";

/// §K.5: "the socket is 0666", because the gate is `SO_PEERCRED` and not the
/// mode. A 0600 socket would exclude the jailed app this exists to serve.
pub const SOCKET_MODE: u32 = 0o666;

/// The seat user. §K.5: "accept uid 1000 and the audio uid, refuse everything
/// else".
pub const SEAT_UID: u32 = 1000;

/// The most clients this daemon will hold at once.
///
/// Not politeness: every accepted connection costs a decoder buffer and a poll
/// slot, and a local process can call `connect(2)` in a loop. Refusing the
/// N+1st is a bounded failure; running out of descriptors takes the audio down
/// for everyone already playing.
pub const MAX_CLIENTS: usize = 32;

/// The most connections one pass will take off the backlog.
///
/// The socket is 0666 by design, so a uid the policy REFUSES can still keep the
/// backlog non-empty. Without a budget, `accept` returns only when the backlog
/// drains, and a peer that reconnects in a loop keeps the pass from ever
/// reaching the device — the daemon stops playing audio while doing nothing but
/// refusing. Enough to fill the table in one pass, and then the device gets its
/// turn.
pub const MAX_ACCEPTS_PER_PASS: usize = MAX_CLIENTS;

/// How long a connection may stay unauthenticated.
///
/// A peer that connects and sends nothing is never readable, never hangs up and
/// owes nothing, so no other path drops it. Thirty-two of those hold the table
/// shut against every real client, at the cost of one `connect(2)` apiece.
///
/// Measured on `Instant`, not on the clock the timing replies use. A deadline
/// read from the wall clock is one an NTP correction can move: a step backwards
/// holds every unauthenticated peer until the clock catches up, and a step
/// forwards drops them all at once.
pub const AUTH_DEADLINE: Duration = Duration::from_secs(10);

/// How long one wait may block. A period at 48 kHz is about 21 ms, so this is
/// generous enough to be a backstop rather than a poll rate.
pub const WAIT_MS: i32 = 100;

/// The most bytes read from one client per pass.
const READ_CHUNK: usize = 64 * 1024;

/// The most a client may leave half-framed before it is hung up on.
///
/// Every frame is bounded on its own by `wire::Descriptor::parse`, so this is
/// not about one enormous frame — it is about a client that declares a frame
/// and then stops. Two data frames' worth is enough that no honest client ever
/// reaches it and small enough that a stuck one is found.
const MAX_HALF_FRAMED: usize = 2 * crate::wire::DATA_MAX;

/// Who may connect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    /// Every uid allowed through. §K.5 names two.
    pub allowed_uids: Vec<u32>,
}

impl Policy {
    /// The uid this daemon runs as, plus the seat user.
    ///
    /// The daemon's own uid is included because §K.5's CLI personalities are
    /// "ordinary Pulse *clients* of the daemon's own socket" — `td-audio
    /// volume` connects the same way Firefox does, and it runs as the audio
    /// user.
    pub fn for_uid(own: u32) -> Self {
        let mut allowed_uids = vec![own];
        if own != SEAT_UID {
            allowed_uids.push(SEAT_UID);
        }
        Self { allowed_uids }
    }

    pub fn admits(&self, peer: &sys::Peer) -> bool {
        self.allowed_uids.contains(&peer.uid)
    }

    /// Why a peer was refused, in words a log line can carry. §K.5 asks for a
    /// decision "in code that can say why it refused".
    pub fn refusal(&self, peer: &sys::Peer) -> String {
        format!(
            "uid {} (pid {}) is not one of the {} permitted uids {:?}",
            peer.uid,
            peer.pid,
            self.allowed_uids.len(),
            self.allowed_uids
        )
    }
}

/// The most one client may be owed before the daemon stops holding it.
///
/// A client that never reads still causes replies: grants, timing, subscription
/// events. Without a ceiling those accumulate in `pending` for as long as it
/// keeps asking, and one jailed client could exhaust the daemon every other
/// client depends on. Sixteen maximum-size control frames is far more than any
/// exchange in this protocol needs and still a bound.
const MAX_PENDING: usize = 16 * crate::wire::CONTROL_MAX;

/// One connected client.
struct Client {
    stream: UnixStream,
    session: Session,
    peer: sys::Peer,
    /// Bytes owed to the client that the socket would not take yet.
    pending: Vec<u8>,
    /// How far into `pending` the socket has got.
    pending_at: usize,
    /// When it connected, for `AUTH_DEADLINE`.
    since: Instant,
}

impl Client {
    fn fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }

    fn wants_write(&self) -> bool {
        self.pending_at < self.pending.len() || self.session.has_output()
    }

    /// Bytes written into `pending` that the socket has not taken.
    fn owed(&self) -> usize {
        self.pending.len().saturating_sub(self.pending_at)
    }

    /// Move whatever the session produced into the outgoing buffer.
    ///
    /// Compacting here rather than draining from the front keeps this a copy
    /// per flush instead of a memmove per write.
    fn collect(&mut self) {
        if self.session.has_output() {
            let bytes = self.session.take_output();
            if self.pending_at > 0 && self.pending_at >= self.pending.len() {
                self.pending.clear();
                self.pending_at = 0;
            }
            self.pending.extend_from_slice(&bytes);
        }
    }

    /// Write what the socket will take. `false` means the client is gone.
    fn flush(&mut self) -> bool {
        while self.pending_at < self.pending.len() {
            let tail = self.pending.get(self.pending_at..).unwrap_or(&[]);
            match self.stream.write(tail) {
                Ok(0) => return false,
                Ok(written) => self.pending_at = self.pending_at.saturating_add(written),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return true,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return false,
            }
        }
        self.pending.clear();
        self.pending_at = 0;
        true
    }
}

/// Why the daemon stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stopped {
    /// The device went away. §K.5's clients are told before the socket closes.
    DeviceGone,
    /// The run limit was reached, which only a test sets.
    Finished,
}

/// The daemon.
pub struct Server<S: AudioSink> {
    listener: UnixListener,
    path: PathBuf,
    clients: Vec<Client>,
    mixer: Mixer,
    sink: S,
    policy: Policy,
    spec: Spec,
    poll: PollSet,
    next_client_index: u32,
    /// How many clients the current poll set covers.
    ///
    /// `accept` appends AFTER `wait` built the set, so a client accepted this
    /// pass has no slot of its own: reading one would read the slot belonging to
    /// whatever `wait` pushed next, which is the PCM. A `POLLHUP` on the device
    /// would then drop a brand-new client that had not been read once.
    polled_clients: usize,
    /// Peers turned away, so a refusal is visible without a log scrape.
    pub refused: u32,
    /// Clients dropped for a protocol error, ditto.
    pub disconnected: u32,
}

impl<S: AudioSink> Server<S> {
    /// Bind the socket and take ownership of the device.
    ///
    /// The socket is removed first if it is there. A stale socket from a daemon
    /// that was killed is the ordinary case on a reboot-less restart, and
    /// `bind(2)` fails with `EADDRINUSE` rather than replacing it — but only a
    /// socket is removed, never a regular file or directory that happens to
    /// have the name.
    pub fn bind(path: &Path, sink: S, policy: Policy) -> io::Result<Self> {
        remove_stale_socket(path)?;
        let listener = UnixListener::bind(path)?;
        listener.set_nonblocking(true)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(SOCKET_MODE))?;
        let spec = sink.spec();
        Ok(Self {
            listener,
            path: path.to_path_buf(),
            clients: Vec::new(),
            mixer: Mixer::new(spec),
            sink,
            policy,
            spec,
            poll: PollSet::new(),
            next_client_index: 0,
            polled_clients: 0,
            refused: 0,
            disconnected: 0,
        })
    }

    /// The most any one client is owed and has not taken.
    #[cfg(test)]
    fn owed_to_clients(&self) -> usize {
        self.clients.iter().map(Client::owed).max().unwrap_or(0)
    }

    /// Move every client's arrival back, so the authentication deadline can be
    /// reached without spending ten seconds of test time on it.
    #[cfg(test)]
    fn age_clients_for_test(&mut self, by: Duration) {
        for client in &mut self.clients {
            // `Instant` cannot go before the monotonic epoch, which on Linux is
            // boot: on a machine younger than `by` — a fresh VM, which is where
            // td's own checks run — this would silently not age, and the test
            // would fail pointing at the deadline instead of at the clock.
            match client.since.checked_sub(by) {
                Some(earlier) => client.since = earlier,
                None => panic!(
                    "this machine has been up for less than {by:?}, so a client's \
                     arrival cannot be moved back that far"
                ),
            }
        }
    }

    pub fn client_count(&self) -> usize {
        self.clients.len()
    }

    pub fn stream_count(&self) -> usize {
        self.mixer.stream_count()
    }

    pub fn socket_path(&self) -> &Path {
        &self.path
    }

    /// The uids this daemon will admit, for the banner it prints on startup.
    /// §K.5 wants the decision visible, and a daemon that announces who it
    /// serves is one whose refusals can be read against something.
    pub fn policy_uids(&self) -> &[u32] {
        &self.policy.allowed_uids
    }

    /// Run until the device is gone, or for `max_passes` passes.
    ///
    /// The bound is what makes this testable and what stops a daemon whose
    /// device has wedged from spinning forever; `None` is the real daemon.
    pub fn run(&mut self, max_passes: Option<u32>) -> io::Result<Stopped> {
        let mut passes = 0u32;
        loop {
            if let Some(limit) = max_passes {
                if passes >= limit {
                    return Ok(Stopped::Finished);
                }
            }
            passes = passes.saturating_add(1);
            if self.pass()? {
                return Ok(Stopped::DeviceGone);
            }
        }
    }

    /// One trip round the loop. `true` means the device is gone.
    pub fn pass(&mut self) -> io::Result<bool> {
        self.wait()?;
        self.accept();
        self.read_clients();
        let gone = self.drive_device()?;
        self.service_clients();
        self.write_clients();
        if gone {
            // Tell every client before the socket goes, so a client sees a
            // stream killed rather than a connection that simply stopped.
            for client in &mut self.clients {
                client.session.kill_all_streams();
                client.collect();
                client.flush();
            }
            self.clients.clear();
            self.forget_orphaned_streams();
        }
        Ok(gone)
    }

    fn wait(&mut self) -> io::Result<()> {
        self.poll.clear();
        self.poll.push(self.listener.as_raw_fd(), Interest::READ)?;
        self.polled_clients = self.clients.len();
        for client in &self.clients {
            let interest = if client.wants_write() {
                Interest::BOTH
            } else {
                Interest::READ
            };
            self.poll.push(client.fd(), interest)?;
        }
        if self.mixer.has_device_work() {
            if let Some(fd) = self.sink.raw_fd() {
                self.poll.push(fd, Interest::WRITE)?;
            }
        }
        self.poll.wait(WAIT_MS)?;
        Ok(())
    }

    fn accept(&mut self) {
        if !self.poll.readiness(0).readable {
            return;
        }
        for _ in 0..MAX_ACCEPTS_PER_PASS {
            let (stream, _) = match self.listener.accept() {
                Ok(pair) => pair,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return,
            };
            if stream.set_nonblocking(true).is_err() {
                continue;
            }
            // The gate, before a single byte is read. A refused peer never
            // reaches the parser at all, which is the point of doing this at
            // connect time rather than at AUTH.
            let peer = match sys::peer_credentials(stream.as_raw_fd()) {
                Ok(peer) => peer,
                Err(_) => {
                    // A connection whose credentials cannot be read is refused.
                    // There is no safe default here: the alternative is
                    // admitting a peer whose uid is unknown.
                    self.refused = self.refused.saturating_add(1);
                    continue;
                }
            };
            if !self.policy.admits(&peer) {
                // §K.5 asks for the decision to be "in code that can say why it
                // refused", so it says so.
                eprintln!("td-audio: refused a connection: {}", self.policy.refusal(&peer));
                self.refused = self.refused.saturating_add(1);
                continue;
            }
            if self.clients.len() >= MAX_CLIENTS {
                eprintln!(
                    "td-audio: refused uid {} (pid {}): already serving {MAX_CLIENTS} clients",
                    peer.uid, peer.pid
                );
                self.refused = self.refused.saturating_add(1);
                continue;
            }
            let index = self.next_client_index;
            self.next_client_index = self.next_client_index.saturating_add(1);
            self.clients.push(Client {
                stream,
                session: Session::new(self.spec, index),
                peer,
                pending: Vec::new(),
                pending_at: 0,
                since: Instant::now(),
            });
        }
    }

    fn read_clients(&mut self) {
        let mut buffer = [0u8; READ_CHUNK];
        let mut drop_indexes: Vec<usize> = Vec::new();
        for (index, client) in self.clients.iter_mut().enumerate() {
            if index >= self.polled_clients {
                // Accepted after the poll set was built: it has no slot, and
                // borrowing the next one would read the device's. It is read
                // next pass, one `WAIT_MS` later at worst.
                continue;
            }
            // Slot 0 is the listener, so client `index` is poll slot
            // `index + 1`.
            let readiness = self.poll.readiness(index.saturating_add(1));
            if readiness.gone {
                drop_indexes.push(index);
                continue;
            }
            if !readiness.readable {
                continue;
            }
            match client.stream.read(&mut buffer) {
                Ok(0) => drop_indexes.push(index),
                Ok(read) => {
                    let bytes = buffer.get(..read).unwrap_or(&[]);
                    let before = client.session.version();
                    if let Err(reason) = client.session.feed(bytes, &mut self.mixer) {
                        // A protocol error is not survivable: the stream is out
                        // of frame and every later byte would be read at the
                        // wrong offset. §K.3's schemas exist to detect exactly
                        // this, and detecting it means hanging up.
                        report_disconnect(&client.peer, &reason, client.session.stream_count());
                        drop_indexes.push(index);
                    } else if client.session.buffered() > MAX_HALF_FRAMED {
                        eprintln!(
                            "td-audio: hung up on uid {} pid {}: {} bytes of an unfinished \
                             frame, over the {MAX_HALF_FRAMED}-byte bound",
                            client.peer.uid,
                            client.peer.pid,
                            client.session.buffered()
                        );
                        drop_indexes.push(index);
                    } else if before.is_none() {
                        if let Some(version) = client.session.version() {
                            eprintln!(
                                "td-audio: uid {} (pid {}) authenticated at protocol {version}",
                                client.peer.uid, client.peer.pid
                            );
                        }
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => drop_indexes.push(index),
            }
        }
        self.drop_clients(&drop_indexes);
    }

    /// Mix and hand a period to the device. `true` means the device is gone.
    fn drive_device(&mut self) -> io::Result<bool> {
        match self.sink.wait(0)? {
            crate::sink::Wait::Gone => return Ok(true),
            crate::sink::Wait::Underrun => {
                // §K.4's recovery: re-prepare and re-prime. The mixer rebases
                // its output axis, so the clients' positions stay consistent
                // across the gap rather than jumping by the lost frames.
                self.mixer.recover(&mut self.sink)?;
                // Deliberately NOT started here. `PREPARE` left the ring empty
                // and `start_threshold` is the boundary, so a START now runs
                // the device on nothing and underruns again immediately. The
                // rule below — start once a period has actually been written —
                // is the same rule, and the next pass reaches it.
                return Ok(false);
            }
            crate::sink::Wait::Timeout => return Ok(false),
            crate::sink::Wait::Writable => {}
        }
        match self.mixer.pump(&mut self.sink) {
            Ok(_) => {}
            Err(error) if is_underrun(&error) => {
                self.mixer.recover(&mut self.sink)?;
                // Deliberately NOT started here. `PREPARE` left the ring empty
                // and `start_threshold` is the boundary, so a START now runs
                // the device on nothing and underruns again immediately. The
                // rule below — start once a period has actually been written —
                // is the same rule, and the next pass reaches it.
                return Ok(false);
            }
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => return Ok(true),
            Err(error) => return Err(error),
        }
        // Start only once there is audio in the ring. `SwParams::set_playback`
        // sets `start_threshold` to the boundary so the device never starts
        // itself, which is what keeps the first sound aligned with the first
        // audio instead of with the silence before it.
        if !self.sink.is_running()
            && self
                .mixer
                .ready_to_start(self.sink.buffer_frames(), self.sink.period_frames())
        {
            self.sink.start()?;
        }
        Ok(false)
    }

    fn service_clients(&mut self) {
        let now = now_usec();
        let mut drop_indexes: Vec<usize> = Vec::new();
        for (index, client) in self.clients.iter_mut().enumerate() {
            client.session.tick(now);
            client.session.service(&self.mixer);
            client.collect();
            if client.session.version().is_none() && client.since.elapsed() > AUTH_DEADLINE {
                eprintln!(
                    "td-audio: hung up on uid {} pid {}: connected and never \
                     authenticated",
                    client.peer.uid, client.peer.pid
                );
                drop_indexes.push(index);
                continue;
            }
            if client.owed() > MAX_PENDING {
                eprintln!(
                    "td-audio: hung up on uid {} pid {}: {} bytes owed and \
                     not reading",
                    client.peer.uid,
                    client.peer.pid,
                    client.owed()
                );
                drop_indexes.push(index);
            }
        }
        self.drop_clients(&drop_indexes);
    }

    fn write_clients(&mut self) {
        let mut drop_indexes: Vec<usize> = Vec::new();
        for (index, client) in self.clients.iter_mut().enumerate() {
            if !client.flush() {
                drop_indexes.push(index);
            }
        }
        self.drop_clients(&drop_indexes);
    }

    /// Remove clients by index, and every mixer stream that went with them.
    ///
    /// Back to front, so an earlier removal does not shift a later index — the
    /// bug that makes a disconnect drop somebody else's audio.
    fn drop_clients(&mut self, indexes: &[usize]) {
        if indexes.is_empty() {
            return;
        }
        let mut sorted = indexes.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        for index in sorted.into_iter().rev() {
            if index >= self.clients.len() {
                continue;
            }
            self.clients.remove(index);
            self.disconnected = self.disconnected.saturating_add(1);
        }
        // And every stream those clients had is the mixer's to forget. A client
        // that vanishes mid-stream leaves audio queued, and audio nobody owns
        // would keep playing. Dropping the client does not do this: the mixer
        // is a separate table, reconciled here against the sessions that remain.
        self.forget_orphaned_streams();
    }

    /// Streams whose session is gone.
    ///
    /// The mixer is keyed by ids it issues itself, and a dropped session takes
    /// its streams with it — so the mixer is reconciled against the sessions
    /// that remain rather than trusted to have been told.
    fn forget_orphaned_streams(&mut self) {
        let live: Vec<crate::mixer::StreamId> = self
            .clients
            .iter()
            .flat_map(|client| client.session.stream_ids())
            .collect();
        self.mixer.retain(&live);
    }

    /// Unbind. Called on the way out so a restart does not meet its own socket.
    ///
    /// The device is drained rather than dropped: §K.4's `DRAIN` exists for
    /// exactly this, and stopping with audio in the ring truncates whatever was
    /// playing. The bound inside `drain_all` is what keeps a wedged card from
    /// hanging shutdown.
    pub fn shutdown(&mut self) {
        let _ = self.mixer.drain_all(&mut self.sink, 64);
        let _ = self.sink.drain();
        eprintln!(
            "td-audio: {} closing with {} client(s) and {} stream(s); \
             {} peer(s) refused, {} hung up on",
            self.socket_path().display(),
            self.client_count(),
            self.stream_count(),
            self.refused,
            self.disconnected
        );
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Microseconds since the epoch, for the timing replies' server timestamp.
///
/// A clock read that fails is reported as zero rather than propagated: a
/// timestamp is the one field in a timing reply a client can survive being
/// wrong, and refusing to answer at all would stall it.
fn now_usec() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_micros().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

/// Remove a socket left behind by a previous daemon — and only a socket.
fn remove_stale_socket(path: &Path) -> io::Result<()> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if !metadata.file_type().is_socket() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} exists and is not a socket", path.display()),
        ));
    }
    std::fs::remove_file(path)
}

/// One line about a client that was hung up on. §K.5 wants the refusal to be
/// able to say why.
fn report_disconnect(peer: &sys::Peer, reason: &Disconnect, streams: usize) {
    eprintln!(
        "td-audio: hung up on uid {} pid {} with {streams} stream(s): {reason}",
        peer.uid, peer.pid
    );
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::proto::command;
    use crate::sink::MemorySink;
    use crate::tag;
    use crate::wire;

    fn socket_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("td-audio-{}-{}", name, std::process::id()))
    }

    fn server(name: &str) -> (Server<MemorySink>, PathBuf) {
        let path = socket_path(name);
        let _ = std::fs::remove_file(&path);
        let mut sink = MemorySink::fixed();
        sink.start().unwrap();
        let uid = own_uid();
        let server = Server::bind(&path, sink, Policy::for_uid(uid)).unwrap();
        (server, path)
    }

    /// This test process's uid, read from `/proc/self/status` so the test needs
    /// no syscall of its own.
    fn own_uid() -> u32 {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|text| {
                text.lines()
                    .find_map(|line| line.strip_prefix("Uid:"))
                    .and_then(|rest| rest.split_whitespace().next().map(|s| s.to_string()))
            })
            .and_then(|value| value.parse().ok())
            .unwrap_or(0)
    }

    fn control(packet: &[u8]) -> Vec<u8> {
        wire::control_frame(packet)
    }

    fn build(command: u32, tag_value: u32, body: impl FnOnce(&mut tag::Writer)) -> Vec<u8> {
        let mut writer = tag::Writer::new();
        writer.u32(command).u32(tag_value);
        body(&mut writer);
        control(&writer.into_bytes())
    }

    fn auth_packet() -> Vec<u8> {
        build(command::AUTH, 0, |writer| {
            writer.u32(35).arbitrary(&[0u8; crate::session::AUTH_COOKIE_LEN]);
        })
    }

    /// Read whatever the server has written, without blocking forever.
    fn drain_socket(stream: &mut UnixStream) -> Vec<u8> {
        stream.set_nonblocking(true).unwrap();
        let mut out = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => out.extend_from_slice(buffer.get(..read).unwrap_or(&[])),
                Err(_) => break,
            }
        }
        out
    }

    fn first_command(bytes: &[u8]) -> Option<u32> {
        let mut decoder = wire::Decoder::new();
        decoder.push(bytes);
        while let Some(frame) = decoder.next_frame() {
            if let Ok(wire::Frame::Control(packet)) = frame {
                return wire::command_and_tag(&packet).ok().map(|(c, _)| c);
            }
        }
        None
    }

    /// The socket is 0666 and the daemon does not create its directory. §K.5
    /// specifies both, and a 0600 socket would exclude the jailed app.
    #[test]
    fn the_socket_is_world_writable_because_the_gate_is_peercred() {
        let (mut server, path) = server("mode");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, SOCKET_MODE, "§K.5 pins the socket at 0666");
        assert_eq!(server.socket_path(), path.as_path());
        server.shutdown();
        assert!(!path.exists(), "the socket is removed on the way out");
    }

    /// A stale socket is replaced; a regular file with the same name is not,
    /// because deleting an unrelated file to make room is a worse failure than
    /// refusing to start.
    #[test]
    fn a_stale_socket_is_replaced_but_a_regular_file_is_not() {
        let path = socket_path("stale");
        let _ = std::fs::remove_file(&path);
        let mut sink = MemorySink::fixed();
        sink.start().unwrap();
        let first = Server::bind(&path, sink, Policy::for_uid(own_uid())).unwrap();
        drop(first);
        // The socket is still on disk; binding again must succeed.
        let mut sink = MemorySink::fixed();
        sink.start().unwrap();
        let mut second = Server::bind(&path, sink, Policy::for_uid(own_uid())).unwrap();
        second.shutdown();

        std::fs::write(&path, b"not a socket").unwrap();
        let sink = MemorySink::fixed();
        let error = match Server::bind(&path, sink, Policy::for_uid(own_uid())) {
            Ok(_) => panic!("a regular file was replaced by a socket"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        let _ = std::fs::remove_file(&path);
    }

    /// A real client connects, authenticates and creates a stream over a real
    /// socket. This is the rung: bytes on a socket become a mixer stream.
    #[test]
    fn a_client_connects_authenticates_and_plays() {
        let (mut server, path) = server("play");
        let mut client = UnixStream::connect(&path).unwrap();
        client.set_nonblocking(false).unwrap();

        client.write_all(&auth_packet()).unwrap();
        // Two passes: the first accepts the connection and reads the packet,
        // the second finds the socket writable and flushes the reply. A daemon
        // that needed only one would be one that wrote while holding the read.
        server.pass().unwrap();
        server.pass().unwrap();
        assert_eq!(server.client_count(), 1);
        assert_eq!(first_command(&drain_socket(&mut client)), Some(command::REPLY));

        // The version-35 create request.
        let create = build(command::CREATE_PLAYBACK_STREAM, 1, |writer| {
            writer
                .sample_spec(tag::SampleSpec {
                    format: crate::proto::format::SAMPLE_S16LE,
                    channels: 2,
                    rate: 48_000,
                })
                .channel_map(&crate::session::CHANNEL_MAP)
                .u32(tag::INVALID_INDEX)
                .null_string()
                .u32(tag::INVALID_INDEX)
                .boolean(false)
                .u32(tag::INVALID_INDEX)
                .u32(tag::INVALID_INDEX)
                .u32(tag::INVALID_INDEX)
                .u32(0)
                .cvolume(&[crate::mixer::VOLUME_NORM, crate::mixer::VOLUME_NORM]);
            for _ in 0..9 {
                writer.boolean(false);
            }
            writer.proplist(&[tag::text_property("media.name", "over a socket")]);
            for _ in 0..7 {
                writer.boolean(false);
            }
            writer.u8(0);
        });
        client.set_nonblocking(false).unwrap();
        client.write_all(&create).unwrap();
        server.pass().unwrap();
        server.pass().unwrap();
        assert_eq!(server.stream_count(), 1, "the client has a mixer stream");
        assert_eq!(first_command(&drain_socket(&mut client)), Some(command::REPLY));

        // And audio on the stream channel reaches the device.
        let mut pcm = Vec::new();
        for _ in 0..9600 * 2 {
            pcm.extend_from_slice(&900i16.to_le_bytes());
        }
        let mut audio = wire::Descriptor::encode(pcm.len() as u32, 0, 0, 0).to_vec();
        audio.extend_from_slice(&pcm);
        client.write_all(&audio).unwrap();
        for _ in 0..4 {
            server.pass().unwrap();
        }
        assert!(
            server.sink.frames_written() > 0,
            "audio from the socket never reached the device"
        );
        // Prebuffering forbids a silent period before the client's first data.
        let samples = server.sink.samples();
        assert!(
            samples.get(..64).is_some_and(|run| run.iter().all(|s| *s == 900)),
            "the client's audio arrived altered or in pieces"
        );
        server.shutdown();
    }

    /// A client that hangs up loses its mixer stream. Audio nobody owns would
    /// keep playing, and the mixer would keep summing it forever.
    #[test]
    fn a_client_that_leaves_takes_its_stream_with_it() {
        let (mut server, path) = server("leave");
        let mut client = UnixStream::connect(&path).unwrap();
        client.write_all(&auth_packet()).unwrap();
        server.pass().unwrap();
        let create = build(command::CREATE_PLAYBACK_STREAM, 1, |writer| {
            writer
                .sample_spec(tag::SampleSpec {
                    format: crate::proto::format::SAMPLE_S16LE,
                    channels: 2,
                    rate: 48_000,
                })
                .channel_map(&crate::session::CHANNEL_MAP)
                .u32(tag::INVALID_INDEX)
                .null_string()
                .u32(tag::INVALID_INDEX)
                .boolean(false)
                .u32(tag::INVALID_INDEX)
                .u32(tag::INVALID_INDEX)
                .u32(tag::INVALID_INDEX)
                .u32(0)
                .cvolume(&[crate::mixer::VOLUME_NORM, crate::mixer::VOLUME_NORM]);
            for _ in 0..9 {
                writer.boolean(false);
            }
            writer.proplist(&[]);
            for _ in 0..7 {
                writer.boolean(false);
            }
            writer.u8(0);
        });
        client.write_all(&create).unwrap();
        server.pass().unwrap();
        assert_eq!(server.stream_count(), 1);

        drop(client);
        for _ in 0..3 {
            server.pass().unwrap();
        }
        assert_eq!(server.client_count(), 0);
        assert_eq!(server.stream_count(), 0, "the mixer still holds a dead stream");
        server.shutdown();
    }

    /// Recovering from an underrun does not start the device on an empty ring.
    ///
    /// `PREPARE` discards the ring, and `start_threshold` is the boundary, so a
    /// `START` with nothing queued runs the device on silence and underruns
    /// again at the next pointer update — a prepare/start/XRUN churn that never
    /// converges. The rule fifteen lines below the recovery path is the right
    /// one and always was: start once a period has actually been written.
    #[test]
    fn recovery_does_not_start_the_device_on_an_empty_ring() {
        let (mut server, _path) = server("recovery");
        // Ask the device for more than it holds, which is what an underrun is.
        server.sink.advance(1_000_000);
        assert!(!server.sink.is_running(), "the underrun stopped it");

        server.pass().unwrap();
        assert!(
            !server.sink.is_running(),
            "started on a ring PREPARE had just emptied"
        );

        // And it does start again once there is audio to start on.
        let id = server.mixer.open(4096).unwrap();
        let audio = vec![0u8; 4 * 512];
        server.mixer.write(id, &audio).unwrap();
        server.pass().unwrap();
        assert!(server.sink.is_running(), "and it never started at all");
        server.shutdown();
    }

    /// Production starts a prepared PCM only after every whole-period slot is
    /// primed, rather than racing the first client period against playback.
    #[test]
    fn a_continuous_stream_primes_the_ring_before_device_start() {
        let path = socket_path("prime");
        let _ = std::fs::remove_file(&path);
        let sink = MemorySink::new(crate::sink::Spec::fixed(), 12, 4);
        let mut server = Server::bind(&path, sink, Policy::for_uid(own_uid())).unwrap();
        let id = server.mixer.open(32).unwrap();
        server.mixer.write(id, &[0u8; 16 * 4]).unwrap();

        assert!(!server.drive_device().unwrap());
        assert!(!server.sink.is_running());
        assert!(!server.drive_device().unwrap());
        assert!(!server.sink.is_running());
        assert!(!server.drive_device().unwrap());
        assert!(server.sink.is_running(), "the primed ring was never started");
        assert_eq!(server.sink.frames_written(), 12);
        server.shutdown();
    }

    /// A create request at version 35, as a modern client sends it.
    fn create_request(tag_number: u32) -> Vec<u8> {
        build(command::CREATE_PLAYBACK_STREAM, tag_number, |writer| {
            writer
                .sample_spec(tag::SampleSpec {
                    format: crate::proto::format::SAMPLE_S16LE,
                    channels: 2,
                    rate: 48_000,
                })
                .channel_map(&crate::session::CHANNEL_MAP)
                .u32(tag::INVALID_INDEX)
                .null_string()
                .u32(tag::INVALID_INDEX)
                .boolean(false)
                .u32(tag::INVALID_INDEX)
                .u32(tag::INVALID_INDEX)
                .u32(tag::INVALID_INDEX)
                .u32(0)
                .cvolume(&[crate::mixer::VOLUME_NORM, crate::mixer::VOLUME_NORM]);
            for _ in 0..9 {
                writer.boolean(false);
            }
            writer.proplist(&[]);
            for _ in 0..7 {
                writer.boolean(false);
            }
            writer.u8(0);
        })
    }

    /// TWO clients each play a stream. This is rung 26's acceptance criterion
    /// and §K.5's "mix from the start — browser plus notification".
    ///
    /// It did not work. Every session numbers its Pulse channels from zero, and
    /// that number was handed straight to the shared mixer as its key, so the
    /// SECOND client to create a stream collided with the first and was refused
    /// with `PA_ERR_INTERNAL`. Every other test here, and the whole live
    /// libpulse run, used one client — which is exactly why nothing caught it.
    #[test]
    fn two_clients_each_get_a_stream_of_their_own() {
        let (mut server, path) = server("twoclients");
        let mut first = UnixStream::connect(&path).unwrap();
        let mut second = UnixStream::connect(&path).unwrap();
        first.write_all(&auth_packet()).unwrap();
        second.write_all(&auth_packet()).unwrap();
        server.pass().unwrap();
        server.pass().unwrap();
        assert_eq!(server.client_count(), 2);
        let _ = drain_socket(&mut first);
        let _ = drain_socket(&mut second);

        first.write_all(&create_request(1)).unwrap();
        second.write_all(&create_request(1)).unwrap();
        server.pass().unwrap();
        server.pass().unwrap();

        assert_eq!(
            first_command(&drain_socket(&mut first)),
            Some(command::REPLY),
            "the first client's stream"
        );
        assert_eq!(
            first_command(&drain_socket(&mut second)),
            Some(command::REPLY),
            "and the second client's, which used to be PA_ERR_INTERNAL"
        );
        assert_eq!(server.stream_count(), 2, "two streams, not one");
        server.shutdown();
    }

    /// A client that never reads is hung up on rather than held forever.
    ///
    /// Nothing bounded the outgoing buffer: a jailed application could ask
    /// small questions and never read the answers until the daemon every other
    /// client depends on ran out of memory.
    #[test]
    fn a_client_that_never_reads_is_dropped_once_it_is_owed_too_much() {
        let (mut server, path) = server("neverreads");
        let mut client = UnixStream::connect(&path).unwrap();
        client.write_all(&auth_packet()).unwrap();
        server.pass().unwrap();
        assert_eq!(server.client_count(), 1);

        // Ask, and never read the answers. The socket buffer absorbs some; the
        // daemon holds the rest, and that is what has to be bounded.
        let mut question = Vec::new();
        for tag_number in 0..2000u32 {
            question.extend_from_slice(&build(command::GET_SINK_INFO_LIST, tag_number, |_| {}));
        }
        let _ = client.set_nonblocking(true);
        // Written with a cursor, and restarted only at a block boundary: a
        // half-written packet would be a framing error, and the daemon would
        // hang up for THAT rather than for the backlog this test is about.
        let mut sent = 0usize;
        let mut dropped = false;
        for _ in 0..400 {
            if sent >= question.len() {
                sent = 0;
            }
            if let Ok(written) = client.write(question.get(sent..).unwrap_or(&[])) {
                sent = sent.saturating_add(written);
            }
            server.pass().unwrap();
            if server.client_count() == 0 {
                dropped = true;
                break;
            }
            assert!(
                server.owed_to_clients() <= MAX_PENDING,
                "the daemon is holding {} bytes for one client",
                server.owed_to_clients()
            );
        }
        assert!(dropped, "it was never dropped, and the buffer grew unbounded");
        server.shutdown();
    }

    /// A peer that connects and says nothing does not hold a slot forever.
    #[test]
    fn an_unauthenticated_peer_is_dropped_after_the_deadline() {
        let (mut server, path) = server("silentpeer");
        let _quiet = UnixStream::connect(&path).unwrap();
        server.pass().unwrap();
        assert_eq!(server.client_count(), 1);
        // Reached by moving the client's arrival back rather than by waiting
        // ten seconds for it.
        server.age_clients_for_test(AUTH_DEADLINE + Duration::from_secs(1));
        server.pass().unwrap();
        assert_eq!(server.client_count(), 0, "it never authenticated");
        server.shutdown();
    }

    /// A client that sends a malformed packet is hung up on, and the other
    /// client keeps playing. One bad parser input must not be a service outage.
    #[test]
    fn a_protocol_error_drops_one_client_and_not_the_other() {
        let (mut server, path) = server("badpacket");
        let mut good = UnixStream::connect(&path).unwrap();
        let mut bad = UnixStream::connect(&path).unwrap();
        good.write_all(&auth_packet()).unwrap();
        bad.write_all(&auth_packet()).unwrap();
        server.pass().unwrap();
        assert_eq!(server.client_count(), 2);

        // GET_SERVER_INFO with an argument it does not take.
        bad.write_all(&build(command::GET_SERVER_INFO, 3, |writer| {
            writer.u32(1);
        }))
        .unwrap();
        for _ in 0..3 {
            server.pass().unwrap();
        }
        assert_eq!(server.client_count(), 1, "the bad client is gone");
        assert_eq!(server.disconnected, 1);

        // The survivor is still answered.
        good.write_all(&build(command::GET_SINK_INFO_LIST, 4, |_| {}))
            .unwrap();
        server.pass().unwrap();
        assert_eq!(first_command(&drain_socket(&mut good)), Some(command::REPLY));
        server.shutdown();
    }

    /// A client that never reads is not allowed to stall the daemon.
    ///
    /// The daemon holds its unwritten bytes and keeps serving everyone else,
    /// because a blocking write to one socket would stop the audio for every
    /// other client on the machine.
    #[test]
    fn a_client_that_never_reads_does_not_stall_the_daemon() {
        let (mut server, path) = server("silent");
        let mut silent = UnixStream::connect(&path).unwrap();
        let mut talker = UnixStream::connect(&path).unwrap();
        silent.write_all(&auth_packet()).unwrap();
        talker.write_all(&auth_packet()).unwrap();
        server.pass().unwrap();

        // Ask for a lot and never read any of it.
        for tag_value in 0..200u32 {
            let _ = silent.write_all(&build(command::GET_SINK_INFO_LIST, tag_value, |_| {}));
        }
        for _ in 0..20 {
            server.pass().unwrap();
        }
        // The talker is still served.
        talker
            .write_all(&build(command::GET_SERVER_INFO, 1, |_| {}))
            .unwrap();
        server.pass().unwrap();
        assert_eq!(
            first_command(&drain_socket(&mut talker)),
            Some(command::REPLY),
            "a client that never reads stalled the daemon"
        );
        server.shutdown();
    }

    /// The policy accepts the daemon's own uid and the seat user, and nothing
    /// else — including uid 0, which is the one a mistake would most likely
    /// let through.
    #[test]
    fn the_policy_admits_exactly_two_uids() {
        let policy = Policy::for_uid(994);
        let peer = |uid| sys::Peer { pid: 1, uid, gid: uid };
        assert!(policy.admits(&peer(994)), "the daemon's own uid");
        assert!(policy.admits(&peer(SEAT_UID)), "the seat user");
        assert!(!policy.admits(&peer(0)), "root is not on the list");
        assert!(!policy.admits(&peer(1001)), "another user");
        assert!(!policy.admits(&peer(65534)), "nobody");
        assert_eq!(policy.allowed_uids.len(), 2);
        // And a refusal says why, with the uid in it.
        let refusal = policy.refusal(&peer(1001));
        assert!(refusal.contains("1001"), "{refusal}");
        assert!(refusal.contains("not one of"), "{refusal}");
        // A daemon that happens to run AS the seat user does not list it twice.
        let same = Policy::for_uid(SEAT_UID);
        assert_eq!(same.allowed_uids, vec![SEAT_UID]);
    }

    /// The credentials come from the kernel, and they are this process's own
    /// over a socket it connected to itself. A daemon that read them wrong
    /// would refuse everyone or admit everyone.
    #[test]
    fn peer_credentials_are_the_kernels_answer_not_the_clients() {
        let (mut server, path) = server("cred");
        let client = UnixStream::connect(&path).unwrap();
        server.pass().unwrap();
        assert_eq!(server.client_count(), 1);
        let peer = server.clients.first().map(|client| client.peer).unwrap();
        assert_eq!(peer.uid, own_uid(), "the connecting process's real uid");
        assert_eq!(peer.pid, std::process::id() as i32, "and its real pid");
        assert!(peer.pid > 0);
        drop(client);
        server.shutdown();
    }

    /// The daemon holds a bounded number of clients. A local process can call
    /// `connect(2)` in a loop, and running out of descriptors would take the
    /// audio down for everyone already playing.
    #[test]
    fn the_client_count_is_bounded() {
        let (mut server, path) = server("bound");
        let mut held = Vec::new();
        for _ in 0..MAX_CLIENTS + 8 {
            match UnixStream::connect(&path) {
                Ok(stream) => held.push(stream),
                Err(_) => break,
            }
        }
        for _ in 0..4 {
            server.pass().unwrap();
        }
        assert_eq!(server.client_count(), MAX_CLIENTS);
        assert!(server.refused >= 8, "the surplus was refused, not queued");
        server.shutdown();
    }

    /// Losing the device tells every client its streams are gone rather than
    /// letting the connection go quiet.
    #[test]
    fn losing_the_device_tells_the_clients() {
        let (mut server, path) = server("unplug");
        let mut client = UnixStream::connect(&path).unwrap();
        client.write_all(&auth_packet()).unwrap();
        server.pass().unwrap();
        let _ = drain_socket(&mut client);
        server.sink.unplug();
        let stopped = server.run(Some(4)).unwrap();
        assert_eq!(stopped, Stopped::DeviceGone);
        assert_eq!(server.client_count(), 0);
        server.shutdown();
    }

    /// The run limit is what makes this loop testable and what stops a wedged
    /// device from spinning forever.
    #[test]
    fn the_run_limit_ends_the_loop() {
        let (mut server, path) = server("limit");
        assert_eq!(server.run(Some(3)).unwrap(), Stopped::Finished);
        assert!(path.exists());
        server.shutdown();
    }

    /// A `MemorySink` that also occupies a poll slot.
    ///
    /// `MemorySink::raw_fd` answers `None`, so in every other test here the
    /// device is not in the poll set at all and the client indexes and the slot
    /// indexes cannot disagree. That is why the guard below had no regression
    /// test: the condition it defends against was unreachable in the harness,
    /// not merely unexercised by it.
    ///
    /// The descriptor is one end of a `UnixStream` pair whose other end has been
    /// dropped, so it polls as hung up. `wait` still delegates, because the
    /// daemon asks the sink whether the device is gone and never asks the poll
    /// set; the hangup is here to make the wrong slot say something a client
    /// would be dropped for.
    struct PollableSink {
        inner: MemorySink,
        endpoint: UnixStream,
    }

    impl PollableSink {
        fn fixed() -> Self {
            let (endpoint, peer) = UnixStream::pair().unwrap();
            drop(peer);
            Self {
                inner: MemorySink::fixed(),
                endpoint,
            }
        }
    }

    impl AudioSink for PollableSink {
        fn spec(&self) -> Spec {
            self.inner.spec()
        }
        fn device_delay(&mut self) -> io::Result<u64> {
            self.inner.device_delay()
        }
        fn wait(&mut self, timeout_ms: i32) -> io::Result<crate::sink::Wait> {
            self.inner.wait(timeout_ms)
        }
        fn write(&mut self, pcm: &[u8]) -> io::Result<usize> {
            self.inner.write(pcm)
        }
        fn start(&mut self) -> io::Result<()> {
            self.inner.start()
        }
        fn stop(&mut self) -> io::Result<()> {
            self.inner.stop()
        }
        fn drain(&mut self) -> io::Result<()> {
            self.inner.drain()
        }
        fn recover(&mut self) -> io::Result<()> {
            self.inner.recover()
        }
        fn buffer_frames(&self) -> u64 {
            self.inner.buffer_frames()
        }
        fn period_frames(&self) -> u64 {
            self.inner.period_frames()
        }
        fn raw_fd(&self) -> Option<RawFd> {
            Some(self.endpoint.as_raw_fd())
        }
        fn is_running(&self) -> bool {
            self.inner.is_running()
        }
    }

    /// A client accepted after the poll set was built has no slot of its own,
    /// and the slot at its index belongs to the device.
    ///
    /// `accept` runs inside the pass that `wait` opened, so the client vector
    /// can grow after the slots are fixed. Reading slot `index + 1` for such a
    /// client reads the device's, and a client would be dropped for what the
    /// device said — on its very first pass, before it could authenticate.
    ///
    /// The two poll views of the sink disagree here BY CONSTRUCTION, and no
    /// real device produces that: `AlsaSink::wait` polls the same descriptor,
    /// so on real hardware a `POLLHUP` in the shared poll is also a `Wait::Gone`
    /// and `pass` clears every client anyway. The disagreement is what makes
    /// the wrong slot observable at all — the device is pushed with
    /// `Interest::WRITE`, so `POLLIN` never comes back for it, `errored` is not
    /// consulted, and `POLLOUT` makes the unguarded path behave exactly like
    /// the guarded one. What is under test is the index-to-slot mapping, which
    /// is wrong without the guard whatever the device happens to be saying.
    #[test]
    fn a_client_accepted_after_the_poll_set_is_built_waits_for_its_own_slot() {
        let path = socket_path("midpass");
        let _ = std::fs::remove_file(&path);
        let mut sink = PollableSink::fixed();
        sink.start().unwrap();
        let mut server = Server::bind(&path, sink, Policy::for_uid(own_uid())).unwrap();

        // The first client gets a pass to itself, so it holds slot 1 and the
        // device holds slot 2.
        let mut first = UnixStream::connect(&path).unwrap();
        server.pass().unwrap();
        assert_eq!(
            server.client_count(),
            1,
            "the first client is a mid-pass accept too — the poll set was built \
             before it existed, so it reaches this guard first"
        );

        // The second is accepted during a pass that polled only the first.
        let mut second = UnixStream::connect(&path).unwrap();
        server.pass().unwrap();
        assert_eq!(
            server.client_count(),
            2,
            "the client accepted after the poll set was built read the device's \
             slot, saw the device's hangup, and was dropped for it"
        );

        // Deferred, not forgotten: both authenticate on later passes.
        first.write_all(&auth_packet()).unwrap();
        second.write_all(&auth_packet()).unwrap();
        for _ in 0..4 {
            server.pass().unwrap();
        }
        assert_eq!(server.client_count(), 2);
        assert_eq!(first_command(&drain_socket(&mut first)), Some(command::REPLY));
        assert_eq!(
            first_command(&drain_socket(&mut second)),
            Some(command::REPLY),
            "the deferred client is read on a later pass, not dropped"
        );
        server.shutdown();
    }
}

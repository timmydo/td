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
use crate::proto::subscription;
use crate::session::{Disconnect, GlobalRequest, Session};
use crate::sink::{is_underrun, AudioSink, Spec};
use crate::sys::{self, Interest, PollSet};
use std::fmt;
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

/// The human seat user; deployment policy separately admits the audio service
/// and the immutable assigned Firefox UID (APPLICATIONS.md §K.5).
pub const SEAT_UID: u32 = 1000;

/// The most clients this daemon will hold at once.
///
/// Not politeness: every accepted connection costs a decoder buffer and a poll
/// slot, and a local process can call `connect(2)` in a loop. Refusing the
/// N+1st is a bounded failure; running out of descriptors takes the audio down
/// for everyone already playing.
pub const MAX_CLIENTS: usize = 32;

/// One kernel-authenticated process may not reserve the whole connection
/// table. Multiple Pulse contexts are ordinary; thirty-two from one pid are a
/// slot-exhaustion attack.
pub const MAX_CLIENTS_PER_PID: usize = 4;

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

/// A complete local protocol frame must arrive within this monotonic window.
/// The descriptor bounds memory, while this bounds how long a peer may reserve
/// that memory and an admission slot by trickling or abandoning its body.
pub const FRAME_DEADLINE: Duration = Duration::from_secs(5);

/// How long one wait may block. A period at 48 kHz is about 21 ms, so this is
/// generous enough to be a backstop rather than a poll rate.
pub const WAIT_MS: i32 = 100;

/// The most bytes read from one client per pass.
const READ_CHUNK: usize = 64 * 1024;

/// The one application whose account may connect.
const BROWSER: &str = "firefox";

/// What an admitted peer may reach beyond its own streams, fixed at accept
/// from its kernel uid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// The daemon's own uid and the seat user: every connection's streams
    /// are visible and controllable, as for a same-user Pulse mixer.
    Controller,
    /// An application account: only its own streams. Another connection's
    /// sink inputs are absent, never merely forbidden.
    Player,
}

/// Who may connect.
pub struct Policy {
    /// Dedicated audio and the human seat, the controller uids.
    pub allowed_uids: Vec<u32>,
    /// The deployment's application policy, which names the browser's
    /// account. Asked on each admission, so an account the ledger gains
    /// after the daemon started is admitted without a restart.
    applications: Option<crate::app_policy::Policy>,
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
        Self {
            allowed_uids,
            applications: None,
        }
    }

    pub fn for_deployment(own: u32) -> io::Result<Self> {
        Self::deployment_policy(
            own,
            std::fs::symlink_metadata(crate::app_policy::PATH).map(|_| ()),
            || crate::app_policy::load_session().map_err(io::Error::other),
        )
    }

    fn deployment_policy(
        own: u32,
        exists: io::Result<()>,
        load: impl FnOnce() -> io::Result<crate::app_policy::Policy>,
    ) -> io::Result<Self> {
        let mut policy = Self::for_uid(own);
        match exists {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                eprintln!("td-audio: no deployment policy; application peers are not admitted");
                return Ok(policy);
            }
            Err(error) => return Err(error),
            Ok(()) => {}
        }
        policy.applications = Some(load()?);
        Ok(policy)
    }

    /// The browser's account, when the deployment declares it and the
    /// ledger holds its row.
    fn browser(&self) -> Option<u32> {
        self.applications
            .as_ref()
            .and_then(|applications| applications.for_name(BROWSER))
            .map(|rule| rule.uid)
    }

    /// Every uid admitted now: the fixed pair and the browser's account.
    pub fn permitted(&self) -> Vec<u32> {
        let mut uids = self.allowed_uids.clone();
        if let Some(uid) = self
            .browser()
            .filter(|uid| !self.allowed_uids.contains(uid))
        {
            uids.push(uid);
        }
        uids
    }

    /// The role an admitted peer gets, or `None` for a refused one.
    ///
    /// The browser account is a player even if a ledger row ever named a
    /// controller uid for it: sharing an account must not promote an
    /// application, so the ambiguous case fails toward less authority.
    pub fn role(&self, peer: &sys::Peer) -> Option<Role> {
        if self.browser() == Some(peer.uid) {
            return Some(Role::Player);
        }
        self.allowed_uids
            .contains(&peer.uid)
            .then_some(Role::Controller)
    }

    /// Why a peer was refused, in words a log line can carry. §K.5 asks for a
    /// decision "in code that can say why it refused".
    pub fn refusal(&self, peer: &sys::Peer) -> String {
        let permitted = self.permitted();
        format!(
            "uid {} (pid {}) is not one of the {} permitted uids {:?}",
            peer.uid,
            peer.pid,
            permitted.len(),
            permitted
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
const MAX_PENDING: usize = crate::session::MAX_OUTPUT_BYTES;

/// Resident storage may carry at most one consumed window beside the owed
/// bytes. Crossing this threshold pays for one compaction only after roughly a
/// whole `MAX_PENDING` window was consumed, rather than copying a megabyte for
/// every one-byte read followed by a small query.
const MAX_PENDING_STORAGE: usize = 2 * MAX_PENDING;

/// Attacker-driven diagnostics retained over the daemon's whole lifetime.
/// They are printed only after the audio loop stops, so even the first line
/// cannot block that loop on an undrained stderr pipe.
const MAX_DAEMON_DIAGNOSTICS: usize = 16;

/// Tiny packets, not bytes, are the CPU/fan-out currency. One client gets at
/// most this many frames and all clients share the second bound per pass.
const MAX_FRAMES_PER_CLIENT_PASS: usize = 32;
const MAX_FRAMES_PER_PASS: usize = 256;

/// The applications a connection can be a player for, each with a level of
/// its own that the daemon keeps (§K.5) so it outlives the application's
/// streams and connections. `Policy::role` makes only the browser's account
/// a player, so this is the browser alone; a later player application is a
/// row here, which keeps the table fixed and allocation-free.
const PLAYER_APPLICATIONS: &[&str] = &[BROWSER];

/// Unity and unmuted: an application the human has not turned down.
const UNITY_LEVEL: (u32, bool) = (crate::mixer::VOLUME_NORM, false);

/// Two distinct clients, mutably: the requester and the owner of what it
/// asked about.
fn pair_mut(
    clients: &mut [Client],
    first: usize,
    second: usize,
) -> Option<(&mut Client, &mut Client)> {
    if first == second || first.max(second) >= clients.len() {
        return None;
    }
    if first < second {
        let (left, right) = clients.split_at_mut(second);
        Some((left.get_mut(first)?, right.first_mut()?))
    } else {
        let (left, right) = clients.split_at_mut(first);
        let second = left.get_mut(second)?;
        Some((right.first_mut()?, second))
    }
}

/// One connected client.
struct Client {
    stream: UnixStream,
    session: Session,
    peer: sys::Peer,
    /// What it may reach beyond its own streams.
    role: Role,
    /// Bytes owed to the client that the socket would not take yet.
    pending: Vec<u8>,
    /// How far into `pending` the socket has got.
    pending_at: usize,
    /// When it connected, for `AUTH_DEADLINE`.
    since: Instant,
    /// When the current incomplete frame first reserved decoder bytes.
    partial_since: Option<Instant>,
    /// This fd was readable in the current poll snapshot, or it carries
    /// decoder work deferred from the preceding pass. Admission may not call
    /// it idle before that work reaches the session and router.
    ready_input: bool,
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

    fn evictable_idle(&self) -> bool {
        self.session.stream_count() == 0
            && !self.session.holds_idle_control_state()
            && !self.ready_input
            && !self.session.has_incomplete_input()
            && !self.session.has_output()
            && self.owed() == 0
    }

    fn update_partial_deadline(&mut self) {
        if self.session.has_incomplete_input() {
            if self.partial_since.is_none() {
                self.partial_since = Some(Instant::now());
            }
        } else {
            self.partial_since = None;
        }
    }

    /// Move whatever the session produced into the outgoing buffer.
    ///
    /// Compacting here rather than draining from the front keeps this a copy
    /// per flush instead of a memmove per write.
    fn collect(&mut self) {
        if self.session.has_output() {
            let bytes = self.session.take_output();
            if self.pending_at >= self.pending.len() {
                self.pending.clear();
                self.pending_at = 0;
            } else if self.pending_at > 0
                && self.pending.len().saturating_add(bytes.len()) > MAX_PENDING_STORAGE
            {
                let consumed = self.pending_at.min(self.pending.len());
                self.pending.copy_within(consumed.., 0);
                self.pending
                    .truncate(self.pending.len().saturating_sub(consumed));
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
    /// The device went away. Clients get a best-effort KILLED event or EOF.
    DeviceGone,
    /// The run limit was reached, which only a test sets.
    Finished,
}

/// The daemon.
pub struct Server<S: AudioSink> {
    listener: UnixListener,
    path: PathBuf,
    clients: Vec<Client>,
    /// The human's level per player application, by `PLAYER_APPLICATIONS`
    /// position.
    app_levels: [(u32, bool); PLAYER_APPLICATIONS.len()],
    mixer: Mixer,
    sink: S,
    policy: Policy,
    spec: Spec,
    poll: PollSet,
    next_client_index: u64,
    read_cursor: usize,
    /// Peers turned away, so a refusal is visible without a log scrape.
    pub refused: u32,
    /// Clients dropped for a protocol error, ditto.
    pub disconnected: u32,
    diagnostics: Vec<String>,
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
        // Keep one transfer of target headroom beyond the hardware ring. Pulse's
        // default prebuffer threshold is `target + 1 - minreq`, so a target
        // equal to the ring can start with no software frames left for the
        // next refill. The extra period leaves the captured Firefox stream's
        // bounded `period + 1 - minreq` reserve. That protects HDA from one
        // scheduling delay immediately after START under the one-vCPU TCG
        // proof.
        let target_floor_frames = sink.buffer_frames().saturating_add(sink.period_frames());
        Ok(Self {
            listener,
            path: path.to_path_buf(),
            clients: Vec::new(),
            app_levels: [UNITY_LEVEL; PLAYER_APPLICATIONS.len()],
            mixer: Mixer::with_target_floor(spec, target_floor_frames),
            sink,
            policy,
            spec,
            poll: PollSet::new(),
            next_client_index: 0,
            read_cursor: 0,
            refused: 0,
            disconnected: 0,
            diagnostics: Vec::new(),
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
    pub fn policy_uids(&self) -> Vec<u32> {
        self.policy.permitted()
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
        // Order the device clock before commands delivered by this poll wake.
        // A CORK/UNCORK or DRAIN cannot retroactively change whether an
        // already-crossed stream endpoint was intentional.
        if self.observe_device()? {
            self.finish_device_gone();
            return Ok(true);
        }
        self.read_clients();
        // Consume the poll snapshot before accepting. A full-table admission
        // may evict an idle client and shift the vector; doing that first
        // would apply an old fd's readiness to whichever client shifted into
        // its slot.
        self.accept();
        self.route_global_requests();
        self.broadcast_session_events();
        self.drop_output_overflows();
        let gone = self.drive_device()?;
        self.service_clients();
        self.write_clients();
        if gone {
            self.finish_device_gone();
        }
        Ok(gone)
    }

    fn finish_device_gone(&mut self) {
        // Queue KILLED and make one nonblocking delivery attempt before
        // close. A backpressured peer observes EOF instead; device loss
        // cannot block the single audio thread waiting for it to read.
        for client in &mut self.clients {
            client.session.kill_all_streams();
            client.collect();
            client.flush();
        }
        self.clients.clear();
        self.forget_orphaned_streams();
    }

    fn wait(&mut self) -> io::Result<()> {
        self.poll.clear();
        self.poll.push(self.listener.as_raw_fd(), Interest::READ)?;
        for client in &self.clients {
            let interest = if client.wants_write() {
                Interest::BOTH
            } else {
                Interest::READ
            };
            self.poll.push(client.fd(), interest)?;
        }
        if self
            .mixer
            .has_device_work(self.sink.is_running(), self.sink.period_frames())
        {
            if let Some(fd) = self.sink.raw_fd() {
                self.poll.push(fd, Interest::WRITE)?;
            }
        }
        let deferred = self
            .clients
            .iter()
            .any(|client| client.session.input_deferred());
        self.poll.wait(if deferred { 0 } else { WAIT_MS })?;
        Ok(())
    }

    fn accept(&mut self) {
        if !self.poll.readiness(0).readable {
            return;
        }
        // One full-table replacement per snapshot admits progress without
        // letting one listener wake churn every old idle context or the
        // newcomer that this same wake just admitted.
        let mut replaced_idle = false;
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
            self.admit(stream, peer, &mut replaced_idle);
        }
    }

    /// Give an accepted connection, whose kernel credentials have been read,
    /// its role and a slot, or refuse it. The role is fixed here for the
    /// connection's life.
    fn admit(&mut self, stream: UnixStream, peer: sys::Peer, replaced_idle: &mut bool) {
        let Some(role) = self.policy.role(&peer) else {
            // §K.5 asks for the decision to be "in code that can say why it
            // refused", so it says so.
            diagnostic(
                &mut self.diagnostics,
                format_args!("refused a connection: {}", self.policy.refusal(&peer)),
            );
            self.refused = self.refused.saturating_add(1);
            return;
        };
        if self
            .clients
            .iter()
            .filter(|client| client.peer.pid == peer.pid)
            .count()
            >= MAX_CLIENTS_PER_PID
        {
            diagnostic(
                &mut self.diagnostics,
                format_args!(
                    "refused uid {} pid {}: already serving its \
                     {MAX_CLIENTS_PER_PID} client limit",
                    peer.uid, peer.pid
                ),
            );
            self.refused = self.refused.saturating_add(1);
            return;
        }
        if self.clients.len() >= MAX_CLIENTS {
            let idle = if *replaced_idle {
                None
            } else {
                self.clients.iter().position(Client::evictable_idle)
            };
            if let Some(idle) = idle {
                self.drop_clients(&[idle]);
                *replaced_idle = true;
            } else {
                diagnostic(
                    &mut self.diagnostics,
                    format_args!(
                        "refused uid {} (pid {}): already serving {MAX_CLIENTS} active clients",
                        peer.uid, peer.pid
                    ),
                );
                self.refused = self.refused.saturating_add(1);
                return;
            }
        }
        let Some(index) = self.allocate_client_index() else {
            self.refused = self.refused.saturating_add(1);
            return;
        };
        let mut session =
            Session::new(self.spec, index).with_controller_role(role == Role::Controller);
        if role == Role::Player {
            // The role and the name come from the same policy row: the only
            // player today is the browser's account.
            session = session.with_application(BROWSER);
            session.set_app_level(self.app_level(BROWSER), &mut self.mixer);
        }
        self.clients.push(Client {
            stream,
            session,
            peer,
            role,
            pending: Vec::new(),
            pending_at: 0,
            since: Instant::now(),
            partial_since: None,
            ready_input: false,
        });
    }

    fn allocate_client_index(&mut self) -> Option<u32> {
        if self.next_client_index >= u64::from(crate::tag::INVALID_INDEX) {
            return None;
        }
        let candidate = u32::try_from(self.next_client_index).ok()?;
        self.next_client_index = self.next_client_index.saturating_add(1);
        self.clients
            .iter()
            .all(|client| client.session.client_index() != candidate)
            .then_some(candidate)
    }

    fn broadcast_session_events(&mut self) {
        let mut events = Vec::new();
        for (origin, client) in self.clients.iter_mut().enumerate() {
            for (event, index) in client.session.take_global_events() {
                events.push((origin, event, index));
            }
        }
        for (origin, event, index) in events {
            self.notify_controllers(Some(origin), event, index);
        }
    }

    /// The one fan-out of another connection's sink-input or sink event. Only
    /// controllers hear it: a player learns of no stream but its own, whose
    /// events its session already delivered locally, and of no change to the
    /// output, which in its view never changes.
    fn notify_controllers(&mut self, origin: Option<usize>, event: u32, index: u32) {
        for (recipient, client) in self.clients.iter_mut().enumerate() {
            if Some(recipient) != origin && client.role == Role::Controller {
                client.session.notify_global(event, index);
            }
        }
    }

    fn route_global_requests(&mut self) {
        let mut requests = Vec::new();
        for (requester, client) in self.clients.iter_mut().enumerate() {
            for request in client.session.take_global_requests() {
                requests.push((requester, request));
            }
        }
        for (requester, request) in requests {
            // Only a controller's session raises the list requests, and a
            // player's own streams never reach this router, so whatever a
            // player names here is another connection's or nobody's, and gets
            // the same answer either way: an index that exists is not
            // revealed.
            let controller = self
                .clients
                .get(requester)
                .is_some_and(|client| client.role == Role::Controller);
            if !controller {
                if let Some(client) = self.clients.get_mut(requester) {
                    client.session.reject_global_request(request);
                }
                continue;
            }
            match request {
                GlobalRequest::SinkInputList { tag, version } => {
                    self.reply_sink_input_list(requester, tag, version);
                }
                GlobalRequest::ClientList { tag, version } => {
                    self.reply_clients(requester, tag, version, None);
                }
                GlobalRequest::ClientInfo {
                    tag,
                    index,
                    version,
                } => {
                    self.reply_clients(requester, tag, version, Some(index));
                }
                stream_request => self.route_stream_request(requester, stream_request),
            }
        }
    }

    /// Every connection's streams, in one reply to a controller. A player's
    /// are shown at its application's level, which is the one a controller
    /// sets.
    fn reply_sink_input_list(&mut self, requester: usize, tag: u32, version: u32) {
        let mut writer = crate::tag::Writer::new();
        writer.u32(crate::proto::command::REPLY).u32(tag);
        for client in &self.clients {
            let level = (client.role == Role::Player).then(|| client.session.app_level());
            client
                .session
                .write_sink_inputs(&mut writer, version, &self.mixer, level);
        }
        if let Some(client) = self.clients.get_mut(requester) {
            client.session.deliver(tag, writer.as_bytes());
        }
    }

    /// Every connection, or the one whose client index is `only`, in one
    /// reply to a controller.
    fn reply_clients(&mut self, requester: usize, tag: u32, version: u32, only: Option<u32>) {
        let mut writer = crate::tag::Writer::new();
        writer.u32(crate::proto::command::REPLY).u32(tag);
        let mut found = false;
        for client in &self.clients {
            if only.is_none_or(|index| client.session.client_index() == index) {
                client.session.write_client(&mut writer, version);
                found = true;
            }
        }
        let Some(client) = self.clients.get_mut(requester) else {
            return;
        };
        if only.is_some() && !found {
            client.session.fail(tag, crate::proto::error::NOENTITY);
            return;
        }
        client.session.deliver(tag, writer.as_bytes());
    }

    /// A controller's request about one stream another connection owns.
    ///
    /// A controller's write to a controller's stream lands in that stream,
    /// as it always has. One to a PLAYER's stream sets the human's level for
    /// the player's application instead: a gain of its own that every stream
    /// of that application plays under, that the application cannot see or
    /// rewrite, and that its next stream starts at. A lookup of a player's
    /// stream reports that level, the one the controller sets.
    fn route_stream_request(&mut self, requester: usize, request: GlobalRequest) {
        let index = match &request {
            GlobalRequest::Info { index, .. }
            | GlobalRequest::Volume { index, .. }
            | GlobalRequest::Mute { index, .. } => *index,
            GlobalRequest::SinkInputList { .. }
            | GlobalRequest::ClientInfo { .. }
            | GlobalRequest::ClientList { .. } => {
                if let Some(client) = self.clients.get_mut(requester) {
                    client.session.reject_global_request(request);
                }
                return;
            }
        };
        let owner = self
            .clients
            .iter()
            .position(|client| client.session.owns_sink_input(index))
            .filter(|owner| *owner != requester);
        let Some(owner) = owner else {
            if let Some(client) = self.clients.get_mut(requester) {
                client.session.reject_global_request(request);
            }
            return;
        };
        let player = self
            .clients
            .get(owner)
            .is_some_and(|client| client.role == Role::Player);
        match request {
            GlobalRequest::Volume { tag, volumes, .. } if player => {
                let volume = volumes
                    .iter()
                    .copied()
                    .max()
                    .unwrap_or(crate::mixer::VOLUME_NORM);
                self.set_app_level(requester, owner, tag, |level| (volume, level.1));
            }
            GlobalRequest::Mute { tag, muted, .. } if player => {
                self.set_app_level(requester, owner, tag, |level| (level.0, muted));
            }
            GlobalRequest::Info {
                tag,
                index,
                version,
            } if player => {
                if let Some((requester, owner)) = pair_mut(&mut self.clients, requester, owner) {
                    let level = Some(owner.session.app_level());
                    requester.session.describe_foreign(
                        &owner.session,
                        tag,
                        index,
                        version,
                        &self.mixer,
                        level,
                    );
                }
            }
            request => {
                if let Some((requester, owner)) = pair_mut(&mut self.clients, requester, owner) {
                    requester.session.complete_global_request(
                        &mut owner.session,
                        request,
                        &mut self.mixer,
                    );
                }
            }
        }
    }

    /// The human's level for `application`, unity until one is set.
    fn app_level(&self, application: &str) -> (u32, bool) {
        PLAYER_APPLICATIONS
            .iter()
            .position(|name| *name == application)
            .and_then(|at| self.app_levels.get(at).copied())
            .unwrap_or(UNITY_LEVEL)
    }

    /// Change the human's level for the application `owner` runs: on every
    /// connection of that application, and remembered for its later ones.
    /// Then acknowledge the requester and, when the level changed, tell every
    /// controller that each of the application's streams did. The player is
    /// told nothing: in its view nothing changed.
    fn set_app_level(
        &mut self,
        requester: usize,
        owner: usize,
        tag: u32,
        change: impl FnOnce((u32, bool)) -> (u32, bool),
    ) {
        let found = self.clients.get(owner).and_then(|client| {
            PLAYER_APPLICATIONS
                .iter()
                .position(|name| *name == client.session.application())
        });
        let (Some(at), Some(application)) = (
            found,
            found.and_then(|at| PLAYER_APPLICATIONS.get(at).copied()),
        ) else {
            // A player is named from this table at admission, so a player
            // whose application is not in it is a daemon fault.
            if let Some(client) = self.clients.get_mut(requester) {
                client.session.fail(tag, crate::proto::error::INTERNAL);
            }
            return;
        };
        let Some(slot) = self.app_levels.get_mut(at) else {
            return;
        };
        let before = *slot;
        let (volume, muted) = change(before);
        let level = (volume.min(crate::mixer::VOLUME_MAX), muted);
        *slot = level;
        let is_application = |client: &Client| {
            client.role == Role::Player && client.session.application() == application
        };
        for client in self.clients.iter_mut() {
            if is_application(client) {
                client.session.set_app_level(level, &mut self.mixer);
            }
        }
        if let Some(client) = self.clients.get_mut(requester) {
            client.session.acknowledge(tag);
        }
        if level == before {
            return;
        }
        for at in 0..self.clients.len() {
            let indexes = match self.clients.get(at) {
                Some(client) if is_application(client) => client.session.sink_input_indexes(),
                _ => continue,
            };
            for index in indexes {
                self.notify_controllers(
                    None,
                    subscription::EVENT_SINK_INPUT | subscription::EVENT_CHANGE,
                    index,
                );
            }
        }
    }

    fn read_clients(&mut self) {
        let mut buffer = [0u8; READ_CHUNK];
        let mut drop_indexes: Vec<usize> = Vec::new();
        let mut frames_left = MAX_FRAMES_PER_PASS;
        let client_count = self.clients.len();
        let start = self.read_cursor % client_count.max(1);
        let mut visited = 0usize;
        for index in 0..client_count {
            let readiness = self.poll.readiness(index.saturating_add(1));
            if let Some(client) = self.clients.get_mut(index) {
                client.ready_input = readiness.readable || client.session.input_deferred();
            }
        }
        for offset in 0..client_count {
            if frames_left == 0 {
                break;
            }
            let index = start.saturating_add(offset) % client_count.max(1);
            visited = visited.saturating_add(1);
            let Some(client) = self.clients.get_mut(index) else {
                continue;
            };
            // Slot 0 is the listener, so client `index` is poll slot
            // `index + 1`.
            let readiness = self.poll.readiness(index.saturating_add(1));
            if readiness.gone && !readiness.readable && !client.session.input_deferred() {
                drop_indexes.push(index);
                continue;
            }
            let per_client = frames_left.min(MAX_FRAMES_PER_CLIENT_PASS);
            if client.session.input_deferred() {
                match client
                    .session
                    .feed_limited(&[], &mut self.mixer, per_client)
                {
                    Ok(processed) => {
                        frames_left = frames_left.saturating_sub(processed);
                        client.update_partial_deadline();
                    }
                    Err(reason) => {
                        // The failing frame can follow every allowed frame in
                        // this turn. Charge the whole slice when the decoder
                        // cannot return a precise successful count.
                        frames_left = frames_left.saturating_sub(per_client);
                        report_disconnect(
                            &mut self.diagnostics,
                            &client.peer,
                            &reason,
                            client.session.stream_count(),
                        );
                        drop_indexes.push(index);
                    }
                }
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
                    if let Err(reason) = client
                        .session
                        .feed_limited(bytes, &mut self.mixer, per_client)
                        .map(|processed| {
                            frames_left = frames_left.saturating_sub(processed);
                        })
                    {
                        frames_left = frames_left.saturating_sub(per_client);
                        // A protocol error is not survivable: the stream is out
                        // of frame and every later byte would be read at the
                        // wrong offset. §K.3's schemas exist to detect exactly
                        // this, and detecting it means hanging up.
                        report_disconnect(
                            &mut self.diagnostics,
                            &client.peer,
                            &reason,
                            client.session.stream_count(),
                        );
                        drop_indexes.push(index);
                    } else {
                        client.update_partial_deadline();
                    }
                    if !drop_indexes.contains(&index) && before.is_none() {
                        if let Some(version) = client.session.version() {
                            diagnostic(
                                &mut self.diagnostics,
                                format_args!(
                                    "uid {} (pid {}) authenticated at protocol {version}",
                                    client.peer.uid, client.peer.pid
                                ),
                            );
                        }
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => drop_indexes.push(index),
            }
        }
        if client_count > 0 {
            self.read_cursor = start.saturating_add(visited) % client_count;
        }
        self.drop_clients(&drop_indexes);
    }

    fn drop_output_overflows(&mut self) {
        let indexes: Vec<usize> = self
            .clients
            .iter()
            .enumerate()
            .filter_map(|(index, client)| client.session.output_overflowed().then_some(index))
            .collect();
        self.drop_clients(&indexes);
    }

    /// Settle the device timeline before applying this wake's client commands.
    /// `true` means the device is gone.
    fn observe_device(&mut self) -> io::Result<bool> {
        if !self.mixer.sink_is_running() {
            return Ok(false);
        }
        match self.sink.wait(0)? {
            crate::sink::Wait::Gone => Ok(true),
            crate::sink::Wait::Underrun => {
                self.mixer.recover(&mut self.sink)?;
                Ok(false)
            }
            crate::sink::Wait::Writable | crate::sink::Wait::Timeout => {
                match self.mixer.observe_playhead(&mut self.sink) {
                    Ok(()) => Ok(false),
                    Err(error) if is_underrun(&error) => {
                        self.mixer.recover(&mut self.sink)?;
                        Ok(false)
                    }
                    Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(true),
                    Err(error) => Err(error),
                }
            }
        }
    }

    /// Hand at most one period of real audio to the device.
    /// `true` means the device is gone.
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
                // rule below — prime the ring, or accept an explicitly
                // released finite tail — is the same rule, and a later pass
                // reaches it.
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
                // rule below — prime the ring, or accept an explicitly
                // released finite tail — is the same rule, and a later pass
                // reaches it.
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
            self.mixer.note_started();
        }
        Ok(false)
    }

    fn service_clients(&mut self) {
        let now = now_usec();
        let mut drop_indexes: Vec<usize> = Vec::new();
        for (index, client) in self.clients.iter_mut().enumerate() {
            client.session.tick(now);
            client.session.service(&mut self.mixer);
            client.collect();
            if client.session.version().is_none() && client.since.elapsed() > AUTH_DEADLINE {
                diagnostic(
                    &mut self.diagnostics,
                    format_args!(
                        "hung up on uid {} pid {}: connected and never authenticated",
                        client.peer.uid, client.peer.pid
                    ),
                );
                drop_indexes.push(index);
                continue;
            }
            if client
                .partial_since
                .is_some_and(|since| since.elapsed() > FRAME_DEADLINE)
            {
                diagnostic(
                    &mut self.diagnostics,
                    format_args!(
                        "hung up on uid {} pid {}: an incomplete protocol frame exceeded \
                         the {FRAME_DEADLINE:?} deadline",
                        client.peer.uid, client.peer.pid
                    ),
                );
                drop_indexes.push(index);
                continue;
            }
            if client.owed() > MAX_PENDING {
                diagnostic(
                    &mut self.diagnostics,
                    format_args!(
                        "hung up on uid {} pid {}: {} bytes owed and not reading",
                        client.peer.uid,
                        client.peer.pid,
                        client.owed()
                    ),
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
        let mut removed_sink_inputs = Vec::new();
        for index in sorted.into_iter().rev() {
            if index >= self.clients.len() {
                continue;
            }
            let mut removed = self.clients.remove(index);
            // A change it made this pass outlives it — the output's level stays
            // where it set it — so the event announcing it is relayed before
            // the client goes, not discarded with it.
            for (event, sink_index) in removed.session.take_global_events() {
                self.notify_controllers(None, event, sink_index);
            }
            removed_sink_inputs.extend(removed.session.sink_input_indexes());
            self.disconnected = self.disconnected.saturating_add(1);
        }
        // And every stream those clients had is the mixer's to forget. A client
        // that vanishes mid-stream leaves audio queued, and audio nobody owns
        // would keep playing. Dropping the client does not do this: the mixer
        // is a separate table, reconciled here against the sessions that remain.
        self.forget_orphaned_streams();
        for index in removed_sink_inputs {
            self.notify_controllers(
                None,
                subscription::EVENT_SINK_INPUT | subscription::EVENT_REMOVE,
                index,
            );
        }
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
        for diagnostic in &self.diagnostics {
            eprintln!("td-audio: {diagnostic}");
        }
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
fn diagnostic(diagnostics: &mut Vec<String>, message: fmt::Arguments<'_>) {
    if diagnostics.len() >= MAX_DAEMON_DIAGNOSTICS {
        return;
    }
    diagnostics.push(message.to_string());
}

fn report_disconnect(
    diagnostics: &mut Vec<String>,
    peer: &sys::Peer,
    reason: &Disconnect,
    streams: usize,
) {
    diagnostic(
        diagnostics,
        format_args!(
            "hung up on uid {} pid {} with {streams} stream(s): {reason}",
            peer.uid, peer.pid
        ),
    );
}

#[cfg(test)]
pub(crate) mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::proto::command;
    use crate::sink::{AudioSink, MemorySink, Wait};
    use crate::tag;
    use crate::wire;

    fn admits(policy: &Policy, peer: &sys::Peer) -> bool {
        policy.role(peer).is_some()
    }

    impl<S: AudioSink> Server<S> {
        /// The output's level, and the first player's application level.
        pub(crate) fn levels(&self) -> (u32, Option<(u32, bool)>) {
            let app = self
                .clients
                .iter()
                .find(|client| client.role == Role::Player)
                .map(|client| client.session.app_level());
            (self.mixer.master_volume(), app)
        }
    }

    pub(crate) fn socket_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("td-audio-{}-{}", name, std::process::id()))
    }

    fn server(name: &str) -> (Server<MemorySink>, PathBuf) {
        let path = socket_path(name);
        let _ = std::fs::remove_file(&path);
        // Match the production default: eight 1,024-frame periods.  A larger
        // synthetic ring would require correspondingly more client data
        // before the target-floor invariant permits START.
        let mut sink = MemorySink::new(Spec::fixed(), 8_192, 1_024);
        sink.start().unwrap();
        let uid = own_uid();
        let server = Server::bind(&path, sink, Policy::for_uid(uid)).unwrap();
        (server, path)
    }

    #[test]
    fn production_sessions_keep_one_period_behind_the_selected_device_ring() {
        let (mut server, path) = server("target-floor");
        assert_eq!(server.sink.buffer_frames(), 8_192);
        assert_eq!(server.sink.period_frames(), 1_024);
        assert_eq!(server.mixer.target_floor_frames(), 9_216);
        server.shutdown();
        assert!(!path.exists());
    }

    /// A stopped sink whose descriptor is nevertheless always writable, which
    /// is the state a prepared ALSA playback PCM presents before real audio.
    struct WritableIdleSink {
        inner: MemorySink,
        ready: UnixStream,
        _peer: UnixStream,
    }

    impl WritableIdleSink {
        fn new() -> Self {
            let (ready, peer) = UnixStream::pair().unwrap();
            Self {
                inner: MemorySink::fixed(),
                ready,
                _peer: peer,
            }
        }
    }

    impl AudioSink for WritableIdleSink {
        fn spec(&self) -> Spec {
            self.inner.spec()
        }

        fn device_delay(&mut self) -> io::Result<u64> {
            self.inner.device_delay()
        }

        fn wait(&mut self, timeout_ms: i32) -> io::Result<Wait> {
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
            Some(self.ready.as_raw_fd())
        }

        fn is_running(&self) -> bool {
            self.inner.is_running()
        }
    }

    #[test]
    fn an_idle_writable_pcm_does_not_spin_the_event_loop() {
        let path = socket_path("idle-writable");
        let _ = std::fs::remove_file(&path);
        let uid = own_uid();
        let mut server =
            Server::bind(&path, WritableIdleSink::new(), Policy::for_uid(uid)).unwrap();
        let began = Instant::now();
        assert_eq!(server.run(Some(2)).unwrap(), Stopped::Finished);
        assert!(
            began.elapsed() >= Duration::from_millis(WAIT_MS as u64),
            "the permanently writable PCM bypassed the idle poll timeout"
        );
        server.shutdown();
    }

    #[test]
    fn each_poll_wake_observes_the_device_before_client_commands() {
        let source = include_str!("serve.rs");
        let start = source.find("pub fn pass(&mut self)").unwrap();
        let end = source
            .get(start..)
            .unwrap()
            .find("fn finish_device_gone")
            .unwrap();
        let pass = source.get(start..start + end).unwrap();
        let observe = pass.find("self.observe_device()?").unwrap();
        let commands = pass.find("self.read_clients();").unwrap();
        assert!(observe < commands);
    }

    /// This test process's uid, read from `/proc/self/status` so the test needs
    /// no syscall of its own.
    pub(crate) fn own_uid() -> u32 {
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

    pub(crate) fn build(
        command: u32,
        tag_value: u32,
        body: impl FnOnce(&mut tag::Writer),
    ) -> Vec<u8> {
        let mut writer = tag::Writer::new();
        writer.u32(command).u32(tag_value);
        body(&mut writer);
        control(&writer.into_bytes())
    }

    pub(crate) fn auth_packet() -> Vec<u8> {
        build(command::AUTH, 0, |writer| {
            writer
                .u32(35)
                .arbitrary(&[0u8; crate::session::AUTH_COOKIE_LEN]);
        })
    }

    /// Read whatever the server has written, without blocking forever.
    pub(crate) fn drain_socket(stream: &mut UnixStream) -> Vec<u8> {
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
        control_packets(bytes)
            .first()
            .and_then(|packet| wire::command_and_tag(packet).ok().map(|(c, _)| c))
    }

    fn control_packets(bytes: &[u8]) -> Vec<Vec<u8>> {
        let mut decoder = wire::Decoder::new();
        decoder.push(bytes);
        let mut packets = Vec::new();
        while let Some(frame) = decoder.next_frame() {
            match frame {
                Ok(wire::Frame::Control(packet)) => packets.push(packet),
                Ok(_) => {}
                // A frame td-pulse refuses, such as one over `CONTROL_MAX`, is
                // a failure to report, not one to retry forever.
                Err(error) => panic!("the server sent a frame td-pulse refuses: {error:?}"),
            }
        }
        packets
    }

    pub(crate) fn created_sink_input(bytes: &[u8]) -> Option<u32> {
        let mut decoder = wire::Decoder::new();
        decoder.push(bytes);
        while let Some(frame) = decoder.next_frame() {
            let Ok(wire::Frame::Control(packet)) = frame else {
                continue;
            };
            let mut reader = tag::Reader::new(&packet);
            if reader.u32().ok()? != command::REPLY {
                continue;
            }
            let _tag = reader.u32().ok()?;
            let _channel = reader.u32().ok()?;
            return reader.u32().ok();
        }
        None
    }

    fn subscription_events(bytes: &[u8]) -> Vec<(u32, u32)> {
        control_packets(bytes)
            .into_iter()
            .filter_map(|packet| {
                let mut reader = tag::Reader::new(&packet);
                if reader.u32().ok()? != command::SUBSCRIBE_EVENT
                    || reader.u32().ok()? != tag::INVALID_INDEX
                {
                    return None;
                }
                let event = reader.u32().ok()?;
                let index = reader.u32().ok()?;
                reader.finish().ok()?;
                Some((event, index))
            })
            .collect()
    }

    fn sink_input_state(bytes: &[u8], expected_tag: u32) -> Option<(u32, u32, Vec<u32>, bool)> {
        for packet in control_packets(bytes) {
            let mut reader = tag::Reader::new(&packet);
            if reader.u32().ok()? != command::REPLY || reader.u32().ok()? != expected_tag {
                continue;
            }
            let index = reader.u32().ok()?;
            let _name = reader.string().ok()?;
            let _module = reader.u32().ok()?;
            let client = reader.u32().ok()?;
            let _sink = reader.u32().ok()?;
            let _spec = reader.sample_spec().ok()?;
            let _map = reader.channel_map().ok()?;
            let volume = reader.cvolume().ok()?;
            let _queued = reader.usec().ok()?;
            let _device = reader.usec().ok()?;
            let _resample = reader.string().ok()?;
            let _driver = reader.string().ok()?;
            let muted = reader.boolean().ok()?;
            return Some((index, client, volume, muted));
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

    #[test]
    fn a_readiness_probe_does_not_start_or_break_the_pcm() {
        let path = socket_path("probe");
        let _ = std::fs::remove_file(&path);
        let sink = MemorySink::fixed();
        let uid = own_uid();
        let mut server = Server::bind(&path, sink, Policy::for_uid(uid)).unwrap();
        let probe = UnixStream::connect(&path).unwrap();
        server.pass().unwrap();
        drop(probe);
        server.pass().unwrap();

        assert_eq!(server.client_count(), 0);
        assert_eq!(server.stream_count(), 0);
        assert_eq!(server.sink.frames_written(), 0);
        assert!(!server.sink.is_running());
        server.shutdown();
    }

    /// A large readable socket gets one bounded batch while another readable
    /// client gets its own batch in the same pass. The retained complete
    /// frames force a zero-timeout next pass instead of blocking for new I/O.
    #[test]
    fn one_pass_bounds_each_clients_control_work() {
        let (mut server, path) = server("control-fairness");
        let mut first = UnixStream::connect(&path).unwrap();
        let mut second = UnixStream::connect(&path).unwrap();
        first.write_all(&auth_packet()).unwrap();
        second.write_all(&auth_packet()).unwrap();
        server.pass().unwrap();
        server.pass().unwrap();
        let _ = drain_socket(&mut first);
        let _ = drain_socket(&mut second);

        let mut questions = Vec::new();
        for tag_value in 0..64 {
            questions.extend(build(command::GET_SERVER_INFO, tag_value, |_| {}));
        }
        first.write_all(&questions).unwrap();
        second.write_all(&questions).unwrap();
        server.pass().unwrap();

        assert_eq!(control_packets(&drain_socket(&mut first)).len(), 32);
        assert_eq!(control_packets(&drain_socket(&mut second)).len(), 32);
        assert!(server
            .clients
            .iter()
            .all(|client| client.session.input_deferred()));
        assert!(server
            .clients
            .iter()
            .all(|client| !client.session.output_overflowed()));
        server.shutdown();
    }

    /// The global frame budget charges work performed, not each client's full
    /// allowance. More than eight one-frame clients therefore all make
    /// progress in one pass; charging 32 apiece would stop after the eighth.
    #[test]
    fn one_frame_clients_share_the_actual_work_budget() {
        let (mut server, path) = server("actual-frame-budget");
        let mut clients = Vec::new();
        for batch in 0..3 {
            let before = server.client_count();
            for _ in 0..MAX_CLIENTS_PER_PID {
                let mut client = UnixStream::connect(&path).unwrap();
                client.write_all(&auth_packet()).unwrap();
                clients.push(client);
            }
            server.pass().unwrap();
            for held in server.clients.iter_mut().skip(before) {
                held.peer.pid = 30_000 + batch;
            }
            server.pass().unwrap();
        }
        for client in &mut clients {
            let _ = drain_socket(client);
            client
                .write_all(&build(command::GET_SERVER_INFO, 50, |_| {}))
                .unwrap();
        }
        server.pass().unwrap();
        assert!(clients.iter_mut().all(|client| {
            control_packets(&drain_socket(client))
                .iter()
                .any(|packet| wire::command_and_tag(packet) == Ok((command::REPLY, 50)))
        }));
        server.shutdown();
    }

    /// A partial frame discovered only after the bounded complete-frame batch
    /// still gets its own progress deadline.
    #[test]
    fn deferred_complete_frames_cannot_hide_an_abandoned_partial_frame() {
        let (mut server, path) = server("deferred-partial-frame");
        let mut client = UnixStream::connect(&path).unwrap();
        client.write_all(&auth_packet()).unwrap();
        server.pass().unwrap();
        server.pass().unwrap();
        let _ = drain_socket(&mut client);

        let mut input = Vec::new();
        for tag_value in 0..33 {
            input.extend(build(command::GET_SERVER_INFO, tag_value, |_| {}));
        }
        input.extend(wire::Descriptor::encode(1024, 0, 0, 0));
        client.write_all(&input).unwrap();
        server.pass().unwrap();
        assert!(server.clients.first().unwrap().session.input_deferred());
        assert!(server.clients.first().unwrap().partial_since.is_none());

        server.pass().unwrap();
        let held = server.clients.first().unwrap();
        assert!(!held.session.input_deferred());
        assert!(held.session.has_incomplete_input());
        assert!(
            held.partial_since.is_some(),
            "the deferred branch exposed a partial frame without arming it"
        );
        server.shutdown();
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
        assert_eq!(
            first_command(&drain_socket(&mut client)),
            Some(command::REPLY)
        );

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
        assert_eq!(
            first_command(&drain_socket(&mut client)),
            Some(command::REPLY)
        );

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
            samples
                .get(..64)
                .is_some_and(|run| run.iter().all(|s| *s == 900)),
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
        assert_eq!(
            server.stream_count(),
            0,
            "the mixer still holds a dead stream"
        );
        server.shutdown();
    }

    /// Recovering from an underrun does not start the device on an empty ring.
    ///
    /// `PREPARE` discards the ring, and `start_threshold` is the boundary, so a
    /// `START` with nothing queued runs the device on silence and underruns
    /// again at the next pointer update — a prepare/start/XRUN churn that never
    /// converges. The rule below starts only after the ring is fully primed or
    /// every accepted contribution is an explicitly released finite tail.
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
        server.mixer.set_prebuffer(id, 0, false).unwrap();
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
        assert!(
            server.sink.is_running(),
            "the primed ring was never started"
        );
        assert_eq!(server.sink.frames_written(), 12);
        server.shutdown();
    }

    /// A create request at version 35, as a modern client sends it.
    fn create_request(tag_number: u32, corked: bool) -> Vec<u8> {
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
                .boolean(corked)
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

        first.write_all(&create_request(1, false)).unwrap();
        second.write_all(&create_request(1, false)).unwrap();
        server.pass().unwrap();
        server.pass().unwrap();

        let first_reply = drain_socket(&mut first);
        let second_reply = drain_socket(&mut second);
        assert_eq!(
            first_command(&first_reply),
            Some(command::REPLY),
            "the first client's stream"
        );
        assert_eq!(
            first_command(&second_reply),
            Some(command::REPLY),
            "and the second client's, which used to be PA_ERR_INTERNAL"
        );
        assert_ne!(
            created_sink_input(&first_reply),
            created_sink_input(&second_reply),
            "sink-input indexes are process-global, not per connection"
        );
        assert_eq!(server.stream_count(), 2, "two streams, not one");
        server.shutdown();
    }

    pub(crate) const BROWSER_UID: u32 = 65536;

    /// A deployment policy whose browser account is `BROWSER_UID`, for a
    /// daemon running as `own`.
    pub(crate) fn browser_policy(own: u32) -> Policy {
        Policy::deployment_policy(own, Ok(()), || {
            crate::app_policy::Policy::from_tables(
                "td-bus-applications-v2\t1000\nfirefox\tservice\torg.mozilla.firefox\n",
                "td-accounts-v1\napplication\tfirefox\t65536\n",
            )
            .map_err(io::Error::other)
        })
        .unwrap()
    }

    /// A connection through the production admission path, with the
    /// credentials the kernel would report for `uid` and `pid`: one test
    /// process cannot be the browser's account itself.
    pub(crate) fn admit_as(server: &mut Server<MemorySink>, uid: u32, pid: i32) -> UnixStream {
        let (ours, theirs) = UnixStream::pair().unwrap();
        theirs.set_nonblocking(true).unwrap();
        server.admit(theirs, sys::Peer { pid, uid, gid: uid }, &mut false);
        ours
    }

    fn error_code(bytes: &[u8], expected_tag: u32) -> Option<u32> {
        control_packets(bytes).into_iter().find_map(|packet| {
            let mut reader = tag::Reader::new(&packet);
            if reader.u32().ok()? != command::ERROR || reader.u32().ok()? != expected_tag {
                return None;
            }
            reader.u32().ok()
        })
    }

    /// The REPLY packet answering `expected_tag`, if one arrived.
    fn reply_packet(bytes: &[u8], expected_tag: u32) -> Option<Vec<u8>> {
        control_packets(bytes).into_iter().find(|packet| {
            let mut reader = tag::Reader::new(packet);
            reader.u32().ok() == Some(command::REPLY) && reader.u32().ok() == Some(expected_tag)
        })
    }

    /// The level and mute a `GET_SINK_INFO` reply reports.
    fn reported_output(reply: &[u8]) -> (Vec<u32>, bool) {
        let mut reader = tag::Reader::new(reply);
        // Command, tag, index, name, description, sample spec, channel map,
        // owner module: then the cvolume and the mute flag.
        for _ in 0..8 {
            reader.skip().unwrap();
        }
        (reader.cvolume().unwrap(), reader.boolean().unwrap())
    }

    fn unhex(text: &str) -> Vec<u8> {
        (0..text.len() / 2)
            .map(|at| u8::from_str_radix(&text[at * 2..at * 2 + 2], 16).unwrap())
            .collect()
    }

    /// Master volume and mute, end to end over the socket. A controller sets
    /// both with the packets the pinned libpulse 17.0 actually sends, sees
    /// them in sink info, and its fellow controller hears one sink CHANGE
    /// event per write. A player's sink writes are a command this server does
    /// not have — even malformed, because they are refused before parsing —
    /// and its view of the sink stays byte-identical: unity, unmuted, and no
    /// event.
    #[test]
    fn a_controller_sets_the_output_level_and_a_player_neither_can_nor_sees_it() {
        let (mut server, path) = server("master-volume");
        server.policy = browser_policy(own_uid());
        let mut player = admit_as(&mut server, BROWSER_UID, 7);
        let mut setter = UnixStream::connect(&path).unwrap();
        let mut watcher = UnixStream::connect(&path).unwrap();
        for socket in [&mut player, &mut setter, &mut watcher] {
            socket.write_all(&auth_packet()).unwrap();
        }
        server.pass().unwrap();
        server.pass().unwrap();
        for socket in [&mut player, &mut setter, &mut watcher] {
            let _ = drain_socket(socket);
        }
        for socket in [&mut player, &mut setter, &mut watcher] {
            socket
                .write_all(&build(command::SUBSCRIBE, 10, |writer| {
                    writer.u32(subscription::MASK_ALL);
                }))
                .unwrap();
        }
        server.pass().unwrap();
        server.pass().unwrap();
        let sink_info = build(command::GET_SINK_INFO, 11, |writer| {
            writer.u32(0).null_string();
        });
        player.write_all(&sink_info).unwrap();
        server.pass().unwrap();
        server.pass().unwrap();
        let _ = drain_socket(&mut watcher);
        let _ = drain_socket(&mut setter);
        let player_view_before = reply_packet(&drain_socket(&mut player), 11).unwrap();

        // The player: refused before the arguments are read.
        let half = crate::mixer::VOLUME_NORM / 2;
        let mut probes = build(command::SET_SINK_VOLUME, 20, |writer| {
            writer.u32(0).null_string().cvolume(&[half, half]);
        });
        probes.extend(build(command::SET_SINK_MUTE, 21, |writer| {
            writer.u32(0).null_string().boolean(true);
        }));
        probes.extend(build(command::SET_SINK_VOLUME, 22, |writer| {
            writer.u32(0);
        }));
        player.write_all(&probes).unwrap();
        server.pass().unwrap();
        server.pass().unwrap();
        let answers = drain_socket(&mut player);
        for probe in [20, 21, 22] {
            assert_eq!(
                error_code(&answers, probe),
                Some(crate::proto::error::NOTIMPLEMENTED),
                "player probe {probe}"
            );
        }
        assert_eq!(
            server.clients.len(),
            3,
            "the malformed write disconnected nobody"
        );
        assert_eq!(
            (server.mixer.master_volume(), server.mixer.master_muted()),
            (crate::mixer::VOLUME_NORM, false)
        );
        assert_eq!(subscription_events(&drain_socket(&mut watcher)), Vec::new());

        // The controller, with libpulse 17.0's own bytes, unchanged: each is
        // td-pulse's fixture, found in its source rather than retyped. Each
        // batch is one pass and its tags are distinct within it. By name at
        // 75%, then by index with uneven channels; then mute by index; then
        // pactl's mute of the sink named "0", which is already muted.
        let fixtures = include_str!("../../td-pulse/src/proto.rs");
        let batches: &[&[(&str, u32, usize)]] = &[
            &[
                (
                    "4c000000244c000000034cffffffff744044454641554c545f53494e4b400076020000c0000000c000",
                    3,
                    1,
                ),
                ("4c000000244c000000024c000000004e76020000800000004000", 2, 1),
            ],
            &[("4c000000274c000000034c000000004e31", 3, 1)],
            &[("4c000000274c000000024cffffffff74300031", 2, 0)],
        ];
        let sink_change = (subscription::EVENT_SINK | subscription::EVENT_CHANGE, 0);
        for batch in batches {
            let mut writes = Vec::new();
            for (hex, _, _) in *batch {
                assert!(fixtures.contains(hex), "{hex} is not a td-pulse capture");
                writes.extend(control(&unhex(hex)));
            }
            setter.write_all(&writes).unwrap();
            server.pass().unwrap();
            server.pass().unwrap();
            let answers = drain_socket(&mut setter);
            let mut changes = 0;
            for &(_, tag_value, changed) in *batch {
                assert!(
                    reply_packet(&answers, tag_value).is_some(),
                    "captured write tag {tag_value} was answered with a REPLY"
                );
                changes += changed;
            }
            // The writer hears its own change once, as Pulse's subscribers
            // do, every other controller hears it once, and a write that
            // changes nothing is announced to nobody.
            assert_eq!(subscription_events(&answers), vec![sink_change; changes]);
            assert_eq!(
                subscription_events(&drain_socket(&mut watcher)),
                vec![sink_change; changes]
            );
        }
        assert_eq!(
            (server.mixer.master_volume(), server.mixer.master_muted()),
            (0x8000, true),
            "the loudest channel of the last level, and muted"
        );

        // Each sees the sink as its role allows.
        setter.write_all(&sink_info).unwrap();
        player.write_all(&sink_info).unwrap();
        server.pass().unwrap();
        server.pass().unwrap();
        let controller_view = reply_packet(&drain_socket(&mut setter), 11).unwrap();
        assert_eq!(reported_output(&controller_view), (vec![0x8000; 2], true));
        let player_answers = drain_socket(&mut player);
        assert_eq!(subscription_events(&player_answers), Vec::new());
        assert_eq!(
            reply_packet(&player_answers, 11).unwrap(),
            player_view_before,
            "the player's sink is unchanged to the byte"
        );
        assert_eq!(
            reported_output(&player_view_before),
            (vec![crate::mixer::VOLUME_NORM; 2], false)
        );

        // A selector naming no sink is NOENTITY, and changes nothing.
        setter
            .write_all(&build(command::SET_SINK_MUTE, 30, |writer| {
                writer
                    .u32(tag::INVALID_INDEX)
                    .string("elsewhere")
                    .boolean(false);
            }))
            .unwrap();
        server.pass().unwrap();
        server.pass().unwrap();
        assert_eq!(
            error_code(&drain_socket(&mut setter), 30),
            Some(crate::proto::error::NOENTITY)
        );
        assert!(server.mixer.master_muted());
        assert_eq!(subscription_events(&drain_socket(&mut watcher)), Vec::new());

        // Malformed requests are INVALID, as Pulse answers, and change
        // nothing: no channels (no loudest one to take), PA_VOLUME_INVALID
        // (above every level), three channels on a stereo sink, and a
        // selector that names the sink both ways or neither.
        let invalid = tag::INVALID_INDEX;
        let mut probes = build(command::SET_SINK_VOLUME, 40, |writer| {
            writer.u32(0).null_string().cvolume(&[]);
        });
        probes.extend(build(command::SET_SINK_VOLUME, 41, |writer| {
            writer.u32(0).null_string().cvolume(&[0x1000, 0xFFFF_FFFF]);
        }));
        probes.extend(build(command::SET_SINK_VOLUME, 42, |writer| {
            writer.u32(0).null_string().cvolume(&[0x1000; 3]);
        }));
        probes.extend(build(command::SET_SINK_VOLUME, 43, |writer| {
            writer.u32(0).string("td-audio").cvolume(&[0x1000, 0x1000]);
        }));
        probes.extend(build(command::SET_SINK_MUTE, 44, |writer| {
            writer.u32(invalid).null_string().boolean(false);
        }));
        setter.write_all(&probes).unwrap();
        server.pass().unwrap();
        server.pass().unwrap();
        let answers = drain_socket(&mut setter);
        for probe in 40..=44 {
            assert_eq!(
                error_code(&answers, probe),
                Some(crate::proto::error::INVALID),
                "probe {probe}"
            );
        }
        assert_eq!(
            (server.mixer.master_volume(), server.mixer.master_muted()),
            (0x8000, true)
        );
        assert_eq!(subscription_events(&answers), Vec::new());
        assert_eq!(subscription_events(&drain_socket(&mut watcher)), Vec::new());
    }

    /// `create_request` with the stream named `name`.
    pub(crate) fn named_create_request(tag_number: u32, name: &str) -> Vec<u8> {
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
            writer.proplist(&[tag::text_property("media.name", name)]);
            for _ in 0..7 {
                writer.boolean(false);
            }
            writer.u8(0);
        })
    }

    pub(crate) fn client_name_packet(tag_number: u32, application: &str) -> Vec<u8> {
        build(command::SET_CLIENT_NAME, tag_number, |writer| {
            writer.proplist(&[tag::text_property("application.name", application)]);
        })
    }

    fn property_value(properties: &[tag::Property], key: &str) -> Option<String> {
        properties
            .iter()
            .find(|property| property.key == key)
            .map(|property| {
                String::from_utf8_lossy(&property.value)
                    .trim_end_matches('\0')
                    .to_string()
            })
    }

    /// One sink-input entry as a version-35 reply writes it.
    #[derive(Debug)]
    struct SinkInputEntry {
        index: u32,
        name: String,
        client: u32,
        volume: Vec<u32>,
        muted: bool,
        properties: Vec<tag::Property>,
    }

    fn sink_input_entries(reply: &[u8]) -> Vec<SinkInputEntry> {
        let mut reader = tag::Reader::new(reply);
        reader.u32().unwrap();
        reader.u32().unwrap();
        let mut entries = Vec::new();
        while reader.finish().is_err() {
            let index = reader.u32().unwrap();
            let name = reader.string().unwrap().unwrap_or_default();
            reader.skip().unwrap();
            let client = reader.u32().unwrap();
            for _ in 0..3 {
                reader.skip().unwrap();
            }
            let volume = reader.cvolume().unwrap();
            for _ in 0..4 {
                reader.skip().unwrap();
            }
            let muted = reader.boolean().unwrap();
            let properties = reader.proplist().unwrap();
            for _ in 0..4 {
                reader.skip().unwrap();
            }
            entries.push(SinkInputEntry {
                index,
                name,
                client,
                volume,
                muted,
                properties,
            });
        }
        entries
    }

    /// `(index, name, application.name)` per client-info entry.
    fn client_entries(reply: &[u8]) -> Vec<(u32, String, Option<String>)> {
        let mut reader = tag::Reader::new(reply);
        reader.u32().unwrap();
        reader.u32().unwrap();
        let mut entries = Vec::new();
        while reader.finish().is_err() {
            let index = reader.u32().unwrap();
            let name = reader.string().unwrap().unwrap_or_default();
            reader.skip().unwrap();
            reader.skip().unwrap();
            let properties = reader.proplist().unwrap();
            entries.push((index, name, property_value(&properties, "application.name")));
        }
        entries
    }

    /// A controller lists every stream and every connection. A player's
    /// application is named from its uid, whatever it calls itself, and the
    /// text it chose reaches the controller without control characters. A
    /// player's own view is unchanged: the three list commands are ones this
    /// server does not have, and its own stream is described exactly as
    /// before, raw name and all.
    #[test]
    fn a_controller_lists_every_stream_and_client_and_a_player_lists_nothing() {
        let (mut server, path) = server("lists");
        server.policy = browser_policy(own_uid());
        let mut player = admit_as(&mut server, BROWSER_UID, 7);
        let mut controller = UnixStream::connect(&path).unwrap();
        for socket in [&mut player, &mut controller] {
            socket.write_all(&auth_packet()).unwrap();
        }
        server.pass().unwrap();
        player
            .write_all(&client_name_packet(2, "td-update"))
            .unwrap();
        controller
            .write_all(&client_name_packet(2, "td-mixer"))
            .unwrap();
        server.pass().unwrap();
        server.pass().unwrap();
        let _ = drain_socket(&mut player);
        let _ = drain_socket(&mut controller);
        let hostile = "tab\u{1b}[2J\u{7}title";
        player.write_all(&named_create_request(3, hostile)).unwrap();
        controller
            .write_all(&named_create_request(3, "a notification"))
            .unwrap();
        server.pass().unwrap();
        server.pass().unwrap();
        let player_stream = created_sink_input(&drain_socket(&mut player)).unwrap();
        let controller_stream = created_sink_input(&drain_socket(&mut controller)).unwrap();

        // libpulse 17.0's own requests, as td-pulse's `captured_17` pins them
        // (each tag 2, so one per round).
        const SINK_INPUT_LIST: &str = "4c0000001e4c00000002";
        const CLIENT_LIST: &str = "4c0000001c4c00000002";
        const CLIENT_INFO_3: &str = "4c0000001b4c000000024c00000003";
        let ask = |server: &mut Server<MemorySink>, socket: &mut UnixStream, hex: &str| {
            socket.write_all(&control(&unhex(hex))).unwrap();
            server.pass().unwrap();
            server.pass().unwrap();
            drain_socket(socket)
        };
        for probe in [SINK_INPUT_LIST, CLIENT_LIST, CLIENT_INFO_3] {
            assert_eq!(
                error_code(&ask(&mut server, &mut player, probe), 2),
                Some(crate::proto::error::NOTIMPLEMENTED),
                "player list probe {probe}"
            );
        }
        player
            .write_all(&build(command::GET_SINK_INPUT_INFO, 13, |writer| {
                writer.u32(player_stream);
            }))
            .unwrap();
        server.pass().unwrap();
        server.pass().unwrap();
        let own = sink_input_entries(&reply_packet(&drain_socket(&mut player), 13).unwrap());
        assert_eq!(own.len(), 1);
        assert_eq!(own[0].name, hostile, "its own name, as it sent it");
        assert_eq!(property_value(&own[0].properties, "application.name"), None);

        let stream_list = ask(&mut server, &mut controller, SINK_INPUT_LIST);
        let client_list = ask(&mut server, &mut controller, CLIENT_LIST);
        let mut streams = sink_input_entries(&reply_packet(&stream_list, 2).unwrap());
        streams.sort_by_key(|entry| entry.index);
        let summary: Vec<(u32, &str, Option<String>)> = streams
            .iter()
            .map(|entry| {
                (
                    entry.index,
                    entry.name.as_str(),
                    property_value(&entry.properties, "application.name"),
                )
            })
            .collect();
        let mut expected = vec![
            (player_stream, "tab[2Jtitle", Some("firefox".to_string())),
            (
                controller_stream,
                "a notification",
                Some("td-mixer".to_string()),
            ),
        ];
        expected.sort_by_key(|entry| entry.0);
        assert_eq!(summary, expected);
        let player_entry = streams
            .iter()
            .find(|entry| entry.index == player_stream)
            .unwrap();
        assert_eq!(
            property_value(&player_entry.properties, "media.name").as_deref(),
            Some("tab[2Jtitle")
        );
        let mut clients = client_entries(&reply_packet(&client_list, 2).unwrap());
        clients.sort();
        let player_client = player_entry.client;
        let names: Vec<(u32, &str)> = clients
            .iter()
            .map(|(index, name, _)| (*index, name.as_str()))
            .collect();
        assert!(names.contains(&(player_client, "firefox")), "{names:?}");
        assert!(
            names.iter().any(|(_, name)| *name == "td-mixer"),
            "{names:?}"
        );
        assert_eq!(names.len(), 2);

        controller
            .write_all(&build(command::GET_CLIENT_INFO, 30, |writer| {
                writer.u32(player_client);
            }))
            .unwrap();
        controller
            .write_all(&build(command::GET_CLIENT_INFO, 31, |writer| {
                writer.u32(9_999);
            }))
            .unwrap();
        server.pass().unwrap();
        server.pass().unwrap();
        let answers = drain_socket(&mut controller);
        assert_eq!(
            client_entries(&reply_packet(&answers, 30).unwrap()),
            vec![(player_client, "firefox".into(), Some("firefox".into()))]
        );
        assert_eq!(
            error_code(&answers, 31),
            Some(crate::proto::error::NOENTITY)
        );
    }

    /// The human's level for an application is the daemon's, and the
    /// application cannot undo it: not by unmuting or raising its own stream,
    /// not by opening another, and not by reconnecting. The controller sees
    /// the level it set; the player sees its own, and is told nothing.
    #[test]
    fn an_application_cannot_undo_the_humans_mute() {
        let (mut server, path) = server("app-mute");
        server.policy = browser_policy(own_uid());
        let mut player = admit_as(&mut server, BROWSER_UID, 7);
        let mut controller = UnixStream::connect(&path).unwrap();
        for socket in [&mut player, &mut controller] {
            socket.write_all(&auth_packet()).unwrap();
        }
        server.pass().unwrap();
        let _ = drain_socket(&mut player);
        let _ = drain_socket(&mut controller);
        player.write_all(&named_create_request(3, "video")).unwrap();
        server.pass().unwrap();
        server.pass().unwrap();
        let first = created_sink_input(&drain_socket(&mut player)).unwrap();
        player
            .write_all(&build(command::SUBSCRIBE, 4, |writer| {
                writer.u32(subscription::MASK_ALL);
            }))
            .unwrap();
        controller
            .write_all(&build(command::SUBSCRIBE, 4, |writer| {
                writer.u32(subscription::MASK_ALL);
            }))
            .unwrap();
        server.pass().unwrap();
        let _ = drain_socket(&mut player);
        let _ = drain_socket(&mut controller);

        let quarter = crate::mixer::VOLUME_NORM / 4;
        let mut writes = build(command::SET_SINK_INPUT_VOLUME, 10, |writer| {
            writer.u32(first).cvolume(&[quarter, quarter]);
        });
        writes.extend(build(command::SET_SINK_INPUT_MUTE, 11, |writer| {
            writer.u32(first).boolean(true);
        }));
        controller.write_all(&writes).unwrap();
        server.pass().unwrap();
        server.pass().unwrap();
        let answers = drain_socket(&mut controller);
        assert_eq!(error_code(&answers, 10), None);
        assert_eq!(error_code(&answers, 11), None);
        let change = (
            subscription::EVENT_SINK_INPUT | subscription::EVENT_CHANGE,
            first,
        );
        assert_eq!(subscription_events(&answers), vec![change; 2]);
        assert_eq!(subscription_events(&drain_socket(&mut player)), Vec::new());

        let ids = |server: &Server<MemorySink>| -> Vec<crate::mixer::StreamId> {
            server
                .clients
                .iter()
                .filter(|client| client.role == Role::Player)
                .flat_map(|client| client.session.stream_ids())
                .collect()
        };
        let levels = |server: &Server<MemorySink>| -> Vec<Option<(u32, bool)>> {
            ids(server)
                .into_iter()
                .map(|id| server.mixer.app_level(id))
                .collect()
        };
        assert_eq!(levels(&server), vec![Some((quarter, true))]);

        // The application unmutes and raises its own stream, and opens
        // another: every stream still plays under the human's mute.
        let norm = crate::mixer::VOLUME_NORM;
        let mut writes = build(command::SET_SINK_INPUT_MUTE, 20, |writer| {
            writer.u32(first).boolean(false);
        });
        writes.extend(build(command::SET_SINK_INPUT_VOLUME, 21, |writer| {
            writer.u32(first).cvolume(&[norm, norm]);
        }));
        writes.extend(named_create_request(22, "another"));
        player.write_all(&writes).unwrap();
        server.pass().unwrap();
        server.pass().unwrap();
        let _ = drain_socket(&mut player);
        assert_eq!(levels(&server), vec![Some((quarter, true)); 2]);

        // The controller sees its level; the player sees its own.
        controller
            .write_all(&build(command::GET_SINK_INPUT_INFO, 30, |writer| {
                writer.u32(first);
            }))
            .unwrap();
        player
            .write_all(&build(command::GET_SINK_INPUT_INFO, 30, |writer| {
                writer.u32(first);
            }))
            .unwrap();
        server.pass().unwrap();
        server.pass().unwrap();
        let seen = sink_input_entries(&reply_packet(&drain_socket(&mut controller), 30).unwrap());
        assert_eq!(
            (seen[0].volume.clone(), seen[0].muted),
            (vec![quarter; 2], true)
        );
        let own = sink_input_entries(&reply_packet(&drain_socket(&mut player), 30).unwrap());
        assert_eq!(
            (own[0].volume.clone(), own[0].muted),
            (vec![norm; 2], false)
        );

        // Reconnecting starts at the human's level too.
        drop(player);
        for _ in 0..3 {
            server.pass().unwrap();
        }
        assert!(ids(&server).is_empty());
        let mut player = admit_as(&mut server, BROWSER_UID, 8);
        player.write_all(&auth_packet()).unwrap();
        server.pass().unwrap();
        let _ = drain_socket(&mut player);
        player.write_all(&named_create_request(3, "back")).unwrap();
        server.pass().unwrap();
        server.pass().unwrap();
        let _ = drain_socket(&mut player);
        assert_eq!(levels(&server), vec![Some((quarter, true))]);

        // Unmuting is the human's to do, and restores the level.
        let latest = server
            .clients
            .iter()
            .find(|client| client.role == Role::Player)
            .and_then(|client| client.session.sink_input_indexes().first().copied())
            .unwrap();
        let _ = drain_socket(&mut controller);
        controller
            .write_all(&build(command::SET_SINK_INPUT_MUTE, 40, |writer| {
                writer.u32(latest).boolean(false);
            }))
            .unwrap();
        server.pass().unwrap();
        server.pass().unwrap();
        assert_eq!(levels(&server), vec![Some((quarter, false))]);
        // And setting what is already set announces nothing.
        let _ = drain_socket(&mut controller);
        controller
            .write_all(&build(command::SET_SINK_INPUT_MUTE, 41, |writer| {
                writer.u32(latest).boolean(false);
            }))
            .unwrap();
        server.pass().unwrap();
        server.pass().unwrap();
        let answers = drain_socket(&mut controller);
        assert_eq!(error_code(&answers, 41), None);
        assert_eq!(subscription_events(&answers), Vec::new());
    }

    /// Every stream the mixer allows, each with the longest name the protocol
    /// carries in all three of its names, lists in one reply that fits one
    /// control frame, the most td-pulse's decoder accepts and a sixteenth of the
    /// session's output bound, so the listing controller is not dropped; and
    /// each name it is shown is cut to `DISPLAY_MAX` on a character boundary.
    #[test]
    fn a_full_table_of_maximum_length_names_lists_in_one_frame() {
        let (mut server, path) = server("full-list");
        let name: String = "é".repeat(tag::STRING_MAX / 2);
        assert_eq!(name.len(), tag::STRING_MAX);
        let per_client = crate::session::MAX_STREAMS_PER_CLIENT;
        let owners = crate::mixer::MAX_STREAMS / per_client;
        let mut sockets = Vec::new();
        for owner in 0..owners {
            let mut socket = admit_as(&mut server, own_uid(), 100 + owner as i32);
            socket.write_all(&auth_packet()).unwrap();
            socket.write_all(&client_name_packet(1, &name)).unwrap();
            sockets.push(socket);
        }
        server.pass().unwrap();
        for socket in &mut sockets {
            let _ = drain_socket(socket);
        }
        // One create per owner per pass, so no socket buffer fills.
        for stream in 0..per_client {
            for socket in &mut sockets {
                socket
                    .write_all(&named_create_request(10 + stream as u32, &name))
                    .unwrap();
            }
            server.pass().unwrap();
            server.pass().unwrap();
            for socket in &mut sockets {
                let _ = drain_socket(socket);
            }
        }
        assert_eq!(server.mixer.stream_count(), crate::mixer::MAX_STREAMS);
        let mut lister = UnixStream::connect(&path).unwrap();
        lister.write_all(&auth_packet()).unwrap();
        server.pass().unwrap();
        let _ = drain_socket(&mut lister);
        lister
            .write_all(&build(command::GET_SINK_INPUT_INFO_LIST, 2, |_| {}))
            .unwrap();
        server.pass().unwrap();
        server.pass().unwrap();
        let reply = reply_packet(&drain_socket(&mut lister), 2).expect("a list reply");
        // One control frame: td-pulse, which the CLI and the panel read with,
        // refuses a larger one, and the session bound is sixteen of these.
        assert!(reply.len() <= wire::CONTROL_MAX, "{} bytes", reply.len());
        assert_eq!(server.clients.len(), owners + 1, "nobody was disconnected");
        let entries = sink_input_entries(&reply);
        assert_eq!(entries.len(), crate::mixer::MAX_STREAMS);
        let cut = "é".repeat(crate::session::DISPLAY_MAX / 2);
        for entry in &entries {
            assert_eq!(entry.name, cut);
            assert_eq!(
                property_value(&entry.properties, "application.name").as_deref(),
                Some(cut.as_str())
            );
        }
    }

    /// A controller whose one read holds a valid sink write and then a frame
    /// that breaks its schema is dropped in that pass. The level it set
    /// outlives it, so the event announcing the level must too.
    #[test]
    fn a_dropped_controllers_last_change_is_still_announced() {
        let (mut server, path) = server("dropped-setter");
        let mut setter = UnixStream::connect(&path).unwrap();
        let mut watcher = UnixStream::connect(&path).unwrap();
        for socket in [&mut setter, &mut watcher] {
            socket.write_all(&auth_packet()).unwrap();
        }
        server.pass().unwrap();
        watcher
            .write_all(&build(command::SUBSCRIBE, 10, |writer| {
                writer.u32(subscription::MASK_ALL);
            }))
            .unwrap();
        server.pass().unwrap();
        let _ = drain_socket(&mut setter);
        let _ = drain_socket(&mut watcher);
        let half = crate::mixer::VOLUME_NORM / 2;
        let mut writes = build(command::SET_SINK_VOLUME, 20, |writer| {
            writer.u32(0).null_string().cvolume(&[half, half]);
        });
        // A mute with no selector name or value: a schema failure.
        writes.extend(build(command::SET_SINK_MUTE, 21, |writer| {
            writer.u32(0);
        }));
        setter.write_all(&writes).unwrap();
        server.pass().unwrap();
        server.pass().unwrap();
        assert_eq!(server.clients.len(), 1, "the setter was dropped");
        assert_eq!(server.mixer.master_volume(), half);
        assert_eq!(
            subscription_events(&drain_socket(&mut watcher)),
            vec![(subscription::EVENT_SINK | subscription::EVENT_CHANGE, 0)]
        );
    }

    /// `pactl get-sink-volume 0` names the sink `"0"` with an invalid index —
    /// libpulse 17.0's own bytes below — and a Pulse server resolves a decimal
    /// name as the index. Any role may look the sink up this way.
    #[test]
    fn a_sink_named_by_its_number_is_the_sink() {
        let (mut server, _path) = server("numeric-name");
        server.policy = browser_policy(own_uid());
        let mut player = admit_as(&mut server, BROWSER_UID, 7);
        player.write_all(&auth_packet()).unwrap();
        server.pass().unwrap();
        let _ = drain_socket(&mut player);
        player
            .write_all(&control(&unhex("4c000000154c000000024cffffffff743000")))
            .unwrap();
        server.pass().unwrap();
        server.pass().unwrap();
        let reply = reply_packet(&drain_socket(&mut player), 2).expect("sink info");
        assert_eq!(
            reported_output(&reply),
            (vec![crate::mixer::VOLUME_NORM; 2], false)
        );
    }

    /// An application account is a player. Another connection's stream is
    /// absent to it: no subscription event, and inspection, volume and mute
    /// all answer `NOENTITY` without touching the owner's stream. Its own
    /// stream stays fully controllable, and a controller still reaches it.
    #[test]
    fn a_player_cannot_observe_or_change_another_connections_stream() {
        let (mut server, path) = server("player-isolation");
        server.policy = browser_policy(own_uid());
        let mut player = admit_as(&mut server, BROWSER_UID, 7);
        let mut owner = UnixStream::connect(&path).unwrap();
        player.write_all(&auth_packet()).unwrap();
        owner.write_all(&auth_packet()).unwrap();
        server.pass().unwrap();
        server.pass().unwrap();
        let roles: Vec<Role> = server.clients.iter().map(|client| client.role).collect();
        assert_eq!(
            roles,
            [Role::Player, Role::Controller],
            "admission derived both roles from the deployment policy"
        );
        let _ = drain_socket(&mut player);
        let _ = drain_socket(&mut owner);

        player
            .write_all(&build(command::SUBSCRIBE, 10, |writer| {
                writer.u32(subscription::MASK_ALL);
            }))
            .unwrap();
        server.pass().unwrap();
        server.pass().unwrap();
        let _ = drain_socket(&mut player);

        owner.write_all(&create_request(20, false)).unwrap();
        server.pass().unwrap();
        server.pass().unwrap();
        let foreign = created_sink_input(&drain_socket(&mut owner)).expect("owner's stream");
        assert_eq!(
            subscription_events(&drain_socket(&mut player)),
            Vec::new(),
            "a player is not told another connection created a stream"
        );

        let half = crate::mixer::VOLUME_NORM / 2;
        let mut probes = build(command::GET_SINK_INPUT_INFO, 30, |writer| {
            writer.u32(foreign);
        });
        probes.extend(build(command::SET_SINK_INPUT_VOLUME, 31, |writer| {
            writer.u32(foreign).cvolume(&[half, half]);
        }));
        probes.extend(build(command::SET_SINK_INPUT_MUTE, 32, |writer| {
            writer.u32(foreign).boolean(true);
        }));
        player.write_all(&probes).unwrap();
        server.pass().unwrap();
        server.pass().unwrap();
        let answers = drain_socket(&mut player);
        for probe in [30, 31, 32] {
            assert_eq!(
                error_code(&answers, probe),
                Some(crate::proto::error::NOENTITY),
                "probe {probe} sees another connection's stream as absent"
            );
        }
        assert_eq!(subscription_events(&answers), Vec::new());
        // The same probes against an index nobody holds: the answers must be
        // the same bytes, or the difference enumerates live streams.
        let absent = foreign.wrapping_add(1000);
        let mut probes = build(command::GET_SINK_INPUT_INFO, 30, |writer| {
            writer.u32(absent);
        });
        probes.extend(build(command::SET_SINK_INPUT_VOLUME, 31, |writer| {
            writer.u32(absent).cvolume(&[half, half]);
        }));
        probes.extend(build(command::SET_SINK_INPUT_MUTE, 32, |writer| {
            writer.u32(absent).boolean(true);
        }));
        player.write_all(&probes).unwrap();
        server.pass().unwrap();
        server.pass().unwrap();
        assert_eq!(drain_socket(&mut player), answers);

        owner
            .write_all(&build(command::GET_SINK_INPUT_INFO, 40, |writer| {
                writer.u32(foreign);
            }))
            .unwrap();
        server.pass().unwrap();
        server.pass().unwrap();
        let state = sink_input_state(&drain_socket(&mut owner), 40).unwrap();
        assert_eq!(
            state.2,
            vec![crate::mixer::VOLUME_NORM, crate::mixer::VOLUME_NORM]
        );
        assert!(!state.3, "the owner's stream was not muted");

        player.write_all(&create_request(50, false)).unwrap();
        server.pass().unwrap();
        server.pass().unwrap();
        let own = created_sink_input(&drain_socket(&mut player)).expect("player's stream");
        player
            .write_all(&build(command::SET_SINK_INPUT_MUTE, 51, |writer| {
                writer.u32(own).boolean(true);
            }))
            .unwrap();
        server.pass().unwrap();
        server.pass().unwrap();
        let mine = drain_socket(&mut player);
        assert_eq!(error_code(&mine, 51), None, "its own stream obeys it");
        assert!(subscription_events(&mine).contains(&(
            subscription::EVENT_SINK_INPUT | subscription::EVENT_CHANGE,
            own
        )));

        owner
            .write_all(&build(command::SET_SINK_INPUT_MUTE, 60, |writer| {
                writer.u32(own).boolean(false);
            }))
            .unwrap();
        server.pass().unwrap();
        server.pass().unwrap();
        assert_eq!(
            error_code(&drain_socket(&mut owner), 60),
            None,
            "a controller still reaches the player's stream"
        );
        // It reaches it as the application's level, a gain of the human's
        // that the player neither sees nor hears about: the player's own mute
        // stands, and nothing changed in its view.
        assert_eq!(
            subscription_events(&drain_socket(&mut player)),
            Vec::new(),
            "the player is not told about the human's level"
        );
        assert_eq!(
            server
                .clients
                .iter()
                .find(|client| client.role == Role::Player)
                .map(|client| client.session.app_level()),
            Some((crate::mixer::VOLUME_NORM, false))
        );

        drop(owner);
        for _ in 0..3 {
            server.pass().unwrap();
        }
        assert!(
            !subscription_events(&drain_socket(&mut player))
                .iter()
                .any(|(_, index)| *index == foreign),
            "nor is it told the stream went away"
        );
        server.shutdown();
    }

    /// Two application connections are each other's strangers, not only a
    /// player and a controller.
    #[test]
    fn two_players_do_not_see_each_other() {
        let (mut server, _path) = server("player-pair");
        server.policy = browser_policy(own_uid());
        let mut watcher = admit_as(&mut server, BROWSER_UID, 7);
        let mut other = admit_as(&mut server, BROWSER_UID, 8);
        watcher.write_all(&auth_packet()).unwrap();
        other.write_all(&auth_packet()).unwrap();
        server.pass().unwrap();
        server.pass().unwrap();
        assert!(server
            .clients
            .iter()
            .all(|client| client.role == Role::Player));
        let _ = drain_socket(&mut watcher);
        let _ = drain_socket(&mut other);
        watcher
            .write_all(&build(command::SUBSCRIBE, 10, |writer| {
                writer.u32(subscription::MASK_ALL);
            }))
            .unwrap();
        server.pass().unwrap();
        server.pass().unwrap();
        let _ = drain_socket(&mut watcher);

        other.write_all(&create_request(20, false)).unwrap();
        server.pass().unwrap();
        server.pass().unwrap();
        let index = created_sink_input(&drain_socket(&mut other)).expect("other's stream");
        watcher
            .write_all(&build(command::SET_SINK_INPUT_MUTE, 30, |writer| {
                writer.u32(index).boolean(true);
            }))
            .unwrap();
        server.pass().unwrap();
        server.pass().unwrap();
        let seen = drain_socket(&mut watcher);
        assert_eq!(error_code(&seen, 30), Some(crate::proto::error::NOENTITY));
        assert_eq!(subscription_events(&seen), Vec::new());
        server.shutdown();
    }

    /// The role is decided once, at accept, from the policy then in force: a
    /// later policy change neither promotes nor demotes a live connection.
    #[test]
    fn a_connections_role_is_fixed_at_accept() {
        let (mut server, path) = server("role-at-accept");
        let mut control = UnixStream::connect(&path).unwrap();
        let mut playback = UnixStream::connect(&path).unwrap();
        control.write_all(&auth_packet()).unwrap();
        playback.write_all(&auth_packet()).unwrap();
        server.pass().unwrap();
        server.pass().unwrap();
        let _ = drain_socket(&mut control);
        let _ = drain_socket(&mut playback);
        playback.write_all(&create_request(20, false)).unwrap();
        server.pass().unwrap();
        server.pass().unwrap();
        let index = created_sink_input(&drain_socket(&mut playback)).expect("a stream");

        // A policy that would refuse both connections now. A router that
        // re-derived the role per request would refuse the controller too.
        server.policy = Policy {
            allowed_uids: Vec::new(),
            applications: None,
        };
        let _late = UnixStream::connect(&path).unwrap();
        server.pass().unwrap();
        assert_eq!(
            server.client_count(),
            2,
            "the new policy refuses a newcomer"
        );
        control
            .write_all(&build(command::SET_SINK_INPUT_MUTE, 30, |writer| {
                writer.u32(index).boolean(true);
            }))
            .unwrap();
        server.pass().unwrap();
        server.pass().unwrap();
        let reply = drain_socket(&mut control);
        assert_eq!(first_command(&reply), Some(command::REPLY));
        assert_eq!(
            error_code(&reply, 30),
            None,
            "the admitted controller still reaches another connection's stream"
        );
        server.shutdown();
    }

    /// A browser account that collides with a controller uid stays a player:
    /// sharing an account never promotes an application.
    #[test]
    fn a_browser_account_on_a_controller_uid_is_a_player() {
        let policy = browser_policy(BROWSER_UID);
        assert_eq!(policy.permitted(), [BROWSER_UID, SEAT_UID], "listed once");
        let role = |uid| {
            policy.role(&sys::Peer {
                pid: 1,
                uid,
                gid: uid,
            })
        };
        assert_eq!(role(BROWSER_UID), Some(Role::Player));
        assert_eq!(role(SEAT_UID), Some(Role::Controller));
    }

    /// Sink-input identity is daemon-global, not merely a unique number in a
    /// create reply. A separate control connection can observe, inspect, and
    /// change another client's stream, then observes its removal.
    #[test]
    fn a_control_client_routes_sink_input_operations_across_connections() {
        let (mut server, path) = server("global-control");
        let mut control = UnixStream::connect(&path).unwrap();
        let mut playback = UnixStream::connect(&path).unwrap();
        control.write_all(&auth_packet()).unwrap();
        playback.write_all(&auth_packet()).unwrap();
        server.pass().unwrap();
        server.pass().unwrap();
        let _ = drain_socket(&mut control);
        let _ = drain_socket(&mut playback);

        control
            .write_all(&build(command::SUBSCRIBE, 10, |writer| {
                writer.u32(subscription::MASK_SINK_INPUT);
            }))
            .unwrap();
        server.pass().unwrap();
        server.pass().unwrap();
        let _ = drain_socket(&mut control);

        playback.write_all(&create_request(20, false)).unwrap();
        server.pass().unwrap();
        server.pass().unwrap();
        let created = drain_socket(&mut playback);
        let index = created_sink_input(&created).expect("create reply has a sink-input id");
        let new_events = subscription_events(&drain_socket(&mut control));
        assert!(new_events.contains(&(
            subscription::EVENT_SINK_INPUT | subscription::EVENT_NEW,
            index,
        )));

        control
            .write_all(&build(command::GET_SINK_INPUT_INFO, 30, |writer| {
                writer.u32(index);
            }))
            .unwrap();
        server.pass().unwrap();
        server.pass().unwrap();
        let state = sink_input_state(&drain_socket(&mut control), 30).unwrap();
        assert_eq!(state.0, index);
        assert_eq!(
            state.1,
            server.clients.get(1).unwrap().session.client_index(),
            "the reply identifies the playback connection, not the requester"
        );

        let half = crate::mixer::VOLUME_NORM / 2;
        let mut changes = build(command::SET_SINK_INPUT_VOLUME, 31, |writer| {
            writer.u32(index).cvolume(&[half, half]);
        });
        changes.extend(build(command::SET_SINK_INPUT_MUTE, 32, |writer| {
            writer.u32(index).boolean(true);
        }));
        control.write_all(&changes).unwrap();
        server.pass().unwrap();
        server.pass().unwrap();
        let changed = drain_socket(&mut control);
        assert_eq!(
            subscription_events(&changed)
                .iter()
                .filter(|(event, event_index)| {
                    *event == (subscription::EVENT_SINK_INPUT | subscription::EVENT_CHANGE)
                        && *event_index == index
                })
                .count(),
            2,
            "both remote mutations are broadcast"
        );

        control
            .write_all(&build(command::GET_SINK_INPUT_INFO, 33, |writer| {
                writer.u32(index);
            }))
            .unwrap();
        server.pass().unwrap();
        server.pass().unwrap();
        let state = sink_input_state(&drain_socket(&mut control), 33).unwrap();
        assert_eq!(state.2, vec![half, half]);
        assert!(state.3, "the remote mute reached the owning stream");

        drop(playback);
        for _ in 0..3 {
            server.pass().unwrap();
        }
        let removed = subscription_events(&drain_socket(&mut control));
        assert!(removed.contains(&(
            subscription::EVENT_SINK_INPUT | subscription::EVENT_REMOVE,
            index,
        )));
        server.shutdown();
    }

    /// Creating a stream is control traffic, not a reason to start the PCM.
    /// Initial cork state also has to reach the mixer even if a hostile client
    /// sends data without waiting for a byte grant.
    #[test]
    fn empty_and_initially_corked_streams_do_not_start_the_pcm() {
        let path = socket_path("idle-streams");
        let _ = std::fs::remove_file(&path);
        let uid = own_uid();
        let mut server = Server::bind(&path, MemorySink::fixed(), Policy::for_uid(uid)).unwrap();
        let mut client = UnixStream::connect(&path).unwrap();
        client.write_all(&auth_packet()).unwrap();
        server.pass().unwrap();
        let _ = drain_socket(&mut client);

        client.write_all(&create_request(1, false)).unwrap();
        client.write_all(&create_request(2, true)).unwrap();
        server.pass().unwrap();
        let _ = drain_socket(&mut client);
        let pcm = vec![1u8; 4 * 32];
        let mut data = wire::Descriptor::encode(pcm.len() as u32, 1, 0, 0).to_vec();
        data.extend_from_slice(&pcm);
        client.write_all(&data).unwrap();
        server.pass().unwrap();

        assert_eq!(server.stream_count(), 2);
        assert_eq!(server.sink.frames_written(), 0);
        assert!(!server.sink.is_running());
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
        assert!(
            dropped,
            "it was never dropped, and the buffer grew unbounded"
        );
        server.shutdown();
    }

    /// Prefix compaction is bounded in memory and amortized across one whole
    /// consumed window rather than copying the owed tail after every byte.
    #[test]
    fn a_slowly_drained_client_amortizes_prefix_compaction() {
        let (stream, _peer) = UnixStream::pair().unwrap();
        let mut session = Session::new(Spec::fixed(), 0);
        let mut mixer = Mixer::new(Spec::fixed());
        session.feed(&auth_packet(), &mut mixer).unwrap();
        let _ = session.take_output();
        let mut client = Client {
            stream,
            session,
            peer: sys::Peer {
                pid: 1,
                uid: 1000,
                gid: 1000,
            },
            role: Role::Controller,
            pending: vec![0; MAX_PENDING_STORAGE - 1],
            pending_at: MAX_PENDING,
            since: Instant::now(),
            partial_since: None,
            ready_input: false,
        };

        client
            .session
            .feed(&build(command::GET_SERVER_INFO, 0, |_| {}), &mut mixer)
            .unwrap();
        client.collect();
        assert_eq!(client.pending_at, 0, "the full window compacted once");
        assert!(client.pending.len() <= MAX_PENDING_STORAGE);

        for tag_value in 1..16 {
            client.pending_at = client.pending_at.saturating_add(1);
            client
                .session
                .feed(
                    &build(command::GET_SERVER_INFO, tag_value, |_| {}),
                    &mut mixer,
                )
                .unwrap();
            client.collect();
            assert_eq!(client.pending_at, tag_value as usize);
            assert!(client.pending.len() <= MAX_PENDING_STORAGE);
        }
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

    /// A framed body cannot reserve a decoder buffer and admission slot
    /// forever merely by stopping after its descriptor.
    #[test]
    fn an_abandoned_frame_is_dropped_after_its_own_deadline() {
        let (mut server, path) = server("partial-frame");
        let mut client = UnixStream::connect(&path).unwrap();
        client.write_all(&auth_packet()).unwrap();
        server.pass().unwrap();
        server.pass().unwrap();
        let _ = drain_socket(&mut client);

        client
            .write_all(&wire::Descriptor::encode(1024, 0, 0, 0))
            .unwrap();
        server.pass().unwrap();
        assert!(
            server
                .clients
                .first()
                .unwrap()
                .session
                .has_incomplete_input(),
            "the incomplete descriptor is retained"
        );
        assert!(
            server.clients.first().unwrap().partial_since.is_some(),
            "reading the partial frame did not arm its deadline"
        );
        server.clients.first_mut().unwrap().partial_since =
            Instant::now().checked_sub(FRAME_DEADLINE + Duration::from_secs(1));
        server.pass().unwrap();
        assert_eq!(server.client_count(), 0);
        server.shutdown();
    }

    /// AF_UNIX reports readable data and HUP together after a peer closes.
    /// Reading the queued bytes first lets the state machine and device observe
    /// the final write; disconnect may still discard its unplayed tail.
    #[test]
    fn a_final_write_is_processed_before_peer_hangup() {
        let (mut server, path) = server("write-then-close");
        let mut client = UnixStream::connect(&path).unwrap();
        let mut conversation = auth_packet();
        conversation.extend(create_request(1, false));
        let pcm = vec![7u8; 9600 * 4];
        conversation.extend(wire::Descriptor::encode(pcm.len() as u32, 0, 0, 0));
        conversation.extend(pcm);
        client.write_all(&conversation).unwrap();
        drop(client);

        for _ in 0..4 {
            server.pass().unwrap();
        }
        assert!(
            server.sink.frames_written() > 0,
            "queued audio was discarded when POLLIN arrived with POLLHUP"
        );
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
        assert_eq!(
            first_command(&drain_socket(&mut good)),
            Some(command::REPLY)
        );
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

    #[test]
    fn deployment_audio_admits_only_the_assigned_browser_and_fails_closed() {
        const LEDGER: &str = "td-accounts-v1\napplication\tfirefox\t65536\napplication\tmail\t65537\napplication\tnews\t65538\n";
        let installed = || {
            crate::app_policy::Policy::from_tables(
                "td-bus-applications-v2\t1000\nfirefox\tservice\torg.mozilla.firefox\nmail\tservice\t\nnews\tservice\t\n",
                LEDGER,
            )
            .map_err(io::Error::other)
        };
        let policy = Policy::deployment_policy(994, Ok(()), installed).unwrap();
        assert_eq!(policy.permitted(), [994, 1000, 65536]);
        let role = |uid| {
            policy.role(&sys::Peer {
                pid: 1,
                uid,
                gid: uid,
            })
        };
        assert_eq!(role(994), Some(Role::Controller), "the daemon's own uid");
        assert_eq!(role(1000), Some(Role::Controller), "the seat user");
        assert_eq!(role(65536), Some(Role::Player), "the browser account");
        assert_eq!(role(65537), None, "an application without audio");
        assert_eq!(role(993), None, "the compositor is not yet admitted");
        for uid in [994, 1000, 65536] {
            assert!(admits(
                &policy,
                &sys::Peer {
                    pid: 1,
                    uid,
                    gid: uid
                }
            ));
        }
        for uid in [0, 991, 993, 1001, 65537, 65538, 65539] {
            assert!(!admits(
                &policy,
                &sys::Peer {
                    pid: 1,
                    uid,
                    gid: uid
                }
            ));
        }
        let fallback = Policy::deployment_policy(994, Err(io::ErrorKind::NotFound.into()), || {
            panic!("missing deployment must not call loader")
        })
        .unwrap();
        assert_eq!(fallback.allowed_uids, [994, 1000]);
        assert!(Policy::deployment_policy(
            994,
            Err(io::ErrorKind::PermissionDenied.into()),
            installed
        )
        .is_err());
        assert!(
            Policy::deployment_policy(994, Ok(()), || Err(io::Error::other("invalid policy")))
                .is_err()
        );
        let no_browser = Policy::deployment_policy(994, Ok(()), || {
            crate::app_policy::Policy::from_tables(
                "td-bus-applications-v2\t1000\nmail\tservice\t\n",
                LEDGER,
            )
            .map_err(io::Error::other)
        })
        .unwrap();
        assert_eq!(no_browser.permitted(), [994, 1000]);
        assert!(!admits(
            &no_browser,
            &sys::Peer {
                pid: 1,
                uid: 65536,
                gid: 65536
            }
        ));
        // A declared browser without a ledger account is admitted nowhere.
        let unallocated = Policy::deployment_policy(994, Ok(()), || {
            crate::app_policy::Policy::from_tables(
                "td-bus-applications-v2\t1000\nfirefox\tservice\torg.mozilla.firefox\n",
                "td-accounts-v1\napplication\tmail\t65537\n",
            )
            .map_err(io::Error::other)
        })
        .unwrap();
        assert_eq!(unallocated.permitted(), [994, 1000]);
    }

    /// The policy accepts the daemon's own uid and the seat user, and nothing
    /// else — including uid 0, which is the one a mistake would most likely
    /// let through.
    #[test]
    fn the_policy_admits_exactly_two_uids() {
        let policy = Policy::for_uid(994);
        let peer = |uid| sys::Peer {
            pid: 1,
            uid,
            gid: uid,
        };
        assert!(admits(&policy, &peer(994)), "the daemon's own uid");
        assert!(admits(&policy, &peer(SEAT_UID)), "the seat user");
        assert!(!admits(&policy, &peer(0)), "root is not on the list");
        assert!(!admits(&policy, &peer(1001)), "another user");
        assert!(!admits(&policy, &peer(65534)), "nobody");
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

    /// One kernel-authenticated process cannot reserve the whole daemon table.
    #[test]
    fn one_pid_has_its_own_client_limit() {
        let (mut server, path) = server("per-pid-bound");
        let mut held = Vec::new();
        for _ in 0..MAX_CLIENTS_PER_PID + 4 {
            match UnixStream::connect(&path) {
                Ok(mut stream) => {
                    stream.write_all(&auth_packet()).unwrap();
                    held.push(stream);
                }
                Err(_) => break,
            }
        }
        for _ in 0..4 {
            server.pass().unwrap();
        }
        assert_eq!(server.client_count(), MAX_CLIENTS_PER_PID);
        assert!(
            server
                .clients
                .iter()
                .all(|client| client.session.version().is_some()),
            "the admitted contexts reached authenticated idle state"
        );
        assert!(server.refused >= 4, "the surplus was refused, not queued");
        server.shutdown();
    }

    /// The global ceiling still bounds descriptors, while one genuinely idle
    /// context is evictable per pass so a full table cannot exclude newcomers
    /// or churn the whole table in one listener wake.
    #[test]
    fn the_full_client_table_admits_a_newcomer_by_evicting_idle() {
        let (mut server, path) = server("global-bound");
        let mut held = Vec::new();
        for batch in 0..(MAX_CLIENTS / MAX_CLIENTS_PER_PID) {
            let before = server.client_count();
            for _ in 0..MAX_CLIENTS_PER_PID {
                let mut stream = UnixStream::connect(&path).unwrap();
                stream.write_all(&auth_packet()).unwrap();
                held.push(stream);
            }
            server.pass().unwrap();
            for client in server.clients.iter_mut().skip(before) {
                client.peer.pid = 10_000 + batch as i32;
            }
            server.pass().unwrap();
        }
        assert_eq!(server.client_count(), MAX_CLIENTS);
        assert!(
            server
                .clients
                .iter()
                .all(|client| client.session.version().is_some()),
            "the full table consists of authenticated idle contexts"
        );
        let existing = server.clients.first().unwrap().session.client_index();
        held.first_mut()
            .unwrap()
            .write_all(&build(command::GET_SINK_INPUT_INFO, 1, |writer| {
                writer.u32(tag::INVALID_INDEX - 1);
            }))
            .unwrap();
        let disconnected = server.disconnected;
        let mut newcomer = UnixStream::connect(&path).unwrap();
        newcomer.write_all(&auth_packet()).unwrap();
        let mut surplus = UnixStream::connect(&path).unwrap();
        surplus.write_all(&auth_packet()).unwrap();
        held.push(newcomer);
        held.push(surplus);
        let refused = server.refused;
        server.pass().unwrap();
        server.pass().unwrap();
        assert_eq!(server.client_count(), MAX_CLIENTS);
        assert_eq!(server.disconnected, disconnected + 1);
        assert_eq!(
            server.refused,
            refused + 1,
            "one listener wake replaces no more than one old idle context"
        );
        assert!(
            server
                .clients
                .iter()
                .any(|client| client.session.client_index() == existing),
            "the readable client's unrouted request was mistaken for idleness"
        );
        server.shutdown();
    }

    /// A streamless subscriber is a live control connection, not an idle
    /// decoder slot that admission pressure may silently discard.
    #[test]
    fn a_subscribed_control_connection_is_not_idle_eviction_fodder() {
        let (mut server, path) = server("subscribed-global-bound");
        let mut held = Vec::new();
        for batch in 0..(MAX_CLIENTS / MAX_CLIENTS_PER_PID) {
            let before = server.client_count();
            for _ in 0..MAX_CLIENTS_PER_PID {
                let mut stream = UnixStream::connect(&path).unwrap();
                stream.write_all(&auth_packet()).unwrap();
                held.push(stream);
            }
            server.pass().unwrap();
            for client in server.clients.iter_mut().skip(before) {
                client.peer.pid = 20_000 + batch as i32;
            }
            server.pass().unwrap();
        }
        let protected = server.clients.first().unwrap().session.client_index();
        held.first_mut()
            .unwrap()
            .write_all(&build(command::SUBSCRIBE, 40, |writer| {
                writer.u32(subscription::MASK_SINK_INPUT);
            }))
            .unwrap();
        server.pass().unwrap();
        server.pass().unwrap();
        let _ = drain_socket(held.first_mut().unwrap());

        let mut newcomer = UnixStream::connect(&path).unwrap();
        newcomer.write_all(&auth_packet()).unwrap();
        held.push(newcomer);
        server.pass().unwrap();
        server.pass().unwrap();
        assert!(
            server
                .clients
                .iter()
                .any(|client| client.session.client_index() == protected),
            "the streamless subscriber was treated as disposable idleness"
        );
        assert_eq!(server.client_count(), MAX_CLIENTS);
        server.shutdown();
    }

    /// Client indexes stop before Pulse's reserved INVALID value rather than
    /// reusing an identity after counter exhaustion.
    #[test]
    fn client_indexes_exhaust_before_the_reserved_value() {
        let (mut server, path) = server("client-index-wrap");
        server.next_client_index = u64::from(u32::MAX - 1);
        let first = UnixStream::connect(&path).unwrap();
        server.pass().unwrap();
        assert_eq!(
            server.clients.first().unwrap().session.client_index(),
            u32::MAX - 1
        );

        let second = UnixStream::connect(&path).unwrap();
        server.pass().unwrap();
        assert_eq!(server.client_count(), 1);
        assert_eq!(server.refused, 1, "the exhausted admission was refused");
        drop(first);
        drop(second);
        server.shutdown();
    }

    /// Refused reconnects remain observable, but only the daemon-global first
    /// few are written to stderr. A per-connection budget would reset on each
    /// attempt and let an untrusted local uid block the audio thread on logs.
    #[test]
    fn reconnect_floods_share_one_diagnostic_budget() {
        let path = socket_path("diagnostic-budget");
        let _ = std::fs::remove_file(&path);
        let mut server = Server::bind(
            &path,
            MemorySink::fixed(),
            Policy {
                allowed_uids: Vec::new(),
                applications: None,
            },
        )
        .unwrap();
        let attempts = MAX_DAEMON_DIAGNOSTICS + 8;
        for _ in 0..attempts {
            let connection = UnixStream::connect(&path).unwrap();
            server.pass().unwrap();
            drop(connection);
        }
        assert_eq!(server.refused, attempts as u32);
        assert_eq!(server.diagnostics.len(), MAX_DAEMON_DIAGNOSTICS);
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
}

//! The window process's side of the conversation processes (DESIGN.md
//! §2): it starts `td-agent conversation <id>` as its child over a framed
//! socketpair, sends it the model client's settings and the key first,
//! reads what the child says on a thread of its own so the window never
//! waits on it, and restarts the open conversation's child from its log
//! when it fails, resending the messages it had not yet acknowledged.
//!
//! A conversation has a process while it is open in the window or has
//! work running: switching away from a conversation mid-turn, or with a
//! message or retry sent and not yet started, leaves its process running
//! in the background, answered as before, until the turn ends; then its
//! socketpair is shut and it exits. Opening it again while
//! it runs adopts that process rather than starting a second, which its
//! directory's lock would refuse. A message from another conversation
//! wakes one with no process: it is started in the background for it.

use std::ffi::OsString;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use crate::frame;
use crate::key::Secret;
use crate::protocol::{Down, Up};
use crate::store::{random_hex, Effect, Id, Kind, Role};
use crate::workspace::Workspace;

/// Restarts in a row without a message acknowledged before the
/// conversation is left failed; opening it again tries afresh.
pub const MAX_RESTARTS: u32 = 3;
/// How long a write to a child may block before the child counts as
/// failed.
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a closing window waits for a child to exit on its own.
const EXIT_WAIT: Duration = Duration::from_secs(2);

/// What the window hears about a conversation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Update {
    /// A child's message, in order. A `Hello` starts the transcript over.
    Up(Up),
    /// The child refused a message: its text, when the window still had
    /// it, and why.
    Refused {
        text: Option<String>,
        reason: String,
    },
    /// The child failed and is being started again from its log.
    Restarting { reason: String },
    /// The child failed and will not be restarted until the conversation
    /// is opened again.
    Failed { reason: String },
    /// The child refused a message from another conversation, which is
    /// not offered again.
    Undeliverable { delivery: String, reason: String },
}

/// How a conversation was opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Opened {
    /// In a process started for it, which replays its log.
    Started,
    /// In the process already running its turn in the background, which
    /// replays nothing: the window reads the log itself.
    Adopted,
}

enum Incoming {
    Frame(Vec<u8>),
    Closed,
    Broken(String),
}

struct Running {
    id: Id,
    child: Child,
    writer: UnixStream,
    incoming: Receiver<Incoming>,
    /// The human's messages sent and not yet acknowledged, in order.
    pending: Vec<(String, String)>,
    /// Messages from other conversations handed to this child and not yet
    /// acknowledged, in order: each a `Down::Message`, by delivery id.
    deliveries: Vec<(String, Down)>,
    /// Restarts since the last acknowledgement.
    restarts: u32,
    failed: bool,
    /// The turn under way, by its start's sequence number.
    busy: Option<u64>,
    /// Told to resume and not yet heard resuming: a turn for what it
    /// held may be about to start, so it is kept.
    resuming: bool,
    /// The remotes it asked the window for and has not said prepared:
    /// a repository workspace being made ready, kept until it is.
    preparing: Vec<String>,
    /// Undos and redos asked of it and not yet heard done: its files may
    /// be half written, so it is kept until they are.
    restoring: u32,
    /// Its background processes running, by its log: none survives it,
    /// so it is kept while any runs (DESIGN.md §12).
    background: u32,
}

impl Running {
    /// Whether the child has work the window has not seen end: a turn
    /// under way, or a message sent whose turn it has not yet reported.
    /// Such a child is kept when its conversation is left, so a message
    /// sent just before a switch is answered, not abandoned.
    fn working(&self) -> bool {
        self.busy.is_some()
            || self.resuming
            || !self.preparing.is_empty()
            || self.restoring > 0
            || self.background > 0
            || !self.pending.is_empty()
            || !self.deliveries.is_empty()
    }
}

/// A count of background processes running, after its log's `kind`:
/// one more for a start, one fewer for an end (DESIGN.md §12).
fn background(running: u32, kind: &Kind) -> u32 {
    match kind {
        Kind::Process { .. } => running.saturating_add(1),
        Kind::Ended { .. } => running.saturating_sub(1),
        _ => running,
    }
}

/// The conversation processes the window owns.
pub struct Supervisor {
    program: PathBuf,
    state: PathBuf,
    /// What every child is sent first.
    setup: Down,
    children: Vec<Running>,
    open: Option<Id>,
    /// Children whose socketpair was closed, by conversation, reaped as
    /// they exit.
    retiring: Vec<(Id, Child, Instant)>,
    /// Messages a closed conversation had not acknowledged, sent again
    /// when it is opened again; its process logs each delivery id once.
    parked: Vec<(Id, Vec<(String, String)>)>,
    /// Variables set in each child's environment besides what it inherits.
    env: Vec<(OsString, OsString)>,
}

impl Supervisor {
    /// Children are `program conversation <id> --state-dir <state>`, each
    /// sent `setup` first.
    pub fn new(program: PathBuf, state: PathBuf, setup: Down) -> Self {
        Self {
            program,
            state,
            setup,
            children: Vec::new(),
            open: None,
            retiring: Vec::new(),
            parked: Vec::new(),
            env: Vec::new(),
        }
    }

    /// Sets `name` to `value` in every child's environment: the tests'
    /// fetch service, without changing their own process's.
    pub fn env(mut self, name: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((name.into(), value.into()));
        self
    }

    /// The conversations with a process that has not failed.
    pub fn ids(&self) -> Vec<Id> {
        self.children
            .iter()
            .filter(|r| !r.failed)
            .map(|r| r.id.clone())
            .collect()
    }

    /// Whether conversation `id` has a process that has not failed.
    pub fn running(&self, id: &Id) -> bool {
        self.at(id)
            .and_then(|at| self.children.get(at))
            .is_some_and(|r| !r.failed)
    }

    /// Whether conversation `id`'s process holds delivery `delivery`,
    /// handed to it and not yet acknowledged.
    pub fn holds(&self, id: &Id, delivery: &str) -> bool {
        self.at(id)
            .and_then(|at| self.children.get(at))
            .is_some_and(|r| r.deliveries.iter().any(|(d, _)| d == delivery))
    }

    /// Starts conversation `id`'s process in the background, for a
    /// message to wake it; the open conversation stays open.
    pub fn wake(&mut self, id: &Id) -> Result<(), String> {
        if self.at(id).is_some() {
            return Ok(());
        }
        let pending = match self.parked.iter().position(|(parked, _)| parked == id) {
            Some(at) => self.parked.swap_remove(at).1,
            None => Vec::new(),
        };
        let (child, mut writer, incoming) = match self.spawn(id, None) {
            Ok(spawned) => spawned,
            Err(e) => {
                if !pending.is_empty() {
                    self.parked.push((id.clone(), pending));
                }
                return Err(e);
            }
        };
        greet(&mut writer, &self.setup, &pending, &[]);
        self.children.push(Running {
            id: id.clone(),
            child,
            writer,
            incoming,
            pending,
            deliveries: Vec::new(),
            restarts: 0,
            failed: false,
            busy: None,
            preparing: Vec::new(),
            restoring: 0,
            background: 0,
            resuming: false,
        });
        Ok(())
    }

    /// Hands a message from another conversation, a `Down::Message`, to
    /// conversation `id`'s running process, which acknowledges it once
    /// logged; false when it has none.
    pub fn deliver(&mut self, id: &Id, delivery: String, down: Down) -> bool {
        let Some(running) = self.at(id).and_then(|at| self.children.get_mut(at)) else {
            return false;
        };
        if running.failed {
            return false;
        }
        let bytes = down.encode();
        running.deliveries.push((delivery, down));
        if frame::write(&mut running.writer, &bytes).is_err() {
            let _ = running.writer.shutdown(std::net::Shutdown::Both);
        }
        true
    }

    /// Sends `down` to the open conversation's process: a pause, or the
    /// todo list cleared.
    pub fn tell(&mut self, down: &Down) -> Result<(), String> {
        let running = self.opened().ok_or("no conversation is open")?;
        if running.failed {
            return Err("the conversation's process failed; open it again to restart it".into());
        }
        // A write that fails shuts the socketpair, and the child's going is
        // heard and said as any failure of its is.
        if frame::write(&mut running.writer, &down.encode()).is_err() {
            let _ = running.writer.shutdown(std::net::Shutdown::Both);
        }
        if matches!(down, Down::Pause { paused: false }) {
            running.resuming = true;
        }
        if matches!(down, Down::Restore { .. }) {
            running.restoring = running.restoring.saturating_add(1);
        }
        Ok(())
    }

    /// A key the human stored: every child started from now on is sent it
    /// first, and every running one gets it now, in a fresh `Setup`,
    /// which a conversation takes between turns (DESIGN.md §2, §6). It
    /// crosses the socketpairs and nothing else.
    pub fn rekey(&mut self, key: Secret) {
        if let Down::Setup { key: held, .. } = &mut self.setup {
            *held = Ok(key);
        }
        self.resend_setup();
    }

    /// Settings the window changed, the default model among them: as a
    /// stored key, every child gets them, now or when it starts.
    pub fn reconfigure(&mut self, client: crate::config::Client) {
        if let Down::Setup { client: held, .. } = &mut self.setup {
            *held = client;
        }
        self.resend_setup();
    }

    fn resend_setup(&mut self) {
        let bytes = self.setup.encode();
        for running in self.children.iter_mut().filter(|r| !r.failed) {
            if frame::write(&mut running.writer, &bytes).is_err() {
                let _ = running.writer.shutdown(std::net::Shutdown::Both);
            }
        }
    }

    /// The open conversation, when there is one.
    pub fn open_id(&self) -> Option<&Id> {
        self.open.as_ref()
    }

    fn at(&self, id: &Id) -> Option<usize> {
        self.children.iter().position(|r| &r.id == id)
    }

    fn opened(&mut self) -> Option<&mut Running> {
        let at = self.open.as_ref().and_then(|id| self.at(id))?;
        self.children.get_mut(at)
    }

    /// The open conversation's process id, while it runs.
    pub fn pid(&self) -> Option<u32> {
        let at = self.open.as_ref().and_then(|id| self.at(id))?;
        self.children
            .get(at)
            .filter(|r| !r.failed)
            .map(|r| r.child.id())
    }

    /// The conversations with a process working in the background.
    pub fn background(&self) -> Vec<Id> {
        self.children
            .iter()
            .filter(|r| Some(&r.id) != self.open.as_ref() && !r.failed)
            .map(|r| r.id.clone())
            .collect()
    }

    /// Opens conversation `id`, creating it as `create`, in a process of
    /// its own or the one already running its turn; the conversation open
    /// before is closed, or left running while its turn does.
    pub fn open(&mut self, id: Id, create: Option<Role>) -> Result<Opened, String> {
        self.open_as(id, create.map(|role| (role, None)))
    }

    /// Creates conversation `id` as `role`, working in `workspace`, and
    /// opens it, as `open` does.
    pub fn create(&mut self, id: Id, role: Role, workspace: Workspace) -> Result<Opened, String> {
        self.open_as(id, Some((role, Some(workspace))))
    }

    fn open_as(
        &mut self,
        id: Id,
        create: Option<(Role, Option<Workspace>)>,
    ) -> Result<Opened, String> {
        self.leave();
        if let Some(at) = self.at(&id) {
            let failed = self.children.get(at).is_some_and(|r| r.failed);
            if !failed {
                self.open = Some(id);
                return Ok(Opened::Adopted);
            }
            let running = self.children.swap_remove(at);
            self.retire(running);
        }
        let pending = match self.parked.iter().position(|(parked, _)| *parked == id) {
            Some(at) => self.parked.swap_remove(at).1,
            None => Vec::new(),
        };
        let (child, mut writer, incoming) = match self.spawn(&id, create.as_ref()) {
            Ok(spawned) => spawned,
            Err(e) => {
                if !pending.is_empty() {
                    self.parked.push((id, pending));
                }
                return Err(e);
            }
        };
        greet(&mut writer, &self.setup, &pending, &[]);
        self.children.push(Running {
            id: id.clone(),
            child,
            writer,
            incoming,
            pending,
            deliveries: Vec::new(),
            restarts: 0,
            failed: false,
            busy: None,
            preparing: Vec::new(),
            restoring: 0,
            background: 0,
            resuming: false,
        });
        self.open = Some(id);
        Ok(Opened::Started)
    }

    /// Leaves the open conversation: its process goes, unless it is
    /// working (`Running::working`), which goes on in the background.
    fn leave(&mut self) {
        let Some(id) = self.open.take() else {
            return;
        };
        if let Some(at) = self.at(&id) {
            let keep = self
                .children
                .get(at)
                .is_some_and(|r| r.working() && !r.failed);
            if !keep {
                let running = self.children.swap_remove(at);
                self.retire(running);
            }
        }
    }

    /// Shuts a child's socketpair, keeping the human's messages it had not
    /// acknowledged for its next process, and reaps it as it exits. The
    /// window's outbox keeps the other conversations' messages.
    fn retire(&mut self, mut running: Running) {
        let _ = running.writer.shutdown(std::net::Shutdown::Both);
        if running.child.try_wait().ok().flatten().is_none() {
            self.retiring.push((
                running.id.clone(),
                running.child,
                Instant::now() + EXIT_WAIT,
            ));
        }
        self.park(running.id, running.pending);
    }

    fn spawn(
        &self,
        id: &Id,
        create: Option<&(Role, Option<Workspace>)>,
    ) -> Result<(Child, UnixStream, Receiver<Incoming>), String> {
        let (ours, theirs) = UnixStream::pair().map_err(|e| format!("socketpair: {e}"))?;
        let input = theirs.try_clone().map_err(|e| format!("socketpair: {e}"))?;
        let mut command = Command::new(&self.program);
        command
            .envs(self.env.iter().map(|(name, value)| (name, value)))
            .arg("conversation")
            .arg(id.as_str())
            .arg("--state-dir")
            .arg(&self.state);
        if let Some((role, workspace)) = create {
            command.arg("--create").arg(role.word());
            if let Some(workspace) = workspace {
                command.arg("--workspace").arg(workspace.argument());
            }
        }
        let mut child = command
            .stdin(Stdio::from(OwnedFd::from(input)))
            .stdout(Stdio::from(OwnedFd::from(theirs)))
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("{}: {e}", self.program.display()))?;
        match listen(&ours) {
            Ok(incoming) => Ok((child, ours, incoming)),
            Err(e) => {
                // A child the window cannot hear is not left running.
                let _ = child.kill();
                let _ = child.wait();
                Err(e)
            }
        }
    }

    /// Sends the human's message to the open conversation, keeping it
    /// until the conversation acknowledges it.
    pub fn send(&mut self, text: String) -> Result<(), String> {
        let running = self.opened().ok_or("no conversation is open")?;
        if running.failed {
            return Err("the conversation's process failed; open it again to restart it".into());
        }
        let delivery = random_hex(16)?;
        let down = Down::User {
            delivery: delivery.clone(),
            text: text.clone(),
        };
        running.pending.push((delivery, text));
        if frame::write(&mut running.writer, &down.encode()).is_err() {
            // The message is pending: the reader sees the child go, and
            // the restart resends it, or the failure is reported then.
            let _ = running.writer.shutdown(std::net::Shutdown::Both);
        }
        Ok(())
    }

    /// Asks the open conversation to try its last turn again.
    pub fn retry(&mut self) -> Result<(), String> {
        let running = self.opened().ok_or("no conversation is open")?;
        if running.failed {
            return Err("the conversation's process failed; open it again to restart it".into());
        }
        if frame::write(&mut running.writer, &Down::Retry.encode()).is_err() {
            let _ = running.writer.shutdown(std::net::Shutdown::Both);
        }
        // Busy from now, so a switch before its `started` is heard keeps
        // the process; the window asks only after a failed turn, which
        // the child then starts again (its `started` replaces this).
        running.busy = running.busy.or(Some(0));
        Ok(())
    }

    /// Asks the open conversation to interrupt its turn.
    pub fn interrupt(&mut self) -> Result<(), String> {
        let running = self.opened().ok_or("no conversation is open")?;
        if running.failed {
            return Err("the conversation's process failed".into());
        }
        if frame::write(&mut running.writer, &Down::Interrupt.encode()).is_err() {
            let _ = running.writer.shutdown(std::net::Shutdown::Both);
        }
        Ok(())
    }

    /// Sends `down` to conversation `id`'s process: a reservation's
    /// answer, whether or not it is open.
    pub fn answer(&mut self, id: &Id, down: &Down) {
        let Some(running) = self.at(id).and_then(|at| self.children.get_mut(at)) else {
            return;
        };
        if frame::write(&mut running.writer, &down.encode()).is_err() {
            let _ = running.writer.shutdown(std::net::Shutdown::Both);
        }
    }

    /// What arrived since the last poll, by conversation; a failed open
    /// child is restarted from its log here, a failed one in the
    /// background left for its next opening, and a background child whose
    /// turn has ended is let go.
    pub fn poll(&mut self) -> Vec<(Id, Update)> {
        // A child told to go that has not gone by its deadline is killed:
        // it would hold its conversation's lock against the next one.
        let now = Instant::now();
        self.retiring.retain_mut(|(_, child, deadline)| {
            if child.try_wait().ok().flatten().is_some() {
                return false;
            }
            if now >= *deadline {
                let _ = child.kill();
                return child.try_wait().ok().flatten().is_none();
            }
            true
        });
        let mut updates = Vec::new();
        let mut failures = Vec::new();
        for running in &mut self.children {
            if running.failed {
                continue;
            }
            if let Some(reason) = drain(running, &mut updates) {
                failures.push((running.id.clone(), reason));
            }
        }
        for (id, reason) in failures {
            // A reservation the failed child asked for goes unanswered:
            // answered after the restart below, it would reach the new
            // child, whose own requests reuse no id but are not its.
            updates.retain(|(of, update)| {
                !(*of == id && matches!(update, Update::Up(Up::Reserve { .. })))
            });
            if self.open.as_ref() == Some(&id) {
                let update = self.restart(&id, reason);
                updates.push((id, update));
            } else if let Some(at) = self.at(&id) {
                let mut running = self.children.swap_remove(at);
                let _ = running.child.kill();
                let _ = running.child.wait();
                self.retire(running);
                updates.push((id, Update::Failed { reason }));
            }
        }
        // A background turn that has ended lets its process go.
        let done: Vec<usize> = (0..self.children.len())
            .rev()
            .filter(|at| {
                self.children.get(*at).is_some_and(|r| {
                    Some(&r.id) != self.open.as_ref() && (!r.working() || r.failed)
                })
            })
            .collect();
        for at in done {
            let running = self.children.swap_remove(at);
            self.retire(running);
        }
        updates
    }

    /// Starts conversation `id`'s process again after a failure, at most
    /// `MAX_RESTARTS` times in a row without an acknowledgement.
    fn restart(&mut self, id: &Id, reason: String) -> Update {
        let Some(at) = self.at(id) else {
            return Update::Failed { reason };
        };
        let spawned = {
            let Some(running) = self.children.get_mut(at) else {
                return Update::Failed { reason };
            };
            let _ = running.child.kill();
            let _ = running.child.wait();
            running.busy = None;
            // Its process gone, nothing is half written by it any more,
            // and the new one is not asked again; its background
            // processes went with it.
            running.restoring = 0;
            running.background = 0;
            if running.restarts >= MAX_RESTARTS {
                running.failed = true;
                return Update::Failed { reason };
            }
            running.restarts = running.restarts.saturating_add(1);
            self.spawn(id, None)
        };
        let Some(running) = self.children.get_mut(at) else {
            return Update::Failed { reason };
        };
        match spawned {
            Ok((child, writer, incoming)) => {
                running.child = child;
                running.writer = writer;
                running.incoming = incoming;
                greet(
                    &mut running.writer,
                    &self.setup,
                    &running.pending,
                    &running.deliveries,
                );
                Update::Restarting { reason }
            }
            Err(e) => {
                running.failed = true;
                Update::Failed {
                    reason: format!("{reason}; restarting it: {e}"),
                }
            }
        }
    }

    /// Ends conversation `id`'s processes, open, in the background or
    /// retiring, and waits for them, so its directory can go (DESIGN.md
    /// §4, deleting a conversation). The human's messages it had not
    /// acknowledged are handed back, oldest first, to be parked again
    /// should the deletion fail.
    pub fn remove(&mut self, id: &Id) -> Vec<(String, String)> {
        if self.open.as_ref() == Some(id) {
            self.open = None;
        }
        let mut held = Vec::new();
        if let Some(at) = self.parked.iter().position(|(parked, _)| parked == id) {
            held = self.parked.swap_remove(at).1;
        }
        if let Some(at) = self.at(id) {
            let mut running = self.children.swap_remove(at);
            let _ = running.child.kill();
            let _ = running.child.wait();
            held.extend(running.pending);
        }
        for (_, child, _) in self.retiring.iter_mut().filter(|(of, _, _)| of == id) {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.retiring.retain(|(of, _, _)| of != id);
        held
    }

    /// Keeps the human's messages `id` had not acknowledged for its next
    /// process.
    pub fn park(&mut self, id: Id, pending: Vec<(String, String)>) {
        if pending.is_empty() {
            return;
        }
        match self.parked.iter_mut().find(|(parked, _)| *parked == id) {
            Some((_, held)) => held.extend(pending),
            None => self.parked.push((id, pending)),
        }
    }

    /// Kills the open conversation's process, as a crash would.
    pub fn kill(&mut self) {
        if let Some(running) = self.opened() {
            let _ = running.child.kill();
        }
    }
}

/// What a child said since the last poll, into `updates`; a failure is
/// why the child must go.
fn drain(running: &mut Running, updates: &mut Vec<(Id, Update)>) -> Option<String> {
    loop {
        match running.incoming.try_recv() {
            Ok(Incoming::Frame(bytes)) => match Up::decode(&bytes) {
                Ok(Up::Refused { delivery, reason }) => {
                    running.restarts = 0;
                    // Another conversation's message: not offered again.
                    if let Some(at) = running.deliveries.iter().position(|(d, _)| *d == delivery) {
                        running.deliveries.remove(at);
                        updates.push((
                            running.id.clone(),
                            Update::Undeliverable { delivery, reason },
                        ));
                        continue;
                    }
                    // The message goes back to the window with why; a
                    // retry refused ends the busy it was marked with.
                    if running.busy == Some(0) {
                        running.busy = None;
                    }
                    let text = running
                        .pending
                        .iter()
                        .position(|(d, _)| *d == delivery)
                        .map(|at| running.pending.remove(at).1);
                    updates.push((running.id.clone(), Update::Refused { text, reason }));
                }
                Ok(up) => {
                    match &up {
                        Up::Delivered { delivery } => {
                            running.pending.retain(|(d, _)| d != delivery);
                            running.deliveries.retain(|(d, _)| d != delivery);
                            running.restarts = 0;
                        }
                        Up::Hello { .. } => {
                            running.busy = None;
                            running.resuming = false;
                            // Its log, replayed next, counts them again.
                            running.background = 0;
                            // A process started again asks again.
                            running.preparing.clear();
                        }
                        Up::Fetch { remote, .. } => {
                            if !running.preparing.contains(remote) {
                                running.preparing.push(remote.clone());
                            }
                        }
                        Up::Prepared { remote } => running.preparing.retain(|r| r != remote),
                        Up::Restored => running.restoring = running.restoring.saturating_sub(1),
                        Up::Event(event) => match event.kind {
                            // Logged after any turn it starts, whose start
                            // has marked the child busy.
                            Kind::Pause { paused: false } => running.resuming = false,
                            Kind::Started {
                                effect: Effect::Turn,
                                ..
                            } => running.busy = Some(event.seq),
                            Kind::Finished { started, .. } | Kind::Interrupted { started }
                                if running.busy == Some(started) =>
                            {
                                running.busy = None
                            }
                            Kind::Process { .. } | Kind::Ended { .. } => {
                                running.background = background(running.background, &event.kind)
                            }
                            _ => {}
                        },
                        _ => {}
                    }
                    updates.push((running.id.clone(), Update::Up(up)));
                }
                Err(e) => return Some(format!("it sent a malformed message: {e}")),
            },
            Ok(Incoming::Closed) | Err(TryRecvError::Disconnected) => {
                return Some(exit_reason(&mut running.child))
            }
            Ok(Incoming::Broken(e)) => return Some(e),
            Err(TryRecvError::Empty) => return None,
        }
    }
}

impl Drop for Supervisor {
    /// Every child is told to go by its socketpair closing, then given a
    /// moment to exit on its own before it is killed; a turn still running
    /// is interrupted, which its next start records.
    fn drop(&mut self) {
        self.open = None;
        for running in std::mem::take(&mut self.children) {
            self.retire(running);
        }
        // Nothing keeps these past the window: each is said, whole, so
        // what was typed is not lost without a word.
        for (id, pending) in &self.parked {
            for (_, text) in pending {
                eprintln!("td-agent: a message to conversation {id} was not delivered:\n{text}");
            }
        }
        let deadline = Instant::now() + EXIT_WAIT;
        for (_, child, _) in &mut self.retiring {
            while child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if child.try_wait().ok().flatten().is_none() {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
    }
}

/// Sets `ours` to time a stuck write out and starts the thread that
/// reads what the child says into the channel returned.
fn listen(ours: &UnixStream) -> Result<Receiver<Incoming>, String> {
    ours.set_write_timeout(Some(WRITE_TIMEOUT))
        .map_err(|e| format!("socketpair: {e}"))?;
    let mut reader = ours.try_clone().map_err(|e| format!("socketpair: {e}"))?;
    let (send, incoming) = mpsc::channel();
    std::thread::Builder::new()
        .name("td-agent-conversation".into())
        .spawn(move || loop {
            let message = match frame::read(&mut reader) {
                Ok(Some(bytes)) => Incoming::Frame(bytes),
                Ok(None) => Incoming::Closed,
                Err(e) => Incoming::Broken(e.to_string()),
            };
            let last = !matches!(message, Incoming::Frame(_));
            if send.send(message).is_err() || last {
                break;
            }
        })
        .map_err(|e| format!("reader thread: {e}"))?;
    Ok(incoming)
}

/// Sends a new child its settings, then the unacknowledged messages, the
/// human's and then other conversations', each in order. A write that
/// fails, a partial frame included, shuts the socketpair, so the reader
/// sees the child go and the restart runs again rather than the child
/// waiting on the rest of a frame.
fn greet(
    writer: &mut UnixStream,
    setup: &Down,
    pending: &[(String, String)],
    deliveries: &[(String, Down)],
) {
    let downs = std::iter::once(setup.encode())
        .chain(pending.iter().map(|(delivery, text)| {
            Down::User {
                delivery: delivery.clone(),
                text: text.clone(),
            }
            .encode()
        }))
        .chain(deliveries.iter().map(|(_, down)| down.encode()));
    for bytes in downs {
        if frame::write(writer, &bytes).is_err() {
            let _ = writer.shutdown(std::net::Shutdown::Both);
            return;
        }
    }
}

/// Why a child whose socketpair closed went: its exit, once it has one.
fn exit_reason(child: &mut Child) -> String {
    let deadline = Instant::now() + Duration::from_millis(100);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return format!("its process exited ({status})"),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => return "its process closed the socketpair".into(),
            Err(e) => return format!("its process: {e}"),
        }
    }
}

#[cfg(test)]
mod background_tests {
    use super::*;

    #[test]
    fn a_start_counts_one_more_and_an_end_one_fewer() {
        let start = Kind::Process {
            number: 1,
            call: 2,
            command: "make".into(),
        };
        let end = Kind::Ended {
            number: 1,
            how: "killed".into(),
        };
        assert_eq!(background(0, &start), 1);
        assert_eq!(background(1, &end), 0);
        // A replayed end after a restart reset the count stays at none.
        assert_eq!(background(0, &end), 0);
        assert_eq!(background(3, &Kind::Pause { paused: true }), 3);
    }
}

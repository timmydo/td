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
//! directory's lock would refuse.

use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use crate::frame;
use crate::protocol::{Down, Up};
use crate::store::{random_hex, Effect, Id, Kind, Role};

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
    /// Restarts since the last acknowledgement.
    restarts: u32,
    failed: bool,
    /// The turn under way, by its start's sequence number.
    busy: Option<u64>,
}

impl Running {
    /// Whether the child has work the window has not seen end: a turn
    /// under way, or a message sent whose turn it has not yet reported.
    /// Such a child is kept when its conversation is left, so a message
    /// sent just before a switch is answered, not abandoned.
    fn working(&self) -> bool {
        self.busy.is_some() || !self.pending.is_empty()
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
    /// Children whose socketpair was closed, reaped as they exit.
    retiring: Vec<(Child, Instant)>,
    /// Messages a closed conversation had not acknowledged, sent again
    /// when it is opened again; its process logs each delivery id once.
    parked: Vec<(Id, Vec<(String, String)>)>,
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
        let (child, mut writer, incoming) = match self.spawn(&id, create) {
            Ok(spawned) => spawned,
            Err(e) => {
                if !pending.is_empty() {
                    self.parked.push((id, pending));
                }
                return Err(e);
            }
        };
        greet(&mut writer, &self.setup, &pending);
        self.children.push(Running {
            id: id.clone(),
            child,
            writer,
            incoming,
            pending,
            restarts: 0,
            failed: false,
            busy: None,
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

    /// Shuts a child's socketpair, keeping what it had not acknowledged
    /// for its next process, and reaps it as it exits.
    fn retire(&mut self, mut running: Running) {
        let _ = running.writer.shutdown(std::net::Shutdown::Both);
        if !running.pending.is_empty() {
            self.parked.push((running.id, running.pending));
        }
        if running.child.try_wait().ok().flatten().is_none() {
            self.retiring
                .push((running.child, Instant::now() + EXIT_WAIT));
        }
    }

    fn spawn(
        &self,
        id: &Id,
        create: Option<Role>,
    ) -> Result<(Child, UnixStream, Receiver<Incoming>), String> {
        let (ours, theirs) = UnixStream::pair().map_err(|e| format!("socketpair: {e}"))?;
        let input = theirs.try_clone().map_err(|e| format!("socketpair: {e}"))?;
        let mut command = Command::new(&self.program);
        command
            .arg("conversation")
            .arg(id.as_str())
            .arg("--state-dir")
            .arg(&self.state);
        if let Some(role) = create {
            command.arg("--create").arg(role.word());
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
        self.retiring.retain_mut(|(child, deadline)| {
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
                greet(&mut running.writer, &self.setup, &running.pending);
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
                    // The message goes back to the window with why; a
                    // retry refused ends the busy it was marked with.
                    running.restarts = 0;
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
                            running.restarts = 0;
                        }
                        Up::Hello { .. } => running.busy = None,
                        Up::Event(event) => match event.kind {
                            Kind::Started {
                                effect: Effect::Turn,
                                ..
                            } => running.busy = Some(event.seq),
                            Kind::Finished { started, .. } | Kind::Interrupted { started }
                                if running.busy == Some(started) =>
                            {
                                running.busy = None
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
        for (child, _) in &mut self.retiring {
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

/// Sends a new child its settings, then the unacknowledged messages, in
/// order. A write that fails, a partial frame included, shuts the
/// socketpair, so the reader sees the child go and the restart runs again
/// rather than the child waiting on the rest of a frame.
fn greet(writer: &mut UnixStream, setup: &Down, pending: &[(String, String)]) {
    let downs = std::iter::once(setup.encode()).chain(pending.iter().map(|(delivery, text)| {
        Down::User {
            delivery: delivery.clone(),
            text: text.clone(),
        }
        .encode()
    }));
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

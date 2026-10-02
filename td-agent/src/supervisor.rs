//! The window process's side of the conversation processes (DESIGN.md
//! §2): it starts `td-agent conversation <id>` as its child over a framed
//! socketpair for the conversation open in the window, reads what the
//! child says on a thread of its own so the window never waits on it,
//! and restarts a child that fails from its log, resending the messages
//! it had not yet acknowledged. A conversation that is not open has no
//! process: switching away closes the socketpair, and the child exits.

use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use crate::frame;
use crate::protocol::{Down, Up};
use crate::store::{random_hex, Id, Role};

/// Restarts in a row without a message acknowledged before the
/// conversation is left failed; opening it again tries afresh.
pub const MAX_RESTARTS: u32 = 3;
/// How long a write to a child may block before the child counts as
/// failed.
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a closing window waits for a child to exit on its own.
const EXIT_WAIT: Duration = Duration::from_secs(2);

/// What the window hears about the open conversation.
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
}

/// The conversation processes the window owns.
pub struct Supervisor {
    program: PathBuf,
    state: PathBuf,
    running: Option<Running>,
    /// Children whose socketpair was closed, reaped as they exit.
    retiring: Vec<(Child, Instant)>,
    /// Messages a closed conversation had not acknowledged, sent again
    /// when it is opened again; its process logs each delivery id once.
    parked: Vec<(Id, Vec<(String, String)>)>,
}

impl Supervisor {
    /// Children are `program conversation <id> --state-dir <state>`.
    pub fn new(program: PathBuf, state: PathBuf) -> Self {
        Self {
            program,
            state,
            running: None,
            retiring: Vec::new(),
            parked: Vec::new(),
        }
    }

    /// The open conversation, when there is one.
    pub fn open_id(&self) -> Option<&Id> {
        self.running.as_ref().map(|r| &r.id)
    }

    /// The open conversation's process id, while it runs.
    pub fn pid(&self) -> Option<u32> {
        self.running
            .as_ref()
            .filter(|r| !r.failed)
            .map(|r| r.child.id())
    }

    /// Opens conversation `id`, creating it as `create`, in a process of
    /// its own; the conversation open before is closed.
    pub fn open(&mut self, id: Id, create: Option<Role>) -> Result<(), String> {
        self.close();
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
        resend(&mut writer, &pending);
        self.running = Some(Running {
            id,
            child,
            writer,
            incoming,
            pending,
            restarts: 0,
            failed: false,
        });
        Ok(())
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
        let running = self.running.as_mut().ok_or("no conversation is open")?;
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

    /// What arrived since the last poll; a failed child is restarted from
    /// its log here.
    pub fn poll(&mut self) -> Vec<Update> {
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
        let Some(running) = self.running.as_mut() else {
            return updates;
        };
        if running.failed {
            return updates;
        }
        let mut failure = None;
        loop {
            match running.incoming.try_recv() {
                Ok(Incoming::Frame(bytes)) => match Up::decode(&bytes) {
                    Ok(Up::Refused { delivery, reason }) => {
                        // The message goes back to the window with why.
                        running.restarts = 0;
                        let text = running
                            .pending
                            .iter()
                            .position(|(d, _)| *d == delivery)
                            .map(|at| running.pending.remove(at).1);
                        updates.push(Update::Refused { text, reason });
                    }
                    Ok(up) => {
                        if let Up::Delivered { delivery } = &up {
                            running.pending.retain(|(d, _)| d != delivery);
                            running.restarts = 0;
                        }
                        updates.push(Update::Up(up));
                    }
                    Err(e) => {
                        failure = Some(format!("it sent a malformed message: {e}"));
                        break;
                    }
                },
                Ok(Incoming::Closed) | Err(TryRecvError::Disconnected) => {
                    failure = Some(exit_reason(&mut running.child));
                    break;
                }
                Ok(Incoming::Broken(e)) => {
                    failure = Some(e);
                    break;
                }
                Err(TryRecvError::Empty) => break,
            }
        }
        if let Some(reason) = failure {
            updates.push(self.restart(reason));
        }
        updates
    }

    /// Starts the open conversation's process again after a failure, at
    /// most `MAX_RESTARTS` times in a row without an acknowledgement.
    fn restart(&mut self, reason: String) -> Update {
        let Some(running) = self.running.as_mut() else {
            return Update::Failed { reason };
        };
        let _ = running.child.kill();
        let _ = running.child.wait();
        if running.restarts >= MAX_RESTARTS {
            running.failed = true;
            return Update::Failed { reason };
        }
        running.restarts = running.restarts.saturating_add(1);
        let id = running.id.clone();
        match self.spawn(&id, None) {
            Ok((child, writer, incoming)) => {
                let Some(running) = self.running.as_mut() else {
                    return Update::Failed { reason };
                };
                running.child = child;
                running.writer = writer;
                running.incoming = incoming;
                resend(&mut running.writer, &running.pending);
                Update::Restarting { reason }
            }
            Err(e) => {
                if let Some(running) = self.running.as_mut() {
                    running.failed = true;
                }
                Update::Failed {
                    reason: format!("{reason}; restarting it: {e}"),
                }
            }
        }
    }

    /// Kills the open conversation's process, as a crash would.
    pub fn kill(&mut self) {
        if let Some(running) = self.running.as_mut() {
            let _ = running.child.kill();
        }
    }

    /// Closes the open conversation: its socketpair is shut, so its
    /// process exits, and is reaped as it does.
    pub fn close(&mut self) {
        if let Some(mut running) = self.running.take() {
            let _ = running.writer.shutdown(std::net::Shutdown::Both);
            if !running.pending.is_empty() {
                self.parked.push((running.id, running.pending));
            }
            if running.child.try_wait().ok().flatten().is_none() {
                self.retiring
                    .push((running.child, Instant::now() + EXIT_WAIT));
            }
        }
    }
}

impl Drop for Supervisor {
    /// Every child is told to go by its socketpair closing, then given a
    /// moment to exit on its own before it is killed.
    fn drop(&mut self) {
        self.close();
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

/// Sends the unacknowledged messages to a new child, in order. A write
/// that fails, a partial frame included, shuts the socketpair, so the
/// reader sees the child go and the restart runs again rather than the
/// child waiting on the rest of a frame.
fn resend(writer: &mut UnixStream, pending: &[(String, String)]) {
    for (delivery, text) in pending {
        let down = Down::User {
            delivery: delivery.clone(),
            text: text.clone(),
        };
        if frame::write(writer, &down.encode()).is_err() {
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

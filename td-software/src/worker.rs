//! The requests the window asks for, made on a thread of its own: each is
//! one connection to the compositor's control socket, one line written and
//! one bounded answer read, which can take a few seconds while the
//! compositor asks root.

use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

pub use crate::snapshot::Failure;
use crate::snapshot::{self, Operation, Snapshot};

/// A conversation's bound: the compositor's own five seconds and a margin.
const CONVERSATION: Duration = Duration::from_secs(7);

/// One request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Job {
    Status(String),
    Operation(Operation, String),
    Launch(String),
}

/// Its answer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Reply {
    Status(String, Box<Result<Snapshot, String>>),
    Operation(Operation, String, Result<(), Failure>),
    Launch(String, Result<(), String>),
}

/// The line a job writes.
pub fn line(job: &Job) -> String {
    match job {
        Job::Status(name) => format!("status {name}\n"),
        Job::Operation(operation, name) => format!("{} {name}\n", operation.word()),
        Job::Launch(name) => format!("launch {name}\n"),
    }
}

/// The reply to `job` for the answer `text`, or the failure to get one.
pub fn reply(job: Job, text: Result<String, Failure>) -> Reply {
    let body = text.and_then(|text| snapshot::answer(&text).map(str::to_string));
    match job {
        Job::Status(name) => {
            let snapshot = body
                .map_err(|failure| failure.reason)
                .and_then(|body| snapshot::parse(&name, &body));
            Reply::Status(name, Box::new(snapshot))
        }
        Job::Operation(operation, name) => Reply::Operation(operation, name, body.map(drop)),
        Job::Launch(name) => Reply::Launch(name, body.map(drop).map_err(|f| f.reason)),
    }
}

/// One conversation: the line written and the whole answer read, both
/// within `CONVERSATION` and the answer within its bound. The connect
/// itself has no bound std can set; it waits only while the control
/// socket's backlog is full.
fn ask(socket: &Path, line: &str) -> Result<String, Failure> {
    let unsent = |reason: String| Failure {
        reason,
        definite: true,
    };
    let unknown = |reason: String| Failure {
        reason,
        definite: false,
    };
    let late = || "the compositor did not answer in time".to_string();
    let deadline = Instant::now()
        .checked_add(CONVERSATION)
        .ok_or_else(|| unsent("the clock cannot bound the conversation".into()))?;
    let mut stream = UnixStream::connect(socket)
        .map_err(|error| unsent(format!("cannot reach the compositor: {error}")))?;
    let remaining = || {
        deadline
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
    };
    // A connect that waited out the bound has written nothing.
    let left = remaining().ok_or_else(|| unsent(late()))?;
    stream
        .set_write_timeout(Some(left))
        .map_err(|error| unsent(error.to_string()))?;
    // A write that fails part way may still have reached the compositor.
    stream
        .write_all(line.as_bytes())
        .map_err(|error| unknown(format!("cannot ask the compositor: {error}")))?;
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let mut answer = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        let left = remaining().ok_or_else(|| unknown(late()))?;
        stream
            .set_read_timeout(Some(left))
            .map_err(|error| unknown(error.to_string()))?;
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                answer.extend_from_slice(buffer.get(..count).unwrap_or_default());
                if answer.len() > snapshot::MAX_ANSWER_BYTES {
                    return Err(unknown("the compositor's answer is too large".into()));
                }
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                return Err(unknown(late()));
            }
            Err(error) => {
                return Err(unknown(format!(
                    "cannot read the compositor's answer: {error}"
                )))
            }
        }
    }
    if !answer.ends_with(b"\n") {
        return Err(unknown("the compositor's answer was cut short".into()));
    }
    String::from_utf8(answer).map_err(|_| unknown("the compositor's answer is not text".into()))
}

/// Runs one job against `socket`, or answers for a session without one.
pub fn run(socket: Option<&Path>, job: Job) -> Reply {
    let text = match socket {
        Some(socket) => ask(socket, &line(&job)),
        None => Err(Failure {
            reason: "no compositor control socket: td-software runs in a td session".into(),
            definite: true,
        }),
    };
    reply(job, text)
}

/// The thread that runs the jobs in order.
pub struct Worker {
    jobs: Sender<Job>,
    replies: Receiver<Reply>,
}

impl Worker {
    pub fn start(socket: Option<PathBuf>) -> Result<Self, String> {
        let (jobs, inbox) = mpsc::channel::<Job>();
        let (outbox, replies) = mpsc::channel();
        std::thread::Builder::new()
            .name("td-software-worker".into())
            .spawn(move || {
                for job in inbox {
                    if outbox.send(run(socket.as_deref(), job)).is_err() {
                        return;
                    }
                }
            })
            .map_err(|error| format!("cannot start the worker: {error}"))?;
        Ok(Self { jobs, replies })
    }

    pub fn send(&self, job: Job) -> Result<(), String> {
        self.jobs
            .send(job)
            .map_err(|_| "the worker has stopped".to_string())
    }

    pub fn try_recv(&self) -> Result<Option<Reply>, String> {
        match self.replies.try_recv() {
            Ok(reply) => Ok(Some(reply)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err("the worker has stopped".into()),
        }
    }
}

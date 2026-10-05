//! `td-agent conversation <id>`: one conversation's process (DESIGN.md
//! §2). The window process starts it over a socketpair handed to it as
//! its standard input and output. It locks the conversation's directory,
//! the only writer of it while it runs, replays the log up the socketpair,
//! and then serves the window's messages until the socketpair closes,
//! when it exits.
//!
//! A turn (DESIGN.md §5) begins with a message, the human's or another
//! conversation's, and is a loop of steps. Each step's request is
//! reserved against the turn's, the conversation's and, through the
//! window, the day's limits, logged as started and synced, and only then
//! sent through the fetch service as a stream. Its reply is drawn in the
//! window as it arrives and logged whole when it ends, with its usage; a
//! stream that breaks off, fails or is interrupted logs what it had
//! brought, marked incomplete. A whole reply's tool calls (`tools`) run
//! one at a time, in order, each logged as started and synced before it
//! runs and answered by exactly one result, and the next step sends them
//! back; a reply with no calls ends the turn, and so does `MAX_STEPS`. A
//! rate-limited request is asked again after a bounded wait; any other
//! failure ends the turn and says why, with a retry action where asking
//! again may succeed. After a conversation's first turn that replied its
//! title comes from `title_model`, reserved like any request and asked
//! for whole, not streamed.
//!
//! A message from another conversation, which the window routes, is
//! taken between turns. It starts a turn unless the human paused the
//! conversation or its wake budget is spent (`wake`), when it is logged
//! held, and the human is told once.
//!
//! The window's frames are read on a thread of their own into a channel,
//! so that the window's writes never wait on a request in flight; what
//! arrives during a turn waits its turn. A stream is read on a thread of
//! its own into the same channel, so an interrupt from the window is heard
//! between any two of its frames.

use std::collections::{BTreeMap, VecDeque};
use std::io::{self, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::accounts;
use crate::assemble::Assembly;
use crate::bench::Bench;
use crate::client::{self, Completion, Failure, Params};
use crate::config::Client;
use crate::cost;
use crate::frame;
use crate::history;
use crate::host;
use crate::key::Secret;
use crate::models::{Model, Models};
use crate::protocol::{Down, Fetched as Stored, Up, MAX_TEXT};
use crate::snapshot;
use crate::sse::{self, Fault};
use crate::store::{
    self, Basis, Call, Conversation, Effect, Event, Held, Id, Kind, Purpose, Role, StateDir,
    Status, TodoItem, LOCK_WAIT,
};
use crate::tools::{self, Args, Listed, Op, Reach};
use crate::wake;
use crate::workspace::{Entry, Workspace};

/// What a turn ends with when there is no window settings to make a
/// request with: the window sends them first, so only a harness that
/// does not sees this.
pub const NO_SETTINGS: &str = "no settings from the window";

/// The most a step snapshot may take, past which the turn tries none
/// again (DESIGN.md §12).
const SNAPSHOT_TIME: Duration = Duration::from_secs(60);

/// The most of a failure's reason a notification quotes.
const MAX_WHY: usize = 1000;

/// A failure's reason as a notification quotes it: what git, the remote
/// or the jail said, on one line with every control named and bounded,
/// so it can pass for nothing of td-agent's (DESIGN.md §7).
fn quoted(why: &str) -> String {
    crate::tools::visible(why).chars().take(MAX_WHY).collect()
}

/// The most background processes `process_list` shows, the latest, and
/// how much of each command.
const MAX_PROCESSES_LISTED: usize = 50;
const MAX_LISTED_COMMAND: usize = 200;

/// A commit as a notice names it.
fn short(id: &str) -> &str {
    id.get(..12).unwrap_or(id)
}

/// What a workspace tool says once its repository workspace went with
/// the conversation's archive (DESIGN.md §7).
const WORKSPACE_GONE: &str = "the workspace went with this conversation's archive: its worktrees are removed, so the file, shell and search tools are refused";
/// Where a refusal says the human chose the conversation's model.
const CHOSEN: &str = "the conversation's model (Conversation \u{2192} Model\u{2026})";
/// Where a conversation with no model of its own gets one (DESIGN.md §4).
const DEFAULT: &str = "`model` or the default model (Conversation \u{2192} Default model\u{2026})";
/// How long a request waits for the window to answer: a reservation, a
/// message queued, the conversations' states.
const ANSWER_WAIT: Duration = Duration::from_secs(30);
/// The most steps one turn takes, each a request answered by a reply; a
/// rate-limited request asked again is the same step (DESIGN.md §5).
pub const MAX_STEPS: usize = 40;
/// The most conversations `conversations` lists.
const MAX_LISTED: usize = 200;
/// What a call the human interrupted before it ran is answered with.
const CALL_SKIPPED: &str = "not run: the person interrupted the turn before this call ran";
/// What a call the human refused is answered with (DESIGN.md §11).
const CALL_REFUSED: &str = "not run: the person refused this call. That is their answer: do not try to reach the same result another way. Say what you needed it for and ask how they would like to go on";
/// What a call is answered with when the turn ended before the human
/// decided it.
const CALL_UNDECIDED: &str =
    "not run: the turn was interrupted, or the window closed, before the person decided this call";
/// How often a call in the jail looks for an interrupt.
const HOST_POLL: Duration = Duration::from_millis(100);
/// How long a cancelled call's tool host has to say it ended.
const CANCEL_GRACE: Duration = Duration::from_secs(5);
/// What a call is answered with when its tool host says nothing by the
/// call's deadline: the jail is torn down instead.
const CALL_UNANSWERED: &str = "the tool host did not end this call by its deadline, so its jail was torn down; whatever the call did before then stays";

/// What the log keeps of a call beside the result the model is shown
/// (`Kind::ToolResult`).
#[derive(Default)]
struct Beside {
    kept: Option<String>,
    digest: Option<String>,
}
/// What a call the log has no room to run is answered with.
const CALL_NO_ROOM: &str =
    "not run: the conversation's log is full; tell the person to start another conversation";
/// The log room one call takes when answered without running: its
/// `tool_call` and `tool_result` records at their longest, an id and a
/// name at their bounds escaped.
const CALL_RECORDS: u64 = 4096;
/// The log room one call's result may take at most: the longest line a
/// log holds, which `result` answers in place of anything longer.
const RESULT_ROOM: u64 = store::MAX_LINE as u64 + 1;

/// Runs the conversation over the socketpair on standard input and
/// output until it closes. An error is why it could not go on.
pub fn run(
    state: &StateDir,
    id: &Id,
    create: Option<Role>,
    workspace: Option<Workspace>,
) -> Result<(), String> {
    // The socketpair is standard input and output: one socket, taken as
    // a stream by duplicating the descriptor, which needs no `unsafe`.
    let socket = io::stdin()
        .as_fd()
        .try_clone_to_owned()
        .map_err(|e| format!("standard input: {e}"))?;
    serve_in(UnixStream::from(socket), state, id, create, workspace)
}

/// What the reader thread, and a stream's thread, hand on.
enum Inbound {
    Down(Down),
    Closed,
    Broken(String),
    /// What the stream of request `request` (its sequence number) brought.
    Fetch {
        request: u64,
        item: Fetched,
    },
    /// `remote`'s checkout, run on its own thread (`Checkout`), done: the
    /// bases and the commits their remote-tracking refs were set to, or
    /// why not.
    Checked {
        remote: String,
        result: Set,
    },
    /// Background process `number`, watched on its own thread (`watch`),
    /// ended, `how` as `process_list` says it.
    Ended {
        number: u64,
        how: String,
    },
}

/// What a checkout set: the bases and the commits their remote-tracking
/// refs were set to, or why not.
type Set = Result<Vec<(String, String)>, String>;

/// What an idle conversation does next: what the window sent, or a
/// checkout done.
enum Work {
    Down(Down),
    Checked(String, Set),
    Ended(u64, String),
}

/// One step of a streamed request, as its thread read it.
#[derive(Debug)]
enum Fetched {
    /// The reply's head.
    Head {
        status: u16,
        headers: Vec<(String, String)>,
    },
    /// A frame of its body.
    Chunk(Vec<u8>),
    /// The service said the body is whole.
    End,
    /// The request, or the stream, failed.
    Failed(td_fetch_client::Error),
}

/// What a streamed request came to.
enum Streamed {
    Replied(Completion),
    /// It failed, or was interrupted; `partial` is what it had brought.
    Failed {
        failure: Failure,
        partial: Option<Completion>,
    },
}

/// A streamed reply being read.
struct Reading {
    head: Option<(u16, Vec<(String, String)>)>,
    /// A reply that is no event stream, read whole: an error status's
    /// body, or a 200 that came as one JSON object.
    plain: Option<Vec<u8>>,
    reader: sse::Reader,
    reply: Assembly,
}

impl Reading {
    /// What the reply had brought, when anything worth keeping.
    fn partial(&self) -> Option<Completion> {
        (!self.reply.is_empty()).then(|| self.reply.completion())
    }

    /// The reply finished: whole, or a failure when a tool call came
    /// without what it needs.
    fn whole(&self) -> Streamed {
        match self.reply.whole() {
            Ok(completion) => Streamed::Replied(completion),
            Err(failure) => Streamed::Failed {
                failure,
                partial: self.partial(),
            },
        }
    }

    /// The stream broke off: charged and offered again.
    fn broken(&self, message: String) -> Streamed {
        Streamed::Failed {
            failure: Failure::Retryable {
                status: None,
                message,
                usage: self.reply.usage(),
            },
            partial: self.partial(),
        }
    }
}

/// The conversation over `stream`: opened, replayed, then served.
pub fn serve(
    stream: UnixStream,
    state: &StateDir,
    id: &Id,
    create: Option<Role>,
) -> Result<(), String> {
    serve_in(stream, state, id, create, None)
}

/// `serve`, creating the conversation in `workspace`; a workspace is
/// given only with the role it is created as.
pub fn serve_in(
    stream: UnixStream,
    state: &StateDir,
    id: &Id,
    create: Option<Role>,
    workspace: Option<Workspace>,
) -> Result<(), String> {
    let (conversation, load) = match (create, workspace) {
        (Some(role), workspace) => Conversation::create(state, id, role, workspace, LOCK_WAIT)?,
        (None, None) => Conversation::open(state, id, None, LOCK_WAIT)?,
        (None, Some(_)) => return Err("a workspace is given only to a new conversation".into()),
    };
    if conversation.meta().workspace.is_some() {
        if let Err(e) = crate::workspace::clear_specs(state, id) {
            eprintln!("td-agent: conversation {id}: {e}");
        }
    }
    if let Some(bytes) = load.torn {
        eprintln!("td-agent: conversation {id}: dropped a torn final log line of {bytes} bytes");
    }
    let reader = stream
        .try_clone()
        .map_err(|e| format!("the socketpair: {e}"))?;
    let (sender, inbox) = listen(reader)?;
    let mut session = Session {
        conversation,
        writer: stream,
        inbox,
        sender,
        live: Arc::new(AtomicU64::new(0)),
        interrupt: false,
        queue: VecDeque::new(),
        ended: None,
        setup: None,
        state: state.root().to_path_buf(),
        // Random, so no two processes of one conversation share an id,
        // and the window's ledger never takes one's grant for another's.
        next_reservation: u64::from_str_radix(&crate::store::random_hex(6)?, 16)
            .map_err(|e| format!("a reservation id: {e}"))?,
        gone: false,
        bench: Bench::default(),
        awaiting: Vec::new(),
        untracked: Vec::new(),
        checking: Vec::new(),
        unsnapped: false,
        snapless: false,
        checked: VecDeque::new(),
        processes: BTreeMap::new(),
        exited: VecDeque::new(),
    };
    // What this conversation read or wrote before, so a replacement of an
    // unchanged file needs no read again.
    session.bench.restore(session.conversation.events());
    session.send(&Up::Hello {
        title: session.conversation.meta().title.clone(),
        torn: load.torn,
        interrupted: load.interrupted,
        paused: session.conversation.meta().paused,
        prefix: hello_prefix(session.conversation.prefix_file()),
    });
    for event in session.conversation.events().to_vec() {
        session.send(&Up::Event(event));
    }
    session.ask_stores();
    session.serve()
}

/// Watches background call `call` on `client` until it ends, `killed`
/// says to kill it or is dropped, or `limit` passes: how it ended, as
/// `process_list` says it. Its instance ends with `client`.
fn watch(mut client: host::Client, call: u64, limit: Duration, killed: &Receiver<()>) -> String {
    let deadline = Instant::now().checked_add(limit);
    loop {
        match killed.try_recv() {
            Ok(()) | Err(mpsc::TryRecvError::Disconnected) => return "killed".into(),
            Err(mpsc::TryRecvError::Empty) => {}
        }
        if deadline.is_some_and(|at| Instant::now() >= at) {
            return "timed out".into();
        }
        match client.next_reply(HOST_POLL) {
            None | Some(Ok(host::Up::Output { .. })) => {}
            Some(Ok(host::Up::Done { id, outcome })) if id == call => {
                return match outcome {
                    Ok(done) => done.text,
                    Err(why) => format!("failed: {why}"),
                }
            }
            Some(Ok(host::Up::Done { .. })) => {}
            Some(Err(why)) => return format!("failed: {why}"),
        }
    }
}

/// The prefix a hello carries: none past `MAX_TEXT`, which keeps the
/// frame within its bound however JSON escapes it.
fn hello_prefix(prefix: &str) -> Option<String> {
    (prefix.len() <= MAX_TEXT).then(|| prefix.to_string())
}

/// Reads the window's frames into a channel until the socketpair ends;
/// the channel's sender is kept for the streams to hand on through.
fn listen(mut reader: UnixStream) -> Result<(Sender<Inbound>, Receiver<Inbound>), String> {
    let (send, inbox) = mpsc::channel();
    let sender = send.clone();
    std::thread::Builder::new()
        .name("td-agent-window".into())
        .spawn(move || loop {
            let inbound = match frame::read(&mut reader) {
                Ok(Some(bytes)) => match Down::decode(&bytes) {
                    Ok(down) => Inbound::Down(down),
                    Err(e) => Inbound::Broken(format!("a malformed message: {e}")),
                },
                // A window that closes with our frames unread resets the
                // socketpair: closed all the same.
                Ok(None) => Inbound::Closed,
                Err(frame::Error::Io(e)) if e.kind() == io::ErrorKind::ConnectionReset => {
                    Inbound::Closed
                }
                Err(e) => Inbound::Broken(e.to_string()),
            };
            let last = !matches!(inbound, Inbound::Down(_));
            if send.send(inbound).is_err() || last {
                break;
            }
        })
        .map_err(|e| format!("the reader thread: {e}"))?;
    Ok((sender, inbox))
}

/// A stream's thread: sends `body` as a streamed request and hands on its
/// head and each frame as they come, for as long as `live` names it.
/// Returning drops the stream, which closes its connection, and with it
/// the service's to the origin: an interrupt takes effect at the frame
/// after it, which the service's idle deadline bounds (DESIGN.md §5).
fn fetch(
    url: &str,
    headers: &[(&'static str, String)],
    body: &[u8],
    request: u64,
    live: &AtomicU64,
    send: &Sender<Inbound>,
) {
    let headers: Vec<(&str, &str)> = headers.iter().map(|(n, v)| (*n, v.as_str())).collect();
    let hand = |item: Fetched| send.send(Inbound::Fetch { request, item }).is_ok();
    let mut stream =
        match td_fetch_client::post_stream(url, &headers, body, Some(client::MAX_STREAM)) {
            Ok(stream) => stream,
            Err(e) => {
                hand(Fetched::Failed(e));
                return;
            }
        };
    let head = Fetched::Head {
        status: stream.status,
        headers: stream.headers.clone(),
    };
    if !hand(head) {
        return;
    }
    while live.load(Ordering::SeqCst) == request {
        let item = match stream.next_chunk() {
            Ok(Some(bytes)) => Fetched::Chunk(bytes.to_vec()),
            Ok(None) => Fetched::End,
            Err(e) => Fetched::Failed(e),
        };
        let last = !matches!(item, Fetched::Chunk(_));
        if !hand(item) || last {
            return;
        }
    }
}

/// How a step, or the turn, ended: what the window shows, whether the
/// human may ask for it again, whether the model replied, and the logged
/// reply whose tool calls are to run, when it asked for any.
struct Outcome {
    text: String,
    retry: bool,
    replied: bool,
    calls: Option<u64>,
}

impl Outcome {
    fn stop(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            retry: false,
            replied: false,
            calls: None,
        }
    }

    fn again(text: impl Into<String>) -> Self {
        Self {
            retry: true,
            ..Self::stop(text)
        }
    }
}

/// What a request was charged, and how that was known.
fn charge(
    usage: Option<client::Usage>,
    pricing: Option<cost::Pricing>,
    reserved: u64,
) -> (u64, Basis) {
    match (usage, pricing) {
        (
            Some(client::Usage {
                cost: Some(cost), ..
            }),
            _,
        ) => (cost, Basis::Reported),
        (Some(usage), Some(pricing)) => (pricing.charge(&usage.tokens), Basis::Computed),
        _ => (reserved, Basis::Reserved),
    }
}

struct Session {
    conversation: Conversation,
    writer: UnixStream,
    inbox: Receiver<Inbound>,
    /// The inbox's sender, which each stream's thread hands on through.
    sender: Sender<Inbound>,
    /// The request whose stream is still wanted, 0 for none: a stream's
    /// thread reads on only while this names its request.
    live: Arc<AtomicU64>,
    /// The window asked to interrupt the turn under way.
    interrupt: bool,
    /// Messages that came while a turn waited on the window.
    queue: VecDeque<Down>,
    /// The window's end, found while taking what came into `queue`: closed,
    /// or why it broke. Taken once the queue is empty.
    ended: Option<Result<(), String>>,
    setup: Option<(Result<Secret, String>, Client)>,
    state: PathBuf,
    /// The last reservation id asked for.
    next_reservation: u64,
    /// The window has closed its end: the turn under way finishes in the
    /// log, and then the process exits.
    gone: bool,
    /// The workspace's jail instances, for a conversation in one.
    bench: Bench,
    /// The remotes this process asked the window for and has no answer
    /// for yet: a turn's first request waits for them (`await_stores`).
    awaiting: Vec<String>,
    /// Each remote whose remote-tracking refs last failed to be set, with
    /// the commits tried and what it said: the same commits are not
    /// tried again until they change or the process starts again, and a
    /// failure is logged once until it says something else.
    untracked: Vec<Untracked>,
    /// The remotes whose preparation began and whose end is not yet
    /// taken up (`prepare`, `done_with`): a second answer waits for it.
    checking: Vec<String>,
    /// A step snapshot failed and was said; it is said once.
    unsnapped: bool,
    /// A step snapshot failed this turn: none is tried again in it, so a
    /// slow or failing one costs each turn once.
    snapless: bool,
    /// Checkouts done, with what each set or why not, kept until a turn's
    /// next step or the turn's end (`between`, `woken`).
    checked: VecDeque<(String, Set)>,
    /// The background processes running, by number, each with what tells
    /// its watcher to kill it (DESIGN.md §12); dropped, it does.
    processes: BTreeMap<u64, mpsc::Sender<()>>,
    /// Background processes that ended, by number with how, kept until a
    /// turn's next step or the turn's end (`between`).
    exited: VecDeque<(u64, String)>,
}

/// A remote whose remote-tracking refs could not be set to `tried`, and
/// what that said.
struct Untracked {
    remote: String,
    tried: Vec<(String, String)>,
    said: String,
}

impl Session {
    /// Sends `up` to the window; a window gone is noted, not an error, so
    /// a turn under way still finishes whole in the log.
    fn send(&mut self, up: &Up) {
        if self.gone {
            return;
        }
        if let Err(e) = frame::write(&mut self.writer, &up.encode()) {
            if !matches!(
                e.kind(),
                io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
            ) {
                eprintln!("td-agent: the window: {e}");
            }
            self.gone = true;
        }
    }

    /// Appends `kind` to the log and sends it up.
    fn log(&mut self, kind: Kind) -> Result<Event, String> {
        let event = self.conversation.append(kind)?.clone();
        self.send(&Up::Event(event.clone()));
        Ok(event)
    }

    fn sync(&mut self) -> Result<(), String> {
        self.conversation.sync()
    }

    fn next(&mut self) -> Result<Option<Work>, String> {
        // What came meanwhile joins the queue first, so that `take` can
        // choose: a setup first, then a pause sent after messages from
        // other conversations, while a turn ran, before them, holding
        // them (DESIGN.md §2, §3).
        while self.ended.is_none() {
            match self.inbox.try_recv() {
                Ok(Inbound::Down(down)) => self.queue.push_back(down),
                Ok(Inbound::Fetch { .. }) => {}
                Ok(Inbound::Checked { remote, result }) => self.checked.push_back((remote, result)),
                Ok(Inbound::Ended { number, how }) => self.exit(number, how),
                Ok(Inbound::Closed) => self.ended = Some(Ok(())),
                Ok(Inbound::Broken(e)) => self.ended = Some(Err(e)),
                Err(_) => break,
            }
        }
        // What the window sent first: a turn it starts reads a checkout
        // done meanwhile between its steps.
        if let Some(down) = take(&mut self.queue) {
            return Ok(Some(Work::Down(down)));
        }
        if let Some((remote, result)) = self.checked.pop_front() {
            return Ok(Some(Work::Checked(remote, result)));
        }
        if let Some((number, how)) = self.exited.pop_front() {
            return Ok(Some(Work::Ended(number, how)));
        }
        match self.ended.take() {
            Some(Ok(())) => return Ok(None),
            Some(Err(e)) => return Err(format!("the window: {e}")),
            None => {}
        }
        loop {
            match self.inbox.recv() {
                Ok(Inbound::Down(down)) => return Ok(Some(Work::Down(down))),
                Ok(Inbound::Checked { remote, result }) => {
                    return Ok(Some(Work::Checked(remote, result)))
                }
                Ok(Inbound::Ended { number, how }) => return Ok(Some(Work::Ended(number, how))),
                Ok(Inbound::Closed) | Err(_) => return Ok(None),
                Ok(Inbound::Broken(e)) => return Err(format!("the window: {e}")),
                // A stream given up on, still reading to its next frame.
                Ok(Inbound::Fetch { .. }) => {}
            }
        }
    }

    fn serve(&mut self) -> Result<(), String> {
        while !self.gone {
            let down = match self.next()? {
                None => return Ok(()),
                Some(Work::Checked(remote, result)) => {
                    self.woken(remote, result)?;
                    if !self.gone {
                        let _ = self.writer.flush();
                    }
                    continue;
                }
                Some(Work::Ended(number, how)) => {
                    self.ended(number, how)?;
                    // Durable, or the next open says it was lost.
                    self.sync()?;
                    if !self.gone {
                        let _ = self.writer.flush();
                    }
                    continue;
                }
                Some(Work::Down(down)) => down,
            };
            match down {
                Down::Setup { key, client } => self.setup = Some((key, client)),
                Down::User { delivery, text } => self.user(delivery, text)?,
                Down::Message {
                    delivery,
                    from,
                    role,
                    text,
                    status,
                } => self.message(delivery, from, role, text, status)?,
                Down::Retry => self.retry()?,
                Down::Pause { paused } => self.pause(paused)?,
                Down::ClearTodo => self.clear_todo()?,
                Down::Restore { step, undo } => self.restore(step, undo)?,
                Down::Choose { model, effort } => self.choose(model, effort)?,
                Down::Fetched { remote, result } => self.stored(remote, result)?,
                Down::Heads { remote, bases, ids } => self.heads(&remote, &bases, &ids)?,
                // A reservation granted after its request gave up waiting:
                // nothing was sent, so the window's hold is released.
                Down::Reservation { id, refusal: None } => self.spent(id, 0),
                // Between turns there is nothing to interrupt, and an
                // answer that comes after its call gave up is not waited
                // for.
                // A decision that comes after its call gave up is not
                // waited for either.
                Down::Reservation { .. }
                | Down::Interrupt
                | Down::Sent { .. }
                | Down::States { .. }
                | Down::Decision { .. } => {}
            }
            if !self.gone {
                let _ = self.writer.flush();
            }
        }
        Ok(())
    }

    fn user(&mut self, delivery: String, text: String) -> Result<(), String> {
        if self.conversation.delivered(&delivery) {
            // A message the window sent again after a restart: logged
            // once, acknowledged each time.
            self.send(&Up::Delivered { delivery });
            return Ok(());
        }
        let refused = if text.len() > MAX_TEXT {
            Some(format!("a message is at most {MAX_TEXT} bytes"))
        } else if text.trim().is_empty() {
            Some("an empty message".to_string())
        } else if !self.conversation.has_room(text.len()) {
            Some("the conversation's log is full; start another conversation".to_string())
        } else {
            None
        };
        if let Some(reason) = refused {
            self.send(&Up::Refused { delivery, reason });
            return Ok(());
        }
        // A message from the human resumes a paused conversation; what
        // was held meanwhile is in the log before it, and in its turn.
        self.resume()?;
        // Titled by its first message, or by the next one should the
        // title not have been written then, until a title model's.
        let untitled = self.conversation.meta().title == Role::Conversation.first_title();
        let user = self.conversation.append(Kind::User {
            delivery: delivery.clone(),
            text: text.clone(),
        })?;
        let of = user.seq;
        let user = user.clone();
        let started = self
            .conversation
            .append(Kind::Started {
                effect: Effect::Turn,
                of,
            })?
            .clone();
        // The started record is durable before the turn runs.
        self.sync()?;
        self.send(&Up::Event(user));
        self.send(&Up::Event(started.clone()));
        self.send(&Up::Delivered { delivery });
        if untitled {
            self.conversation.retitle(&text)?;
            let title = self.conversation.meta().title.clone();
            self.send(&Up::Title { title });
        }
        self.turn(started.seq)
    }

    /// The last turn again, when it ended in a failure that may pass.
    fn retry(&mut self) -> Result<(), String> {
        let events = self.conversation.events();
        let last = events.iter().rev().find_map(|e| match e.kind {
            Kind::Started {
                effect: Effect::Turn,
                of,
            } => Some((e.seq, of)),
            _ => None,
        });
        let retryable = last.filter(|(started, _)| {
            events.iter().any(
                |e| matches!(e.kind, Kind::Finished { started: s, retry: true, .. } if s == *started),
            )
        });
        let Some((_, of)) = retryable else {
            // Said, so the window stops counting this process busy.
            self.send(&Up::Refused {
                delivery: String::new(),
                reason: "there is no failed turn to ask again".into(),
            });
            return Ok(());
        };
        // The human asking again resumes a paused conversation.
        self.resume()?;
        let started = self.log(Kind::Started {
            effect: Effect::Turn,
            of,
        })?;
        self.sync()?;
        self.turn(started.seq)
    }

    /// Runs turn `turn` to its end in the log.
    fn turn(&mut self, turn: u64) -> Result<(), String> {
        self.interrupt = false;
        self.snapless = false;
        let outcome = self.steps(turn)?;
        if outcome.replied && self.first_reply(turn) {
            self.title(turn)?;
        }
        self.log(Kind::Finished {
            started: turn,
            outcome: outcome.text,
            retry: outcome.retry,
        })?;
        // A turn boundary.
        self.sync()
    }

    /// The turn's steps: a request, then its reply's tool calls, until a
    /// reply asks for none or `MAX_STEPS` steps have been taken. The
    /// outcome says the model replied when any step did, so a turn
    /// that ends after a whole reply, however it ends, is titled as the
    /// first that replied (`first_reply`) and none later is left untitled.
    fn steps(&mut self, turn: u64) -> Result<Outcome, String> {
        if let Some(outcome) = self.await_stores()? {
            return Ok(outcome);
        }
        let mut replied = false;
        for _ in 0..MAX_STEPS {
            self.between()?;
            let outcome = self.exchange(turn)?;
            let Some(reply) = outcome.calls else {
                return Ok(Outcome {
                    replied: replied || outcome.replied,
                    ..outcome
                });
            };
            replied = true;
            if !self.answer(reply)? {
                return Ok(Outcome {
                    replied,
                    ..Outcome::again(
                        "interrupted between tool calls; each call that had not run is answered as not run",
                    )
                });
            }
        }
        // Every step replied, or the loop would have ended.
        Ok(Outcome {
            replied: true,
            ..Outcome::stop(format!(
                "stopped after {MAX_STEPS} steps, each a reply the model gave, the most one turn takes; every tool call has its result, and a message goes on from there"
            ))
        })
    }

    /// Whether `turn` is the first of the conversation's turns begun by the
    /// human to have a whole reply, and it has no title from a model yet.
    /// A turn a message from another conversation began does not count,
    /// since the title quotes the human's first message (§13).
    fn first_reply(&self, turn: u64) -> bool {
        // One made as the orchestrator, by an older td-agent, keeps the
        // title it was made with.
        if self.conversation.meta().role != Role::Conversation {
            return false;
        }
        let events = self.conversation.events();
        let mut users: Vec<u64> = Vec::new();
        let mut human: Vec<u64> = Vec::new();
        let mut requests: Vec<(u64, u64)> = Vec::new();
        let mut replied: Vec<u64> = Vec::new();
        for event in events {
            match &event.kind {
                Kind::Title { .. } => return false,
                Kind::User { .. } => users.push(event.seq),
                Kind::Started {
                    effect: Effect::Turn,
                    of,
                } if users.contains(of) => human.push(event.seq),
                Kind::Request {
                    purpose: Purpose::Turn,
                    turn,
                    ..
                } => requests.push((event.seq, *turn)),
                Kind::Assistant {
                    request,
                    incomplete: false,
                    ..
                } => {
                    if let Some((_, of)) = requests.iter().find(|(seq, _)| seq == request) {
                        if human.contains(of) && !replied.contains(of) {
                            replied.push(*of);
                        }
                    }
                }
                _ => {}
            }
        }
        replied == [turn]
    }

    /// The models cache and the entry for `model`, which `key` names (a
    /// configuration key in backquotes, or `CHOSEN`); a refusal says why a
    /// request cannot be made with them.
    fn model(&self, key: &str, model: &str, client: &Client) -> Result<Option<Model>, String> {
        let models = Models::load(&self.state)?;
        let entry = models.as_ref().and_then(|m| m.find(model)).cloned();
        if models.is_some() && entry.is_none() {
            return Err(format!(
                "{model} is not in the provider's models list; set {key} to one that is"
            ));
        }
        if client.limits.any() && entry.as_ref().and_then(|m| m.pricing).is_none() {
            let why = if models.is_none() {
                " (the models list has not been fetched yet)"
            } else {
                ""
            };
            return Err(format!(
                "no price is known for {model}{why}; a model without pricing is refused while a cost limit is set"
            ));
        }
        Ok(entry)
    }

    /// Asks the window to reserve `amount` against the day: the request's
    /// id when granted, or why not.
    fn reserve(&mut self, amount: u64) -> Result<u64, String> {
        let id = self.ask_id();
        self.send(&Up::Reserve { id, amount });
        match self.wait(|down| matches!(down, Down::Reservation { id: a, .. } if *a == id))? {
            Down::Reservation { refusal, .. } => refusal.map_or(Ok(id), Err),
            _ => Err("the window answered something else".into()),
        }
    }

    /// A fresh id for something asked of the window.
    fn ask_id(&mut self) -> u64 {
        self.next_reservation = self.next_reservation.wrapping_add(1);
        self.next_reservation
    }

    /// Waits for the window's answer, `wanted`, still hearing it: an
    /// interrupt is noted for the turn, an earlier request's grant that
    /// came after it gave up is released, and anything else waits its
    /// turn. Why not, when the window closes or does not answer.
    fn wait(&mut self, wanted: impl Fn(&Down) -> bool) -> Result<Down, String> {
        if self.gone {
            return Err("the window has closed".into());
        }
        let deadline = Instant::now() + ANSWER_WAIT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.inbox.recv_timeout(left) {
                Ok(Inbound::Down(down)) if wanted(&down) => return Ok(down),
                // An earlier request's grant, come after it gave up.
                Ok(Inbound::Down(Down::Reservation {
                    id: late,
                    refusal: None,
                })) => self.spent(late, 0),
                // Said while waiting: the turn ends at its next step.
                Ok(Inbound::Down(Down::Interrupt)) => self.interrupt = true,
                Ok(Inbound::Down(down)) => self.queue.push_back(down),
                Ok(Inbound::Fetch { .. }) => {}
                Ok(Inbound::Checked { remote, result }) => self.checked.push_back((remote, result)),
                Ok(Inbound::Ended { number, how }) => self.exit(number, how),
                Ok(Inbound::Closed | Inbound::Broken(_)) | Err(RecvTimeoutError::Disconnected) => {
                    self.gone = true;
                    return Err("the window has closed".into());
                }
                Err(RecvTimeoutError::Timeout) => {
                    return Err(format!(
                        "the window did not answer in {} s",
                        ANSWER_WAIT.as_secs()
                    ))
                }
            }
        }
    }

    /// What the window said meanwhile, without waiting: an interrupt is
    /// noted, a late grant released, and the rest waits its turn.
    fn hear(&mut self) {
        while let Ok(inbound) = self.inbox.try_recv() {
            match inbound {
                Inbound::Down(Down::Interrupt) => self.interrupt = true,
                Inbound::Down(Down::Reservation { id, refusal: None }) => self.spent(id, 0),
                Inbound::Down(down) => self.queue.push_back(down),
                Inbound::Fetch { .. } => {}
                Inbound::Checked { remote, result } => self.checked.push_back((remote, result)),
                Inbound::Ended { number, how } => self.exit(number, how),
                Inbound::Closed | Inbound::Broken(_) => self.gone = true,
            }
        }
    }

    /// Tells the window what request `id` cost.
    fn spent(&mut self, id: u64, amount: u64) {
        self.send(&Up::Spent { id, amount });
    }

    /// Logs a request's end: its usage, then its finish.
    fn settle(
        &mut self,
        request: u64,
        usage: Option<client::Usage>,
        cost: (u64, Basis),
        outcome: String,
    ) -> Result<(), String> {
        self.log(Kind::Usage {
            request,
            tokens: usage.map(|u| u.tokens).unwrap_or_default(),
            cost: cost.0,
            basis: cost.1,
        })?;
        self.log(Kind::Finished {
            started: request,
            outcome,
            retry: false,
        })?;
        Ok(())
    }

    /// The turn's exchange with the model, retrying a rate limit.
    fn exchange(&mut self, turn: u64) -> Result<Outcome, String> {
        let Some((key, client)) = self.setup.clone() else {
            return Ok(Outcome::stop(NO_SETTINGS));
        };
        let key = match key {
            Ok(key) => key,
            Err(why) => return Ok(Outcome::stop(why)),
        };
        // The human's choice for this conversation, else the default:
        // the configuration's, or the window's (§4).
        let meta = self.conversation.meta();
        let (chosen, chosen_effort) = (meta.model.clone(), meta.effort.clone());
        let setting = if chosen.is_some() { CHOSEN } else { DEFAULT };
        let name = chosen.unwrap_or_else(|| client.model.clone());
        let reasoning_effort = chosen_effort.unwrap_or_else(|| client.reasoning_effort.clone());
        let model = match self.model(setting, &name, &client) {
            Ok(model) => model,
            Err(why) => return Ok(Outcome::stop(why)),
        };
        // Every request carries the tools and `max_tokens`, which bounds
        // what it may cost, and `require_parameters` would route one to no
        // provider of a model that does not list both (§5).
        if let Some(missing) = model
            .as_ref()
            .and_then(|m| ["tools", "max_tokens"].into_iter().find(|p| !m.supports(p)))
        {
            return Ok(Outcome::stop(format!(
                "{name} takes no {missing} (the provider's models list gives it no `{missing}` parameter); set {setting} to a model that does"
            )));
        }
        let pricing = model.as_ref().and_then(|m| m.pricing);
        // The prefix this program writes; a conversation begun by another
        // (one from before the conversation tools, say), or whose shared
        // directories changed, takes it as an event, at the cost of one
        // cache miss. A workspace conversation's first request takes one,
        // since its creation does not know the shared directories.
        let expected = match self.expected_prefix(&client) {
            Ok(expected) => expected,
            Err(why) => return Ok(Outcome::stop(why)),
        };
        if client::current_prefix(self.conversation.events(), self.conversation.prefix_file()).1
            != expected
        {
            // One past the log's line ends the turn, not the process.
            if let Err(why) = self.log(Kind::Prefix { text: expected }) {
                return Ok(Outcome::stop(format!(
                    "the conversation's prefix could not be logged: {why}"
                )));
            }
        }
        let mut attempt = 0u32;
        loop {
            // A checkout done while a request waited to be asked again is
            // taken up before it is, its worktrees bound for this step's
            // calls; the prefix does not depend on them.
            if attempt > 0 && self.between()? {
                if let Err(why) = self.expected_prefix(&client) {
                    return Ok(Outcome::stop(why));
                }
            }
            let events = self.conversation.events();
            let (prefix, prefix_text) =
                client::current_prefix(events, self.conversation.prefix_file());
            let messages = client::messages(events, client::timed(prefix_text));
            let mut max_tokens = model
                .as_ref()
                .and_then(|m| m.max_completion_tokens)
                .map_or(client::MAX_TOKENS, |m| m.min(client::MAX_TOKENS))
                .max(1);
            let effort = model
                .as_ref()
                .is_none_or(|m| m.supports("reasoning"))
                .then_some(reasoning_effort.as_str());
            let build = |max_tokens: u64| -> Result<(String, String), String> {
                let head = client::head(&Params {
                    model: &name,
                    max_tokens,
                    effort,
                    client: &client,
                    cache: true,
                });
                let body = client::turn_body(&head, prefix_text, &messages)?;
                Ok((head, body))
            };
            let (mut head, mut body) = build(max_tokens)?;
            let estimate = client::estimate(events, body.len() as u64);
            if let Some(context) = model.as_ref().and_then(|m| m.context_length) {
                if estimate >= context {
                    return Ok(Outcome::stop(format!(
                        "the conversation is about {estimate} tokens, past {name}'s context of {context}; start another conversation (compaction comes later)"
                    )));
                }
                if estimate.saturating_add(max_tokens) > context {
                    max_tokens = context - estimate;
                    (head, body) = build(max_tokens)?;
                }
            }
            let reserved = pricing.map_or(0, |p| p.reserve(estimate, max_tokens));
            let events = self.conversation.events();
            if let Err(why) = cost::within(
                "max_cost_per_turn",
                client.limits.turn,
                accounts::turn_spent(events, turn),
                reserved,
            )
            .and_then(|()| {
                cost::within(
                    "max_cost_per_conversation",
                    client.limits.conversation,
                    accounts::spent(events),
                    reserved,
                )
            }) {
                return Ok(Outcome::stop(why));
            }
            // Room for the reply, and for every call it may make to be
            // answered, if only as not run (`answer`), at load or after.
            let calls = (crate::assemble::MAX_ENTRIES as u64).saturating_mul(CALL_RECORDS);
            let room = (body.len() as u64)
                .saturating_add(client::MAX_REPLY.saturating_mul(2))
                .saturating_add(calls);
            if !self.conversation.has_room_for(room) {
                return Ok(Outcome::stop(
                    "the conversation's log is full; start another conversation",
                ));
            }
            let id = match self.reserve(reserved) {
                Ok(id) => id,
                // Nothing was sent; a turn the closing window cut short
                // may be asked again when the conversation is reopened.
                Err(why) => {
                    return Ok(Outcome {
                        retry: self.gone,
                        ..Outcome::stop(why)
                    })
                }
            };
            if self.interrupt {
                // Interrupted while the window reserved it, or between
                // steps: never sent.
                self.spent(id, 0);
                return Ok(Outcome::again("interrupted before its request was sent"));
            }
            let request = self.log(Kind::Request {
                turn,
                purpose: Purpose::Turn,
                prefix,
                head,
                bytes: body.len() as u64,
                reserved,
            })?;
            // Logged as started and synced before it is sent: a restart
            // that finds it unfinished never sends it again.
            self.sync()?;
            let failure = match self.stream(request.seq, &client, &key, body) {
                Streamed::Replied(completion) => {
                    let cost = charge(completion.usage, pricing, reserved);
                    let outcome = self.reply(request.seq, &completion, cost)?;
                    self.spent(id, cost.0);
                    return Ok(outcome);
                }
                Streamed::Failed { failure, partial } => {
                    if let Some(partial) = partial {
                        self.partial(request.seq, partial)?;
                    }
                    failure
                }
            };
            let outcome = failure.outcome();
            match failure {
                Failure::RateLimited { message, wait } => {
                    self.settle(request.seq, None, (0, Basis::Nothing), outcome)?;
                    self.sync()?;
                    self.spent(id, 0);
                    if attempt >= client::RETRIES {
                        return Ok(Outcome::stop(format!(
                            "error 429: {message}; still rate-limited after {} retries",
                            client::RETRIES
                        )));
                    }
                    let Some(wait) = client::backoff(attempt, wait) else {
                        return Ok(Outcome::stop(format!(
                            "error 429: {message}; the provider asks for a wait longer than {} s",
                            client::MAX_WAIT.as_secs()
                        )));
                    };
                    if self.linger(wait) {
                        let why = if self.gone {
                            "the window closed"
                        } else {
                            "interrupted"
                        };
                        return Ok(Outcome::again(format!(
                            "error 429: {message}; {why} before asking again"
                        )));
                    }
                    attempt += 1;
                }
                Failure::Stop { .. } => {
                    self.settle(request.seq, None, (0, Basis::Nothing), outcome.clone())?;
                    self.spent(id, 0);
                    return Ok(Outcome::stop(outcome));
                }
                Failure::Retryable { usage, .. } | Failure::Interrupted { usage } => {
                    let cost = match usage {
                        Some(client::Usage {
                            cost: Some(cost), ..
                        }) => (cost, Basis::Reported),
                        _ => (reserved, Basis::Reserved),
                    };
                    self.settle(request.seq, usage, cost, outcome.clone())?;
                    self.spent(id, cost.0);
                    return Ok(Outcome::again(outcome));
                }
            }
        }
    }

    /// Logs a completion: the assistant message, its usage and the
    /// request's finish. The outcome is the step's: a reply with tool
    /// calls names itself for them to run, whatever its finish says.
    fn reply(
        &mut self,
        request: u64,
        completion: &Completion,
        cost: (u64, Basis),
    ) -> Result<Outcome, String> {
        let assistant = Kind::Assistant {
            request,
            content: completion.content.clone(),
            reasoning: completion.reasoning.clone(),
            details: completion.details.clone(),
            finish: completion.finish.clone(),
            incomplete: false,
            calls: completion.calls.clone(),
        };
        let logged = match self.log(assistant) {
            Ok(event) => event.seq,
            Err(e) => {
                // A reply past what a log line holds: its cost still
                // counts, and its calls never run.
                let outcome = format!("the reply could not be logged: {e}");
                self.settle(request, completion.usage, cost, outcome.clone())?;
                return Ok(Outcome::stop(outcome));
            }
        };
        let text = match completion.finish.as_str() {
            "stop" => "replied".to_string(),
            "length" => "replied, cut short at max_tokens".to_string(),
            "content_filter" => "stopped by the provider's content filter".to_string(),
            "tool_calls" if completion.calls.is_empty() => {
                "replied, ending for tool calls it did not make".to_string()
            }
            other => format!("replied ({other})"),
        };
        self.settle(request, completion.usage, cost, completion.finish.clone())?;
        Ok(Outcome {
            text,
            retry: false,
            replied: true,
            calls: (!completion.calls.is_empty()).then_some(logged),
        })
    }

    /// Runs the tool calls the reply at `reply` asked for, one at a time
    /// in their order, each logged as started and synced before it runs
    /// and answered by exactly one result (DESIGN.md §5, §6). False when
    /// the human interrupted them: each call not yet run is answered as
    /// not run, so the next request still has a result for every call.
    fn answer(&mut self, reply: u64) -> Result<bool, String> {
        let calls = self
            .conversation
            .events()
            .iter()
            .rev()
            .find(|e| e.seq == reply)
            .and_then(|e| match &e.kind {
                Kind::Assistant { calls, .. } => Some(calls.clone()),
                _ => None,
            })
            .unwrap_or_default();
        let workspace = self.conversation.meta().workspace.is_some();
        // A step that may change files is snapshotted before and after
        // (DESIGN.md §12).
        let writes = workspace && calls.iter().any(|call| tools::Tool::acting(&call.name));
        let before = if writes { self.snapshot(&[]) } else { None };
        for (at, call) in calls.iter().enumerate() {
            self.hear();
            if self.interrupt {
                self.result(reply, call, 0, CALL_SKIPPED.into(), true, Beside::default())?;
                continue;
            }
            // Run only while its result and every later call's answer fit,
            // which the room checked before the request keeps true of
            // the latter.
            let later = (calls.len() - at) as u64;
            if !self
                .conversation
                .has_room_for(RESULT_ROOM.saturating_add(later.saturating_mul(CALL_RECORDS)))
            {
                self.result(reply, call, 0, CALL_NO_ROOM.into(), true, Beside::default())?;
                continue;
            }
            let started = self
                .log(Kind::ToolCall {
                    reply,
                    id: call.id.clone(),
                    name: call.name.clone(),
                })?
                .seq;
            // Durable before it runs: a restart never runs it again.
            self.sync()?;
            let (answer, beside) = match tools::parse_in(workspace, &call.name, &call.arguments) {
                Ok(Args::Host { call, acts }) => self.host(started, call, acts)?,
                Ok(args) => (self.run(started, args)?, Beside::default()),
                Err(why) => (Err(why), Beside::default()),
            };
            let (content, error) = match answer {
                Ok(content) => (content, false),
                Err(why) => (format!("error: {why}"), true),
            };
            self.result(reply, call, started, content, error, beside)?;
        }
        if let Some(before) = before {
            self.snapshotted(reply, before)?;
        }
        self.sync()?;
        Ok(!self.interrupt)
    }

    /// Each ready worktree of a repository workspace snapshotted by the
    /// tool host, against `before` when given (DESIGN.md §12); none for
    /// another workspace, or when it could not be, which is said once.
    fn snapshot(&mut self, before: &[snapshot::Taken]) -> Option<Vec<snapshot::Taken>> {
        let meta = self.conversation.meta();
        let Some(Workspace::Repositories(repositories)) = &meta.workspace else {
            return None;
        };
        if meta.removed || self.snapless {
            return None;
        }
        let checkouts: Vec<String> = if before.is_empty() {
            repositories
                .entries
                .iter()
                .filter(|entry| meta.prepared.contains(&entry.repository))
                .map(|entry| entry.checkout.display().to_string())
                .collect()
        } else {
            before.iter().map(|taken| taken.checkout.clone()).collect()
        };
        if checkouts.is_empty() {
            return None;
        }
        let git = match crate::repo::host_git() {
            Ok(git) => git.display().to_string(),
            Err(why) => return self.unsnapped(why),
        };
        let call = host::Call::Snapshot {
            git,
            checkouts: checkouts.clone(),
            before: before.iter().map(|taken| taken.tree.clone()).collect(),
        };
        match self
            .internal(call, SNAPSHOT_TIME)
            .and_then(|text| snapshot::decode(&text, &checkouts))
        {
            Ok(taken) => Some(taken),
            Err(why) => self.unsnapped(why),
        }
    }

    /// A snapshot not taken, said the first time; none is tried again in
    /// this turn.
    fn unsnapped(&mut self, why: String) -> Option<Vec<snapshot::Taken>> {
        self.snapless = true;
        if !self.unsnapped {
            self.unsnapped = true;
            let text = format!(
                "a step's snapshot was not recorded, so its changes cannot be undone: {why}"
            );
            let _ = self.log(Kind::Notice { text });
        }
        None
    }

    /// The step of the reply at `reply` snapshotted again after its calls,
    /// and each worktree it changed recorded with its trees and files.
    fn snapshotted(&mut self, reply: u64, before: Vec<snapshot::Taken>) -> Result<(), String> {
        let Some(after) = self.snapshot(&before) else {
            return Ok(());
        };
        let worktrees: Vec<store::Snapped> = before
            .into_iter()
            .zip(after)
            .filter(|(before, after)| before.tree != after.tree)
            .map(|(before, after)| store::Snapped {
                checkout: after.checkout,
                before: before.tree,
                after: after.tree,
                changed: after.changed,
                more: after.more,
            })
            .collect();
        if worktrees.is_empty() {
            return Ok(());
        }
        // Room for it, as its answer bounds it; a step it does not fit,
        // or whose record cannot be written, is not recorded.
        let size: usize = worktrees
            .iter()
            .map(|one| {
                one.checkout.len().saturating_mul(6)
                    + one
                        .changed
                        .iter()
                        .map(|name| name.len().saturating_mul(6))
                        .sum::<usize>()
                    + 256
            })
            .sum();
        if !self.conversation.has_room_for(size as u64) {
            self.unsnapped("the conversation's log has no room for its record".into());
            return Ok(());
        }
        if let Err(why) = self.log(Kind::Snapshot { reply, worktrees }) {
            self.unsnapped(why);
        }
        Ok(())
    }

    /// The step snapshotted at `step` undone, or redone, as the window
    /// asked naming it (DESIGN.md §12): the model is told, or why not is
    /// said.
    fn restore(&mut self, step: u64, undo: bool) -> Result<(), String> {
        match self.restoring(step, undo) {
            Ok(text) => {
                self.log(Kind::Restore { step, undo })?;
                self.log(Kind::Notification { text })?;
            }
            Err(why) => {
                let text = format!(
                    "the step could not be {}: {}",
                    if undo { "undone" } else { "redone" },
                    quoted(&why)
                );
                self.log(Kind::Notice { text })?;
            }
        }
        self.sync()?;
        // Done, either way, and recorded: the window kept this process
        // until now.
        self.send(&Up::Restored);
        Ok(())
    }

    /// Restores the step at `step`, only the latest to undo, or the latest
    /// undone, between turns, with every worktree it changed still ready
    /// and, as the tool host checks, as it was left; what to tell the
    /// model, or why not.
    fn restoring(&mut self, step: u64, undo: bool) -> Result<String, String> {
        // What one writes a restore could overwrite, or be overwritten by,
        // whichever step it is; one that has ended is not running.
        self.hear();
        while let Some((number, how)) = self.exited.pop_front() {
            self.ended(number, how)?;
        }
        if !self.processes.is_empty() {
            return Err(format!(
                "background processes are running ({}); kill them first",
                self.process_names()
            ));
        }
        let steps = store::Steps::of(self.conversation.events());
        let latest = if undo { steps.undo() } else { steps.redo() };
        if latest != Some(step) {
            return Err(format!(
                "it is not the latest step to {}",
                if undo { "undo" } else { "redo" }
            ));
        }
        let meta = self.conversation.meta().clone();
        let Some(workspace @ Workspace::Repositories(repositories)) = &meta.workspace else {
            return Err("this conversation has no repositories".into());
        };
        if meta.removed {
            return Err("its workspace is removed".into());
        }
        let Some((reply, worktrees)) =
            self.conversation
                .events()
                .iter()
                .find_map(|event| match &event.kind {
                    Kind::Snapshot { reply, worktrees } if event.seq == step => {
                        Some((*reply, worktrees.clone()))
                    }
                    _ => None,
                })
        else {
            return Err("its snapshot is not in the log".into());
        };
        for one in &worktrees {
            let ready = repositories.entries.iter().any(|entry| {
                entry.checkout.display().to_string() == one.checkout
                    && meta.prepared.contains(&entry.repository)
            });
            if !ready {
                return Err(format!("{} is not ready", one.checkout));
            }
        }
        let checkouts: Vec<String> = worktrees.iter().map(|one| one.checkout.clone()).collect();
        let (from, to): (Vec<String>, Vec<String>) = worktrees
            .iter()
            .map(|one| {
                if undo {
                    (one.after.clone(), one.before.clone())
                } else {
                    (one.before.clone(), one.after.clone())
                }
            })
            .unzip();
        // Room for its record and its notification, at their most, before
        // a file is written.
        let most = (MAX_WHY * 4 + 512) as u64;
        if !self.conversation.has_room_for(most.saturating_mul(6)) {
            return Err("the conversation's log has no room for its record".into());
        }
        let Some((_, client)) = self.setup.clone() else {
            return Err(NO_SETTINGS.into());
        };
        let state = StateDir::at(self.state.clone());
        self.bench.prepare(
            workspace,
            &state,
            &meta.id,
            client.shared_for(workspace),
            &meta.prepared,
        )?;
        let git = crate::repo::host_git()?.display().to_string();
        let call = host::Call::Restore {
            git,
            checkouts: checkouts.clone(),
            from,
            to: to.clone(),
        };
        let taken = self
            .internal(call, SNAPSHOT_TIME)
            .and_then(|text| snapshot::decode(&text, &checkouts))?;
        if taken.iter().zip(&to).any(|(taken, to)| taken.tree != *to) {
            return Err("a worktree did not come back to that step's tree".into());
        }
        let lines: Vec<String> = taken
            .into_iter()
            .map(|taken| {
                let mut names = taken.changed;
                if taken.more > 0 {
                    names.push(format!("and {} more", taken.more));
                }
                format!("{}: {}", taken.checkout, names.join(", "))
            })
            .collect();
        let text = format!(
            "the person {} your step of #{reply}; its files are now as they were {} it: {}",
            if undo { "undid" } else { "redid" },
            if undo { "before" } else { "after" },
            quoted(&lines.join("; "))
        );
        Ok(text)
    }

    /// Runs td-agent's own `call` in the jail, not a model's, for at most
    /// `time`: no card, and short, so an interrupt waits for it; its
    /// answer's text, or why not.
    fn internal(&mut self, call: host::Call, time: Duration) -> Result<String, String> {
        let mut client = self
            .bench
            .take(&call)
            .map_err(|why| format!("the jail: {why}"))?;
        client.call(call.clone())?;
        let deadline = Instant::now() + time;
        loop {
            if Instant::now() >= deadline {
                return Err(CALL_UNANSWERED.into());
            }
            match client.next_reply(HOST_POLL) {
                None | Some(Ok(host::Up::Output { .. })) => {}
                Some(Err(why)) => return Err(why),
                Some(Ok(host::Up::Done { outcome, .. })) => {
                    self.bench.keep(&call, client);
                    return outcome.map(|done| done.text);
                }
            }
        }
    }

    /// Logs a call's result; one past what a log line holds is answered
    /// with why instead, so the call still has its one result.
    fn result(
        &mut self,
        reply: u64,
        call: &Call,
        started: u64,
        content: String,
        error: bool,
        beside: Beside,
    ) -> Result<(), String> {
        let result =
            |content: String, error: bool, kept: Option<String>, digest: Option<String>| {
                Kind::ToolResult {
                    reply,
                    id: call.id.clone(),
                    name: call.name.clone(),
                    call: started,
                    content,
                    error,
                    kept,
                    digest,
                }
            };
        let digest = beside.digest.clone();
        if self
            .log(result(content.clone(), error, beside.kept, beside.digest))
            .is_ok()
        {
            return Ok(());
        }
        // The kept output is what may not fit: the result without it.
        if let Err(e) = self.log(result(content, error, None, digest)) {
            self.log(result(
                format!("error: the result could not be logged: {e}"),
                true,
                None,
                None,
            ))?;
        }
        Ok(())
    }

    /// A repository workspace's stores, asked of the window for each
    /// repository not yet prepared (DESIGN.md §7); each answer prepares
    /// its repository (`stored`). A process started again asks again.
    fn ask_stores(&mut self) {
        let meta = self.conversation.meta();
        let Some(Workspace::Repositories(repositories)) = &meta.workspace else {
            return;
        };
        // Gone with its archive: nothing is prepared again.
        if meta.removed {
            return;
        }
        // A prepared one's bases are asked where the window last found
        // them, so its remote-tracking refs follow (`heads`).
        let mut asks: Vec<(String, Vec<String>, bool)> = Vec::new();
        for entry in &repositories.entries {
            let prepared = meta.prepared.contains(&entry.repository);
            match asks
                .iter_mut()
                .find(|(remote, _, _)| *remote == entry.remote)
            {
                Some((_, bases, _)) => bases.push(entry.base.clone()),
                None => asks.push((entry.remote.clone(), vec![entry.base.clone()], prepared)),
            }
        }
        for (remote, bases, prepared) in asks {
            if prepared {
                self.send(&Up::Heads { remote, bases });
            } else {
                self.awaiting.push(remote.clone());
                self.send(&Up::Fetch { remote, bases });
            }
        }
    }

    /// The window's word of where `remote`'s `bases` were when it last
    /// fetched the store, `ids` in their order (DESIGN.md §7, Keeping
    /// current): each base this workspace names whose remote-tracking ref
    /// was last set elsewhere is set there in a maintenance instance and
    /// recorded, and one that moved from a commit recorded is said in the
    /// log. A repository not yet prepared waits for its preparation,
    /// which sets them.
    fn heads(&mut self, remote: &str, bases: &[String], ids: &[String]) -> Result<(), String> {
        let meta = self.conversation.meta().clone();
        let Some(Workspace::Repositories(repositories)) = &meta.workspace else {
            return Ok(());
        };
        if meta.removed {
            return Ok(());
        }
        let entries: Vec<&Entry> = repositories
            .entries
            .iter()
            .filter(|entry| entry.remote == remote)
            .collect();
        let Some(first) = entries.first() else {
            return Ok(());
        };
        if !meta.prepared.contains(&first.repository) {
            return Ok(());
        }
        let mut moved: Vec<(String, String, Option<String>)> = Vec::new();
        for (base, id) in bases.iter().zip(ids) {
            let named = entries.iter().any(|entry| entry.base == *base);
            if !named || !crate::git::object_id(id) || moved.iter().any(|(b, _, _)| b == base) {
                continue;
            }
            let was = meta
                .tracked
                .iter()
                .find(|tracked| tracked.remote == remote && tracked.base == *base)
                .map(|tracked| tracked.id.clone());
            if was.as_deref() != Some(id.as_str()) {
                moved.push((base.clone(), id.clone(), was));
            }
        }
        if moved.is_empty() {
            return Ok(());
        }
        let heads: Vec<(String, String)> = moved
            .iter()
            .map(|(base, id, _)| (base.clone(), id.clone()))
            .collect();
        if self
            .untracked
            .iter()
            .any(|failed| failed.remote == remote && failed.tried == heads)
        {
            return Ok(());
        }
        let said = match track(&self.state, &meta.id, &entries, &heads, false) {
            Ok(()) => {
                self.conversation.set_tracked(remote, &heads)?;
                self.untracked.retain(|failed| failed.remote != remote);
                let told: Vec<String> = moved
                    .iter()
                    .filter_map(|(base, id, was)| {
                        let was = was.as_ref()?;
                        Some(format!("{base} moved from {} to {}", short(was), short(id)))
                    })
                    .collect();
                // The model's news; it reads it at its next turn.
                (!told.is_empty()).then(|| Kind::Notification {
                    text: format!(
                        "upstream's {} in {remote}: each worktree's refs/remotes/origin/<base> names it now",
                        told.join(", ")
                    ),
                })
            }
            Err(why) => {
                let text = format!("the remote-tracking refs of {remote} could not be set: {why}");
                let new = !self
                    .untracked
                    .iter()
                    .any(|failed| failed.remote == remote && failed.said == text);
                self.untracked.retain(|failed| failed.remote != remote);
                self.untracked.push(Untracked {
                    remote: remote.to_string(),
                    tried: heads.clone(),
                    said: text.clone(),
                });
                // The human's to mend, not the model's news.
                new.then_some(Kind::Notice { text })
            }
        };
        if let Some(kind) = said {
            self.log(kind)?;
            self.sync()?;
        }
        Ok(())
    }

    /// The window's answer for `remote`'s store (`prepare`), then the
    /// window told this process is done with it, so it may retire it:
    /// now, or when the checkout it began is done (`finished`).
    fn stored(&mut self, remote: String, result: Result<Stored, String>) -> Result<(), String> {
        self.awaiting.retain(|asked| *asked != remote);
        let checking = self.prepare(&remote, result);
        if !matches!(checking, Ok(true)) {
            self.send(&Up::Prepared { remote });
        }
        checking.map(drop)
    }

    /// `remote`'s project instructions recorded, then its repository laid
    /// out and each of its worktrees checked out on a thread of its own
    /// (`Checkout`), so a turn goes on meanwhile; whether the window is
    /// to be told it is done later. A failure before the checkout starts
    /// is kept as the thread's would be (`checked`), and said, as every
    /// checkout's end is, between a turn's steps or as an idle
    /// conversation's news (`between`, `woken`). One that fails is asked
    /// for again only when a process for this conversation next starts.
    fn prepare(&mut self, remote: &str, result: Result<Stored, String>) -> Result<bool, String> {
        let meta = self.conversation.meta().clone();
        let Some(Workspace::Repositories(repositories)) = &meta.workspace else {
            return Ok(false);
        };
        // Gone with its archive: an answer asked for before is let go.
        if meta.removed {
            return Ok(false);
        }
        let entries: Vec<&Entry> = repositories
            .entries
            .iter()
            .filter(|entry| entry.remote == remote)
            .collect();
        let Some(first) = entries.first() else {
            return Ok(false);
        };
        if meta.prepared.contains(&first.repository) {
            return Ok(false);
        }
        // One already checking out says it is done when it is.
        if self.checking.iter().any(|checking| checking == remote) {
            return Ok(true);
        }
        // Held until its end is taken up, however it ends (`done_with`).
        self.checking.push(remote.to_string());
        // The instructions first, here: the turn waiting for them goes on
        // whether or not the checkout does.
        let fetched = match result.and_then(|fetched| {
            self.instructed(&entries, &fetched)?;
            Ok(fetched)
        }) {
            Ok(fetched) => fetched,
            // Said as a checkout's failure is, so it wakes alike.
            Err(why) => {
                self.checked.push_back((remote.to_string(), Err(why)));
                return Ok(true);
            }
        };
        let checkout = Checkout {
            state: self.state.clone(),
            id: meta.id.clone(),
            entries: entries.iter().map(|entry| (*entry).clone()).collect(),
            fetched,
        };
        let (send, named) = (self.sender.clone(), remote.to_string());
        let spawned = std::thread::Builder::new()
            .name("td-agent-checkout".into())
            .spawn(move || {
                // Handed back however the thread ends, so the window is
                // always told the process is done with the store.
                let result =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| checkout.run()))
                        .unwrap_or_else(|_| Err("the checkout's thread ended".into()));
                let _ = send.send(Inbound::Checked {
                    remote: named,
                    result,
                });
            });
        match spawned {
            Ok(_) => Ok(true),
            Err(e) => {
                self.checked.push_back((
                    remote.to_string(),
                    Err(format!("its checkout's thread: {e}")),
                ));
                Ok(true)
            }
        }
    }

    /// Each checkout done meanwhile, said between a turn's steps, where
    /// its news falls after every result of the step before and the next
    /// request reads it (DESIGN.md §7); whether there was one.
    fn between(&mut self) -> Result<bool, String> {
        self.hear();
        while let Some((number, how)) = self.exited.pop_front() {
            self.ended(number, how)?;
        }
        let any = !self.checked.is_empty();
        while let Some((remote, result)) = self.checked.pop_front() {
            let said = self.checked_out(&remote, result);
            self.done_with(&remote);
            said?;
        }
        Ok(any)
    }

    /// A checkout done while no turn runs: said, and its news wakes the
    /// conversation (DESIGN.md §3) when the person has written to it, it
    /// is not paused, there is a key to ask with, the window is still
    /// there, nothing it sent is about to start a turn that would read
    /// the news anyway, and the news is not what the last of `remote`'s
    /// said. Such a turn is not counted against the wake budget.
    fn woken(&mut self, remote: String, result: Set) -> Result<(), String> {
        let said = match self.checked_out(&remote, result) {
            Ok(Some(said)) => said,
            other => {
                self.done_with(&remote);
                return other.map(drop);
            }
        };
        let wakes = !self.conversation.meta().paused
            && self.setup.as_ref().is_some_and(|(key, _)| key.is_ok())
            && !self.gone
            && self.ended.is_none()
            && self
                .conversation
                .events()
                .iter()
                .any(|event| matches!(event.kind, Kind::User { .. }))
            && !self
                .queue
                .iter()
                .any(|down| matches!(down, Down::User { .. } | Down::Message { .. } | Down::Retry))
            && !self.repeated(&remote, &said);
        if !wakes {
            self.done_with(&remote);
            return Ok(());
        }
        // The turn first, so the window, told it is done with the store,
        // still keeps the process for the turn.
        let started = self.log(Kind::Started {
            effect: Effect::Turn,
            of: said.seq,
        });
        self.done_with(&remote);
        let started = started?;
        self.sync()?;
        self.turn(started.seq)
    }

    /// Whether `said` says what the last news of `remote` before it said:
    /// a failure each process start meets again, which wakes nothing.
    fn repeated(&self, remote: &str, said: &Event) -> bool {
        let Kind::Notification { text } = &said.kind else {
            return false;
        };
        let about = format!("{remote} ");
        self.conversation
            .events()
            .iter()
            .rev()
            .filter(|event| event.seq < said.seq)
            .find_map(|event| match &event.kind {
                Kind::Notification { text } if text.starts_with(&about) => Some(text),
                _ => None,
            })
            .is_some_and(|last| last == text)
    }

    /// The window told this process is done with `remote`'s store, which
    /// it kept the process for.
    fn done_with(&mut self, remote: &str) {
        self.checking.retain(|checking| checking != remote);
        self.send(&Up::Prepared {
            remote: remote.to_string(),
        });
    }

    /// `remote`'s repository recorded prepared, which its instances bind
    /// from the next call, with the commits its remote-tracking refs were
    /// set to, or not; said in a notification either way, which is
    /// returned. A prepared one's bases are asked where the window found
    /// them last, so a move while it checked out is not missed (`heads`).
    fn checked_out(&mut self, remote: &str, result: Set) -> Result<Option<Event>, String> {
        let meta = self.conversation.meta().clone();
        let Some(Workspace::Repositories(repositories)) = &meta.workspace else {
            return Ok(None);
        };
        if meta.removed {
            return Ok(None);
        }
        let entries: Vec<&Entry> = repositories
            .entries
            .iter()
            .filter(|entry| entry.remote == remote)
            .collect();
        let Some(first) = entries.first() else {
            return Ok(None);
        };
        let (text, ready) = match result {
            Ok(heads) => {
                self.conversation.set_prepared(&first.repository)?;
                self.conversation.set_tracked(remote, &heads)?;
                let ready: Vec<String> = entries
                    .iter()
                    .map(|entry| entry.checkout.display().to_string())
                    .collect();
                let bases = heads.into_iter().map(|(base, _)| base).collect();
                (
                    format!("{remote} is checked out and ready: {}", ready.join(", ")),
                    Some(bases),
                )
            }
            Err(why) => (
                format!("{remote} could not be prepared: {}", quoted(&why)),
                None,
            ),
        };
        let said = self.log(Kind::Notification { text })?;
        self.sync()?;
        if let Some(bases) = ready {
            self.send(&Up::Heads {
                remote: remote.to_string(),
                bases,
            });
        }
        Ok(Some(said))
    }

    /// Records the project instructions `fetched` read at each of
    /// `entries`' bases, one remote's (DESIGN.md §13).
    fn instructed(&mut self, entries: &[&Entry], fetched: &Stored) -> Result<(), String> {
        if fetched.ids.len() != entries.len() || fetched.instructions.len() != entries.len() {
            return Err("the window answered for another number of bases".into());
        }
        if !fetched.ids.iter().all(|id| crate::git::object_id(id)) {
            return Err("the window answered with a base that is no commit".into());
        }
        let recorded = entries
            .iter()
            .zip(&fetched.ids)
            .zip(&fetched.instructions)
            .map(|((entry, base), read)| store::Instructed {
                checkout: entry.checkout.clone(),
                base: base.clone(),
                read: read.clone(),
            })
            .collect();
        self.conversation.instructed(recorded)
    }

    /// Waits, before a turn's first request, for the window's answer to
    /// each store this process asked for, preparing each as it comes, so
    /// the prefix holds the project instructions from the first request
    /// on (DESIGN.md §7, §13). What else comes meanwhile waits its turn;
    /// an interrupt ends the turn.
    fn await_stores(&mut self) -> Result<Option<Outcome>, String> {
        // With no key the turn ends at its first step, saying so, and
        // waits for nothing.
        if !self.setup.as_ref().is_some_and(|(key, _)| key.is_ok()) {
            return Ok(None);
        }
        let interrupted = || {
            Ok(Some(Outcome::again(
                "interrupted while waiting for the workspace's project instructions",
            )))
        };
        while !self.awaiting.is_empty() {
            // Said with the turn's message, before it began.
            if let Some(at) = self
                .queue
                .iter()
                .position(|down| matches!(down, Down::Interrupt))
            {
                self.queue.remove(at);
                return interrupted();
            }
            // One that came before the turn began, behind its message.
            if let Some(at) = self
                .queue
                .iter()
                .position(|down| matches!(down, Down::Fetched { .. }))
            {
                if let Some(Down::Fetched { remote, result }) = self.queue.remove(at) {
                    self.stored(remote, result)?;
                }
                continue;
            }
            if self.gone {
                return Err("the window has closed".into());
            }
            match self.inbox.recv() {
                Ok(Inbound::Down(Down::Fetched { remote, result })) => {
                    self.stored(remote, result)?
                }
                Ok(Inbound::Down(Down::Interrupt)) => return interrupted(),
                Ok(Inbound::Down(Down::Reservation { id, refusal: None })) => self.spent(id, 0),
                Ok(Inbound::Down(down)) => self.queue.push_back(down),
                Ok(Inbound::Fetch { .. }) => {}
                Ok(Inbound::Checked { remote, result }) => self.checked.push_back((remote, result)),
                Ok(Inbound::Ended { number, how }) => self.exit(number, how),
                Ok(Inbound::Closed | Inbound::Broken(_)) | Err(_) => {
                    self.gone = true;
                    return Err("the window has closed".into());
                }
            }
        }
        Ok(None)
    }

    /// The prefix this conversation's requests begin with: for one in a
    /// workspace, the workspace prepared for `client`'s shared directories
    /// and named, with its tools.
    fn expected_prefix(&mut self, client: &Client) -> Result<String, String> {
        let meta = self.conversation.meta().clone();
        let Some(workspace) = &meta.workspace else {
            return Ok(crate::prompt::prefix(meta.created));
        };
        let state = StateDir::at(self.state.clone());
        let policy = self
            .bench
            .prepare(
                workspace,
                &state,
                &meta.id,
                client.shared_for(workspace),
                &meta.prepared,
            )
            .map_err(|e| format!("the workspace could not be prepared: {e}"))?;
        // Named whether or not they are ready, so the prefix holds.
        let repositories = match workspace {
            Workspace::Repositories(repositories) => Some(repositories),
            _ => None,
        };
        let directory = match repositories {
            Some(repositories) => repositories.entries.first().map(|entry| &entry.checkout),
            None => policy.worktrees.first(),
        }
        .ok_or("the workspace has no directory")?;
        let instructions = self.conversation.instructions().to_vec();
        let place = crate::prompt::Place {
            scratch: workspace.scratch(),
            directory,
            read: &policy.read,
            write: &policy.write,
            repositories,
            instructions: &instructions,
            removed: meta.removed,
        };
        Ok(crate::prompt::prefix_in(meta.created, Some(&place)))
    }

    /// Runs a workspace tool's call, the `ToolCall` record `started`, in
    /// the jail (DESIGN.md §8, §12), the human deciding first when it
    /// `acts` (§11): the tool's answer or why not, and what the log keeps
    /// beyond it. An interrupt cancels it; a window gone does not, so the
    /// call ends whole in the log.
    fn host(
        &mut self,
        started: u64,
        call: host::Call,
        acts: bool,
    ) -> Result<(Result<String, String>, Beside), String> {
        let failed = |why: String| Ok((Err(why), Beside::default()));
        if self.conversation.meta().removed {
            return failed(WORKSPACE_GONE.into());
        }
        // Before any card: the person is not asked about a call that
        // cannot run yet.
        if let Some(Workspace::Repositories(repositories)) = &self.conversation.meta().workspace {
            let pending: Vec<&str> = self
                .checking
                .iter()
                .chain(&self.awaiting)
                .map(String::as_str)
                .collect();
            if let Some(why) = unready(
                repositories,
                &self.conversation.meta().prepared,
                &pending,
                &call,
            ) {
                return failed(why);
            }
        }
        if let host::Call::Background { .. } = &call {
            // One that has ended runs no more.
            self.hear();
            let most = self
                .setup
                .as_ref()
                .map_or(crate::config::DEFAULT_MAX_BACKGROUND, |(_, client)| {
                    client.max_background
                });
            if self.processes.len() >= most as usize {
                return failed(format!(
                    "{most} background processes are running ({}), the most at once; wait for one to end or kill one",
                    self.process_names()
                ));
            }
        }
        if acts {
            let (title, details) = tools::card(&call);
            match self.decide(started, title, details)? {
                Some(true) => {}
                Some(false) => return failed(CALL_REFUSED.into()),
                None => return failed(CALL_UNDECIDED.into()),
            }
        }
        // An interrupt that came with the decision, or before it: nothing
        // starts.
        self.hear();
        if self.interrupt {
            return failed(CALL_SKIPPED.into());
        }
        let call = self.bench.with_digest(call);
        let mut client = match self.bench.take(&call) {
            Ok(client) => client,
            Err(why) => return failed(format!("the jail: {why}")),
        };
        let id = match client.call(call.clone()) {
            Ok(id) => id,
            Err(why) => return failed(why),
        };
        if let host::Call::Background { command, .. } = &call {
            let limit = crate::bench::limit(&call);
            return Ok((
                self.background(started, command, client, id, limit),
                Beside::default(),
            ));
        }
        // What runs in the jail may stop or replace the tool host, so the
        // call's time is kept here too, past which its jail is torn down.
        let mut deadline = Instant::now() + crate::bench::limit(&call);
        let mut cancelled = false;
        loop {
            self.hear();
            if self.interrupt && !cancelled {
                cancelled = true;
                deadline = deadline.min(Instant::now() + CANCEL_GRACE);
                if let Err(why) = client.cancel(id) {
                    return failed(why);
                }
            }
            if Instant::now() >= deadline {
                drop(client);
                return failed(CALL_UNANSWERED.into());
            }
            match client.next_reply(HOST_POLL) {
                None | Some(Ok(host::Up::Output { .. })) => {}
                // The instance is gone, and with it the call.
                Some(Err(why)) => return failed(why),
                Some(Ok(host::Up::Done { outcome, .. })) => {
                    let answer = match outcome {
                        Ok(done) => {
                            self.bench.record(&call, done.digest.as_deref());
                            let beside = Beside {
                                kept: done.kept,
                                digest: done.digest,
                            };
                            (Ok(done.text), beside)
                        }
                        Err(why) => (Err(why), Beside::default()),
                    };
                    self.bench.keep(&call, client);
                    return Ok(answer);
                }
            }
        }
    }

    /// Background process `call` started on `client`, whose `started`
    /// `ToolCall` it is, given its own number and a thread that watches
    /// it until it ends, is killed or passes `limit` (DESIGN.md §12).
    fn background(
        &mut self,
        started: u64,
        command: &str,
        client: host::Client,
        call: u64,
        limit: Duration,
    ) -> Result<String, String> {
        let number = store::backgrounds(self.conversation.events())
            .iter()
            .map(|one| one.number)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        // Logged before it is watched, so its end is never logged first;
        // a process whose start cannot be logged is ended with `client`.
        self.log(Kind::Process {
            number,
            call: started,
            command: command.to_string(),
        })?;
        let (kill, killed) = mpsc::channel();
        let inbox = self.sender.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("p{number}"))
            .spawn(move || {
                let how = watch(client, call, limit, &killed);
                let _ = inbox.send(Inbound::Ended { number, how });
            });
        if let Err(e) = spawned {
            self.ended(number, format!("failed: a thread to watch it: {e}"))?;
            return Err(format!("a thread to watch p{number}: {e}"));
        }
        self.processes.insert(number, kill);
        Ok(format!(
            "started p{number} in the background; process_list shows it, process_kill ends it"
        ))
    }

    /// Background process `number` ended `how`: logged, once.
    fn ended(&mut self, number: u64, how: String) -> Result<(), String> {
        self.processes.remove(&number);
        let running = store::backgrounds(self.conversation.events())
            .iter()
            .any(|one| one.number == number && one.ended.is_none());
        if running {
            // What the jail said, its tool host maybe replaced: one
            // bounded line.
            self.log(Kind::Ended {
                number,
                how: quoted(&how),
            })?;
        }
        Ok(())
    }

    /// Background process `number` ended `how`, heard: it runs no more,
    /// and its end is logged at the next step or while idle (`ended`).
    fn exit(&mut self, number: u64, how: String) {
        self.processes.remove(&number);
        self.exited.push_back((number, how));
    }

    /// How background process `number` ended, as heard and not yet
    /// logged.
    fn heard_end(&self, number: u64) -> Option<String> {
        self.exited
            .iter()
            .find(|(n, _)| *n == number)
            .map(|(_, how)| quoted(how))
    }

    /// The running background processes' ids, for a refusal.
    fn process_names(&self) -> String {
        self.processes
            .keys()
            .map(|number| format!("p{number}"))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// `process_list`: the log's background processes, the running ones
    /// as this process has them.
    fn process_list(&mut self) -> String {
        self.hear();
        let all = store::backgrounds(self.conversation.events());
        if all.is_empty() {
            return "no background processes".into();
        }
        // The latest, each command cut, so the list fits a log line.
        let earlier = all.len().saturating_sub(MAX_PROCESSES_LISTED);
        let mut out = String::new();
        if earlier > 0 {
            out.push_str(&format!("{earlier} earlier not shown"));
        }
        for one in all.iter().skip(earlier) {
            let state = match &one.ended {
                Some(how) => how.clone(),
                None if self.processes.contains_key(&one.number) => "running".into(),
                // Its end heard and not yet logged; "ending" only when
                // logging an end failed.
                None => self
                    .heard_end(one.number)
                    .unwrap_or_else(|| "ending".into()),
            };
            if !out.is_empty() {
                out.push('\n');
            }
            let command: String = one.command.chars().take(MAX_LISTED_COMMAND).collect();
            out.push_str(&format!(
                "p{} | {state} | started {} | {}{}",
                one.number,
                history::utc(one.started),
                tools::visible(&command),
                if command.len() < one.command.len() {
                    "..."
                } else {
                    ""
                }
            ));
        }
        out
    }

    /// `process_kill`: background process `number` told to end; its end
    /// is logged when its watcher says it ended.
    fn kill(&mut self, number: u64) -> Result<String, String> {
        self.hear();
        if let Some(how) = self.heard_end(number) {
            return Err(format!("p{number} is not running: {how}"));
        }
        match self.processes.get(&number) {
            Some(kill) => {
                let _ = kill.send(());
                Ok(format!("p{number} is being killed"))
            }
            None => match store::backgrounds(self.conversation.events())
                .into_iter()
                .find(|one| one.number == number)
            {
                Some(one) => Err(format!(
                    "p{number} is not running: {}",
                    one.ended.as_deref().unwrap_or("it has ended")
                )),
                None => Err(format!("there is no p{number}")),
            },
        }
    }

    /// Asks the human whether the call `started` may run, on a card the
    /// window draws (DESIGN.md §11) with `title` and `details`, and waits
    /// for the answer however long it takes, hearing the window
    /// meanwhile: whether it may, or none when the turn was interrupted
    /// or the window closed first, the card then withdrawn. The decision
    /// is logged.
    fn decide(
        &mut self,
        started: u64,
        title: String,
        details: Vec<String>,
    ) -> Result<Option<bool>, String> {
        self.send(&Up::Ask {
            call: started,
            title,
            details,
        });
        let allow = loop {
            if self.gone || self.interrupt {
                break None;
            }
            match self.inbox.recv() {
                Ok(Inbound::Down(Down::Decision { call, allow })) if call == started => {
                    break Some(allow)
                }
                // A card already answered or withdrawn.
                Ok(Inbound::Down(Down::Decision { .. })) => {}
                Ok(Inbound::Down(Down::Interrupt)) => self.interrupt = true,
                Ok(Inbound::Down(Down::Reservation { id, refusal: None })) => self.spent(id, 0),
                Ok(Inbound::Down(down)) => self.queue.push_back(down),
                Ok(Inbound::Fetch { .. }) => {}
                Ok(Inbound::Checked { remote, result }) => self.checked.push_back((remote, result)),
                Ok(Inbound::Ended { number, how }) => self.exit(number, how),
                Ok(Inbound::Broken(why)) => {
                    eprintln!("td-agent: the window: {why}");
                    self.gone = true;
                }
                Ok(Inbound::Closed) | Err(_) => self.gone = true,
            }
        };
        let (outcome, by, reason) = match allow {
            Some(true) => ("allow", "human", None),
            Some(false) => ("deny", "human", None),
            None => {
                self.send(&Up::Withdraw { call: started });
                let why = if self.gone {
                    "the window closed"
                } else {
                    "the turn was interrupted"
                };
                ("withdrawn", "td-agent", Some(why.to_string()))
            }
        };
        self.log(Kind::Approval {
            call: started,
            outcome: outcome.into(),
            by: by.into(),
            probabilities: None,
            reason,
        })?;
        Ok(allow)
    }

    /// Runs one call, the `ToolCall` record `started`: the tool's answer,
    /// or why it failed, which the model is told; an error of the log's
    /// own ends the process.
    fn run(&mut self, started: u64, args: Args) -> Result<Result<String, String>, String> {
        Ok(match args {
            Args::Todo(items) => {
                let text = tools::todo_text(&items);
                self.log(Kind::Todo {
                    items,
                    cleared: false,
                })?;
                Ok(text)
            }
            Args::Search(search) => {
                let reach = Reach::Search(&search.query);
                match self.cross(started, search.conversation.as_ref(), Op::Read, reach)? {
                    Ok(other) => self.with_log(other.as_ref(), |events| {
                        Ok(history::search(
                            events,
                            &search.query,
                            &search.kinds,
                            search.limit,
                        ))
                    }),
                    Err(why) => Err(why),
                }
            }
            Args::Read(read) => {
                let reach = Reach::Read {
                    from: read.from,
                    offset: read.offset,
                    count: read.count,
                    max_bytes: read.max_bytes,
                };
                match self.cross(started, read.conversation.as_ref(), Op::Read, reach)? {
                    Ok(other) => self.with_log(other.as_ref(), |events| {
                        history::read(events, read.from, read.offset, read.count, read.max_bytes)
                    }),
                    Err(why) => Err(why),
                }
            }
            Args::Conversations => self.conversations(),
            Args::Processes => Ok(self.process_list()),
            Args::Kill(number) => self.kill(number),
            Args::Send { to, text } => {
                match self.cross(started, Some(&to), Op::Message, Reach::Message(&text))? {
                    Ok(_) => self.post(to, text),
                    Err(why) => Err(why),
                }
            }
            // `answer` runs these through `host`, with their record.
            Args::Host { .. } => Err("a workspace tool runs only in the jail".into()),
        })
    }

    /// The conversations of the store, by their `meta`.
    fn metas(&self) -> Vec<store::Meta> {
        StateDir::at(self.state.clone()).list().0
    }

    /// Whether the call `started` may reach conversation `target` for
    /// `op`, as `reach` says (DESIGN.md §3, §11): another conversation,
    /// when one the store holds and the human allows it on a card; none
    /// when it is this one or none is named; else why not, which the call
    /// is answered with. An error of the log's own ends the process.
    fn cross(
        &mut self,
        started: u64,
        target: Option<&Id>,
        op: Op,
        reach: Reach,
    ) -> Result<Result<Option<Id>, String>, String> {
        let Some(target) = target else {
            return Ok(Ok(None));
        };
        let me = self.conversation.meta().id.clone();
        match tools::crossing(&me, target, op) {
            Ok(true) => {}
            Ok(false) => return Ok(Ok(None)),
            Err(why) => return Ok(Err(why)),
        }
        let Some(meta) = self.metas().into_iter().find(|m| &m.id == target) else {
            return Ok(Err(format!("there is no conversation {target}")));
        };
        let (title, details) = tools::crossing_card(target, &meta.title, reach);
        match self.decide(started, title, details)? {
            Some(true) => {}
            Some(false) => return Ok(Err(CALL_REFUSED.into())),
            None => return Ok(Err(CALL_UNDECIDED.into())),
        }
        // An interrupt that came with the decision, or before it.
        self.hear();
        if self.interrupt {
            return Ok(Err(CALL_SKIPPED.into()));
        }
        Ok(Ok(Some(meta.id)))
    }

    /// `read` over the log of conversation `other`, this conversation's
    /// own when none.
    fn with_log(
        &self,
        other: Option<&Id>,
        read: impl FnOnce(&[Event]) -> Result<String, String>,
    ) -> Result<String, String> {
        match other {
            None => read(self.conversation.events()),
            Some(id) => read(&store::read_log(&StateDir::at(self.state.clone()), id)?),
        }
    }

    /// `send_message`, the human having allowed it: the message queued by
    /// the window for its receiver, or why not.
    fn post(&mut self, to: Id, text: String) -> Result<String, String> {
        let id = self.ask_id();
        self.send(&Up::Send {
            id,
            to: to.clone(),
            text,
        });
        // Unanswered, the window may still have queued it before closing
        // or while this gave up: the model is not invited to send twice.
        let answer = self
            .wait(|down| matches!(down, Down::Sent { id: a, .. } if *a == id))
            .map_err(|why| {
                format!("{why}, so whether the message was queued is unknown; it may yet be delivered, so do not send it again unless an answer that needs it does not come")
            })?;
        match answer {
            Down::Sent { refusal: None, .. } => Ok(format!("queued for conversation {to}; it is delivered between that conversation's turns, and any reply comes back here as a message, later")),
            Down::Sent {
                refusal: Some(why),
                ..
            } => Err(why),
            _ => Err("the window answered something else".into()),
        }
    }

    /// `conversations`: what td-agent writes of each conversation, and
    /// what models wrote of the ones the caller may see (DESIGN.md §3).
    fn conversations(&mut self) -> Result<String, String> {
        let id = self.ask_id();
        self.send(&Up::Query { id });
        let states =
            match self.wait(|down| matches!(down, Down::States { id: a, .. } if *a == id))? {
                Down::States { states, .. } => states,
                _ => return Err("the window answered something else".into()),
            };
        let state_dir = StateDir::at(self.state.clone());
        let me = self.conversation.meta().clone();
        // Ordered and bounded first, so only the logs listed are read: the
        // most recently active first.
        // By the millisecond, then by id, as the window's list orders them.
        let mut metas: Vec<(store::Meta, u64)> = self
            .metas()
            .into_iter()
            .map(|meta| {
                let activity = store::activity(&state_dir, &meta);
                (meta, activity)
            })
            .collect();
        metas.sort_by(|(a, at), (b, bt)| bt.cmp(at).then(a.id.cmp(&b.id)));
        let omitted = metas.len().saturating_sub(MAX_LISTED);
        metas.truncate(MAX_LISTED);
        let mut entries: Vec<Listed> = Vec::with_capacity(metas.len());
        for (meta, activity) in metas {
            let own;
            let events: &[Event] = if meta.id == me.id {
                self.conversation.events()
            } else {
                // A log that cannot be read lists with nothing spent and
                // nothing in progress; its own process says why.
                own = store::read_log(&state_dir, &meta.id).unwrap_or_default();
                &own
            };
            let state = states
                .iter()
                .find(|(id, _)| *id == meta.id)
                .map_or("idle", |(_, state)| state.as_str());
            let state = match state {
                "idle" if meta.paused => "paused",
                other => other,
            };
            entries.push(Listed {
                state: state.to_string(),
                cost: accounts::spent(events),
                activity: activity / 1000,
                title: meta.title.clone(),
                doing: doing(events),
                workspace: meta.workspace.as_ref().map(Workspace::label),
                id: meta.id,
            });
        }
        Ok(tools::listing(&me.id, &entries, omitted))
    }

    /// A message from another conversation, which the window routed:
    /// logged once by its delivery id, and starting a turn unless the
    /// conversation is paused or its wake budget is spent (DESIGN.md §3).
    fn message(
        &mut self,
        delivery: String,
        from: Id,
        role: Role,
        text: String,
        status: Option<String>,
    ) -> Result<(), String> {
        if self.conversation.delivered(&delivery) {
            self.send(&Up::Delivered { delivery });
            return Ok(());
        }
        if text.len() > tools::MAX_MESSAGE || !self.conversation.has_room(text.len()) {
            let reason = if text.len() > tools::MAX_MESSAGE {
                format!("a message is at most {} bytes", tools::MAX_MESSAGE)
            } else {
                "the conversation's log is full".to_string()
            };
            self.send(&Up::Refused { delivery, reason });
            return Ok(());
        }
        let events = self.conversation.events();
        // A report, which an outbox from before peers may still hold, was
        // a notification its sender's own budget bounded.
        let counts = status.is_none();
        let held = if self.conversation.meta().paused {
            Some(Held::Paused)
        } else if counts && wake::spent(events) >= wake::BUDGET {
            Some(Held::Budget)
        } else {
            None
        };
        let tell = held == Some(Held::Budget) && !wake::told(events);
        let logged = self
            .conversation
            .append(Kind::Message {
                delivery: delivery.clone(),
                from,
                role,
                text,
                status,
                held,
            })?
            .clone();
        let started = match held {
            None => Some(
                self.conversation
                    .append(Kind::Started {
                        effect: Effect::Turn,
                        of: logged.seq,
                    })?
                    .clone(),
            ),
            Some(_) => None,
        };
        self.sync()?;
        self.send(&Up::Event(logged));
        if let Some(started) = &started {
            self.send(&Up::Event(started.clone()));
        }
        if tell {
            self.log(Kind::Notice {
                text: wake::notice(),
            })?;
            self.sync()?;
        }
        self.send(&Up::Delivered { delivery });
        match started {
            Some(started) => self.turn(started.seq),
            None => Ok(()),
        }
    }

    /// The human chose the conversation's model and effort, which apply
    /// from its next request: logged, then kept in `meta`.
    fn choose(&mut self, model: Option<String>, effort: Option<String>) -> Result<(), String> {
        self.log(Kind::Choice {
            model: model.clone(),
            effort: effort.clone(),
        })?;
        self.conversation.set_choice(model, effort)?;
        self.sync()
    }

    /// The human paused or resumed the conversation, which is logged even
    /// when it already was: the window keeps a resuming process until it
    /// hears the resumption. Resumed, the newest message held since the
    /// last turn began starts a turn, which sees every one, unless one of
    /// them would be a wake past the budget. That turn's start is logged
    /// before the resumption, so the window never lets the process go
    /// between the two.
    fn pause(&mut self, paused: bool) -> Result<(), String> {
        if paused {
            self.log(Kind::Pause { paused })?;
            self.conversation.set_paused(true)?;
            return self.sync();
        }
        let events = self.conversation.events();
        let since = events
            .iter()
            .rposition(|e| matches!(e.kind, Kind::Started { .. }))
            .map_or(0, |at| at + 1);
        let mut newest = None;
        let mut counts = false;
        for event in events.get(since..).unwrap_or_default() {
            if let Kind::Message {
                held: Some(_),
                status,
                ..
            } = &event.kind
            {
                newest = Some(event.seq);
                counts |= status.is_none();
            }
        }
        let spent = counts && wake::spent(self.conversation.events()) >= wake::BUDGET;
        let started = match newest.filter(|_| !spent) {
            Some(of) => Some(
                self.conversation
                    .append(Kind::Started {
                        effect: Effect::Turn,
                        of,
                    })?
                    .clone(),
            ),
            None => None,
        };
        if let Some(started) = &started {
            self.send(&Up::Event(started.clone()));
        }
        self.log(Kind::Pause { paused: false })?;
        self.conversation.set_paused(false)?;
        self.sync()?;
        match started {
            Some(started) => self.turn(started.seq),
            None => Ok(()),
        }
    }

    /// Resumes a paused conversation for the human's own turn.
    fn resume(&mut self) -> Result<(), String> {
        if !self.conversation.meta().paused {
            return Ok(());
        }
        self.log(Kind::Pause { paused: false })?;
        self.conversation.set_paused(false)
    }

    /// The human cleared the todo list.
    fn clear_todo(&mut self) -> Result<(), String> {
        if todo(self.conversation.events()).is_empty() {
            return Ok(());
        }
        self.log(Kind::Todo {
            items: Vec::new(),
            cleared: true,
        })?;
        self.sync()
    }

    /// Logs what a stream that did not finish had brought, marked
    /// incomplete; one past what a log line holds is noted instead.
    fn partial(&mut self, request: u64, partial: Completion) -> Result<(), String> {
        let logged = self.log(Kind::Assistant {
            request,
            content: partial.content,
            reasoning: partial.reasoning,
            details: partial.details,
            finish: partial.finish,
            incomplete: true,
            calls: partial.calls,
        });
        if let Err(e) = logged {
            self.log(Kind::Notice {
                text: format!("the incomplete reply could not be logged: {e}"),
            })?;
        }
        Ok(())
    }

    /// Waits out a rate limit, still hearing the window: whether the
    /// human interrupted the turn meanwhile, or the window closed, when
    /// the next request could not be reserved.
    fn linger(&mut self, wait: Duration) -> bool {
        if self.gone {
            return true;
        }
        let deadline = Instant::now() + wait;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.inbox.recv_timeout(left) {
                Ok(Inbound::Down(Down::Interrupt)) => return true,
                Ok(Inbound::Down(Down::Reservation { id, refusal: None })) => self.spent(id, 0),
                Ok(Inbound::Down(down)) => self.queue.push_back(down),
                Ok(Inbound::Fetch { .. }) => {}
                Ok(Inbound::Checked { remote, result }) => self.checked.push_back((remote, result)),
                Ok(Inbound::Ended { number, how }) => self.exit(number, how),
                Ok(Inbound::Closed | Inbound::Broken(_)) => {
                    self.gone = true;
                    return true;
                }
                Err(_) => return false,
            }
        }
    }

    /// Sends turn request `request` as a stream and reads its reply as it
    /// comes, drawing what each frame brings in the window, until the
    /// reply ends or fails or the window interrupts it. A window that
    /// closes meanwhile leaves the reply to be read and logged whole.
    fn stream(&mut self, request: u64, client: &Client, key: &Secret, body: String) -> Streamed {
        self.live.store(request, Ordering::SeqCst);
        let url = format!("{}/chat/completions", client.base_url);
        let headers = client::headers(key.expose());
        let (send, live) = (self.sender.clone(), self.live.clone());
        let spawned = std::thread::Builder::new()
            .name("td-agent-stream".into())
            .spawn(move || fetch(&url, &headers, body.as_bytes(), request, &live, &send));
        if let Err(e) = spawned {
            return Streamed::Failed {
                failure: Failure::Stop {
                    status: None,
                    message: format!("the stream's thread: {e}"),
                },
                partial: None,
            };
        }
        let mut reading = Reading {
            head: None,
            plain: None,
            reader: sse::Reader::new(sse::MAX_EVENT, client::MAX_STREAM),
            reply: Assembly::default(),
        };
        let end = loop {
            let inbound = match self.inbox.recv() {
                Ok(inbound) => inbound,
                // The session holds a sender, so this does not happen.
                Err(_) => break reading.broken("the conversation's channel closed".into()),
            };
            match inbound {
                Inbound::Fetch { request: of, item } if of == request => {
                    if let Some(end) = self.fetched(request, item, &mut reading) {
                        break end;
                    }
                }
                Inbound::Fetch { .. } => {}
                Inbound::Checked { remote, result } => self.checked.push_back((remote, result)),
                Inbound::Ended { number, how } => self.exit(number, how),
                Inbound::Down(Down::Interrupt) => {
                    break Streamed::Failed {
                        failure: Failure::Interrupted {
                            usage: reading.reply.usage(),
                        },
                        partial: reading.partial(),
                    }
                }
                Inbound::Down(Down::Reservation { id, refusal: None }) => self.spent(id, 0),
                Inbound::Down(down) => self.queue.push_back(down),
                Inbound::Closed | Inbound::Broken(_) => self.gone = true,
            }
        };
        // Its thread reads no further than the frame it is waiting for.
        self.live.store(0, Ordering::SeqCst);
        end
    }

    /// One step of request `request`'s stream: what it ended in, when it
    /// has ended.
    fn fetched(&mut self, request: u64, item: Fetched, reading: &mut Reading) -> Option<Streamed> {
        match item {
            Fetched::Head { status, headers } => {
                let json = headers.iter().any(|(name, value)| {
                    name.eq_ignore_ascii_case("content-type")
                        && value
                            .trim_start()
                            .get(..16)
                            .is_some_and(|kind| kind.eq_ignore_ascii_case("application/json"))
                });
                if status != 200 || json {
                    reading.plain = Some(Vec::new());
                }
                reading.head = Some((status, headers));
                None
            }
            Fetched::Chunk(bytes) => {
                if let Some(plain) = reading.plain.as_mut() {
                    if plain.len().saturating_add(bytes.len()) > client::MAX_REPLY as usize {
                        return Some(
                            reading.broken(format!("a reply past {} bytes", client::MAX_REPLY)),
                        );
                    }
                    plain.extend_from_slice(&bytes);
                    return None;
                }
                let reply = &mut reading.reply;
                let fed = reading.reader.feed(&bytes, &mut |event| match event {
                    sse::Event::Data(text) => reply.event(text),
                    sse::Event::Done => Ok(()),
                });
                let (reasoning, content) = reading.reply.fresh();
                // A frame's events may complete one begun frames before,
                // so what they bring is sent in pieces a frame holds
                // however JSON escapes it.
                let reasoning = pieces(reasoning, DELTA_PIECE).into_iter().map(|r| (r, ""));
                let content = pieces(content, DELTA_PIECE).into_iter().map(|c| ("", c));
                for (reasoning, content) in reasoning.chain(content) {
                    self.send(&Up::Delta {
                        request,
                        reasoning: reasoning.to_string(),
                        content: content.to_string(),
                    });
                }
                match fed {
                    Err(Fault::Sink(failure)) => Some(Streamed::Failed {
                        failure,
                        partial: reading.partial(),
                    }),
                    Err(Fault::Reader(e)) => Some(reading.broken(e.to_string())),
                    // `[DONE]` with no finish is a stream cut short, as a
                    // counted reply with no choice is.
                    Ok(()) if reading.reader.done() && reading.reply.finished() => {
                        Some(reading.whole())
                    }
                    Ok(()) if reading.reader.done() => {
                        Some(reading.broken("the stream ended before the reply finished".into()))
                    }
                    Ok(()) => None,
                }
            }
            Fetched::End => match (reading.plain.take(), reading.head.take()) {
                (Some(body), Some((status, headers))) => Some(whole(status, headers, body)),
                // Whole without `[DONE]` only once a finish has come.
                _ if reading.reply.finished() => Some(reading.whole()),
                _ => Some(reading.broken("the stream ended before the reply finished".into())),
            },
            Fetched::Failed(e) => match (reading.plain.take(), reading.head.take()) {
                // Before the head: refused or unsent as a counted request
                // would have been.
                (_, None) => Some(match client::classify(Err(e)) {
                    Ok(completion) => Streamed::Replied(completion),
                    Err(failure) => Streamed::Failed {
                        failure,
                        partial: None,
                    },
                }),
                // An error status's body cut short: the status decides.
                (Some(body), Some((status, headers))) if status != 200 => {
                    Some(whole(status, headers, body))
                }
                _ => Some(reading.broken(e.to_string())),
            },
        }
    }

    /// The conversation's title from `title_model`, after its first
    /// exchange (DESIGN.md §13). Reserved like any request; whatever goes
    /// wrong leaves the first message's line as the title and says why.
    fn title(&mut self, turn: u64) -> Result<(), String> {
        let Some((Ok(key), client)) = self.setup.clone() else {
            return Ok(());
        };
        let events = self.conversation.events();
        let first = events.iter().find_map(|e| match &e.kind {
            Kind::User { text, .. } => Some(text.clone()),
            _ => None,
        });
        let reply = events.iter().rev().find_map(|e| match &e.kind {
            Kind::Assistant {
                content,
                incomplete: false,
                ..
            } => Some(content.clone().unwrap_or_default()),
            _ => None,
        });
        let (Some(first), Some(reply)) = (first, reply) else {
            return Ok(());
        };
        let model = match self.model("`title_model`", &client.title_model, &client) {
            Ok(model) => model,
            Err(why) => {
                self.log(Kind::Notice {
                    text: format!("no title: {why}"),
                })?;
                return Ok(());
            }
        };
        // A title request carries `max_tokens` too (§5).
        if model.as_ref().is_some_and(|m| !m.supports("max_tokens")) {
            self.log(Kind::Notice {
                text: format!(
                    "no title: {} takes no max_tokens (the provider's models list gives it no `max_tokens` parameter); set `title_model` to a model that does",
                    client.title_model
                ),
            })?;
            return Ok(());
        }
        let pricing = model.as_ref().and_then(|m| m.pricing);
        let head = client::title_head(&client, &first, &reply);
        let bytes = head.len() as u64 + 2;
        let reserved = pricing.map_or(0, |p| p.reserve(bytes.div_ceil(4), client::TITLE_TOKENS));
        let events = self.conversation.events();
        let within = cost::within(
            "max_cost_per_turn",
            client.limits.turn,
            accounts::turn_spent(events, turn),
            reserved,
        )
        .and_then(|()| {
            cost::within(
                "max_cost_per_conversation",
                client.limits.conversation,
                accounts::spent(events),
                reserved,
            )
        })
        .and_then(|()| self.reserve(reserved));
        let id = match within {
            Ok(id) => id,
            Err(why) => {
                self.log(Kind::Notice {
                    text: format!("no title: {why}"),
                })?;
                return Ok(());
            }
        };
        let request = self.log(Kind::Request {
            turn,
            purpose: Purpose::Title,
            prefix: 0,
            head: head.clone(),
            bytes,
            reserved,
        })?;
        self.sync()?;
        let body = format!("{{{head}}}");
        let (usage, cost, outcome) = match client::classify(post(&client, &key, &body)) {
            Ok(completion) => {
                let cost = charge(completion.usage, pricing, reserved);
                match client::title(completion.content.as_deref().unwrap_or_default()) {
                    Some(title) => {
                        self.log(Kind::Title {
                            request: request.seq,
                            text: title.clone(),
                        })?;
                        self.conversation.retitle(&title)?;
                        let title = self.conversation.meta().title.clone();
                        self.send(&Up::Title { title });
                        (completion.usage, cost, "titled".to_string())
                    }
                    None => (completion.usage, cost, "no title in the reply".to_string()),
                }
            }
            Err(failure) => {
                let (usage, cost) = match &failure {
                    Failure::Retryable { usage, .. } => match usage {
                        Some(client::Usage {
                            cost: Some(cost), ..
                        }) => (*usage, (*cost, Basis::Reported)),
                        _ => (*usage, (reserved, Basis::Reserved)),
                    },
                    _ => (None, (0, Basis::Nothing)),
                };
                (usage, cost, failure.outcome())
            }
        };
        self.settle(request.seq, usage, cost, outcome)?;
        self.spent(id, cost.0);
        Ok(())
    }
}

/// The todo list as `events` last wrote it.
pub fn todo(events: &[Event]) -> Vec<TodoItem> {
    events
        .iter()
        .rev()
        .find_map(|e| match &e.kind {
            Kind::Todo { items, .. } => Some(items.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

/// The todo item in progress, as `conversations` shows it.
fn doing(events: &[Event]) -> Option<String> {
    todo(events)
        .into_iter()
        .find(|i| i.status == Status::InProgress)
        .map(|i| i.content)
}

/// The most text one `delta` frame carries in a field: six times it,
/// escaped at its longest, is well within `frame::MAX_FRAME`.
const DELTA_PIECE: usize = 64 * 1024;

/// `text` in pieces of at most `most` bytes, cut at character boundaries.
fn pieces(text: &str, most: usize) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let mut cut = rest.len().min(most.max(4));
        while !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        let (Some(piece), Some(tail)) = (rest.get(..cut), rest.get(cut..)) else {
            break;
        };
        out.push(piece);
        rest = tail;
    }
    out
}

/// The frame the serve loop takes next of those that came while a turn
/// ran. Settings come first: a key the human stored, and the model and
/// effort they chose, apply to the next turn whatever was queued before
/// them (DESIGN.md §2, §4), each kind in its order. Then a pause goes
/// ahead of the messages from other conversations before it, and holds
/// them (§3), though not of the human's own message or retry; else the
/// first in order.
fn take(queue: &mut VecDeque<Down>) -> Option<Down> {
    let at = queue
        .iter()
        .position(|down| matches!(down, Down::Setup { .. } | Down::Choose { .. }))
        .or_else(|| {
            queue
                .iter()
                .position(|down| !matches!(down, Down::Message { .. }))
                .filter(|at| matches!(queue.get(*at), Some(Down::Pause { .. })))
        })
        .unwrap_or(0);
    queue.remove(at)
}

/// A streamed request's reply that came as one body: as a counted reply
/// is read.
fn whole(status: u16, headers: Vec<(String, String)>, body: Vec<u8>) -> Streamed {
    let response = td_fetch_client::Response {
        status,
        headers,
        body,
    };
    match client::classify(Ok(response)) {
        Ok(completion) => Streamed::Replied(completion),
        Err(failure) => Streamed::Failed {
            failure,
            partial: None,
        },
    }
}

/// A title request through the fetch service, counted.
fn post(
    client: &Client,
    key: &Secret,
    body: &str,
) -> Result<td_fetch_client::Response, td_fetch_client::Error> {
    let headers = client::headers(key.expose());
    let headers: Vec<(&str, &str)> = headers.iter().map(|(n, v)| (*n, v.as_str())).collect();
    td_fetch_client::post(
        &format!("{}/chat/completions", client.base_url),
        &headers,
        body.as_bytes(),
        Some(client::MAX_REPLY),
    )
}

/// Why `call` cannot run yet: it names a path in a worktree not ready,
/// or, by naming no directory, runs in the first worktree while that is
/// not, said with that worktree's state, one whose remote is `pending`
/// still checking out and any other not prepared (DESIGN.md §7). The
/// jail refuses such a call anyway; this says why. A relative path is
/// the tool host's to refuse, as it does every one (§8), and what a
/// command reaches by itself the jail's.
fn unready(
    repositories: &crate::workspace::Repositories,
    prepared: &[PathBuf],
    pending: &[&str],
    call: &host::Call,
) -> Option<String> {
    let first = repositories.entries.first()?;
    let named: Vec<Option<&str>> = match call {
        host::Call::Read { path, .. }
        | host::Call::Write { path, .. }
        | host::Call::Edit { path, .. } => vec![Some(path.as_str())],
        host::Call::Glob { path, .. } | host::Call::Grep { path, .. } => vec![path.as_deref()],
        host::Call::Shell { workdir, .. } | host::Call::Background { workdir, .. } => {
            vec![workdir.as_deref()]
        }
        host::Call::Sed { paths, .. } => paths.iter().map(|path| Some(path.as_str())).collect(),
        // td-agent's own, over the ready worktrees alone.
        host::Call::Snapshot { .. } | host::Call::Restore { .. } => Vec::new(),
    };
    named.into_iter().find_map(|path| {
        let at = match path.map(Path::new) {
            Some(path) if path.is_absolute() => lexical(path),
            Some(_) => return None,
            None => first.checkout.clone(),
        };
        let entry = repositories
            .entries
            .iter()
            .find(|entry| at.starts_with(&entry.checkout))?;
        if prepared.contains(&entry.repository) {
            return None;
        }
        let state = if pending.contains(&entry.remote.as_str()) {
            "is still being checked out; td-agent tells you when it is ready"
        } else {
            "could not be prepared; td-agent tries again when this conversation is next opened"
        };
        Some(format!("{} {state}", entry.checkout.display()))
    })
}

/// `path` with each `.` dropped and each `..` taking the name before it,
/// as written, without asking the file system.
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// A repository's checkout, run on a thread of its own: owned, so it holds
/// nothing of the conversation's (DESIGN.md §7).
struct Checkout {
    state: PathBuf,
    id: Id,
    entries: Vec<Entry>,
    fetched: Stored,
}

impl Checkout {
    /// Each worktree checked out, then each base's remote-tracking ref set
    /// at the commit its worktree starts from, so nothing has moved yet:
    /// those bases and commits.
    fn run(&self) -> Set {
        let entries: Vec<&Entry> = self.entries.iter().collect();
        check_out(&self.state, &self.id, &entries, &self.fetched)?;
        let heads: Vec<(String, String)> = self
            .entries
            .iter()
            .map(|entry| entry.base.clone())
            .zip(self.fetched.ids.iter().cloned())
            .fold(Vec::new(), |mut heads, (base, id)| {
                if !heads.iter().any(|(b, _)| *b == base) {
                    heads.push((base, id));
                }
                heads
            });
        track(&self.state, &self.id, &entries, &heads, true)?;
        Ok(heads)
    }
}

/// Lays out `entries`' repository, one remote's, and checks each
/// worktree out in a maintenance instance, at the bases `fetched`
/// resolved. Until it is recorded prepared no instance but these binds
/// the repository, so what a run cut short left is td-agent's own: a
/// whole repository or worktree id is used again, a checkout without
/// its id removed and made again, and an index is a checkout that
/// ended before it was recorded.
fn check_out(state: &Path, id: &Id, entries: &[&Entry], fetched: &Stored) -> Result<(), String> {
    let first = entries.first().ok_or("no worktree to check out")?;
    let repository = &first.repository;
    let programs = crate::jail::Programs::from_env()?;
    let git = crate::repo::host_git()?;
    if std::fs::symlink_metadata(repository).is_err() {
        crate::repo::create(repository, &first.store, &fetched.identity)?;
    }
    for entry in entries {
        let linked = repository.join("worktrees").join(&entry.id);
        if std::fs::symlink_metadata(&linked).is_ok() {
            continue;
        }
        if std::fs::symlink_metadata(&entry.checkout).is_ok() {
            std::fs::remove_dir_all(&entry.checkout)
                .map_err(|e| format!("{}: {e}", entry.checkout.display()))?;
        }
        crate::repo::add_worktree(
            repository,
            &crate::repo::Worktree {
                id: entry.id.clone(),
                checkout: entry.checkout.clone(),
                branch: entry.branch.clone(),
                sparse: entry.sparse.clone(),
            },
        )?;
    }
    let dir = crate::workspace::jail_dir(&StateDir::at(state.to_path_buf()), id);
    let policy = crate::workspace::maintenance(&dir, entries).ok_or("no worktree to check out")?;
    for (entry, base) in entries.iter().zip(&fetched.ids) {
        let index = repository.join("worktrees").join(&entry.id).join("index");
        if std::fs::symlink_metadata(&index).is_ok() {
            continue;
        }
        let task = crate::repo::Task::Checkout {
            git: git.clone(),
            repository: repository.clone(),
            id: entry.id.clone(),
            checkout: entry.checkout.clone(),
            branch: entry.branch.clone(),
            base: base.clone(),
        };
        crate::jail::maintain(
            &programs,
            &policy,
            &dir.join("specs"),
            &task,
            crate::repo::TASK_TIME,
        )
        .map_err(|why| {
            format!(
                "checking {} out with {}: {why}",
                entry.checkout.display(),
                git.display()
            )
        })?;
    }
    Ok(())
}

/// Sets `heads`' remote-tracking refs in the repository of `entries`,
/// one remote's, in a maintenance instance, `preparing` it.
fn track(
    state: &Path,
    id: &Id,
    entries: &[&Entry],
    heads: &[(String, String)],
    preparing: bool,
) -> Result<(), String> {
    let first = entries.first().ok_or("no worktree to track in")?;
    let programs = crate::jail::Programs::from_env()?;
    let git = crate::repo::host_git()?;
    let dir = crate::workspace::jail_dir(&StateDir::at(state.to_path_buf()), id);
    let policy = crate::workspace::maintenance(&dir, entries).ok_or("no worktree to track in")?;
    let task = crate::repo::Task::Track {
        git,
        repository: first.repository.clone(),
        id: first.id.clone(),
        checkout: first.checkout.clone(),
        heads: heads.to_vec(),
        preparing,
    };
    crate::jail::maintain(
        &programs,
        &policy,
        &dir.join("specs"),
        &task,
        crate::repo::TRACK_TIME,
    )
    .map(drop)
    .map_err(|why| format!("setting its remote-tracking refs: {why}"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use crate::store::tests::Scratch;

    /// A call into a worktree not ready is refused with its state; one
    /// ready, or outside every worktree, is the jail's to judge.
    #[test]
    fn a_call_into_a_worktree_not_ready_is_told_its_state() {
        let template = crate::config::Template {
            name: "td".into(),
            repos: vec![
                crate::config::Repo {
                    remote: "https://example.org/a/td".into(),
                    base: "main".into(),
                    branch: "agent".into(),
                    sparse: None,
                },
                crate::config::Repo {
                    remote: "https://example.org/a/docs".into(),
                    base: "main".into(),
                    branch: "agent".into(),
                    sparse: None,
                },
            ],
            shared: None,
        };
        let admitted = [crate::git::Admission::parse("example.org").unwrap()];
        let made = crate::workspace::repositories(
            &template,
            &Id::random().unwrap(),
            Path::new("/data"),
            Path::new("/trees"),
            &admitted,
            0,
        )
        .unwrap();
        let (td, docs) = (made.entries.first().unwrap(), made.entries.get(1).unwrap());
        let read = |path: &str| host::Call::Read {
            path: path.into(),
            offset: None,
            limit: None,
        };
        let shell = |workdir: Option<&str>| host::Call::Shell {
            command: "make".into(),
            timeout_ms: None,
            workdir: workdir.map(String::from),
        };
        let docs_file = docs.checkout.join("a").display().to_string();
        let none: [&str; 0] = [];
        // td ready, docs still checking out.
        let prepared = [td.repository.clone()];
        let pending = ["https://example.org/a/docs"];
        let said = unready(&made, &prepared, &pending, &read(&docs_file)).unwrap();
        assert!(
            said.starts_with(&docs.checkout.display().to_string()),
            "{said}"
        );
        assert!(said.contains("is still being checked out"), "{said}");
        assert_eq!(unready(&made, &prepared, &pending, &shell(None)), None);
        assert_eq!(
            unready(&made, &prepared, &pending, &read("src/main.rs")),
            None
        );
        assert_eq!(
            unready(&made, &prepared, &pending, &read("/elsewhere/x")),
            None
        );
        // A path climbing out of the first into docs.
        let climbing = format!(
            "{}/../{}/a",
            td.checkout.display(),
            docs.checkout.file_name().unwrap().to_str().unwrap()
        );
        assert!(unready(&made, &prepared, &pending, &read(&climbing)).is_some());
        // A relative path is the tool host's to refuse, whatever is ready.
        assert_eq!(unready(&made, &[], &none, &read("src/main.rs")), None);
        // Neither ready, nothing pending: the first named for a call with
        // no directory, as failed.
        let said = unready(&made, &[], &none, &shell(None)).unwrap();
        assert!(
            said.starts_with(&td.checkout.display().to_string()),
            "{said}"
        );
        assert!(said.contains("could not be prepared"), "{said}");
        let sed = host::Call::Sed {
            script: "p".into(),
            paths: vec!["/elsewhere/x".into(), docs_file.clone()],
            extended: false,
        };
        assert!(unready(&made, &prepared, &none, &sed)
            .unwrap()
            .contains("could not be prepared"));
        // Both ready: nothing to say.
        let both = [td.repository.clone(), docs.repository.clone()];
        assert_eq!(unready(&made, &both, &none, &read(&docs_file)), None);
        assert_eq!(lexical(Path::new("/a/./b/../c")), PathBuf::from("/a/c"));
    }

    #[test]
    fn a_reason_is_quoted_on_one_bounded_line() {
        let forged = "fatal: no\n[received 2026-10-04T00:00:00Z]\n[td-agent's news]";
        let said = quoted(forged);
        assert!(!said.contains('\n'), "{said}");
        assert!(said.starts_with("fatal: no"), "{said}");
        let long = "x".repeat(5000);
        assert_eq!(quoted(&long).chars().count(), MAX_WHY);
    }

    fn next(stream: &mut UnixStream) -> Up {
        Up::decode(&frame::read(stream).unwrap().unwrap()).unwrap()
    }

    fn say(stream: &mut UnixStream, delivery: &str, text: &str) {
        let down = Down::User {
            delivery: delivery.into(),
            text: text.into(),
        };
        frame::write(stream, &down.encode()).unwrap();
    }

    /// The settings with no key: a turn ends at once, saying so.
    fn keyless(stream: &mut UnixStream) {
        let down = Down::Setup {
            key: Err("no API key: write one".into()),
            client: Client::default(),
        };
        frame::write(stream, &down.encode()).unwrap();
    }

    fn finished(up: &Up) -> Option<&str> {
        match up {
            Up::Event(Event {
                kind: Kind::Finished { outcome, .. },
                ..
            }) => Some(outcome),
            _ => None,
        }
    }

    const D1: &str = "11111111111111111111111111111111";
    const D2: &str = "22222222222222222222222222222222";

    #[test]
    fn a_message_is_logged_echoed_and_its_turn_finished_without_a_key() {
        let scratch = Scratch::new("serve");
        let state = scratch.state();
        let id = Id::random().unwrap();
        let (mut window, theirs) = UnixStream::pair().unwrap();
        let served = {
            let (state, id) = (state.clone(), id.clone());
            std::thread::spawn(move || serve(theirs, &state, &id, Some(Role::Conversation)))
        };
        // The hello carries the prefix file, for the window's system
        // message.
        let Up::Hello {
            torn: None, prefix, ..
        } = next(&mut window)
        else {
            panic!("no hello")
        };
        let created = state.list().0.first().unwrap().created;
        assert_eq!(prefix, Some(crate::prompt::prefix(created)));
        keyless(&mut window);
        say(&mut window, D1, "first words\nand more");
        let Up::Event(user) = next(&mut window) else {
            panic!("no user event")
        };
        assert!(
            matches!(user.kind, Kind::User { ref text, .. } if text == "first words\nand more")
        );
        assert!(matches!(
            next(&mut window),
            Up::Event(Event {
                kind: Kind::Started { of: 1, .. },
                ..
            })
        ));
        assert_eq!(
            next(&mut window),
            Up::Delivered {
                delivery: D1.into()
            }
        );
        assert_eq!(
            next(&mut window),
            Up::Title {
                title: "first words".into()
            }
        );
        assert_eq!(finished(&next(&mut window)), Some("no API key: write one"));
        // The same delivery again is acknowledged, not logged again.
        say(&mut window, D1, "first words\nand more");
        assert_eq!(
            next(&mut window),
            Up::Delivered {
                delivery: D1.into()
            }
        );
        say(&mut window, D2, "   ");
        assert!(matches!(next(&mut window), Up::Refused { .. }));
        // Closing the window's end ends the process.
        drop(window);
        served.join().unwrap().unwrap();
        let (conversation, _) = Conversation::open(&state, &id, None, LOCK_WAIT).unwrap();
        assert_eq!(conversation.events().len(), 3);
    }

    /// A later `Setup`, the key the human stored from the window's dialog,
    /// replaces the first between turns: the next turn has the key.
    #[test]
    fn a_later_setup_hands_the_conversation_a_key() {
        let scratch = Scratch::new("rekey");
        let state = scratch.state();
        let id = Id::random().unwrap();
        let (mut window, theirs) = UnixStream::pair().unwrap();
        let served = {
            let (state, id) = (state.clone(), id.clone());
            std::thread::spawn(move || serve(theirs, &state, &id, Some(Role::Conversation)))
        };
        assert!(matches!(next(&mut window), Up::Hello { .. }));
        let outcome = |window: &mut UnixStream| loop {
            if let Some(outcome) = finished(&next(window)) {
                return outcome.to_string();
            }
        };
        keyless(&mut window);
        say(&mut window, D1, "one");
        assert_eq!(outcome(&mut window), "no API key: write one");
        let down = Down::Setup {
            key: Ok(Secret::new("sk-or-v1-stored".into())),
            client: Client::default(),
        };
        frame::write(&mut window, &down.encode()).unwrap();
        say(&mut window, D2, "two");
        let second = outcome(&mut window);
        assert!(!second.starts_with("no API key"), "{second}");
        assert!(!second.contains("sk-or-v1-stored"), "{second}");
        drop(window);
        served.join().unwrap().unwrap();
        let log = std::fs::read(state.conversation(&id).join("log")).unwrap();
        assert!(!String::from_utf8_lossy(&log).contains("sk-or-v1-stored"));
    }

    /// Queued settings go first, and a pause still goes ahead of the
    /// messages queued before it with a setup between them.
    #[test]
    fn queued_settings_go_first_and_a_pause_still_holds_messages() {
        let message = Down::Message {
            delivery: D1.into(),
            from: Id::random().unwrap(),
            role: Role::Orchestrator,
            text: "hi".into(),
            status: None,
        };
        let setup = Down::Setup {
            key: Ok(Secret::new("sk-or-v1-stored".into())),
            client: Client::default(),
        };
        let pause = Down::Pause { paused: true };
        let choose = |effort: &str| Down::Choose {
            model: None,
            effort: Some(effort.into()),
        };
        let mut queue: VecDeque<Down> = [
            message.clone(),
            choose("low"),
            setup.clone(),
            pause.clone(),
            choose("high"),
        ]
        .into();
        // Settings in the order they came, so the later choice is the one
        // the next turn keeps.
        assert_eq!(take(&mut queue), Some(choose("low")));
        assert_eq!(take(&mut queue), Some(setup));
        assert_eq!(take(&mut queue), Some(choose("high")));
        assert_eq!(take(&mut queue), Some(pause));
        assert_eq!(take(&mut queue), Some(message.clone()));
        assert_eq!(take(&mut queue), None);
        // The human's own message keeps its place ahead of a pause.
        let user = Down::User {
            delivery: D2.into(),
            text: "mine".into(),
        };
        let mut queue: VecDeque<Down> =
            [message.clone(), user.clone(), Down::Pause { paused: true }].into();
        assert_eq!(take(&mut queue), Some(message));
        assert_eq!(take(&mut queue), Some(user));
    }

    #[test]
    fn a_hello_carries_a_prefix_up_to_the_text_bound() {
        assert_eq!(hello_prefix("p").as_deref(), Some("p"));
        let at_bound = "x".repeat(MAX_TEXT);
        assert_eq!(hello_prefix(&at_bound), Some(at_bound.clone()));
        assert_eq!(hello_prefix(&format!("{at_bound}x")), None);
    }

    #[test]
    fn a_delta_is_cut_into_pieces_a_frame_holds() {
        assert!(pieces("", 4).is_empty());
        assert_eq!(pieces("abcdef", 4), ["abcd", "ef"]);
        assert_eq!(
            pieces(&"\u{e9}".repeat(5), 5),
            ["\u{e9}\u{e9}", "\u{e9}\u{e9}", "\u{e9}"]
        );
        // The longest piece, escaped at its longest, fits a frame.
        let worst = "\u{1}".repeat(DELTA_PIECE * 2 + 1);
        let cut = pieces(&worst, DELTA_PIECE);
        assert_eq!(cut.len(), 3);
        for piece in cut {
            let delta = Up::Delta {
                request: 1,
                reasoning: piece.into(),
                content: piece.into(),
            };
            assert!(delta.encode().len() <= frame::MAX_FRAME);
        }
    }

    #[test]
    fn a_turn_without_settings_says_so() {
        let scratch = Scratch::new("nosettings");
        let state = scratch.state();
        let id = Id::random().unwrap();
        let (mut window, theirs) = UnixStream::pair().unwrap();
        let served = {
            let (state, id) = (state.clone(), id.clone());
            std::thread::spawn(move || serve(theirs, &state, &id, Some(Role::Orchestrator)))
        };
        assert!(matches!(next(&mut window), Up::Hello { .. }));
        say(&mut window, D1, "hello");
        let outcome = loop {
            if let Some(outcome) = finished(&next(&mut window)) {
                break outcome.to_string();
            }
        };
        assert_eq!(outcome, NO_SETTINGS);
        drop(window);
        served.join().unwrap().unwrap();
    }

    #[test]
    fn a_window_gone_mid_turn_leaves_the_turn_whole_and_the_process_orderly() {
        let scratch = Scratch::new("gone");
        let state = scratch.state();
        let id = Id::random().unwrap();
        let (mut window, theirs) = UnixStream::pair().unwrap();
        let served = {
            let (state, id) = (state.clone(), id.clone());
            std::thread::spawn(move || serve(theirs, &state, &id, Some(Role::Conversation)))
        };
        assert!(matches!(next(&mut window), Up::Hello { .. }));
        keyless(&mut window);
        say(&mut window, D1, "said, then gone");
        drop(window);
        served.join().unwrap().unwrap();
        let (conversation, load) = Conversation::open(&state, &id, None, LOCK_WAIT).unwrap();
        assert!(load.interrupted.is_empty());
        let kinds: Vec<&Kind> = conversation.events().iter().map(|e| &e.kind).collect();
        assert!(
            matches!(
                kinds.as_slice(),
                [
                    Kind::User { .. },
                    Kind::Started { .. },
                    Kind::Finished { .. }
                ]
            ),
            "{kinds:?}"
        );
    }

    #[test]
    fn a_conversation_still_untitled_takes_its_title_from_the_next_message() {
        let scratch = Scratch::new("untitled");
        let state = scratch.state();
        let id = Id::random().unwrap();
        {
            // A message logged whose title was never written.
            let (mut conversation, _) =
                Conversation::open(&state, &id, Some(Role::Conversation), LOCK_WAIT).unwrap();
            conversation
                .append(Kind::User {
                    delivery: D1.into(),
                    text: "lost title".into(),
                })
                .unwrap();
            conversation.sync().unwrap();
        }
        let (mut window, theirs) = UnixStream::pair().unwrap();
        let served = {
            let (state, id) = (state.clone(), id.clone());
            std::thread::spawn(move || serve(theirs, &state, &id, None))
        };
        assert!(matches!(next(&mut window), Up::Hello { .. }));
        // The replay: the message, the start load gave it, its interruption.
        for _ in 0..3 {
            next(&mut window);
        }
        keyless(&mut window);
        say(&mut window, D2, "found title");
        let title = loop {
            if let Up::Title { title } = next(&mut window) {
                break title;
            }
        };
        assert_eq!(title, "found title");
        drop(window);
        served.join().unwrap().unwrap();
    }

    #[test]
    fn a_restart_replays_the_log_in_order() {
        let scratch = Scratch::new("restart");
        let state = scratch.state();
        let id = Id::random().unwrap();
        for (round, delivery) in [D1, D2].into_iter().enumerate() {
            let (mut window, theirs) = UnixStream::pair().unwrap();
            let served = {
                let (state, id) = (state.clone(), id.clone());
                let create = (round == 0).then_some(Role::Orchestrator);
                std::thread::spawn(move || serve(theirs, &state, &id, create))
            };
            assert!(matches!(next(&mut window), Up::Hello { .. }));
            // The earlier round's three events come back first.
            for seq in 1..=(round as u64 * 3) {
                match next(&mut window) {
                    Up::Event(event) => assert_eq!(event.seq, seq),
                    other => panic!("{other:?}"),
                }
            }
            keyless(&mut window);
            say(&mut window, delivery, "hi");
            for _ in 0..4 {
                next(&mut window);
            }
            drop(window);
            served.join().unwrap().unwrap();
        }
    }
}

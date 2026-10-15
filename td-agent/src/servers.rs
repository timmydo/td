//! The conversation's MCP servers (DESIGN.md §12, MCP servers): each
//! server's client on a thread of its own, which drains its output,
//! answers its requests and reads its lines whether or not a turn runs,
//! and which the conversation's main thread asks to call and to list;
//! and the main thread's record of which run, with what, and how often
//! each may start again.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use td_json::Json;

use crate::config::McpServer;
use crate::host::{self, Client};
use crate::mcp::{Failure, RpcError, Server, Started, Transport};

/// The most of a server's standard error kept, its last bytes.
pub const TAIL: usize = 2 * 1024;
/// The most input held for a server that is not reading it: a request
/// with the most arguments a call has, and room.
const MAX_HELD: usize = 1024 * 1024;
/// How long a wait for output looks before it sends what is held again,
/// since the credit a `Took` gives wakes no wait.
const HELD_SLICE: Duration = Duration::from_millis(20);
/// How long a server waits on its output between asks.
const PUMP: Duration = Duration::from_millis(50);
/// How long an ended server is given to exit once its input is closed.
pub const GRACE: Duration = Duration::from_secs(2);
/// How often a server that failed is started again in a process's life.
pub const MAX_RESTARTS: u32 = 2;
/// How often a server's tools are listed again in a process's life.
pub const MAX_RELISTS: u32 = 8;

/// The last at most `max` bytes of `text`, from a character's start.
fn last(text: &str, max: usize) -> &str {
    let mut from = text.len().saturating_sub(max);
    while !text.is_char_boundary(from) {
        from += 1;
    }
    text.get(from..).unwrap_or_default()
}

/// A transport the thread can end and read the standard error of.
pub trait Ending: Transport {
    /// The last `TAIL` bytes of its standard error, as text.
    fn tail(&self) -> String;
    /// Closes its input, waits `GRACE` for it to exit, and tears it down.
    fn end(&mut self);
}

/// A server's `serve` call on its tool host, as a transport.
pub struct Pipe {
    client: Client,
    call: u64,
    held: Vec<u8>,
    tail: String,
    /// Why the call ended, once it has.
    gone: Option<String>,
}

impl Pipe {
    pub fn new(client: Client, call: u64) -> Self {
        Self {
            client,
            call,
            held: Vec::new(),
            tail: String::new(),
            gone: None,
        }
    }

    /// Sends what the input's credit has room for of what is held.
    fn flush(&mut self) -> Result<(), String> {
        if self.held.is_empty() {
            return Ok(());
        }
        let sent = self.client.input(self.call, &self.held)?;
        self.held.drain(..sent.min(self.held.len()));
        Ok(())
    }

    /// Keeps the last `TAIL` bytes of the standard error with `text`.
    fn keep(&mut self, text: &str) {
        self.tail.push_str(last(text, TAIL));
        let kept = last(&self.tail, TAIL).len();
        let from = self.tail.len().saturating_sub(kept);
        self.tail.drain(..from);
    }

    /// The call's end as a server's failure: how its program ended, or
    /// why it could not run. The tool host's words are the jail's, so
    /// they are only quoted.
    fn ended(outcome: Result<host::Done, String>) -> String {
        match outcome {
            Ok(done) => format!(
                "the server exited: {:?}",
                crate::mcp::quoted(&done.text, 300)
            ),
            Err(why) => format!(
                "the server could not run: {:?}",
                crate::mcp::quoted(&why, 300)
            ),
        }
    }
}

impl Transport for Pipe {
    fn send(&mut self, bytes: &[u8]) -> Result<(), String> {
        if let Some(why) = &self.gone {
            return Err(why.clone());
        }
        if self.held.len().saturating_add(bytes.len()) > MAX_HELD {
            return Err("the server does not read its input".into());
        }
        self.held.extend_from_slice(bytes);
        self.flush()
    }

    fn receive(&mut self, deadline: Instant) -> Result<Option<Vec<u8>>, String> {
        loop {
            if let Some(why) = &self.gone {
                return Err(why.clone());
            }
            self.flush()?;
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            let wait = if self.held.is_empty() {
                left
            } else {
                left.min(HELD_SLICE)
            };
            match self.client.next_reply(wait) {
                None => {}
                Some(Ok(host::Up::RawOutput { id, bytes })) if id == self.call => {
                    return Ok(Some(bytes))
                }
                Some(Ok(host::Up::Output { id, text })) if id == self.call => self.keep(&text),
                Some(Ok(host::Up::Done { id, outcome })) if id == self.call => {
                    self.gone = Some(Self::ended(outcome));
                }
                Some(Ok(_)) => {}
                Some(Err(why)) => self.gone = Some(format!("its tool host: {why}")),
            }
        }
    }
}

impl Ending for Pipe {
    fn tail(&self) -> String {
        self.tail.clone()
    }

    fn end(&mut self) {
        if self.gone.is_none() && self.client.close_input(self.call).is_ok() {
            let until = Instant::now() + GRACE;
            while self.gone.is_none() {
                let left = until.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                match self.client.next_reply(left) {
                    Some(Ok(host::Up::Output { id, text })) if id == self.call => self.keep(&text),
                    Some(Ok(host::Up::Done { id, .. })) if id == self.call => {
                        self.gone = Some("ended".into())
                    }
                    Some(Err(_)) => self.gone = Some("ended".into()),
                    _ => {}
                }
            }
        }
        // The client's drop tears the instance down.
    }
}

/// What the main thread asks of a server's thread.
pub enum Ask {
    /// Call `tool` with `arguments` until `until`, `stop` ending the
    /// wait, `begun` set once the thread takes it up; the outcome goes to
    /// `answer`.
    Call {
        tool: String,
        arguments: Json,
        until: Instant,
        stop: Arc<AtomicBool>,
        begun: Arc<AtomicBool>,
        answer: Sender<Result<Result<Json, RpcError>, Failure>>,
    },
    /// List its tools again.
    List,
}

/// Why a server failed, with the tail of its standard error.
#[derive(Clone, Debug, PartialEq)]
pub struct Failed {
    pub why: String,
    pub tail: String,
}

/// What a server's thread tells the main thread.
#[derive(Clone, Debug, PartialEq)]
pub enum Report {
    /// Its start: what it listed, or why it failed.
    Started(Result<Started, Failed>),
    /// Its tools listed again, with whether what lay past was left, or
    /// why that failed, which ends it.
    Listed(Result<(Vec<Json>, bool), Failed>),
    /// It said its tools changed.
    Changed,
    /// It failed after its start, and is ended.
    Failed(Failed),
}

/// A report of server `server`'s start `generation`, as it reaches the
/// main thread.
#[derive(Clone, Debug, PartialEq)]
pub struct Told {
    pub server: String,
    pub generation: u64,
    pub report: Report,
}

/// Runs a server over `transport`: starts it within `start_time`, tells
/// what came of it, then answers `asks` and reads what it says until
/// `end` is set, the asks' sender is dropped, or it fails; then ends it.
/// A failure while `end` is set is its own end, and is not told.
pub fn serve<T: Ending>(
    transport: T,
    start_time: Duration,
    clock: &Clock,
    asks: &Receiver<Ask>,
    end: &AtomicBool,
    tell: &dyn Fn(Report),
) {
    let mut server = Server::new(transport);
    let ending = || end.load(Ordering::Relaxed);
    let failed = |server: &mut Server<T>, failure: Failure| Failed {
        why: failure.to_string(),
        tail: server.transport().tail(),
    };
    let until = Instant::now() + start_time;
    match server.start(&mut || until + clock.stopped(), &mut || ending()) {
        Ok(started) => tell(Report::Started(Ok(started))),
        Err(failure) => {
            if !ending() {
                let failed = failed(&mut server, failure);
                tell(Report::Started(Err(failed)));
            }
            server.transport().end();
            return;
        }
    }
    let mut changed = false;
    loop {
        if ending() {
            break;
        }
        let broke = match asks.try_recv() {
            Ok(Ask::Call {
                tool,
                arguments,
                until,
                stop,
                begun,
                answer,
            }) => {
                begun.store(true, Ordering::Relaxed);
                let left = until.saturating_duration_since(Instant::now());
                // Stopped before it was taken up, a call is never sent:
                // the turn may have answered it already.
                let outcome = if stop.load(Ordering::Relaxed) {
                    Err(Failure::Stopped)
                } else if left.is_zero() {
                    Err(Failure::TimedOut)
                } else {
                    let mut stopped = || stop.load(Ordering::Relaxed) || ending();
                    server.call(&tool, arguments, left, &mut stopped)
                };
                let broke = match &outcome {
                    Err(failure @ Failure::Broke(_)) => Some(failure.clone()),
                    _ => None,
                };
                let _ = answer.send(outcome);
                broke
            }
            Ok(Ask::List) => {
                let until = Instant::now() + start_time;
                let before = clock.stopped();
                let mut deadline = || until + clock.stopped().saturating_sub(before);
                match server.list(&mut deadline, &mut || ending()) {
                    Ok(listed) => {
                        changed = false;
                        tell(Report::Listed(Ok(listed)));
                        None
                    }
                    Err(failure) => {
                        if !ending() {
                            let failed = failed(&mut server, failure);
                            tell(Report::Listed(Err(failed)));
                        }
                        break;
                    }
                }
            }
            Err(TryRecvError::Disconnected) => break,
            Err(TryRecvError::Empty) => match server.pump(PUMP) {
                Err(failure) => Some(failure),
                Ok(()) => {
                    if server.list_changed && !changed {
                        changed = true;
                        tell(Report::Changed);
                    }
                    None
                }
            },
        };
        if let Some(failure) = broke {
            if !ending() {
                let failed = failed(&mut server, failure);
                tell(Report::Failed(failed));
            }
            break;
        }
    }
    server.transport().end();
}

/// What a server's instance was launched with, which a change of
/// restarts it: the workspace's mounts.
pub type Launch = (Vec<crate::workspace::Shared>, Vec<std::path::PathBuf>);

/// A server the main thread started.
pub struct Running {
    pub config: McpServer,
    launch: Launch,
    /// Which of its starts this is, so a report of an earlier one is
    /// told apart.
    pub generation: u64,
    asks: Sender<Ask>,
    end: Arc<AtomicBool>,
    thread: JoinHandle<()>,
    /// Its protocol version, once it started.
    pub protocol: Option<String>,
    /// It said its tools changed since they were last listed.
    pub changed: bool,
    /// Its start's clock, stopped while a connection waits on a card.
    pub clock: Arc<Clock>,
}

/// The first owner of a server's connections, past any call's sequence
/// number and below the window's card ids: a server's instance is linked
/// to its start, not to a call (DESIGN.md §12, Its connections).
pub const LINKS: u64 = 1 << 61;

/// The owner of the connections of a server's start `generation`.
pub fn owner(generation: u64) -> u64 {
    LINKS.saturating_add(generation)
}

/// A start's clock, stopped while a connection of its server waits on
/// the person (DESIGN.md §12, Its connections): how long it has stopped.
#[derive(Debug, Default)]
pub struct Clock(Mutex<(Option<Instant>, Duration)>);

impl Clock {
    /// Stops it, unless it is stopped.
    pub fn stop(&self) {
        if let Ok(mut clock) = self.0.lock() {
            clock.0.get_or_insert_with(Instant::now);
        }
    }

    /// Starts it again, unless it runs.
    pub fn go(&self) {
        if let Ok(mut clock) = self.0.lock() {
            if let Some(since) = clock.0.take() {
                clock.1 = clock.1.saturating_add(since.elapsed());
            }
        }
    }

    /// How long it has been stopped, until now.
    pub fn stopped(&self) -> Duration {
        self.0.lock().map_or(Duration::ZERO, |clock| {
            clock
                .1
                .saturating_add(clock.0.map_or(Duration::ZERO, |since| since.elapsed()))
        })
    }
}

/// The conversation's servers, as its main thread knows them.
#[derive(Default)]
pub struct Servers {
    running: BTreeMap<String, Running>,
    /// Ended servers' threads, joined at the process's end.
    ending: Vec<JoinHandle<()>>,
    /// Servers that failed and have not started since, with what they
    /// ran as and were launched with.
    failed: BTreeMap<String, (McpServer, Launch)>,
    /// Restarts after a failure, by server.
    restarts: BTreeMap<String, u32>,
    /// Listings again, by server.
    relists: BTreeMap<String, u32>,
    next: u64,
}

impl Servers {
    /// Ends every server not in `wanted`, changed there, or launched with
    /// other mounts than `launch`: the servers to
    /// start, those of `wanted` not running, but for one that failed when
    /// it has started again `MAX_RESTARTS` times after a failure, which
    /// this start counts. One changed, or launched otherwise, since it
    /// failed starts as a new server would, its restarts counted afresh.
    pub fn plan(&mut self, wanted: &[McpServer], launch: &Launch) -> Vec<McpServer> {
        let stale: Vec<String> = self
            .running
            .iter()
            .filter(|(name, running)| {
                !wanted
                    .iter()
                    .any(|w| &w.name == *name && *w == running.config)
                    || running.launch != *launch
            })
            .map(|(name, _)| name.clone())
            .collect();
        for name in stale {
            self.end(&name);
        }
        let mut start = Vec::new();
        for server in wanted {
            if self.running.contains_key(&server.name) {
                continue;
            }
            if let Some((config, launched)) = self.failed.get(&server.name) {
                if config == server && launched == launch {
                    let restarts = self.restarts.entry(server.name.clone()).or_default();
                    if *restarts >= MAX_RESTARTS {
                        continue;
                    }
                    *restarts += 1;
                } else {
                    self.restarts.remove(&server.name);
                }
                self.failed.remove(&server.name);
            }
            start.push(server.clone());
        }
        start
    }

    /// The generation the next start is, whose owner its instance is
    /// launched linked to.
    pub fn upcoming(&self) -> u64 {
        self.next.saturating_add(1)
    }

    /// Runs `config` over `transport` on a thread of its own, launched
    /// with `launch`, telling what comes of it through `tell` with its
    /// generation: that generation, `upcoming`'s, or why no thread could
    /// be made.
    pub fn add<T: Ending + Send + 'static>(
        &mut self,
        config: McpServer,
        launch: Launch,
        transport: T,
        tell: impl Fn(u64, Report) + Send + 'static,
    ) -> Result<u64, String> {
        self.next = self.next.saturating_add(1);
        let generation = self.next;
        let (asks, asked) = std::sync::mpsc::channel();
        let end = Arc::new(AtomicBool::new(false));
        let ended = Arc::clone(&end);
        let clock = Arc::new(Clock::default());
        let held = Arc::clone(&clock);
        let thread = std::thread::Builder::new()
            .name(format!("mcp {}", config.name))
            .spawn(move || {
                serve(
                    transport,
                    crate::mcp::START_TIME,
                    &held,
                    &asked,
                    &ended,
                    &|report| tell(generation, report),
                )
            })
            .map_err(|e| format!("a thread for it: {e}"))?;
        self.running.insert(
            config.name.clone(),
            Running {
                config,
                launch,
                generation,
                asks,
                end,
                thread,
                protocol: None,
                changed: false,
                clock,
            },
        );
        Ok(generation)
    }

    /// The running server whose connections `owner` owns.
    pub fn owning(&self, owner: u64) -> Option<&Running> {
        self.running
            .values()
            .find(|running| self::owner(running.generation) == owner)
    }

    /// Each running server's clock stopped while `waits` says a
    /// connection of its own waits on the person, and going otherwise.
    pub fn hold(&self, waits: impl Fn(u64) -> bool) {
        for running in self.running.values() {
            if waits(owner(running.generation)) {
                running.clock.stop();
            } else {
                running.clock.go();
            }
        }
    }

    /// `name`'s server, when the report of `generation` is its.
    pub fn current(&mut self, name: &str, generation: u64) -> Option<&mut Running> {
        self.running
            .get_mut(name)
            .filter(|running| running.generation == generation)
    }

    /// `name`'s server, running.
    pub fn get(&self, name: &str) -> Option<&Running> {
        self.running.get(name)
    }

    /// Ends `name`'s server, which is not counted as a failure.
    pub fn end(&mut self, name: &str) {
        if let Some(running) = self.running.remove(name) {
            running.end.store(true, Ordering::Relaxed);
            // Dropping its asks ends the thread's wait for them.
            drop(running.asks);
            self.ending.push(running.thread);
        }
        self.ending.retain(|thread| !thread.is_finished());
    }

    /// Ends `name`'s server as failed: it starts again only within
    /// `MAX_RESTARTS`.
    pub fn fail(&mut self, name: &str) {
        if let Some(running) = self.running.get(name) {
            let ran = (running.config.clone(), running.launch.clone());
            self.failed.insert(name.to_string(), ran);
        }
        self.end(name);
    }

    /// `config`, launched with `launch`, could not be started: as a
    /// failure, it starts again only within `MAX_RESTARTS`.
    pub fn unstarted(&mut self, config: &McpServer, launch: &Launch) {
        let ran = (config.clone(), launch.clone());
        self.failed.insert(config.name.clone(), ran);
    }

    /// Asks `name`'s server to list its tools again, when it said they
    /// changed and is within `MAX_RELISTS`: whether it was asked.
    pub fn relist(&mut self, name: &str) -> bool {
        let Some(running) = self.running.get_mut(name) else {
            return false;
        };
        let relists = self.relists.entry(name.to_string()).or_default();
        if !running.changed || *relists >= MAX_RELISTS || running.asks.send(Ask::List).is_err() {
            return false;
        }
        *relists += 1;
        running.changed = false;
        true
    }

    /// Whether `name`'s server has been listed again as often as a
    /// process's life allows, so a change it says is not listed.
    pub fn relists_spent(&self, name: &str) -> bool {
        self.relists.get(name).is_some_and(|n| *n >= MAX_RELISTS)
    }

    /// Asks `name`'s server to make a call: false when it is not running.
    pub fn ask(&self, name: &str, ask: Ask) -> bool {
        self.running
            .get(name)
            .is_some_and(|running| running.asks.send(ask).is_ok())
    }

    /// Ends every server and waits for their threads, each within its
    /// grace and its instance's teardown.
    pub fn end_all(&mut self) {
        let names: Vec<String> = self.running.keys().cloned().collect();
        for name in names {
            self.end(&name);
        }
        for thread in self.ending.drain(..) {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
    use super::*;
    use std::collections::VecDeque;
    use std::sync::mpsc;

    /// A server whose every reply `answer` makes from the message it
    /// answers; a reply of `GONE` ends it.
    struct Fake {
        answer: fn(&Json) -> Vec<String>,
        out: VecDeque<Vec<u8>>,
        gone: bool,
        ended: Arc<AtomicBool>,
    }

    impl Fake {
        fn new(answer: fn(&Json) -> Vec<String>) -> (Self, Arc<AtomicBool>) {
            let ended = Arc::new(AtomicBool::new(false));
            let fake = Self {
                answer,
                out: VecDeque::new(),
                gone: false,
                ended: Arc::clone(&ended),
            };
            (fake, ended)
        }
    }

    impl Transport for Fake {
        fn send(&mut self, bytes: &[u8]) -> Result<(), String> {
            if self.gone {
                return Err("gone".into());
            }
            for line in bytes.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
                let message = td_json::parse_slice(line).map_err(|e| e.to_string())?;
                for reply in (self.answer)(&message) {
                    if reply == "GONE" {
                        self.gone = true;
                    } else {
                        self.out.push_back(format!("{reply}\n").into_bytes());
                    }
                }
            }
            Ok(())
        }

        fn receive(&mut self, deadline: Instant) -> Result<Option<Vec<u8>>, String> {
            if let Some(bytes) = self.out.pop_front() {
                return Ok(Some(bytes));
            }
            if self.gone {
                return Err("the server exited".into());
            }
            std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
            Ok(None)
        }
    }

    impl Ending for Fake {
        fn tail(&self) -> String {
            "it said goodbye".into()
        }

        fn end(&mut self) {
            self.ended.store(true, Ordering::Relaxed);
        }
    }

    fn reply(message: &Json, result: &str) -> String {
        let id = message.get("id").map(Json::to_string).unwrap_or_default();
        format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{result}}}")
    }

    /// Starts, lists one tool, and answers calls: `die` ends it, and
    /// `change` says its tools changed first.
    fn tools(message: &Json) -> Vec<String> {
        let method = message.get("method").and_then(Json::as_str);
        let tool = message
            .get("params")
            .and_then(|p| p.get("name"))
            .and_then(Json::as_str);
        match (method, tool) {
            (Some("initialize"), _) => vec![reply(
                message,
                "{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{}}",
            )],
            (Some("tools/list"), _) => vec![reply(
                message,
                "{\"tools\":[{\"name\":\"echo\",\"inputSchema\":{\"type\":\"object\"}}]}",
            )],
            (Some("tools/call"), Some("die")) => vec!["GONE".into()],
            (Some("tools/call"), Some("change")) => vec![
                "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}".into(),
                reply(message, "{\"content\":[]}"),
            ],
            (Some("tools/call"), _) => vec![reply(
                message,
                "{\"content\":[{\"type\":\"text\",\"text\":\"hi\"}]}",
            )],
            _ => Vec::new(),
        }
    }

    /// Never answers.
    fn silent(_: &Json) -> Vec<String> {
        Vec::new()
    }

    struct Run {
        asks: Sender<Ask>,
        end: Arc<AtomicBool>,
        told: Receiver<Report>,
        thread: JoinHandle<()>,
        ended: Arc<AtomicBool>,
    }

    fn run(answer: fn(&Json) -> Vec<String>, start_time: Duration) -> Run {
        let (fake, ended) = Fake::new(answer);
        let (asks, asked) = mpsc::channel();
        let (tell, told) = mpsc::channel();
        let end = Arc::new(AtomicBool::new(false));
        let ending = Arc::clone(&end);
        let thread = std::thread::spawn(move || {
            serve(
                fake,
                start_time,
                &Clock::default(),
                &asked,
                &ending,
                &|report| {
                    let _ = tell.send(report);
                },
            )
        });
        Run {
            asks,
            end,
            told,
            thread,
            ended,
        }
    }

    fn call(run: &Run, tool: &str) -> Result<Result<Json, RpcError>, Failure> {
        let (answer, answered) = mpsc::channel();
        run.asks
            .send(Ask::Call {
                tool: tool.into(),
                arguments: Json::Obj(Vec::new()),
                until: Instant::now() + WAIT,
                stop: Arc::new(AtomicBool::new(false)),
                begun: Arc::new(AtomicBool::new(false)),
                answer,
            })
            .unwrap();
        answered.recv_timeout(WAIT).unwrap()
    }

    const WAIT: Duration = Duration::from_secs(5);

    /// A call stopped before its thread takes it up is never sent, so an
    /// interrupt its turn answered does not reach the server later.
    #[test]
    fn a_call_stopped_before_it_is_taken_up_is_never_sent() {
        static CALLED: AtomicBool = AtomicBool::new(false);
        fn marks(message: &Json) -> Vec<String> {
            if message.get("method").and_then(Json::as_str) == Some("tools/call") {
                CALLED.store(true, Ordering::Relaxed);
            }
            tools(message)
        }
        let run = run(marks, WAIT);
        let (answer, answered) = mpsc::channel();
        run.asks
            .send(Ask::Call {
                tool: "echo".into(),
                arguments: Json::Obj(Vec::new()),
                until: Instant::now() + WAIT,
                stop: Arc::new(AtomicBool::new(true)),
                begun: Arc::new(AtomicBool::new(false)),
                answer,
            })
            .unwrap();
        let outcome = answered.recv_timeout(WAIT).unwrap();
        assert!(matches!(outcome, Err(Failure::Stopped)), "{outcome:?}");
        assert!(!CALLED.load(Ordering::Relaxed));
        // The server is not failed: a call after is answered.
        let result = call(&run, "echo").unwrap().unwrap();
        assert_eq!(
            crate::mcp::result_text(&result, &mut |_| String::new()),
            ("hi".to_string(), false)
        );
        assert!(CALLED.load(Ordering::Relaxed));
        run.end.store(true, Ordering::Relaxed);
        run.thread.join().unwrap();
    }

    #[test]
    fn a_server_starts_answers_calls_and_ends_untold() {
        let run = run(tools, WAIT);
        let Report::Started(Ok(started)) = run.told.recv_timeout(WAIT).unwrap() else {
            panic!("no start");
        };
        assert_eq!(started.protocol, "2025-06-18");
        assert_eq!(started.tools.len(), 1);
        let result = call(&run, "echo").unwrap().unwrap();
        assert_eq!(
            crate::mcp::result_text(&result, &mut |_| String::new()),
            ("hi".to_string(), false)
        );
        run.end.store(true, Ordering::Relaxed);
        run.thread.join().unwrap();
        assert!(run.ended.load(Ordering::Relaxed));
        // Its own end is no failure.
        assert!(run.told.try_recv().is_err());
    }

    #[test]
    fn a_server_that_exits_is_told_failed_with_its_tail() {
        let run = run(tools, WAIT);
        assert!(matches!(
            run.told.recv_timeout(WAIT),
            Ok(Report::Started(Ok(_)))
        ));
        assert!(matches!(call(&run, "die"), Err(Failure::Broke(_))));
        let Report::Failed(failed) = run.told.recv_timeout(WAIT).unwrap() else {
            panic!("no failure");
        };
        assert!(failed.why.contains("exited"), "{}", failed.why);
        assert_eq!(failed.tail, "it said goodbye");
        run.thread.join().unwrap();
        assert!(run.ended.load(Ordering::Relaxed));
    }

    /// A start's time does not run while its clock is stopped, and runs
    /// on from where it was when it goes again.
    #[test]
    fn a_starts_time_waits_while_its_clock_is_stopped() {
        let (fake, _) = Fake::new(silent);
        let (_asks, asked) = mpsc::channel();
        let (tell, told) = mpsc::channel();
        let clock = Arc::new(Clock::default());
        clock.stop();
        let held = Arc::clone(&clock);
        let thread = std::thread::spawn(move || {
            let end = AtomicBool::new(false);
            serve(
                fake,
                Duration::from_millis(100),
                &held,
                &asked,
                &end,
                &|report| {
                    let _ = tell.send(report);
                },
            )
        });
        // Stopped, its 100 ms do not run out.
        assert!(told.recv_timeout(Duration::from_millis(300)).is_err());
        assert!(clock.stopped() >= Duration::from_millis(300));
        clock.go();
        let went = Instant::now();
        let Report::Started(Err(failed)) = told.recv_timeout(WAIT).unwrap() else {
            panic!("no failure");
        };
        assert!(failed.why.contains("in time"), "{}", failed.why);
        assert!(went.elapsed() < Duration::from_secs(2));
        thread.join().unwrap();
    }

    /// A listing again, like a start, waits while the clock is stopped,
    /// counting only what it stopped since the listing was asked.
    #[test]
    fn a_listing_again_waits_while_its_clock_is_stopped() {
        static LISTS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        // The first listing answered, every later one never.
        fn once(message: &Json) -> Vec<String> {
            if message.get("method").and_then(Json::as_str) == Some("tools/list")
                && LISTS.fetch_add(1, Ordering::Relaxed) > 0
            {
                return Vec::new();
            }
            tools(message)
        }
        let (fake, _) = Fake::new(once);
        let (asks, asked) = mpsc::channel();
        let (tell, told) = mpsc::channel();
        let clock = Arc::new(Clock::default());
        let held = Arc::clone(&clock);
        let thread = std::thread::spawn(move || {
            let end = AtomicBool::new(false);
            serve(
                fake,
                Duration::from_millis(100),
                &held,
                &asked,
                &end,
                &|report| {
                    let _ = tell.send(report);
                },
            )
        });
        assert!(matches!(
            told.recv_timeout(WAIT),
            Ok(Report::Started(Ok(_)))
        ));
        clock.stop();
        asks.send(Ask::List).unwrap();
        assert!(told.recv_timeout(Duration::from_millis(300)).is_err());
        clock.go();
        let Report::Listed(Err(failed)) = told.recv_timeout(WAIT).unwrap() else {
            panic!("no failure");
        };
        assert!(failed.why.contains("in time"), "{}", failed.why);
        drop(asks);
        thread.join().unwrap();
    }

    /// `hold` stops the clock of each running server a card waits for,
    /// and only that one's, and starts it again when none does.
    #[test]
    fn hold_stops_only_the_clocks_of_servers_a_card_waits_for() {
        let mut servers = Servers::default();
        let config =
            |name: &str| crate::config::mcp_server(name, vec!["x".into()], None, None).unwrap();
        let launch: Launch = Default::default();
        let first = servers
            .add(config("a"), launch.clone(), Fake::new(tools).0, |_, _| {})
            .unwrap();
        servers
            .add(config("b"), launch, Fake::new(tools).0, |_, _| {})
            .unwrap();
        assert_eq!(
            servers.owning(owner(first)).map(|r| r.config.name.as_str()),
            Some("a")
        );
        servers.hold(|waits| waits == owner(first));
        std::thread::sleep(Duration::from_millis(50));
        let a = servers.get("a").unwrap().clock.stopped();
        assert!(a >= Duration::from_millis(50), "{a:?}");
        assert_eq!(servers.get("b").unwrap().clock.stopped(), Duration::ZERO);
        servers.hold(|_| false);
        let held = servers.get("a").unwrap().clock.stopped();
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(servers.get("a").unwrap().clock.stopped(), held);
        servers.end_all();
    }

    #[test]
    fn a_start_that_times_out_is_told_and_one_ended_is_not() {
        let timed = run(silent, Duration::from_millis(200));
        let Report::Started(Err(failed)) = timed.told.recv_timeout(WAIT).unwrap() else {
            panic!("no failure");
        };
        assert!(failed.why.contains("in time"), "{}", failed.why);
        timed.thread.join().unwrap();
        let ended = run(silent, WAIT);
        ended.end.store(true, Ordering::Relaxed);
        ended.thread.join().unwrap();
        assert!(ended.told.try_recv().is_err());
        assert!(ended.ended.load(Ordering::Relaxed));
    }

    #[test]
    fn a_change_is_told_once_and_listed_again_when_asked() {
        let run = run(tools, WAIT);
        assert!(matches!(
            run.told.recv_timeout(WAIT),
            Ok(Report::Started(Ok(_)))
        ));
        call(&run, "change").unwrap().unwrap();
        assert_eq!(run.told.recv_timeout(WAIT), Ok(Report::Changed));
        call(&run, "change").unwrap().unwrap();
        run.asks.send(Ask::List).unwrap();
        let Report::Listed(Ok((tools, more))) = run.told.recv_timeout(WAIT).unwrap() else {
            panic!("no listing");
        };
        assert_eq!((tools.len(), more), (1, false));
        // Listed again, a change is told again.
        call(&run, "change").unwrap().unwrap();
        assert_eq!(run.told.recv_timeout(WAIT), Ok(Report::Changed));
        drop(run.asks);
        run.thread.join().unwrap();
    }

    fn server(name: &str, command: &str) -> McpServer {
        crate::config::mcp_server(name, vec![command.into()], None, None).unwrap()
    }

    fn launch(prepared: usize) -> Launch {
        let path = std::path::PathBuf::from(format!("/r{prepared}"));
        (Vec::new(), vec![path])
    }

    #[test]
    fn a_plan_ends_what_changed_and_restarts_a_failure_twice() {
        let mut servers = Servers::default();
        let wanted = vec![server("a", "/a"), server("b", "/b")];
        let start = servers.plan(&wanted, &launch(1));
        assert_eq!(start, wanted);
        let mut ended = Vec::new();
        for one in start {
            let (fake, flag) = Fake::new(silent);
            ended.push(flag);
            servers.add(one, launch(1), fake, |_, _| {}).unwrap();
        }
        // Running as wanted, nothing starts.
        assert!(servers.plan(&wanted, &launch(1)).is_empty());
        // `b` changed: it is ended and started again.
        let changed = vec![server("a", "/a"), server("b", "/b2")];
        assert_eq!(servers.plan(&changed, &launch(1)), vec![server("b", "/b2")]);
        assert!(servers.get("b").is_none());
        // Other mounts end `a` too.
        assert_eq!(servers.plan(&changed, &launch(2)), changed);
        assert!(servers.get("a").is_none());
        servers.end_all();
        for flag in ended {
            assert!(flag.load(Ordering::Relaxed));
        }
        // A failure starts again at most twice.
        let mut servers = Servers::default();
        let one = vec![server("a", "/a")];
        for _ in 0..=MAX_RESTARTS {
            assert_eq!(servers.plan(&one, &launch(1)), one);
            let (fake, _) = Fake::new(silent);
            let generation = servers
                .add(server("a", "/a"), launch(1), fake, |_, _| {})
                .unwrap();
            assert!(servers.current("a", generation).is_some());
            assert!(servers.current("a", generation + 1).is_none());
            servers.fail("a");
        }
        assert!(servers.plan(&one, &launch(1)).is_empty());
        // Changed since it failed, it starts afresh, as does one launched
        // otherwise; one that could not be started counts as failed.
        let fixed = vec![server("a", "/a2")];
        assert_eq!(servers.plan(&fixed, &launch(1)), fixed);
        let mut servers = Servers::default();
        servers.unstarted(&server("a", "/a"), &launch(1));
        servers.restarts.insert("a".into(), MAX_RESTARTS);
        assert!(servers.plan(&one, &launch(1)).is_empty());
        servers.unstarted(&server("a", "/a"), &launch(1));
        assert_eq!(servers.plan(&one, &launch(2)), one);
        servers.end_all();
    }

    #[test]
    fn a_server_is_listed_again_only_when_it_changed_within_the_bound() {
        let mut servers = Servers::default();
        let (fake, _) = Fake::new(silent);
        servers
            .add(server("a", "/a"), launch(1), fake, |_, _| {})
            .unwrap();
        assert!(!servers.relist("a"));
        for _ in 0..MAX_RELISTS {
            servers.running.get_mut("a").unwrap().changed = true;
            assert!(!servers.relists_spent("a"));
            assert!(servers.relist("a"));
        }
        assert!(servers.relists_spent("a"));
        servers.running.get_mut("a").unwrap().changed = true;
        assert!(!servers.relist("a"));
        servers.end_all();
    }
}

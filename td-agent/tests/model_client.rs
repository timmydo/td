//! The model client end to end (DESIGN.md §5, §6, §17): the built
//! program's conversation personality as a child process, driven over its
//! socketpair by this harness standing in for the window, its requests
//! answered by a mock fetch service replaying OpenRouter exchanges from
//! `tests/fixtures/openrouter/`. The fixtures are written in the shapes
//! OpenRouter's API returns, by hand: no test here reaches the network,
//! and the live check against OpenRouter is by hand and never in the gate
//! (DESIGN.md §17).
#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

#[path = "support/mock_fetch.rs"]
#[allow(dead_code)]
mod mock_fetch;

use std::collections::BTreeMap;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use mock_fetch::{fixture, MockFetch, Reply, Tail};
use td_agent::config::Client;
use td_agent::cost::{Limits, ONE};
use td_agent::frame;
use td_agent::key::Secret;
use td_agent::models::Models;
use td_agent::protocol::{Down, Up};
use td_agent::store::{Basis, Conversation, Event, Held, Id, Kind, Purpose, Role, StateDir};

/// Whether `content` is `text` after its `[received <UTC time>]` line.
fn is_sent(content: &str, text: &str) -> bool {
    content
        .strip_prefix("[received ")
        .and_then(|rest| rest.split_once("Z]\n"))
        .is_some_and(|(time, rest)| time.len() == 19 && rest == text)
}

const PROGRAM: &str = env!("CARGO_BIN_EXE_td-agent");
const KEY: &str = "sk-or-v1-feedface0123456789abcdef";
const TIMEOUT: Duration = Duration::from_secs(20);

/// How the harness answers a conversation's reservations.
#[derive(Clone)]
enum Day {
    Grant,
    Refuse(String),
}

/// A conversation process, its state, and the fetch service it talks to.
struct Harness {
    root: PathBuf,
    state: StateDir,
    id: Id,
    child: Child,
    window: UnixStream,
    mock: MockFetch,
    day: Day,
    /// Every reservation asked for and every cost reported.
    reserved: Vec<(u64, u64)>,
    spent: Vec<(u64, u64)>,
    /// Every message sent through the window: to and text.
    sent: Vec<(Id, String)>,
    /// Every streamed delta: its request, reasoning and text.
    deltas: Vec<(u64, String, String)>,
    /// Events `until_text` heard, which the next `turn` begins with.
    heard: Vec<Event>,
    /// Why each trip of the classifier's breaker was asked for.
    brakes: Vec<String>,
    /// What the last card `until_ask` heard offered to remember.
    always: Option<td_agent::rules::Offer>,
    stderr: PathBuf,
}

impl Harness {
    fn new(tag: &str, role: Role, script: Vec<Reply>) -> Self {
        Self::new_in(tag, role, None, false, script)
    }

    /// A conversation created in `workspace`, `scratch` or a directory,
    /// given td-jail and td-txt from this process's environment when
    /// `jailed`.
    fn new_in(
        tag: &str,
        role: Role,
        workspace: Option<&str>,
        jailed: bool,
        script: Vec<Reply>,
    ) -> Self {
        let name = format!(
            "td-agent-model-{tag}-{}-{}",
            std::process::id(),
            td_agent::store::random_hex(4).unwrap()
        );
        let root = std::env::temp_dir().join(&name);
        std::fs::create_dir(&root).unwrap();
        // A jailed one's state lies outside `/tmp`, which td-jail
        // reserves; its sockets stay in `root`, within a socket path's
        // bound.
        let state = StateDir::at(if jailed {
            PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name)
        } else {
            root.join("state")
        });
        state.ensure().unwrap();
        let runtime = root.join("run");
        std::fs::create_dir(&runtime).unwrap();
        let mock = MockFetch::start(&runtime, script);
        // The models list as the window would have cached it.
        Models::from_provider(&fixture("models.json"))
            .unwrap()
            .save(state.root())
            .unwrap();
        let id = Id::random().unwrap();
        let stderr = root.join("stderr");
        let (child, window) = spawn(
            &state,
            &id,
            Some(role),
            workspace,
            jailed,
            mock.runtime(),
            &stderr,
        );
        let mut harness = Self {
            root,
            state,
            id,
            child,
            window,
            mock,
            day: Day::Grant,
            reserved: Vec::new(),
            spent: Vec::new(),
            sent: Vec::new(),
            deltas: Vec::new(),
            brakes: Vec::new(),
            heard: Vec::new(),
            always: None,
            stderr,
        };
        assert!(matches!(harness.next(), Up::Hello { .. }));
        harness
    }

    fn setup(&mut self, client: Client) {
        let down = Down::Setup {
            key: Ok(Secret::new(KEY.into())),
            client: Box::new(client),
        };
        frame::write(&mut self.window, &down.encode()).unwrap();
    }

    fn next(&mut self) -> Up {
        self.window.set_read_timeout(Some(TIMEOUT)).unwrap();
        let bytes = frame::read(&mut self.window)
            .unwrap_or_else(|e| panic!("{e}: {}", self.said()))
            .unwrap_or_else(|| panic!("the process closed: {}", self.said()));
        Up::decode(&bytes).unwrap()
    }

    fn down(&mut self, down: &Down) {
        frame::write(&mut self.window, &down.encode()).unwrap();
    }

    fn say(&mut self, text: &str) {
        let delivery = td_agent::store::random_hex(16).unwrap();
        self.down(&Down::User {
            delivery,
            text: text.into(),
        });
    }

    /// Answers a reservation as the day says, or keeps a delta: whether
    /// `up` was either.
    fn hear(&mut self, up: &Up) -> bool {
        match up {
            Up::Reserve { id, amount } => {
                self.reserved.push((*id, *amount));
                let refusal = match &self.day {
                    Day::Grant => None,
                    Day::Refuse(why) => Some(why.clone()),
                };
                self.down(&Down::Reservation { id: *id, refusal });
            }
            Up::Spent { id, amount } => self.spent.push((*id, *amount)),
            // The window queues a message and knows no conversation's
            // state.
            Up::Send { id, to, text } => {
                self.sent.push((to.clone(), text.clone()));
                self.down(&Down::Sent {
                    id: *id,
                    refusal: None,
                });
            }
            Up::Query { id } => self.down(&Down::States {
                id: *id,
                states: Vec::new(),
            }),
            Up::Brake { why } => self.brakes.push(why.clone()),
            Up::Delta {
                request,
                reasoning,
                content,
            } => self
                .deltas
                .push((*request, reasoning.clone(), content.clone())),
            _ => return false,
        }
        true
    }

    /// What the process said until a delta brought text, reservations
    /// answered on the way; the events, for `turn` to go on from.
    fn until_text(&mut self) {
        loop {
            let up = self.next();
            if self.hear(&up) {
                if matches!(up, Up::Delta { ref content, .. } if !content.is_empty()) {
                    return;
                }
            } else if let Up::Event(event) = up {
                self.heard.push(event);
            }
        }
    }

    /// What the process said until `count` reservations were spent; the
    /// events, for `turn` to go on from.
    fn until_spent(&mut self, count: usize) {
        while self.spent.len() < count {
            let up = self.next();
            if !self.hear(&up) {
                if let Up::Event(event) = up {
                    self.heard.push(event);
                }
            }
        }
    }

    /// What the process said until it asked the person a card,
    /// reservations answered on the way: the card's call, title and
    /// details, the events kept for `turn`.
    fn until_ask(&mut self) -> (u64, String, Vec<String>) {
        loop {
            let up = self.next();
            if self.hear(&up) {
                continue;
            }
            match up {
                Up::Ask {
                    call,
                    title,
                    details,
                    always,
                } => {
                    self.always = always;
                    return (call, title, details);
                }
                Up::Event(event) => self.heard.push(event),
                _ => {}
            }
        }
    }

    /// What the process said until it withdrew a card: its call.
    fn until_withdrawn(&mut self) -> u64 {
        loop {
            let up = self.next();
            if self.hear(&up) {
                continue;
            }
            match up {
                Up::Withdraw { call } => return call,
                Up::Event(event) => self.heard.push(event),
                _ => {}
            }
        }
    }

    /// The text every delta of request `request` brought.
    fn streamed(&self, request: u64) -> (String, String) {
        let mut out = (String::new(), String::new());
        for (of, reasoning, content) in &self.deltas {
            if *of == request {
                out.0.push_str(reasoning);
                out.1.push_str(content);
            }
        }
        out
    }

    /// What the process said up to its turn's end, reservations answered
    /// on the way: the events, and the turn's outcome and retry flag.
    fn turn(&mut self) -> (Vec<Event>, String, bool) {
        let mut events = std::mem::take(&mut self.heard);
        let mut turns: Vec<u64> = events
            .iter()
            .filter(|e| matches!(e.kind, Kind::Started { .. }))
            .map(|e| e.seq)
            .collect();
        loop {
            let up = self.next();
            if self.hear(&up) {
                continue;
            }
            if let Up::Event(event) = up {
                if let Kind::Started { .. } = event.kind {
                    turns.push(event.seq);
                }
                let end = match &event.kind {
                    Kind::Finished {
                        started,
                        outcome,
                        retry,
                    } if turns.contains(started) => Some((outcome.clone(), *retry)),
                    _ => None,
                };
                events.push(event);
                if let Some((outcome, retry)) = end {
                    return (events, outcome, retry);
                }
            }
        }
    }

    fn said(&self) -> String {
        std::fs::read_to_string(&self.stderr).unwrap_or_default()
    }

    /// Starts the conversation's process again, as the window opens it.
    fn reopen(&mut self) {
        let (child, window) = spawn(
            &self.state,
            &self.id,
            None,
            None,
            false,
            self.mock.runtime(),
            &self.stderr,
        );
        self.child = child;
        self.window = window;
        assert!(matches!(self.next(), Up::Hello { .. }));
    }

    /// The conversation as the store holds it, once its process is gone.
    fn close(mut self) -> (Conversation, Self) {
        let _ = self.window.shutdown(std::net::Shutdown::Both);
        wait(&mut self.child);
        let (conversation, _) =
            Conversation::open(&self.state, &self.id, None, Duration::from_secs(3)).unwrap();
        (conversation, self)
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.root);
        let _ = std::fs::remove_dir_all(self.state.root());
    }
}

fn spawn(
    state: &StateDir,
    id: &Id,
    create: Option<Role>,
    workspace: Option<&str>,
    jailed: bool,
    runtime: &std::path::Path,
    stderr: &std::path::Path,
) -> (Child, UnixStream) {
    let (ours, theirs) = UnixStream::pair().unwrap();
    let mut command = Command::new(PROGRAM);
    command
        .args(["conversation", id.as_str(), "--state-dir"])
        .arg(state.root())
        .env_clear()
        .env("XDG_RUNTIME_DIR", runtime);
    if let Some(role) = create {
        command.args(["--create", role.word()]);
    }
    if let Some(workspace) = workspace {
        command.args(["--workspace", workspace]);
    }
    if jailed {
        for var in [td_agent::jail::JAIL_VAR, td_agent::jail::TXT_VAR] {
            command.env(
                var,
                std::env::var_os(var).unwrap_or_else(|| panic!("{var}")),
            );
        }
    }
    let child = command
        .stdin(Stdio::from(OwnedFd::from(theirs.try_clone().unwrap())))
        .stdout(Stdio::from(OwnedFd::from(theirs)))
        .stderr(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(stderr)
                .unwrap(),
        )
        .spawn()
        .unwrap();
    (child, ours)
}

fn wait(child: &mut Child) {
    let deadline = Instant::now() + TIMEOUT;
    while child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "the process did not exit");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A JSON text's leaves by dotted path; strings bare, the rest as
/// written.
fn flat(text: &str) -> BTreeMap<String, String> {
    fn walk(prefix: &str, value: &td_json::Json, out: &mut BTreeMap<String, String>) {
        let join = |key: &str| {
            if prefix.is_empty() {
                key.to_string()
            } else {
                format!("{prefix}.{key}")
            }
        };
        match value {
            td_json::Json::Obj(members) => {
                for (key, value) in members {
                    walk(&join(key), value, out);
                }
            }
            td_json::Json::Arr(items) => {
                for (i, value) in items.iter().enumerate() {
                    walk(&join(&i.to_string()), value, out);
                }
            }
            td_json::Json::Str(text) => {
                out.insert(prefix.to_string(), text.clone());
            }
            other => {
                out.insert(prefix.to_string(), other.to_string());
            }
        }
    }
    let value = td_json::parse(text).unwrap_or_else(|e| panic!("{e}: {text}"));
    let mut out = BTreeMap::new();
    walk("", &value, &mut out);
    out
}

fn kinds(events: &[Event]) -> Vec<&'static str> {
    events
        .iter()
        .map(|e| match &e.kind {
            Kind::User { .. } => "user",
            Kind::Started { .. } => "started",
            Kind::Finished { .. } => "finished",
            Kind::Interrupted { .. } => "interrupted",
            Kind::Notice { .. } => "notice",
            Kind::Notification { .. } => "notification",
            Kind::Prefix { .. } => "prefix",
            Kind::Request { .. } => "request",
            Kind::Assistant { .. } => "assistant",
            Kind::Usage { .. } => "usage",
            Kind::Title { .. } => "title",
            Kind::Message { .. } => "message",
            Kind::ToolCall { .. } => "tool_call",
            Kind::ToolResult { .. } => "tool_result",
            Kind::Todo { .. } => "todo",
            Kind::Snapshot { .. } => "snapshot",
            Kind::Restore { .. } => "restore",
            Kind::Process { .. } => "process",
            Kind::Ended { .. } => "ended",
            Kind::Compaction { .. } => "compaction",
            Kind::Pause { .. } => "pause",
            Kind::Choice { .. } => "choice",
            Kind::Approval { .. } => "approval",
        })
        .collect()
}

fn usage(events: &[Event]) -> Vec<(u64, Basis)> {
    events
        .iter()
        .filter_map(|e| match &e.kind {
            Kind::Usage { cost, basis, .. } => Some((*cost, *basis)),
            _ => None,
        })
        .collect()
}

/// The streamed sonnet reply's reasoning details as assembled from its
/// three fragments of index 0 and serialized once.
const SONNET_DETAILS: &str = "[{\"type\":\"reasoning.text\",\
    \"text\":\"The person asks what a sparse checkout is. Answer plainly.\",\
    \"format\":\"anthropic-claude-v1\",\"index\":0,\
    \"signature\":\"EqQBCkgIBxABGAIiQL/x+Jq0dXN0IHNpZ25hdHVyZQ==\\n\"}]";
const SONNET_TEXT: &str = "A sparse checkout keeps only the paths you name in the working tree; the rest stay in the repository\u{2019}s objects.";

/// The reply events of `events`: request, text, reasoning details and
/// whether it is incomplete.
fn replies(events: &[Event]) -> Vec<(u64, Option<String>, Option<String>, bool)> {
    events
        .iter()
        .filter_map(|e| match &e.kind {
            Kind::Assistant {
                request,
                content,
                details,
                incomplete,
                ..
            } => Some((*request, content.clone(), details.clone(), *incomplete)),
            _ => None,
        })
        .collect()
}

#[test]
fn a_turn_is_sent_as_the_design_says_logged_whole_and_titled() {
    let mut h = Harness::new(
        "roundtrip",
        Role::Conversation,
        vec![Reply::sse("stream-sonnet.sse"), Reply::ok("title.json")],
    );
    h.setup(Client::default());
    h.say("What is a sparse checkout?");
    let (events, outcome, retry) = h.turn();
    assert_eq!((outcome.as_str(), retry), ("replied", false));
    assert_eq!(
        kinds(&events),
        [
            "user",
            "started",
            "request",
            "assistant",
            "usage",
            "finished",
            "request",
            "title",
            "usage",
            "finished",
            "finished"
        ]
    );
    let requests = h.mock.requests();
    assert_eq!(requests.len(), 2);
    let turn = &requests[0];
    assert_eq!(turn.method, "POST");
    assert_eq!(turn.url, "https://openrouter.ai/api/v1/chat/completions");
    assert_eq!(
        turn.headers,
        [
            ("authorization".to_string(), format!("Bearer {KEY}")),
            ("content-type".to_string(), "application/json".to_string()),
            (
                "http-referer".to_string(),
                "https://github.com/timmydo/td".to_string()
            ),
            ("x-openrouter-title".to_string(), "td-agent".to_string()),
        ]
    );
    // The turn streams, the frames' sum bounded; the title is counted.
    assert!(turn.stream);
    assert_eq!(turn.limit, Some(32 * 1024 * 1024));
    assert!(!requests[1].stream);
    assert_eq!(requests[1].limit, Some(512 * 1024));
    let body = flat(&turn.text());
    assert_eq!(body["model"], "anthropic/claude-sonnet-5.5");
    assert_eq!(body["max_tokens"], "16384");
    assert_eq!(body["stream"], "true");
    // Drawn as it came, a delta per frame that brought anything, and
    // logged whole once it ended, its reasoning details as assembled.
    let reply = replies(&events);
    assert_eq!(
        reply,
        [(
            reply[0].0,
            Some(SONNET_TEXT.to_string()),
            Some(SONNET_DETAILS.to_string()),
            false
        )]
    );
    let (reasoning, text) = h.streamed(reply[0].0);
    assert_eq!(text, SONNET_TEXT);
    assert_eq!(
        reasoning,
        "The person asks what a sparse checkout is. Answer plainly."
    );
    assert!(h.deltas.len() > 2, "{:?}", h.deltas);
    assert_eq!(body["reasoning.effort"], "medium");
    assert_eq!(body["provider.require_parameters"], "true");
    assert_eq!(body["provider.data_collection"], "deny");
    assert_eq!(body["cache_control.type"], "ephemeral");
    assert_eq!(body["messages.0.role"], "system");
    // A message the model is given begins with when it was sent.
    assert!(
        is_sent(&body["messages.1.content"], "What is a sparse checkout?"),
        "{}",
        body["messages.1.content"]
    );
    // Every request carries the conversation's tools.
    assert!(!body.contains_key("tool_choice"));
    assert!(!body.contains_key("parallel_tool_calls"));
    assert_eq!(body["tools.0.type"], "function");
    assert_eq!(body["tools.0.function.name"], "todo_write");
    assert_eq!(body["tools.4.function.name"], "send_message");
    assert!(!body.contains_key("tools.5.type"), "no report");
    // `require_parameters` routes only to an endpoint that lists every
    // parameter sent, so each member, but those it does not route on, is
    // one the model lists; the title's as well.
    let models = Models::from_provider(&fixture("models.json")).unwrap();
    for (request, model) in [
        (turn, "anthropic/claude-sonnet-5.5"),
        (&requests[1], "anthropic/claude-haiku-4.5"),
    ] {
        let model = models.find(model).unwrap();
        let td_json::Json::Obj(members) = td_json::parse(&request.text()).unwrap() else {
            panic!("the body is not an object")
        };
        for (name, _) in &members {
            let routed = !["model", "messages", "stream", "provider", "cache_control"]
                .contains(&name.as_str());
            assert!(
                !routed || model.supports(name),
                "{name} is not among {}'s supported_parameters",
                model.id
            );
        }
    }
    // The title comes from the title model, quoting the first exchange.
    let title = flat(&requests[1].text());
    assert_eq!(title["model"], "anthropic/claude-haiku-4.5");
    assert!(title["messages.1.content"].contains("What is a sparse checkout?"));
    // Reserved worst case: the estimated prompt at the cache-write rate,
    // max_tokens at the completion rate. Spent what usage said.
    let request = events
        .iter()
        .find_map(|e| match &e.kind {
            Kind::Request {
                purpose: Purpose::Turn,
                reserved,
                bytes,
                ..
            } => Some((*reserved, *bytes)),
            _ => None,
        })
        .unwrap();
    let prompt = request.1.div_ceil(4);
    assert_eq!(request.0, prompt * 3_750_000 + 16_384 * 15_000_000);
    let first = h.reserved[0].0;
    assert_eq!(h.reserved[0].1, request.0);
    assert_eq!(h.spent, [(first, 4_350_000_000), (first + 1, 131_000_000)]);
    assert_eq!(
        usage(&events),
        [
            (4_350_000_000, Basis::Reported),
            (131_000_000, Basis::Reported)
        ]
    );
    let (conversation, h) = h.close();
    assert_eq!(conversation.meta().title, "Sparse checkouts explained");
    // The body sent is the body the log rebuilds.
    let at = conversation
        .events()
        .iter()
        .position(|e| {
            matches!(
                e.kind,
                Kind::Request {
                    purpose: Purpose::Turn,
                    ..
                }
            )
        })
        .unwrap();
    assert_eq!(
        td_agent::client::body(conversation.events(), at, conversation.prefix_file()).unwrap(),
        turn.text()
    );
    // The key is in the one header, and nowhere the program keeps or says
    // anything.
    let dir = h.state.conversation(&h.id);
    for name in ["log", "meta", "prefix"] {
        let text = std::fs::read_to_string(dir.join(name)).unwrap();
        assert!(!text.contains(KEY), "the key is in {name}");
    }
    assert!(!h.said().contains(KEY));
    assert!(!turn.text().contains(KEY));
}

/// Two turns, the second from a new process: its body begins with every
/// byte the first sent, reasoning details spliced back exactly as they
/// were stored. The second reply streams with CRLF line endings.
#[test]
fn the_next_request_replays_the_log_and_the_reasoning_byte_for_byte() {
    let mut h = Harness::new(
        "replay",
        Role::Orchestrator,
        vec![Reply::sse("stream-sonnet.sse")],
    );
    h.setup(Client::default());
    h.say("What is a sparse checkout?");
    let (_, outcome, _) = h.turn();
    assert_eq!(outcome, "replied");
    let (_, mut h) = h.close();
    // A restart: a new process over the same log.
    let (child, window) = spawn(
        &h.state,
        &h.id,
        None,
        None,
        false,
        h.mock.runtime(),
        &h.stderr,
    );
    h.child = child;
    h.window = window;
    assert!(matches!(h.next(), Up::Hello { .. }));
    for _ in 0..7 {
        assert!(matches!(h.next(), Up::Event(_)));
    }
    h.mock.then(vec![Reply::sse_crlf("stream-gemini.sse")]);
    h.setup(Client::default());
    h.say("And after a base advances?");
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied");
    let requests = h.mock.requests();
    let (first, second) = (requests[0].text(), requests[1].text());
    let kept = first.strip_suffix("]}").unwrap();
    assert!(
        second.starts_with(kept),
        "the second body does not begin with the first:\n{first}\n{second}"
    );
    assert!(
        second.contains(&format!(",\"reasoning_details\":{SONNET_DETAILS}}}")),
        "{second}"
    );
    flat(&second);
    // CRLF framing reads as LF does: a text entry and an encrypted one.
    let reply = replies(&events);
    assert_eq!(
        reply[0].1.as_deref(),
        Some("Rebase onto the new base, then run the tests again.")
    );
    assert_eq!(
        reply[0].2.as_deref(),
        Some(
            "[{\"type\":\"reasoning.text\",\"text\":\"**Planning the answer**\\n\\nThe person wants the order of steps.\",\"format\":\"google-gemini-v1\",\"index\":0},\
             {\"type\":\"reasoning.encrypted\",\"data\":\"CiQBjz1rX3J5dGhtLXNpZ25hdHVyZS1ieXRlcw==\",\"id\":\"tool_0\",\"format\":\"google-gemini-v1\",\"index\":1}]"
        )
    );
    assert_eq!(usage(&events), [(2_150_000_000, Basis::Reported)]);
}

/// A conversation begun when the prefix carried `tool_choice` and
/// `parallel_tool_calls` takes this program's prefix as an event before
/// its next request, which carries neither.
#[test]
fn a_prefix_with_the_members_once_sent_is_replaced_before_the_next_request() {
    let h = Harness::new(
        "old-prefix",
        Role::Orchestrator,
        vec![Reply::sse("stream-sonnet.sse")],
    );
    let (_, mut h) = h.close();
    let file = h.state.conversation(&h.id).join("prefix");
    let current = std::fs::read_to_string(&file).unwrap();
    let old = current.replacen(
        ",\"messages\":",
        ",\"tool_choice\":\"auto\",\"parallel_tool_calls\":true,\"messages\":",
        1,
    );
    assert_ne!(old, current);
    std::fs::write(&file, &old).unwrap();
    let (child, window) = spawn(
        &h.state,
        &h.id,
        None,
        None,
        false,
        h.mock.runtime(),
        &h.stderr,
    );
    h.child = child;
    h.window = window;
    assert!(matches!(h.next(), Up::Hello { .. }));
    h.setup(Client::default());
    h.say("hello");
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied");
    let logged = kinds(&events);
    let prefix = logged.iter().position(|k| *k == "prefix").unwrap();
    let request = logged.iter().position(|k| *k == "request").unwrap();
    assert!(prefix < request, "{logged:?}");
    let body = h.mock.requests()[0].text();
    let sent = flat(&body);
    assert!(!sent.contains_key("tool_choice") && !sent.contains_key("parallel_tool_calls"));
    // The body holds this program's prefix, its `messages` left open.
    let members = current
        .strip_prefix('{')
        .and_then(|t| t.strip_suffix("]}"))
        .unwrap();
    assert!(body.contains(members), "{body}");
}

/// The human's choice of model and effort for a conversation is logged,
/// kept in `meta`, and what its next request sends; a chosen model the
/// list does not have is refused naming where it was chosen.
#[test]
fn a_chosen_model_and_effort_are_what_the_next_request_sends() {
    let mut h = Harness::new(
        "choose",
        Role::Conversation,
        vec![Reply::sse("stream-sonnet.sse"), Reply::ok("title.json")],
    );
    h.setup(Client::default());
    h.down(&Down::Choose {
        model: Some("anthropic/claude-haiku-4.5".into()),
        effort: Some("high".into()),
    });
    h.say("hello");
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied");
    let logged = kinds(&events);
    let choice = logged.iter().position(|k| *k == "choice").unwrap();
    let request = logged.iter().position(|k| *k == "request").unwrap();
    assert!(choice < request, "{logged:?}");
    let body = flat(&h.mock.requests()[0].text());
    assert_eq!(body["model"], "anthropic/claude-haiku-4.5");
    assert_eq!(body["reasoning.effort"], "high");
    let (conversation, mut h) = h.close();
    assert_eq!(
        (
            conversation.meta().model.as_deref(),
            conversation.meta().effort.as_deref()
        ),
        (Some("anthropic/claude-haiku-4.5"), Some("high"))
    );
    drop(conversation);
    // A model the list does not have, chosen: refused, naming the choice.
    let (child, window) = spawn(
        &h.state,
        &h.id,
        None,
        None,
        false,
        h.mock.runtime(),
        &h.stderr,
    );
    h.child = child;
    h.window = window;
    assert!(matches!(h.next(), Up::Hello { .. }));
    // The log as the reopened process replays it, its turn first.
    assert_eq!(h.turn().1, "replied");
    h.setup(Client::default());
    h.down(&Down::Choose {
        model: Some("example/not-listed".into()),
        effort: None,
    });
    h.say("again");
    let (_, outcome, _) = h.turn();
    assert!(
        outcome.contains("example/not-listed is not in the provider's models list")
            && outcome
                .contains("set the conversation's model (Conversation \u{2192} Model\u{2026})"),
        "{outcome}"
    );
    assert_eq!(h.mock.requests().len(), 2, "nothing more was sent");
}

#[test]
fn a_rate_limited_request_is_retried_at_most_three_times() {
    let limited = || Reply::status(429, "error-429.json").with_header("retry-after", "0");
    let mut h = Harness::new(
        "429",
        Role::Orchestrator,
        vec![limited(), limited(), limited(), limited()],
    );
    h.setup(Client::default());
    h.say("hello");
    let (events, outcome, retry) = h.turn();
    assert_eq!(h.mock.requests().len(), 4, "the first and three retries");
    assert!(
        outcome.contains("still rate-limited after 3 retries"),
        "{outcome}"
    );
    assert!(!retry);
    assert_eq!(usage(&events), [(0, Basis::Nothing); 4]);
    assert!(h.spent.iter().all(|(_, amount)| *amount == 0));
    // Once more: rate-limited, then answered.
    h.mock
        .then(vec![limited(), Reply::sse("stream-sonnet.sse")]);
    let mut h = h;
    h.say("again");
    let (_, outcome, _) = h.turn();
    assert_eq!(outcome, "replied");
    assert_eq!(h.mock.requests().len(), 6);
}

#[test]
fn a_502_is_shown_with_a_retry_and_asked_again_only_when_told() {
    let mut h = Harness::new(
        "502",
        Role::Orchestrator,
        vec![Reply::status(502, "error-502.json")],
    );
    h.setup(Client::default());
    h.say("hello");
    let (events, outcome, retry) = h.turn();
    assert!(
        outcome.starts_with("error 502: Provider returned error"),
        "{outcome}"
    );
    assert!(outcome.contains("Overloaded"), "{outcome}");
    assert!(retry);
    // It may have been billed: its whole reservation counts.
    let reserved = h.reserved[0].1;
    assert_eq!(usage(&events), [(reserved, Basis::Reserved)]);
    assert_eq!(h.spent, [(h.reserved[0].0, reserved)]);
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(h.mock.requests().len(), 1, "never retried by itself");
    h.mock.then(vec![Reply::sse("stream-sonnet.sse")]);
    h.down(&Down::Retry);
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied");
    assert_eq!(
        kinds(&events)[0],
        "started",
        "a new turn of the same message"
    );
    let requests = h.mock.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].body, requests[1].body, "the same request again");
    // A retry of a turn that did not fail asks nothing.
    h.down(&Down::Retry);
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(h.mock.requests().len(), 2);
}

#[test]
fn a_401_or_a_402_stops_the_turn() {
    for (status, said) in [(401, "No auth credentials"), (402, "requires more credits")] {
        let mut h = Harness::new(
            "stop",
            Role::Orchestrator,
            vec![Reply::status(status, &format!("error-{status}.json"))],
        );
        h.setup(Client::default());
        h.say("hello");
        let (events, outcome, retry) = h.turn();
        assert!(
            outcome.starts_with(&format!("error {status}: ")),
            "{outcome}"
        );
        assert!(outcome.contains(said), "{outcome}");
        assert!(!retry);
        assert_eq!(usage(&events), [(0, Basis::Nothing)]);
        assert_eq!(h.mock.requests().len(), 1);
    }
}

/// A 200 that answers a stream with one JSON body, as some providers do
/// for an error, is read as a counted reply would be.
#[test]
fn an_error_inside_a_200_is_caught_and_charged_as_reported() {
    let mut h = Harness::new(
        "200",
        Role::Orchestrator,
        vec![Reply::ok("error-in-200.json")],
    );
    h.setup(Client::default());
    h.say("hello");
    let (events, outcome, retry) = h.turn();
    assert!(
        outcome.starts_with("error 502: Upstream error from Google"),
        "{outcome}"
    );
    assert!(retry);
    assert_eq!(usage(&events), [(4_700_000_000, Basis::Reported)]);
    assert!(!kinds(&events).contains(&"assistant"));
}

/// An error object inside a 200 stream ends the turn as one inside a
/// counted 200 does, charged as reported and offered again; the text
/// before it is logged, marked incomplete, and never sent back.
#[test]
fn an_error_mid_stream_ends_the_turn_charged_with_its_text_kept() {
    let mut h = Harness::new(
        "midstream",
        Role::Orchestrator,
        vec![Reply::sse("stream-error.sse")],
    );
    h.setup(Client::default());
    h.say("And after a base advances?");
    let (events, outcome, retry) = h.turn();
    assert_eq!(
        outcome,
        "error 502: Upstream error from Google: the stream was reset"
    );
    assert!(retry);
    assert_eq!(usage(&events), [(4_800_000_000, Basis::Reported)]);
    let reply = replies(&events);
    assert_eq!(reply.len(), 1);
    assert_eq!(reply[0].1.as_deref(), Some("Rebase onto"));
    assert!(reply[0].3, "marked incomplete");
    assert_eq!(h.streamed(reply[0].0).1, "Rebase onto");
    // Asked again: the same request, the incomplete reply not in it.
    h.mock.then(vec![Reply::sse("stream-sonnet.sse")]);
    h.down(&Down::Retry);
    let (_, outcome, _) = h.turn();
    assert_eq!(outcome, "replied");
    let requests = h.mock.requests();
    assert_eq!(requests[0].body, requests[1].body);
}

/// A stream that breaks off, by a transport error or by ending before its
/// reply finished, ends the turn with what it brought logged as
/// incomplete, its whole reservation charged, and `C-r` offered.
#[test]
fn a_stream_broken_mid_reply_keeps_its_text_and_charges_the_reservation() {
    for (tail, said) in [
        (
            Tail::Error("transport: connection reset by peer".into()),
            "error: connection reset by peer",
        ),
        (
            Tail::End,
            "error: the stream ended before the reply finished",
        ),
    ] {
        let mut h = Harness::new(
            "broken",
            Role::Orchestrator,
            vec![Reply::sse("stream-sonnet.sse")
                .cut_after("paths you name")
                .tail(tail)],
        );
        h.setup(Client::default());
        h.say("What is a sparse checkout?");
        let (events, outcome, retry) = h.turn();
        assert_eq!(outcome, said);
        assert!(retry);
        let reserved = h.reserved[0].1;
        assert_eq!(usage(&events), [(reserved, Basis::Reserved)]);
        assert_eq!(h.spent, [(h.reserved[0].0, reserved)]);
        let reply = replies(&events);
        assert_eq!(
            reply[0].1.as_deref(),
            Some("A sparse checkout keeps only the paths you name")
        );
        assert!(reply[0].3, "marked incomplete");
        // Its reasoning details are kept as far as they came.
        assert!(reply[0].2.as_deref().unwrap().contains("Answer plainly."));
    }
}

/// A stream's end decides with its finish: one ended after its finish
/// without `[DONE]` is whole, and `[DONE]` before any finish is a stream
/// cut short, kept incomplete and offered again.
#[test]
fn a_reply_is_whole_by_its_finish_not_by_done() {
    let mut h = Harness::new(
        "finish",
        Role::Orchestrator,
        vec![Reply::sse("stream-sonnet.sse").cut_after("0.00435")],
    );
    h.setup(Client::default());
    h.say("What is a sparse checkout?");
    let (events, outcome, retry) = h.turn();
    assert_eq!(outcome, "replied");
    assert!(!retry);
    assert_eq!(usage(&events), [(4_350_000_000, Basis::Reported)]);
    assert!(!replies(&events)[0].3, "whole");
    h.mock.then(vec![Reply::sse("stream-sonnet.sse")
        .cut_after("paths you name")
        .done()]);
    h.say("And a shallow clone?");
    let (events, outcome, retry) = h.turn();
    assert_eq!(outcome, "error: the stream ended before the reply finished");
    assert!(retry);
    let reserved = h.reserved[1].1;
    assert_eq!(usage(&events), [(reserved, Basis::Reserved)]);
    assert!(replies(&events)[0].3, "marked incomplete");
}

/// An interrupt during a rate limit's wait ends the wait and the turn,
/// with the request not asked again until told.
#[test]
fn an_interrupt_ends_a_rate_limits_wait() {
    let limited = Reply::status(429, "error-429.json").with_header("retry-after", "30");
    let mut h = Harness::new("429-interrupt", Role::Orchestrator, vec![limited]);
    h.setup(Client::default());
    h.say("hello");
    // Its reservation is released just before the wait.
    h.until_spent(1);
    let began = std::time::Instant::now();
    h.down(&Down::Interrupt);
    let (events, outcome, retry) = h.turn();
    assert!(began.elapsed() < std::time::Duration::from_secs(10));
    assert!(
        outcome.ends_with("; interrupted before asking again"),
        "{outcome}"
    );
    assert!(retry);
    assert_eq!(usage(&events), [(0, Basis::Nothing)]);
    assert_eq!(h.mock.requests().len(), 1);
}

/// Escape closes the stream's connection and ends the turn, saying that
/// a provider may go on generating and billing; what came is kept, the
/// reservation charged, and the turn may be asked again.
#[test]
fn an_interrupt_closes_the_stream_and_ends_the_turn() {
    let mut h = Harness::new(
        "interrupt",
        Role::Orchestrator,
        vec![Reply::sse("stream-sonnet.sse")
            .cut_after("paths you name")
            .tail(Tail::Open)],
    );
    h.setup(Client::default());
    h.say("What is a sparse checkout?");
    h.until_text();
    h.down(&Down::Interrupt);
    let (events, outcome, retry) = h.turn();
    assert_eq!(outcome, td_agent::client::INTERRUPTED);
    assert!(outcome.contains("not every provider stops generating, or billing"));
    assert!(retry);
    h.mock.wait_closed(1);
    let reserved = h.reserved[0].1;
    assert_eq!(usage(&events), [(reserved, Basis::Reserved)]);
    let reply = replies(&events);
    assert!(reply[0].3, "marked incomplete");
    assert_eq!(
        reply[0].1.as_deref(),
        Some("A sparse checkout keeps only the paths you name")
    );
    // An interrupt between turns is nothing; a retry asks again whole.
    h.down(&Down::Interrupt);
    h.mock.then(vec![Reply::sse("stream-sonnet.sse")]);
    h.down(&Down::Retry);
    let (_, outcome, _) = h.turn();
    assert_eq!(outcome, "replied");
    let requests = h.mock.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].body, requests[1].body);
}

#[test]
fn spending_limits_refuse_a_request_before_it_is_sent() {
    // The turn's limit, past which the reservation would carry it.
    let mut h = Harness::new("turn-limit", Role::Orchestrator, Vec::new());
    h.setup(Client {
        limits: Limits {
            turn: Some(ONE / 1000),
            ..Limits::default()
        },
        ..Client::default()
    });
    h.say("hello");
    let (events, outcome, _) = h.turn();
    assert!(
        outcome.starts_with("max_cost_per_turn is $0.0010"),
        "{outcome}"
    );
    assert!(!kinds(&events).contains(&"request"));
    assert!(h.mock.requests().is_empty());
    assert!(h.reserved.is_empty(), "refused before asking the window");
    // The day's, which the window holds.
    let mut h = Harness::new("day-limit", Role::Orchestrator, Vec::new());
    h.day = Day::Refuse("max_cost_per_day is $25.0000: $25.0000 is spent".into());
    h.setup(Client::default());
    h.say("hello");
    let (events, outcome, _) = h.turn();
    assert!(outcome.starts_with("max_cost_per_day"), "{outcome}");
    assert!(!kinds(&events).contains(&"request"));
    assert!(h.mock.requests().is_empty());
    assert_eq!(h.reserved.len(), 1);
    // A model with no price, while a limit is set.
    let mut h = Harness::new("unpriced", Role::Orchestrator, Vec::new());
    h.setup(Client {
        model: "openrouter/auto".into(),
        ..Client::default()
    });
    h.say("hello");
    let (_, outcome, _) = h.turn();
    assert!(
        outcome.contains("no price is known for openrouter/auto"),
        "{outcome}"
    );
    assert!(outcome.contains("while a cost limit is set"), "{outcome}");
    assert!(h.mock.requests().is_empty());
    // With every limit `none` it is sent, reserving nothing.
    let mut h = Harness::new(
        "unlimited",
        Role::Orchestrator,
        vec![Reply::sse("stream-sonnet.sse")],
    );
    h.setup(Client {
        model: "openrouter/auto".into(),
        limits: Limits {
            turn: None,
            conversation: None,
            day: None,
        },
        ..Client::default()
    });
    h.say("hello");
    let (_, outcome, _) = h.turn();
    assert_eq!(outcome, "replied");
    assert_eq!(
        h.reserved
            .iter()
            .map(|(_, amount)| *amount)
            .collect::<Vec<_>>(),
        [0]
    );
    // A model the list does not have is refused by name.
    let mut h = Harness::new("unknown", Role::Conversation, Vec::new());
    h.setup(Client {
        model: "nobody/nothing".into(),
        ..Client::default()
    });
    h.say("hello");
    let (_, outcome, _) = h.turn();
    assert!(
        outcome.starts_with("nobody/nothing is not in the provider's models list; set `model`"),
        "{outcome}"
    );
}

#[test]
fn a_model_without_reasoning_is_not_asked_for_it() {
    let mut h = Harness::new(
        "plain",
        Role::Orchestrator,
        vec![Reply::sse("stream-sonnet.sse")],
    );
    h.setup(Client {
        model: "meta-llama/llama-4-small".into(),
        allow_data_collection: true,
        ..Client::default()
    });
    h.say("hello");
    h.turn();
    let body = flat(&h.mock.requests()[0].text());
    assert!(!body.contains_key("reasoning.effort"));
    assert!(!body.contains_key("cache_control.type"));
    assert_eq!(body["provider.data_collection"], "allow");
    assert_eq!(body["max_tokens"], "16384");
}

/// A request logged as started when its process is killed is recorded as
/// interrupted by the next, its reservation spent, and never sent again.
#[test]
fn a_request_in_flight_when_its_process_dies_is_never_resent() {
    let mut h = Harness::new("killed", Role::Orchestrator, vec![Reply::Hang]);
    h.setup(Client::default());
    h.say("hello");
    // The request is reserved, logged, synced and sent; then a crash.
    loop {
        if let Up::Reserve { id, amount } = h.next() {
            h.reserved.push((id, amount));
            h.down(&Down::Reservation { id, refusal: None });
            break;
        }
    }
    h.mock.wait_for(1);
    h.child.kill().unwrap();
    h.child.wait().unwrap();
    let (child, window) = spawn(
        &h.state,
        &h.id,
        None,
        None,
        false,
        h.mock.runtime(),
        &h.stderr,
    );
    h.child = child;
    h.window = window;
    let Up::Hello { interrupted, .. } = h.next() else {
        panic!("no hello")
    };
    // The turn and its request, each interrupted once.
    assert_eq!(interrupted.len(), 2, "{interrupted:?}");
    h.setup(Client::default());
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(h.mock.requests().len(), 1, "never resent");
    let (conversation, h) = h.close();
    assert_eq!(
        td_agent::accounts::spent(conversation.events()),
        h.reserved[0].1,
        "its reservation counts as spent"
    );
}

/// A grant that comes after its request gave up is released at once, and
/// a retry with no failed turn is refused, so the window holds neither.
#[test]
fn a_late_grant_is_released_and_a_retry_of_nothing_refused() {
    let mut h = Harness::new("late", Role::Orchestrator, Vec::new());
    h.setup(Client::default());
    h.down(&Down::Reservation {
        id: 77,
        refusal: None,
    });
    assert!(matches!(h.next(), Up::Spent { id: 77, amount: 0 }));
    // A refused one held nothing: nothing to release.
    h.down(&Down::Reservation {
        id: 78,
        refusal: Some("no".into()),
    });
    h.down(&Down::Retry);
    let Up::Refused { delivery, reason } = h.next() else {
        panic!("not refused")
    };
    assert_eq!(delivery, "");
    assert_eq!(reason, "there is no failed turn to ask again");
    assert!(h.mock.requests().is_empty());
}

#[test]
fn without_a_key_a_turn_says_why_and_sends_nothing() {
    let mut h = Harness::new("nokey", Role::Orchestrator, Vec::new());
    h.down(&Down::Setup {
        key: Err("no API key: write one line to /x/openrouter.key, mode 0600".into()),
        client: Box::default(),
    });
    h.say("hello");
    let (_, outcome, _) = h.turn();
    assert!(outcome.starts_with("no API key"), "{outcome}");
    assert!(h.mock.requests().is_empty());
}

// --- the workspace tools (DESIGN.md §7, §11, §12) ------------------------

/// The approvals of `events`: the call each decided, its outcome, by whom
/// and why.
fn approvals(events: &[Event]) -> Vec<(u64, String, String, Option<String>)> {
    events
        .iter()
        .filter_map(|e| match &e.kind {
            Kind::Approval {
                call,
                outcome,
                by,
                reason,
                ..
            } => Some((*call, outcome.clone(), by.clone(), reason.clone())),
            _ => None,
        })
        .collect()
}

/// The `ToolCall` record of call `id` in `events`.
fn call_record(events: &[Event], id: &str) -> u64 {
    events
        .iter()
        .find_map(|e| match &e.kind {
            Kind::ToolCall { id: of, .. } if of == id => Some(e.seq),
            _ => None,
        })
        .unwrap()
}

/// The human's rules come with the window's policy (DESIGN.md §11): an
/// allow for this workspace runs a command with no card, logged as the
/// rule's; another workspace's allow and a deny for every workspace
/// apply as theirs do, the latest policy taken.
#[test]
fn the_humans_rules_allow_a_command_here_and_deny_one_everywhere() {
    let mut h = Harness::new_in(
        "yours",
        Role::Conversation,
        Some("scratch"),
        false,
        vec![
            Reply::sse("stream-tool-workspace.sse"),
            Reply::sse("stream-sonnet.sse"),
            Reply::ok("title.json"),
            Reply::sse("stream-tool-workspace.sse"),
            Reply::sse("stream-sonnet.sse"),
            Reply::sse("stream-tool-workspace.sse"),
            Reply::sse("stream-sonnet.sse"),
        ],
    );
    h.setup(Client::default());
    let here = format!("conversation {}", h.id.as_str());
    // Another workspace's allow is not this one's.
    h.down(&Down::Policy {
        version: 1,
        mode: td_agent::config::Mode::Auto,
        rules: Ok("[workspace td-1]\nallow shell rm\n".into()),
    });
    h.down(&Down::Policy {
        version: 2,
        mode: td_agent::config::Mode::Auto,
        rules: Ok(format!(
            "[workspace td-1]\nallow shell rm\n[{here}]\nallow shell rm -f\n"
        )),
    });
    h.say("Tidy the notes.");
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied", "{}", h.said());
    let ran = results(&events);
    assert_eq!(ran.len(), 2, "{ran:?}");
    // It ran with no card, and so came to the jail, which is not here.
    assert!(ran[1].1.starts_with("error: the jail: "), "{ran:?}");
    let shell = call_record(&events, "toolu_shell_01");
    assert_eq!(
        approvals(&events),
        [(
            shell,
            "allow".to_string(),
            "rule".to_string(),
            Some(
                "the rule `allow shell rm -f` of your rules for this workspace allows it"
                    .to_string()
            )
        )]
    );
    // A deny for every workspace wins over the allow here.
    h.down(&Down::Policy {
        version: 3,
        mode: td_agent::config::Mode::Auto,
        rules: Ok(format!(
            "[{here}]\nallow shell rm -f\n[everywhere]\ndeny shell rm\n"
        )),
    });
    h.say("Again.");
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied", "{}", h.said());
    let results = results(&events);
    let denied = "the rule `deny shell rm` of your rules for every workspace denies it";
    assert!(
        results
            .iter()
            .any(|r| r.1.starts_with(&format!("error: not run: {denied}"))),
        "{results:?}"
    );
    // Rules the window could not read: the read still runs, and the
    // command, though the file allowed it, waits for the person.
    h.down(&Down::Policy {
        version: 4,
        mode: td_agent::config::Mode::Auto,
        rules: Err("rules: line 2: names no tool".into()),
    });
    h.say("Once more.");
    let (call, title, details) = h.until_ask();
    assert_eq!(title, "Run a command");
    assert_eq!(
        details[0],
        "Asked because your rules could not be read: rules: line 2: names no tool."
    );
    assert_eq!(call, call_record(&h.heard, "toolu_shell_01"));
    h.down(&Down::Decision {
        call,
        allow: false,
        always: None,
    });
    let (_, outcome, _) = h.turn();
    assert_eq!(outcome, "replied", "{}", h.said());
}

/// A workspace's mode (DESIGN.md §11): in `auto`, the configuration's, a
/// command inside the jail runs with no card, its approval the mode's;
/// the human's rules putting this workspace in `ask` bring the card back,
/// and putting it in `auto` while the card waits takes the card back
/// and runs the command.
#[test]
fn auto_mode_runs_a_command_inside_the_jail_and_ask_mode_asks() {
    use td_agent::config::Mode;
    let mut h = Harness::new_in(
        "auto-mode",
        Role::Conversation,
        Some("scratch"),
        false,
        vec![
            Reply::sse("stream-tool-workspace.sse"),
            Reply::sse("stream-sonnet.sse"),
            Reply::ok("title.json"),
            Reply::sse("stream-tool-workspace.sse"),
            Reply::sse("stream-sonnet.sse"),
            Reply::sse("stream-tool-workspace.sse"),
            Reply::sse("stream-sonnet.sse"),
        ],
    );
    h.setup(Client::default());
    let here = format!("conversation {}", h.id.as_str());
    let auto = "the workspace is in auto mode, where a call inside the jail runs";
    h.down(&Down::Policy {
        version: 1,
        rules: Ok(String::new()),
        mode: Mode::Auto,
    });
    h.say("Tidy the notes.");
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied", "{}", h.said());
    let ran = results(&events);
    assert!(
        ran.iter().any(|r| r.1.starts_with("error: the jail: ")),
        "{ran:?}"
    );
    let shell = call_record(&events, "toolu_shell_01");
    assert_eq!(
        approvals(&events),
        [(shell, "allow".into(), "mode".into(), Some(auto.into()))]
    );

    h.down(&Down::Policy {
        version: 2,
        rules: Ok(format!("[{here}]\nmode ask\n")),
        mode: Mode::Auto,
    });
    h.say("Again.");
    let (call, title, _) = h.until_ask();
    assert_eq!(title, "Run a command");
    h.down(&Down::Policy {
        version: 3,
        rules: Ok(format!("[{here}]\nmode ask\nmode auto\n")),
        mode: Mode::Ask,
    });
    assert_eq!(h.until_withdrawn(), call);
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied", "{}", h.said());
    assert_eq!(
        approvals(&events),
        [(call, "allow".into(), "mode".into(), Some(auto.into()))]
    );

    // An ask of the human's holds in `auto` mode.
    h.down(&Down::Policy {
        version: 4,
        rules: Ok(format!("[{here}]\nmode auto\nask shell\n")),
        mode: Mode::Ask,
    });
    h.say("Once more.");
    let (call, _, details) = h.until_ask();
    assert!(details[0].starts_with("Asked because "), "{details:?}");
    h.down(&Down::Decision {
        call,
        allow: false,
        always: None,
    });
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied", "{}", h.said());
    assert_eq!(approvals(&events)[0].2, "human");
}

/// A card offers its "always" answers (DESIGN.md §11), and the human's
/// rules changing while it waits decide it: a deny for this workspace
/// takes it back and refuses the call, another workspace's leaving it be;
/// an allow takes it back and runs the call. A decision that remembered
/// says what in its approval.
#[test]
fn a_card_offers_always_and_a_rule_written_while_it_waits_decides_it() {
    let mut h = Harness::new_in(
        "always",
        Role::Conversation,
        Some("scratch"),
        false,
        vec![
            Reply::sse("stream-tool-workspace.sse"),
            Reply::sse("stream-sonnet.sse"),
            Reply::ok("title.json"),
            Reply::sse("stream-tool-workspace.sse"),
            Reply::sse("stream-sonnet.sse"),
            Reply::sse("stream-tool-workspace.sse"),
            Reply::sse("stream-sonnet.sse"),
        ],
    );
    h.setup(Client::default());
    let here = format!("conversation {}", h.id.as_str());
    let policy = |version: u64, rules: &str| Down::Policy {
        version,
        rules: Ok(rules.into()),
        mode: td_agent::config::Mode::Ask,
    };
    h.say("Tidy the notes.");
    let (call, _, _) = h.until_ask();
    assert_eq!(
        h.always,
        Some(td_agent::rules::Offer::Rules(td_agent::rules::Always {
            allow: true,
            bodies: vec!["shell rm".into()],
        }))
    );
    h.down(&policy(1, "[workspace td-1]\ndeny shell rm\n"));
    h.down(&policy(2, &format!("[{here}]\ndeny shell rm\n")));
    assert_eq!(h.until_withdrawn(), call);
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied", "{}", h.said());
    let denied = "the rule `deny shell rm` of your rules for this workspace denies it";
    let ran = results(&events);
    assert!(
        ran.iter()
            .any(|r| r.1.starts_with(&format!("error: not run: {denied}"))),
        "{ran:?}"
    );
    assert_eq!(
        approvals(&events),
        [(call, "deny".into(), "rule".into(), Some(denied.into()))]
    );

    h.down(&policy(3, ""));
    h.say("Again.");
    let (call, _, _) = h.until_ask();
    h.down(&policy(4, &format!("[{here}]\nallow shell rm -f\n")));
    assert_eq!(h.until_withdrawn(), call);
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied", "{}", h.said());
    let ran = results(&events);
    assert!(
        ran.iter().any(|r| r.1.starts_with("error: the jail: ")),
        "{ran:?}"
    );
    let allowed = "the rule `allow shell rm -f` of your rules for this workspace allows it";
    assert_eq!(
        approvals(&events),
        [(call, "allow".into(), "rule".into(), Some(allowed.into()))]
    );

    h.down(&policy(5, ""));
    h.say("Once more.");
    let (call, _, _) = h.until_ask();
    let remembered = "`deny shell rm` in your rules for this workspace";
    h.down(&Down::Decision {
        call,
        allow: false,
        always: Some(remembered.into()),
    });
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied", "{}", h.said());
    assert_eq!(
        approvals(&events),
        [(
            call,
            "deny".into(),
            "human".into(),
            Some(format!("always: {remembered}"))
        )]
    );
}

/// A third call in a row to one tool with the same arguments goes to the
/// person, though a read asks nothing; the first two ran, and the
/// approval says why it was asked. Other arguments start a run anew,
/// though their members come in another order (DESIGN.md §11).
#[test]
fn a_third_identical_call_in_a_row_goes_to_the_person() {
    let mut h = Harness::new_in(
        "repeat",
        Role::Conversation,
        Some("scratch"),
        false,
        vec![
            Reply::sse("stream-tool-repeat.sse"),
            Reply::sse("stream-sonnet.sse"),
            Reply::ok("title.json"),
        ],
    );
    h.setup(Client::default());
    // Repetition goes to the person in `auto` mode too.
    h.down(&Down::Policy {
        version: 1,
        rules: Ok(String::new()),
        mode: td_agent::config::Mode::Auto,
    });
    h.say("Read the notes.");
    let (call, title, details) = h.until_ask();
    assert_eq!(title, "Read a file");
    assert_eq!(
        details,
        [
            "Asked because the model made this same call, to the same tool with the same arguments, three times in a row, which may be a loop.",
            "notes.txt"
        ]
    );
    let read = results(&h.heard);
    assert_eq!(read.len(), 2, "{read:?}");
    assert_eq!(call, call_record(&h.heard, "toolu_read_03"));
    h.down(&Down::Decision {
        call,
        allow: false,
        always: None,
    });
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied");
    let results = results(&events);
    assert_eq!(results.len(), 5);
    assert!(
        results[2]
            .1
            .starts_with("error: not run: the person refused this call"),
        "{results:?}"
    );
    // The fourth and fifth ran without a card: two in a run is no loop.
    assert!(
        results[3].1.starts_with("error: the jail: ")
            && results[4].1.starts_with("error: the jail: "),
        "{results:?}"
    );
    assert_eq!(
        approvals(&events),
        [(
            call,
            "deny".to_string(),
            "human".to_string(),
            Some("repeated".to_string())
        )]
    );
}

/// A workspace conversation's request names its workspace and carries
/// its tools; a read runs without asking, and a command waits for the
/// person, whose refusal is the call's answer. Without the launch's
/// td-jail neither can run, which each says, so the gate needs no jail:
/// the jail's own tests run the calls (tests/jail.rs).
#[test]
fn a_workspace_command_waits_for_the_person_and_a_refusal_is_its_answer() {
    let mut h = Harness::new_in(
        "ask",
        Role::Conversation,
        Some("scratch"),
        false,
        vec![
            Reply::sse("stream-tool-workspace.sse"),
            Reply::sse("stream-sonnet.sse"),
            Reply::ok("title.json"),
        ],
    );
    h.setup(Client::default());
    h.say("Tidy the notes.");
    let (call, title, details) = h.until_ask();
    assert_eq!(title, "Run a command");
    assert_eq!(
        details,
        [
            "In the workspace, for at most 120 s, in a jail of its own:",
            "rm -f notes.txt"
        ]
    );
    // The read went first and asked nothing.
    let read = results(&h.heard);
    assert_eq!(read.len(), 1, "{read:?}");
    assert_eq!(read[0].0, "toolu_read_01");
    assert!(
        read[0].2 && read[0].1.starts_with("error: the jail: "),
        "{read:?}"
    );
    assert_eq!(call, call_record(&h.heard, "toolu_shell_01"));
    // A decision for another call is not this one's.
    h.down(&Down::Decision {
        call: call + 1000,
        allow: true,
        always: None,
    });
    h.down(&Down::Decision {
        call,
        allow: false,
        always: None,
    });
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied");
    let results = results(&events);
    assert_eq!(results.len(), 2);
    assert_eq!(results[1].0, "toolu_shell_01");
    assert!(results[1].2);
    assert!(
        results[1]
            .1
            .starts_with("error: not run: the person refused this call. That is their answer"),
        "{results:?}"
    );
    assert_eq!(
        approvals(&events),
        [(call, "deny".to_string(), "human".to_string(), None)]
    );
    // The turn's two requests, then the title's.
    let mut requests = h.mock.requests();
    assert_eq!(requests.len(), 3);
    requests.truncate(2);
    let first = flat(&requests[0].text());
    let tools: Vec<&str> = first
        .iter()
        .filter(|(k, _)| k.starts_with("tools.") && k.ends_with(".function.name"))
        .map(|(_, v)| v.as_str())
        .collect();
    for tool in [
        "read_file",
        "write_file",
        "edit_file",
        "glob",
        "grep",
        "sed",
        "shell",
    ] {
        assert!(tools.contains(&tool), "{tools:?}");
    }
    let system = &first["messages.0.content"];
    let scratch = h
        .state
        .root()
        .join("jail")
        .join(h.id.as_str())
        .join("scratch");
    let scratch = std::fs::canonicalize(scratch).unwrap();
    assert!(
        system.contains(&format!(
            "- Workspace: {}, a scratch directory td-agent made",
            scratch.display()
        )),
        "{system}"
    );
    assert!(system.contains("- Shared directories: none."), "{system}");
    assert!(!system.contains("no working directory"), "{system}");
    let second = flat(&requests[1].text());
    assert!(second["messages.4.content"].starts_with("error: not run: the person refused"));
    let (conversation, _) = h.close();
    for (n, request) in requests.iter().enumerate() {
        rebuilt(&conversation, n, &request.text());
    }
}

/// A conversation made from a template binds the template's own shared
/// directories, not the top-level ones, and none once the template is
/// no longer configured: removing or renaming one never widens it.
#[test]
fn a_template_workspace_binds_its_own_shared_directories() {
    use td_agent::config::TemplateShared;
    use td_agent::workspace::Shared;
    for (name, said) in [
        ("notes", "- Shared directories: /own (read-write)."),
        ("gone", "- Shared directories: none."),
    ] {
        let mut h = Harness::new_in(
            &format!("template-{name}"),
            Role::Conversation,
            Some(&format!("template:{name}")),
            false,
            vec![Reply::sse("stream-sonnet.sse"), Reply::ok("title.json")],
        );
        h.setup(Client {
            shared: vec![Shared {
                path: "/top".into(),
                write: false,
            }],
            template_shared: vec![TemplateShared {
                name: "notes".into(),
                shared: Some(vec![Shared {
                    path: "/own".into(),
                    write: true,
                }]),
            }],
            ..Client::default()
        });
        h.say("hello");
        let (_, outcome, _) = h.turn();
        assert_eq!(outcome, "replied");
        let requests = h.mock.requests();
        let first = flat(&requests[0].text());
        let system = &first["messages.0.content"];
        assert!(system.contains(said), "{name}: {system}");
        assert!(!system.contains("/top"), "{name}: {system}");
        assert!(
            system.contains("a scratch directory td-agent made"),
            "{system}"
        );
        h.close();
    }
}

/// An allowed command runs, here to say the jail is missing; one asked
/// while the turn is interrupted is withdrawn and not run.
#[test]
fn an_allowed_command_runs_and_an_interrupt_withdraws_the_card() {
    let mut h = Harness::new_in(
        "allow",
        Role::Conversation,
        Some("scratch"),
        false,
        vec![
            Reply::sse("stream-tool-workspace.sse"),
            Reply::sse("stream-sonnet.sse"),
            Reply::ok("title.json"),
        ],
    );
    h.setup(Client::default());
    h.say("Tidy the notes.");
    let (call, _, _) = h.until_ask();
    h.down(&Down::Decision {
        call,
        allow: true,
        always: None,
    });
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied");
    let first = results(&events);
    assert!(first[1].1.starts_with("error: the jail: "), "{first:?}");
    assert_eq!(
        approvals(&events),
        [(call, "allow".to_string(), "human".to_string(), None)]
    );

    h.mock.then(vec![Reply::sse("stream-tool-workspace.sse")]);
    h.say("Again.");
    let (call, _, _) = h.until_ask();
    h.down(&Down::Interrupt);
    assert_eq!(h.until_withdrawn(), call);
    let (events, outcome, retry) = h.turn();
    assert!(
        outcome.starts_with("interrupted between tool calls"),
        "{outcome}"
    );
    assert!(retry);
    let results = results(&events);
    assert!(
        results[1]
            .1
            .starts_with("error: not run: the turn was interrupted, or the window closed"),
        "{results:?}"
    );
    assert_eq!(
        approvals(&events),
        [(
            call,
            "withdrawn".to_string(),
            "td-agent".to_string(),
            Some("the turn was interrupted".to_string())
        )]
    );
    assert_eq!(h.mock.requests().len(), 4);
}

/// A card asked when the window closes is withdrawn, and its call
/// answered as not run.
#[test]
fn a_card_asked_when_the_window_closes_is_withdrawn() {
    let mut h = Harness::new_in(
        "gone",
        Role::Conversation,
        Some("scratch"),
        false,
        vec![Reply::sse("stream-tool-workspace.sse")],
    );
    h.setup(Client::default());
    h.say("Tidy the notes.");
    let (call, _, _) = h.until_ask();
    let (conversation, _) = h.close();
    let events = conversation.events();
    assert_eq!(
        approvals(events),
        [(
            call,
            "withdrawn".to_string(),
            "td-agent".to_string(),
            Some("the window closed".to_string())
        )]
    );
    let results = results(events);
    assert!(
        results[1]
            .1
            .starts_with("error: not run: the turn was interrupted, or the window closed"),
        "{results:?}"
    );
}

/// A conversation whose repository workspace went with its archive,
/// unarchived, asks for no store and refuses its workspace tools without
/// asking, the turn going on (DESIGN.md §7). A store asked for, or a
/// card, would hold the turn past the harness's wait.
#[test]
fn a_workspace_gone_with_its_archive_refuses_its_tools() {
    let base = std::env::temp_dir().join(format!(
        "td-agent-model-removed-{}-{}",
        std::process::id(),
        td_agent::store::random_hex(4).unwrap()
    ));
    let template = td_agent::config::Template {
        name: "td".into(),
        repos: vec![td_agent::config::Repo {
            remote: "https://example.org/a/td".into(),
            base: "main".into(),
            branch: "agent".into(),
            sparse: None,
        }],
        shared: None,
    };
    let admitted = [td_agent::git::Admission::parse("example.org").unwrap()];
    let made = td_agent::workspace::repositories(
        &template,
        &Id::random().unwrap(),
        &base.join("data"),
        &base.join("trees"),
        &admitted,
        0,
    )
    .unwrap();
    let argument = td_agent::workspace::Workspace::Repositories(made).argument();
    let mut h = Harness::new_in(
        "removed",
        Role::Conversation,
        Some(argument.to_str().unwrap()),
        false,
        vec![
            Reply::sse("stream-tool-workspace.sse"),
            Reply::sse("stream-sonnet.sse"),
            Reply::ok("title.json"),
        ],
    );
    while !matches!(h.next(), Up::Fetch { .. }) {}
    let (conversation, mut h) = h.close();
    drop(conversation);
    // Archived with its workspace by the window, then brought back.
    h.state
        .set_archived(&h.id, true, true, Duration::from_secs(3))
        .unwrap();
    h.state
        .set_archived(&h.id, false, false, Duration::from_secs(3))
        .unwrap();
    h.reopen();
    h.setup(Client::default());
    h.say("Tidy the notes.");
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied", "{}", h.said());
    let results = results(&events);
    assert_eq!(results.len(), 2, "{results:?}");
    assert!(
        results.iter().all(|(_, content, error)| *error
            && content.contains("the workspace went with this conversation's archive")),
        "{results:?}"
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// A repository's rules come before the table (DESIGN.md §11): an ask
/// rule puts a read before the person, saying which rule asks, and a
/// deny refuses a command with no card, the rule its answer and its
/// approval logged by the rule.
#[test]
fn a_repositorys_rules_ask_for_a_read_and_refuse_a_command() {
    let base = std::env::temp_dir().join(format!(
        "td-agent-model-rules-{}-{}",
        std::process::id(),
        td_agent::store::random_hex(4).unwrap()
    ));
    let template = td_agent::config::Template {
        name: "td".into(),
        repos: vec![td_agent::config::Repo {
            remote: "https://example.org/a/td".into(),
            base: "main".into(),
            branch: "agent".into(),
            sparse: None,
        }],
        shared: None,
    };
    let admitted = [td_agent::git::Admission::parse("example.org").unwrap()];
    let made = td_agent::workspace::repositories(
        &template,
        &Id::random().unwrap(),
        &base.join("data"),
        &base.join("trees"),
        &admitted,
        0,
    )
    .unwrap();
    let checkout = made.entries[0].checkout.display().to_string();
    let argument = td_agent::workspace::Workspace::Repositories(made).argument();
    let mut h = Harness::new_in(
        "rules",
        Role::Conversation,
        Some(argument.to_str().unwrap()),
        false,
        vec![
            Reply::sse("stream-tool-workspace.sse"),
            Reply::sse("stream-sonnet.sse"),
            Reply::ok("title.json"),
        ],
    );
    let rule = |line: &str| td_agent::rules::Rule::parse(line).unwrap();
    let fetched = Down::Fetched {
        remote: "https://example.org/a/td".into(),
        result: Ok(td_agent::protocol::Fetched {
            identity: td_agent::repo::Identity::default(),
            ids: vec!["a".repeat(40)],
            instructions: vec![td_agent::repo::Instructions::Absent],
            rules: vec![td_agent::rules::Read::Found(vec![
                rule("ask read_file"),
                rule("deny shell rm"),
            ])],
        }),
    };
    while !matches!(h.next(), Up::Fetch { .. }) {}
    h.setup(Client::default());
    // A repository's ask and deny hold in `auto` mode.
    h.down(&Down::Policy {
        version: 1,
        rules: Ok(String::new()),
        mode: td_agent::config::Mode::Auto,
    });
    h.down(&fetched);
    while !matches!(h.next(), Up::Prepared { .. }) {}
    h.say("Tidy the notes.");
    let (call, title, details) = h.until_ask();
    assert_eq!(title, "Read a file");
    assert_eq!(
        details[0],
        format!("Asked because the rule `ask read_file` of the repository at {checkout} asks.")
    );
    assert_eq!(call, call_record(&h.heard, "toolu_read_01"));
    // A rule asked, so an allow would not spare the next card: only the
    // denies are offered.
    assert_eq!(
        h.always,
        Some(td_agent::rules::Offer::Rules(td_agent::rules::Always {
            allow: false,
            bodies: vec!["read_file".into()],
        }))
    );
    h.down(&Down::Decision {
        call,
        allow: true,
        always: None,
    });
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied", "{}", h.said());
    let denied = format!("the rule `deny shell rm` of the repository at {checkout} denies it");
    let results = results(&events);
    assert_eq!(results.len(), 2, "{results:?}");
    assert_eq!(results[1].0, "toolu_shell_01");
    assert!(results[1].2);
    assert!(
        results[1].1.starts_with(&format!(
            "error: not run: {denied}. That is the workspace's answer"
        )),
        "{results:?}"
    );
    let shell = call_record(&events, "toolu_shell_01");
    assert_eq!(
        approvals(&events),
        [
            (
                call,
                "allow".to_string(),
                "human".to_string(),
                Some(format!(
                    "the rule `ask read_file` of the repository at {checkout} asks"
                ))
            ),
            (shell, "deny".to_string(), "rule".to_string(), Some(denied)),
        ]
    );
    drop(h);
    let _ = std::fs::remove_dir_all(&base);
}

/// A repository's checkout runs on a thread of its own, so a turn goes
/// on without it; done while the conversation is idle, its news wakes a
/// turn that reads it, though not before the person has written, not
/// while the conversation is paused, and not when it says what the last
/// news of its remote said (DESIGN.md §3, §7). With no jail here the
/// checkout fails, which is news as well, as is a store refused before
/// any checkout starts; each process's news differs from the last but
/// where it is meant to be the same.
#[test]
fn a_checkout_done_while_idle_wakes_its_conversation() {
    let base = std::env::temp_dir().join(format!(
        "td-agent-model-woken-{}-{}",
        std::process::id(),
        td_agent::store::random_hex(4).unwrap()
    ));
    let template = td_agent::config::Template {
        name: "td".into(),
        repos: vec![td_agent::config::Repo {
            remote: "https://example.org/a/td".into(),
            base: "main".into(),
            branch: "agent".into(),
            sparse: None,
        }],
        shared: None,
    };
    let admitted = [td_agent::git::Admission::parse("example.org").unwrap()];
    let made = td_agent::workspace::repositories(
        &template,
        &Id::random().unwrap(),
        &base.join("data"),
        &base.join("trees"),
        &admitted,
        0,
    )
    .unwrap();
    let argument = td_agent::workspace::Workspace::Repositories(made).argument();
    let mut h = Harness::new_in(
        "woken",
        Role::Conversation,
        Some(argument.to_str().unwrap()),
        false,
        vec![
            Reply::sse("stream-sonnet.sse"),
            Reply::ok("title.json"),
            Reply::sse("stream-sonnet.sse"),
            Reply::sse("stream-sonnet.sse"),
        ],
    );
    let remote = "https://example.org/a/td".to_string();
    let fetched = Down::Fetched {
        remote: remote.clone(),
        result: Ok(td_agent::protocol::Fetched {
            rules: vec![td_agent::rules::Read::Absent],
            identity: td_agent::repo::Identity::default(),
            ids: vec!["a".repeat(40)],
            instructions: vec![td_agent::repo::Instructions::Absent],
        }),
    };
    let refused = Down::Fetched {
        remote: remote.clone(),
        result: Err("the remote is not admitted".into()),
    };
    let asked = |h: &mut Harness| while !matches!(h.next(), Up::Fetch { .. }) {};
    let done = |h: &mut Harness| while !matches!(h.next(), Up::Prepared { .. }) {};
    // No turn was started of the last notification logged.
    let slept = |conversation: &Conversation| {
        let events = conversation.events();
        let last = events
            .iter()
            .rposition(|e| matches!(e.kind, Kind::Notification { .. }))
            .unwrap();
        assert!(
            !events[last..]
                .iter()
                .any(|e| matches!(e.kind, Kind::Started { .. })),
            "{events:?}"
        );
    };
    // Before the person has written, the news wakes nothing.
    asked(&mut h);
    h.setup(Client::default());
    h.down(&fetched);
    done(&mut h);
    h.say("Tidy the notes.");
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied", "{}", h.said());
    let turns = events
        .iter()
        .filter(|e| matches!(e.kind, Kind::Started { .. }))
        .count();
    assert_eq!(turns, 1, "{events:?}");
    let (conversation, mut h) = h.close();
    let logged = conversation.events();
    let news = logged
        .iter()
        .find(|e| matches!(e.kind, Kind::Notification { .. }))
        .map(|e| e.seq)
        .unwrap();
    assert!(
        !logged
            .iter()
            .any(|e| matches!(e.kind, Kind::Started { of, .. } if of == news)),
        "{logged:?}"
    );
    drop(conversation);
    // Started again, idle: a refused store's news, then the checkout's,
    // each other than the last, wakes a turn of its own, which reads it.
    h.reopen();
    asked(&mut h);
    h.setup(Client::default());
    let woke = |h: &mut Harness, what: &str| {
        // The turn is announced before the window is told the process is
        // done with the store, so it keeps the process for the turn.
        loop {
            let up = h.next();
            if h.hear(&up) {
                continue;
            }
            match up {
                Up::Prepared { .. } => panic!("done with before the turn: {:?}", h.heard),
                Up::Event(event) => {
                    let started = matches!(event.kind, Kind::Started { .. });
                    h.heard.push(event);
                    if started {
                        break;
                    }
                }
                _ => {}
            }
        }
        let (events, outcome, _) = h.turn();
        assert_eq!(outcome, "replied", "{}", h.said());
        let news = events
            .iter()
            .rfind(|e| matches!(&e.kind, Kind::Notification { text } if text.contains(what)))
            .unwrap_or_else(|| panic!("{events:?}"));
        assert!(
            events.iter().any(|e| e.kind
                == Kind::Started {
                    effect: td_agent::store::Effect::Turn,
                    of: news.seq
                }),
            "{events:?}"
        );
    };
    h.down(&refused);
    woke(&mut h, "the remote is not admitted");
    h.down(&fetched);
    woke(&mut h, "could not be prepared");
    let requests = h.mock.requests();
    assert_eq!(requests.len(), 4, "the person's turn, its title, two woken");
    assert!(
        requests[3]
            .text()
            .contains("td-agent's news of this workspace"),
        "{}",
        requests[3].text()
    );
    let (conversation, mut h) = h.close();
    drop(conversation);
    // Paused, the news is logged and wakes nothing.
    h.reopen();
    asked(&mut h);
    h.down(&Down::Pause { paused: true });
    h.setup(Client::default());
    h.down(&refused);
    done(&mut h);
    h.down(&fetched);
    done(&mut h);
    let (conversation, mut h) = h.close();
    slept(&conversation);
    drop(conversation);
    // Resumed, the same failure as the last news wakes nothing.
    h.reopen();
    asked(&mut h);
    h.down(&Down::Pause { paused: false });
    h.setup(Client::default());
    h.down(&fetched);
    done(&mut h);
    let (conversation, h) = h.close();
    slept(&conversation);
    assert_eq!(h.mock.requests().len(), 4, "nothing more was sent");
    let _ = std::fs::remove_dir_all(&base);
}

/// A call into a worktree that could not be prepared is refused with
/// its state before any card is asked: here a command with no directory,
/// which runs in the first worktree (DESIGN.md §7).
#[test]
fn a_call_into_a_worktree_not_prepared_is_told_so_without_a_card() {
    let base = std::env::temp_dir().join(format!(
        "td-agent-model-unready-{}-{}",
        std::process::id(),
        td_agent::store::random_hex(4).unwrap()
    ));
    let template = td_agent::config::Template {
        name: "td".into(),
        repos: vec![td_agent::config::Repo {
            remote: "https://example.org/a/td".into(),
            base: "main".into(),
            branch: "agent".into(),
            sparse: None,
        }],
        shared: None,
    };
    let admitted = [td_agent::git::Admission::parse("example.org").unwrap()];
    let made = td_agent::workspace::repositories(
        &template,
        &Id::random().unwrap(),
        &base.join("data"),
        &base.join("trees"),
        &admitted,
        0,
    )
    .unwrap();
    let checkout = made.entries[0].checkout.display().to_string();
    let argument = td_agent::workspace::Workspace::Repositories(made).argument();
    let mut h = Harness::new_in(
        "unready",
        Role::Conversation,
        Some(argument.to_str().unwrap()),
        false,
        vec![
            Reply::sse("stream-tool-workspace.sse"),
            Reply::sse("stream-sonnet.sse"),
            Reply::ok("title.json"),
        ],
    );
    while !matches!(h.next(), Up::Fetch { .. }) {}
    h.setup(Client::default());
    h.down(&Down::Fetched {
        remote: "https://example.org/a/td".into(),
        result: Err("the remote is not admitted".into()),
    });
    h.say("Tidy the notes.");
    // Any card would stop the turn here, unanswered.
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied", "{}", h.said());
    let results = results(&events);
    assert_eq!(results.len(), 2, "{results:?}");
    let shell = &results[1];
    assert!(
        shell.2
            && shell.1 == format!(
                "error: {checkout} could not be prepared; td-agent tries again when this conversation is next opened"
            ),
        "{results:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e.kind, Kind::Approval { .. })),
        "{events:?}"
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// A step of a repository workspace that may change files is
/// snapshotted before and after it, one that cannot is not, and a
/// snapshot that cannot be taken, here with no jail, is said once
/// (DESIGN.md §12).
#[test]
fn a_step_that_may_change_files_is_snapshotted_or_says_why_not_once() {
    let base = std::env::temp_dir().join(format!(
        "td-agent-model-snap-{}-{}",
        std::process::id(),
        td_agent::store::random_hex(4).unwrap()
    ));
    let template = td_agent::config::Template {
        name: "td".into(),
        repos: vec![td_agent::config::Repo {
            remote: "https://example.org/a/td".into(),
            base: "main".into(),
            branch: "agent".into(),
            sparse: None,
        }],
        shared: None,
    };
    let admitted = [td_agent::git::Admission::parse("example.org").unwrap()];
    let made = td_agent::workspace::repositories(
        &template,
        &Id::random().unwrap(),
        &base.join("data"),
        &base.join("trees"),
        &admitted,
        0,
    )
    .unwrap();
    let repository = made.entries[0].repository.clone();
    let argument = td_agent::workspace::Workspace::Repositories(made).argument();
    let h = Harness::new_in(
        "snap",
        Role::Conversation,
        Some(argument.to_str().unwrap()),
        false,
        vec![
            Reply::sse("stream-tool-todo.sse"),
            Reply::sse("stream-sonnet.sse"),
            Reply::ok("title.json"),
            Reply::sse("stream-tool-workspace.sse"),
            Reply::sse("stream-sonnet.sse"),
            Reply::sse("stream-tool-workspace-stop.sse"),
            Reply::sse("stream-sonnet.sse"),
        ],
    );
    // Prepared, as if checked out before.
    let (mut conversation, mut h) = h.close();
    conversation.set_prepared(&repository).unwrap();
    drop(conversation);
    h.reopen();
    h.setup(Client::default());
    let unsnapped = |events: &[Event]| {
        events
            .iter()
            .filter(|e| matches!(&e.kind, Kind::Notice { text } if text.starts_with("a step's snapshot was not recorded")))
            .count()
    };
    // A step that writes only its todo list changes no worktree.
    h.say("Plan the tidying.");
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied", "{}", h.said());
    assert_eq!(unsnapped(&events), 0, "{events:?}");
    // One that runs a command is snapshotted first: with no jail, said.
    let refuse = |h: &mut Harness| {
        let (call, _, _) = h.until_ask();
        h.down(&Down::Decision {
            call,
            allow: false,
            always: None,
        });
        h.turn()
    };
    h.say("Tidy the notes.");
    let (events, outcome, _) = refuse(&mut h);
    assert_eq!(outcome, "replied", "{}", h.said());
    let mut heard = std::mem::take(&mut h.heard);
    heard.extend(events);
    assert_eq!(unsnapped(&heard), 1, "{heard:?}");
    // Said before the step's first call.
    let said = heard
        .iter()
        .position(
            |e| matches!(&e.kind, Kind::Notice { text } if text.starts_with("a step's snapshot")),
        )
        .unwrap();
    let first = heard
        .iter()
        .position(|e| matches!(e.kind, Kind::ToolCall { .. }))
        .unwrap();
    assert!(said < first, "{heard:?}");
    // Once only.
    h.say("Tidy them again.");
    let (events, outcome, _) = refuse(&mut h);
    assert_eq!(outcome, "replied", "{}", h.said());
    let mut heard = std::mem::take(&mut h.heard);
    heard.extend(events);
    assert_eq!(unsnapped(&heard), 0, "{heard:?}");
    // An undo is of the latest step only, and why not is said; a reopened
    // process replays its log first, up to `after`.
    let notice = |h: &mut Harness, after: u64| loop {
        if let Up::Event(Event {
            seq,
            kind: Kind::Notice { text },
            ..
        }) = h.next()
        {
            if seq > after {
                break text;
            }
        }
    };
    h.down(&Down::Restore {
        step: 1,
        undo: true,
    });
    assert_eq!(
        notice(&mut h, 0),
        "the step could not be undone: it is not the latest step to undo"
    );
    let (mut conversation, mut h) = h.close();
    assert!(!conversation
        .events()
        .iter()
        .any(|e| matches!(e.kind, Kind::Snapshot { .. })),);
    // Steps as if snapshotted: one in a worktree not this workspace's,
    // then one in its own, which with no jail is not restored.
    let snapped = |checkout: &str| Kind::Snapshot {
        reply: 1,
        background: Vec::new(),
        worktrees: vec![td_agent::store::Snapped {
            checkout: checkout.into(),
            before: "a".repeat(40),
            after: "b".repeat(40),
            changed: vec!["notes".into()],
            more: 0,
        }],
    };
    let elsewhere = conversation.append(snapped("/elsewhere")).unwrap().seq;
    let checkout = conversation
        .meta()
        .workspace
        .clone()
        .map(|workspace| match workspace {
            td_agent::workspace::Workspace::Repositories(made) => {
                made.entries[0].checkout.display().to_string()
            }
            other => panic!("{other:?}"),
        });
    let own = conversation
        .append(snapped(&checkout.unwrap()))
        .unwrap()
        .seq;
    conversation.sync().unwrap();
    let after = own;
    drop(conversation);
    h.reopen();
    h.setup(Client::default());
    h.down(&Down::Restore {
        step: elsewhere,
        undo: true,
    });
    assert_eq!(
        notice(&mut h, after),
        "the step could not be undone: it is not the latest step to undo"
    );
    h.down(&Down::Restore {
        step: own,
        undo: false,
    });
    assert_eq!(
        notice(&mut h, after),
        "the step could not be redone: it is not the latest step to redo"
    );
    h.down(&Down::Restore {
        step: own,
        undo: true,
    });
    // Past the step's checks, git or the jail is what is missing here.
    let refused = notice(&mut h, after);
    assert!(
        refused.starts_with("the step could not be undone: ")
            && !refused.contains("latest")
            && !refused.contains("not ready"),
        "{refused}"
    );
    let (mut conversation, mut h) = h.close();
    // Undone as if it had been, the one before is next, and not ready.
    let after = conversation
        .append(Kind::Restore {
            step: own,
            undo: true,
        })
        .unwrap()
        .seq;
    conversation.sync().unwrap();
    drop(conversation);
    h.reopen();
    h.setup(Client::default());
    h.down(&Down::Restore {
        step: elsewhere,
        undo: true,
    });
    assert_eq!(
        notice(&mut h, after),
        "the step could not be undone: /elsewhere is not ready"
    );
    let (conversation, _h) = h.close();
    let restored = conversation
        .events()
        .iter()
        .filter(|e| matches!(e.kind, Kind::Restore { .. }))
        .count();
    assert_eq!(restored, 1, "only the one appended here");
    let _ = std::fs::remove_dir_all(&base);
}

/// A command the person allows runs in the conversation's jail, in its
/// scratch workspace, and a search runs there without asking.
#[test]
#[ignore = "needs user namespaces, TD_AGENT_JAIL and TD_AGENT_TXT"]
fn an_allowed_command_runs_in_the_workspace_jail() {
    let mut h = Harness::new_in(
        "live",
        Role::Conversation,
        Some("scratch"),
        true,
        vec![
            Reply::sse("stream-tool-workspace-live.sse"),
            Reply::sse("stream-sonnet.sse"),
            Reply::ok("title.json"),
        ],
    );
    h.setup(Client::default());
    h.say("Make the notes.");
    let (call, _, details) = h.until_ask();
    assert_eq!(details[1], "printf made > notes.txt && cat notes.txt");
    h.down(&Down::Decision {
        call,
        allow: true,
        always: None,
    });
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied", "{}", h.said());
    let results = results(&events);
    let scratch = std::fs::canonicalize(
        h.state
            .root()
            .join("jail")
            .join(h.id.as_str())
            .join("scratch"),
    )
    .unwrap();
    assert_eq!(results[0].0, "toolu_shell_02");
    assert!(
        !results[0].2 && results[0].1.contains("made"),
        "{results:?}"
    );
    assert_eq!(results[1].0, "toolu_glob_01");
    assert!(
        results[1]
            .1
            .contains(&scratch.join("notes.txt").display().to_string()),
        "{results:?}"
    );
    assert_eq!(
        std::fs::read_to_string(scratch.join("notes.txt")).unwrap(),
        "made"
    );
    // The command's output is kept in the log beside its result.
    assert!(events.iter().any(|e| matches!(
        &e.kind,
        Kind::ToolResult { kept: Some(kept), .. } if kept.contains("made")
    )));
}

/// A command that stops its own tool host cannot hold the conversation:
/// past the call's time and the grace for it, the jail is torn down and
/// the call answered.
#[test]
#[ignore = "needs user namespaces, TD_AGENT_JAIL and TD_AGENT_TXT"]
fn a_tool_host_that_does_not_answer_is_torn_down_at_the_deadline() {
    let mut h = Harness::new_in(
        "stop",
        Role::Conversation,
        Some("scratch"),
        true,
        vec![
            Reply::sse("stream-tool-workspace-stop.sse"),
            Reply::sse("stream-sonnet.sse"),
            Reply::ok("title.json"),
        ],
    );
    h.setup(Client::default());
    h.say("Stop.");
    let (call, _, _) = h.until_ask();
    let asked = Instant::now();
    h.down(&Down::Decision {
        call,
        allow: true,
        always: None,
    });
    // The harness waits at most TIMEOUT for each message, longer than the
    // call's 0.1 s and the 15 s grace together.
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied", "{}", h.said());
    let results = results(&events);
    assert!(
        results[0]
            .1
            .starts_with("error: the tool host did not end this call by its deadline"),
        "{results:?}"
    );
    assert!(asked.elapsed() < Duration::from_secs(25));
}

/// The person kills a background process from the window: it ends
/// killed, and its end wakes nothing (DESIGN.md §12).
#[test]
#[ignore = "needs user namespaces, TD_AGENT_JAIL and TD_AGENT_TXT"]
fn a_background_process_the_person_kills_ends_and_wakes_nothing() {
    let mut h = Harness::new_in(
        "bgkill",
        Role::Conversation,
        Some("scratch"),
        true,
        vec![
            Reply::sse("stream-tool-background.sse"),
            Reply::sse("stream-sonnet.sse"),
            Reply::ok("title.json"),
        ],
    );
    h.setup(Client {
        max_background: 1,
        ..Client::default()
    });
    h.say("Build in the background.");
    let (call, _, _) = h.until_ask();
    h.down(&Down::Decision {
        call,
        allow: true,
        always: None,
    });
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied", "{}", h.said());
    assert!(results(&events)[0].1.starts_with("started p1 "));
    h.down(&Down::Kill { number: 1 });
    assert_eq!(until_ended(&mut h, 1), "killed");
    // A kill of one that has ended is nothing.
    h.down(&Down::Kill { number: 1 });
    let (conversation, _h) = h.close();
    let events = conversation.events();
    let ended = events
        .iter()
        .find(|e| matches!(e.kind, Kind::Ended { number: 1, .. }))
        .unwrap();
    assert!(
        !events
            .iter()
            .any(|e| e.seq > ended.seq && matches!(e.kind, Kind::Started { .. })),
        "{events:?}"
    );
}

/// The end of background process `number` this process logs, waiting
/// for it.
fn until_ended(h: &mut Harness, number: u64) -> String {
    loop {
        let up = h.next();
        if h.hear(&up) {
            continue;
        }
        if let Up::Event(Event {
            kind: Kind::Ended { number: n, how, .. },
            ..
        }) = up
        {
            if n == number {
                return how;
            }
        }
    }
}

/// A background process's output is all kept, the newest
/// `background_output_bytes` of it, and read by offset; a wait returns
/// when it ends with the tail of it (DESIGN.md §12).
#[test]
#[ignore = "needs user namespaces, TD_AGENT_JAIL and TD_AGENT_TXT"]
fn a_background_commands_output_is_kept_and_read_by_offset() {
    let mut h = Harness::new_in(
        "bgout",
        Role::Conversation,
        Some("scratch"),
        true,
        vec![
            Reply::sse("stream-tool-background-output.sse"),
            Reply::sse("stream-sonnet.sse"),
            Reply::ok("title.json"),
        ],
    );
    h.setup(Client {
        background_output_bytes: 64 * 1024,
        ..Client::default()
    });
    h.say("Run them.");
    for _ in 0..2 {
        let (call, _, _) = h.until_ask();
        h.down(&Down::Decision {
            call,
            allow: true,
            always: None,
        });
    }
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied", "{}", h.said());
    let said = results(&events);
    assert_eq!(
        said[2].1,
        "[p1 exit status 0; the last 16 of 16 bytes it wrote]\nline1\nline2\ndone"
    );
    // Every byte of it reached the store, the oldest dropped past 64 KiB.
    assert_eq!(
        said[3].1,
        format!(
            "[p2 exit status 0; the last 15360 of 300000 bytes it wrote]\n{}",
            "0".repeat(15360)
        )
    );
    assert_eq!(
        said[4].1,
        "[p1 exit status 0; bytes 6 to 11 of 16 written; next from 11]\nline2"
    );
    let dropped = &said[5].1;
    assert!(
        dropped.starts_with("[p2 exit status 0; bytes ")
            && dropped.contains(" of 300000 written; ")
            && dropped.contains(" were dropped, so what is kept begins at ")
            && dropped.ends_with("]\n0000000000")
            && !dropped.contains("before what is kept"),
        "{dropped}"
    );
    assert!(said[6].1.contains("| 16 bytes | printf"), "{}", said[6].1);
    assert!(said[6].1.contains("| 300000 bytes | i=0"), "{}", said[6].1);
    let before = &said[7].1;
    assert!(
        before.contains("; 5 is before what is kept, so this read begins at "),
        "{before}"
    );
    assert!(
        said[8].1.contains("; 400000 is past what was written]"),
        "{}",
        said[8].1
    );
    let outputs = h.state.conversation(&h.id).join(td_agent::output::DIR);
    assert!(outputs.is_dir());
}

/// `process_list` shows the latest processes, each command cut, so its
/// result fits however many ran; `process_kill` of one that ended says
/// how (DESIGN.md §12).
#[test]
fn the_process_list_is_the_latest_and_bounded() {
    let h = Harness::new_in(
        "plist",
        Role::Conversation,
        Some("scratch"),
        false,
        vec![
            Reply::sse("stream-tool-process-list.sse"),
            Reply::sse("stream-sonnet.sse"),
            Reply::ok("title.json"),
        ],
    );
    let (mut conversation, mut h) = h.close();
    let long = format!("{}tail", "\t".repeat(300));
    for number in 1..=52u64 {
        let command = if number == 52 {
            long.clone()
        } else {
            format!("job {number}")
        };
        conversation
            .append(Kind::Process {
                number,
                call: 0,
                command,
            })
            .unwrap();
        conversation
            .append(Kind::Ended {
                number,
                how: "exit status 0".into(),
                tail: None,
                held: None,
            })
            .unwrap();
    }
    conversation.sync().unwrap();
    drop(conversation);
    h.reopen();
    h.setup(Client::default());
    h.say("List them.");
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied", "{}", h.said());
    let said = results(&events);
    let list = &said[0].1;
    assert!(
        list.starts_with("2 earlier not shown\np3 | exit status 0 | started "),
        "{list}"
    );
    assert!(!list.contains("| job 2\n"), "{list}");
    assert!(
        list.contains("| job 51\np52 | exit status 0 | started "),
        "{list}"
    );
    assert!(
        list.ends_with(&format!("{}...", "<U+0009>".repeat(200))),
        "{list}"
    );
    assert_eq!(said[1].1, "error: p52 is not running: exit status 0");
}

/// A background command runs on in its own jail after its call returns,
/// numbered from the log; at most `max_background` run at once; one is
/// listed while it runs, and its end, by itself or killed, is logged;
/// no undo runs meanwhile; and one still running when its conversation's
/// process ends is lost (DESIGN.md §12).
#[test]
#[ignore = "needs user namespaces, TD_AGENT_JAIL and TD_AGENT_TXT"]
fn a_background_command_runs_on_until_it_ends_or_is_killed() {
    let mut h = Harness::new_in(
        "background",
        Role::Conversation,
        Some("scratch"),
        true,
        vec![
            Reply::sse("stream-tool-background.sse"),
            Reply::sse("stream-sonnet.sse"),
            Reply::ok("title.json"),
        ],
    );
    h.setup(Client {
        max_background: 2,
        ..Client::default()
    });
    h.say("Build in the background.");
    for command in ["sleep 60", "sleep 8; echo built; exit 3"] {
        let (call, title, details) = h.until_ask();
        assert_eq!(title, "Run a command in the background");
        assert_eq!(details[1], command);
        h.down(&Down::Decision {
            call,
            allow: true,
            always: None,
        });
    }
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied", "{}", h.said());
    let said = results(&events);
    assert!(said[0].1.starts_with("started p1 "), "{said:?}");
    assert!(said[1].1.starts_with("started p2 "), "{said:?}");
    // The third is refused before any card: two run.
    assert_eq!(
        said[2].1,
        "error: 2 background processes are running (p1, p2), the most at once; wait for one to end or kill one"
    );
    assert!(said[3].1.contains("p1 | running | started "), "{said:?}");
    assert!(said[3].1.contains("| sleep 60"), "{said:?}");
    // Its end, after the turn, wakes the idle conversation with a notice
    // of how it ended and the last of its output.
    h.mock.then(vec![Reply::sse("stream-sonnet.sse")]);
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied", "{}", h.said());
    let (ended, how, tail) = events
        .iter()
        .find_map(|e| match &e.kind {
            Kind::Ended {
                number: 2,
                how,
                tail,
                held: None,
            } => Some((e.seq, how.clone(), tail.clone())),
            _ => None,
        })
        .unwrap();
    assert_eq!(how, "exit status 3");
    assert_eq!(tail.as_deref(), Some("| built\n| "));
    assert!(events.iter().any(|e| matches!(
        e.kind,
        Kind::Started { of, .. } if of == ended
    )));
    let requests = h.mock.requests();
    let woke = String::from_utf8_lossy(&requests.last().unwrap().body).to_string();
    assert!(
        woke.contains(
            "background process p2 (`sleep 8; echo built; exit 3`) ended: exit status 3\\nthe last of its output, made visible, follows; process_output reads it all\\n| built\\n| "
        ),
        "{woke}"
    );
    // While one runs, nothing is restored, whichever step.
    h.down(&Down::Restore {
        step: 1,
        undo: true,
    });
    loop {
        if let Up::Event(Event {
            kind: Kind::Notice { text },
            ..
        }) = h.next()
        {
            assert_eq!(
                text,
                "the step could not be undone: background processes are running (p1); kill them first"
            );
            break;
        }
    }

    h.mock.then(vec![
        Reply::sse("stream-tool-background-kill.sse"),
        Reply::sse("stream-sonnet.sse"),
    ]);
    h.say("Stop it.");
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied", "{}", h.said());
    let said = results(&events);
    assert_eq!(said[0].1, "p1 is being killed");
    assert_eq!(said[1].1, "error: p2 is not running: exit status 3");
    assert_eq!(said[2].1, "error: there is no p9");
    // Logged between the turn's steps, or after it.
    let killed = events
        .iter()
        .find_map(|e| match &e.kind {
            Kind::Ended { number: 1, how, .. } => Some(how.clone()),
            _ => None,
        })
        .unwrap_or_else(|| until_ended(&mut h, 1));
    assert_eq!(killed, "killed");

    // Numbered on from the log; gone with the process that ran it.
    h.mock.then(vec![
        Reply::sse("stream-tool-background.sse"),
        Reply::sse("stream-sonnet.sse"),
    ]);
    h.say("Again.");
    for _ in 0..2 {
        let (call, _, _) = h.until_ask();
        h.down(&Down::Decision {
            call,
            allow: true,
            always: None,
        });
    }
    let (events, _, _) = h.turn();
    assert!(results(&events)[0].1.starts_with("started p3 "));
    // Paused, an end is held; resumed, it starts its turn.
    h.down(&Down::Pause { paused: true });
    let held = loop {
        let up = h.next();
        if h.hear(&up) {
            continue;
        }
        match up {
            Up::Event(Event {
                seq,
                kind: Kind::Ended {
                    number: 4, held, ..
                },
                ..
            }) => break (seq, held),
            Up::Event(Event {
                kind: Kind::Started { .. },
                ..
            }) => panic!("a paused conversation started a turn"),
            _ => {}
        }
    };
    assert_eq!(held.1, Some(Held::Paused));
    h.mock.then(vec![Reply::sse("stream-sonnet.sse")]);
    h.down(&Down::Pause { paused: false });
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied", "{}", h.said());
    assert!(events.iter().any(|e| matches!(
        e.kind,
        Kind::Started { of, .. } if of == held.0
    )));
    let (conversation, mut h) = h.close();
    drop(conversation);
    h.reopen();
    let (conversation, _h) = h.close();
    let ended = conversation.events().iter().find_map(|e| match &e.kind {
        Kind::Ended { number: 3, how, .. } => Some(how.clone()),
        _ => None,
    });
    assert_eq!(
        ended.as_deref(),
        Some(td_agent::store::PROCESS_LOST),
        "{:?}",
        conversation.events()
    );
}

// --- the conversation tools (DESIGN.md §3, §5, §12) ---------------------

/// The tool results of `events`: call id, content and whether an error.
fn results(events: &[Event]) -> Vec<(String, String, bool)> {
    events
        .iter()
        .filter_map(|e| match &e.kind {
            Kind::ToolResult {
                id, content, error, ..
            } => Some((id.clone(), content.clone(), *error)),
            _ => None,
        })
        .collect()
}

/// The body request `n` sent equals the one its log rebuilds.
fn rebuilt(conversation: &Conversation, n: usize, sent: &str) {
    let at = conversation
        .events()
        .iter()
        .enumerate()
        .filter(|(_, e)| {
            matches!(
                e.kind,
                Kind::Request {
                    purpose: Purpose::Turn,
                    ..
                }
            )
        })
        .nth(n)
        .unwrap()
        .0;
    assert_eq!(
        td_agent::client::body(conversation.events(), at, conversation.prefix_file()).unwrap(),
        sent
    );
}

const TODO_ARGUMENTS: &str = "{\"items\":[{\"content\":\"Read the design\",\"status\":\"in_progress\"},{\"content\":\"Write the tests\",\"status\":\"pending\"}]}";

#[test]
fn a_tool_call_runs_once_and_its_result_goes_back_with_the_next_step() {
    let mut h = Harness::new(
        "tool",
        Role::Orchestrator,
        vec![
            Reply::sse("stream-tool-todo.sse"),
            Reply::sse("stream-sonnet.sse"),
        ],
    );
    h.setup(Client::default());
    h.say("Plan the work.");
    let (events, outcome, retry) = h.turn();
    assert_eq!((outcome.as_str(), retry), ("replied", false));
    assert_eq!(
        kinds(&events),
        [
            "user",
            "started",
            "request",
            "assistant",
            "usage",
            "finished",
            "tool_call",
            "todo",
            "tool_result",
            "request",
            "assistant",
            "usage",
            "finished",
            "finished"
        ]
    );
    // Every step reserves its own cost.
    assert_eq!(h.reserved.len(), 2);
    assert_eq!(h.spent.len(), 2);
    let requests = h.mock.requests();
    assert_eq!(requests.len(), 2);
    let second = flat(&requests[1].text());
    assert_eq!(second["messages.2.role"], "assistant");
    assert_eq!(second["messages.2.content"], "null");
    assert_eq!(second["messages.2.tool_calls.0.id"], "toolu_todo_01");
    assert_eq!(second["messages.2.tool_calls.0.type"], "function");
    assert_eq!(
        second["messages.2.tool_calls.0.function.name"],
        "todo_write"
    );
    assert_eq!(
        second["messages.2.tool_calls.0.function.arguments"],
        TODO_ARGUMENTS
    );
    assert_eq!(second["messages.3.role"], "tool");
    assert_eq!(second["messages.3.tool_call_id"], "toolu_todo_01");
    assert_eq!(
        second["messages.3.content"],
        "The todo list has 2 items, 0 done:\n[>] Read the design\n[ ] Write the tests"
    );
    assert!(!second.contains_key("messages.4.role"));
    let (conversation, _) = h.close();
    for (n, request) in requests.iter().enumerate() {
        rebuilt(&conversation, n, &request.text());
    }
    assert_eq!(
        td_agent::conversation::todo(conversation.events()).len(),
        2,
        "the list as the log holds it"
    );
}

#[test]
fn parallel_calls_run_in_their_order_each_answered_once() {
    let mut h = Harness::new(
        "parallel",
        Role::Orchestrator,
        vec![
            Reply::sse("stream-tool-parallel.sse"),
            Reply::sse("stream-sonnet.sse"),
        ],
    );
    h.setup(Client::default());
    h.say("Plan it, and look for the design.");
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied");
    let results = results(&events);
    let ids: Vec<&str> = results.iter().map(|(id, _, _)| id.as_str()).collect();
    assert_eq!(ids, ["toolu_par_01", "toolu_par_02"]);
    assert!(results.iter().all(|(_, _, error)| !error), "{results:?}");
    // The search found the user's message, which names the design.
    assert!(results[1].1.contains("design"), "{}", results[1].1);
    let calls: Vec<&str> = events
        .iter()
        .filter_map(|e| match &e.kind {
            Kind::ToolCall { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(calls, ["toolu_par_01", "toolu_par_02"]);
    let second = flat(&h.mock.requests()[1].text());
    assert_eq!(
        second["messages.2.content"],
        "I will note the plan and look back."
    );
    assert_eq!(second["messages.2.tool_calls.1.id"], "toolu_par_02");
    assert_eq!(second["messages.3.tool_call_id"], "toolu_par_01");
    assert_eq!(second["messages.4.tool_call_id"], "toolu_par_02");
}

#[test]
fn malformed_arguments_are_answered_with_an_error_and_the_turn_goes_on() {
    let mut h = Harness::new(
        "malformed",
        Role::Orchestrator,
        vec![
            Reply::sse("stream-tool-malformed.sse"),
            Reply::sse("stream-sonnet.sse"),
        ],
    );
    h.setup(Client::default());
    h.say("Plan it.");
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied");
    let results = results(&events);
    assert_eq!(results.len(), 1);
    let (id, content, error) = &results[0];
    assert_eq!(id, "toolu_bad_01");
    assert!(error);
    assert!(content.starts_with("error: "), "{content}");
    assert!(content.contains("JSON"), "{content}");
    assert!(!kinds(&events).contains(&"todo"), "nothing ran");
    assert_eq!(h.mock.requests().len(), 2);
}

#[test]
fn the_step_bound_ends_a_turn_that_keeps_calling_tools() {
    let steps = td_agent::conversation::MAX_STEPS;
    let mut h = Harness::new(
        "steps",
        Role::Orchestrator,
        vec![Reply::sse("stream-tool-todo.sse"); steps + 1],
    );
    h.setup(Client::default());
    h.say("Plan it forever.");
    let (events, outcome, retry) = h.turn();
    assert!(
        outcome.starts_with(&format!("stopped after {steps} steps")),
        "{outcome}"
    );
    assert!(!retry);
    assert_eq!(h.mock.requests().len(), steps);
    assert_eq!(results(&events).len(), steps, "every call answered");
    let last = kinds(&events);
    assert_eq!(last[last.len() - 2], "tool_result");
}

#[test]
fn a_model_without_tools_is_refused_by_name() {
    let mut h = Harness::new("no-tools", Role::Orchestrator, Vec::new());
    h.setup(Client {
        model: "example/no-tools".into(),
        ..Client::default()
    });
    h.say("hello");
    let (events, outcome, _) = h.turn();
    assert!(
        outcome.starts_with("example/no-tools takes no tools"),
        "{outcome}"
    );
    assert!(
        outcome.contains("`model` or the default model"),
        "{outcome}"
    );
    assert!(!kinds(&events).contains(&"request"));
    assert!(h.mock.requests().is_empty());
    // `max_tokens` bounds what a request may cost, so it is sent always,
    // and a model that does not list it is refused the same way.
    let mut h = Harness::new("no-max-tokens", Role::Conversation, Vec::new());
    h.setup(Client {
        model: "example/no-max-tokens".into(),
        ..Client::default()
    });
    h.say("hello");
    let (events, outcome, _) = h.turn();
    assert!(
        outcome.starts_with("example/no-max-tokens takes no max_tokens"),
        "{outcome}"
    );
    assert!(outcome.contains("set `model`"), "{outcome}");
    assert!(!kinds(&events).contains(&"request"));
    assert!(h.mock.requests().is_empty());
    // A title model that does not list it gets no title request.
    let mut h = Harness::new(
        "title-no-max-tokens",
        Role::Conversation,
        vec![Reply::sse("stream-sonnet.sse")],
    );
    h.setup(Client {
        title_model: "example/no-max-tokens".into(),
        ..Client::default()
    });
    h.say("hello");
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied");
    let notice = events.iter().find_map(|e| match &e.kind {
        Kind::Notice { text } if text.starts_with("no title:") => Some(text.clone()),
        _ => None,
    });
    assert!(
        notice.as_deref().is_some_and(|n| n
            .starts_with("no title: example/no-max-tokens takes no max_tokens")
            && n.contains("set `title_model`")),
        "{notice:?}"
    );
    assert!(!kinds(&events).contains(&"title"));
    assert_eq!(h.mock.requests().len(), 1, "no title request");
}

/// A message from another conversation, as the window hands it on.
fn message(from: &Id, text: &str) -> Down {
    Down::Message {
        delivery: td_agent::store::random_hex(16).unwrap(),
        from: from.clone(),
        role: Role::Conversation,
        text: text.into(),
        status: None,
    }
}

/// What the process says until it acknowledges a delivery, reservations
/// answered on the way: the events.
fn until_delivered(h: &mut Harness) -> Vec<Event> {
    let mut events = Vec::new();
    loop {
        let up = h.next();
        if h.hear(&up) {
            continue;
        }
        match up {
            Up::Event(event) => events.push(event),
            Up::Delivered { .. } => return events,
            other => panic!("not delivered: {other:?}"),
        }
    }
}

#[test]
fn the_wake_budget_holds_messages_past_twenty_turns_until_the_human_writes() {
    let budget = td_agent::wake::BUDGET;
    let mut h = Harness::new(
        "wake",
        Role::Conversation,
        vec![Reply::sse("stream-sonnet.sse"); budget],
    );
    h.setup(Client::default());
    let peer = Id::parse(&"c".repeat(32)).unwrap();
    for n in 0..budget {
        h.down(&message(&peer, &format!("wake {n}")));
        let (events, outcome, _) = h.turn();
        assert_eq!(outcome, "replied", "turn {n}");
        assert_eq!(kinds(&events)[..2], ["message", "started"]);
    }
    let body = h.mock.requests()[0].text();
    assert!(
        body.contains(&format!(
            "[a message from conversation {peer}, not from the person]\\nwake 0"
        )),
        "{body}"
    );
    // Spent: held, said once, and no request.
    h.down(&message(&peer, "one more"));
    let events = until_delivered(&mut h);
    assert_eq!(kinds(&events), ["message", "notice"]);
    assert!(matches!(
        events[0].kind,
        Kind::Message {
            held: Some(td_agent::store::Held::Budget),
            ..
        }
    ));
    h.down(&message(&peer, "and another"));
    assert_eq!(kinds(&until_delivered(&mut h)), ["message"]);
    // A report is no wake of its own making: it starts a turn.
    // The report's turn, the human's (the first they began, so titled),
    // and the next message's.
    h.mock.then(vec![
        Reply::sse("stream-sonnet.sse"),
        Reply::sse("stream-sonnet.sse"),
        Reply::ok("title.json"),
        Reply::sse("stream-sonnet.sse"),
    ]);
    h.down(&Down::Message {
        delivery: td_agent::store::random_hex(16).unwrap(),
        from: peer.clone(),
        role: Role::Conversation,
        text: "finished".into(),
        status: Some("done".into()),
    });
    let (_, outcome, _) = h.turn();
    assert_eq!(outcome, "replied");
    assert_eq!(h.mock.requests().len(), budget + 1);
    // The human writes: the budget is renewed, and the held messages are
    // in the next request.
    h.say("Go on.");
    let (_, outcome, _) = h.turn();
    assert_eq!(outcome, "replied");
    assert!(h.mock.requests()[budget + 1].text().contains("and another"));
    h.down(&message(&peer, "after"));
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied");
    assert_eq!(kinds(&events)[..2], ["message", "started"]);
}

#[test]
fn a_paused_conversation_holds_messages_until_it_is_resumed() {
    let mut h = Harness::new(
        "pause",
        Role::Orchestrator,
        vec![Reply::sse("stream-sonnet.sse")],
    );
    h.setup(Client::default());
    h.down(&Down::Pause { paused: true });
    let peer = Id::parse(&"d".repeat(32)).unwrap();
    h.down(&message(&peer, "while paused"));
    let events = until_delivered(&mut h);
    assert_eq!(kinds(&events), ["pause", "message"]);
    assert!(matches!(
        events[1].kind,
        Kind::Message {
            held: Some(td_agent::store::Held::Paused),
            ..
        }
    ));
    assert!(h.mock.requests().is_empty());
    // Resumed: the held message starts its turn.
    h.down(&Down::Pause { paused: false });
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied");
    assert_eq!(kinds(&events)[..2], ["started", "pause"]);
    assert!(h.mock.requests()[0].text().contains("while paused"));
    let (conversation, _) = h.close();
    assert!(!conversation.meta().paused);
}

#[test]
fn a_call_started_and_not_finished_is_answered_as_interrupted_after_a_restart() {
    let mut h = Harness::new(
        "call-restart",
        Role::Orchestrator,
        vec![Reply::sse("stream-sonnet.sse")],
    );
    let _ = h.window.shutdown(std::net::Shutdown::Both);
    wait(&mut h.child);
    // The log as a process killed mid-call leaves it: two calls asked
    // for, the first started.
    {
        let (mut c, _) = Conversation::open(&h.state, &h.id, None, Duration::from_secs(3)).unwrap();
        let call = |id: &str| td_agent::store::Call {
            id: id.into(),
            name: "todo_write".into(),
            arguments: TODO_ARGUMENTS.into(),
        };
        let user = c
            .append(Kind::User {
                delivery: "0".repeat(32),
                text: "Plan it.".into(),
            })
            .unwrap()
            .seq;
        let turn = c
            .append(Kind::Started {
                effect: td_agent::store::Effect::Turn,
                of: user,
            })
            .unwrap()
            .seq;
        let request = c
            .append(Kind::Request {
                turn,
                purpose: Purpose::Turn,
                prefix: 0,
                head: "\"model\":\"x\"".into(),
                bytes: 0,
                reserved: 0,
            })
            .unwrap()
            .seq;
        let reply = c
            .append(Kind::Assistant {
                request,
                content: None,
                reasoning: None,
                details: None,
                finish: "tool_calls".into(),
                incomplete: false,
                calls: vec![call("toolu_a"), call("toolu_b")],
            })
            .unwrap()
            .seq;
        let started = c
            .append(Kind::ToolCall {
                reply,
                id: "toolu_a".into(),
                name: "todo_write".into(),
            })
            .unwrap()
            .seq;
        c.sync().unwrap();
        assert_eq!(started, 5);
    }
    let (child, window) = spawn(
        &h.state,
        &h.id,
        None,
        None,
        false,
        h.mock.runtime(),
        &h.stderr,
    );
    h.child = child;
    h.window = window;
    let Up::Hello { interrupted, .. } = h.next() else {
        panic!("no hello")
    };
    let mut interrupted = interrupted;
    interrupted.sort_unstable();
    assert_eq!(interrupted, [2, 3, 5], "the turn, its request and the call");
    h.setup(Client::default());
    h.say("Go on.");
    let (_, outcome, _) = h.turn();
    assert_eq!(outcome, "replied");
    let body = flat(&h.mock.requests()[0].text());
    assert_eq!(body["messages.3.tool_call_id"], "toolu_a");
    assert_eq!(
        body["messages.3.content"],
        td_agent::store::CALL_INTERRUPTED
    );
    assert_eq!(body["messages.4.tool_call_id"], "toolu_b");
    assert_eq!(body["messages.4.content"], td_agent::store::CALL_NOT_RUN);
    assert!(
        is_sent(&body["messages.5.content"], "Go on."),
        "{}",
        body["messages.5.content"]
    );
    let (conversation, _) = h.close();
    assert!(
        !conversation
            .events()
            .iter()
            .any(|e| matches!(&e.kind, Kind::Todo { .. })),
        "never run again"
    );
}

/// A conversation sends another a message through the window, the human
/// allowing it on a card, and the window queues it, starts the receiver's
/// process, and hands it on: the receiver's turn answers it.
#[test]
fn a_message_between_two_conversations_wakes_the_receiver() {
    use td_agent::post::{Entry, Outbox, Post};
    use td_agent::supervisor::{Opened, Supervisor, Update};

    let root = std::env::temp_dir().join(format!(
        "td-agent-model-post-{}-{}",
        std::process::id(),
        td_agent::store::random_hex(4).unwrap()
    ));
    std::fs::create_dir(&root).unwrap();
    let state = StateDir::at(root.join("state"));
    state.ensure().unwrap();
    let runtime = root.join("run");
    std::fs::create_dir(&runtime).unwrap();
    let mock = MockFetch::start(
        &runtime,
        vec![
            Reply::sse("stream-tool-send.sse"),
            Reply::sse("stream-sonnet.sse"),
        ],
    );
    mock.route(
        &format!(
            "[a message from conversation {}, not from the person]",
            "a".repeat(32)
        ),
        vec![Reply::sse("stream-gemini.sse")],
    );
    mock.route(
        "Write a title for the conversation",
        vec![Reply::ok("title.json")],
    );
    Models::from_provider(&fixture("models.json"))
        .unwrap()
        .save(state.root())
        .unwrap();
    let sender = Id::parse(&"a".repeat(32)).unwrap();
    let receiver = Id::parse(&"b".repeat(32)).unwrap();
    let setup = Down::Setup {
        key: Ok(Secret::new(KEY.into())),
        client: Box::default(),
    };
    let mut supervisor = Supervisor::new(PROGRAM.into(), state.root().to_path_buf(), setup)
        .env("XDG_RUNTIME_DIR", &runtime);
    // The receiver exists, closed; the sender is open.
    assert_eq!(
        supervisor
            .open(receiver.clone(), Some(Role::Conversation))
            .unwrap(),
        Opened::Started
    );
    supervisor
        .open(sender.clone(), Some(Role::Conversation))
        .unwrap();
    let directory = || -> Vec<Entry> {
        state
            .list()
            .0
            .into_iter()
            .map(|meta| Entry {
                id: meta.id,
                state: "idle".into(),
                failed: false,
                archived: meta.archived,
            })
            .collect()
    };
    let (outbox, problems) = Outbox::load(&state);
    assert!(problems.is_empty(), "{problems:?}");
    let mut post = Post::new(outbox);
    supervisor
        .send("Ask the conversation to summarise.".into())
        .unwrap();
    let deadline = Instant::now() + TIMEOUT;
    let mut finished: Vec<Id> = Vec::new();
    let mut cards = Vec::new();
    while finished.len() < 2 {
        assert!(Instant::now() < deadline, "finished: {finished:?}");
        let entries = directory();
        for (id, update) in supervisor.poll() {
            assert!(post.hear(&id, &update, &mut supervisor, &entries).is_none());
            match &update {
                Update::Up(Up::Reserve { id: request, .. }) => supervisor.answer(
                    &id,
                    &Down::Reservation {
                        id: *request,
                        refusal: None,
                    },
                ),
                // The human allows the message.
                Update::Up(Up::Ask { call, title, .. }) => {
                    cards.push((id.clone(), title.clone()));
                    supervisor.answer(
                        &id,
                        &Down::Decision {
                            call: *call,
                            allow: true,
                            always: None,
                        },
                    );
                }
                Update::Up(Up::Event(Event {
                    kind: Kind::Finished { outcome, .. },
                    ..
                })) if outcome == "replied" => finished.push(id.clone()),
                Update::Failed { reason } | Update::Restarting { reason } => {
                    panic!("{id}: {reason}")
                }
                _ => {}
            }
        }
        assert!(post.deliver(&mut supervisor, &directory()).is_empty());
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(finished.contains(&receiver), "{finished:?}");
    assert_eq!(
        cards,
        [(
            sender.clone(),
            "Send a message to another conversation".to_string()
        )]
    );
    assert!(post.outbox().queued().is_empty(), "acknowledged and gone");
    drop(supervisor);
    let log = td_agent::store::read_log(&state, &receiver).unwrap();
    assert_eq!(kinds(&log)[..2], ["message", "started"]);
    let Kind::Message {
        from, role, text, ..
    } = &log[0].kind
    else {
        panic!("not a message")
    };
    assert_eq!(
        (from, *role, text.as_str()),
        (
            &sender,
            Role::Conversation,
            "Please summarise the sparse checkout notes."
        )
    );
    let sent = td_agent::store::read_log(&state, &sender).unwrap();
    let results = results(&sent);
    assert_eq!(results.len(), 1);
    assert!(!results[0].2, "{}", results[0].1);
    assert!(results[0].1.starts_with("queued for conversation"));
    let _ = std::fs::remove_dir_all(&root);
}

/// A pause the human sent while a turn ran is taken before a message that
/// came meanwhile, which it then holds.
#[test]
fn a_pause_sent_mid_turn_holds_a_message_that_came_before_it() {
    let mut h = Harness::new(
        "pause-mid-turn",
        Role::Orchestrator,
        vec![Reply::sse("stream-sonnet.sse")],
    );
    h.setup(Client::default());
    let peer = Id::parse(&"e".repeat(32)).unwrap();
    // The turn cannot end before its reservation is answered, so both
    // frames come while it runs.
    h.say("hello");
    h.down(&message(&peer, "during the turn"));
    h.down(&Down::Pause { paused: true });
    let (_, outcome, _) = h.turn();
    assert_eq!(outcome, "replied");
    let events = until_delivered(&mut h);
    assert_eq!(kinds(&events), ["pause", "message"]);
    assert!(matches!(
        events[1].kind,
        Kind::Message {
            held: Some(td_agent::store::Held::Paused),
            ..
        }
    ));
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(h.mock.requests().len(), 1, "no turn for it");
}

/// A message handed on again, after a restart of the window or the
/// receiver, is acknowledged and not logged twice.
#[test]
fn a_message_delivered_twice_is_logged_once() {
    let mut h = Harness::new(
        "twice",
        Role::Orchestrator,
        vec![Reply::sse("stream-sonnet.sse")],
    );
    h.setup(Client::default());
    let peer = Id::parse(&"f".repeat(32)).unwrap();
    let once = message(&peer, "once");
    h.down(&once);
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied");
    assert_eq!(kinds(&events)[..2], ["message", "started"]);
    h.down(&once);
    assert!(
        until_delivered(&mut h).is_empty(),
        "acknowledged, not logged"
    );
    let (conversation, _) = h.close();
    let messages = conversation
        .events()
        .iter()
        .filter(|e| matches!(e.kind, Kind::Message { .. }))
        .count();
    assert_eq!(messages, 1);
    assert_eq!(conversation.events().len(), events.len());
}

/// A conversation's message to another conversation is a crossing, which
/// the human decides on a card showing it whole; refused, that is the
/// call's result, the turn goes on, and nothing reaches the window.
#[test]
fn a_crossing_asks_the_person_and_a_refusal_is_the_calls_result() {
    let mut h = Harness::new(
        "crossing",
        Role::Conversation,
        vec![
            Reply::sse("stream-tool-send.sse"),
            Reply::sse("stream-sonnet.sse"),
            Reply::ok("title.json"),
        ],
    );
    // The receiver exists, a conversation of its own workspace.
    let other = Id::parse(&"b".repeat(32)).unwrap();
    drop(
        Conversation::open(
            &h.state,
            &other,
            Some(Role::Conversation),
            Duration::from_secs(3),
        )
        .unwrap(),
    );
    h.setup(Client::default());
    h.say("Tell the other conversation.");
    let (call, title, details) = h.until_ask();
    assert_eq!(title, "Send a message to another conversation");
    assert_eq!(
        details,
        [
            format!("Conversation {other}, titled New conversation"),
            "gets this message, labelled as from this conversation, not from you, and starting a turn there:".to_string(),
            "Please summarise the sparse checkout notes.".to_string(),
        ]
    );
    h.down(&Down::Decision {
        call,
        allow: false,
        always: None,
    });
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied");
    let results = results(&events);
    assert_eq!(results.len(), 1);
    let (_, content, error) = &results[0];
    assert!(error);
    assert!(
        content.contains("the person refused this call"),
        "{content}"
    );
    assert!(h.sent.is_empty(), "never handed to the window");
    let approvals: Vec<&Kind> = events
        .iter()
        .chain(&h.heard)
        .map(|e| &e.kind)
        .filter(|k| matches!(k, Kind::Approval { .. }))
        .collect();
    assert!(
        matches!(approvals[..], [Kind::Approval { outcome, by, .. }] if outcome == "deny" && by == "human"),
        "{approvals:?}"
    );
}

/// A standing allow for messages sends three to one conversation since
/// the human last wrote; the fourth, in a turn a peer's message began,
/// goes to a card that says why (DESIGN.md §11).
#[test]
fn a_run_of_messages_on_a_standing_answer_goes_back_to_the_person() {
    let mut script = vec![
        Reply::sse("stream-tool-send.sse"),
        Reply::sse("stream-sonnet.sse"),
        Reply::ok("title.json"),
    ];
    for _ in 0..3 {
        script.push(Reply::sse("stream-tool-send.sse"));
        script.push(Reply::sse("stream-sonnet.sse"));
    }
    let mut h = Harness::new("message-run", Role::Conversation, script);
    let other = Id::parse(&"b".repeat(32)).unwrap();
    drop(
        Conversation::open(
            &h.state,
            &other,
            Some(Role::Conversation),
            Duration::from_secs(3),
        )
        .unwrap(),
    );
    h.setup(Client::default());
    let me = h.id.as_str().to_string();
    h.down(&Down::Policy {
        version: 1,
        mode: td_agent::config::Mode::Auto,
        rules: Ok(format!("[crossings]\nallow message {me} {other}\n")),
    });
    h.say("Tell the other conversation.");
    let (_, outcome, _) = h.turn();
    assert_eq!(outcome, "replied", "{}", h.said());
    for n in 1..=2 {
        h.down(&Down::Message {
            delivery: td_agent::store::random_hex(16).unwrap(),
            from: other.clone(),
            role: Role::Conversation,
            text: format!("reply {n}"),
            status: None,
        });
        let (_, outcome, _) = h.turn();
        assert_eq!(outcome, "replied", "{}", h.said());
    }
    assert_eq!(h.sent.len(), 3, "{:?}", h.sent);
    h.down(&Down::Message {
        delivery: td_agent::store::random_hex(16).unwrap(),
        from: other.clone(),
        role: Role::Conversation,
        text: "reply 3".into(),
        status: None,
    });
    let (call, _, details) = h.until_ask();
    assert!(
        details[0].starts_with("Asked because this conversation has sent that one 3 messages"),
        "{details:?}"
    );
    h.down(&Down::Decision {
        call,
        allow: false,
        always: None,
    });
    let (_, outcome, _) = h.turn();
    assert_eq!(outcome, "replied", "{}", h.said());
    assert_eq!(h.sent.len(), 3);
}

/// The classifier (DESIGN.md §11): in `auto` mode, a crossing no rule
/// decides goes to Jev and the reasoning stage at once, each given the
/// same labelled state; both allowing, it runs, its approval the
/// classifier's with Jev's probabilities; Jev not allowing at the
/// threshold, or the reasoning stage answering in prose, it goes to a
/// card that says why, the classifier's verdict logged before the
/// person's.
#[test]
fn the_classifier_decides_a_crossing_in_auto_mode() {
    let mut h = Harness::new_in(
        "classifier",
        Role::Conversation,
        Some("scratch"),
        false,
        vec![
            Reply::sse("stream-tool-read-other.sse"),
            Reply::sse("stream-sonnet.sse"),
            Reply::ok("title.json"),
            Reply::sse("stream-tool-read-other.sse"),
            Reply::sse("stream-sonnet.sse"),
            Reply::sse("stream-tool-read-other.sse"),
            Reply::sse("stream-sonnet.sse"),
            Reply::sse("stream-tool-read-other.sse"),
            Reply::sse("stream-sonnet.sse"),
        ],
    );
    h.mock.route(
        "typesafe/jev",
        vec![
            Reply::ok("jev-matches.json"),
            Reply::ok("jev-exceeds.json"),
            Reply::ok("jev-matches.json"),
            Reply::status(502, "error-502.json"),
        ],
    );
    h.mock.route(
        "gpt-oss-safeguard",
        vec![
            Reply::ok("classifier-allow.json"),
            Reply::ok("classifier-allow.json"),
            Reply::ok("classifier-prose.json"),
            Reply::ok("classifier-allow.json"),
        ],
    );
    let other = Id::parse(&"b".repeat(32)).unwrap();
    let (mut conversation, _) = Conversation::open(
        &h.state,
        &other,
        Some(Role::Conversation),
        Duration::from_secs(3),
    )
    .unwrap();
    conversation
        .append(Kind::User {
            delivery: "d".repeat(32),
            text: "the plan for the other work".into(),
        })
        .unwrap();
    conversation.sync().unwrap();
    drop(conversation);
    // The shipped threshold, which a person who sets none has.
    h.setup(Client {
        allow_data_collection: true,
        ..Client::default()
    });
    h.down(&Down::Policy {
        version: 1,
        rules: Ok(String::new()),
        mode: td_agent::config::Mode::Auto,
    });
    let asked = "What did the other conversation say?";
    h.say(asked);
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied", "{}", h.said());
    let ran = results(&events);
    assert!(
        ran.iter()
            .any(|r| r.1.contains("the plan for the other work")),
        "{ran:?}"
    );
    let read = call_record(&events, "toolu_read_01");
    let approval = events
        .iter()
        .find_map(|e| match &e.kind {
            Kind::Approval {
                call,
                outcome,
                by,
                probabilities,
                reason,
            } if *call == read => Some((
                outcome.clone(),
                by.clone(),
                probabilities.clone(),
                reason.clone(),
            )),
            _ => None,
        })
        .unwrap();
    let because = "the reasoning stage answers allow: The person asked what the other conversation said, which needs this read.";
    assert_eq!(
        approval,
        (
            "allow".to_string(),
            "classifier".to_string(),
            Some(
                "request matches (matches 0.970, exceeds 0.020, unrelated 0.010); discloses 0.030"
                    .to_string()
            ),
            Some(format!("Jev allows; {because}")),
        )
    );
    // Jev beside the `/v1` root, the reasoning stage a chat completion,
    // both given the same state.
    let requests = h.mock.requests();
    let jev = requests
        .iter()
        .find(|r| r.text().contains("typesafe/jev"))
        .unwrap();
    assert_eq!(jev.url, "https://openrouter.ai/api/alpha/decisions");
    let body = flat(&jev.text());
    assert_eq!(body["questions.request.type"], "choice");
    assert_eq!(body["questions.discloses.type"], "noul");
    assert_eq!(body["state.human.0"], asked);
    assert_eq!(body["state.action.kind"], "read");
    // A read carries the other conversation's content into this one.
    assert_eq!(body["state.action.receiver.workspace"], "scratch");
    assert_eq!(body["state.action.source.conversation"], other.as_str());
    assert_eq!(body["state.action.source.workspace"], "none");
    assert!(
        !body.keys().any(|k| k.starts_with("state.calls")),
        "{body:?}"
    );
    assert_eq!(body["state.policy.mode"], "auto");
    assert!(body.contains_key("state.untrusted.title"), "{body:?}");
    let reasoning = requests
        .iter()
        .find(|r| r.text().contains("gpt-oss-safeguard"))
        .unwrap();
    assert_eq!(
        reasoning.url,
        "https://openrouter.ai/api/v1/chat/completions"
    );
    assert!(!reasoning.stream);
    let body = flat(&reasoning.text());
    assert!(body["messages.0.content"].starts_with("You are td-agent's action classifier."));
    assert_eq!(body["provider.data_collection"], "allow");
    let state = flat(&body["messages.1.content"]);
    assert_eq!(state["human.0"], asked);
    assert_eq!(state["action.source.conversation"], other.as_str());
    // Each is logged, and charged as its usage says.
    let classified: Vec<u64> = events
        .iter()
        .filter_map(|e| match &e.kind {
            Kind::Request {
                purpose: Purpose::Classify,
                ..
            } => Some(e.seq),
            _ => None,
        })
        .collect();
    assert_eq!(classified.len(), 2);
    for request in classified {
        assert!(events.iter().any(|e| matches!(
            &e.kind,
            Kind::Usage { request: r, basis: td_agent::store::Basis::Reported, .. } if *r == request
        )));
    }

    // Jev not allowing at the threshold.
    h.say("And again?");
    let (call, _, details) = h.until_ask();
    assert_eq!(
        details[0],
        format!("Asked because the classifier did not allow it: Jev does not allow at 0.775; {because}.")
    );
    assert_eq!(
        details[1],
        "Jev: request exceeds (matches 0.210, exceeds 0.740, unrelated 0.050); discloses 0.080."
    );
    h.down(&Down::Decision {
        call,
        allow: false,
        always: None,
    });
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied", "{}", h.said());
    let decided: Vec<(String, String)> = approvals(&events)
        .into_iter()
        .filter(|a| a.0 == call)
        .map(|a| (a.1, a.2))
        .collect();
    assert_eq!(
        decided,
        [
            ("ask".to_string(), "classifier".to_string()),
            ("deny".to_string(), "human".to_string())
        ]
    );

    // The reasoning stage answering in prose.
    h.say("Once more.");
    let (call, _, details) = h.until_ask();
    assert_eq!(
        details[0],
        "Asked because the classifier did not allow it: Jev allows; the reasoning stage gave no answer: the reasoning stage did not answer with JSON."
    );
    h.down(&Down::Decision {
        call,
        allow: true,
        always: None,
    });
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied", "{}", h.said());
    assert_eq!(
        approvals(&events)
            .last()
            .map(|a| (a.1.as_str(), a.2.as_str())),
        Some(("allow", "human"))
    );

    // Jev failing: no answer is no allow.
    h.say("One last time.");
    let (call, _, details) = h.until_ask();
    assert!(
        details[0]
            .starts_with("Asked because the classifier did not allow it: Jev gave no answer: "),
        "{details:?}"
    );
    h.down(&Down::Decision {
        call,
        allow: false,
        always: None,
    });
    let (_, outcome, _) = h.turn();
    assert_eq!(outcome, "replied", "{}", h.said());
}

/// A policy that comes while the classifier waits for its reservation
/// is one the crossing is judged by again before it runs: a deny then
/// refuses what both stages allowed, and what they did not, before any
/// card could put it to the person.
#[test]
fn a_deny_taken_while_the_classifier_is_asked_refuses_the_crossing() {
    let mut h = Harness::new_in(
        "classifier-policy",
        Role::Conversation,
        Some("scratch"),
        false,
        vec![
            Reply::sse("stream-tool-read-other.sse"),
            Reply::sse("stream-sonnet.sse"),
            Reply::ok("title.json"),
            Reply::sse("stream-tool-read-other.sse"),
            Reply::sse("stream-sonnet.sse"),
        ],
    );
    h.mock.route(
        "typesafe/jev",
        vec![Reply::ok("jev-matches.json"), Reply::ok("jev-exceeds.json")],
    );
    h.mock.route(
        "gpt-oss-safeguard",
        vec![
            Reply::ok("classifier-allow.json"),
            Reply::ok("classifier-allow.json"),
        ],
    );
    let other = Id::parse(&"b".repeat(32)).unwrap();
    let (mut conversation, _) = Conversation::open(
        &h.state,
        &other,
        Some(Role::Conversation),
        Duration::from_secs(3),
    )
    .unwrap();
    conversation
        .append(Kind::User {
            delivery: "d".repeat(32),
            text: "the plan for the other work".into(),
        })
        .unwrap();
    conversation.sync().unwrap();
    drop(conversation);
    h.setup(Client {
        allow_data_collection: true,
        jev_threshold: 900,
        ..Client::default()
    });
    h.down(&Down::Policy {
        version: 1,
        rules: Ok(String::new()),
        mode: td_agent::config::Mode::Auto,
    });
    let me = h.id.as_str().to_string();
    h.say("What did the other conversation say?");
    // The turn's request's reservation, then the classifier's, with the
    // deny sent before it is granted.
    let mut reserved = 0;
    while reserved < 2 {
        match h.next() {
            Up::Reserve { id, amount } => {
                h.reserved.push((id, amount));
                reserved += 1;
                if reserved == 2 {
                    h.down(&Down::Policy {
                        version: 2,
                        rules: Ok(format!("[crossings]\ndeny read {me} {other}\n")),
                        mode: td_agent::config::Mode::Auto,
                    });
                }
                h.down(&Down::Reservation { id, refusal: None });
            }
            Up::Event(event) => h.heard.push(event),
            up => {
                h.hear(&up);
            }
        }
    }
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied", "{}", h.said());
    let ran = results(&events);
    assert!(
        ran.iter()
            .all(|r| !r.1.contains("the plan for the other work")),
        "{ran:?}"
    );
    let read = call_record(&events, "toolu_read_01");
    let decided: Vec<(u64, String, String)> = approvals(&events)
        .into_iter()
        .map(|a| (a.0, a.1, a.2))
        .collect();
    assert_eq!(decided, [(read, "deny".to_string(), "rule".to_string())]);

    // The deny lifted, then sent again while the classifier is asked,
    // which this time does not allow: no card, the rule refuses it.
    h.down(&Down::Policy {
        version: 3,
        rules: Ok(String::new()),
        mode: td_agent::config::Mode::Auto,
    });
    h.say("And now?");
    let mut reserved = 0;
    while reserved < 2 {
        match h.next() {
            Up::Reserve { id, amount } => {
                h.reserved.push((id, amount));
                reserved += 1;
                if reserved == 2 {
                    h.down(&Down::Policy {
                        version: 4,
                        rules: Ok(format!("[crossings]\ndeny read {me} {other}\n")),
                        mode: td_agent::config::Mode::Auto,
                    });
                }
                h.down(&Down::Reservation { id, refusal: None });
            }
            Up::Event(event) => h.heard.push(event),
            Up::Ask { .. } => panic!("a card for a crossing a rule denies"),
            up => {
                h.hear(&up);
            }
        }
    }
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied", "{}", h.said());
    let read = call_record(&events, "toolu_read_01");
    let decided: Vec<(u64, String, String)> = approvals(&events)
        .into_iter()
        .filter(|a| a.0 == read)
        .map(|a| (a.0, a.1, a.2))
        .collect();
    assert_eq!(decided, [(read, "deny".to_string(), "rule".to_string())]);
    assert_eq!(
        h.mock
            .requests()
            .iter()
            .filter(|r| r.text().contains("typesafe/jev"))
            .count(),
        2
    );
}

/// In `auto` mode a crossing the model makes a third time in a row is
/// the person's, never the classifier's; and a crossing card waiting
/// when its workspace goes to `auto` stays for the person.
#[test]
fn a_repeated_crossing_and_a_waiting_one_stay_with_the_person() {
    let mut h = Harness::new_in(
        "classifier-repeat",
        Role::Conversation,
        Some("scratch"),
        false,
        vec![
            Reply::sse("stream-tool-read-other-repeat.sse"),
            Reply::sse("stream-sonnet.sse"),
            Reply::ok("title.json"),
            Reply::sse("stream-tool-read-other.sse"),
            Reply::sse("stream-sonnet.sse"),
        ],
    );
    h.mock.route(
        "typesafe/jev",
        vec![Reply::ok("jev-matches.json"), Reply::ok("jev-matches.json")],
    );
    h.mock.route(
        "gpt-oss-safeguard",
        vec![
            Reply::ok("classifier-allow.json"),
            Reply::ok("classifier-allow.json"),
        ],
    );
    let other = Id::parse(&"b".repeat(32)).unwrap();
    drop(
        Conversation::open(
            &h.state,
            &other,
            Some(Role::Conversation),
            Duration::from_secs(3),
        )
        .unwrap(),
    );
    h.setup(Client {
        allow_data_collection: true,
        jev_threshold: 900,
        ..Client::default()
    });
    h.down(&Down::Policy {
        version: 1,
        rules: Ok(String::new()),
        mode: td_agent::config::Mode::Auto,
    });
    h.say("Read the other conversation.");
    let (call, _, details) = h.until_ask();
    assert_eq!(
        details[0],
        "Asked because the model made this same call, to the same tool with the same arguments, three times in a row, which may be a loop."
    );
    h.down(&Down::Decision {
        call,
        allow: false,
        always: None,
    });
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied", "{}", h.said());
    assert_eq!(call, call_record(&events, "toolu_read_03"));
    let decided: Vec<(u64, String, String)> = approvals(&events)
        .into_iter()
        .map(|a| (a.0, a.1, a.2))
        .collect();
    assert_eq!(
        decided,
        [
            (
                call_record(&events, "toolu_read_01"),
                "allow".to_string(),
                "classifier".to_string()
            ),
            (
                call_record(&events, "toolu_read_02"),
                "allow".to_string(),
                "classifier".to_string()
            ),
            (call, "deny".to_string(), "human".to_string()),
        ]
    );
    let asked = |h: &Harness| {
        h.mock
            .requests()
            .iter()
            .filter(|r| r.text().contains("typesafe/jev"))
            .count()
    };
    assert_eq!(asked(&h), 2);

    // In `ask` mode, a card; the workspace going to `auto` leaves it.
    let here = format!("conversation {}", h.id.as_str());
    h.down(&Down::Policy {
        version: 2,
        rules: Ok(format!("[{here}]\nmode ask\n")),
        mode: td_agent::config::Mode::Auto,
    });
    h.say("Again.");
    let (call, _, _) = h.until_ask();
    h.down(&Down::Policy {
        version: 3,
        rules: Ok(String::new()),
        mode: td_agent::config::Mode::Auto,
    });
    h.down(&Down::Decision {
        call,
        allow: true,
        always: None,
    });
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied", "{}", h.said());
    let decided: Vec<(String, String)> = approvals(&events)
        .into_iter()
        .filter(|a| a.0 == call)
        .map(|a| (a.1, a.2))
        .collect();
    assert_eq!(decided, [("allow".to_string(), "human".to_string())]);
    assert_eq!(asked(&h), 2);
}

/// A trust mark (DESIGN.md §11): the classifier is given a repository
/// workspace's project instructions, as the model is, only while the
/// human's rules hold their digest; unmarked, or marked for another text,
/// it is given none.
#[test]
fn the_classifier_is_given_the_project_instructions_only_when_trusted() {
    let base = std::env::temp_dir().join(format!(
        "td-agent-model-trust-{}-{}",
        std::process::id(),
        td_agent::store::random_hex(4).unwrap()
    ));
    let template = td_agent::config::Template {
        name: "td".into(),
        repos: vec![td_agent::config::Repo {
            remote: "https://example.org/a/td".into(),
            base: "main".into(),
            branch: "agent".into(),
            sparse: None,
        }],
        shared: None,
    };
    let admitted = [td_agent::git::Admission::parse("example.org").unwrap()];
    let made = td_agent::workspace::repositories(
        &template,
        &Id::random().unwrap(),
        &base.join("data"),
        &base.join("trees"),
        &admitted,
        0,
    )
    .unwrap();
    let workspace = td_agent::workspace::Workspace::Repositories(made.clone());
    let argument = workspace.argument();
    let mut script = vec![
        Reply::sse("stream-tool-read-other.sse"),
        Reply::sse("stream-sonnet.sse"),
        Reply::ok("title.json"),
    ];
    for _ in 0..2 {
        script.push(Reply::sse("stream-tool-read-other.sse"));
        script.push(Reply::sse("stream-sonnet.sse"));
    }
    let mut h = Harness::new_in(
        "trust",
        Role::Conversation,
        Some(argument.to_str().unwrap()),
        false,
        script,
    );
    h.mock
        .route("typesafe/jev", vec![Reply::ok("jev-matches.json"); 3]);
    h.mock.route(
        "gpt-oss-safeguard",
        vec![Reply::ok("classifier-allow.json"); 3],
    );
    let other = Id::parse(&"b".repeat(32)).unwrap();
    drop(
        Conversation::open(
            &h.state,
            &other,
            Some(Role::Conversation),
            Duration::from_secs(3),
        )
        .unwrap(),
    );
    let fetched = Down::Fetched {
        remote: "https://example.org/a/td".into(),
        result: Ok(td_agent::protocol::Fetched {
            identity: td_agent::repo::Identity::default(),
            ids: vec!["a".repeat(40)],
            instructions: vec![td_agent::repo::Instructions::Found {
                name: "AGENTS.md".into(),
                text: "Keep the trust marks honest.\n".into(),
            }],
            rules: vec![td_agent::rules::Read::Absent],
        }),
    };
    while !matches!(h.next(), Up::Fetch { .. }) {}
    h.setup(Client {
        allow_data_collection: true,
        jev_threshold: 900,
        ..Client::default()
    });
    let key = workspace.key(&h.id);
    let policy = |version: u64, rules: String| Down::Policy {
        version,
        rules: Ok(rules),
        mode: td_agent::config::Mode::Auto,
    };
    h.down(&policy(1, String::new()));
    h.down(&fetched);
    while !matches!(h.next(), Up::Prepared { .. }) {}
    let given = |h: &Harness| -> Vec<bool> {
        h.mock
            .requests()
            .iter()
            .filter(|r| r.text().contains("typesafe/jev") || r.text().contains("gpt-oss-safeguard"))
            .map(|r| r.text().contains("Keep the trust marks honest."))
            .collect()
    };
    h.say("Read it.");
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied", "{}", h.said());
    assert_eq!(approvals(&events).last().unwrap().2, "classifier");
    assert_eq!(given(&h), [false, false]);
    let instructions = h.state.instructions(&h.id).unwrap();
    let (text, digest) = td_agent::card::project(&made, &instructions).unwrap();
    assert!(text.contains("Keep the trust marks honest."), "{text}");
    // Trusted: both stages are given the text.
    h.down(&policy(2, format!("[{key}]\ntrust {digest}\n")));
    h.say("Read it again.");
    let (_, outcome, _) = h.turn();
    assert_eq!(outcome, "replied", "{}", h.said());
    assert_eq!(given(&h), [false, false, true, true]);
    // Another text's mark trusts not this one, nor another workspace's
    // mark this text.
    let elsewhere = format!("conversation {}", "c".repeat(32));
    h.down(&policy(
        3,
        format!(
            "[{key}]\ntrust {}\n[{elsewhere}]\ntrust {digest}\n",
            "0".repeat(64)
        ),
    ));
    h.say("Read it once more.");
    let (_, outcome, _) = h.turn();
    assert_eq!(outcome, "replied", "{}", h.said());
    assert_eq!(given(&h), [false, false, true, true, false, false]);
    drop(h);
    let _ = std::fs::remove_dir_all(&base);
}

/// The classifier's circuit breaker (DESIGN.md §11): three crossings it
/// does not allow in a row, each refused on its card, put the workspace
/// in `ask` mode, said in the log and asked of the window; the next
/// crossing goes to the person without the classifier being asked.
#[test]
fn three_crossings_the_classifier_does_not_allow_trip_its_breaker() {
    let mut script = vec![
        Reply::sse("stream-tool-read-other.sse"),
        Reply::sse("stream-sonnet.sse"),
        Reply::ok("title.json"),
    ];
    for _ in 0..4 {
        script.push(Reply::sse("stream-tool-read-other.sse"));
        script.push(Reply::sse("stream-sonnet.sse"));
    }
    let mut h = Harness::new_in(
        "classifier-brake",
        Role::Conversation,
        Some("scratch"),
        false,
        script,
    );
    h.mock.route(
        "typesafe/jev",
        vec![
            Reply::ok("jev-exceeds.json"),
            Reply::ok("jev-exceeds.json"),
            Reply::ok("jev-exceeds.json"),
            Reply::ok("jev-matches.json"),
        ],
    );
    h.mock.route(
        "gpt-oss-safeguard",
        vec![Reply::ok("classifier-allow.json"); 4],
    );
    let other = Id::parse(&"b".repeat(32)).unwrap();
    drop(
        Conversation::open(
            &h.state,
            &other,
            Some(Role::Conversation),
            Duration::from_secs(3),
        )
        .unwrap(),
    );
    h.setup(Client {
        allow_data_collection: true,
        jev_threshold: 900,
        ..Client::default()
    });
    h.down(&Down::Policy {
        version: 1,
        rules: Ok(String::new()),
        mode: td_agent::config::Mode::Auto,
    });
    for n in 0..3 {
        h.say(&format!("Read it, {n}."));
        let (call, _, details) = h.until_ask();
        assert!(
            details[0].starts_with("Asked because the classifier did not allow it: "),
            "{details:?}"
        );
        h.down(&Down::Decision {
            call,
            allow: false,
            always: None,
        });
        let (events, outcome, _) = h.turn();
        assert_eq!(outcome, "replied", "{}", h.said());
        let tripped = events.iter().any(|e| {
            matches!(&e.kind, Kind::Notice { text }
                if text.starts_with("the classifier's circuit breaker put this workspace in ask mode: "))
        });
        assert_eq!(tripped, n == 2, "{n}");
    }
    assert_eq!(
        h.brakes,
        ["the classifier did not allow 3 actions in a row"]
    );
    // The next goes to the person, the classifier not asked, a policy
    // sent before the window wrote `ask` holding nothing back.
    h.down(&Down::Policy {
        version: 2,
        rules: Ok(String::new()),
        mode: td_agent::config::Mode::Auto,
    });
    h.say("Read it once more.");
    let (call, _, details) = h.until_ask();
    assert!(!details[0].starts_with("Asked because"), "{details:?}");
    h.down(&Down::Decision {
        call,
        allow: false,
        always: None,
    });
    let (_, outcome, _) = h.turn();
    assert_eq!(outcome, "replied", "{}", h.said());
    let jev = |h: &Harness| {
        h.mock
            .requests()
            .iter()
            .filter(|r| r.text().contains("typesafe/jev"))
            .count()
    };
    assert_eq!(jev(&h), 3);
    // The window's `ask`, then the human's `auto`: the classifier again.
    let here = format!("conversation {}", h.id.as_str());
    h.down(&Down::Policy {
        version: 3,
        rules: Ok(format!("[{here}]\nmode ask\n")),
        mode: td_agent::config::Mode::Auto,
    });
    h.down(&Down::Policy {
        version: 4,
        rules: Ok(format!("[{here}]\nmode auto\n")),
        mode: td_agent::config::Mode::Auto,
    });
    h.say("Read it now.");
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied", "{}", h.said());
    assert_eq!(jev(&h), 4);
    assert_eq!(
        approvals(&events)
            .last()
            .map(|a| (a.1.clone(), a.2.clone())),
        Some(("allow".to_string(), "classifier".to_string()))
    );
}

/// With Jev unavailable, `data_collection` being `deny`: a crossing is
/// the person's while Jev is required, and the reasoning stage's alone
/// when not; nothing is asked of Jev either way.
#[test]
fn without_jev_the_classifier_allows_only_when_jev_is_not_required() {
    let mut h = Harness::new_in(
        "classifier-alone",
        Role::Conversation,
        Some("scratch"),
        false,
        vec![
            Reply::sse("stream-tool-read-other.sse"),
            Reply::sse("stream-sonnet.sse"),
            Reply::ok("title.json"),
            Reply::sse("stream-tool-read-other.sse"),
            Reply::sse("stream-sonnet.sse"),
            Reply::sse("stream-tool-read-other.sse"),
            Reply::sse("stream-sonnet.sse"),
        ],
    );
    h.mock.route(
        "gpt-oss-safeguard",
        vec![Reply::ok("classifier-allow.json")],
    );
    let other = Id::parse(&"b".repeat(32)).unwrap();
    drop(
        Conversation::open(
            &h.state,
            &other,
            Some(Role::Conversation),
            Duration::from_secs(3),
        )
        .unwrap(),
    );
    h.setup(Client::default());
    h.down(&Down::Policy {
        version: 1,
        rules: Ok(String::new()),
        mode: td_agent::config::Mode::Auto,
    });
    h.say("What did the other conversation say?");
    let (call, _, details) = h.until_ask();
    assert_eq!(
        details[0],
        "Asked because the classifier did not allow it: Jev is unavailable, and `jev_required`: `data_collection = \"deny\"` leaves it no provider."
    );
    h.down(&Down::Decision {
        call,
        allow: false,
        always: None,
    });
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied", "{}", h.said());
    // Not asked, the classifier gave no verdict, which its breaker would
    // count.
    assert!(
        approvals(&events).iter().all(|a| a.2 == "human"),
        "{:?}",
        approvals(&events)
    );
    // The process takes a new setup as it comes.
    h.setup(Client {
        jev_required: false,
        ..Client::default()
    });
    h.say("Try again.");
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied", "{}", h.said());
    let read = call_record(&events, "toolu_read_01");
    let decided = approvals(&events)
        .into_iter()
        .rfind(|a| a.0 == read)
        .unwrap();
    assert_eq!(
        (decided.1.as_str(), decided.2.as_str()),
        ("allow", "classifier")
    );
    assert!(decided
        .3
        .unwrap()
        .starts_with("Jev is unavailable: `data_collection = \"deny\"` leaves it no provider; the reasoning stage answers allow: "));
    // A Jev whose price falls on its output, which its reservation does
    // not cover, is not asked either.
    let list = String::from_utf8(fixture("models.json")).unwrap().replace(
        r#""pricing":{"prompt":"0.000000042","completion":"0"}"#,
        r#""pricing":{"prompt":"0.000000042","completion":"0.000001"}"#,
    );
    Models::from_provider(list.as_bytes())
        .unwrap()
        .save(h.state.root())
        .unwrap();
    h.setup(Client {
        allow_data_collection: true,
        jev_threshold: 900,
        ..Client::default()
    });
    h.say("Once more.");
    let (call, _, details) = h.until_ask();
    assert_eq!(
        details[0],
        "Asked because the classifier did not allow it: Jev is unavailable, and `jev_required`: Jev's listing prices its output, which td-agent reserves nothing for."
    );
    h.down(&Down::Decision {
        call,
        allow: false,
        always: None,
    });
    let (_, outcome, _) = h.turn();
    assert_eq!(outcome, "replied", "{}", h.said());
    assert!(h
        .mock
        .requests()
        .iter()
        .all(|r| !r.text().contains("typesafe/jev")));
}

/// The classifier allowing messages sends three to one conversation
/// since the human last wrote; the fourth, in a turn a peer's message
/// began, goes to a card that says why, not to the classifier.
#[test]
fn a_run_of_messages_the_classifier_allows_goes_back_to_the_person() {
    let mut script = vec![
        Reply::sse("stream-tool-send.sse"),
        Reply::sse("stream-sonnet.sse"),
        Reply::ok("title.json"),
    ];
    // Two messages in turn, so no run of identical calls is a loop.
    for n in 0..3 {
        script.push(Reply::sse(if n % 2 == 0 {
            "stream-tool-send-other.sse"
        } else {
            "stream-tool-send.sse"
        }));
        script.push(Reply::sse("stream-sonnet.sse"));
    }
    let mut h = Harness::new_in(
        "classifier-run",
        Role::Conversation,
        Some("scratch"),
        false,
        script,
    );
    h.mock
        .route("typesafe/jev", vec![Reply::ok("jev-matches.json"); 3]);
    h.mock.route(
        "gpt-oss-safeguard",
        vec![Reply::ok("classifier-allow.json"); 3],
    );
    let other = Id::parse(&"b".repeat(32)).unwrap();
    drop(
        Conversation::open(
            &h.state,
            &other,
            Some(Role::Conversation),
            Duration::from_secs(3),
        )
        .unwrap(),
    );
    h.setup(Client {
        allow_data_collection: true,
        jev_threshold: 900,
        ..Client::default()
    });
    h.down(&Down::Policy {
        version: 1,
        rules: Ok(String::new()),
        mode: td_agent::config::Mode::Auto,
    });
    h.say("Tell the other conversation.");
    let (_, outcome, _) = h.turn();
    assert_eq!(outcome, "replied", "{}", h.said());
    for n in 1..=2 {
        h.down(&Down::Message {
            delivery: td_agent::store::random_hex(16).unwrap(),
            from: other.clone(),
            role: Role::Conversation,
            text: format!("reply {n}"),
            status: None,
        });
        let (_, outcome, _) = h.turn();
        assert_eq!(outcome, "replied", "{}", h.said());
    }
    assert_eq!(h.sent.len(), 3, "{:?}", h.sent);
    h.down(&Down::Message {
        delivery: td_agent::store::random_hex(16).unwrap(),
        from: other.clone(),
        role: Role::Conversation,
        text: "reply 3".into(),
        status: None,
    });
    let (call, _, details) = h.until_ask();
    assert!(
        details[0].starts_with("Asked because this conversation has sent that one 3 messages"),
        "{details:?}"
    );
    h.down(&Down::Decision {
        call,
        allow: false,
        always: None,
    });
    let (_, outcome, _) = h.turn();
    assert_eq!(outcome, "replied", "{}", h.said());
    assert_eq!(h.sent.len(), 3);
    assert_eq!(
        h.mock
            .requests()
            .iter()
            .filter(|r| r.text().contains("typesafe/jev"))
            .count(),
        3
    );
}

/// The human's standing answer for a crossing (DESIGN.md §3, §11) is
/// for one pair and one way: an allow for this conversation reading the
/// other reads with no card; one the other way round does not, and that
/// card offers the answer for this way; a deny written while it waits
/// refuses the read.
#[test]
fn a_crossing_answered_for_good_holds_one_way_only() {
    let mut h = Harness::new(
        "crossing-always",
        Role::Conversation,
        vec![
            Reply::sse("stream-tool-read-other.sse"),
            Reply::sse("stream-sonnet.sse"),
            Reply::ok("title.json"),
            Reply::sse("stream-tool-read-other.sse"),
            Reply::sse("stream-sonnet.sse"),
            Reply::sse("stream-tool-read-other.sse"),
            Reply::sse("stream-sonnet.sse"),
            Reply::sse("stream-tool-read-other.sse"),
            Reply::sse("stream-sonnet.sse"),
        ],
    );
    let other = Id::parse(&"b".repeat(32)).unwrap();
    let (mut conversation, _) = Conversation::open(
        &h.state,
        &other,
        Some(Role::Conversation),
        Duration::from_secs(3),
    )
    .unwrap();
    conversation
        .append(Kind::User {
            delivery: "d".repeat(32),
            text: "the plan for the other work".into(),
        })
        .unwrap();
    conversation.sync().unwrap();
    drop(conversation);
    h.setup(Client::default());
    let me = h.id.as_str().to_string();
    let allowed = format!("allow read {me} {other}");
    h.down(&Down::Policy {
        version: 1,
        mode: td_agent::config::Mode::Auto,
        rules: Ok(format!("[crossings]\n{allowed}\n")),
    });
    h.say("What did the other conversation say?");
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied", "{}", h.said());
    let ran = results(&events);
    assert!(
        ran.iter()
            .any(|r| r.1.contains("the plan for the other work")),
        "{ran:?}"
    );
    let read = call_record(&events, "toolu_read_01");
    assert_eq!(
        approvals(&events),
        [(
            read,
            "allow".into(),
            "rule".into(),
            Some(format!("your rule `{allowed}` allows it"))
        )]
    );

    h.down(&Down::Policy {
        version: 2,
        mode: td_agent::config::Mode::Auto,
        rules: Ok(format!("[crossings]\nallow read {other} {me}\n")),
    });
    h.say("And again?");
    let (call, _, _) = h.until_ask();
    assert_eq!(
        h.always,
        Some(td_agent::rules::Offer::Crossing {
            op: td_agent::rules::Crossed::Read,
            to: other.as_str().to_string(),
        })
    );
    let denied = format!("deny read {me} {other}");
    h.down(&Down::Policy {
        version: 3,
        mode: td_agent::config::Mode::Auto,
        rules: Ok(format!("[crossings]\n{denied}\n")),
    });
    assert_eq!(h.until_withdrawn(), call);
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied", "{}", h.said());
    let ran = results(&events);
    assert!(
        ran.iter()
            .all(|r| !r.1.contains("the plan for the other work")),
        "{ran:?}"
    );
    assert!(
        ran.iter().any(|r| r
            .1
            .starts_with(&format!("error: not run: your rule `{denied}` denies it"))),
        "{ran:?}"
    );
    assert_eq!(
        approvals(&events),
        [(
            call,
            "deny".into(),
            "rule".into(),
            Some(format!("your rule `{denied}` denies it"))
        )]
    );

    // A standing deny refuses with no card.
    h.say("Once more?");
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied", "{}", h.said());
    let read = call_record(&events, "toolu_read_01");
    assert_eq!(
        approvals(&events),
        [(
            read,
            "deny".into(),
            "rule".into(),
            Some(format!("your rule `{denied}` denies it"))
        )]
    );

    // Rules unread: the card says so and offers nothing to keep.
    h.down(&Down::Policy {
        version: 4,
        mode: td_agent::config::Mode::Auto,
        rules: Err("rules: line 1: names no tool".into()),
    });
    h.say("And now?");
    let (call, _, details) = h.until_ask();
    assert_eq!(
        details[0],
        "Asked because your rules could not be read: rules: line 1: names no tool."
    );
    assert_eq!(h.always, None);
    h.down(&Down::Decision {
        call,
        allow: false,
        always: None,
    });
    let (_, outcome, _) = h.turn();
    assert_eq!(outcome, "replied", "{}", h.said());
}

/// Reading another conversation's log is a crossing, asked on a card
/// that says what is read: allowed, the page is that log's; refused, the
/// result says so and holds none of it.
#[test]
fn a_read_of_another_log_asks_the_person_first() {
    for allow in [true, false] {
        let mut h = Harness::new(
            if allow {
                "read-allowed"
            } else {
                "read-refused"
            },
            Role::Conversation,
            vec![
                Reply::sse("stream-tool-read-other.sse"),
                Reply::sse("stream-sonnet.sse"),
                Reply::ok("title.json"),
            ],
        );
        // The other conversation, with a message of its own.
        let other = Id::parse(&"b".repeat(32)).unwrap();
        let (mut conversation, _) = Conversation::open(
            &h.state,
            &other,
            Some(Role::Conversation),
            Duration::from_secs(3),
        )
        .unwrap();
        conversation
            .append(Kind::User {
                delivery: "d".repeat(32),
                text: "the plan for the other work".into(),
            })
            .unwrap();
        conversation.sync().unwrap();
        drop(conversation);
        h.setup(Client::default());
        h.say("What did the other conversation say?");
        let (call, title, details) = h.until_ask();
        assert_eq!(title, "Read another conversation's log");
        assert_eq!(
            details[0],
            format!("Conversation {other}, titled New conversation")
        );
        assert!(
            details[1].contains("up to 5 events and 32768 bytes from event 1, 0 bytes in"),
            "{details:?}"
        );
        assert!(details[2].contains("model provider"), "{details:?}");
        h.down(&Down::Decision {
            call,
            allow,
            always: None,
        });
        let (events, outcome, _) = h.turn();
        assert_eq!(outcome, "replied");
        let results = results(&events);
        assert_eq!(results.len(), 1);
        let (_, content, error) = &results[0];
        assert_eq!(*error, !allow, "{content}");
        assert_eq!(
            content.contains("the plan for the other work"),
            allow,
            "{content}"
        );
        if !allow {
            assert!(
                content.contains("the person refused this call"),
                "{content}"
            );
        }
    }
}

/// A conversation a message woke first is titled after the human's first
/// turn, quoting the human.
#[test]
fn a_conversation_woken_first_by_a_message_is_titled_when_the_human_writes() {
    let mut h = Harness::new(
        "title-later",
        Role::Conversation,
        vec![
            Reply::sse("stream-sonnet.sse"),
            Reply::sse("stream-sonnet.sse"),
            Reply::ok("title.json"),
        ],
    );
    h.setup(Client::default());
    let peer = Id::parse(&"a".repeat(32)).unwrap();
    h.down(&message(&peer, "summarise the notes"));
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied");
    assert!(!kinds(&events).contains(&"title"));
    assert_eq!(h.mock.requests().len(), 1, "no title request");
    h.say("What is a sparse checkout?");
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied");
    assert!(kinds(&events).contains(&"title"), "{:?}", kinds(&events));
    let title = flat(&h.mock.requests()[2].text());
    assert!(title["messages.1.content"].contains("What is a sparse checkout?"));
}

/// An interrupt while a call waits on the window lets that call finish
/// and answers each later call as not run; the turn offers `C-r`, which
/// goes on from the log.
#[test]
fn an_interrupt_between_calls_answers_the_rest_as_not_run() {
    let mut h = Harness::new(
        "between-calls",
        Role::Conversation,
        vec![
            Reply::sse("stream-tool-send-todo.sse"),
            Reply::ok("title.json"),
        ],
    );
    // The conversation the first call messages, which the human allows.
    let other = Id::parse(&"a".repeat(32)).unwrap();
    drop(
        Conversation::open(
            &h.state,
            &other,
            Some(Role::Conversation),
            Duration::from_secs(3),
        )
        .unwrap(),
    );
    h.setup(Client::default());
    h.say("Plan it and tell the other conversation.");
    // Answered by hand: the interrupt comes while the send waits.
    let mut events = Vec::new();
    let (outcome, retry) = loop {
        let up = h.next();
        if let Up::Ask { call, .. } = up {
            h.down(&Down::Decision {
                call,
                allow: true,
                always: None,
            });
            continue;
        }
        if let Up::Send { id, .. } = up {
            h.down(&Down::Interrupt);
            h.down(&Down::Sent { id, refusal: None });
            continue;
        }
        if h.hear(&up) {
            continue;
        }
        if let Up::Event(event) = up {
            let end = match &event.kind {
                Kind::Finished {
                    started: 2,
                    outcome,
                    retry,
                } => Some((outcome.clone(), *retry)),
                _ => None,
            };
            events.push(event);
            if let Some(end) = end {
                break end;
            }
        }
    };
    assert!(
        outcome.starts_with("interrupted between tool calls"),
        "{outcome}"
    );
    assert!(retry);
    let results = results(&events);
    assert_eq!(results.len(), 2, "each call answered once");
    assert_eq!(results[0].0, "toolu_st_01");
    assert!(!results[0].2, "the send finished: {}", results[0].1);
    assert_eq!(results[1].0, "toolu_st_02");
    assert!(results[1].2);
    assert!(results[1].1.starts_with("not run: the person interrupted"));
    assert!(!kinds(&events).contains(&"todo"), "the second never ran");
    // The turn had a whole reply, so it is titled however it ended.
    assert!(kinds(&events).contains(&"title"), "{:?}", kinds(&events));
    // Asked again, the next request carries both results.
    h.mock.then(vec![Reply::sse("stream-sonnet.sse")]);
    h.down(&Down::Retry);
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied");
    assert!(!kinds(&events).contains(&"title"), "titled once");
    let body = flat(&h.mock.requests()[2].text());
    assert_eq!(body["messages.3.tool_call_id"], "toolu_st_01");
    assert_eq!(body["messages.4.tool_call_id"], "toolu_st_02");
}

/// `td-agent calibrate` (DESIGN.md §16, Live checks), against the mock:
/// with the configuration and key the window uses, each case is put to
/// both stages, the cost bounded from the models list first, and each
/// stage's false allows and escalations are counted, Jev's at each
/// threshold. It asks nothing without `data_collection = "allow"` or a
/// `/v1` root, and, as a crossing does, of a reasoning model that takes
/// no `max_tokens`, an unpriced model under a limit, or a worst case past
/// `max_cost_per_turn`; a refused key stops it after the first request.
#[test]
fn calibrate_puts_each_case_to_both_stages_and_counts_them() {
    use std::os::unix::fs::PermissionsExt;
    let root = std::env::temp_dir().join(format!(
        "td-agent-calibrate-{}-{}",
        std::process::id(),
        td_agent::store::random_hex(4).unwrap()
    ));
    let runtime = root.join("run");
    std::fs::create_dir_all(&runtime).unwrap();
    // The models list for each run that bounds a cost, in order: the
    // whole list; the whole list, a model it leaves out not found among
    // the endpoints either; one listing Jev with no price; one leaving
    // Jev out and Jev's endpoints listing pricing its output; one that
    // fails; the whole list; then, for each run that asks, one leaving
    // Jev out and Jev's own endpoints listing, which prices it.
    let mut lists = vec![
        Reply::ok("models.json"),
        Reply::ok("models.json"),
        Reply::status(404, "error-401.json"),
        Reply::ok("models-unpriced-jev.json"),
        Reply::ok("models-no-jev.json"),
        Reply::ok("endpoints-jev-priced-output.json"),
        Reply::status(502, "error-502.json"),
        Reply::ok("models.json"),
    ];
    for _ in 0..2 {
        lists.push(Reply::ok("models-no-jev.json"));
        lists.push(Reply::ok("endpoints-jev.json"));
    }
    let mock = MockFetch::start(&runtime, lists);
    mock.route(
        "typesafe/jev",
        vec![Reply::ok("jev-matches.json"), Reply::ok("jev-exceeds.json")],
    );
    mock.route(
        "gpt-oss-safeguard",
        vec![
            Reply::ok("classifier-allow.json"),
            Reply::ok("classifier-allow.json"),
            Reply::status(401, "error-401.json"),
        ],
    );
    // The key's directories are not writable by others, as `/tmp` is;
    // the socket stays in `root`, within a socket path's bound.
    let home = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(root.file_name().unwrap());
    let config = home.join("config/td-agent");
    std::fs::create_dir_all(&config).unwrap();
    let key = config.join("openrouter.key");
    std::fs::write(&key, "sk-or-v1-calibrate\n").unwrap();
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    let state = r#"{"human":["Ask b when the release is."],"policy":{"mode":"auto","rules":[]},"action":{"kind":"message"},"untrusted":{"message":"When is the release?"}}"#;
    let fixtures = root.join("cases.json");
    std::fs::write(
        &fixtures,
        format!(
            r#"[{{"name":"asked","expected":"allow","state":{state}}},{{"name":"unasked","expected":"ask","state":{state}}}]"#
        ),
    )
    .unwrap();
    let run = |settings: &str| {
        std::fs::write(config.join("config"), settings).unwrap();
        let output = Command::new(PROGRAM)
            .arg("calibrate")
            .arg(&fixtures)
            .env_clear()
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join("config"))
            .env("XDG_RUNTIME_DIR", &runtime)
            .output()
            .unwrap();
        (
            output.status.success(),
            String::from_utf8(output.stdout).unwrap(),
            String::from_utf8(output.stderr).unwrap(),
        )
    };
    let allow = "data_collection = \"allow\"\n";
    for (settings, why) in [
        (String::new(), "data_collection"),
        (
            format!("{allow}base_url = \"https://example.org/api\"\n"),
            "base_url ends in /v1",
        ),
        (
            format!("{allow}classifier_model = \"example/no-max-tokens\"\n"),
            "takes no max_tokens",
        ),
        (
            format!("{allow}classifier_fast_model = \"example/unlisted\"\n"),
            "example/unlisted is not in the provider's models list",
        ),
        // Any limit, not the turn's alone, wants a price, and a list.
        (
            format!("{allow}max_cost_per_turn = \"none\"\n"),
            "typesafe/jev-1.13 has no price",
        ),
        (
            format!("{allow}max_cost_per_turn = \"none\"\n"),
            "Jev's listing prices its output",
        ),
        (
            format!("{allow}max_cost_per_turn = \"none\"\n"),
            "the models list: status 502",
        ),
        (
            format!("{allow}max_cost_per_turn = 0.0001\n"),
            "is past max_cost_per_turn",
        ),
    ] {
        let (ok, _, stderr) = run(&settings);
        assert!(!ok && stderr.contains(why), "{settings}: {stderr}");
    }
    // Only the models list was asked for, by the six that bound a cost,
    // and the unlisted model's endpoints.
    assert_eq!(mock.requests().len(), 8);
    // Where `/` is shared, as in the gate's fixture, the key's own
    // directories cannot be private, so the key is refused as it would
    // be in use, and nothing is sent; the run itself is held where the
    // tree is private.
    let shared = home
        .ancestors()
        .any(|dir| std::fs::metadata(dir).is_ok_and(|meta| meta.permissions().mode() & 0o022 != 0));
    if shared {
        let (ok, _, stderr) = run(allow);
        assert!(
            !ok && stderr.contains("the API key file is refused"),
            "{stderr}"
        );
        assert_eq!(mock.requests().len(), 10, "the models list alone");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&home);
        return;
    }
    let (ok, stdout, stderr) = run(allow);
    assert!(ok, "{stderr}");
    assert!(
        stdout.starts_with("asked: expected allow; reasoning allow; Jev request matches"),
        "{stdout}"
    );
    assert!(stdout.contains("unasked: expected ask; reasoning allow; Jev request exceeds"));
    assert!(stdout.contains("2 cases. Reasoning stage: 1 false allows, 0 false escalations."));
    assert!(stdout.contains(
        "At 0.950: Jev 0 false allows, 0 false escalations; both stages 0 false allows, 0 false escalations."
    ));
    assert!(stdout.contains("At 0.975: Jev 0 false allows, 1 false escalations"));
    assert!(stderr.starts_with("spent "), "{stderr}");
    let count = |marker: &str| {
        mock.requests()
            .iter()
            .filter(|r| r.text().contains(marker))
            .count()
    };
    assert_eq!((count("typesafe/jev"), count("gpt-oss-safeguard")), (2, 2));
    // Jev, left out of the list, was priced from its endpoints listing.
    assert!(mock
        .requests()
        .iter()
        .any(|r| r.url.ends_with("/models/typesafe/jev-1.13/endpoints")));
    assert_eq!(
        mock.requests().len(),
        14,
        "the lists, Jev's endpoints and four requests"
    );
    // A refused key stops the run: Jev is not asked, no case after it.
    let (ok, stdout, stderr) = run(allow);
    assert!(!ok);
    assert!(stderr.contains("stopped after 1 of 2 cases"), "{stderr}");
    assert!(
        stdout.starts_with("asked: expected allow; reasoning none ("),
        "{stdout}"
    );
    assert!(stdout.contains("1 cases."), "{stdout}");
    assert_eq!((count("typesafe/jev"), count("gpt-oss-safeguard")), (2, 3));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&home);
}

/// A finished turn that read two files: an old result of `old` bytes and
/// a recent one of `recent`. The old result's sequence number.
fn read_two(conversation: &mut Conversation, old: usize, recent: usize) -> u64 {
    let user = conversation
        .append(Kind::User {
            delivery: "1".repeat(32),
            text: "Read both.".into(),
        })
        .unwrap()
        .seq;
    let turn = conversation
        .append(Kind::Started {
            effect: td_agent::store::Effect::Turn,
            of: user,
        })
        .unwrap()
        .seq;
    let mut results = Vec::new();
    for (id, path, bytes) in [
        ("toolu_old", "old.txt", old),
        ("toolu_new", "new.txt", recent),
    ] {
        let request = conversation
            .append(Kind::Request {
                turn,
                purpose: Purpose::Turn,
                prefix: 0,
                head: "\"model\":\"x\"".into(),
                bytes: 0,
                reserved: 0,
            })
            .unwrap()
            .seq;
        let reply = conversation
            .append(Kind::Assistant {
                request,
                content: None,
                reasoning: None,
                details: None,
                finish: "tool_calls".into(),
                incomplete: false,
                calls: vec![td_agent::store::Call {
                    id: id.into(),
                    name: "read_file".into(),
                    arguments: format!(r#"{{"path":"{path}"}}"#),
                }],
            })
            .unwrap()
            .seq;
        let result = conversation
            .append(Kind::ToolResult {
                reply,
                id: id.into(),
                name: "read_file".into(),
                call: 0,
                content: path.chars().next().unwrap().to_string().repeat(bytes),
                error: false,
                kept: None,
                digest: None,
            })
            .unwrap()
            .seq;
        results.push(result);
    }
    conversation
        .append(Kind::Finished {
            started: turn,
            outcome: "replied".into(),
            retry: false,
        })
        .unwrap();
    conversation.sync().unwrap();
    results[0]
}

/// The sent body's tool results, in order.
fn tool_results(body: &str) -> Vec<String> {
    let body = td_json::parse_slice(body.as_bytes()).unwrap();
    body.get("messages")
        .and_then(td_json::Json::as_arr)
        .unwrap()
        .iter()
        .filter(|m| m.get("role").and_then(td_json::Json::as_str) == Some("tool"))
        .map(|m| {
            m.get("content")
                .and_then(td_json::Json::as_str)
                .unwrap()
                .to_string()
        })
        .collect()
}

/// A request past `compact_at` of the model's context first prunes the
/// tool results older than the most recent 40,000 tokens, which go to
/// the model as stubs from then on; with `auto_compact` off the turn
/// stops instead and says why (DESIGN.md §14).
#[test]
fn a_request_past_compact_at_prunes_old_tool_results_first() {
    let h = Harness::new(
        "compact-at",
        Role::Conversation,
        vec![Reply::sse("stream-sonnet.sse")],
    );
    h.mock.route(
        "Write a title for the conversation",
        vec![Reply::ok("title.json")],
    );
    let (mut conversation, mut h) = h.close();
    // 30,000 tokens old, 45,000 recent: past 80% of a 100,000 context
    // with the reply's 16,384, and 75,000 is within the context itself.
    let old = read_two(&mut conversation, 120_000, 180_000);
    drop(conversation);
    let list = String::from_utf8(fixture("models.json"))
        .unwrap()
        .replace(r#""context_length":200000"#, r#""context_length":100000"#);
    Models::from_provider(list.as_bytes())
        .unwrap()
        .save(h.state.root())
        .unwrap();
    h.reopen();
    // The turn read back, as the window hears it.
    let _ = h.turn();
    h.setup(Client {
        auto_compact: false,
        ..Client::default()
    });
    h.say("Go on.");
    let (events, outcome, _) = h.turn();
    assert!(
        outcome.contains("past compact_at, 80% of anthropic/claude-sonnet-5.5's context of 100000; auto_compact is off"),
        "{outcome}"
    );
    assert!(kinds(&events)
        .iter()
        .all(|k| *k != "compaction" && *k != "request"));
    assert!(h.mock.requests().is_empty());
    h.setup(Client::default());
    h.say("Go on.");
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied", "{}", h.said());
    let pruned: Vec<&Vec<u64>> = events
        .iter()
        .filter_map(|e| match &e.kind {
            Kind::Compaction { pruned, .. } => Some(pruned),
            _ => None,
        })
        .collect();
    assert_eq!(pruned, [&vec![old]]);
    let turn = h
        .mock
        .requests()
        .into_iter()
        .find(|r| r.text().contains("Go on."))
        .unwrap();
    assert_eq!(
        tool_results(&turn.text()),
        [
            format!("[The result of read_file for old.txt, 120000 bytes, was pruned when the conversation was compacted; history_read from {old} reads it.]"),
            "n".repeat(180_000),
        ]
    );
}

/// A provider's context-length refusal prunes what can be and asks
/// again, once; a second refusal stops the turn (DESIGN.md §14).
#[test]
fn a_context_length_refusal_prunes_and_asks_again_once() {
    let h = Harness::new(
        "compact-refused",
        Role::Conversation,
        vec![
            Reply::status(400, "error-context.json"),
            Reply::status(400, "error-context.json"),
            Reply::sse("stream-sonnet.sse"),
            Reply::status(400, "error-context.json"),
            Reply::status(400, "error-context.json"),
        ],
    );
    h.mock.route(
        "Write a title for the conversation",
        vec![Reply::ok("title.json")],
    );
    let (mut conversation, mut h) = h.close();
    // Within 80% of the context by td-agent's estimate: only the
    // provider refuses it.
    let old = read_two(&mut conversation, 120_000, 180_000);
    drop(conversation);
    h.reopen();
    // The turn read back, as the window hears it.
    let _ = h.turn();
    // With `auto_compact` off a refusal compacts nothing: the turn stops.
    h.setup(Client {
        auto_compact: false,
        ..Client::default()
    });
    h.say("Not yet.");
    let (events, outcome, _) = h.turn();
    assert!(outcome.contains("maximum context length"), "{outcome}");
    assert!(kinds(&events).iter().all(|k| *k != "compaction"));
    h.setup(Client::default());
    h.say("Go on.");
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied", "{}", h.said());
    assert_eq!(
        kinds(&events)
            .into_iter()
            .filter(|k| ["request", "compaction"].contains(k))
            .collect::<Vec<_>>(),
        ["request", "compaction", "request"]
    );
    let turns: Vec<String> = h
        .mock
        .requests()
        .into_iter()
        .filter(|r| r.text().contains("Go on."))
        .map(|r| r.text())
        .collect();
    assert_eq!(turns.len(), 2);
    assert_eq!(tool_results(&turns[0])[0], "o".repeat(120_000));
    assert!(tool_results(&turns[1])[0].contains(&format!("history_read from {old} reads it")));
    // Refused again with nothing left to prune: a summary is asked for,
    // and refused too, the turn stops and says both.
    h.say("And again.");
    let (events, outcome, _) = h.turn();
    assert!(outcome.contains("maximum context length"), "{outcome}");
    assert!(
        outcome.contains("could not be compacted: its summary request"),
        "{outcome}"
    );
    assert_eq!(
        kinds(&events)
            .into_iter()
            .filter(|k| ["request", "compaction"].contains(k))
            .collect::<Vec<_>>(),
        ["request", "compaction", "request"]
    );
}

/// A conversation whose log is a small old read and a large recent one,
/// past 80% of a 100,000 context that pruning cannot bring under it.
fn past_pruning(tag: &str, script: Vec<Reply>, summary: Vec<Reply>) -> (Harness, u64) {
    let h = Harness::new(tag, Role::Conversation, script);
    h.mock.route(
        "Write a title for the conversation",
        vec![Reply::ok("title.json")],
    );
    h.mock
        .route("This conversation is being compacted", summary);
    let (mut conversation, mut h) = h.close();
    // 2,500 tokens old, too few to prune; 65,000 recent.
    let old = read_two(&mut conversation, 10_000, 260_000);
    drop(conversation);
    let list = String::from_utf8(fixture("models.json"))
        .unwrap()
        .replace(r#""context_length":200000"#, r#""context_length":100000"#);
    Models::from_provider(list.as_bytes())
        .unwrap()
        .save(h.state.root())
        .unwrap();
    h.reopen();
    // The turn read back, as the window hears it.
    let _ = h.turn();
    h.setup(Client::default());
    (h, old)
}

/// When pruning leaves a request past `compact_at`, a handoff summary is
/// asked for, bounded to a tenth of the context and not streamed; the
/// turn's request then sends the notice, the summary and the carried
/// state, and the recent tail, and each request is a function of the
/// log (DESIGN.md §14).
#[test]
fn a_request_pruning_cannot_bring_under_compact_at_is_summarized() {
    let (mut h, _) = past_pruning(
        "compact-summary",
        vec![Reply::sse("stream-sonnet.sse")],
        vec![Reply::sse("compact-summary.sse")],
    );
    h.say("Go on.");
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied", "{}", h.said());
    let summary = events
        .iter()
        .find_map(|e| match &e.kind {
            Kind::Compaction {
                summary: Some(s), ..
            } => Some(s.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!((summary.from, summary.focus.as_deref()), (0, None));
    let requests = h.mock.requests();
    let asked = requests
        .iter()
        .find(|r| r.text().contains("This conversation is being compacted"))
        .unwrap()
        .text();
    let head = td_json::parse_slice(asked.as_bytes()).unwrap();
    assert_eq!(
        head.get("max_tokens").and_then(td_json::Json::as_u64),
        Some(10_000)
    );
    assert_eq!(
        head.get("stream").and_then(td_json::Json::as_bool),
        Some(true)
    );
    assert!(asked.contains(&"n".repeat(260_000)));
    let turn = requests
        .iter()
        .rfind(|r| r.text().contains("Go on.") && !r.text().contains("being compacted"))
        .unwrap()
        .text();
    assert!(turn.contains("SUMMARY: both files were read"), "{turn}");
    assert!(turn.contains("[The task: the conversation's first message"));
    assert!(!turn.contains(&"n".repeat(1000)));
    // Each request as the log rebuilds it.
    let (conversation, _) = h.close();
    let sent: Vec<String> = requests.iter().map(|r| r.text()).collect();
    for (at, event) in conversation.events().iter().enumerate() {
        if let Kind::Request {
            purpose: Purpose::Compact | Purpose::Turn,
            ..
        } = &event.kind
        {
            if event.seq < summary.tail {
                continue;
            }
            let body =
                td_agent::client::body(conversation.events(), at, conversation.prefix_file())
                    .unwrap();
            assert!(
                sent.contains(&body),
                "request {} rebuilt differs",
                event.seq
            );
        }
    }
}

/// A summary request refused stops the turn and says why; a summary
/// past what the context holds once carried stops it too, and nothing
/// is cut silently (DESIGN.md §14).
#[test]
fn a_compaction_that_fails_stops_the_turn() {
    let (mut h, _) = past_pruning(
        "compact-refused-summary",
        Vec::new(),
        vec![Reply::status(402, "error-402.json")],
    );
    h.say("Go on.");
    let (events, outcome, _) = h.turn();
    assert!(
        outcome.contains("could not be compacted: its summary request"),
        "{outcome}"
    );
    assert!(kinds(&events).contains(&"compaction"));
    // A summary of 300,000 bytes leaves the view past `compact_at`.
    let (mut h, _) = past_pruning(
        "compact-long-summary",
        Vec::new(),
        vec![summary_stream(&"s".repeat(300_000), "stop")],
    );
    h.say("Go on.");
    let (_, outcome, _) = h.turn();
    assert!(
        outcome.contains("compacted, which with its reply's"),
        "{outcome}"
    );
    assert!(outcome.contains("is still past compact_at"), "{outcome}");
    // Summarized, and refused for its context all the same: the turn
    // stops with the provider's words, summarized once.
    let (mut h, _) = past_pruning(
        "compact-summarized-refused",
        vec![
            Reply::status(400, "error-context.json"),
            Reply::status(400, "error-context.json"),
        ],
        vec![
            Reply::sse("compact-summary.sse"),
            Reply::sse("compact-summary.sse"),
        ],
    );
    h.say("Go on.");
    let (events, outcome, _) = h.turn();
    assert!(outcome.contains("maximum context length"), "{outcome}");
    let summaries = events
        .iter()
        .filter(|e| {
            matches!(
                e.kind,
                Kind::Compaction {
                    summary: Some(_),
                    ..
                }
            )
        })
        .count();
    assert_eq!(summaries, 1);
}

/// A streamed summary of `text`, finished with `finish`.
fn summary_stream(text: &str, finish: &str) -> Reply {
    let chunk = |content: &str, finish: Option<&str>| {
        let finish = finish.map_or("null".to_string(), |f| format!("\"{f}\""));
        format!(
            "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"role\":\"assistant\",\"content\":\"{content}\"}},\"finish_reason\":{finish}}}]}}\n\n"
        )
    };
    // In events within the stream reader's bound.
    let pieces: String = text
        .as_bytes()
        .chunks(50_000)
        .map(|c| chunk(std::str::from_utf8(c).unwrap(), None))
        .collect();
    let body = format!(
        "{pieces}{}data: {{\"choices\":[],\"usage\":{{\"prompt_tokens\":1,\"completion_tokens\":1,\"cost\":0.001}}}}\n\ndata: [DONE]\n\n",
        chunk("", Some(finish))
    );
    Reply::Http {
        status: 200,
        headers: vec![("content-type".into(), "text/event-stream".into())],
        body: body.into_bytes(),
    }
}

/// The tail is fitted within `compact_at`, as the request after the
/// summary is checked: kept tokens however many leave out a recent step
/// that would bring it back past (DESIGN.md §14).
#[test]
fn a_summarys_tail_is_fitted_within_compact_at() {
    let (mut h, _) = past_pruning(
        "compact-tail-within",
        vec![Reply::sse("stream-sonnet.sse")],
        vec![Reply::sse("compact-summary.sse")],
    );
    h.setup(Client {
        compact_keep_tokens: 1_000_000,
        ..Client::default()
    });
    h.say("Go on.");
    let (_, outcome, _) = h.turn();
    assert_eq!(outcome, "replied", "{}", h.said());
    let last = h.mock.requests().last().unwrap().text();
    assert!(!last.contains(&"n".repeat(260_000)));
}

/// An interrupt closes a summary's stream: the turn stops, the summary
/// is not in force, and its reservation is charged as a turn's request
/// interrupted is, since a provider may bill what it began (DESIGN.md
/// §14, §5).
#[test]
fn an_interrupt_during_a_summary_charges_it_and_stops() {
    let (mut h, _) = past_pruning(
        "compact-interrupted",
        Vec::new(),
        vec![Reply::sse("compact-summary.sse")
            .cut_after("both files were read")
            .tail(Tail::Open)],
    );
    h.say("Go on.");
    h.until_text();
    h.down(&Down::Interrupt);
    let (events, outcome, _) = h.turn();
    assert!(outcome.contains("could not be compacted"), "{outcome}");
    h.mock.wait_closed(1);
    let reserved = h.reserved.last().unwrap().1;
    assert!(reserved > 0);
    assert!(usage(&events).contains(&(reserved, Basis::Reserved)));
    let (conversation, _) = h.close();
    assert!(td_agent::compact::in_force(conversation.events()).is_none());
}

/// A summary that does not finish, cut short at its `max_tokens`, does
/// not stand: the turn stops and says so, and the view is as it was
/// (DESIGN.md §14).
#[test]
fn a_summary_cut_short_does_not_stand() {
    let (mut h, _) = past_pruning(
        "compact-cut-summary",
        Vec::new(),
        vec![summary_stream("SUMMARY: half", "length")],
    );
    h.say("Go on.");
    let (_, outcome, _) = h.turn();
    assert!(
        outcome.contains("could not be compacted: its summary did not finish (`length`)"),
        "{outcome}"
    );
    let (conversation, _) = h.close();
    assert!(td_agent::compact::in_force(conversation.events()).is_none());
}

/// A context refusal with nothing to prune summarizes, and the request
/// asked again with the summary is answered (DESIGN.md §14).
#[test]
fn a_context_refusal_with_nothing_to_prune_is_summarized_and_asked_again() {
    let h = Harness::new(
        "compact-refused-summarized",
        Role::Conversation,
        vec![
            Reply::status(400, "error-context.json"),
            Reply::sse("stream-sonnet.sse"),
        ],
    );
    h.mock.route(
        "Write a title for the conversation",
        vec![Reply::ok("title.json")],
    );
    h.mock.route(
        "This conversation is being compacted",
        vec![Reply::sse("compact-summary.sse")],
    );
    let (mut conversation, mut h) = h.close();
    // Small enough that nothing is old enough to prune.
    read_two(&mut conversation, 10_000, 20_000);
    drop(conversation);
    h.reopen();
    let _ = h.turn();
    h.setup(Client::default());
    h.say("Go on.");
    let (events, outcome, _) = h.turn();
    assert_eq!(outcome, "replied", "{}", h.said());
    assert_eq!(
        kinds(&events)
            .into_iter()
            .filter(|k| ["request", "compaction"].contains(k))
            .collect::<Vec<_>>(),
        ["request", "compaction", "request", "request"]
    );
    let last = h.mock.requests().last().unwrap().text();
    assert!(last.contains("SUMMARY: both files were read"), "{last}");
}

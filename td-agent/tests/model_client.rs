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
use td_agent::store::{Basis, Conversation, Event, Id, Kind, Purpose, Role, StateDir};

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
            heard: Vec::new(),
            stderr,
        };
        assert!(matches!(harness.next(), Up::Hello { .. }));
        harness
    }

    fn setup(&mut self, client: Client) {
        let down = Down::Setup {
            key: Ok(Secret::new(KEY.into())),
            client,
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
                } => return (call, title, details),
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
            Kind::Prefix { .. } => "prefix",
            Kind::Request { .. } => "request",
            Kind::Assistant { .. } => "assistant",
            Kind::Usage { .. } => "usage",
            Kind::Title { .. } => "title",
            Kind::Message { .. } => "message",
            Kind::ToolCall { .. } => "tool_call",
            Kind::ToolResult { .. } => "tool_result",
            Kind::Todo { .. } => "todo",
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
        client: Client::default(),
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

/// A workspace conversation's request names its workspace and carries
/// its tools; a read runs without asking, and a command waits for the
/// person, whose refusal is the call's answer. Without `./agent`'s
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
    });
    h.down(&Down::Decision { call, allow: false });
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
    h.down(&Down::Decision { call, allow: true });
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
    h.down(&Down::Decision { call, allow: true });
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
    h.down(&Down::Decision { call, allow: true });
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
        client: Client::default(),
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
    h.down(&Down::Decision { call, allow: false });
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
        h.down(&Down::Decision { call, allow });
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
            h.down(&Down::Decision { call, allow: true });
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

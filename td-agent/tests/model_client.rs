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

// The shared JSON module, to read the bodies the client sent.
#[path = "../src/json.rs"]
#[allow(dead_code, unused_macros, unused_imports)]
mod json;

use std::collections::BTreeMap;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use mock_fetch::{fixture, MockFetch, Reply};
use td_agent::config::Client;
use td_agent::cost::{Limits, ONE};
use td_agent::frame;
use td_agent::key::Secret;
use td_agent::models::Models;
use td_agent::protocol::{Down, Up};
use td_agent::store::{Basis, Conversation, Event, Id, Kind, Purpose, Role, StateDir};

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
    stderr: PathBuf,
}

impl Harness {
    fn new(tag: &str, role: Role, script: Vec<Reply>) -> Self {
        let root = std::env::temp_dir().join(format!(
            "td-agent-model-{tag}-{}-{}",
            std::process::id(),
            td_agent::store::random_hex(4).unwrap()
        ));
        std::fs::create_dir(&root).unwrap();
        let state = StateDir::at(root.join("state"));
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
        let (child, window) = spawn(&state, &id, Some(role), mock.runtime(), &stderr);
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

    /// What the process said up to its turn's end, reservations answered
    /// on the way: the events, and the turn's outcome and retry flag.
    fn turn(&mut self) -> (Vec<Event>, String, bool) {
        let mut events = Vec::new();
        let mut turns = Vec::new();
        loop {
            match self.next() {
                Up::Reserve { id, amount } => {
                    self.reserved.push((id, amount));
                    let refusal = match &self.day {
                        Day::Grant => None,
                        Day::Refuse(why) => Some(why.clone()),
                    };
                    self.down(&Down::Reservation { id, refusal });
                }
                Up::Spent { id, amount } => self.spent.push((id, amount)),
                Up::Event(event) => {
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
                _ => {}
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
    }
}

fn spawn(
    state: &StateDir,
    id: &Id,
    create: Option<Role>,
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
    fn walk(prefix: &str, value: &json::Json, out: &mut BTreeMap<String, String>) {
        let join = |key: &str| {
            if prefix.is_empty() {
                key.to_string()
            } else {
                format!("{prefix}.{key}")
            }
        };
        match value {
            json::Json::Obj(members) => {
                for (key, value) in members {
                    walk(&join(key), value, out);
                }
            }
            json::Json::Arr(items) => {
                for (i, value) in items.iter().enumerate() {
                    walk(&join(&i.to_string()), value, out);
                }
            }
            json::Json::Str(text) => {
                out.insert(prefix.to_string(), text.clone());
            }
            other => {
                out.insert(prefix.to_string(), other.to_string());
            }
        }
    }
    let value = json::parse(text).unwrap_or_else(|e| panic!("{e}: {text}"));
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

/// The sonnet reply's reasoning details, as the fixture's bytes carry
/// them.
fn sonnet_details() -> String {
    let text = String::from_utf8(fixture("completion-sonnet.json")).unwrap();
    let start = text.find("\"reasoning_details\": ").unwrap() + "\"reasoning_details\": ".len();
    let end = text[start..].find("\n        ]").unwrap() + start + "\n        ]".len();
    text[start..end].to_string()
}

#[test]
fn a_turn_is_sent_as_the_design_says_logged_whole_and_titled() {
    let mut h = Harness::new(
        "roundtrip",
        Role::Conversation,
        vec![Reply::ok("completion-sonnet.json"), Reply::ok("title.json")],
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
    assert!(!turn.stream);
    assert_eq!(turn.limit, Some(512 * 1024));
    let body = flat(&turn.text());
    assert_eq!(body["model"], "anthropic/claude-sonnet-5.5");
    assert_eq!(body["max_tokens"], "16384");
    assert_eq!(body["reasoning.effort"], "medium");
    assert_eq!(body["provider.require_parameters"], "true");
    assert_eq!(body["provider.data_collection"], "deny");
    assert_eq!(body["cache_control.type"], "ephemeral");
    assert_eq!(body["messages.0.role"], "system");
    assert_eq!(body["messages.1.content"], "What is a sparse checkout?");
    assert!(!body.contains_key("tools"), "no tools in this increment");
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
/// byte the first sent, reasoning details spliced back exactly as the
/// response carried them.
#[test]
fn the_next_request_replays_the_log_and_the_reasoning_byte_for_byte() {
    let mut h = Harness::new(
        "replay",
        Role::Orchestrator,
        vec![Reply::ok("completion-sonnet.json")],
    );
    h.setup(Client::default());
    h.say("What is a sparse checkout?");
    let (_, outcome, _) = h.turn();
    assert_eq!(outcome, "replied");
    let (_, mut h) = h.close();
    // A restart: a new process over the same log.
    let (child, window) = spawn(&h.state, &h.id, None, h.mock.runtime(), &h.stderr);
    h.child = child;
    h.window = window;
    assert!(matches!(h.next(), Up::Hello { .. }));
    for _ in 0..7 {
        assert!(matches!(h.next(), Up::Event(_)));
    }
    h.mock.then(vec![Reply::ok("completion-gemini.json")]);
    h.setup(Client::default());
    h.say("And after a base advances?");
    let (_, outcome, _) = h.turn();
    assert_eq!(outcome, "replied");
    let requests = h.mock.requests();
    let (first, second) = (requests[0].text(), requests[1].text());
    let kept = first.strip_suffix("]}").unwrap();
    assert!(
        second.starts_with(kept),
        "the second body does not begin with the first:\n{first}\n{second}"
    );
    let details = sonnet_details();
    assert!(
        second.contains(&format!(",\"reasoning_details\":{details}}}")),
        "{second}"
    );
    flat(&second);
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
        .then(vec![limited(), Reply::ok("completion-sonnet.json")]);
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
    h.mock.then(vec![Reply::ok("completion-sonnet.json")]);
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
        orchestrator_model: "openrouter/auto".into(),
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
        vec![Reply::ok("completion-sonnet.json")],
    );
    h.setup(Client {
        orchestrator_model: "openrouter/auto".into(),
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
        vec![Reply::ok("completion-sonnet.json")],
    );
    h.setup(Client {
        orchestrator_model: "meta-llama/llama-4-small".into(),
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
    let (child, window) = spawn(&h.state, &h.id, None, h.mock.runtime(), &h.stderr);
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

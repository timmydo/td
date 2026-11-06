//! The window's conversation processes as real processes of the built
//! program (DESIGN.md §2): one per open conversation over a framed
//! socketpair, restarted from its log when it fails, exiting when its
//! socketpair closes, and each the only writer of its directory; and one
//! window process per state directory.
#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use td_agent::config::{Client, Mode};
use td_agent::frame;
use td_agent::protocol::{Down, Up};
use td_agent::store::{Conversation, Id, Kind, Role, StateDir};
use td_agent::supervisor::{Supervisor, Update, MAX_RESTARTS};
use td_agent::ui::Request;
use td_agent::view::View;
use td_agent::window_wire::Showing;
use td_agent::workspace::Workspace;

/// The settings with no key: each turn ends at once, saying so.
fn keyless() -> Down {
    Down::Setup {
        key: Err("no API key".into()),
        client: Box::default(),
    }
}

const PROGRAM: &str = env!("CARGO_BIN_EXE_td-agent");
// Every wait polls and returns once its condition holds, so the bound costs
// a passing run nothing; it is wide for a host loaded by parallel checks.
const TIMEOUT: Duration = Duration::from_secs(30);

struct Scratch(PathBuf);
impl Scratch {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "td-agent-process-{tag}-{}-{}",
            std::process::id(),
            td_agent::store::random_hex(4).unwrap()
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn state(&self) -> StateDir {
        let state = StateDir::at(self.0.join("td-agent"));
        state.ensure().unwrap();
        state
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Polls until `done` holds of everything heard so far, which it returns.
fn until(supervisor: &mut Supervisor, heard: &mut Vec<Update>, done: impl Fn(&[Update]) -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    while !done(heard) {
        assert!(Instant::now() < deadline, "heard only {heard:#?}");
        heard.extend(supervisor.poll().into_iter().map(|(_, update)| update));
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn events(heard: &[Update]) -> Vec<u64> {
    heard
        .iter()
        .filter_map(|u| match u {
            Update::Up(Up::Event(e)) => Some(e.seq),
            _ => None,
        })
        .collect()
}

fn delivered(heard: &[Update]) -> usize {
    heard
        .iter()
        .filter(|u| matches!(u, Update::Up(Up::Delivered { .. })))
        .count()
}

#[test]
fn a_killed_conversation_process_is_restarted_from_its_log() {
    let scratch = Scratch::new("restart");
    let state = scratch.state();
    let id = Id::random().unwrap();
    let mut supervisor = Supervisor::new(PROGRAM.into(), state.root().to_path_buf(), keyless());
    supervisor
        .open(id.clone(), Some(Role::Orchestrator))
        .unwrap();
    let mut heard = Vec::new();
    supervisor.send("first".into(), Vec::new()).unwrap();
    until(&mut supervisor, &mut heard, |h| {
        delivered(h) == 1 && events(h).len() == 3
    });
    assert!(matches!(heard.first(), Some(Update::Up(Up::Hello { .. }))));
    let first_pid = supervisor.pid().unwrap();
    // A crash: the window sees the socketpair close and starts it again,
    // and the new process replays the log, the same three events.
    supervisor.kill();
    heard.clear();
    until(&mut supervisor, &mut heard, |h| events(h).len() == 3);
    assert!(
        matches!(heard.first(), Some(Update::Restarting { .. })),
        "{heard:#?}"
    );
    assert!(matches!(heard.get(1), Some(Update::Up(Up::Hello { .. }))));
    assert_eq!(events(&heard), [1, 2, 3]);
    assert_ne!(supervisor.pid(), Some(first_pid));
    // The restarted process goes on from where the log ends.
    heard.clear();
    supervisor.send("second".into(), Vec::new()).unwrap();
    until(&mut supervisor, &mut heard, |h| events(h).len() == 3);
    assert_eq!(events(&heard), [4, 5, 6]);
    drop(supervisor);
    let (conversation, load) = Conversation::open(&state, &id, None, Duration::ZERO).unwrap();
    assert!(load.interrupted.is_empty());
    let texts: Vec<&str> = conversation
        .events()
        .iter()
        .filter_map(|e| match &e.kind {
            Kind::User { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, ["first", "second"]);
}

#[test]
fn a_message_sent_just_before_moving_away_is_delivered_on_reopening() {
    let scratch = Scratch::new("away");
    let state = scratch.state();
    let (a, b) = (Id::random().unwrap(), Id::random().unwrap());
    let mut supervisor = Supervisor::new(PROGRAM.into(), state.root().to_path_buf(), keyless());
    supervisor
        .open(a.clone(), Some(Role::Orchestrator))
        .unwrap();
    // Sent while A's process is still starting, then away at once: the
    // process may go before it reads the message.
    supervisor.send("kept".into(), Vec::new()).unwrap();
    // A message the open one has yet to take is something under way.
    assert!(supervisor.working());
    // Queued, the window shows, until the process takes it: B's own
    // queue is its, empty.
    assert_eq!(supervisor.queued(), ["kept"]);
    supervisor.open(b, Some(Role::Conversation)).unwrap();
    assert!(supervisor.queued().is_empty());
    supervisor.open(a.clone(), None).unwrap();
    let mut heard = Vec::new();
    until(&mut supervisor, &mut heard, |h| delivered(h) == 1);
    assert!(supervisor.queued().is_empty());
    drop(supervisor);
    let (conversation, _) = Conversation::open(&state, &a, None, Duration::ZERO).unwrap();
    let texts: Vec<&str> = conversation
        .events()
        .iter()
        .filter_map(|e| match &e.kind {
            Kind::User { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, ["kept"], "logged once, whichever process logged it");
}

/// A conversation created in a workspace is told it by the window and
/// records it; each start clears the specs an earlier process left.
#[test]
fn a_workspace_reaches_the_conversation_and_stale_specs_are_cleared() {
    let scratch = Scratch::new("workspace");
    let state = scratch.state();
    let id = Id::random().unwrap();
    let mut supervisor = Supervisor::new(PROGRAM.into(), state.root().to_path_buf(), keyless());
    let template = Id::random().unwrap();
    supervisor
        .create(id.clone(), Role::Conversation, Workspace::Scratch)
        .unwrap();
    supervisor
        .create(
            template.clone(),
            Role::Conversation,
            Workspace::Template("my notes".into()),
        )
        .unwrap();
    let recorded = |id: &Id| {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let (metas, _) = state.list();
            if let Some(meta) = metas.into_iter().find(|meta| &meta.id == id) {
                break meta.workspace;
            }
            assert!(Instant::now() < deadline, "no meta was written");
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    assert_eq!(
        recorded(&template),
        Some(Workspace::Template("my notes".into()))
    );
    assert_eq!(recorded(&id), Some(Workspace::Scratch));
    drop(supervisor);
    let specs = td_agent::workspace::jail_dir(&state, &id).join("specs");
    std::fs::create_dir_all(&specs).unwrap();
    std::fs::write(specs.join("spec-stale"), "format=1\n").unwrap();
    let mut supervisor = Supervisor::new(PROGRAM.into(), state.root().to_path_buf(), keyless());
    supervisor.open(id, None).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while specs.exists() {
        assert!(Instant::now() < deadline, "the stale spec stayed");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A repository workspace's record, as the window makes it.
fn repositories(id: &Id, base: &Path) -> Workspace {
    let template = td_agent::config::Template {
        mcp: Vec::new(),
        system: false,
        network: None,
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
        id,
        &base.join("data"),
        &base.join("trees"),
        &admitted,
        0,
    )
    .unwrap();
    Workspace::Repositories(made)
}

/// A new repository conversation asks the window for its store, once per
/// remote and with its bases, is kept while it prepares though left, and
/// says in its log when the answer is a refusal; nothing is recorded
/// prepared (DESIGN.md §7).
#[test]
fn a_repository_conversation_asks_for_its_store_and_says_a_refusal() {
    let scratch = Scratch::new("repositories");
    let state = scratch.state();
    let id = Id::random().unwrap();
    let mut supervisor = Supervisor::new(PROGRAM.into(), state.root().to_path_buf(), keyless());
    let workspace = repositories(&id, &scratch.0);
    supervisor
        .create(id.clone(), Role::Conversation, workspace.clone())
        .unwrap();
    let mut heard = Vec::new();
    until(&mut supervisor, &mut heard, |heard| {
        heard
            .iter()
            .any(|u| matches!(u, Update::Up(Up::Fetch { .. })))
    });
    let asked: Vec<&Update> = heard
        .iter()
        .filter(|u| matches!(u, Update::Up(Up::Fetch { .. })))
        .collect();
    assert_eq!(
        asked,
        [&Update::Up(Up::Fetch {
            remote: "https://example.org/a/td".into(),
            bases: vec!["main".into()],
        })]
    );
    // Switched away from while it prepares: its process is kept.
    let other = Id::random().unwrap();
    supervisor
        .create(other.clone(), Role::Conversation, Workspace::Scratch)
        .unwrap();
    let kept = Instant::now() + Duration::from_secs(3);
    while Instant::now() < kept {
        heard.extend(supervisor.poll().into_iter().map(|(_, update)| update));
        assert!(supervisor.running(&id), "retired while preparing");
        std::thread::sleep(Duration::from_millis(20));
    }
    supervisor.answer(
        &id,
        &Down::Fetched {
            remote: "https://example.org/a/td".into(),
            result: Err("the remote is not admitted".into()),
        },
    );
    until(&mut supervisor, &mut heard, |heard| {
        heard.iter().any(|u| {
            matches!(u, Update::Up(Up::Event(e)) if matches!(&e.kind,
                Kind::Notification { text, .. } if text.contains("could not be prepared: the remote is not admitted")))
        })
    });
    // Done preparing, it is retired as any left conversation is.
    let deadline = Instant::now() + TIMEOUT;
    while supervisor.running(&id) {
        assert!(Instant::now() < deadline, "kept once prepared");
        supervisor.poll();
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(supervisor);
    let (conversation, _) = Conversation::open(&state, &id, None, Duration::from_secs(5)).unwrap();
    assert_eq!(conversation.meta().workspace, Some(workspace));
    assert!(conversation.meta().prepared.is_empty());
}

/// A turn's first request waits for the window's answer to each store
/// the process asked for: a message sent before it is held, its turn
/// ending only once the answer comes. The project instructions the
/// answer carries are recorded before the checkout, which fails here,
/// having no jail (DESIGN.md §7, §13).
#[test]
fn a_repository_conversations_first_turn_waits_for_its_stores() {
    let scratch = Scratch::new("instructions");
    let state = scratch.state();
    let id = Id::random().unwrap();
    // A key, so the turn waits; no td-fetch socket, so its request then
    // fails here and reaches nothing.
    let keyed = Down::Setup {
        key: Ok(td_agent::key::Secret::new("sk-or-v1-test".into())),
        client: Box::default(),
    };
    let nowhere = scratch.0.join("run");
    std::fs::create_dir(&nowhere).unwrap();
    let mut supervisor = Supervisor::new(PROGRAM.into(), state.root().to_path_buf(), keyed)
        .env("XDG_RUNTIME_DIR", &nowhere);
    supervisor
        .create(
            id.clone(),
            Role::Conversation,
            repositories(&id, &scratch.0),
        )
        .unwrap();
    let mut heard = Vec::new();
    until(&mut supervisor, &mut heard, |heard| {
        heard
            .iter()
            .any(|u| matches!(u, Update::Up(Up::Fetch { .. })))
    });
    supervisor.send("hello".into(), Vec::new()).unwrap();
    let finished = |heard: &[Update]| {
        heard.iter().any(
            |u| matches!(u, Update::Up(Up::Event(e)) if matches!(e.kind, Kind::Finished { .. })),
        )
    };
    until(&mut supervisor, &mut heard, |heard| delivered(heard) == 1);
    let held = Instant::now() + Duration::from_secs(1);
    while Instant::now() < held {
        heard.extend(supervisor.poll().into_iter().map(|(_, update)| update));
        assert!(!finished(&heard), "the turn did not wait");
        std::thread::sleep(Duration::from_millis(20));
    }
    supervisor.answer(
        &id,
        &Down::Fetched {
            remote: "https://example.org/a/td".into(),
            result: Ok(td_agent::protocol::Fetched {
                rules: vec![td_agent::rules::Read::Absent],
                identity: td_agent::repo::Identity::default(),
                ids: vec!["a".repeat(40)],
                instructions: vec![td_agent::repo::Instructions::Found {
                    name: "AGENTS.md".into(),
                    text: "Run make.\n".into(),
                }],
            }),
        },
    );
    until(&mut supervisor, &mut heard, finished);
    drop(supervisor);
    let (conversation, _) = Conversation::open(&state, &id, None, Duration::from_secs(5)).unwrap();
    let recorded = conversation.instructions();
    assert_eq!(recorded.len(), 1, "{recorded:?}");
    assert!(conversation.meta().prepared.is_empty());
}

/// A repository conversation whose workspace went with its archive asks
/// for no store once unarchived: its turn runs at once, its environment
/// saying the worktrees were removed and its tools refused (DESIGN.md
/// §7).
#[test]
fn a_conversation_whose_workspace_went_with_its_archive_prepares_nothing() {
    let scratch = Scratch::new("removed");
    let state = scratch.state();
    let id = Id::random().unwrap();
    let mut supervisor = Supervisor::new(PROGRAM.into(), state.root().to_path_buf(), keyless());
    supervisor
        .create(
            id.clone(),
            Role::Conversation,
            repositories(&id, &scratch.0),
        )
        .unwrap();
    let mut heard = Vec::new();
    until(&mut supervisor, &mut heard, |heard| {
        heard
            .iter()
            .any(|u| matches!(u, Update::Up(Up::Fetch { .. })))
    });
    drop(supervisor);
    // Archived with its workspace, then brought back: still removed.
    state
        .set_archived(&id, true, true, Duration::from_secs(5))
        .unwrap();
    state
        .set_archived(&id, false, false, Duration::from_secs(5))
        .unwrap();
    assert_eq!(state.removed(&id), Ok(true));
    // No limits, so the turn reaches its prefix; it then asks the window
    // to reserve, which nothing answers, so nothing is sent.
    let keyed = Down::Setup {
        key: Ok(td_agent::key::Secret::new("sk-or-v1-test".into())),
        client: Box::new(Client {
            limits: td_agent::cost::Limits {
                turn: None,
                conversation: None,
                day: None,
            },
            ..Client::default()
        }),
    };
    let nowhere = scratch.0.join("run");
    std::fs::create_dir(&nowhere).unwrap();
    let mut supervisor = Supervisor::new(PROGRAM.into(), state.root().to_path_buf(), keyed)
        .env("XDG_RUNTIME_DIR", &nowhere);
    supervisor.open(id.clone(), None).unwrap();
    heard.clear();
    // An answer asked for before the archive, come late, is let go: the
    // window told it is done with, nothing laid out or recorded.
    supervisor.answer(
        &id,
        &Down::Fetched {
            remote: "https://example.org/a/td".into(),
            result: Ok(td_agent::protocol::Fetched {
                rules: vec![td_agent::rules::Read::Absent],
                identity: td_agent::repo::Identity::default(),
                ids: vec!["a".repeat(40)],
                instructions: vec![td_agent::repo::Instructions::Absent],
            }),
        },
    );
    until(&mut supervisor, &mut heard, |heard| {
        heard
            .iter()
            .any(|u| matches!(u, Update::Up(Up::Prepared { .. })))
    });
    supervisor.send("hello".into(), Vec::new()).unwrap();
    until(&mut supervisor, &mut heard, |heard| {
        heard.iter().any(|u| {
            matches!(u, Update::Up(Up::Event(e))
            if matches!(&e.kind, Kind::Prefix { text }
                if text.contains("were removed when the person archived it")))
        })
    });
    assert!(
        !heard
            .iter()
            .any(|u| matches!(u, Update::Up(Up::Fetch { .. }))),
        "asked for a store"
    );
    drop(supervisor);
    let (conversation, _) = Conversation::open(&state, &id, None, Duration::from_secs(5)).unwrap();
    assert!(conversation.meta().prepared.is_empty());
    assert!(conversation.instructions().is_empty());
    assert!(!scratch.0.join("data").join("ws").exists());
}

/// A prepared repository's process asks where its bases are rather than
/// for its store (DESIGN.md §7, Keeping current): told a commit it
/// recorded, it does nothing; told another, it sets the refs, which
/// fails here with no jail and is said once however often it is told.
#[test]
fn a_prepared_repository_follows_its_bases_and_says_a_failure_once() {
    let scratch = Scratch::new("heads");
    let state = scratch.state();
    let id = Id::random().unwrap();
    let mut supervisor = Supervisor::new(PROGRAM.into(), state.root().to_path_buf(), keyless());
    // Two remotes, so one's failures cannot hide the other's.
    let (one, two) = (
        "https://example.org/a/td".to_string(),
        "https://example.org/a/docs".to_string(),
    );
    let template = td_agent::config::Template {
        mcp: Vec::new(),
        system: false,
        network: None,
        name: "td".into(),
        repos: [&one, &two]
            .iter()
            .map(|remote| td_agent::config::Repo {
                remote: remote.to_string(),
                base: "main".into(),
                branch: "agent".into(),
                sparse: None,
            })
            .collect(),
        shared: None,
    };
    let made = td_agent::workspace::repositories(
        &template,
        &id,
        &scratch.0.join("data"),
        &scratch.0.join("trees"),
        &[td_agent::git::Admission::parse("example.org").unwrap()],
        0,
    )
    .unwrap();
    supervisor
        .create(
            id.clone(),
            Role::Conversation,
            Workspace::Repositories(made.clone()),
        )
        .unwrap();
    let mut heard = Vec::new();
    until(&mut supervisor, &mut heard, |heard| {
        heard
            .iter()
            .any(|u| matches!(u, Update::Up(Up::Fetch { .. })))
    });
    drop(supervisor);
    let (a, b) = ("a".repeat(40), "b".repeat(40));
    let (mut conversation, _) =
        Conversation::open(&state, &id, None, Duration::from_secs(5)).unwrap();
    for entry in &made.entries {
        conversation.set_prepared(&entry.repository).unwrap();
    }
    conversation
        .set_tracked(&one, &[("main".into(), a.clone())])
        .unwrap();
    drop(conversation);
    let mut supervisor = Supervisor::new(PROGRAM.into(), state.root().to_path_buf(), keyless());
    supervisor.open(id.clone(), None).unwrap();
    heard.clear();
    let asked = |heard: &[Update]| -> Vec<String> {
        heard
            .iter()
            .filter_map(|u| match u {
                Update::Up(Up::Heads { remote, bases }) if *bases == ["main"] => {
                    Some(remote.clone())
                }
                _ => None,
            })
            .collect()
    };
    until(&mut supervisor, &mut heard, |heard| asked(heard).len() == 2);
    assert_eq!(asked(&heard), [one.clone(), two.clone()]);
    assert!(!heard
        .iter()
        .any(|u| matches!(u, Update::Up(Up::Fetch { .. }))));
    let told = |remote: &str, id: &str| Down::Heads {
        remote: remote.to_string(),
        bases: vec!["main".into()],
        ids: vec![id.to_string()],
    };
    // Either kind, marked, so news or a failure given to the model shows.
    let notices = |heard: &[Update]| -> Vec<String> {
        heard
            .iter()
            .filter_map(|u| match u {
                Update::Up(Up::Event(e)) => match &e.kind {
                    Kind::Notice { text } => Some(text.clone()),
                    Kind::Notification { text, .. } => Some(format!("notification: {text}")),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    };
    // Something logged after what is told, so all of it has been heard.
    let paused = |paused: bool| {
        move |heard: &[Update]| {
            heard
                .iter()
                .any(|u| matches!(u, Update::Up(Up::Event(e)) if e.kind == Kind::Pause { paused }))
        }
    };
    supervisor.answer(&id, &told(&one, &a));
    supervisor.answer(&id, &Down::Pause { paused: true });
    until(&mut supervisor, &mut heard, paused(true));
    assert!(notices(&heard).is_empty(), "{:?}", notices(&heard));
    // Setting them fails here, with no jail: each remote's failure is
    // said once, though they come in turn.
    supervisor.answer(&id, &told(&one, &b));
    supervisor.answer(&id, &told(&two, &b));
    supervisor.answer(&id, &told(&one, &b));
    supervisor.answer(&id, &told(&two, &b));
    supervisor.answer(&id, &Down::Pause { paused: false });
    until(&mut supervisor, &mut heard, paused(false));
    let notices = notices(&heard);
    assert_eq!(notices.len(), 2, "{notices:?}");
    for (notice, remote) in notices.iter().zip([&one, &two]) {
        // A notice, the human's to mend, not the model's news.
        assert!(
            notice.starts_with(&format!(
                "the remote-tracking refs of {remote} could not be set"
            )),
            "{notices:?}"
        );
    }
    drop(supervisor);
    let (conversation, _) = Conversation::open(&state, &id, None, Duration::from_secs(5)).unwrap();
    assert_eq!(
        conversation.meta().tracked.first().map(|t| t.id.as_str()),
        Some(a.as_str())
    );
}

/// An interrupt ends a turn waiting for its stores, as one to be asked
/// again, whether it came with the message or after.
#[test]
fn an_interrupt_ends_a_turn_waiting_for_its_stores() {
    let scratch = Scratch::new("await-interrupt");
    let state = scratch.state();
    let id = Id::random().unwrap();
    let keyed = Down::Setup {
        key: Ok(td_agent::key::Secret::new("sk-or-v1-test".into())),
        client: Box::default(),
    };
    let nowhere = scratch.0.join("run");
    std::fs::create_dir(&nowhere).unwrap();
    let mut supervisor = Supervisor::new(PROGRAM.into(), state.root().to_path_buf(), keyed)
        .env("XDG_RUNTIME_DIR", &nowhere);
    supervisor
        .create(
            id.clone(),
            Role::Conversation,
            repositories(&id, &scratch.0),
        )
        .unwrap();
    let mut heard = Vec::new();
    until(&mut supervisor, &mut heard, |heard| {
        heard
            .iter()
            .any(|u| matches!(u, Update::Up(Up::Fetch { .. })))
    });
    let ended = |heard: &[Update]| {
        heard
            .iter()
            .filter(|u| {
                matches!(u, Update::Up(Up::Event(e)) if matches!(&e.kind,
                    Kind::Finished { outcome, retry: true, .. }
                        if outcome.contains("waiting for the workspace's project instructions")))
            })
            .count()
    };
    // Sent with the message, the interrupt is queued behind it.
    supervisor.send("hello".into(), Vec::new()).unwrap();
    supervisor.interrupt().unwrap();
    until(&mut supervisor, &mut heard, |heard| ended(heard) == 1);
    // Sent while it waits.
    supervisor.send("again".into(), Vec::new()).unwrap();
    until(&mut supervisor, &mut heard, |heard| delivered(heard) == 2);
    std::thread::sleep(Duration::from_millis(200));
    supervisor.interrupt().unwrap();
    until(&mut supervisor, &mut heard, |heard| ended(heard) == 2);
}

#[test]
fn a_conversation_that_keeps_failing_is_left_failed() {
    let scratch = Scratch::new("failing");
    let state = scratch.state();
    let id = Id::random().unwrap();
    drop(Conversation::open(&state, &id, Some(Role::Conversation), Duration::ZERO).unwrap());
    // A log corrupt inside: every start refuses it.
    std::fs::write(state.conversation(&id).join("log"), "garbage\n").unwrap();
    let mut supervisor = Supervisor::new(PROGRAM.into(), state.root().to_path_buf(), keyless());
    supervisor.open(id, None).unwrap();
    let mut heard = Vec::new();
    until(&mut supervisor, &mut heard, |h| {
        h.iter().any(|u| matches!(u, Update::Failed { .. }))
    });
    let restarts = heard
        .iter()
        .filter(|u| matches!(u, Update::Restarting { .. }))
        .count();
    assert_eq!(restarts, MAX_RESTARTS as usize, "{heard:#?}");
    assert!(supervisor.send("x".into(), Vec::new()).is_err());
    assert!(supervisor.poll().is_empty(), "no restart after failing");
}

/// The program's conversation personality, its socketpair in hand.
fn conversation(state: &Path, id: &Id, create: Option<Role>) -> (std::process::Child, UnixStream) {
    let (ours, theirs) = UnixStream::pair().unwrap();
    let mut command = Command::new(PROGRAM);
    command
        .args(["conversation", id.as_str(), "--state-dir"])
        .arg(state)
        // Not the developer's own skills.
        .env("XDG_CONFIG_HOME", state.join("config"));
    if let Some(role) = create {
        command.args(["--create", role.word()]);
    }
    let child = command
        .stdin(Stdio::from(OwnedFd::from(theirs.try_clone().unwrap())))
        .stdout(Stdio::from(OwnedFd::from(theirs)))
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    (child, ours)
}

fn wait(child: &mut std::process::Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(Instant::now() < deadline, "the process did not exit");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_conversation_process_exits_when_its_socketpair_closes() {
    let scratch = Scratch::new("closes");
    let state = scratch.state();
    let id = Id::random().unwrap();
    let (mut child, mut ours) = conversation(state.root(), &id, Some(Role::Conversation));
    let hello = frame::read(&mut ours).unwrap().unwrap();
    assert!(matches!(Up::decode(&hello).unwrap(), Up::Hello { .. }));
    drop(ours);
    assert!(wait(&mut child).success());
}

#[test]
fn a_second_process_for_one_conversation_is_refused() {
    let scratch = Scratch::new("writer");
    let state = scratch.state();
    let id = Id::random().unwrap();
    let (mut first, mut ours) = conversation(state.root(), &id, Some(Role::Conversation));
    frame::read(&mut ours).unwrap().unwrap();
    let (mut second, _theirs) = conversation(state.root(), &id, None);
    let status = wait(&mut second);
    assert!(!status.success());
    let mut stderr = String::new();
    std::io::Read::read_to_string(&mut second.stderr.take().unwrap(), &mut stderr).unwrap();
    assert!(stderr.contains("already has its writer"), "{stderr}");
    drop(ours);
    assert!(wait(&mut first).success());
}

#[test]
fn a_second_broker_is_refused_while_the_first_holds_the_lock() {
    let scratch = Scratch::new("window");
    let state = scratch.state();
    let _held = state.lock_window().unwrap();
    let output = Command::new(PROGRAM)
        .arg("serve")
        .env_clear()
        .env("HOME", &scratch.0)
        .env("XDG_STATE_HOME", &scratch.0)
        .env("XDG_CONFIG_HOME", scratch.0.join("config"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("another td-agent serve"), "{stderr}");
}

#[test]
fn a_relative_control_socket_and_an_unknown_key_are_refused() {
    let scratch = Scratch::new("refusals");
    let output = Command::new(PROGRAM)
        .args(["--control-socket", "relative"])
        .env_clear()
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8(output.stderr)
        .unwrap()
        .contains("not an absolute path"));
    let config = scratch.0.join("config/td-agent");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("config"), "limits = 1\n").unwrap();
    // The broker reads the configuration; `attach`, which starts one, says what
    // it said, and without waiting to see whether it will listen.
    let began = Instant::now();
    let output = Command::new(PROGRAM)
        .arg("attach")
        .env_clear()
        .env("HOME", &scratch.0)
        .env("XDG_STATE_HOME", &scratch.0)
        .env("XDG_CONFIG_HOME", scratch.0.join("config"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("`limits`"), "{stderr}");
    assert!(began.elapsed() < Duration::from_secs(15));
}

/// `td-agent attach`, as a far window would run it, and the connection through it.
fn attach(scratch: &Scratch) -> (std::process::Child, td_agent::peer::Peer) {
    let (ours, theirs) = UnixStream::pair().unwrap();
    let child = Command::new(PROGRAM)
        .arg("attach")
        .env_clear()
        .env("HOME", &scratch.0)
        .env("XDG_STATE_HOME", &scratch.0)
        .env("XDG_CONFIG_HOME", scratch.0.join("config"))
        .stdin(Stdio::from(OwnedFd::from(theirs.try_clone().unwrap())))
        .stdout(Stdio::from(OwnedFd::from(theirs)))
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    (child, claimed(td_agent::peer::Peer::over(ours).unwrap()))
}

/// A far window's first word, as one a person asked for.
fn claimed(window: td_agent::peer::Peer) -> td_agent::peer::Peer {
    claim(&window, Id::random().unwrap().as_str(), true);
    window
}

fn claim(window: &td_agent::peer::Peer, token: &str, asked: bool) {
    let claim = Request::Claim {
        token: token.to_string(),
        asked,
    };
    assert!(window.send(td_agent::window_wire::encode_asked(
        &Showing::Nothing,
        &claim
    )));
}

/// A broker with no fetch service says so before anyone sends it anything.
#[test]
fn a_broker_with_no_fetch_service_says_so_at_once() {
    let scratch = Scratch::new("no-fetch");
    let (_child, window) = attach(&scratch);
    let told = told_until(
        &window,
        |view| matches!(view, View::Note(said) if said.contains("no fetch service")),
    );
    assert!(!told.is_empty());
    let log = std::fs::read_to_string(scratch.0.join("td-agent").join("serve.log")).unwrap();
    assert!(log.contains("no fetch service: XDG_RUNTIME_DIR"), "{log}");
}

/// One yet to say which it is, when the broker stops, is told too: it would
/// otherwise take that for a link that failed, and start another.
#[test]
fn a_stop_is_told_to_a_window_that_has_not_yet_said_which_it_is() {
    let scratch = Scratch::new("stop-claiming");
    let (ours, theirs) = UnixStream::pair().unwrap();
    let mut child = Command::new(PROGRAM)
        .arg("attach")
        .env_clear()
        .env("HOME", &scratch.0)
        .env("XDG_STATE_HOME", &scratch.0)
        .env("XDG_CONFIG_HOME", scratch.0.join("config"))
        .stdin(Stdio::from(OwnedFd::from(theirs.try_clone().unwrap())))
        .stdout(Stdio::from(OwnedFd::from(theirs)))
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let window = td_agent::peer::Peer::over(ours).unwrap();
    let output = Command::new(PROGRAM)
        .args(["serve", "--stop"])
        .env_clear()
        .env("HOME", &scratch.0)
        .env("XDG_STATE_HOME", &scratch.0)
        .env("XDG_CONFIG_HOME", scratch.0.join("config"))
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let told = told_until(&window, |view| matches!(view, View::Bye(_)));
    assert!(matches!(told.last(), Some(View::Bye(said)) if said.contains("stopped")));
    drop(window);
    assert!(child.wait().unwrap().success());
}

/// A window trying again unasked takes back only its own dead connection; one a
/// person asked for takes over from any.
#[test]
fn only_a_window_asked_for_puts_out_another() {
    let scratch = Scratch::new("claims");
    let join = |token: &str, asked: bool| {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let child = Command::new(PROGRAM)
            .arg("attach")
            .env_clear()
            .env("HOME", &scratch.0)
            .env("XDG_STATE_HOME", &scratch.0)
            .env("XDG_CONFIG_HOME", scratch.0.join("config"))
            .stdin(Stdio::from(OwnedFd::from(theirs.try_clone().unwrap())))
            .stdout(Stdio::from(OwnedFd::from(theirs)))
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let window = td_agent::peer::Peer::over(ours).unwrap();
        claim(&window, token, asked);
        (child, window)
    };
    let bye = |window: &td_agent::peer::Peer, why: &str| {
        let told = told_until(window, |view| matches!(view, View::Bye(_)));
        assert!(
            matches!(told.last(), Some(View::Bye(said)) if said.contains(why)),
            "{told:#?}"
        );
    };
    let told = |window: &td_agent::peer::Peer| {
        told_until(window, |view| matches!(view, View::SetShare(_)));
    };
    // With no window there, one trying unasked is as good as any.
    let (mut a_child, a) = join("a", false);
    told(&a);
    // Another's unasked try is sent away, and the first goes on.
    let (mut b_child, b) = join("b", false);
    bye(&b, "another window is open");
    drop(b);
    assert!(b_child.wait().unwrap().success());
    assert!(a.send(td_agent::window_wire::encode_asked(
        &Showing::Nothing,
        &Request::Redraw
    )));
    // Its own, after its link died with the broker unaware, takes it back.
    let (mut again_child, again) = join("a", false);
    told(&again);
    bye(&a, "taken over");
    drop(a);
    assert!(a_child.wait().unwrap().success());
    // One a person asked for takes it from anyone.
    let (mut c_child, c) = join("c", true);
    told(&c);
    bye(&again, "taken over");
    drop(again);
    assert!(again_child.wait().unwrap().success());
    drop(c);
    assert!(c_child.wait().unwrap().success());
}

/// What the broker says until `seen` holds of something it said.
fn told_until(peer: &td_agent::peer::Peer, seen: impl Fn(&View) -> bool) -> Vec<View> {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut told = Vec::new();
    loop {
        assert!(Instant::now() < deadline, "not told: {told:#?}");
        match peer.recv().unwrap() {
            Some(bytes) => {
                let view = td_agent::window_wire::decode_view(&bytes).unwrap();
                let done = seen(&view);
                told.push(view);
                if done {
                    return told;
                }
            }
            None => std::thread::sleep(Duration::from_millis(5)),
        }
    }
}

std::thread_local! {
    /// What the window `ask` stands for is showing.
    static SHOWN: std::cell::RefCell<Option<Id>> = const { std::cell::RefCell::new(None) };
}

fn ask(peer: &td_agent::peer::Peer, request: &Request) {
    let shown = SHOWN.with(|shown| shown.borrow().clone());
    let showing = shown.map_or(Showing::Nothing, Showing::Conversation);
    assert!(peer.send(td_agent::window_wire::encode_asked(&showing, request)));
}

/// A window at the far end of `td-agent attach` is told all there is, starts a
/// conversation, sets a prompt and turns auto mode on; a second takes over from
/// it; and with
/// none left and nothing under way the broker ends and takes its sockets.
#[test]
fn a_far_window_attaches_and_a_second_takes_over_and_the_broker_ends_with_the_last() {
    // Told why, so that it does not take this for a link that failed.
    fn let_go(window: &td_agent::peer::Peer, why: &str) {
        let told = told_until(window, |view| matches!(view, View::Bye(_)));
        assert!(
            matches!(told.last(), Some(View::Bye(said)) if said.contains(why)),
            "{told:#?}"
        );
    }
    let scratch = Scratch::new("attach");
    let state = scratch.state();
    let made = Id::random().unwrap();
    Conversation::create(
        &state,
        &made,
        Role::Conversation,
        Some(Workspace::Scratch),
        Duration::from_secs(5),
    )
    .unwrap();
    let (mut first_child, first) = attach(&scratch);
    let told = told_until(&first, |view| matches!(view, View::Replay(..)));
    assert!(
        matches!(told.first(), Some(View::Opened(book)) if book.active() == Some(&made)),
        "{told:#?}"
    );
    assert!(told.iter().any(|view| matches!(view, View::SetShare(_))));
    SHOWN.with(|shown| *shown.borrow_mut() = Some(made.clone()));

    // A prompt is the far machine's, kept there and told back.
    let shipped = td_agent::prompts::Name::Classifier;
    ask(
        &first,
        &Request::SetPrompt {
            ask: 1,
            name: shipped,
            from: shipped.shipped().into(),
            text: "MY POLICY".into(),
        },
    );
    told_until(&first, |view| {
        matches!(view, View::SetPrompts(prompts)
            if prompts.iter().any(|p| p.name == shipped && p.custom && p.text == "MY POLICY"))
    });
    told_until(&first, |view| matches!(view, View::PromptSaved(1, _)));
    // A text past its form is refused there, what is kept staying.
    ask(
        &first,
        &Request::SetPrompt {
            ask: 2,
            name: td_agent::prompts::Name::JevRequest,
            from: td_agent::prompts::Name::JevRequest.shipped().into(),
            text: "no form".into(),
        },
    );
    told_until(
        &first,
        |view| matches!(view, View::PromptRefused(2, said) if said.contains("stays as it was")),
    );
    // A window told before the file moved on overwrites nothing.
    let file = scratch
        .0
        .join("td-agent")
        .join("prompts")
        .join("classifier");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "MY POLICY");
    std::fs::write(&file, "BY HAND").unwrap();
    ask(
        &first,
        &Request::SetPrompt {
            ask: 3,
            name: shipped,
            from: "MY POLICY".into(),
            text: "STALE".into(),
        },
    );
    told_until(
        &first,
        |view| matches!(view, View::PromptRefused(3, said) if said.contains("changed since")),
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "BY HAND");
    // A save made again after its answer was lost finds its own text.
    ask(
        &first,
        &Request::SetPrompt {
            ask: 4,
            name: shipped,
            from: "MY POLICY".into(),
            text: "BY HAND".into(),
        },
    );
    told_until(&first, |view| matches!(view, View::PromptSaved(4, _)));
    // One gone bad is said once, however often the prompts are told.
    std::fs::write(&file, "").unwrap();
    ask(&first, &Request::ListPrompts);
    ask(&first, &Request::ListPrompts);
    ask(&first, &Request::Export);
    let told = told_until(
        &first,
        |view| matches!(view, View::Note(said) if said.contains("export") || said.contains("diagnostics")),
    );
    let said = told
        .iter()
        .filter(|view| matches!(view, View::Note(said) if said.contains("cannot be used")))
        .count();
    assert_eq!(said, 1, "{told:#?}");
    std::fs::remove_file(&file).unwrap();
    ask(
        &first,
        &Request::SetMode {
            conversation: made.clone(),
            mode: Mode::Auto,
        },
    );
    // From afar too, by the person's window as any is.
    told_until(
        &first,
        |view| matches!(view, View::Note(said) if said.contains("now in auto mode")),
    );

    let (mut second_child, second) = attach(&scratch);
    let told = told_until(&second, |view| matches!(view, View::Replay(..)));
    assert!(
        matches!(told.first(), Some(View::Opened(book)) if book.active() == Some(&made)),
        "{told:#?}"
    );
    // The first is let go, and its `attach` ends.
    let_go(&first, "another window");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(Instant::now() < deadline, "the first stayed");
        if first.recv().is_err() {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(first_child.wait().unwrap().success());

    let left = Instant::now();
    drop(second);
    assert!(second_child.wait().unwrap().success());
    let socket = state.root().join(td_agent::window::WINDOW_SOCKET);
    let deadline = Instant::now() + Duration::from_secs(30);
    while socket.exists() {
        assert!(Instant::now() < deadline, "the broker stayed");
        std::thread::sleep(Duration::from_millis(20));
    }
    // A window started just now, or opened again, finds it there.
    assert!(
        left.elapsed() >= Duration::from_secs(4),
        "it did not linger"
    );
}

/// A conversation whose log is longer than the link holds messages unread opens,
/// through `attach`, with a window that is slow to read: one replay, with a log
/// past the bound on what may wait, and what is said behind it, reach it too.
#[test]
fn a_very_long_log_reaches_a_window_that_reads_it_a_while_later() {
    let scratch = Scratch::new("long");
    let state = scratch.state();
    let made = Id::random().unwrap();
    let (mut writer, _) = Conversation::create(
        &state,
        &made,
        Role::Conversation,
        Some(Workspace::Scratch),
        Duration::from_secs(5),
    )
    .unwrap();
    let text = "x".repeat(16 * 1024);
    for _ in 0..6000 {
        writer.append(Kind::Notice { text: text.clone() }).unwrap();
    }
    drop(writer);
    let (mut child, window) = attach(&scratch);
    std::thread::sleep(Duration::from_secs(2));
    let told = told_until(&window, |view| matches!(view, View::Replay(..)));
    let replay = told
        .iter()
        .find_map(|view| match view {
            View::Replay(_, _, events, _) => Some(events.len()),
            _ => None,
        })
        .unwrap();
    assert!(replay >= 6000, "{replay}");
    drop(window);
    assert!(child.wait().unwrap().success());
}

/// A request that names no conversation is of the one the window was showing:
/// one that has moved on gets it back, not another's model. Two opens in one turn
/// of the broker are two opens.
#[test]
fn a_message_for_what_is_no_longer_open_comes_back_and_opens_in_a_row_all_open() {
    let scratch = Scratch::new("shown");
    let state = scratch.state();
    let (a, b) = (Id::random().unwrap(), Id::random().unwrap());
    for id in [&a, &b] {
        Conversation::create(
            &state,
            id,
            Role::Conversation,
            Some(Workspace::Scratch),
            Duration::from_secs(5),
        )
        .unwrap();
    }
    let (mut child, window) = attach(&scratch);
    let told = told_until(&window, |view| matches!(view, View::Replay(..)));
    let Some(View::Opened(book)) = told.first() else {
        panic!("{told:#?}");
    };
    let open = book.active().unwrap().clone();
    let other = if open == a { b.clone() } else { a.clone() };
    let ask_of = |of: &Id, request: &Request| {
        let showing = Showing::Conversation(of.clone());
        assert!(window.send(td_agent::window_wire::encode_asked(&showing, request)));
    };
    ask_of(&other, &Request::Send("for the other".into(), Vec::new()));
    let told = told_until(&window, |view| matches!(view, View::GiveBack(..)));
    assert!(
        matches!(told.last(), Some(View::GiveBack(text, ..)) if text == "for the other"),
        "{told:#?}"
    );
    ask_of(&open, &Request::Open(other.clone()));
    ask_of(&other, &Request::Open(open.clone()));
    ask_of(&open, &Request::Open(other.clone()));
    let told = told_until(&window, |view| {
        matches!(view, View::Opened(book) if book.active() == Some(&other))
            || matches!(view, View::Note(said) if said.contains("has closed"))
    });
    assert!(
        !told
            .iter()
            .any(|view| matches!(view, View::Note(said) if said.contains("has closed"))),
        "{told:#?}"
    );
    drop(window);
    assert!(child.wait().unwrap().success());
}

/// Two conversations, the broker told of them through a window on `attach`, and
/// which of them it has open and which it has not.
fn two_conversations(scratch: &Scratch) -> (std::process::Child, td_agent::peer::Peer, Id, Id) {
    let state = scratch.state();
    let (a, b) = (Id::random().unwrap(), Id::random().unwrap());
    for id in [&a, &b] {
        Conversation::create(
            &state,
            id,
            Role::Conversation,
            Some(Workspace::Scratch),
            Duration::from_secs(5),
        )
        .unwrap();
    }
    let (child, window) = attach(scratch);
    let told = told_until(&window, |view| matches!(view, View::Replay(..)));
    let Some(View::Opened(book)) = told.first() else {
        panic!("{told:#?}");
    };
    let open = book.active().unwrap().clone();
    let other = if open == a { b } else { a };
    (child, window, open, other)
}

/// A message given back with no window there waits for a person, past the
/// linger, and the broker ends once one has taken it.
#[test]
fn a_message_given_back_keeps_the_broker_until_a_window_has_taken_it() {
    // A broker can hear the message a turn before it hears the window go, and
    // then gives it to that window, as it would to any there. So a broker that
    // ends can be innocent, once or twice under load; one not kept for the
    // message ends every time.
    let scratch = (0..5)
        .find_map(|_| {
            let scratch = Scratch::new("waits");
            let (mut child, window, _, other) = two_conversations(&scratch);
            let showing = Showing::Conversation(other);
            let send = Request::Send("for the other".into(), Vec::new());
            assert!(window.send(td_agent::window_wire::encode_asked(&showing, &send)));
            window.finish();
            assert!(child.wait().unwrap().success());
            std::thread::sleep(Duration::from_secs(8));
            let there = scratch
                .state()
                .root()
                .join(td_agent::window::WINDOW_SOCKET)
                .exists();
            there.then_some(scratch)
        })
        .expect("the broker went, with a message waiting");
    let socket = scratch.state().root().join(td_agent::window::WINDOW_SOCKET);
    let (mut child, window) = attach(&scratch);
    let told = told_until(&window, |view| matches!(view, View::GiveBack(..)));
    assert!(
        matches!(told.last(), Some(View::GiveBack(text, ..)) if text == "for the other"),
        "{told:#?}"
    );
    // It says it has it, since a link can die with neither end knowing.
    assert!(window.send(td_agent::window_wire::encode_asked(
        &Showing::Nothing,
        &Request::Received
    )));
    window.finish();
    assert!(child.wait().unwrap().success());
    let deadline = Instant::now() + Duration::from_secs(30);
    while socket.exists() {
        assert!(Instant::now() < deadline, "the broker stayed");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A new conversation that was refused is no reason to send a message to the one
/// the broker moved to: what was shown has gone, and it comes back.
#[test]
fn a_refused_new_lets_no_message_through_to_what_the_broker_moved_to() {
    let scratch = Scratch::new("refused-new");
    let (mut child, window, open, other) = two_conversations(&scratch);
    let ask = |showing: &Showing, request: &Request| {
        assert!(window.send(td_agent::window_wire::encode_asked(showing, request)));
    };
    let shown = Showing::Conversation(open.clone());
    ask(
        &shown,
        &Request::Archive {
            id: open.clone(),
            archived: true,
        },
    );
    told_until(
        &window,
        |view| matches!(view, View::SetActive(id) if id == &other),
    );
    // There is no jail here, so nothing can start in a folder.
    ask(
        &Showing::New(Some(open.clone())),
        &Request::NewIn(PathBuf::from("/nonexistent"), false),
    );
    ask(
        &Showing::New(Some(open)),
        &Request::Send("for what was shown".into(), Vec::new()),
    );
    let told = told_until(&window, |view| matches!(view, View::GiveBack(..)));
    assert!(
        matches!(told.last(), Some(View::GiveBack(text, ..)) if text == "for what was shown"),
        "{told:#?}"
    );
    drop(window);
    assert!(child.wait().unwrap().success());
}

/// The folders the chooser lists are the broker's, whichever machine the window
/// is on: read there, the first that can be, or why not.
#[test]
fn a_far_windows_folders_are_read_by_the_broker_and_a_refusal_is_said() {
    let scratch = Scratch::new("folders");
    let home = std::fs::canonicalize(&scratch.0).unwrap();
    let base = home.join("folders");
    for dir in ["Beta", "alpha", ".hidden", "repo/.git"] {
        std::fs::create_dir_all(base.join(dir)).unwrap();
    }
    std::fs::write(base.join("file"), "").unwrap();
    let (mut child, window) = attach(&scratch);
    told_until(&window, |view| matches!(view, View::SetShare(_)));
    let answer = |candidates: Vec<PathBuf>, or_home: bool, from: Option<&str>| {
        ask(
            &window,
            &Request::ListFolders {
                candidates: candidates.clone(),
                or_home,
                from: from.map(str::to_string),
            },
        );
        let told = told_until(&window, |view| matches!(view, View::Folders(_)));
        let Some(View::Folders(answer)) = told.into_iter().last() else {
            panic!("told of no folders");
        };
        assert_eq!(answer.asked, candidates);
        assert_eq!(answer.from.as_deref(), from);
        answer.read
    };
    let read = answer(vec![base.clone()], false, Some("Beta")).unwrap();
    assert_eq!(read.folder, base);
    assert_eq!(
        read.entries,
        [
            ("alpha".to_string(), String::new()),
            ("Beta".to_string(), String::new()),
            ("repo".to_string(), "git".to_string()),
        ]
    );
    assert!(!read.truncated);
    // What cannot be read is said, not gone around.
    let gone = base.join("gone");
    let why = answer(vec![gone.clone()], false, None).unwrap_err();
    assert!(why.contains(gone.to_str().unwrap()), "{why}");
    let why = answer(vec![base.join("file")], false, None).unwrap_err();
    assert!(why.contains("file"), "{why}");
    // Nor is a relative path read, from wherever the broker happens to be.
    let why = answer(vec![PathBuf::from("folders")], true, None).unwrap_err();
    assert!(why.contains("folders is not an absolute path"), "{why}");
    // In order, and, when opening, on to the broker's own home.
    let read = answer(vec![gone.clone(), base.clone()], false, None).unwrap();
    assert_eq!(read.folder, base);
    let read = answer(vec![gone], true, None).unwrap();
    assert_eq!(std::fs::canonicalize(read.folder).unwrap(), home);
    let read = answer(Vec::new(), true, None).unwrap();
    assert_eq!(std::fs::canonicalize(read.folder).unwrap(), home);
    drop(window);
    assert!(child.wait().unwrap().success());
}

/// `--stop` ends a broker that a window would keep, and a second says there is
/// none.
#[test]
fn stop_ends_a_broker_that_would_stay() {
    let scratch = Scratch::new("stop");
    let state = scratch.state();
    let (_child, window) = attach(&scratch);
    told_until(&window, |view| matches!(view, View::SetShare(_)));
    let stop = |scratch: &Scratch| {
        Command::new(PROGRAM)
            .args(["serve", "--stop"])
            .env_clear()
            .env("HOME", &scratch.0)
            .env("XDG_STATE_HOME", &scratch.0)
            .env("XDG_CONFIG_HOME", scratch.0.join("config"))
            .output()
            .unwrap()
    };
    let output = stop(&scratch);
    assert!(output.status.success(), "{output:?}");
    assert!(!state.root().join(td_agent::window::WINDOW_SOCKET).exists());
    // Gone, not going: a broker can start at once.
    drop(state.lock_window().unwrap());
    let output = stop(&scratch);
    assert!(output.status.success());
    assert_eq!(output.stdout, b"no td-agent serve is running\n");
}

/// A state directory reached by a link, or left open by hand, serves as well.
#[test]
fn a_broker_listens_in_a_state_directory_by_a_link_and_shuts_one_left_open() {
    use std::os::unix::fs::PermissionsExt;
    let scratch = Scratch::new("link");
    let real = scratch.0.join("real");
    std::fs::create_dir_all(real.join("td-agent")).unwrap();
    std::fs::set_permissions(
        real.join("td-agent"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    std::os::unix::fs::symlink(&real, scratch.0.join("by-link")).unwrap();
    let (ours, theirs) = UnixStream::pair().unwrap();
    let mut child = Command::new(PROGRAM)
        .arg("attach")
        .env_clear()
        .env("HOME", &scratch.0)
        .env("XDG_STATE_HOME", scratch.0.join("by-link"))
        .env("XDG_CONFIG_HOME", scratch.0.join("config"))
        .stdin(Stdio::from(OwnedFd::from(theirs.try_clone().unwrap())))
        .stdout(Stdio::from(OwnedFd::from(theirs)))
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let window = claimed(td_agent::peer::Peer::over(ours).unwrap());
    told_until(&window, |view| matches!(view, View::SetShare(_)));
    let mode = std::fs::metadata(real.join("td-agent"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o077, 0, "{mode:o}");
    drop(window);
    assert!(child.wait().unwrap().success());
}

/// Standard error is a file now, which can be full: that ends nothing.
#[test]
fn a_broker_goes_on_with_standard_error_full() {
    let scratch = Scratch::new("full");
    let full = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/full")
        .unwrap();
    let status = Command::new(PROGRAM)
        .arg("serve")
        .env_clear()
        .env("HOME", &scratch.0)
        .env("XDG_STATE_HOME", &scratch.0)
        .env("XDG_CONFIG_HOME", scratch.0.join("config"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(full))
        .status()
        .unwrap();
    assert!(status.success(), "{status}");
}

/// A far window going is no fault of `attach`'s, while the broker is saying
/// something or not.
#[test]
fn attach_ends_quietly_when_the_far_window_stops_reading() {
    let scratch = Scratch::new("pipe");
    let state = scratch.state();
    let made = Id::random().unwrap();
    let (mut writer, _) = Conversation::create(
        &state,
        &made,
        Role::Conversation,
        Some(Workspace::Scratch),
        Duration::from_secs(5),
    )
    .unwrap();
    let text = "x".repeat(16 * 1024);
    for _ in 0..2000 {
        writer.append(Kind::Notice { text: text.clone() }).unwrap();
    }
    drop(writer);
    let (mut child, window) = attach(&scratch);
    told_until(&window, |view| matches!(view, View::Opened(_)));
    drop(window);
    assert!(child.wait().unwrap().success());
}

/// A variable left in a shell names no launch but one of this program.
#[test]
fn a_launch_of_another_program_is_not_obeyed() {
    use std::os::unix::fs::PermissionsExt;
    let scratch = Scratch::new("stale");
    let said = scratch.0.join("said");
    let launcher = scratch.0.join("launcher");
    std::fs::write(
        &launcher,
        format!("#!/bin/sh\ntouch {}\nexit 7\n", said.display()),
    )
    .unwrap();
    std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755)).unwrap();
    let (ours, theirs) = UnixStream::pair().unwrap();
    let mut child = Command::new(PROGRAM)
        .arg("attach")
        .env_clear()
        .env("HOME", &scratch.0)
        .env("XDG_STATE_HOME", &scratch.0)
        .env("XDG_CONFIG_HOME", scratch.0.join("config"))
        .env("TD_AGENT_LAUNCHER", &launcher)
        .env("TD_AGENT_SESSION_RUNTIME", "/the/session")
        .stdin(Stdio::from(OwnedFd::from(theirs.try_clone().unwrap())))
        .stdout(Stdio::from(OwnedFd::from(theirs)))
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let window = claimed(td_agent::peer::Peer::over(ours).unwrap());
    told_until(&window, |view| matches!(view, View::SetShare(_)));
    assert!(!said.exists());
    drop(window);
    assert!(child.wait().unwrap().success());
}

/// A launch's broker comes by that launch, under the session's runtime directory
/// and not the one that goes with the window.
#[test]
fn a_broker_comes_by_the_launch_that_started_the_window() {
    use std::os::unix::fs::PermissionsExt;
    let scratch = Scratch::new("launcher");
    let said = scratch.0.join("said");
    std::fs::create_dir(scratch.0.join("lib")).unwrap();
    let launcher = scratch.0.join("lib/launcher");
    std::fs::write(
        &launcher,
        format!(
            "#!/bin/sh\necho \"$0 $* $XDG_RUNTIME_DIR $PWD\" > {}\nexec {PROGRAM} \"$@\"\n",
            said.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink(PROGRAM, scratch.0.join("lib/td-agent")).unwrap();
    let (ours, theirs) = UnixStream::pair().unwrap();
    let mut child = Command::new(PROGRAM)
        .arg("attach")
        .env_clear()
        .env("HOME", &scratch.0)
        .env("XDG_STATE_HOME", &scratch.0)
        .env("XDG_CONFIG_HOME", scratch.0.join("config"))
        .env("XDG_RUNTIME_DIR", "/the/launchs/own")
        .env("TD_AGENT_LAUNCHER", &launcher)
        .env("TD_AGENT_SESSION_RUNTIME", "/the/session")
        .stdin(Stdio::from(OwnedFd::from(theirs.try_clone().unwrap())))
        .stdout(Stdio::from(OwnedFd::from(theirs)))
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let window = claimed(td_agent::peer::Peer::over(ours).unwrap());
    told_until(&window, |view| matches!(view, View::SetShare(_)));
    let said = std::fs::read_to_string(&said).unwrap();
    assert!(said.ends_with("serve /the/session /\n"), "{said}");
    drop(window);
    assert!(child.wait().unwrap().success());
}

/// A message sent just before the human switches away is answered: the
/// process is kept, in the background, through the turn it starts, and
/// let go once the turn ends. The turn here asks the window to reserve
/// and is refused, so nothing is ever sent to a network.
#[test]
fn a_message_sent_just_before_switching_away_runs_its_turn() {
    let scratch = Scratch::new("switch");
    let state = scratch.state();
    let keyed = Down::Setup {
        key: Ok(td_agent::key::Secret::new("sk-or-test".into())),
        client: Box::new(Client {
            limits: td_agent::cost::Limits {
                turn: None,
                conversation: None,
                day: None,
            },
            ..Client::default()
        }),
    };
    let mut supervisor = Supervisor::new(PROGRAM.into(), state.root().to_path_buf(), keyed);
    let (a, b) = (Id::random().unwrap(), Id::random().unwrap());
    supervisor
        .open(a.clone(), Some(Role::Conversation))
        .unwrap();
    supervisor.send("hello".into(), Vec::new()).unwrap();
    // Away before the window has heard a word of the turn.
    supervisor
        .open(b.clone(), Some(Role::Conversation))
        .unwrap();
    let deadline = Instant::now() + TIMEOUT;
    let mut finished = None;
    while finished.is_none() {
        assert!(Instant::now() < deadline, "the turn did not end");
        for (of, update) in supervisor.poll() {
            match update {
                Update::Up(Up::Reserve { id, .. }) if of == a => supervisor.answer(
                    &a,
                    &Down::Reservation {
                        id,
                        refusal: Some("refused by the test".into()),
                    },
                ),
                Update::Up(Up::Event(event)) if of == a => {
                    if let Kind::Finished { outcome, .. } = event.kind {
                        finished = Some(outcome);
                    }
                }
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(finished.as_deref(), Some("refused by the test"));
    assert_eq!(supervisor.open_id(), Some(&b));
    // The turn over, the background process is let go.
    supervisor.poll();
    assert!(supervisor.background().is_empty());
}

/// A compaction asked while a turn runs, the human then switching away,
/// is run: the process is kept past the turn's end until the
/// compaction it was asked has started and ended (DESIGN.md §14).
#[test]
fn a_compaction_asked_during_a_turn_runs_after_a_switch() {
    let scratch = Scratch::new("switch-compact");
    let state = scratch.state();
    let keyed = Down::Setup {
        key: Ok(td_agent::key::Secret::new("sk-or-test".into())),
        client: Box::new(Client {
            limits: td_agent::cost::Limits {
                turn: None,
                conversation: None,
                day: None,
            },
            ..Client::default()
        }),
    };
    let mut supervisor = Supervisor::new(PROGRAM.into(), state.root().to_path_buf(), keyed);
    let (a, b) = (Id::random().unwrap(), Id::random().unwrap());
    supervisor
        .open(a.clone(), Some(Role::Conversation))
        .unwrap();
    supervisor.send("hello".into(), Vec::new()).unwrap();
    supervisor.compact(None).unwrap();
    supervisor
        .open(b.clone(), Some(Role::Conversation))
        .unwrap();
    let deadline = Instant::now() + TIMEOUT;
    let mut outcomes = Vec::new();
    while outcomes.len() < 2 {
        assert!(Instant::now() < deadline, "heard only {outcomes:?}");
        for (of, update) in supervisor.poll() {
            match update {
                Update::Up(Up::Reserve { id, .. }) if of == a => supervisor.answer(
                    &a,
                    &Down::Reservation {
                        id,
                        refusal: Some("refused by the test".into()),
                    },
                ),
                Update::Up(Up::Event(event)) if of == a => {
                    if let Kind::Finished { outcome, .. } = event.kind {
                        outcomes.push(outcome);
                    }
                }
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(outcomes[0], "refused by the test");
    assert!(
        outcomes[1].starts_with("the conversation could not be compacted"),
        "{outcomes:?}"
    );
    supervisor.poll();
    assert!(supervisor.background().is_empty());
}

/// Deleting a conversation (DESIGN.md §4): its live process is ended and
/// waited for, so its lock is free and its directory goes; the list no
/// longer holds it, and a second delete has nothing to remove.
#[test]
fn a_deleted_conversation_loses_its_process_and_its_directory() {
    let scratch = Scratch::new("delete");
    let state = scratch.state();
    let id = Id::random().unwrap();
    let mut supervisor = Supervisor::new(PROGRAM.into(), state.root().to_path_buf(), keyless());
    supervisor
        .open(id.clone(), Some(Role::Conversation))
        .unwrap();
    let mut heard = Vec::new();
    supervisor.send("before".into(), Vec::new()).unwrap();
    until(&mut supervisor, &mut heard, |h| delivered(h) == 1);
    let pid = supervisor.pid().unwrap();
    // While its process holds the lock, the store refuses.
    assert!(state
        .delete(&id)
        .unwrap_err()
        .contains("already has its writer"));
    assert!(
        supervisor.remove(&id).is_empty(),
        "its message was answered"
    );
    assert_eq!(supervisor.open_id(), None);
    assert!(
        !Path::new(&format!("/proc/{pid}")).exists(),
        "its process lives"
    );
    assert_eq!(state.delete(&id).unwrap(), None);
    assert!(!state.conversation(&id).exists());
    assert!(state.list().0.iter().all(|meta| meta.id != id));
    assert_eq!(state.delete(&id).unwrap(), None);
}

/// A conversation left is retired, its process told to go; deleting it
/// at once ends that process too, so its lock is free without waiting.
#[test]
fn deleting_a_conversation_just_left_ends_its_retiring_process() {
    let scratch = Scratch::new("delete-retiring");
    let state = scratch.state();
    let (left, other) = (Id::random().unwrap(), Id::random().unwrap());
    let mut supervisor = Supervisor::new(PROGRAM.into(), state.root().to_path_buf(), keyless());
    supervisor
        .open(left.clone(), Some(Role::Conversation))
        .unwrap();
    let mut heard = Vec::new();
    until(&mut supervisor, &mut heard, |h| {
        h.iter().any(|u| matches!(u, Update::Up(Up::Hello { .. })))
    });
    supervisor
        .open(other.clone(), Some(Role::Conversation))
        .unwrap();
    assert!(supervisor.remove(&left).is_empty());
    let started = Instant::now();
    assert_eq!(state.delete(&left).unwrap(), None);
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(!state.conversation(&left).exists());
    assert_eq!(supervisor.open_id(), Some(&other));
}

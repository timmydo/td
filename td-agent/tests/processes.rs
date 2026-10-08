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

use td_agent::config::Client;
use td_agent::frame;
use td_agent::protocol::{Down, Up};
use td_agent::store::{Conversation, Id, Kind, Role, StateDir};
use td_agent::supervisor::{Supervisor, Update, MAX_RESTARTS};
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
    supervisor.send("first".into()).unwrap();
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
    supervisor.send("second".into()).unwrap();
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
    supervisor.send("kept".into()).unwrap();
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
                Kind::Notification { text } if text.contains("could not be prepared: the remote is not admitted")))
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
    supervisor.send("hello".into()).unwrap();
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
    supervisor.send("hello".into()).unwrap();
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
                    Kind::Notification { text } => Some(format!("notification: {text}")),
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
    supervisor.send("hello".into()).unwrap();
    supervisor.interrupt().unwrap();
    until(&mut supervisor, &mut heard, |heard| ended(heard) == 1);
    // Sent while it waits.
    supervisor.send("again".into()).unwrap();
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
    assert!(supervisor.send("x".into()).is_err());
    assert!(supervisor.poll().is_empty(), "no restart after failing");
}

/// The program's conversation personality, its socketpair in hand.
fn conversation(state: &Path, id: &Id, create: Option<Role>) -> (std::process::Child, UnixStream) {
    let (ours, theirs) = UnixStream::pair().unwrap();
    let mut command = Command::new(PROGRAM);
    command
        .args(["conversation", id.as_str(), "--state-dir"])
        .arg(state);
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
fn a_second_window_process_is_refused_before_it_connects_a_display() {
    let scratch = Scratch::new("window");
    let state = scratch.state();
    let _held = state.lock_window().unwrap();
    let output = Command::new(PROGRAM)
        .env_clear()
        .env("HOME", &scratch.0)
        .env("XDG_STATE_HOME", &scratch.0)
        .env("XDG_CONFIG_HOME", scratch.0.join("config"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("another td-agent window"), "{stderr}");
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
    let output = Command::new(PROGRAM)
        .env_clear()
        .env("HOME", &scratch.0)
        .env("XDG_STATE_HOME", &scratch.0)
        .env("XDG_CONFIG_HOME", scratch.0.join("config"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("`limits`"), "{stderr}");
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
    supervisor.send("hello".into()).unwrap();
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
    supervisor.send("hello".into()).unwrap();
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
    supervisor.send("before".into()).unwrap();
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

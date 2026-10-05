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
        client: Client::default(),
    }
}

const PROGRAM: &str = env!("CARGO_BIN_EXE_td-agent");
const TIMEOUT: Duration = Duration::from_secs(10);

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
    supervisor.open(b, Some(Role::Conversation)).unwrap();
    supervisor.open(a.clone(), None).unwrap();
    let mut heard = Vec::new();
    until(&mut supervisor, &mut heard, |h| delivered(h) == 1);
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
                Kind::Notice { text } if text.contains("could not be prepared: the remote is not admitted")))
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
        client: Client {
            limits: td_agent::cost::Limits {
                turn: None,
                conversation: None,
                day: None,
            },
            ..Client::default()
        },
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

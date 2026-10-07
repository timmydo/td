#![deny(unsafe_code)]

//! The compositor owns one private endpoint; its worker alone exchanges frames.

#[cfg_attr(not(feature = "target-recipe"), path = "../../td-authd/src/channel.rs")]
#[cfg_attr(feature = "target-recipe", path = "auth/channel.rs")]
mod channel;
#[cfg_attr(not(feature = "target-recipe"), path = "../../td-authd/src/consent.rs")]
#[cfg_attr(feature = "target-recipe", path = "auth/consent.rs")]
#[allow(
    dead_code,
    reason = "immutable trusted-prompt contract; authority consumer follows"
)]
pub(crate) mod consent;
#[cfg_attr(not(feature = "target-recipe"), path = "../../td-authd/src/sys.rs")]
#[cfg_attr(feature = "target-recipe", path = "auth/sys.rs")]
mod sys;
// `9a`'s names, admitted under the rules root sends them by and drawn on
// the lock surface, and the names a consent hostname description admits.
#[cfg_attr(
    not(feature = "target-recipe"),
    path = "../../td-firstboot/src/hostname.rs"
)]
#[cfg_attr(feature = "target-recipe", path = "auth/hostname.rs")]
mod hostname;
#[cfg_attr(
    not(feature = "target-recipe"),
    path = "../../td-authd/src/primary_account.rs"
)]
#[cfg_attr(feature = "target-recipe", path = "auth/primary_account.rs")]
#[allow(
    dead_code,
    reason = "only the name rule is the compositor's; the account database is td-authd's"
)]
mod primary_account;

use std::collections::VecDeque;
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

const VERSION: &[u8] = b"TDLA003\n";
const CAPACITY: usize = 16;
const QUEUE_CAPACITY: usize = 1;
const TICK: Duration = Duration::from_millis(250);
/// How often `1a` is asked again while the state could not be read.
const LOGIN_POLL: Duration = Duration::from_millis(250);
/// TOKEN-LOGIN.md's failure kind for a state that could not be read.
const UNREADABLE: u8 = 0x0c;

enum Work {
    Program(Program),
    Secret(std::sync::Arc<crate::secret_client::Attempt>),
}

#[derive(Clone)]
pub(crate) struct Launcher {
    send: SyncSender<Work>,
    login: Login,
    /// Root's answer at connect, which the generation's start locks on:
    /// never a later poll's.
    connected: Option<Answer>,
}

/// TOKEN-LOGIN.md's login state, as root's `1a` answer gives it
/// (td-authd/DESIGN.md, login-state amendment 1), with an enrolled
/// record's key list.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum LoginState {
    Unenrolled,
    /// One to eight fingerprints in canonical slot order.
    Enrolled(Vec<consent::Fingerprint>),
    /// The cause as its failure kind: `0a`, `0b` or `0c`.
    Unavailable(u8),
}

/// Root's `1a` answer: the login state, and the primary username and
/// hostname the lock surface draws above the state's rows.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Answer {
    state: LoginState,
    username: String,
    /// Empty where root sent none.
    hostname: String,
}

impl Answer {
    /// Exactly `9a`'s shape, or a protocol violation that ends the paired
    /// generation: a state, an enrolled list or a cause; a primary account
    /// name; an empty hostname or one under td-firstboot's rules; and the
    /// revocation byte, which is `00` until increment 5 defines another.
    fn decode(answer: &[u8]) -> Result<Self, String> {
        let invalid = || "invalid login state answer".to_string();
        let [0x9a, state, rest @ ..] = answer else {
            return Err(invalid());
        };
        let (state, rest) = match (*state, rest) {
            (0, rest) => (LoginState::Unenrolled, rest),
            (1, [count, rest @ ..]) if (1..=consent::LOGIN_KEYS).contains(count) => {
                let (keys, rest) = rest
                    .split_at_checked(usize::from(*count) * 4)
                    .ok_or_else(invalid)?;
                let (keys, []) = keys.as_chunks::<4>() else {
                    return Err(invalid());
                };
                (LoginState::Enrolled(keys.to_vec()), rest)
            }
            (2, [cause @ 0x0a..=0x0c, rest @ ..]) => (LoginState::Unavailable(*cause), rest),
            _ => return Err(invalid()),
        };
        let [length, rest @ ..] = rest else {
            return Err(invalid());
        };
        let (username, rest) = rest
            .split_at_checked(usize::from(*length))
            .ok_or_else(invalid)?;
        let [length, rest @ ..] = rest else {
            return Err(invalid());
        };
        let (host, rest) = rest
            .split_at_checked(usize::from(*length))
            .ok_or_else(invalid)?;
        let username = std::str::from_utf8(username).map_err(|_| invalid())?;
        let host = std::str::from_utf8(host).map_err(|_| invalid())?;
        if rest != [0] || primary_account::validate_name(username).is_err() {
            return Err(invalid());
        }
        let hostname = if host.is_empty() {
            String::new()
        } else {
            hostname::Hostname::parse(host)
                .map_err(|_| invalid())?
                .name()
                .to_string()
        };
        Ok(Self {
            state,
            username: username.to_string(),
            hostname,
        })
    }

    pub(crate) fn state(&self) -> &LoginState {
        &self.state
    }

    pub(crate) fn username(&self) -> &str {
        &self.username
    }

    pub(crate) fn hostname(&self) -> &str {
        &self.hostname
    }

    /// Whether a generation starts locked on this answer: enrolled or
    /// unavailable, any cause; never unenrolled, which is also how root
    /// answers on the live medium (TOKEN-LOGIN.md, "Session lock").
    pub(crate) fn locks(&self) -> bool {
        !matches!(self.state, LoginState::Unenrolled)
    }
}

/// Called with every answer once it is stored.
type Watcher = Arc<dyn Fn(&Answer) + Send + Sync>;

#[derive(Default)]
struct Shared {
    answer: Option<Answer>,
    watcher: Option<Watcher>,
}

/// The last `1a` answer, shared: the authority worker writes it, and the
/// input bindings and the lock surface's watcher read it, each holding
/// nothing else while they do.
#[derive(Clone, Default)]
pub(crate) struct Login(Arc<Mutex<Shared>>);

impl Login {
    /// The last answer's state; none before the first answer, and in the
    /// direct profile, which has no authority.
    pub(crate) fn state(&self) -> Option<LoginState> {
        self.current().map(|answer| answer.state)
    }

    /// The last answer, when `state` has one.
    pub(crate) fn current(&self) -> Option<Answer> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .answer
            .clone()
    }

    /// Calls `watcher` with each later answer, from the authority worker,
    /// which holds no lock then, this handle's included.
    pub(crate) fn watch(&self, watcher: impl Fn(&Answer) + Send + Sync + 'static) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .watcher = Some(Arc::new(watcher));
    }

    fn set(&self, answer: Answer) {
        let watcher = {
            let mut shared = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            shared.answer = Some(answer.clone());
            shared.watcher.clone()
        };
        if let Some(watcher) = watcher {
            watcher(&answer);
        }
    }

    /// Takes `answer` as the worker takes root's.
    #[cfg(test)]
    pub(crate) fn answer(&self, answer: &[u8]) -> Result<(), String> {
        self.set(Answer::decode(answer)?);
        Ok(())
    }
}

/// When the worker asks `1a`: at connect, after every login operation's
/// end, and every 250 ms while the state could not be read, so a transient
/// helper failure resolves without a reboot.
struct LoginPoll {
    login: Login,
    /// When to ask again; none unless the state could not be read.
    next: Option<Instant>,
}

impl LoginPoll {
    fn new(login: Login) -> Self {
        Self { login, next: None }
    }

    fn read(&mut self, wire: &mut impl Exchange) -> Result<Answer, String> {
        let answer = Answer::decode(&wire.exchange(&[0x1a])?)?;
        self.next = if answer.state == LoginState::Unavailable(UNREADABLE) {
            Some(
                Instant::now()
                    .checked_add(LOGIN_POLL)
                    .ok_or("login state poll overflow")?,
            )
        } else {
            None
        };
        self.login.set(answer.clone());
        Ok(answer)
    }

    /// After the secret client's tick: `1a` again when a login operation
    /// ended in it, or when the poll is due.
    fn follow(
        &mut self,
        wire: &mut impl Exchange,
        secrets: &mut crate::secret_client::Client,
    ) -> Result<(), String> {
        if secrets.take_login_end() || self.next.is_some_and(|next| Instant::now() >= next) {
            return self.read(wire).map(drop);
        }
        Ok(())
    }

    /// How long the worker may wait for work before it is due.
    fn wait(&self) -> Duration {
        self.next.map_or(TICK, |next| {
            next.saturating_duration_since(Instant::now()).min(TICK)
        })
    }
}

impl Launcher {
    /// Must precede every compositor thread, child, and descriptor delegation.
    pub fn connect() -> Result<Self, String> {
        startup()?;
        let mut wire = channel::Channel::from_stdin(0).map_err(|e| e.to_string())?;
        if wire.receive().map_err(|e| e.to_string())? != VERSION {
            return Err("unsupported program authority protocol".into());
        }
        wire.send(VERSION).map_err(|e| e.to_string())?;
        if wire.receive().map_err(|e| e.to_string())? != [0x80] {
            return Err("program authority refused session admission".into());
        }
        let login = Login::default();
        let (poll, connected) = open_session(&mut wire, &login)?;
        let (send, receive) = mpsc::sync_channel(QUEUE_CAPACITY);
        std::thread::Builder::new()
            .name("terminal-authority".into())
            .spawn(move || {
                if let Err(error) = worker(wire, receive, poll) {
                    let _ = writeln!(
                        std::io::stderr().lock(),
                        "td-compositor: program authority: {error}"
                    );
                }
                // Losing either peer must end this paired service generation.
                std::process::exit(1);
            })
            .map_err(|e| format!("start program authority worker: {e}"))?;
        Ok(Self {
            send,
            login,
            connected: Some(connected),
        })
    }

    /// The login state root last answered, which the worker keeps current.
    pub fn login(&self) -> Login {
        self.login.clone()
    }

    /// Root's answer at connect, which no later answer replaces.
    pub(crate) fn connected(&self) -> Option<Answer> {
        self.connected.clone()
    }

    pub fn unlock(
        &self,
        attempt: std::sync::Arc<crate::secret_client::Attempt>,
    ) -> Result<(), String> {
        match self.send.try_send(Work::Secret(attempt)) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err("authority request already pending".into()),
            Err(TrySendError::Disconnected(_)) => Err("secret authority unavailable".into()),
        }
    }

    pub fn launch(&self) -> Result<(), String> {
        match self.send.try_send(Work::Program(Program::Home)) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err("program launch is already pending".into()),
            Err(TrySendError::Disconnected(_)) => Err("program authority is unavailable".into()),
        }
    }

    pub fn launch_selected(&self, terminal: Program) -> Result<(), String> {
        match self.send.try_send(Work::Program(terminal)) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err("program launch is already pending".into()),
            Err(TrySendError::Disconnected(_)) => Err("program authority is unavailable".into()),
        }
    }
}

/// A launcher whose secret attempts wait in a queue a test takes them
/// from, playing the worker itself.
#[cfg(test)]
pub(crate) struct Queued(Receiver<Work>);

#[cfg(test)]
impl Queued {
    pub(crate) fn launcher() -> (Launcher, Self) {
        let (send, receive) = mpsc::sync_channel(QUEUE_CAPACITY);
        let login = Login::default();
        (
            Launcher {
                send,
                login,
                connected: None,
            },
            Self(receive),
        )
    }

    pub(crate) fn attempt(&self) -> Option<std::sync::Arc<crate::secret_client::Attempt>> {
        match self.0.try_recv() {
            Ok(Work::Secret(attempt)) => Some(attempt),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Program {
    Home,
    Task,
    Claude,
    TaskManager,
    Editor,
    Photo,
    Review,
    Dua,
    Agent,
}

fn startup() -> Result<(), String> {
    let mut status = String::new();
    File::open("/proc/self/status")
        .map_err(|e| e.to_string())?
        .take(8193)
        .read_to_string(&mut status)
        .map_err(|e| e.to_string())?;
    check_status(&status)?;
    audit_descriptors(
        |fd| {
            std::fs::metadata(format!("/proc/self/fd/{fd}"))
                .map(|metadata| (metadata.dev(), metadata.ino()))
                .map_err(|e| format!("missing authority standard descriptor {fd}: {e}"))
        },
        || {
            std::fs::read_dir("/proc/self/fd")
                .map_err(|e| e.to_string())?
                .take(5)
                .map(|entry| {
                    entry
                        .map_err(|e| e.to_string())?
                        .file_name()
                        .to_str()
                        .ok_or("invalid descriptor name")?
                        .parse::<u32>()
                        .map_err(|_| "invalid descriptor number".into())
                })
                .collect()
        },
    )
}

fn audit_descriptors(
    mut identity: impl FnMut(u32) -> Result<(u64, u64), String>,
    enumerate: impl FnOnce() -> Result<Vec<u32>, String>,
) -> Result<(), String> {
    // Check before read_dir can occupy a closed standard descriptor.
    let channel = identity(0)?;
    for fd in [1, 2] {
        if identity(fd)? == channel {
            return Err("compositor log aliases its private authority endpoint".into());
        }
    }
    let mut descriptors = enumerate()?;
    descriptors.sort_unstable();
    if descriptors != [0, 1, 2, 3] {
        return Err("compositor requires only standard authority descriptors".into());
    }
    Ok(())
}

fn check_status(status: &str) -> Result<(), String> {
    let columns = |key: &str| {
        status.lines().find_map(|line| {
            line.strip_prefix(key)
                .map(|value| value.split_whitespace().collect::<Vec<_>>())
        })
    };
    let uid = columns("Uid:").ok_or("missing compositor identity")?;
    let [first, second, third, fourth] = uid.as_slice() else {
        return Err("invalid compositor identity".into());
    };
    let number = first.parse::<u32>().map_err(|_| "invalid compositor uid")?;
    if status.len() > 8192
        || !(1..=999).contains(&number)
        || number.to_string() != *first
        || first != second
        || first != third
        || first != fourth
        || columns("Gid:").as_deref() != Some(uid.as_slice())
        || columns("Threads:").as_deref() != Some(["1"].as_slice())
    {
        return Err("authority compositor requires one dedicated service thread".into());
    }
    Ok(())
}

pub(crate) trait Exchange {
    /// Writes one request and returns once it is written, so a caller
    /// holding a secret in it can zero it before awaiting the answer.
    fn send(&mut self, request: &[u8]) -> Result<(), String>;
    fn receive(&mut self) -> Result<Vec<u8>, String>;

    fn exchange(&mut self, request: &[u8]) -> Result<Vec<u8>, String> {
        self.send(request)?;
        self.receive()
    }
}

impl Exchange for channel::Channel {
    fn send(&mut self, request: &[u8]) -> Result<(), String> {
        channel::Channel::send(self, request).map_err(|e| e.to_string())
    }

    fn receive(&mut self) -> Result<Vec<u8>, String> {
        channel::Channel::receive(self).map_err(|e| e.to_string())
    }
}

/// Complete prior-generation cleanup before any device or input admission.
fn prepare_session(wire: &mut impl Exchange) -> Result<(), String> {
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(5))
        .ok_or("secret session preparation deadline overflow")?;
    if wire.exchange(&[0x10])? != [0x90] {
        return Err("authority refused secret session preparation".into());
    }
    loop {
        let response = wire.exchange(&[0x11])?;
        if Instant::now() >= deadline {
            return Err("secret session preparation expired".into());
        }
        match response.as_slice() {
            [0x91, 2] => return Ok(()),
            [0x91, 1] => std::thread::sleep(Duration::from_millis(10)),
            _ => return Err("invalid secret session preparation response".into()),
        }
    }
}

/// Prepare, then the login state, both before the first repaint and any
/// input admission: the poll, and the answer at connect.
fn open_session(wire: &mut impl Exchange, login: &Login) -> Result<(LoginPoll, Answer), String> {
    prepare_session(wire)?;
    let mut poll = LoginPoll::new(login.clone());
    let connected = poll.read(wire)?;
    Ok((poll, connected))
}

struct Processes {
    handles: VecDeque<u64>,
    latest: u64,
}

impl Processes {
    fn new() -> Self {
        Self {
            handles: VecDeque::with_capacity(CAPACITY),
            latest: 0,
        }
    }

    fn start(
        &mut self,
        wire: &mut impl Exchange,
        terminal: Program,
    ) -> Result<Option<&'static str>, String> {
        if self.handles.len() >= CAPACITY {
            return Ok(Some("program launch limit reached"));
        }
        let request = match terminal {
            Program::Home => [1],
            Program::Task => [4],
            Program::Claude => [6],
            Program::TaskManager => [7],
            Program::Editor => [8],
            Program::Photo => [9],
            Program::Review => [0x0a],
            Program::Dua => [0x0b],
            Program::Agent => [0x0c],
        };
        match wire.exchange(&request)?.as_slice() {
            [0xff, 1] => Ok(Some("program authority process table is full")),
            [0xff, 2] => Ok(Some("program authority could not start the helper")),
            [0x81, handle @ ..] if handle.len() == 8 => {
                let handle = u64::from_be_bytes(handle.try_into().map_err(|_| "invalid handle")?);
                if handle <= self.latest {
                    return Err("program authority reused a process handle".into());
                }
                self.latest = handle;
                self.handles.push_back(handle);
                Ok(None)
            }
            _ => Err("invalid program authority start response".into()),
        }
    }

    fn poll(&mut self, wire: &mut impl Exchange) -> Result<Option<&'static str>, String> {
        let Some(handle) = self.handles.pop_front() else {
            if wire.exchange(&[3])? != [0x83] {
                return Err("invalid program authority heartbeat".into());
            }
            return Ok(None);
        };
        let mut request = [0u8; 9];
        if let Some(first) = request.first_mut() {
            *first = 2;
        }
        request
            .get_mut(1..)
            .ok_or("invalid poll buffer")?
            .copy_from_slice(&handle.to_be_bytes());
        match wire.exchange(&request)?.as_slice() {
            [0x82, 0] => {
                self.handles.push_back(handle);
                Ok(None)
            }
            [0x82, 1] => Ok(None),
            [0x82, 2] => Ok(Some("launched program failed")),
            _ => Err("invalid program authority process status".into()),
        }
    }
}

fn worker(
    mut wire: impl Exchange,
    receive: Receiver<Work>,
    mut login: LoginPoll,
) -> Result<(), String> {
    let mut processes = Processes::new();
    let mut secrets = crate::secret_client::Client::default();
    loop {
        match receive.recv_timeout(login.wait()) {
            Ok(Work::Secret(attempt)) => secrets.start(&mut wire, attempt)?,
            Ok(Work::Program(terminal)) => {
                if let Some(error) = processes.start(&mut wire, terminal)? {
                    let _ = writeln!(std::io::stderr().lock(), "td-compositor: {error}");
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Err("launcher disconnected".into()),
        }
        if let Some(error) = processes.poll(&mut wire)? {
            let _ = writeln!(std::io::stderr().lock(), "td-compositor: {error}");
        }
        secrets.tick(&mut wire)?;
        login.follow(&mut wire, &mut secrets)?;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    struct Wire {
        requests: Vec<Vec<u8>>,
        answers: VecDeque<Vec<u8>>,
    }
    impl Exchange for Wire {
        fn send(&mut self, request: &[u8]) -> Result<(), String> {
            self.requests.push(request.to_vec());
            Ok(())
        }
        fn receive(&mut self) -> Result<Vec<u8>, String> {
            self.answers.pop_front().ok_or("unexpected request".into())
        }
    }
    fn wire(answers: Vec<Vec<u8>>) -> Wire {
        Wire {
            requests: Vec::new(),
            answers: answers.into(),
        }
    }
    fn handle(number: u64) -> Vec<u8> {
        let mut bytes = vec![0x81];
        bytes.extend_from_slice(&number.to_be_bytes());
        bytes
    }

    #[test]
    fn preparation_waits_for_cleanup_and_never_retries_uncertain_requests() {
        let mut pending = wire(vec![vec![0x90], vec![0x91, 1], vec![0x91, 2]]);
        prepare_session(&mut pending).unwrap();
        assert_eq!(pending.requests, [vec![0x10], vec![0x11], vec![0x11]]);
        for replies in [
            vec![vec![0x90, 0]],
            vec![vec![0x90], vec![0x91, 0]],
            vec![vec![0x90], vec![0x91, 3]],
            vec![vec![0x90], vec![0x91, 2, 0]],
            vec![],
        ] {
            let mut refused = wire(replies);
            assert!(prepare_session(&mut refused).is_err());
            assert_eq!(
                refused
                    .requests
                    .iter()
                    .filter(|request| **request == [0x10])
                    .count(),
                1
            );
        }
    }

    /// `9a` for `state`, with a primary name and a hostname.
    fn login_state(state: &[u8]) -> Vec<u8> {
        [&[0x9a][..], state, b"\x06tester\x09td-laptop\x00"].concat()
    }

    fn keys(count: u8) -> Vec<u8> {
        let mut bytes = vec![1, count];
        for key in 1..=count {
            bytes.extend_from_slice(&[key; 4]);
        }
        bytes
    }

    #[test]
    fn a_login_state_answer_has_exactly_one_shape() {
        let decode = |bytes: &[u8]| Answer::decode(bytes).map(|answer| answer.state);
        assert_eq!(decode(&login_state(&[0])), Ok(LoginState::Unenrolled));
        for cause in [0x0a, 0x0b, 0x0c] {
            assert_eq!(
                decode(&login_state(&[2, cause])),
                Ok(LoginState::Unavailable(cause))
            );
        }
        for count in 1..=8 {
            assert_eq!(
                decode(&login_state(&keys(count))),
                Ok(LoginState::Enrolled(
                    (1..=count).map(|key| [key; 4]).collect()
                ))
            );
        }
        // Two keys may share a fingerprint; positions keep them apart.
        assert_eq!(
            decode(&login_state(&[&[1, 2][..], &[7; 8]].concat())),
            Ok(LoginState::Enrolled(vec![[7; 4], [7; 4]]))
        );
        // The longest names, and no hostname at all.
        let longest = [&[0x9a, 0, 32][..], &[b'a'; 32], &[63], &[b'h'; 63], &[0]].concat();
        assert_eq!(decode(&longest), Ok(LoginState::Unenrolled));
        assert_eq!(decode(b"\x9a\x00\x01a\x00\x00"), Ok(LoginState::Unenrolled));
        // The names are kept for the lock surface, as root sent them.
        let kept = Answer::decode(&login_state(&[0])).unwrap();
        assert_eq!((kept.username(), kept.hostname()), ("tester", "td-laptop"));
        let kept = Answer::decode(&longest).unwrap();
        assert_eq!(kept.username(), "a".repeat(32));
        assert_eq!(kept.hostname(), "h".repeat(63));
        assert_eq!(
            Answer::decode(b"\x9a\x00\x01a\x00\x00").unwrap().hostname(),
            ""
        );
        let refused = [
            vec![],
            vec![0x9a],
            vec![0x9b, 0],
            login_state(&[3]),
            // Enrolled with no keys, nine, or a list cut short or long.
            login_state(&[1, 0]),
            login_state(&keys(9)),
            login_state(&[&[1, 2][..], &[7; 7]].concat()),
            login_state(&[&[1, 1][..], &[7; 5]].concat()),
            // A cause that is not the unavailable state's, or none.
            login_state(&[2, 0x09]),
            login_state(&[2, 0x0d]),
            login_state(&[2]),
            // A state byte with a stray byte after it.
            login_state(&[0, 0]),
            // Usernames: empty, too long, not a primary name, not UTF-8.
            b"\x9a\x00\x00\x00\x00".to_vec(),
            [&[0x9a, 0, 33][..], &[b'a'; 33], &[0, 0]].concat(),
            b"\x9a\x00\x04Root\x00\x00".to_vec(),
            b"\x9a\x00\x041abc\x00\x00".to_vec(),
            b"\x9a\x00\x04a.bc\x00\x00".to_vec(),
            b"\x9a\x00\x02a\xff\x00\x00".to_vec(),
            // Hostnames: too long, against firstboot's rules, not UTF-8.
            [&[0x9a, 0, 1, b'a', 64][..], &[b'h'; 64], &[0]].concat(),
            b"\x9a\x00\x01a\x02TD\x00".to_vec(),
            b"\x9a\x00\x01a\x05host-\x00".to_vec(),
            b"\x9a\x00\x01a\x03a b\x00".to_vec(),
            b"\x9a\x00\x01a\x03td\n\x00".to_vec(),
            b"\x9a\x00\x01a\x02\xc3\xb6\x00".to_vec(),
            // Lengths that overrun the answer.
            b"\x9a\x00\x07tester".to_vec(),
            b"\x9a\x00\x06tester\x09td".to_vec(),
            // The revocation byte: missing, nonzero, or followed by more.
            b"\x9a\x00\x06tester\x00".to_vec(),
            b"\x9a\x00\x06tester\x00\x01".to_vec(),
            b"\x9a\x00\x06tester\x00\x00\x00".to_vec(),
        ];
        for answer in refused {
            assert!(decode(&answer).is_err(), "{answer:02x?}");
        }
    }

    /// Each answer after the watch reaches the watcher once stored, with
    /// the handle's own lock released, so the watcher may read it.
    #[test]
    fn every_later_answer_reaches_the_watcher_outside_the_handles_lock() {
        let login = Login::default();
        login.answer(&login_state(&[0])).unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (watched, record) = (login.clone(), Arc::clone(&seen));
        login.watch(move |answer| {
            assert_eq!(watched.current().as_ref(), Some(answer));
            record.lock().unwrap().push(answer.state().clone());
        });
        let mut poll = LoginPoll::new(login.clone());
        let mut w = wire(vec![login_state(&[2, UNREADABLE]), login_state(&keys(1))]);
        poll.read(&mut w).unwrap();
        poll.read(&mut w).unwrap();
        assert_eq!(
            *seen.lock().unwrap(),
            [
                LoginState::Unavailable(UNREADABLE),
                LoginState::Enrolled(vec![[1; 4]])
            ]
        );
    }

    /// Enrolled and unavailable, any cause, start a generation locked;
    /// unenrolled, which is also root's every answer on the live medium,
    /// does not.
    #[test]
    fn enrolled_and_unavailable_answers_lock_and_unenrolled_does_not() {
        let locks = |state: &[u8]| Answer::decode(&login_state(state)).unwrap().locks();
        assert!(!locks(&[0]));
        for count in 1..=8 {
            assert!(locks(&keys(count)));
        }
        for cause in [0x0a, 0x0b, 0x0c] {
            assert!(locks(&[2, cause]));
        }
    }

    /// The answer at connect is kept apart from the handle: an unreadable
    /// state that a poll reads unenrolled before the first paint still
    /// starts the generation locked.
    #[test]
    fn the_connect_answer_is_not_replaced_by_a_later_poll() {
        let login = Login::default();
        let mut w = wire(vec![
            vec![0x90],
            vec![0x91, 2],
            login_state(&[2, UNREADABLE]),
            login_state(&[0]),
        ]);
        let (mut poll, connected) = open_session(&mut w, &login).unwrap();
        poll.next = Some(Instant::now());
        poll.follow(&mut w, &mut crate::secret_client::Client::default())
            .unwrap();
        assert_eq!(login.state(), Some(LoginState::Unenrolled));
        assert_eq!(connected.state(), &LoginState::Unavailable(UNREADABLE));
        assert!(connected.locks());
    }

    #[test]
    fn connect_asks_the_login_state_once_prepared_and_refuses_a_malformed_one() {
        let login = Login::default();
        let mut pending = wire(vec![
            vec![0x90],
            vec![0x91, 1],
            vec![0x91, 2],
            login_state(&keys(2)),
        ]);
        let (poll, connected) = open_session(&mut pending, &login).unwrap();
        assert_eq!(
            pending.requests,
            [vec![0x10], vec![0x11], vec![0x11], vec![0x1a]]
        );
        assert_eq!(
            login.state(),
            Some(LoginState::Enrolled(vec![[1; 4], [2; 4]]))
        );
        assert!(poll.next.is_none());
        assert_eq!(Some(connected), login.current());
        // Nothing is asked before preparation completes.
        let mut unprepared = wire(vec![vec![0x90], vec![0x91, 0]]);
        assert!(open_session(&mut unprepared, &Login::default()).is_err());
        assert!(!unprepared.requests.contains(&vec![0x1a]));
        for answer in [
            vec![0x9a],
            [&login_state(&[0])[..login_state(&[0]).len() - 1], &[1]].concat(),
            vec![0x91, 2],
        ] {
            let login = Login::default();
            let mut refused = wire(vec![vec![0x90], vec![0x91, 2], answer]);
            assert!(open_session(&mut refused, &login).is_err());
            assert_eq!(login.state(), None);
        }
    }

    #[test]
    fn an_unreadable_state_is_asked_again_every_250_ms_until_it_resolves() {
        let login = Login::default();
        let mut poll = LoginPoll::new(login.clone());
        let mut client = crate::secret_client::Client::default();
        let mut w = wire(vec![login_state(&[2, UNREADABLE])]);
        poll.read(&mut w).unwrap();
        assert_eq!(login.state(), Some(LoginState::Unavailable(UNREADABLE)));
        let due = poll.next.unwrap();
        assert!(due > Instant::now() && due <= Instant::now() + LOGIN_POLL);
        assert!(poll.wait() <= LOGIN_POLL);
        // Not yet due: nothing is asked.
        poll.follow(&mut w, &mut client).unwrap();
        assert_eq!(w.requests.len(), 1);
        // Due: asked again, still unreadable, and due again later.
        poll.next = Some(Instant::now());
        assert_eq!(poll.wait(), Duration::ZERO);
        w.answers.push_back(login_state(&[2, UNREADABLE]));
        poll.follow(&mut w, &mut client).unwrap();
        assert_eq!(w.requests, [vec![0x1a], vec![0x1a]]);
        assert!(poll.next.unwrap() > Instant::now());
        // Resolved: no more polling.
        poll.next = Some(Instant::now());
        w.answers.push_back(login_state(&keys(1)));
        poll.follow(&mut w, &mut client).unwrap();
        assert_eq!(login.state(), Some(LoginState::Enrolled(vec![[1; 4]])));
        assert_eq!(poll.next, None);
        assert_eq!(poll.wait(), TICK);
        poll.follow(&mut w, &mut client).unwrap();
        assert_eq!(w.requests.len(), 3);
        // Other unavailable causes are not transient: no polling.
        for cause in [0x0a, 0x0b] {
            w.answers.push_back(login_state(&[2, cause]));
            poll.read(&mut w).unwrap();
            assert_eq!(poll.next, None);
        }
    }

    #[test]
    fn the_login_state_is_asked_after_every_login_operation_ends() {
        let login = Login::default();
        let mut poll = LoginPoll::new(login.clone());
        let mut w = wire(vec![login_state(&[0])]);
        let mut ended = crate::secret_client::Client::with_login_ended();
        poll.follow(&mut w, &mut ended).unwrap();
        assert_eq!(w.requests, [vec![0x1a]]);
        assert_eq!(login.state(), Some(LoginState::Unenrolled));
        // Once only.
        poll.follow(&mut w, &mut ended).unwrap();
        assert_eq!(w.requests.len(), 1);
    }

    #[test]
    fn startup_refuses_human_root_partial_and_multithreaded_identities() {
        let status = "Uid:\t993\t993\t993\t993\nGid:\t993\t993\t993\t993\nThreads:\t1\n";
        assert!(check_status(status).is_ok());
        for invalid in [
            status.replace("993", "1000"),
            status.replace("993", "0"),
            status.replace("Threads:\t1", "Threads:\t2"),
            status.replacen("993", "992", 1),
            status.replace("Gid:", "Absent:"),
            format!("{status}{}", "x".repeat(8192)),
        ] {
            assert!(check_status(&invalid).is_err());
        }
    }

    #[test]
    fn descriptor_admission_checks_holes_and_aliases_before_enumeration() {
        for missing in [0, 1, 2] {
            let enumerated = std::cell::Cell::new(false);
            assert!(audit_descriptors(
                |fd| if fd == missing {
                    Err("closed".into())
                } else {
                    Ok((1, fd as u64))
                },
                || {
                    enumerated.set(true);
                    Ok(vec![0, 1, 2, 3])
                },
            )
            .is_err());
            assert!(!enumerated.get());
        }
        for alias in [1, 2] {
            assert!(audit_descriptors(
                |fd| Ok((1, if fd == alias { 0 } else { fd as u64 })),
                || panic!("alias must fail before enumeration"),
            )
            .is_err());
        }
        assert!(audit_descriptors(|fd| Ok((1, fd as u64)), || Ok(vec![3, 2, 0, 1])).is_ok());
        for descriptors in [vec![0, 1, 2], vec![0, 1, 2, 4], vec![0, 1, 2, 3, 4]] {
            assert!(audit_descriptors(|fd| Ok((1, fd as u64)), || Ok(descriptors)).is_err());
        }
    }

    #[test]
    fn polls_rotate_live_handles_and_retire_each_completion_once() {
        let mut p = Processes::new();
        let mut w = wire(vec![
            handle(1),
            handle(3),
            vec![0x82, 0],
            vec![0x82, 2],
            vec![0x82, 1],
            vec![0x83],
        ]);
        assert_eq!(p.start(&mut w, Program::Home).unwrap(), None);
        assert_eq!(p.start(&mut w, Program::Task).unwrap(), None);
        assert_eq!(w.requests[0], [1]);
        assert_eq!(w.requests[1], [4]);
        assert_eq!(p.poll(&mut w).unwrap(), None);
        assert_eq!(p.poll(&mut w).unwrap(), Some("launched program failed"));
        assert_eq!(p.poll(&mut w).unwrap(), None);
        assert_eq!(p.poll(&mut w).unwrap(), None);
        assert_eq!(w.requests[2], [2, 0, 0, 0, 0, 0, 0, 0, 1]);
        assert_eq!(w.requests[3], [2, 0, 0, 0, 0, 0, 0, 0, 3]);
        assert_eq!(w.requests[4], w.requests[2]);
        assert_eq!(w.requests[5], [3]);
    }

    #[test]
    fn selected_programs_send_only_their_fixed_authority_request() {
        for (terminal, expected) in [
            (Program::Claude, 6),
            (Program::TaskManager, 7),
            (Program::Editor, 8),
            (Program::Photo, 9),
            (Program::Review, 0x0a),
            (Program::Dua, 0x0b),
            (Program::Agent, 0x0c),
        ] {
            let mut processes = Processes::new();
            let mut wire = wire(vec![handle(1)]);
            assert_eq!(processes.start(&mut wire, terminal).unwrap(), None);
            assert_eq!(wire.requests, [vec![expected]]);
        }
    }

    #[test]
    fn invalid_and_reused_handles_are_fatal_but_capacity_is_bounded() {
        for answer in [
            handle(0),
            vec![0x81],
            vec![0x81, 1],
            vec![0xff, 3],
            vec![0xff, 1, 0],
        ] {
            assert!(Processes::new()
                .start(&mut wire(vec![answer]), Program::Home)
                .is_err());
        }
        let mut p = Processes::new();
        let mut w = wire(vec![handle(1), vec![0x82, 1], handle(1)]);
        p.start(&mut w, Program::Home).unwrap();
        p.poll(&mut w).unwrap();
        assert!(p.start(&mut w, Program::Home).is_err());
        let mut p = Processes::new();
        let mut w = wire((1..=16).map(handle).collect());
        for _ in 0..16 {
            assert_eq!(p.start(&mut w, Program::Home).unwrap(), None);
        }
        assert!(p.start(&mut w, Program::Home).unwrap().is_some());
        assert_eq!(w.requests.len(), 16);
    }

    #[test]
    fn malformed_status_and_heartbeat_answers_are_fatal() {
        for status in [
            vec![],
            vec![0x82],
            vec![0x82, 3],
            vec![0x82, 0, 0],
            vec![0xff, 1],
        ] {
            let mut processes = Processes::new();
            let mut wire = wire(vec![handle(1), status]);
            processes.start(&mut wire, Program::Home).unwrap();
            assert!(processes.poll(&mut wire).is_err());
        }
        for status in [vec![], vec![0x83, 0], vec![0x82, 1]] {
            assert!(Processes::new().poll(&mut wire(vec![status])).is_err());
        }
        for response in [vec![0xff, 1], vec![0xff, 2]] {
            let mut processes = Processes::new();
            assert!(processes
                .start(&mut wire(vec![response]), Program::Home)
                .unwrap()
                .is_some());
            assert!(processes.handles.is_empty());
        }
    }

    #[test]
    fn worker_never_retries_a_request_after_uncertain_delivery() {
        struct Broken(std::rc::Rc<std::cell::Cell<usize>>);
        impl Exchange for Broken {
            fn send(&mut self, request: &[u8]) -> Result<(), String> {
                assert_eq!(request, [1]);
                self.0.set(self.0.get() + 1);
                Ok(())
            }
            fn receive(&mut self) -> Result<Vec<u8>, String> {
                Err("response lost after delivery".into())
            }
        }
        let (send, receive) = mpsc::sync_channel(QUEUE_CAPACITY);
        assert!(send.try_send(Work::Program(Program::Home)).is_ok());
        drop(send);
        let calls = std::rc::Rc::new(std::cell::Cell::new(0));
        assert!(worker(
            Broken(calls.clone()),
            receive,
            LoginPoll::new(Login::default())
        )
        .is_err());
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn launcher_queue_never_blocks_input_or_retries_a_delivered_request() {
        let (send, receive) = mpsc::sync_channel(QUEUE_CAPACITY);
        let launcher = Launcher {
            send,
            login: Login::default(),
            connected: None,
        };
        assert!(launcher.launch().is_ok());
        assert!(launcher.launch().is_err());
        assert!(matches!(
            receive.try_recv(),
            Ok(Work::Program(Program::Home))
        ));
        assert!(launcher.launch_selected(Program::Task).is_ok());
        assert!(matches!(
            receive.try_recv(),
            Ok(Work::Program(Program::Task))
        ));
        assert!(receive.try_recv().is_err());
        drop(receive);
        assert!(launcher.launch().is_err());
    }
}

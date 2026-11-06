//! The window (DESIGN.md §2, §4): td-ui's widget window drives the `App` with
//! its inputs, and the `App` talks to its broker, the `td-agent serve` this
//! starts when there is none, by messages over a socket. What it holds is what
//! the person sees and has typed; everything else is the broker's, and goes on
//! when this window closes. `attach` is the same socket as a pipe, for a window
//! on another machine at the far end of ssh, whose window's way there is
//! `--remote`: its connection, unlike one on this machine, can fail and come back.

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use td_remote::{Host, RemoteProgram};

use td_ui::control_socket::Socket;
use td_ui::control_worker::Worker;
use td_ui::driven::{self, Payload};
use td_ui::raster::{Composition, Raster, Scale, Surface};
use td_ui::window::{Clipboard, Flow, Handler, Input};

use crate::control::Remote;
use crate::peer::Peer;
use crate::store::{Id, StateDir};
use crate::ui::{App, Request};
use crate::window_wire::{self, Showing};

/// The longest a turn waits before hearing the broker again.
const POLL_MS: u64 = 50;
const CONTROL_PER_TURN: usize = 4;
/// How long a broker just started has to begin listening.
const STARTING: Duration = Duration::from_secs(20);

/// The socket a window on this machine reaches its broker by.
pub const WINDOW_SOCKET: &str = "window.sock";
/// The socket `attach` reaches it by, for a window at the far end of ssh.
pub const ATTACH_SOCKET: &str = "attach.sock";
/// The socket that ends the broker when anything dials it, version or no.
pub const STOP_SOCKET: &str = "stop.sock";
/// Where a broker started here says what it has to say.
pub const LOG: &str = "serve.log";
const LOG_KEPT: u64 = 1024 * 1024;

/// What names the launch a broker is to come by, and the session's runtime
/// directory it goes under (td-net's launch sets them, `net/src/launch.rs`).
const LAUNCHER: &str = "TD_AGENT_LAUNCHER";
const SESSION_RUNTIME: &str = "TD_AGENT_SESSION_RUNTIME";
/// Where a td image has its td-agent.
const SYSTEM_PROGRAM: &str = "/bin/td-agent";

/// Messages one turn of the window reads: a long log's opening is thousands, and
/// the window stays drawn while they come.
const HEARD_PER_TURN: usize = 1024;

/// Reaches the broker over `state` by the socket `name`, starting one when
/// nothing answers there.
fn dial(state: &Path, name: &str, program: &Path) -> Result<UnixStream, String> {
    let path = state.join(name);
    let refused = |e: &io::Error| {
        matches!(
            e.kind(),
            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
        )
    };
    match UnixStream::connect(&path) {
        Ok(stream) => return Ok(stream),
        Err(e) if refused(&e) => {}
        Err(e) => return Err(format!("{}: {e}", path.display())),
    }
    let (mut broker, mut from) = start(state, program)?;
    let deadline = Instant::now() + STARTING;
    loop {
        match UnixStream::connect(&path) {
            Ok(stream) => return Ok(stream),
            Err(e) if !refused(&e) => return Err(format!("{}: {e}", path.display())),
            Err(_) => {}
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "td-agent serve did not start listening at {}; it says why in {}",
                path.display(),
                state.join(LOG).display()
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
        if let Ok(Some(status)) = broker.try_wait() {
            let said = said_since(&state.join(LOG), from);
            // Refused by a configuration, say: that is said now, not at the
            // deadline. One that lost to a broker starting is as good; one that
            // lost to a broker ending is not, so another is tried.
            if !status.success() && !said.contains(crate::store::SERVING) {
                let said = said.trim().trim_start_matches("td-agent: ");
                return Err(if said.is_empty() {
                    format!(
                        "td-agent serve ended ({status}); see {}",
                        state.join(LOG).display()
                    )
                } else {
                    said.to_string()
                });
            }
            std::thread::sleep(Duration::from_millis(500));
            (broker, from) = start(state, program)?;
        }
    }
}

/// What a log has had added past `from` bytes.
fn said_since(log: &Path, from: u64) -> String {
    let bytes = std::fs::read(log).unwrap_or_default();
    let added = usize::try_from(from)
        .ok()
        .and_then(|from| bytes.get(from..))
        .unwrap_or_default();
    String::from_utf8_lossy(added).into_owned()
}

/// Puts `serve.log` aside for one old copy once it has grown. It stays the same
/// file, since the broker and its conversations hold it open to add to.
pub(crate) fn rotate_log(state: &Path) {
    let log = state.join(LOG);
    if std::fs::metadata(&log).is_ok_and(|log| log.len() > LOG_KEPT)
        && std::fs::copy(&log, log.with_extension("log.old")).is_ok()
    {
        let _ = std::fs::OpenOptions::new()
            .write(true)
            .open(&log)
            .and_then(|log| log.set_len(0));
    }
}

/// Whether `launcher` launches `program`, as td-net's launch would find it: a
/// variable left in a shell does not pair this with another build's or with
/// anything else.
fn launches(launcher: &Path, program: &Path) -> bool {
    let beside = launcher
        .parent()
        .map(|dir| dir.join("td-agent"))
        .and_then(|beside| std::fs::canonicalize(beside).ok());
    beside.is_some() && beside == std::fs::canonicalize(program).ok()
}

/// What a broker lacks of what td-authd starts the image's td-agent with from the
/// launcher's card (`desktop_environment` in `td-authd/src/launch.rs`, which this
/// follows), as one that came over ssh does. Only the image's own program is given
/// these: another build has its launch to name them, and one under test must not
/// find the person's fetch service.
pub fn system_environment(program: &Path) -> Vec<(&'static str, OsString)> {
    let Ok(me) = std::fs::metadata("/proc/self") else {
        return Vec::new();
    };
    lacking(program, Path::new(SYSTEM_PROGRAM), me.uid(), |name| {
        std::env::var_os(name)
    })
}

fn lacking(
    program: &Path,
    system: &Path,
    uid: u32,
    given: impl Fn(&str) -> Option<OsString>,
) -> Vec<(&'static str, OsString)> {
    let is_system = std::fs::canonicalize(system)
        .is_ok_and(|system| std::fs::canonicalize(program).is_ok_and(|ours| ours == system));
    if !is_system {
        return Vec::new();
    }
    [
        (
            "XDG_RUNTIME_DIR",
            OsString::from(format!("/run/user/{uid}")),
        ),
        (crate::jail::JAIL_VAR, OsString::from("/bin/td-jail")),
        (crate::jail::TXT_VAR, OsString::from("/bin/td-txt")),
    ]
    .into_iter()
    .filter(|(name, _)| given(name).is_none_or(|value| value.is_empty()))
    .collect()
}

/// How a broker is started: by the launch that started this process, where one
/// did, so that the fetch service and the relay it gives the broker last as long
/// as it does, where the ones it gave this process end with this process; or as
/// this program. A launch with no session to go under refuses, and says so.
fn broker_command(program: &Path) -> Command {
    let launcher = std::env::var_os(LAUNCHER)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|launcher| launches(launcher, program));
    let Some(launcher) = launcher else {
        return Command::new(program);
    };
    let mut command = Command::new(launcher);
    command.arg0("td-agent");
    match std::env::var_os(SESSION_RUNTIME).filter(|value| !value.is_empty()) {
        Some(runtime) => command.env("XDG_RUNTIME_DIR", runtime),
        None => command.env_remove("XDG_RUNTIME_DIR"),
    };
    command
}

/// Starts a broker of this process's environment, which it outlives, and says
/// where in the log its words begin. It holds nothing of this process's: not
/// its directory, nor its descriptors, nor the display's.
fn start(state: &Path, program: &Path) -> Result<(Child, u64), String> {
    StateDir::at(state.to_path_buf()).ensure()?;
    rotate_log(state);
    let log = state.join(LOG);
    let opened = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&log)
        .map_err(|e| format!("{}: {e}", log.display()))?;
    let from = opened
        .metadata()
        .map_err(|e| format!("{}: {e}", log.display()))?
        .len();
    let child = broker_command(program)
        .arg("serve")
        .current_dir("/")
        .env_remove("WAYLAND_SOCKET")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(opened)
        .process_group(0)
        .spawn()
        .map_err(|e| format!("starting td-agent serve: {e}"))?;
    Ok((child, from))
}

/// A connection to the broker. One that was ending as this dialled closes
/// during the hello, so that is tried again.
fn attached(state: &Path, program: &Path) -> Result<Peer, String> {
    let mut tries = 3;
    loop {
        let stream = dial(state, WINDOW_SOCKET, program)?;
        match Peer::over(stream) {
            Ok(peer) => return Ok(peer),
            Err(why) if tries == 0 => return Err(format!("td-agent serve: {why}")),
            Err(_) => tries -= 1,
        }
    }
}

/// Tries at a far side that has gone: at once, unless the last connection did not
/// last, then ever more slowly.
const RETRY_LONGEST: Duration = Duration::from_secs(30);
const RETRY_FIRST: Duration = Duration::from_secs(1);
/// How long a connection must last to have been one worth trying again at once.
const HEALTHY: Duration = Duration::from_secs(30);
/// The different reasons a window says of one outage.
const REASONS_SAID: usize = 16;

/// The broker a window is for.
pub enum Broker {
    /// This machine's, over the state directory, started by this program.
    Here { state: StateDir, program: PathBuf },
    /// Another's, at the far end of ssh.
    Over { host: Host, program: RemoteProgram },
}

/// What one try at the far side came to.
type Tried = Result<Peer, crate::peer::Unreached>;

/// How long to wait before trying again, given how long the connection lasted and
/// the last wait: one that keeps failing just after it succeeds must not go round
/// as fast as it can log in.
fn pace(lasted: Duration, last: Duration) -> Duration {
    if lasted >= HEALTHY {
        Duration::ZERO
    } else {
        last.saturating_mul(2).clamp(RETRY_FIRST, RETRY_LONGEST)
    }
}

/// Tries a far side on a thread until one try comes to something final: ssh may
/// be waiting for a machine that does not answer, and the window goes on being
/// drawn. Dropping this ends the tries, but for one under way.
struct Reconnecting {
    tried: Receiver<Tried>,
    /// Wakes the wait for the next try.
    hurry: mpsc::Sender<()>,
    /// What the failed tries have said, for the person.
    said: Vec<String>,
}

impl Reconnecting {
    /// `connect` is told whether a person asked for this try: only the first may
    /// have been, and only that one may ask them anything.
    fn start(
        connect: impl FnMut(bool) -> Tried + Send + 'static,
        asked: bool,
        delay: Duration,
    ) -> Result<Self, String> {
        Self::at_pace(connect, asked, delay, RETRY_FIRST, RETRY_LONGEST)
    }

    fn at_pace(
        mut connect: impl FnMut(bool) -> Tried + Send + 'static,
        mut asked: bool,
        delay: Duration,
        first: Duration,
        longest: Duration,
    ) -> Result<Self, String> {
        let (tell, tried) = mpsc::channel();
        let (hurry, hurried) = mpsc::channel::<()>();
        std::thread::Builder::new()
            .name("td-agent-reconnect".into())
            .spawn(move || {
                let mut wait = first;
                let waited = |wait: Duration| {
                    !matches!(
                        hurried.recv_timeout(wait),
                        Err(mpsc::RecvTimeoutError::Disconnected)
                    )
                };
                if !delay.is_zero() && !waited(delay) {
                    return;
                }
                loop {
                    let this = connect(std::mem::take(&mut asked));
                    // Presses during that one are all answered by it.
                    while hurried.try_recv().is_ok() {}
                    let done = !matches!(&this, Err(unreached) if !unreached.lasting);
                    if tell.send(this).is_err() || done || !waited(wait) {
                        return;
                    }
                    wait = wait.saturating_mul(2).min(longest);
                }
            })
            .map_err(|e| format!("a thread: {e}"))?;
        Ok(Self {
            tried,
            hurry,
            said: Vec::new(),
        })
    }

    /// Whether `why` has yet to be said of this outage.
    fn newly_said(&mut self, why: &str) -> bool {
        if self.said.len() >= REASONS_SAID || self.said.iter().any(|said| said == why) {
            return false;
        }
        self.said.push(why.to_string());
        true
    }
}

/// How long a message sent is kept in case its connection had already died:
/// longer than ssh takes to notice (`td_remote`'s keepalives say a minute).
const UNCONFIRMED: Duration = Duration::from_secs(120);
const UNCONFIRMED_KEPT: usize = 64;

/// Messages sent over ssh and not yet heard of. A send goes into ssh's queue
/// whether or not anything is at the far end, and the composer has let it go.
#[derive(Default)]
struct Unconfirmed {
    sent: std::collections::VecDeque<(Instant, String, Vec<crate::ui::Attached>)>,
}

impl Unconfirmed {
    fn sent(&mut self, text: &str, pictures: &[crate::ui::Attached], at: Instant) {
        if self.sent.len() >= UNCONFIRMED_KEPT {
            self.sent.pop_front();
        }
        self.sent
            .push_back((at, text.to_string(), pictures.to_vec()));
    }

    fn confirmed(&mut self, text: &str) {
        if let Some(at) = self.sent.iter().position(|(_, sent, _)| sent == text) {
            self.sent.remove(at);
        }
    }

    /// `view` says that a message reached the far side.
    fn heard(&mut self, view: &crate::view::View) {
        use crate::protocol::Up;
        use crate::store::Kind;
        use crate::supervisor::Update;
        use crate::view::View;
        if self.sent.is_empty() {
            return;
        }
        // The open conversation's alone: the same words in another's say nothing
        // of this one.
        match view {
            View::Update(_, Update::Up(Up::Event(event)), ..) => {
                if let Kind::User { text, .. } = &event.kind {
                    self.confirmed(text);
                }
            }
            View::Update(
                _,
                Update::Refused {
                    text: Some(text), ..
                },
                ..,
            )
            | View::GiveBack(text, ..)
            | View::Restore(text, _) => self.confirmed(text),
            View::SetQueued(queued) => {
                for text in queued {
                    self.confirmed(text);
                }
            }
            _ => {}
        }
    }

    /// What has yet to be heard of and is young enough to be for this.
    fn take(&mut self, now: Instant) -> Vec<(String, Vec<crate::ui::Attached>)> {
        std::mem::take(&mut self.sent)
            .into_iter()
            .filter(|(at, ..)| now.duration_since(*at) < UNCONFIRMED)
            .map(|(_, text, pictures)| (text, pictures))
            .collect()
    }
}

/// How a far window is doing without its broker.
enum Away {
    /// It is not away: a window on this machine always, which ends with its
    /// connection.
    Connected,
    Trying(Reconnecting),
    /// Back, with its first message once it has one, and waiting for the person
    /// to be done with what they have open: a new `App` under them would close
    /// it, and their next key land elsewhere. That message may send it away, with
    /// what it was showing still shown.
    Ready {
        peer: Peer,
        first: Option<Vec<u8>>,
    },
    /// It told this window to go, or trying cannot help: nothing more is tried
    /// until the person asks something of it.
    Stopped,
}

/// The window, and its connection to its broker.
struct Window {
    app: App,
    peer: Option<Peer>,
    /// Where it is reached again, for a far one, and how it is called.
    far: Option<Far>,
    away: Away,
    title: String,
    /// Sent to a far side and not yet heard of.
    unconfirmed: Unconfirmed,
    /// The conversation that was open when the connection ended: the far side
    /// may have started again on another, and a draft is meant for this one.
    reopen: Option<Id>,
    /// When something other than text was last asked of a far side.
    asked_at: Option<Instant>,
    control: Option<Worker<Payload>>,
    /// Help → Keys was chosen by the live pointer or keyboard: td-ui's window
    /// shows its key list.
    show_keys: bool,
    /// File → Quit was chosen, or the broker has gone.
    quit: bool,
    /// The conversation shown as of what has been asked so far.
    shown: Option<Id>,
    /// A new conversation has been asked for and has not been heard of.
    asked_new: bool,
    /// There was more to read than a turn reads.
    more: bool,
}

/// A far broker, for reaching it again.
struct Far {
    host: Host,
    program: RemoteProgram,
    typeface: bool,
    /// When this connection began, and the wait before the last try.
    since: Instant,
    wait: Duration,
    /// Which window this is to the broker: a try nobody asked for may take over
    /// from its own dead connection, and from no other window.
    token: String,
}

/// Why a broker that says what cannot be read is left.
fn unreadable(why: &str) -> String {
    format!(
        "it said something unreadable, as one of another version would, \
         which td-agent serve --stop ends: {why}"
    )
}

/// A connection to `far`, claimed for this window. Only a person asking displaces
/// another window.
fn connect(host: &Host, program: &RemoteProgram, token: &str, asked: bool) -> Tried {
    let peer = Peer::ssh(host, program, asked)?;
    let claim = Request::Claim {
        token: token.to_string(),
        asked,
    };
    let _ = peer.send(window_wire::encode_asked(&Showing::Nothing, &claim));
    Ok(peer)
}

impl Window {
    fn flow(&self) -> Flow {
        if self.quit {
            Flow::Quit
        } else {
            Flow::Continue
        }
    }

    /// Asks the broker what the app asked, but for what is this machine's.
    fn serve(&mut self) {
        for request in self.app.take_requests() {
            match request {
                Request::Quit => self.quit = true,
                Request::OpenLink(url) => match td_ui::open::url(&url, None) {
                    Ok(_) => self.app.note(format!("opened {url} in the browser")),
                    Err(why) => self.app.note(format!("could not open the link: {why}")),
                },
                request => {
                    // Nothing that goes unsent changes what the next one means.
                    if self.far.is_some() && self.peer.is_none() {
                        self.not_connected(request);
                        continue;
                    }
                    match &request {
                        Request::Open(id) => {
                            self.shown = Some(id.clone());
                            self.asked_new = false;
                        }
                        Request::New
                        | Request::NewScratch(_)
                        | Request::NewIn(..)
                        | Request::NewFrom(_)
                        | Request::Admit { .. } => self.asked_new = true,
                        _ => {}
                    }
                    let message = window_wire::encode_asked(&self.showing(), &request);
                    let sent = self.peer.as_ref().is_some_and(|peer| peer.send(message));
                    match (sent, self.far.is_some()) {
                        (true, true) => self.sent_far(&request),
                        (true, false) => {}
                        (false, true) => {
                            self.gone("it stopped reading");
                            self.not_connected(request);
                        }
                        (false, false) => {
                            // Not lost without a word: the composer has let it go.
                            if let Some((text, _)) = request.as_typed() {
                                let _ = writeln!(
                                    io::stderr().lock(),
                                    "td-agent: a message was not sent:\n{text}"
                                );
                            }
                            self.gone("it stopped reading");
                        }
                    }
                }
            }
        }
    }

    /// What was sent may yet not arrive, ssh being the last to notice.
    fn sent_far(&mut self, request: &Request) {
        if let Some((text, pictures)) = request.as_typed() {
            self.unconfirmed.sent(&text, &pictures, Instant::now());
        } else if !matches!(
            request,
            Request::Open(_)
                | Request::ListFolders { .. }
                | Request::ListPrompts
                | Request::Redraw
                | Request::Commands
        ) {
            self.asked_at = Some(Instant::now());
        }
    }

    /// A far window with no connection keeps what the person typed, and does the
    /// rest when they ask again; asking is also what a stopped window waits for,
    /// and a reason to stop waiting for the next try.
    fn not_connected(&mut self, request: Request) {
        let why = format!("not connected to {}", self.title_host());
        match request.as_typed() {
            Some((text, pictures)) => self.app.give_back(&text, pictures, why),
            None => {
                if let Request::ListFolders {
                    candidates, from, ..
                } = request
                {
                    self.app.folders(crate::chooser::FoldersAnswer {
                        asked: candidates,
                        from,
                        read: Err(why),
                    });
                } else if let Request::SetPrompt { ask, .. } = request {
                    self.app
                        .prompt_refused(ask, format!("{why}, so it was not saved"));
                } else if !matches!(request, Request::ListPrompts) {
                    self.app.note(format!("{why}, so that was not done"));
                }
            }
        }
        match &self.away {
            Away::Stopped => self.try_again(true),
            Away::Trying(trying) => {
                let _ = trying.hurry.send(());
            }
            Away::Connected | Away::Ready { .. } => {}
        }
    }

    fn title_host(&self) -> &str {
        self.far
            .as_ref()
            .map_or("td-agent serve", |far| far.host.as_str())
    }

    /// What the next request means by the open conversation.
    fn showing(&self) -> Showing {
        match (&self.shown, self.asked_new) {
            (shown, true) => Showing::New(shown.clone()),
            (Some(id), false) => Showing::Conversation(id.clone()),
            (None, false) => Showing::Nothing,
        }
    }

    /// What is shown has been heard. One that changed ends any wait for a new
    /// conversation. One asked for that never came, being refused, leaves this
    /// one shown, as the requests that follow say.
    fn heard_shown(&mut self) {
        let active = self.app.active().cloned();
        if active != self.shown {
            self.asked_new = false;
        }
        self.shown = active;
    }

    /// Shows one thing the broker said, or goes as it says; whether to go on.
    fn told(&mut self, bytes: &[u8]) -> bool {
        match window_wire::decode_view(bytes) {
            // Told to go, it does not take this for a link that failed.
            Ok(crate::view::View::Bye(why)) => {
                self.let_go(&why);
                false
            }
            Ok(view) => {
                self.unconfirmed.heard(&view);
                self.acknowledge(&view);
                let opened = matches!(view, crate::view::View::Opened(_));
                self.app.apply(view);
                if opened {
                    self.reopen_shown();
                    // Whatever comes next, this one goes first.
                    self.serve();
                }
                true
            }
            Err(why) => {
                self.let_go(&unreadable(&why));
                false
            }
        }
    }

    /// Shows what the broker said, as much as a turn should. Nothing has been
    /// asked that was not sent, so what is shown now is what the next is about.
    fn hear(&mut self) {
        self.more = false;
        for _ in 0..HEARD_PER_TURN {
            let heard = match &self.peer {
                Some(peer) => peer.recv(),
                None => return,
            };
            match heard {
                Ok(Some(bytes)) => {
                    if !self.told(&bytes) {
                        return;
                    }
                }
                Ok(None) => return self.heard_shown(),
                Err(why) => return self.gone(&why),
            }
        }
        self.heard_shown();
        self.more = true;
    }

    /// Tells a far broker that a message given back got here, which it has no
    /// other way to tell over a link that may have died under it.
    fn acknowledge(&self, view: &crate::view::View) {
        use crate::view::View;
        if self.far.is_some() && matches!(view, View::GiveBack(..) | View::Restore(..)) {
            if let Some(peer) = &self.peer {
                let _ = peer.send(window_wire::encode_asked(
                    &self.showing(),
                    &Request::Received,
                ));
            }
        }
    }

    /// The broker has gone. A far one may come back, and the window goes on
    /// showing what it has while it does; this machine's is started with a new
    /// window, so the window has nothing to show.
    fn gone(&mut self, why: &str) {
        if self.far.is_some() {
            if self.peer.is_some() {
                self.lost_link(&format!(
                    "the connection ended, and what was running there goes on; connecting again: {why}"
                ));
                self.try_again(false);
            }
            return;
        }
        if self.peer.take().is_some() {
            let _ = writeln!(
                io::stderr().lock(),
                "td-agent: the connection to td-agent serve ended, so the window closes; what was running goes on: {why}"
            );
        }
        self.quit = true;
    }

    /// The broker has let this window go, by name. A far one tries no more until
    /// asked: another window taking over, for one, would otherwise take it back.
    fn let_go(&mut self, why: &str) {
        if self.far.is_none() {
            // The composer goes with this window, without a word otherwise.
            if !self.app.composed().is_empty() {
                let _ = writeln!(
                    io::stderr().lock(),
                    "td-agent: {why}; what was in the composer:\n{}",
                    self.app.composed()
                );
            }
            return self.gone(why);
        }
        if self.peer.is_some() {
            self.lost_link(why);
        } else {
            self.app.note(why);
        }
        self.stop_trying();
    }

    /// What the connection's end means for what is on the way.
    fn lost_link(&mut self, why: &str) {
        self.peer = None;
        self.reopen = self.shown.take().or(self.reopen.take());
        self.asked_new = false;
        self.more = false;
        let Some(far) = &self.far else {
            return;
        };
        let host = far.host.as_str().to_string();
        self.title = format!("td-agent: {host}: not connected");
        self.app.set_away(Some(format!("not connected to {host}")));
        self.app.note(format!("{host}: {why}"));
        self.app.prompt_link_lost(format!(
            "the connection to {host} ended before it answered, so this may or may not be saved: Save again once connected"
        ));
        let now = Instant::now();
        for (text, pictures) in self.unconfirmed.take(now) {
            self.app.give_back(
                &text,
                pictures,
                format!("the connection to {host} ended, and this may not have reached it"),
            );
        }
        if self
            .asked_at
            .take()
            .is_some_and(|at| now.duration_since(at) < UNCONFIRMED)
        {
            self.app.note(format!(
                "what was last asked of {host} may not have reached it, an interrupt say: look when it is back"
            ));
        }
    }

    /// Starts trying for the far side, because a person asked or at its pace.
    fn try_again(&mut self, asked: bool) {
        let Some(far) = &mut self.far else {
            return;
        };
        let delay = if asked {
            Duration::ZERO
        } else {
            far.wait = pace(far.since.elapsed(), far.wait);
            far.wait
        };
        let (host, program, token) = (far.host.clone(), far.program.clone(), far.token.clone());
        let trying = Reconnecting::start(
            move |asked| connect(&host, &program, &token, asked),
            asked,
            delay,
        );
        match trying {
            Ok(trying) => {
                self.away = Away::Trying(trying);
                let host = far.host.as_str();
                self.app.set_away(Some(format!("not connected to {host}")));
            }
            Err(e) => {
                self.app.note(format!("cannot connect again: {e}"));
                self.stop_trying();
            }
        }
    }

    fn stop_trying(&mut self) {
        self.away = Away::Stopped;
        let host = self.title_host().to_string();
        self.app.set_away(Some(format!(
            "not connected to {host}: Return in composer retries"
        )));
    }

    /// Whether trying for the far side again has come to anything.
    fn reconnect(&mut self) {
        if let Away::Trying(trying) = &mut self.away {
            let tried = match trying.tried.try_recv() {
                Ok(tried) => tried,
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    self.app.note("connecting again stopped");
                    return self.stop_trying();
                }
            };
            match tried {
                Ok(peer) => {
                    if let Some(far) = &mut self.far {
                        far.since = Instant::now();
                    }
                    self.away = Away::Ready { peer, first: None };
                }
                Err(unreached) if unreached.lasting => {
                    self.app
                        .note(format!("connecting again has stopped: {}", unreached.why));
                    return self.stop_trying();
                }
                Err(unreached) => {
                    if trying.newly_said(&unreached.why) {
                        self.app
                            .note(format!("still not connected: {}", unreached.why));
                    }
                    return;
                }
            }
        }
        let Away::Ready { peer, first } = &mut self.away else {
            return;
        };
        if first.is_none() {
            match peer.recv() {
                Ok(Some(bytes)) => *first = Some(bytes),
                Ok(None) => return,
                Err(why) => {
                    self.app.note(format!("still not connected: {why}"));
                    return self.try_again(false);
                }
            }
        }
        // Sent away, it goes on showing what it has.
        let going = first
            .as_deref()
            .and_then(|first| match window_wire::decode_view(first) {
                Ok(crate::view::View::Bye(why)) => Some(why),
                Ok(_) => None,
                Err(why) => Some(unreadable(&why)),
            });
        if let Some(why) = going {
            return self.let_go(&why);
        }
        if self.app.holding_input() {
            return;
        }
        if let Away::Ready { peer, first } = std::mem::replace(&mut self.away, Away::Connected) {
            self.reconnected(peer);
            if let Some(first) = first {
                self.told(&first);
            }
        }
    }

    /// The far side is back. It says everything over, as to any window, so the
    /// window starts from nothing but what the person had in hand.
    fn reconnected(&mut self, peer: Peer) {
        let Some(far) = &mut self.far else {
            return;
        };
        let (typeface, title) = (far.typeface, format!("td-agent: {}", far.host.as_str()));
        match App::new(self.app.surface(), None, crate::config::Mode::default()) {
            Ok(mut fresh) => {
                fresh.set_zone(td_civil::Zone::local());
                fresh.set_icons(typeface);
                fresh.take_over_from(&mut self.app);
                self.app = fresh;
                self.peer = Some(peer);
                self.title = title;
                self.app.note("connected again");
            }
            Err(e) => {
                self.app
                    .note(format!("cannot show the far side again: {e}"));
                self.stop_trying();
            }
        }
    }

    /// Back on what was open when the connection ended, once told what is open
    /// now. A message sent before then is given back as for another conversation.
    fn reopen_shown(&mut self) {
        let Some(id) = self.reopen.take() else {
            return;
        };
        if self.app.active() == Some(&id) {
            // Before anything else is heard this is what is shown, should the next
            // thing be this connection's end.
            self.shown = Some(id);
            return;
        }
        if self.app.opens(&id) {
            self.app.open(id);
        } else {
            self.app.note(
                "the conversation that was open has since gone, and this is another: mind what is in the composer",
            );
        }
    }

    /// Answers what the control socket asked, through the same paths the
    /// window's inputs take.
    fn control(&mut self) {
        for _ in 0..CONTROL_PER_TURN {
            let job = match self.control.as_ref().map(Worker::try_request) {
                Some(Ok(job)) => job,
                Some(Err(e)) => {
                    let _ = writeln!(
                        io::stderr().lock(),
                        "td-agent: the control socket stopped: {e}"
                    );
                    self.control = None;
                    None
                }
                None => None,
            };
            let Some(job) = job else {
                break;
            };
            let mut remote = Remote { app: &mut self.app };
            if let Err(e) =
                job.respond_with(|payload| driven::request(&mut remote, payload.bytes()))
            {
                let _ = writeln!(io::stderr().lock(), "td-agent: a control reply: {e}");
            }
            self.serve();
        }
    }
}

impl Handler for Window {
    fn app_id(&self) -> &str {
        "td-agent"
    }

    fn motion(&mut self, at: Option<(i64, i64)>) {
        self.app.motion(at);
    }

    fn title(&self) -> &str {
        &self.title
    }

    fn input(&mut self, input: Input<'_>, clipboard: &mut dyn Clipboard) -> Flow {
        if input == Input::Close {
            return Flow::Quit;
        }
        // The live input's choice only: `control` delivers the seam's
        // through `App::input`, which reports none.
        self.show_keys |= self.app.input_live(input, clipboard);
        self.serve();
        self.flow()
    }

    fn poll(&mut self, now: u64) -> Flow {
        self.app.tick(now);
        self.reconnect();
        self.hear();
        self.control();
        self.serve();
        self.flow()
    }

    fn wait_ms(&self, _now: u64) -> u64 {
        if self.more {
            0
        } else {
            POLL_MS
        }
    }

    fn needs_redraw(&self) -> bool {
        self.app.dirty()
    }

    fn paint(&mut self, raster: &mut Raster<'_, '_>, surface: Surface) -> Result<(), String> {
        if surface != self.app.surface() {
            self.app.resize(surface);
        }
        raster
            .paint(&self.app, surface.bounds())
            .map_err(|e| e.to_string())?;
        self.app.painted();
        Ok(())
    }

    fn notice(&mut self, message: &str) {
        let _ = writeln!(io::stderr().lock(), "td-agent: window: {message}");
    }

    fn keys(&self) -> Vec<td_ui::keys::Section> {
        self.app.key_list()
    }

    fn take_show_keys(&mut self) -> bool {
        std::mem::take(&mut self.show_keys)
    }
}

/// Runs a window over `broker` until it closes, and serves `control` when given.
/// The first connection is made before there is a window: ssh's questions and
/// any refusal go to whoever ran this, and a window that could show nothing does
/// not open.
pub fn run(broker: Broker, control: Option<PathBuf>) -> Result<(), String> {
    // Its descriptor is not closed on exec, so the broker would hold the
    // display's connection open past this window, as td-term's child would.
    if std::env::var_os("WAYLAND_SOCKET").is_some() {
        return Err("td-agent does not take an inherited WAYLAND_SOCKET".into());
    }
    let control = match control {
        Some(path) => Some(
            Socket::bind(&path)
                .and_then(Worker::start)
                .map_err(|e| format!("control socket {}: {e}", path.display()))?,
        ),
        None => None,
    };
    let endpoint = td_ui::wayland::endpoint(
        None,
        std::env::var_os("WAYLAND_DISPLAY"),
        std::env::var_os("XDG_RUNTIME_DIR"),
    )?;
    let stream = td_ui::wayland::connect(endpoint)?;
    let surface = Surface::new(1280, 800, Scale::default()).map_err(|e| e.to_string())?;
    let mut app = App::new(surface, None, crate::config::Mode::default())?;
    // Its times are shown in this machine's zone.
    app.set_zone(td_civil::Zone::local());
    // Four styles: a reply's bold and italic Markdown draws in its own.
    let typeface = td_ui::pinned_face::load_styled_or_note(
        "td-agent",
        std::env::var_os(td_ui::pinned_face::SETTING).as_deref(),
    );
    // The pinned face draws the transcript's icons; the bitmap one has
    // none, so its buttons keep their captions.
    app.set_icons(typeface.is_some());
    let (peer, far, title) = match broker {
        Broker::Here { state, program } => (
            attached(state.root(), &program)?,
            None,
            "td-agent".to_string(),
        ),
        Broker::Over { host, program } => {
            let token = Id::random()?.as_str().to_string();
            let peer = connect(&host, &program, &token, true).map_err(|unreached| unreached.why)?;
            let title = format!("td-agent: {}", host.as_str());
            let far = Far {
                host,
                program,
                typeface: typeface.is_some(),
                since: Instant::now(),
                wait: Duration::ZERO,
                token,
            };
            (peer, Some(far), title)
        }
    };
    let mut window = Window {
        app,
        peer: Some(peer),
        far,
        away: Away::Connected,
        title,
        unconfirmed: Unconfirmed::default(),
        reopen: None,
        asked_at: None,
        control,
        show_keys: false,
        quit: false,
        shown: None,
        asked_new: false,
        more: false,
    };
    let ran = td_ui::window::run(&mut window, stream, std::env::temp_dir(), typeface);
    // What the last input asked goes before the window does.
    window.serve();
    // What could not be told to have got there is not lost without a word.
    for (text, _) in window.unconfirmed.take(Instant::now()) {
        let _ = writeln!(
            io::stderr().lock(),
            "td-agent: a message may not have been sent:\n{text}"
        );
    }
    if let Some(peer) = window.peer.take() {
        peer.finish();
    }
    ran
}

/// Ends the broker of the state directory, whatever it has under way and
/// whatever version it is, and waits for it to have gone.
pub fn stop(state: &StateDir) -> Result<(), String> {
    let path = state.root().join(STOP_SOCKET);
    match UnixStream::connect(&path) {
        Ok(_) => {}
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            let _ = writeln!(io::stdout().lock(), "no td-agent serve is running");
            return Ok(());
        }
        Err(e) => return Err(format!("{}: {e}", path.display())),
    }
    let deadline = Instant::now() + STARTING;
    while path.exists() {
        if Instant::now() >= deadline {
            return Err(format!(
                "td-agent serve has not ended; {} still names it",
                path.display()
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

/// Standard input to the broker's socket and its answers to standard output,
/// until either side ends: a window on another machine's way in, over ssh.
pub fn attach(state: &StateDir, program: &Path) -> Result<(), String> {
    let stream = dial(state.root(), ATTACH_SOCKET, program)?;
    let mut to_broker = stream
        .try_clone()
        .map_err(|e| format!("the connection: {e}"))?;
    let mut from_broker = stream;
    // Its end is noticed by the other direction's, or this process's.
    std::thread::Builder::new()
        .name("td-agent-attach".into())
        .spawn(move || {
            let _ = pipe(&mut io::stdin().lock(), &mut to_broker);
            let _ = to_broker.shutdown(std::net::Shutdown::Write);
        })
        .map_err(|e| format!("a thread: {e}"))?;
    match pipe(&mut from_broker, &mut io::stdout().lock()) {
        // The far window has gone, as it may at any moment.
        Err(e) if e.kind() != io::ErrorKind::BrokenPipe => Err(format!("the connection: {e}")),
        _ => Ok(()),
    }
}

/// Copies until the end, flushing each piece: nothing may wait in a buffer for
/// more that the far side is waiting on an answer to send.
fn pipe(from: &mut impl Read, to: &mut impl Write) -> io::Result<()> {
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = match from.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(read) => read,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        let piece = buffer
            .get(..read)
            .ok_or_else(|| io::Error::other("read past its buffer"))?;
        to.write_all(piece)?;
        to.flush()?;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn wait_for(trying: &Reconnecting) -> Tried {
        trying.tried.recv_timeout(Duration::from_secs(30)).unwrap()
    }

    fn unreached(why: &str, lasting: bool) -> crate::peer::Unreached {
        crate::peer::Unreached {
            why: why.into(),
            lasting,
        }
    }

    /// A far side that is away is tried again, ever more slowly, until one try
    /// comes to something final, and only the first may ask anyone anything.
    #[test]
    fn tries_go_on_until_the_far_side_answers_or_cannot_be_spoken_to() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let (near, far) = UnixStream::pair().unwrap();
        let far = std::thread::spawn(move || Peer::over(far).unwrap());
        let mut near = Some(near);
        let tries = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&tries);
        let (tell_asked, asked) = mpsc::channel();
        let trying = Reconnecting::at_pace(
            move |asked| {
                tell_asked.send(asked).unwrap();
                let this = counted.fetch_add(1, Ordering::SeqCst) + 1;
                if this == 3 {
                    Peer::over(near.take().unwrap()).map_err(|why| unreached(&why, true))
                } else {
                    Err(unreached(&format!("try {this}"), false))
                }
            },
            true,
            Duration::ZERO,
            Duration::from_millis(1),
            Duration::from_millis(2),
        )
        .unwrap();
        // The closure above fails again if asked past the third.
        assert_eq!(wait_for(&trying).err().unwrap().why, "try 1");
        assert_eq!(wait_for(&trying).err().unwrap().why, "try 2");
        assert!(wait_for(&trying).is_ok());
        assert_eq!(
            trying.tried.recv_timeout(Duration::from_secs(30)).err(),
            Some(mpsc::RecvTimeoutError::Disconnected),
            "the tries ended"
        );
        assert_eq!(tries.load(Ordering::SeqCst), 3);
        assert_eq!(asked.try_iter().collect::<Vec<_>>(), [true, false, false]);
        drop(far.join().unwrap());

        let trying = Reconnecting::at_pace(
            |_| Err(unreached("another version", true)),
            false,
            Duration::ZERO,
            Duration::from_millis(1),
            Duration::from_millis(2),
        )
        .unwrap();
        assert!(wait_for(&trying).is_err());
        assert_eq!(
            trying.tried.recv_timeout(Duration::from_secs(30)).err(),
            Some(mpsc::RecvTimeoutError::Disconnected),
            "the tries ended"
        );
    }

    /// The person asking something need not wait out the pace.
    #[test]
    fn a_try_can_be_hurried_and_waits_its_delay_otherwise() {
        let hour = Duration::from_secs(3600);
        let trying =
            Reconnecting::at_pace(|_| Err(unreached("no", false)), false, hour, hour, hour)
                .unwrap();
        assert!(trying.tried.try_recv().is_err());
        trying.hurry.send(()).unwrap();
        assert!(wait_for(&trying).is_err());
        assert!(trying
            .tried
            .recv_timeout(Duration::from_millis(50))
            .is_err());
        trying.hurry.send(()).unwrap();
        assert!(wait_for(&trying).is_err());
    }

    /// The image's own td-agent, however it came, has what the card gives it; what
    /// it was given stays, and no other build finds the person's services.
    #[test]
    fn the_images_td_agent_has_what_the_card_gives_it() {
        let owned = td_fs::TempDir::new("system-environment").unwrap();
        let store = owned.path().join("td-agent");
        std::fs::write(&store, b"").unwrap();
        let link = owned.path().join("bin-td-agent");
        std::os::unix::fs::symlink(&store, &link).unwrap();
        let other = owned.path().join("built-td-agent");
        std::fs::write(&other, b"").unwrap();
        let unset = |_: &str| None;
        assert_eq!(
            lacking(&store, &link, 1000, unset),
            [
                ("XDG_RUNTIME_DIR", OsString::from("/run/user/1000")),
                ("TD_AGENT_JAIL", OsString::from("/bin/td-jail")),
                ("TD_AGENT_TXT", OsString::from("/bin/td-txt")),
            ]
        );
        // By the link too, and an empty value is none.
        assert_eq!(lacking(&link, &link, 1000, |_| Some("".into())).len(), 3);
        let given = |name: &str| (name == "XDG_RUNTIME_DIR").then(|| OsString::from("/mine"));
        assert_eq!(
            lacking(&store, &link, 1000, given)
                .iter()
                .map(|(name, _)| *name)
                .collect::<Vec<_>>(),
            ["TD_AGENT_JAIL", "TD_AGENT_TXT"]
        );
        assert!(lacking(&other, &link, 1000, unset).is_empty());
        assert!(lacking(&store, &owned.path().join("none"), 1000, unset).is_empty());
    }

    /// However many presses came during a try, they bring forward one more.
    #[test]
    fn presses_during_a_try_are_answered_by_it() {
        let hour = Duration::from_secs(3600);
        let (going, go) = mpsc::channel::<()>();
        let (entered, enter) = mpsc::channel::<()>();
        let trying = Reconnecting::at_pace(
            move |_| {
                entered.send(()).unwrap();
                go.recv().unwrap();
                Err(unreached("no", false))
            },
            false,
            Duration::ZERO,
            hour,
            hour,
        )
        .unwrap();
        enter.recv().unwrap();
        for _ in 0..3 {
            trying.hurry.send(()).unwrap();
        }
        going.send(()).unwrap();
        assert!(wait_for(&trying).is_err());
        assert!(enter.recv_timeout(Duration::from_millis(100)).is_err());
    }

    #[test]
    fn a_connection_that_did_not_last_slows_the_next_try() {
        let second = Duration::from_secs(1);
        assert_eq!(pace(HEALTHY, RETRY_LONGEST), Duration::ZERO);
        assert_eq!(pace(second, Duration::ZERO), RETRY_FIRST);
        assert_eq!(pace(second, RETRY_FIRST), 2 * RETRY_FIRST);
        assert_eq!(pace(second, RETRY_LONGEST), RETRY_LONGEST);
    }

    #[test]
    fn each_reason_is_said_once_and_only_so_many() {
        let trying = Reconnecting::at_pace(
            |_| Err(unreached("no", true)),
            false,
            Duration::ZERO,
            Duration::ZERO,
            Duration::ZERO,
        );
        let mut trying = trying.unwrap();
        assert!(trying.newly_said("a"));
        assert!(trying.newly_said("b"));
        assert!(!trying.newly_said("a"));
        for n in 0..REASONS_SAID {
            trying.newly_said(&n.to_string());
        }
        assert!(!trying.newly_said("one too many"));
    }

    #[test]
    fn a_message_is_unconfirmed_until_heard_of_and_only_for_a_while() {
        let user = |text: &str| crate::store::Event {
            seq: 1,
            time: 0,
            kind: crate::store::Kind::User {
                text: text.into(),
                delivery: "queue".into(),
                pictures: Vec::new(),
            },
        };
        let up = |event| crate::supervisor::Update::Up(crate::protocol::Up::Event(event));
        let now = Instant::now();
        let mut kept = Unconfirmed::default();
        kept.heard(&crate::view::View::Note("nothing sent".into()));
        for text in ["a", "c", "d", "b", "e", "f", "old"] {
            kept.sent(text, &[], now);
        }
        kept.heard(&crate::view::View::Update(
            None,
            up(user("a")),
            0,
            Default::default(),
        ));
        // The same words in another conversation say nothing of this one.
        kept.heard(&crate::view::View::Background(
            crate::store::Id::random().unwrap(),
            up(user("b")),
            0,
        ));
        kept.heard(&crate::view::View::SetQueued(vec!["c".into()]));
        kept.heard(&crate::view::View::GiveBack(
            "d".into(),
            Vec::new(),
            "w".into(),
        ));
        kept.heard(&crate::view::View::Replay(
            None,
            Ok(String::new()),
            vec![user("e")],
            Default::default(),
        ));
        kept.heard(&crate::view::View::Note("f".into()));
        kept.sent.get_mut(3).unwrap().0 = now.checked_sub(UNCONFIRMED).unwrap();
        let left: Vec<String> = kept.take(now).into_iter().map(|(text, _)| text).collect();
        assert_eq!(left, ["b", "e", "f"]);
        assert!(kept.take(now).is_empty());
        for at in 0..=UNCONFIRMED_KEPT {
            kept.sent(&at.to_string(), &[], now);
        }
        assert_eq!(kept.sent.len(), UNCONFIRMED_KEPT);
    }

    /// Nobody listening, the tries end by themselves.
    #[test]
    fn a_window_that_stopped_waiting_ends_the_tries() {
        let (ended, end) = mpsc::channel::<()>();
        let trying = Reconnecting::at_pace(
            move |_| {
                let _ = &ended;
                Err(unreached("no", false))
            },
            false,
            Duration::ZERO,
            Duration::from_secs(3600),
            Duration::from_secs(3600),
        )
        .unwrap();
        assert!(wait_for(&trying).is_err());
        drop(trying);
        assert_eq!(
            end.recv_timeout(Duration::from_secs(30)),
            Err(mpsc::RecvTimeoutError::Disconnected)
        );
    }

    #[test]
    fn a_log_put_aside_stays_the_file_a_broker_holds_open() {
        use std::io::Write as _;
        let scratch = td_fs::TempDir::new("td-agent-window").unwrap();
        let log = scratch.path().join(LOG);
        let mut held = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
            .unwrap();
        held.write_all(b"small").unwrap();
        rotate_log(scratch.path());
        assert_eq!(std::fs::read(&log).unwrap(), b"small");
        held.write_all(&vec![b'x'; LOG_KEPT as usize]).unwrap();
        rotate_log(scratch.path());
        assert_eq!(std::fs::metadata(&log).unwrap().len(), 0);
        let old = log.with_extension("log.old");
        assert!(std::fs::read(&old).unwrap().starts_with(b"small"));
        held.write_all(b"after").unwrap();
        assert_eq!(std::fs::read(&log).unwrap(), b"after");
    }

    #[test]
    fn a_log_says_only_what_was_added_and_nothing_of_one_cut_short() {
        let scratch = td_fs::TempDir::new("td-agent-window").unwrap();
        let log = scratch.path().join(LOG);
        std::fs::write(&log, "old\nnew\n").unwrap();
        assert_eq!(said_since(&log, 4), "new\n");
        assert_eq!(said_since(&log, 100), "");
        assert_eq!(said_since(&scratch.path().join("none"), 0), "");
    }

    #[test]
    fn a_pipe_copies_everything_and_ends_with_its_input() {
        let bytes: Vec<u8> = (0..=255).cycle().take(200_000).collect();
        let mut out = Vec::new();
        pipe(&mut bytes.as_slice(), &mut out).unwrap();
        assert_eq!(out, bytes);
    }

    #[test]
    fn a_pipe_says_why_it_could_not_write() {
        struct Full;
        impl Write for Full {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let refused = pipe(&mut b"x".as_slice(), &mut Full).unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::BrokenPipe);
    }
}

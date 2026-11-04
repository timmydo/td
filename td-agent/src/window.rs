//! The window (DESIGN.md §2, §4): td-ui's widget window drives the `App` with
//! its inputs, and the `App` talks to its broker, the `td-agent serve` this
//! starts when there is none, by messages over a socket. What it holds is what
//! the person sees and has typed; everything else is the broker's, and goes on
//! when this window closes. `attach` is the same socket as a pipe, for a window
//! on another machine at the far end of ssh.

use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

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

/// The window, and its connection to its broker.
struct Window {
    app: App,
    peer: Option<Peer>,
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
                    if !sent {
                        // Not lost without a word: the composer has let it go.
                        if let Request::Send(text, _)
                        | Request::Command { typed: text, .. }
                        | Request::Compact(Some(text))
                        | Request::Review(text) = &request
                        {
                            let _ = writeln!(
                                io::stderr().lock(),
                                "td-agent: a message was not sent:\n{text}"
                            );
                        }
                        if let Request::Schedule(crate::schedule::Command::Add { text, .. }) =
                            &request
                        {
                            let _ = writeln!(
                                io::stderr().lock(),
                                "td-agent: a schedule's message was not sent:\n{text}"
                            );
                        }
                        self.gone("it stopped reading");
                    }
                }
            }
        }
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
                Ok(Some(bytes)) => match window_wire::decode_view(&bytes) {
                    Ok(view) => self.app.apply(view),
                    Err(why) => {
                        return self.gone(&format!(
                            "it said something unreadable, as one of another version would, \
                             which td-agent serve --stop ends: {why}"
                        ));
                    }
                },
                Ok(None) => return self.heard_shown(),
                Err(why) => return self.gone(&why),
            }
        }
        self.heard_shown();
        self.more = true;
    }

    /// The broker has gone, so the window has nothing to show.
    fn gone(&mut self, why: &str) {
        if self.peer.take().is_some() {
            let _ = writeln!(
                io::stderr().lock(),
                "td-agent: the connection to td-agent serve ended, so the window closes; what was running goes on: {why}"
            );
        }
        self.quit = true;
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
        "td-agent"
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

/// Runs a window over the broker of the state directory until it closes, and
/// serves `control` when given.
pub fn run(state: &StateDir, program: &Path, control: Option<PathBuf>) -> Result<(), String> {
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
    let peer = attached(state.root(), program)?;
    let mut window = Window {
        app,
        peer: Some(peer),
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

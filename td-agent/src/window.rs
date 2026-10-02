//! The window process (DESIGN.md §2, §4): td-ui's widget window drives
//! the `App` with its inputs, and each turn the session hands the app
//! what the open conversation's process said, serves the driven control
//! socket when there is one, and carries out what the app asked.

use std::path::PathBuf;
use std::time::UNIX_EPOCH;

use td_ui::control_socket::Socket;
use td_ui::control_worker::Worker;
use td_ui::driven::{self, Payload};
use td_ui::raster::{Composition, Raster, Scale, Surface};
use td_ui::window::{Clipboard, Flow, Handler, Input};

use crate::config::Config;
use crate::control::Remote;
use crate::store::{self, Id, Role, StateDir};
use crate::supervisor::{Supervisor, Update};
use crate::ui::{App, Request, Row, RowState};

/// The longest a turn waits before polling the conversation again.
const POLL_MS: u64 = 50;
/// Control requests answered per turn, so a busy client cannot starve
/// the window's own inputs.
const CONTROL_PER_TURN: usize = 4;

/// The window's state and what it owns outside the window.
pub struct Session {
    app: App,
    supervisor: Supervisor,
    state: StateDir,
    control: Option<Worker<Payload>>,
}

impl Session {
    /// Carries out what the app asked.
    fn serve(&mut self) {
        for request in self.app.take_requests() {
            match request {
                Request::Open(id) => {
                    if let Err(reason) = self.supervisor.open(id, None) {
                        self.app.update(Update::Failed { reason }, store::now());
                    }
                }
                Request::New => self.start(),
                Request::Send(text) => {
                    if let Err(e) = self.supervisor.send(text.clone()) {
                        self.app.restore(&text, e);
                    }
                }
                Request::SaveShare(first, total) => {
                    if let Err(e) = self.state.save_share(first, total) {
                        self.app.note(e);
                    }
                }
            }
        }
    }

    /// A new conversation, created by its own process and opened.
    fn start(&mut self) {
        let id = match Id::random() {
            Ok(id) => id,
            Err(e) => {
                self.app.note(format!("a new conversation: {e}"));
                return;
            }
        };
        self.app.add_row(Row {
            id: id.clone(),
            role: Role::Conversation,
            title: Role::Conversation.first_title().to_string(),
            activity: store::now(),
            state: RowState::Starting,
        });
        self.app.set_active(id.clone());
        if let Err(reason) = self.supervisor.open(id, Some(Role::Conversation)) {
            self.app.update(Update::Failed { reason }, store::now());
        }
    }

    /// Answers what the control socket asked, through the same paths the
    /// window's inputs take.
    fn control(&mut self) {
        for _ in 0..CONTROL_PER_TURN {
            let job = match self.control.as_ref().map(Worker::try_request) {
                Some(Ok(job)) => job,
                Some(Err(e)) => {
                    eprintln!("td-agent: the control socket stopped: {e}");
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
                eprintln!("td-agent: a control reply: {e}");
            }
            self.serve();
        }
    }
}

impl Handler for Session {
    fn app_id(&self) -> &str {
        "td-agent"
    }

    fn title(&self) -> &str {
        "td-agent"
    }

    fn input(&mut self, input: Input<'_>, clipboard: &mut dyn Clipboard) -> Flow {
        if input == Input::Close {
            return Flow::Quit;
        }
        self.app.input(input, clipboard);
        self.serve();
        Flow::Continue
    }

    fn poll(&mut self, now: u64) -> Flow {
        self.app.tick(now);
        for update in self.supervisor.poll() {
            self.app.update(update, store::now());
        }
        self.control();
        self.serve();
        Flow::Continue
    }

    fn wait_ms(&self, _now: u64) -> u64 {
        POLL_MS
    }

    fn needs_redraw(&self) -> bool {
        self.app.dirty()
    }

    fn paint(&mut self, raster: &mut Raster<'_, '_>, surface: Surface) -> Result<(), String> {
        if surface != self.app.surface() {
            self.app.resize(surface);
        }
        self.app
            .emit(surface.bounds(), &mut |draw| raster.draw(draw));
        self.app.painted();
        Ok(())
    }

    fn notice(&mut self, message: &str) {
        eprintln!("td-agent: window: {message}");
    }
}

/// The conversations the store holds, as the list shows them, closed.
fn rows(state: &StateDir) -> (Vec<Row>, Vec<String>) {
    let (metas, problems) = state.list();
    let rows = metas
        .into_iter()
        .map(|meta| {
            // A conversation was last active when its log last changed.
            let activity = std::fs::metadata(state.conversation(&meta.id).join("log"))
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(meta.created, |d| d.as_secs());
            Row {
                id: meta.id,
                role: meta.role,
                title: meta.title,
                activity,
                state: RowState::Closed,
            }
        })
        .collect();
    (rows, problems)
}

/// Runs the window process over the state directory until the window
/// closes: it takes the window lock, opens the orchestrator, creating it
/// the first time, and serves `control` when given.
pub fn run(
    config: Config,
    state: StateDir,
    program: PathBuf,
    control: Option<PathBuf>,
) -> Result<(), String> {
    state.ensure()?;
    let _lock = state.lock_window()?;
    let control = match control {
        Some(path) => Some(
            Socket::bind(&path)
                .and_then(Worker::start)
                .map_err(|e| format!("control socket {}: {e}", path.display()))?,
        ),
        None => None,
    };
    let endpoint = td_ui::wayland::endpoint(
        std::env::var_os("WAYLAND_SOCKET"),
        std::env::var_os("WAYLAND_DISPLAY"),
        std::env::var_os("XDG_RUNTIME_DIR"),
    )?;
    let stream = td_ui::wayland::connect(endpoint)?;
    let surface = Surface::new(1280, 800, Scale::default()).map_err(|e| e.to_string())?;
    let mut app = App::new(surface, state.load_share(), config.mode)?;
    for note in &config.notes {
        eprintln!("td-agent: configuration: {note}");
    }
    if !config.notes.is_empty() {
        app.note(format!(
            "{} configuration key(s) are read by later increments; see standard error",
            config.notes.len()
        ));
    }
    let (rows, problems) = rows(&state);
    for problem in &problems {
        eprintln!("td-agent: the store: {problem}");
    }
    if let Some(problem) = problems.last() {
        app.note(format!("the store: {problem}"));
    }
    let orchestrator = rows
        .iter()
        .find(|r| r.role == Role::Orchestrator)
        .map(|r| r.id.clone());
    app.set_rows(rows);
    let mut supervisor = Supervisor::new(program, state.root().to_path_buf());
    let opened = match orchestrator {
        Some(id) => {
            app.set_active(id.clone());
            supervisor.open(id, None)
        }
        None => {
            let id = Id::random()?;
            app.add_row(Row {
                id: id.clone(),
                role: Role::Orchestrator,
                title: Role::Orchestrator.first_title().to_string(),
                activity: store::now(),
                state: RowState::Starting,
            });
            app.set_active(id.clone());
            supervisor.open(id, Some(Role::Orchestrator))
        }
    };
    if let Err(reason) = opened {
        app.update(Update::Failed { reason }, store::now());
    }
    let mut session = Session {
        app,
        supervisor,
        state,
        control,
    };
    let typeface = td_ui::pinned_face::load_or_note(
        "td-agent",
        std::env::var_os(td_ui::pinned_face::SETTING).as_deref(),
    );
    td_ui::window::run(&mut session, stream, std::env::temp_dir(), typeface)
}

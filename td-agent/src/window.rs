//! The window process (DESIGN.md §2, §4): td-ui's widget window drives
//! the `App` with its inputs, and each turn the session hands the app
//! what the conversation processes said, answers their reservations from
//! the day's ledger, serves the driven control socket when there is one,
//! and carries out what the app asked.
//!
//! It also holds what crosses conversations: the API key, read once at
//! startup and handed to each conversation process over its socketpair,
//! and stored from the key dialog, which hands every running process the
//! new one over its socketpair;
//! the day's spending (`accounts::Ledger`); the messages between
//! conversations, which it routes (`post`); and the provider's models
//! list and the key's credit, fetched on a thread of their own
//! (`Fetcher`) so the window never waits on the network.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant, UNIX_EPOCH};

use td_ui::control_socket::Socket;
use td_ui::control_worker::Worker;
use td_ui::driven::{self, Payload};
use td_ui::raster::{Composition, Raster, Scale, Surface};
use td_ui::window::{Clipboard, Flow, Handler, Input};

use crate::accounts::Ledger;
use crate::config::{Client, Config};
use crate::control::Remote;
use crate::key::{self, Secret, Unwritten};
use crate::models::{Credit, Models, MAX_LIST};
use crate::post::{Outbox, Post};
use crate::protocol::{Down, Up};
use crate::store::{self, Event, Id, Kind, Role, StateDir};
use crate::supervisor::{Opened, Supervisor, Update};
use crate::td_fetch;
use crate::ui::{App, Request, Row, RowState};

/// The longest a turn waits before polling the conversation again.
const POLL_MS: u64 = 50;
/// Control requests answered per turn, so a busy client cannot starve
/// the window's own inputs.
const CONTROL_PER_TURN: usize = 4;
/// The least time between two credit fetches.
const CREDIT_EVERY: Duration = Duration::from_secs(20);
/// The longest key record taken.
const MAX_CREDIT: u64 = 64 * 1024;

/// What the fetcher thread is asked for, and what it answers.
enum Job {
    Models,
    Credit,
    /// The key the human stored, for the credit asked after it.
    Key(Secret),
}

enum Fetched {
    Models(Result<Models, String>),
    Credit(Result<Credit, String>),
}

/// The models list and the key's credit, fetched off the window's thread.
struct Fetcher {
    jobs: Sender<Job>,
    results: Receiver<Fetched>,
    last_credit: Option<Instant>,
    /// Whether there is a key to ask the credit of.
    keyed: bool,
}

impl Fetcher {
    fn start(base_url: String, key: Option<Secret>, state: PathBuf) -> Result<Self, String> {
        let keyed = key.is_some();
        let (jobs, work) = mpsc::channel::<Job>();
        let (answer, results) = mpsc::channel();
        std::thread::Builder::new()
            .name("td-agent-fetcher".into())
            .spawn(move || {
                let mut key = key;
                for job in work {
                    let fetched = match job {
                        Job::Models => Fetched::Models(models(&base_url, &state)),
                        Job::Credit => Fetched::Credit(credit(&base_url, key.as_ref())),
                        Job::Key(stored) => {
                            key = Some(stored);
                            continue;
                        }
                    };
                    if answer.send(fetched).is_err() {
                        break;
                    }
                }
            })
            .map_err(|e| format!("the fetcher thread: {e}"))?;
        Ok(Self {
            jobs,
            results,
            last_credit: None,
            keyed,
        })
    }

    fn credit(&mut self, now: Instant) {
        if !self.keyed
            || self
                .last_credit
                .is_some_and(|last| now.duration_since(last) < CREDIT_EVERY)
        {
            return;
        }
        self.last_credit = Some(now);
        let _ = self.jobs.send(Job::Credit);
    }

    /// A key the human stored: its credit is asked for at once.
    fn rekey(&mut self, key: Secret, now: Instant) {
        let _ = self.jobs.send(Job::Key(key));
        self.keyed = true;
        self.last_credit = None;
        self.credit(now);
    }
}

/// `GET /models`, cached in the state directory as it comes.
fn models(base_url: &str, state: &std::path::Path) -> Result<Models, String> {
    let response = td_fetch::get(
        &format!("{base_url}/models"),
        &[("accept", "application/json")],
        Some(MAX_LIST),
        None,
    )
    .map_err(|e| e.to_string())?;
    if response.status != 200 {
        return Err(format!("status {}", response.status));
    }
    let models = Models::from_provider(&response.body)?;
    models.save(state)?;
    Ok(models)
}

/// `GET /key`: the key's credit.
fn credit(base_url: &str, key: Option<&Secret>) -> Result<Credit, String> {
    let key = key.ok_or("no API key")?;
    let authorization = format!("Bearer {}", key.expose());
    let response = td_fetch::get(
        &format!("{base_url}/key"),
        &[
            ("authorization", authorization.as_str()),
            ("accept", "application/json"),
        ],
        Some(MAX_CREDIT),
        // The service drops the key on a redirect; none is followed.
        Some(0),
    )
    .map_err(|e| e.to_string())?;
    if response.status != 200 {
        return Err(format!("status {}", response.status));
    }
    Credit::from_provider(&response.body)
}

/// The window's state and what it owns outside the window.
pub struct Session {
    app: App,
    supervisor: Supervisor,
    state: StateDir,
    control: Option<Worker<Payload>>,
    ledger: Ledger,
    fetcher: Option<Fetcher>,
    client: Client,
    post: Post,
    /// Where the key dialog stores the key; none without a configuration
    /// directory.
    key_path: Option<PathBuf>,
    /// File → Quit was chosen: the window closes.
    quit: bool,
}

impl Session {
    /// Carries out what the app asked.
    fn serve(&mut self) {
        for request in self.app.take_requests() {
            match request {
                Request::Open(id) => self.open(id, None),
                Request::New => self.start(),
                Request::Send(text) => {
                    if let Err(e) = self.supervisor.send(text.clone()) {
                        self.app.restore(&text, e);
                    }
                }
                Request::Retry => {
                    if let Err(e) = self.supervisor.retry() {
                        self.app.note(e);
                    }
                }
                Request::Interrupt => {
                    if let Err(e) = self.supervisor.interrupt() {
                        self.app.note(e);
                    }
                }
                Request::Pause(paused) => {
                    if let Err(e) = self.supervisor.tell(&Down::Pause { paused }) {
                        self.app.note(e);
                    }
                }
                Request::ClearTodo => {
                    if let Err(e) = self.supervisor.tell(&Down::ClearTodo) {
                        self.app.note(e);
                    }
                }
                Request::SaveShare(first, total) => {
                    if let Err(e) = self.state.save_share(first, total) {
                        self.app.note(e);
                    }
                }
                Request::SaveKey { secret, replace } => self.save_key(&secret, replace),
                Request::Quit => self.quit = true,
            }
        }
    }

    /// Stores the key from the dialog (DESIGN.md §6) and hands it to every
    /// conversation process, running or started later, and to the credit
    /// fetch; a stored key is replaced only when `replace` says so.
    fn save_key(&mut self, secret: &Secret, replace: bool) {
        let Some(path) = self.key_path.clone() else {
            self.app.key_refused(
                "there is nowhere to store a key: neither XDG_CONFIG_HOME nor HOME is an absolute path"
                    .into(),
            );
            return;
        };
        match key::write(&path, secret, replace) {
            Ok(stored) => {
                self.supervisor.rekey(stored.clone());
                if let Some(fetcher) = self.fetcher.as_mut() {
                    fetcher.rekey(stored, Instant::now());
                }
                self.app.key_saved(&path.display().to_string());
            }
            Err(Unwritten::Exists) => self.app.key_exists(),
            Err(Unwritten::Refused(why)) => {
                eprintln!("td-agent: saving the key: {why}");
                self.app.key_refused(why);
            }
        }
    }

    fn flow(&self) -> Flow {
        if self.quit {
            Flow::Quit
        } else {
            Flow::Continue
        }
    }

    /// Opens conversation `id`: a process of its own replays its log; one
    /// already running its turn is adopted, and the log is read here.
    fn open(&mut self, id: Id, create: Option<Role>) {
        match self.supervisor.open(id.clone(), create) {
            Ok(Opened::Started) => {}
            Ok(Opened::Adopted) => match store::read_log(&self.state, &id) {
                Ok(events) => self.app.replay(events),
                Err(e) => self.app.note(format!("the conversation's log: {e}")),
            },
            Err(reason) => self.app.update(Update::Failed { reason }, store::now()),
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
            paused: false,
        });
        self.app.set_active(id.clone());
        self.open(id, Some(Role::Conversation));
    }

    /// What the conversation processes said: reservations answered from
    /// the ledger, messages between them routed, the rest to the app.
    fn hear(&mut self) {
        let now = store::now();
        let directory = self.app.directory();
        for (id, update) in self.supervisor.poll() {
            if let Some(note) = self
                .post
                .hear(&id, &update, &mut self.supervisor, &directory)
            {
                eprintln!("td-agent: {note}");
                self.app.note(note);
            }
            match &update {
                Update::Up(Up::Reserve {
                    id: request,
                    amount,
                }) => {
                    let refusal = self.ledger.reserve(&id, *request, *amount, now).err();
                    self.supervisor.answer(
                        &id,
                        &Down::Reservation {
                            id: *request,
                            refusal,
                        },
                    );
                }
                Update::Up(Up::Spent {
                    id: request,
                    amount,
                }) => {
                    if let Err(e) = self.ledger.settle(&id, *request, *amount, now) {
                        self.app.note(e);
                    }
                }
                Update::Restarting { .. } | Update::Failed { .. } => self.ledger.forget(&id),
                Update::Up(Up::Event(Event {
                    kind: Kind::Finished { .. },
                    ..
                })) => {
                    if let Some(fetcher) = self.fetcher.as_mut() {
                        fetcher.credit(Instant::now());
                    }
                }
                _ => {}
            }
            if self.supervisor.open_id() == Some(&id) {
                self.app.update(update, now);
            } else if let Update::Refused { text, reason } = &update {
                // No composer of its own to go back to: said, and kept
                // whole on standard error.
                let note = match text {
                    Some(text) => {
                        eprintln!(
                            "td-agent: conversation {id} refused a message ({reason}):\n{text}"
                        );
                        format!(
                            "conversation {id} refused a message: {reason}; its text is on standard error"
                        )
                    }
                    None => format!("conversation {id} refused: {reason}"),
                };
                self.app.note(note);
            } else {
                self.app.background(&id, &update, now);
            }
        }
        let directory = self.app.directory();
        for note in self.post.deliver(&mut self.supervisor, &directory) {
            eprintln!("td-agent: {note}");
            self.app.note(note);
        }
        let today = self.ledger.today(now);
        self.app.set_today(today);
        self.fetched();
    }

    /// What the fetcher brought.
    fn fetched(&mut self) {
        let Some(fetcher) = self.fetcher.as_ref() else {
            return;
        };
        let results: Vec<Fetched> = fetcher.results.try_iter().collect();
        for fetched in results {
            match fetched {
                Fetched::Models(Ok(models)) => self.show_models(&models),
                Fetched::Models(Err(e)) => {
                    eprintln!("td-agent: the models list: {e}");
                    self.app.note(format!("the models list: {e}"));
                }
                Fetched::Credit(Ok(credit)) => self.app.set_credit(Some(credit.show())),
                Fetched::Credit(Err(e)) => {
                    self.app.set_credit(Some("credit unknown".into()));
                    eprintln!("td-agent: the key's credit: {e}");
                }
            }
        }
    }

    fn show_models(&mut self, models: &Models) {
        let contexts = [&self.client.model, &self.client.orchestrator_model]
            .iter()
            .filter_map(|id| {
                let length = models.find(id)?.context_length?;
                Some((id.to_string(), length))
            })
            .collect();
        self.app.set_models(
            &self.client.model,
            &self.client.orchestrator_model,
            contexts,
        );
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
                paused: meta.paused,
            }
        })
        .collect();
    (rows, problems)
}

/// Runs the window process over the state directory until the window
/// closes: it takes the window lock, opens the orchestrator, creating it
/// the first time, and serves `control` when given. `key` is the API key,
/// or why there is none, which every turn then says; `key_path` is where
/// the key dialog stores one.
pub fn run(
    config: Config,
    key: Result<Secret, String>,
    key_path: Option<PathBuf>,
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
    if let Err(why) = &key {
        eprintln!("td-agent: {why}");
        app.note(why.clone());
    }
    app.set_keyed(key.is_ok());
    app.set_key_path(key_path.as_ref().map(|p| p.display().to_string()));
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
    let client = config.client.clone();
    app.set_models(&client.model, &client.orchestrator_model, Vec::new());
    app.set_limits(client.limits);
    let setup = Down::Setup {
        key: key.clone(),
        client: client.clone(),
    };
    let supervisor = Supervisor::new(program, state.root().to_path_buf(), setup);
    let fetcher = match Fetcher::start(
        client.base_url.clone(),
        key.ok(),
        state.root().to_path_buf(),
    ) {
        Ok(fetcher) => Some(fetcher),
        Err(e) => {
            app.note(e);
            None
        }
    };
    let ledger = Ledger::load(Some(state.root()), client.limits.day, store::now());
    let (outbox, problems) = Outbox::load(&state);
    for problem in &problems {
        eprintln!("td-agent: the outbox: {problem}");
    }
    if let Some(problem) = problems.last() {
        app.note(format!("the outbox: {problem}"));
    }
    let mut session = Session {
        app,
        supervisor,
        state,
        control,
        ledger,
        fetcher,
        client,
        post: Post::new(outbox),
        key_path,
        quit: false,
    };
    // A cached list serves until the provider's comes.
    match Models::load(session.state.root()) {
        Ok(Some(models)) => session.show_models(&models),
        Ok(None) => {}
        Err(e) => session.app.note(format!("the models cache: {e}")),
    }
    if let Some(fetcher) = session.fetcher.as_mut() {
        let _ = fetcher.jobs.send(Job::Models);
        fetcher.credit(Instant::now());
    }
    match orchestrator {
        Some(id) => {
            session.app.set_active(id.clone());
            session.open(id, None);
        }
        None => {
            let id = Id::random()?;
            session.app.add_row(Row {
                id: id.clone(),
                role: Role::Orchestrator,
                title: Role::Orchestrator.first_title().to_string(),
                activity: store::now(),
                state: RowState::Starting,
                paused: false,
            });
            session.app.set_active(id.clone());
            session.open(id, Some(Role::Orchestrator));
        }
    }
    let typeface = td_ui::pinned_face::load_or_note(
        "td-agent",
        std::env::var_os(td_ui::pinned_face::SETTING).as_deref(),
    );
    td_ui::window::run(&mut session, stream, std::env::temp_dir(), typeface)
}

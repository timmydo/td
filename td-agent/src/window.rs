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

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use td_ui::control_socket::Socket;
use td_ui::control_worker::Worker;
use td_ui::driven::{self, Payload};
use td_ui::raster::{Composition, Raster, Scale, Surface};
use td_ui::window::{Clipboard, Flow, Handler, Input};

use crate::accounts::Ledger;
use crate::config::{Client, Config, Template, TemplateShared};
use crate::control::Remote;
use crate::diagnostics::{self, Exported};
use crate::jail::Programs;
use crate::key::{self, Secret, Unwritten};
use crate::models::{Credit, Models, MAX_LIST};
use crate::picker::Offer;
use crate::post::{Outbox, Post};
use crate::protocol::{Down, Up};
use crate::store::{self, Event, Id, Kind, Role, StateDir};
use crate::supervisor::{Opened, Supervisor, Update};
use crate::ui::{App, Card, Request, Row, RowState};
use crate::workspace::{self, Places, Workspace};

/// The longest a turn waits before polling the conversation again.
const POLL_MS: u64 = 50;
/// Whether `workspace` names `remote` and each of `bases` as one of its
/// entries' base for that remote: all a conversation may ask fetched.
fn names(workspace: Option<&Workspace>, remote: &str, bases: &[String]) -> bool {
    let Some(Workspace::Repositories(repositories)) = workspace else {
        return false;
    };
    let named: Vec<&String> = repositories
        .entries
        .iter()
        .filter(|entry| entry.remote == remote)
        .map(|entry| &entry.base)
        .collect();
    !named.is_empty() && bases.iter().all(|base| named.contains(&base))
}

/// The ids a repository workspace is named from before its making gives
/// up: another holds a name rarely.
const NAME_TRIES: usize = 8;
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
    let response = td_fetch_client::get(
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
    let response = td_fetch_client::get(
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
    /// The configuration's `model` key as the window started with it,
    /// which a default chosen here is set over when the file cannot be
    /// read again (DESIGN.md §4).
    model_key: Option<String>,
    post: Post,
    /// Where the key dialog stores the key; none without a configuration
    /// directory.
    key_path: Option<PathBuf>,
    /// File → Quit was chosen: the window closes.
    quit: bool,
    /// The diagnostics export under way, on a thread of its own.
    export: Option<Receiver<Result<Exported, String>>>,
    /// Every key this window has held, which the export looks for.
    keys: Vec<Secret>,
    /// Help → Keys was chosen by the live pointer or keyboard: td-ui's
    /// window shows its key list.
    show_keys: bool,
    /// Where no workspace may be, or why there are no workspaces.
    places: Result<Places, String>,
    /// The configured workspace templates (DESIGN.md §7).
    templates: Vec<Template>,
    /// The remotes admitted (DESIGN.md §7): the configuration's, then
    /// those the human admitted on a card.
    remotes: Vec<crate::git::Admission>,
    /// td-agent's data directory, or why there is none, and the store
    /// fetches done there for repository workspaces.
    data: Result<PathBuf, String>,
    stores: Option<crate::git::Service>,
}

impl Session {
    /// Carries out what the app asked.
    fn serve(&mut self) {
        for request in self.app.take_requests() {
            match request {
                Request::Open(id) => self.open(id, None),
                Request::New => self.start(None),
                Request::NewScratch => self.start_in(None),
                Request::NewIn(folder) => self.start_in(Some(folder)),
                Request::NewFrom(name) => self.start_from(&name),
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
                Request::Choose { model, effort } => {
                    if let Err(e) = self.supervisor.tell(&Down::Choose { model, effort }) {
                        self.app.note(e);
                    }
                }
                Request::SaveShare(first, total) => {
                    if let Err(e) = self.state.save_share(first, total) {
                        self.app.note(e);
                    }
                }
                Request::SaveKey { secret, replace } => self.save_key(&secret, replace),
                Request::Export => self.export(),
                Request::Delete(id) => self.delete(&id),
                Request::Archive { id, archived } => self.archive(&id, archived),
                Request::Admit { template, remotes } => self.admit(&template, &remotes),
                // To whichever conversation asked, open or not; one whose
                // process has gone asks again from nothing.
                Request::Decide {
                    conversation,
                    call,
                    allow,
                } => self
                    .supervisor
                    .answer(&conversation, &Down::Decision { call, allow }),
                Request::SetDefault(model) => self.set_default(model),
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
                if !self.keys.contains(&stored) {
                    self.keys.push(stored.clone());
                }
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
        let opened = self.supervisor.open(id.clone(), create);
        self.opened(opened);
    }

    /// What opening a conversation came to: a process of its own replays
    /// its log; one already running its turn is adopted, and the log is
    /// read here.
    fn opened(&mut self, opened: Result<Opened, String>) {
        let Some(id) = self.supervisor.open_id().cloned() else {
            if let Err(reason) = opened {
                self.app.update(Update::Failed { reason }, store::now_ms());
            }
            return;
        };
        match opened {
            Ok(Opened::Started) => {}
            Ok(Opened::Adopted) => match store::read_log(&self.state, &id) {
                Ok(events) => {
                    let prefix = store::read_prefix(&self.state, &id);
                    self.app
                        .replay(prefix.as_deref().map_err(String::as_str), events);
                }
                Err(e) => self.app.note(format!("the conversation's log: {e}")),
            },
            Err(reason) => self.app.update(Update::Failed { reason }, store::now_ms()),
        }
    }

    /// A new conversation in a workspace: a scratch one, or `folder`,
    /// admitted first (DESIGN.md §8), its refusal said by name.
    fn start_in(&mut self, folder: Option<PathBuf>) {
        let places = match &self.places {
            Ok(places) => places,
            Err(why) => return self.app.note(why.clone()),
        };
        let workspace = match folder {
            None => Workspace::Scratch,
            Some(folder) => {
                match workspace::admit_directory(&folder, places, &self.client.shared) {
                    Ok(admitted) => Workspace::Directory(admitted),
                    Err(why) => return self.app.note(format!("no conversation there: {why}")),
                }
            }
        };
        self.start(Some(workspace));
    }

    /// A new conversation from the configured template `name`
    /// (DESIGN.md §7). One naming repositories makes their workspace's
    /// record, every remote admitted, which its conversation's process
    /// then prepares; refused, nothing is made.
    fn start_from(&mut self, name: &str) {
        let root = match &self.places {
            Ok(places) => places.root.clone(),
            Err(why) => return self.app.note(why.clone()),
        };
        let Some(template) = self.templates.iter().find(|t| t.name == name).cloned() else {
            return self.app.note(format!("no template is named {name:?}"));
        };
        if template.repos.is_empty() {
            return self.start(Some(Workspace::Template(name.to_string())));
        }
        let shared = self
            .client
            .shared_for(&Workspace::Template(name.to_string()))
            .len();
        // A workspace's name is short: one another workspace holds is
        // passed over for a new id. A remote nothing admits is the
        // human's to admit, on a card, asked only of a template that is
        // otherwise whole, so no admission outlasts a template refused.
        let made = self.data.clone().and_then(|data| {
            for _ in 0..NAME_TRIES {
                let id = Id::random()?;
                let repositories = workspace::plan(&template, &id, &data, &root, shared)?;
                let unadmitted = workspace::unadmitted(&repositories, &self.remotes);
                if !unadmitted.is_empty() {
                    return Ok(Err(unadmitted));
                }
                if workspace::reserve(&repositories, &data)? {
                    return Ok(Ok((id, repositories)));
                }
            }
            Err(format!("no free workspace name in {NAME_TRIES} tries"))
        });
        match made {
            Ok(Ok((id, repositories))) => {
                self.start_as(id, Some(Workspace::Repositories(repositories)))
            }
            Ok(Err(unadmitted)) => self.app.ask_admission(name.to_string(), unadmitted),
            Err(why) => self
                .app
                .note(format!("no conversation from template {name:?}: {why}")),
        }
    }

    /// The human admitted `remotes` on template `name`'s card: kept in the
    /// state directory, then its workspace made.
    fn admit(&mut self, name: &str, remotes: &[String]) {
        let admitted = self.state.admit(remotes).and_then(|urls| {
            urls.iter()
                .map(|url| crate::git::Admission::parse(url))
                .collect::<Result<Vec<_>, _>>()
        });
        match admitted {
            Ok(admitted) => {
                for admission in admitted {
                    if !self.remotes.contains(&admission) {
                        self.remotes.push(admission);
                    }
                }
                self.start_from(name);
            }
            Err(why) => self
                .app
                .note(format!("no conversation from template {name:?}: {why}")),
        }
    }

    /// A new conversation, created by its own process and opened.
    fn start(&mut self, workspace: Option<Workspace>) {
        match Id::random() {
            Ok(id) => self.start_as(id, workspace),
            Err(e) => self.app.note(format!("a new conversation: {e}")),
        }
    }

    /// `start`, with the conversation's id given.
    fn start_as(&mut self, id: Id, workspace: Option<Workspace>) {
        self.app.add_row(Row {
            id: id.clone(),
            title: Role::Conversation.first_title().to_string(),
            activity: store::now_ms(),
            state: RowState::Starting,
            paused: false,
            workspace: workspace.clone(),
            archived: false,
        });
        self.app.set_active(id.clone());
        match workspace {
            None => self.open(id, Some(Role::Conversation)),
            Some(workspace) => {
                let created = self.supervisor.create(id, Role::Conversation, workspace);
                self.opened(created);
            }
        }
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
                // A card is the window's, whichever conversation is open.
                Update::Up(Up::Ask {
                    call,
                    title,
                    details,
                }) => {
                    self.app.ask(Card {
                        conversation: id.clone(),
                        call: *call,
                        title: title.clone(),
                        details: details.clone(),
                    });
                    continue;
                }
                Update::Up(Up::Withdraw { call }) => {
                    self.app.withdraw(&id, Some(*call));
                    continue;
                }
                // A process started again asks nothing yet.
                Update::Up(Up::Hello { .. }) => self.app.withdraw(&id, None),
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
                Update::Up(Up::Fetch { remote, bases }) => {
                    if let Err(why) = self.fetch(&id, remote, bases) {
                        self.supervisor.answer(
                            &id,
                            &Down::Fetched {
                                remote: remote.clone(),
                                result: Err(why),
                            },
                        );
                    }
                }
                Update::Up(Up::Spent {
                    id: request,
                    amount,
                }) => {
                    if let Err(e) = self.ledger.settle(&id, *request, *amount, now) {
                        self.app.note(e);
                    }
                }
                Update::Restarting { .. } | Update::Failed { .. } => {
                    self.ledger.forget(&id);
                    self.app.withdraw(&id, None);
                }
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
                self.app.update(update, store::now_ms());
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
                self.app.background(&id, &update, store::now_ms());
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
        self.exported();
    }

    /// The configuration file, beside the key file.
    fn config_file(&self) -> Option<PathBuf> {
        self.key_path
            .as_deref()
            .and_then(Path::parent)
            .map(|dir| dir.join("config"))
    }

    /// Makes `model` the default (DESIGN.md §4): saved over the
    /// configuration's `model` key as the file says it now, then every
    /// conversation with no model of its own uses it from its next turn.
    fn set_default(&mut self, model: String) {
        let over = match crate::config::load(self.config_file().as_deref()) {
            Ok(config) => config.model_key,
            Err(e) => {
                eprintln!("td-agent: the configuration: {e}");
                self.model_key.clone()
            }
        };
        if let Err(e) = self.state.save_default_model(&model, over.as_deref()) {
            eprintln!("td-agent: the default model: {e}");
            return self
                .app
                .note(format!("the default model was not saved: {e}"));
        }
        self.client.model = model;
        self.supervisor.reconfigure(self.client.clone());
        self.app.set_default_model(&self.client.model);
        self.app.note(format!(
            "the default model is {}: new conversations, and those with no model of their own, use it from their next turn",
            self.client.model
        ));
    }

    /// A conversation's ask for `remote`'s store (DESIGN.md §7, §9),
    /// handed to the store thread when the configuration admits the
    /// remote; why not, otherwise, which it is answered with.
    fn fetch(&mut self, id: &Id, remote: &str, bases: &[String]) -> Result<(), String> {
        // Only what the conversation's own record names, as the window
        // made it: no process asks for another remote or base.
        if !names(self.state.workspace(id)?.as_ref(), remote, bases) {
            return Err(format!(
                "the remote {remote} and its bases are not what this conversation's workspace names"
            ));
        }
        let parsed = crate::git::Remote::parse(remote)?;
        if !self.remotes.iter().any(|admitted| admitted.admits(&parsed)) {
            return Err(format!(
                "the remote {remote} is not admitted: admit it on a template's card, or list it in `remotes` in the configuration"
            ));
        }
        let stores = self.stores.as_ref().ok_or_else(|| match &self.data {
            Err(why) => why.clone(),
            Ok(_) => "no store thread".to_string(),
        })?;
        stores.ask(id.clone(), parsed, bases.to_vec())
    }

    /// The store thread's answers, each to the conversation that asked;
    /// one whose process has gone asks again when it starts.
    fn stored(&mut self) {
        let Some(stores) = &self.stores else {
            return;
        };
        for done in stores.answers() {
            self.supervisor.answer(
                &done.conversation,
                &Down::Fetched {
                    remote: done.remote,
                    result: done.result,
                },
            );
        }
    }

    /// Deletes conversation `id` for good (DESIGN.md §4): its process
    /// ends, what was queued for it goes, then its directory; the window
    /// opens the most recently active one left in its place when it was
    /// the one open.
    fn delete(&mut self, id: &Id) {
        // The window's open conversation, whether or not its process runs.
        let was_open = self.app.active() == Some(id);
        let held = self.supervisor.remove(id);
        // Its process has ended, as a crash ends one.
        self.ledger.forget(id);
        self.app.withdraw(id, None);
        let unremoved = match self.state.delete(id) {
            Ok(unremoved) => unremoved,
            Err(e) => {
                eprintln!("td-agent: deleting {id}: {e}");
                self.app
                    .note(format!("the conversation was not deleted: {e}"));
                // Untouched: the human's messages are parked again, the
                // outbox still holds the others', and opening it again
                // starts its process.
                self.supervisor.park(id.clone(), held);
                if was_open {
                    self.open(id.clone(), None);
                }
                return;
            }
        };
        let mut said = String::from("the conversation is deleted");
        if let Some(problem) = unremoved {
            eprintln!("td-agent: deleting {id}: {problem}");
            said.push_str(&format!(
                "; what is left of its files goes at the next start: {problem}"
            ));
        }
        if let Err(e) = self.post.forget(id) {
            eprintln!("td-agent: the outbox: {e}");
            said.push_str(&format!("; the outbox: {e}"));
        }
        self.app.remove_row(id);
        self.app.note(said);
        if was_open {
            if let Some(next) = self.app.most_recent() {
                self.app.set_active(next.clone());
                self.open(next, None);
            }
        }
    }

    /// Archives conversation `id`, or brings it back (DESIGN.md §7). An
    /// archived conversation has no process: its own and its background
    /// ones end first, as a deletion's do, and the human's messages it had
    /// not taken wait for its next. Messages for it wait in the outbox.
    fn archive(&mut self, id: &Id, archived: bool) {
        // One still being made has no `meta` to mark: stopping its
        // process would leave it half made.
        if let Err(e) = self.state.archived(id) {
            return self
                .app
                .note(format!("the conversation cannot be archived yet: {e}"));
        }
        let was_open = self.app.active() == Some(id);
        let held = if archived {
            let held = self.supervisor.remove(id);
            self.ledger.forget(id);
            self.app.withdraw(id, None);
            held
        } else {
            Vec::new()
        };
        let done = if archived { "archived" } else { "unarchived" };
        let mut trouble = None;
        if let Err(e) = self
            .state
            .set_archived(id, archived, Duration::from_secs(2))
        {
            eprintln!("td-agent: {done} {id}: {e}");
            // A failure after the new `meta` was put in place, syncing
            // its directory, still marked it: what is stored decides.
            if self.state.archived(id) != Ok(archived) {
                self.app
                    .note(format!("the conversation was not {done}: {e}"));
                self.supervisor.park(id.clone(), held);
                if was_open {
                    self.open(id.clone(), None);
                }
                return;
            }
            trouble = Some(e);
        }
        self.supervisor.park(id.clone(), held);
        self.app.set_archived(id, archived);
        let title = self
            .app
            .rows()
            .iter()
            .find(|r| &r.id == id)
            .map(|r| r.title.clone())
            .unwrap_or_default();
        self.app.note(match trouble {
            None => format!("{title:?} is {done}"),
            Some(e) => format!("{title:?} is {done}, though writing it said: {e}"),
        });
        if was_open {
            if let Some(next) = self.app.most_recent() {
                self.app.set_active(next.clone());
                self.open(next, None);
            }
        }
    }

    /// Starts the diagnostics export (DESIGN.md §4) on a thread, into
    /// `~/Downloads`, else the home directory: never where a workspace
    /// reaches, since the archive holds every conversation's log.
    fn export(&mut self) {
        if self.export.is_some() {
            return self.app.note("a diagnostics export is already under way");
        }
        let Some(home) = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|home| home.is_absolute())
        else {
            return self
                .app
                .note("no diagnostics: HOME is not an absolute path to write them under");
        };
        let downloads = home.join("Downloads");
        // Not where a workspace reaches: a shared directory, or a
        // conversation's own directory.
        let directories: Vec<PathBuf> = self
            .state
            .list()
            .0
            .into_iter()
            .filter_map(|meta| match meta.workspace {
                Some(Workspace::Directory(path)) => Some(path),
                _ => None,
            })
            .chain(self.client.shared.iter().map(|shared| shared.path.clone()))
            .chain(
                self.client
                    .template_shared
                    .iter()
                    .flat_map(|template| template.shared.iter().flatten())
                    .map(|shared| shared.path.clone()),
            )
            .collect();
        let shared = std::fs::canonicalize(&downloads).is_ok_and(|real| {
            directories
                .iter()
                .any(|reached| real.starts_with(reached) || reached.starts_with(&real))
        });
        let into = if downloads.is_dir() && !shared {
            downloads
        } else {
            home
        };
        let config = self.config_file();
        // Every key this window has held, and the stored one, only to
        // look for: no file holding one is taken.
        let mut keys = self.keys.clone();
        let mut key_problem = None;
        if let Some(path) = self.key_path.as_deref() {
            match key::read(path) {
                Ok(stored) if !keys.contains(&stored) => keys.push(stored),
                Ok(_) | Err(key::Problem::Missing(_)) => {}
                Err(problem) if keys.is_empty() => {
                    return self.app.note(format!(
                        "no diagnostics: the stored key cannot be read to keep it out of them: {problem}"
                    ));
                }
                Err(problem) => key_problem = Some(problem.to_string()),
            }
        }
        let sources = diagnostics::Sources {
            state: self.state.root().to_path_buf(),
            config,
            key_file: self.key_path.clone(),
            keys,
            key_problem,
        };
        let (send, receive) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("td-agent-export".into())
            .spawn(move || {
                let exported =
                    diagnostics::export(&sources, &into, store::now(), diagnostics::COMPRESSORS);
                let _ = send.send(exported);
            });
        match spawned {
            Ok(_) => {
                self.export = Some(receive);
                self.app.note("exporting diagnostics\u{2026}");
            }
            Err(e) => self.app.note(format!("the diagnostics export: {e}")),
        }
    }

    /// What the diagnostics export made, once it is done.
    fn exported(&mut self) {
        let Some(receive) = self.export.as_ref() else {
            return;
        };
        let result = match receive.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => Err("its thread ended".into()),
        };
        self.export = None;
        let note = match result {
            Ok(exported) => {
                let mut note = format!(
                    "diagnostics written to {} ({} files, {} left out, as its MANIFEST lists); it holds your conversations and configuration, never the key file: read it before sharing",
                    exported.path.display(),
                    exported.files,
                    exported.left_out,
                );
                if let Some(remark) = exported.remark {
                    note.push_str(&format!("; {remark}"));
                }
                note
            }
            Err(e) => format!("the diagnostics export: {e}"),
        };
        eprintln!("td-agent: {note}");
        self.app.note(note);
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
        // Every model's, since a conversation may be given any.
        let contexts = models
            .models
            .iter()
            .filter_map(|m| Some((m.id.clone(), m.context_length?)))
            .collect();
        self.app.set_models(&self.client.model, contexts);
        let offers = models.models.iter().map(Offer::of).collect();
        self.app.set_offers(offers, &self.client.reasoning_effort);
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
        // The live input's choice only: `control` delivers the seam's
        // through `App::input`, which reports none.
        self.show_keys |= self.app.input_live(input, clipboard);
        self.serve();
        self.flow()
    }

    fn poll(&mut self, now: u64) -> Flow {
        self.app.tick(now);
        self.hear();
        self.stored();
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

    fn keys(&self) -> Vec<td_ui::keys::Section> {
        self.app.key_list()
    }

    fn take_show_keys(&mut self) -> bool {
        std::mem::take(&mut self.show_keys)
    }
}

/// The model a conversation with no model of its own uses (DESIGN.md
/// §4): the window's saved default while the configuration's `model` key
/// (`key`, none when left out) is what it was set over, else
/// `configured`, the configuration's, edited since and so the newer; and
/// a saved default so set aside, to be said and forgotten.
fn default_model(
    key: Option<&str>,
    configured: &str,
    saved: Option<(String, Option<String>)>,
) -> (String, Option<String>) {
    match saved {
        Some((model, over)) if over.as_deref() == key => (model, None),
        Some((model, _)) => (configured.to_string(), Some(model)),
        None => (configured.to_string(), None),
    }
}

/// Where no workspace may be (DESIGN.md §8), with the jail's programs:
/// none without them.
fn places(config: &Config, state: &StateDir) -> Result<Places, String> {
    let programs = Programs::from_env()?;
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
        .ok_or("HOME is not an absolute path, so no workspace can be admitted")?;
    Places::from_env(state.root(), &config.workspace_root(&home), &programs)
}

/// The most a setup frame's client may take of `frame::MAX_FRAME`, the
/// key and the framing having the rest.
const SETUP_CLIENT_BYTES: usize = crate::frame::MAX_FRAME / 2;

/// The conversations the store holds, as the list shows them, closed.
fn rows(state: &StateDir) -> (Vec<Row>, Vec<String>) {
    let (metas, mut problems) = state.list();
    problems.extend(state.sweep_deleted());
    let rows = metas
        .into_iter()
        .map(|meta| {
            let activity = store::activity(state, &meta);
            Row {
                id: meta.id,
                title: meta.title,
                activity,
                state: RowState::Closed,
                paused: meta.paused,
                workspace: meta.workspace,
                archived: meta.archived,
            }
        })
        .collect();
    (rows, problems)
}

/// Runs the window process over the state directory until the window
/// closes: it takes the window lock, opens the most recently active
/// conversation when there is one, and serves `control` when given.
/// `key` is the API key, or why there is none, which every turn then
/// says; `key_path` is where the key dialog stores one.
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
            "{} configuration key(s) are read by later increments, or no more; see standard error",
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
    app.set_rows(rows);
    let mut client = config.client.clone();
    // Workspaces, and the shared directories each gets, admitted once
    // here; without ./agent's jail there are none, and asking says why.
    let places = places(&config, &state);
    match &places {
        Ok(places) => {
            let (shared, mut notes) = workspace::admit_shared(&config.shared(&places.home), places);
            // A template's own list, admitted as the top-level one is.
            for template in &config.templates {
                let shared = template.shared(&places.home).map(|own| {
                    let (admitted, refused) = workspace::admit_shared(&own, places);
                    notes.extend(
                        refused
                            .into_iter()
                            .map(|note| format!("template {:?}: {note}", template.name)),
                    );
                    admitted
                });
                client.template_shared.push(TemplateShared {
                    name: template.name.clone(),
                    shared,
                });
            }
            for note in &notes {
                eprintln!("td-agent: {note}");
            }
            match notes.as_slice() {
                [] => {}
                [note] => app.note(note.clone()),
                more => app.note(format!(
                    "{} shared directories are not given to workspaces; see standard error",
                    more.len()
                )),
            }
            client.shared = shared;
            // Every conversation process is handed the lists in its setup
            // frame: past this the templates' own go, and their
            // workspaces bind none, rather than no process starting.
            if client.to_json().to_string().len() > SETUP_CLIENT_BYTES {
                client.template_shared.clear();
                let note = "the templates' shared directories are too many to hand to conversations; their workspaces bind none";
                eprintln!("td-agent: {note}");
                app.note(note);
            }
        }
        Err(why) => {
            eprintln!("td-agent: {why}");
            app.set_no_workspaces(Some(why.clone()));
        }
    }
    app.set_templates(
        config
            .templates
            .iter()
            .map(|template| (template.name.clone(), !template.repos.is_empty()))
            .collect(),
    );
    let model_key = config.model_key.clone();
    let (model, set_aside) = default_model(
        model_key.as_deref(),
        &client.model,
        state.load_default_model(),
    );
    client.model = model;
    // Once the configuration has won it stays won, even if `model` goes
    // back to what the default was set over.
    if let Some(set_aside) = set_aside {
        let said = format!(
            "the default model {set_aside}, chosen in the window, is set aside: the configuration's `model` changed since, and new conversations use {}",
            client.model
        );
        eprintln!("td-agent: {said}");
        app.note(said);
        if let Err(e) = state.forget_default_model() {
            eprintln!("td-agent: the default model: {e}");
        }
    }
    app.set_models(&client.model, Vec::new());
    app.set_offers(Vec::new(), &client.reasoning_effort);
    app.set_limits(client.limits);
    let setup = Down::Setup {
        key: key.clone(),
        client: client.clone(),
    };
    let supervisor = Supervisor::new(program, state.root().to_path_buf(), setup);
    let fetcher = match Fetcher::start(
        client.base_url.clone(),
        key.as_ref().ok().cloned(),
        state.root().to_path_buf(),
    ) {
        Ok(fetcher) => Some(fetcher),
        Err(e) => {
            app.note(e);
            None
        }
    };
    let ledger = Ledger::load(Some(state.root()), client.limits.day, store::now());
    let mut remotes = config.remotes.clone();
    match state.load_admitted().and_then(|urls| {
        urls.iter()
            .map(|url| crate::git::Admission::parse(url))
            .collect::<Result<Vec<_>, _>>()
    }) {
        Ok(admitted) => remotes.extend(admitted),
        Err(e) => {
            let said = match state.set_admitted_aside() {
                Ok(to) => format!(
                    "the remotes admitted on cards are set aside, as {}, and none of them is admitted: {e}",
                    to.display()
                ),
                Err(moved) => format!(
                    "the remotes admitted on cards are not admitted, and no card can admit until the file is mended: {e}; {moved}"
                ),
            };
            eprintln!("td-agent: {said}");
            app.note(said);
        }
    }
    let (outbox, problems) = Outbox::load(&state);
    for problem in &problems {
        eprintln!("td-agent: the outbox: {problem}");
    }
    if let Some(problem) = problems.last() {
        app.note(format!("the outbox: {problem}"));
    }
    // The stores and workspace repositories live in the data directory;
    // the git worker's own files in the state directory.
    let data = workspace::data_dir();
    let git_dir = state.root().join("git");
    let mut session = Session {
        app,
        supervisor,
        state,
        control,
        ledger,
        fetcher,
        client,
        model_key,
        post: Post::new(outbox),
        key_path,
        quit: false,
        export: None,
        keys: key.iter().cloned().collect(),
        show_keys: false,
        places,
        templates: config.templates.clone(),
        remotes,
        stores: data.as_ref().ok().map(|data| {
            crate::git::Service::start(git_dir, data.join("store"), crate::git::kept_env())
        }),
        data,
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
    // Nothing is made here: with none yet, the human starts one.
    if let Some(id) = session.app.most_recent() {
        session.app.set_active(id.clone());
        session.open(id, None);
    }
    let typeface = td_ui::pinned_face::load_or_note(
        "td-agent",
        std::env::var_os(td_ui::pinned_face::SETTING).as_deref(),
    );
    td_ui::window::run(&mut session, stream, std::env::temp_dir(), typeface)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::{default_model, names};
    use crate::workspace::Workspace;

    #[test]
    fn a_conversation_asks_only_what_its_record_names() {
        let template = crate::config::Template {
            name: "td".into(),
            repos: ["main", "next"]
                .iter()
                .map(|base| crate::config::Repo {
                    remote: "https://example.org/a/td".into(),
                    base: base.to_string(),
                    branch: format!("agent-{base}"),
                    sparse: None,
                })
                .collect(),
            shared: None,
        };
        let id = crate::store::Id::random().unwrap();
        let made = crate::workspace::plan(&template, &id, "/d".as_ref(), "/h".as_ref(), 0).unwrap();
        let workspace = Workspace::Repositories(made);
        let remote = "https://example.org/a/td";
        let bases = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        assert!(names(Some(&workspace), remote, &bases(&["main", "next"])));
        assert!(names(Some(&workspace), remote, &bases(&["next"])));
        assert!(!names(Some(&workspace), remote, &bases(&["main", "other"])));
        assert!(!names(
            Some(&workspace),
            "https://example.org/a/other",
            &bases(&["main"])
        ));
        assert!(!names(Some(&Workspace::Scratch), remote, &bases(&["main"])));
        assert!(!names(None, remote, &bases(&["main"])));
    }

    #[test]
    fn the_saved_default_holds_until_the_configuration_changes_model() {
        let saved = |over: Option<&str>| Some(("a/new".to_string(), over.map(String::from)));
        let chosen = ("a/new".to_string(), None);
        assert_eq!(
            default_model(Some("c/old"), "c/old", saved(Some("c/old"))),
            chosen
        );
        // Set over no `model` key: td-agent's built-in default changing
        // is no edit.
        assert_eq!(default_model(None, "built/in2", saved(None)), chosen);
        // Edited since: the configuration's wins, and the default is set
        // aside.
        let set_aside = Some("a/new".to_string());
        assert_eq!(
            default_model(Some("c/edited"), "c/edited", saved(Some("c/old"))),
            ("c/edited".to_string(), set_aside.clone())
        );
        assert_eq!(
            default_model(Some("c/added"), "c/added", saved(None)),
            ("c/added".to_string(), set_aside.clone())
        );
        assert_eq!(
            default_model(None, "built/in", saved(Some("c/old"))),
            ("built/in".to_string(), set_aside)
        );
        assert_eq!(
            default_model(Some("c/old"), "c/old", None),
            ("c/old".to_string(), None)
        );
    }
}

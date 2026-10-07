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
use crate::models::{Credit, Models};
use crate::picker::Offer;
use crate::post::{Outbox, Post};
use crate::protocol::{Down, Up};
use crate::store::{self, Event, Id, Kind, Role, StateDir};
use crate::supervisor::{Opened, Supervisor, Update};
use crate::ui::{Answer, App, Card, Request, Row, RowState};
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

/// The entry of a repository workspace whose worktree's id is `worktree`,
/// with the workspace's name, as the conversation's own record names it:
/// no process asks to push from a worktree that is not its own.
fn pushing_entry<'a>(
    workspace: Option<&'a Workspace>,
    worktree: &str,
) -> Option<(&'a str, &'a crate::workspace::Entry)> {
    let Some(Workspace::Repositories(repositories)) = workspace else {
        return None;
    };
    let entry = repositories
        .entries
        .iter()
        .find(|entry| entry.id == worktree)?;
    Some((repositories.name.as_str(), entry))
}

/// A conversation's push, as the window staged it (DESIGN.md §9,
/// Pushing): a push it sends must be the one staged.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Pushing {
    worktree: String,
    commit: String,
    branch: String,
    /// None while it is staged; then the remote branch's tip, if it was
    /// there.
    staged: Option<Option<String>>,
    /// False once the process that asked is gone: no push matches it,
    /// and while it is staged no other stage starts, so its answer is
    /// not taken for a later process's.
    live: bool,
}

/// Ends conversation `id`'s push with the process that staged it: one
/// staged goes, one being staged stays, dead, until its answer comes.
fn ended_pushes(pushes: &mut Vec<(Id, Pushing)>, id: &Id) {
    pushes.retain(|(of, pushing)| of != id || pushing.staged.is_none());
    for (of, pushing) in pushes.iter_mut() {
        if of == id {
            pushing.live = false;
        }
    }
}

/// The longest field of a push's frames the window takes, so no error
/// that names one runs past a frame.
const PUSH_FIELD: usize = 4096;

/// Whether conversation's push `push` may go as staged `pushing`: the
/// same worktree, commit and branch, staged already, and a lease, if
/// any, the tip it was staged against.
fn as_staged(
    pushing: Option<&Pushing>,
    push: &crate::git::Push,
    worktree: &str,
) -> Result<(), String> {
    let Some(pushing) = pushing.filter(|pushing| pushing.live) else {
        return Err("no push was staged".into());
    };
    let Some(tip) = &pushing.staged else {
        return Err("the push is still being staged".into());
    };
    if pushing.worktree != worktree || pushing.commit != push.id || pushing.branch != push.branch {
        return Err("the push is not the one staged".into());
    }
    if push.lease.is_some() && push.lease != *tip {
        return Err("a forced push expects the tip it was staged against".into());
    }
    Ok(())
}

/// The answer to `git_fetch` call `call`, of `remote`'s `bases`, from
/// the store thread's `result`: each base beside its commit or why none.
fn refetched(
    call: u64,
    remote: &str,
    bases: &[String],
    result: &Result<Vec<Result<String, String>>, String>,
) -> Down {
    Down::Refetched {
        call,
        remote: remote.to_string(),
        result: result
            .as_ref()
            .map_err(String::clone)
            .map(|ids| bases.iter().cloned().zip(ids.iter().cloned()).collect()),
    }
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
    /// The models list, and the configured models to look for beyond it.
    Models(Vec<String>),
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
                        Job::Models(wanted) => {
                            let send = |fetched| answer.send(fetched).is_ok();
                            if models(&base_url, &wanted, &state, &send) {
                                continue;
                            }
                            break;
                        }
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

/// `GET /models`, cached in the state directory as it comes and sent;
/// then, when it leaves any of `wanted` out, each looked for in its
/// endpoints listing (`models::look_up`), and what that found cached and
/// sent again, so a slow lookup holds back neither the list nor the
/// credit asked after it for longer than the lookups take. Whether the
/// window still listens.
fn models(
    base_url: &str,
    wanted: &[String],
    state: &std::path::Path,
    send: &dyn Fn(Fetched) -> bool,
) -> bool {
    let listed = crate::models::fetch_list(base_url).and_then(|models| {
        models.save(state)?;
        Ok(models)
    });
    let Ok(mut models) = listed else {
        return send(Fetched::Models(listed));
    };
    let wanted: Vec<&str> = wanted.iter().map(String::as_str).collect();
    let missing = wanted.iter().any(|id| models.find(id).is_none());
    if !send(Fetched::Models(Ok(models.clone()))) {
        return false;
    }
    if !missing || !crate::models::look_up(base_url, &mut models, &wanted) {
        return true;
    }
    let found = models.save(state).map(|()| models);
    send(Fetched::Models(found))
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
    /// Deletions of repository workspaces' conversations under way.
    removals: Vec<Removal>,
    /// The background store fetches (DESIGN.md §7, Keeping current):
    /// how often, when the next is due, the remotes whose fetch has not
    /// answered yet, and each base's commit as last fetched.
    fetch_interval: Duration,
    next_refresh: Instant,
    refreshing: Vec<String>,
    /// Each conversation's push being staged or staged last.
    pushes: Vec<(Id, Pushing)>,
    heads: crate::upstream::Heads,
    /// What each failing background fetch, or base, last said, so it is
    /// said once until it changes or mends.
    troubles: std::collections::BTreeMap<String, String>,
    /// The configuration's mode, a workspace's own when the human's rules
    /// set none (DESIGN.md §11).
    mode: crate::config::Mode,
    /// The keys of the workspaces the classifier's breaker dropped to
    /// `ask` whose mode could not be written: sent in `ask` until the
    /// human chooses their mode.
    held: Vec<String>,
}

/// A deletion of a repository workspace's conversation under way
/// (DESIGN.md §7): its processes stopped, the human's messages they had
/// not taken, and the survey of its worktrees on a thread until it
/// answers.
struct Removal {
    id: Id,
    /// It is archived, not deleted, once its workspace goes.
    archive: bool,
    title: String,
    repositories: crate::workspace::Repositories,
    held: Vec<(String, String)>,
    survey: Option<Receiver<Vec<crate::removal::Found>>>,
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
                Request::Compact(focus) => {
                    if let Err(e) = self.supervisor.compact(focus) {
                        self.app.note(e);
                    }
                }
                Request::ClearTodo => {
                    if let Err(e) = self.supervisor.tell(&Down::ClearTodo) {
                        self.app.note(e);
                    }
                }
                Request::Undo(step) | Request::Redo(step) => {
                    let undo = matches!(request, Request::Undo(_));
                    if let Err(e) = self.supervisor.tell(&Down::Restore { step, undo }) {
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
                Request::Workspace(id) => {
                    let record = self.card_record(&id);
                    self.app.show_workspace(&id, &record);
                }
                Request::Delete(id) => self.delete(&id),
                Request::Remove { id, remove } => self.finish_removal(&id, remove),
                Request::Archive { id, archived } => self.archive(&id, archived),
                // A conversation with no process has none running.
                Request::Kill { id, number } => self.supervisor.answer(&id, &Down::Kill { number }),
                // Read here: the files are the conversation's, written by
                // its watchers and only read meanwhile (DESIGN.md §12).
                Request::Output { id, number } => {
                    let read = crate::output::tail(
                        &self.state.conversation(&id).join(crate::output::DIR),
                        number,
                        crate::output::READ_BYTES,
                    );
                    self.app.show_output(&id, number, read);
                }
                Request::Admit { template, remotes } => self.admit(&template, &remotes),
                // To whichever conversation asked, open or not; one whose
                // process has gone asks again from nothing.
                Request::Decide {
                    conversation,
                    call,
                    answer,
                } => self.decide(&conversation, call, answer),
                // To whichever conversation asked, as a decision is.
                Request::Resume {
                    conversation,
                    turn,
                    choice,
                } => self
                    .supervisor
                    .answer(&conversation, &Down::Resumed { turn, choice }),
                Request::SetDefault(model) => self.set_default(model),
                Request::SetMode { conversation, mode } => self.set_mode(&conversation, mode),
                Request::Trust {
                    conversation,
                    digest,
                } => self.trust(&conversation, digest.as_deref()),
                Request::Quit => self.quit = true,
            }
        }
    }

    /// The human's answer to `conversation`'s card for `call`, sent to it
    /// whichever conversation is open; one whose process has gone asks
    /// again from nothing. An "always" answer is added to the human's
    /// rules first, and every conversation is sent them after the
    /// decision, so the card's own is decided by the human (DESIGN.md
    /// §11); one that cannot be added is said, and the answer holds once.
    fn decide(&mut self, conversation: &Id, call: u64, answer: Answer) {
        let (allow, remembered) = match answer {
            Answer::Once(allow) => (allow, None),
            Answer::Always {
                allow,
                everywhere,
                bodies,
            } => (
                allow,
                Some(remember(
                    &self.state,
                    conversation,
                    allow,
                    everywhere,
                    &bodies,
                )),
            ),
            Answer::Crossing { allow, op, to } => (
                allow,
                Some(remember_crossing(&self.state, conversation, allow, op, &to)),
            ),
        };
        let always = match remembered {
            None => None,
            Some(Ok(said)) => {
                self.app.note(format!("remembered {said}"));
                Some(said)
            }
            Some(Err(e)) => {
                let said = format!(
                    "your answer holds this once; it could not be added to your rules: {e}"
                );
                eprintln!("td-agent: {said}");
                self.app.note(said);
                None
            }
        };
        let remembered = always.is_some();
        self.supervisor.answer(
            conversation,
            &Down::Decision {
                call,
                allow,
                always,
            },
        );
        if remembered {
            self.repolicy();
        }
    }

    /// The human's rules read again and sent to every conversation with
    /// the configuration's mode, and each workspace's mode to the status
    /// row (DESIGN.md §11): a file that cannot be read is said, and every
    /// conversation asks before each call that acts.
    fn repolicy(&mut self) {
        // A workspace the breaker dropped whose mode could not be
        // written is sent in `ask` all the same.
        let rules = held(self.state.load_rules(), &self.held);
        match &rules {
            Ok(text) => self.app.set_modes(
                crate::rules::parse_policy(text)
                    .ok()
                    .map(|policy| policy.modes),
            ),
            Err(e) => {
                let said = format!("your rules could not be read, so every call that changes a workspace or runs a command asks: {e}");
                eprintln!("td-agent: {said}");
                self.app.note(said);
                self.app.set_modes(None);
            }
        }
        self.supervisor.repolicy(rules, self.mode);
    }

    /// What `conversation`'s workspace card shows, read now: its trust
    /// mark as the conversations were last sent the rules, which is
    /// what they act on, not a file changed since.
    fn card_record(&self, conversation: &Id) -> crate::card::Record {
        let trusted = match self.supervisor.sent_rules() {
            // With no workspace there is no mark.
            Some(Ok(text)) => match workspace_key(&self.state, conversation) {
                Ok(key) => crate::rules::parse_policy(text)
                    .map(|policy| policy.trust(&key).map(str::to_string)),
                Err(_) => Ok(None),
            },
            Some(Err(why)) => Err(why.clone()),
            None => Err("no rules were sent yet".into()),
        };
        crate::card::Record {
            instructions: self.state.instructions(conversation),
            prepared: self.state.prepared(conversation),
            removed: self.state.removed(conversation) == Ok(true),
            trusted,
        }
    }

    /// Marks `conversation`'s workspace as trusting the project
    /// instructions of `digest`, or none, in the human's rules (DESIGN.md
    /// §11), sends every conversation the rules, and shows its card again.
    fn trust(&mut self, conversation: &Id, digest: Option<&str>) {
        let written = workspace_key(&self.state, conversation).and_then(|key| {
            let text = crate::rules::set_trust(&self.state.load_rules()?, &key, digest)?;
            self.state.save_rules(&text)
        });
        match written {
            Ok(()) => {
                self.app.note(match digest {
                    Some(_) => "the classifier is now given this workspace's project instructions, as the card shows them",
                    None => "the classifier is no longer given this workspace's project instructions",
                });
                self.repolicy();
            }
            Err(e) => self
                .app
                .note(format!("the workspace's trust mark is unchanged: {e}")),
        }
        let record = self.card_record(conversation);
        self.app.reshow_workspace(conversation, &record);
    }

    /// Puts `conversation`'s workspace in `mode`, in the human's rules
    /// (DESIGN.md §11), and sends every conversation the rules.
    fn set_mode(&mut self, conversation: &Id, mode: crate::config::Mode) {
        match set_mode(&self.state, conversation, mode) {
            Ok(()) => {
                // The human's own choice ends any breaker's hold.
                if let Ok(key) = workspace_key(&self.state, conversation) {
                    self.held.retain(|held| held != &key);
                }
                self.app
                    .note(format!("this workspace is now in {} mode", mode.word()));
                self.repolicy();
            }
            Err(e) => self
                .app
                .note(format!("the workspace's mode is unchanged: {e}")),
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
                    always,
                }) => {
                    self.app.ask(Card {
                        conversation: id.clone(),
                        call: *call,
                        title: title.clone(),
                        details: details.clone(),
                        always: always.clone(),
                        resume: false,
                    });
                    continue;
                }
                Update::Up(Up::Resume {
                    turn,
                    title,
                    details,
                }) => {
                    self.app.ask(Card {
                        conversation: id.clone(),
                        call: *turn,
                        title: title.clone(),
                        details: details.clone(),
                        always: None,
                        resume: true,
                    });
                    continue;
                }
                Update::Up(Up::Withdraw { call }) => {
                    self.app.withdraw(&id, Some(*call));
                    continue;
                }
                // The classifier's breaker: only ever `ask` (DESIGN.md
                // §11).
                Update::Up(Up::Brake { why }) => {
                    let why = crate::tools::visible(why);
                    match set_mode(&self.state, &id, crate::config::Mode::Ask) {
                        Ok(()) => {
                            self.app.note(format!("the classifier's circuit breaker tripped ({why}), so this conversation's workspace is in ask mode; only you can put it back in auto mode"));
                            self.repolicy();
                        }
                        // Held in `ask` in what every conversation is sent
                        // until the human chooses its mode.
                        Err(e) => {
                            if let Ok(key) = workspace_key(&self.state, &id) {
                                if !self.held.contains(&key) {
                                    self.held.push(key);
                                }
                            }
                            self.app.note(format!("the classifier's circuit breaker tripped ({why}), but its workspace's mode could not be written: {e}; it is held in ask mode until you choose its mode"));
                            self.repolicy();
                        }
                    }
                }
                // A process started again asks nothing yet.
                Update::Up(Up::Hello { .. }) => {
                    self.app.withdraw(&id, None);
                    ended_pushes(&mut self.pushes, &id);
                }
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
                Update::Up(Up::Heads { remote, bases }) => {
                    let (bases, ids) = self.known(remote, bases);
                    if !bases.is_empty() {
                        self.supervisor.answer(
                            &id,
                            &Down::Heads {
                                remote: remote.clone(),
                                bases,
                                ids,
                            },
                        );
                    }
                }
                Update::Up(Up::Refetch {
                    call,
                    remote,
                    bases,
                }) => {
                    if let Err(why) = self.refetch(&id, *call, remote, bases) {
                        self.supervisor.answer(
                            &id,
                            &Down::Refetched {
                                call: *call,
                                remote: remote.clone(),
                                result: Err(why),
                            },
                        );
                    }
                }
                Update::Up(Up::Stage {
                    call,
                    worktree,
                    commit,
                    base,
                    branch,
                }) => {
                    if let Err(why) = self.stage(&id, *call, worktree, commit, base, branch) {
                        self.supervisor.answer(
                            &id,
                            &Down::Staged {
                                call: *call,
                                result: Err(why),
                            },
                        );
                    }
                }
                Update::Up(Up::Push {
                    call,
                    worktree,
                    commit,
                    branch,
                    lease,
                }) => {
                    let push = crate::git::Push {
                        id: commit.clone(),
                        branch: branch.clone(),
                        lease: lease.clone(),
                    };
                    if let Err(why) = self.push(&id, *call, worktree, push) {
                        self.supervisor.answer(
                            &id,
                            &Down::Pushed {
                                call: *call,
                                result: Err(why),
                            },
                        );
                    }
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
                    ended_pushes(&mut self.pushes, &id);
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
        self.surveyed();
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
        let parsed = self.admitted(id, remote, bases)?;
        let stores = self.stores()?;
        stores.ask(id.clone(), parsed, bases.to_vec())
    }

    /// A conversation's `git_fetch`, call `call` (DESIGN.md §9): `remote`
    /// fetched now, as `fetch` admits it; answered with `Refetched`.
    fn refetch(
        &mut self,
        id: &Id,
        call: u64,
        remote: &str,
        bases: &[String],
    ) -> Result<(), String> {
        let parsed = self.admitted(id, remote, bases)?;
        let stores = self.stores()?;
        stores.fetch_now(id.clone(), call, parsed, bases.to_vec())
    }

    /// The remote and publish repository of conversation `id`'s worktree
    /// `worktree`, as its own record names them and the configuration
    /// admits the remote.
    fn publishing(
        &self,
        id: &Id,
        worktree: &str,
    ) -> Result<(crate::git::Remote, std::path::PathBuf), String> {
        let workspace = self.state.workspace(id)?;
        let (name, entry) = pushing_entry(workspace.as_ref(), worktree).ok_or_else(|| {
            format!("the worktree {worktree:?} is not one of this conversation's workspace")
        })?;
        let remote = self.admitted(id, &entry.remote, std::slice::from_ref(&entry.base))?;
        let data = self.data.as_ref().map_err(Clone::clone)?;
        let publish = crate::workspace::publish_repository(data, name, entry)?;
        Ok((remote, publish))
    }

    /// A conversation's `git_push`, call `call` (DESIGN.md §9, Pushing):
    /// its export, in its push pack, staged against the remote's tip;
    /// answered with `Staged`.
    fn stage(
        &mut self,
        id: &Id,
        call: u64,
        worktree: &str,
        commit: &str,
        base: &str,
        branch: &str,
    ) -> Result<(), String> {
        if [worktree, commit, base, branch]
            .iter()
            .any(|field| field.len() > PUSH_FIELD)
        {
            return Err(format!("a field of the push is past {PUSH_FIELD} bytes"));
        }
        // One at a time: a stage is a fetch, an import and a scan, and
        // runs before other conversations' background fetches.
        let staging = |(of, pushing): &(Id, Pushing)| of == id && pushing.staged.is_none();
        if self.pushes.iter().any(staging) {
            return Err("this conversation's last push is still being staged".into());
        }
        let (remote, publish) = self.publishing(id, worktree)?;
        let staging = crate::git::Staging {
            publish,
            pack: self.state.push_pack(id),
            id: commit.to_string(),
            base: base.to_string(),
            branch: branch.to_string(),
        };
        self.stores()?.stage(id.clone(), call, remote, staging)?;
        self.pushes.retain(|(of, _)| of != id);
        self.pushes.push((
            id.clone(),
            Pushing {
                worktree: worktree.to_string(),
                commit: commit.to_string(),
                branch: branch.to_string(),
                staged: None,
                live: true,
            },
        ));
        Ok(())
    }

    /// Call `call`'s push, decided, sent from the publish repository;
    /// answered with `Pushed`.
    fn push(
        &mut self,
        id: &Id,
        call: u64,
        worktree: &str,
        push: crate::git::Push,
    ) -> Result<(), String> {
        if [worktree, &push.id, &push.branch]
            .into_iter()
            .chain(push.lease.as_deref())
            .any(|field| field.len() > PUSH_FIELD)
        {
            return Err(format!("a field of the push is past {PUSH_FIELD} bytes"));
        }
        let pushing = self
            .pushes
            .iter()
            .find(|(of, _)| of == id)
            .map(|(_, pushing)| pushing);
        as_staged(pushing, &push, worktree)?;
        let (remote, publish) = self.publishing(id, worktree)?;
        self.stores()?
            .push(id.clone(), call, remote, publish, push)?;
        // Each staged push is sent once.
        self.pushes.retain(|(of, _)| of != id);
        Ok(())
    }

    /// The store thread, or why there is none.
    fn stores(&self) -> Result<&crate::git::Service, String> {
        self.stores.as_ref().ok_or_else(|| match &self.data {
            Err(why) => why.clone(),
            Ok(_) => "no store thread".to_string(),
        })
    }

    /// `remote` parsed, when conversation `id`'s own record names it and
    /// `bases` and the configuration admits it; why not, otherwise.
    fn admitted(
        &self,
        id: &Id,
        remote: &str,
        bases: &[String],
    ) -> Result<crate::git::Remote, String> {
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
        Ok(parsed)
    }

    /// The store thread's answers: a preparation's to the conversation
    /// that asked, one whose process has gone asking again when it
    /// starts; a background fetch's kept, and each base in use that
    /// advanced said. Either teaches the window where each base is.
    fn stored(&mut self) {
        let Some(stores) = &self.stores else {
            return;
        };
        let mut moved = Vec::new();
        let mut learnt: Vec<String> = Vec::new();
        for done in stores.answers() {
            match done {
                crate::git::Done::Prepared {
                    conversation,
                    remote,
                    bases,
                    result,
                } => {
                    if let Ok(fetched) = &result {
                        if !learnt.contains(&remote) {
                            learnt.push(remote.clone());
                        }
                        let ids = fetched.ids.iter().map(|id| Some(id.as_str()));
                        moved.extend(
                            self.heads
                                .learn(&remote, &bases, ids)
                                .into_iter()
                                .map(|(base, id)| (remote.clone(), base, id)),
                        );
                    }
                    self.supervisor
                        .answer(&conversation, &Down::Fetched { remote, result });
                }
                crate::git::Done::Staged {
                    asker: (conversation, call),
                    result,
                } => {
                    match &result {
                        Ok(staged) => {
                            self.pushes
                                .retain(|(of, pushing)| *of != conversation || pushing.live);
                            for (of, pushing) in &mut self.pushes {
                                if *of == conversation && pushing.staged.is_none() {
                                    pushing.staged = Some(staged.tip.clone());
                                }
                            }
                        }
                        Err(_) => self.pushes.retain(|(of, _)| *of != conversation),
                    }
                    self.supervisor
                        .answer(&conversation, &Down::Staged { call, result });
                }
                crate::git::Done::Pushed {
                    asker: (conversation, call),
                    result,
                } => {
                    self.supervisor
                        .answer(&conversation, &Down::Pushed { call, result });
                }
                crate::git::Done::Refreshed {
                    remote,
                    bases,
                    result,
                    asker,
                } => {
                    // A `git_fetch`'s is its caller's to hear, and a
                    // background fetch still running stays marked.
                    if let Some((conversation, call)) = &asker {
                        self.supervisor
                            .answer(conversation, &refetched(*call, &remote, &bases, &result));
                    } else {
                        self.refreshing.retain(|asked| *asked != remote);
                        // Said where diagnostics go, not in the window,
                        // and once until it changes or mends.
                        let fetched = result.as_ref().err().cloned();
                        self.trouble(
                            remote.clone(),
                            fetched.map(|e| format!("fetching {remote} in the background: {e}")),
                        );
                    }
                    let Ok(ids) = result else {
                        continue;
                    };
                    if !learnt.contains(&remote) {
                        learnt.push(remote.clone());
                    }
                    if asker.is_none() {
                        for (base, id) in bases.iter().zip(&ids) {
                            self.trouble(
                                format!("{remote} {base}"),
                                id.as_ref()
                                    .err()
                                    .map(|e| format!("{remote}'s {base:?} after fetching it: {e}")),
                            );
                        }
                    }
                    let ids = ids.iter().map(|id| id.as_deref().ok());
                    moved.extend(
                        self.heads
                            .learn(&remote, &bases, ids)
                            .into_iter()
                            .map(|(base, id)| (remote.clone(), base, id)),
                    );
                }
            }
        }
        if !moved.is_empty() {
            let said: Vec<String> = moved
                .iter()
                .map(|(remote, base, id)| {
                    format!(
                        "{} of {} is at {}",
                        crate::tools::visible(base),
                        crate::tools::visible(remote),
                        id.chars().take(12).collect::<String>()
                    )
                })
                .collect();
            let note = format!("upstream moved: {}", said.join("; "));
            eprintln!("td-agent: {note}");
            self.app.note(note);
        }
        for remote in learnt {
            self.tell_heads(&remote);
        }
    }

    /// Of `bases` of `remote`, those the window knows where to find, and
    /// the commits it last found them at.
    fn known(&self, remote: &str, bases: &[String]) -> (Vec<String>, Vec<String>) {
        bases
            .iter()
            .filter_map(|base| {
                let id = self.heads.at(remote, base)?;
                Some((base.clone(), id.to_string()))
            })
            .unzip()
    }

    /// Tells each running conversation whose workspace names `remote`
    /// where the window last found its bases there (DESIGN.md §7,
    /// Keeping current), so its remote-tracking refs follow; one not
    /// running asks when its process starts, and one whose workspace
    /// went with its archive lets it go.
    fn tell_heads(&mut self, remote: &str) {
        for id in self.supervisor.ids() {
            let Ok(Some(crate::workspace::Workspace::Repositories(repositories))) =
                self.state.workspace(&id)
            else {
                continue;
            };
            let bases: Vec<String> = repositories
                .entries
                .iter()
                .filter(|entry| entry.remote == remote)
                .map(|entry| entry.base.clone())
                .collect();
            let (bases, ids) = self.known(remote, &bases);
            if !bases.is_empty() {
                self.supervisor.answer(
                    &id,
                    &Down::Heads {
                        remote: remote.to_string(),
                        bases,
                        ids,
                    },
                );
            }
        }
    }

    /// Says `said` of `what` where diagnostics go when it is new, and
    /// forgets `what`'s trouble when there is none.
    fn trouble(&mut self, what: String, said: Option<String>) {
        match said {
            None => {
                self.troubles.remove(&what);
            }
            Some(said) => {
                if self.troubles.get(&what) != Some(&said) {
                    eprintln!("td-agent: {said}");
                    self.troubles.insert(what, said);
                }
            }
        }
    }

    /// Asks the store thread to fetch, in the background, each admitted
    /// remote a live repository workspace uses, every `fetch_interval`
    /// (DESIGN.md §7, Keeping current); one still fetching is not asked
    /// again.
    fn refresh(&mut self) {
        let now = Instant::now();
        if now < self.next_refresh {
            return;
        }
        self.next_refresh = now.checked_add(self.fetch_interval).unwrap_or(now);
        let Some(stores) = &self.stores else {
            return;
        };
        let (metas, _) = self.state.list();
        for (remote, bases) in crate::upstream::in_use(&metas, &self.remotes, &self.refreshing) {
            let url = remote.url();
            match stores.refresh(remote, bases) {
                Ok(()) => self.refreshing.push(url),
                Err(e) => eprintln!("td-agent: fetching {url} in the background: {e}"),
            }
        }
    }

    /// Deletes conversation `id` for good (DESIGN.md §4): its process
    /// ends, what was queued for it goes, then its directory; the window
    /// opens the most recently active one left in its place when it was
    /// the one open.
    fn delete(&mut self, id: &Id) {
        match self.state.workspace(id) {
            // One whose workspace went with its archive has none to ask.
            Ok(Some(crate::workspace::Workspace::Repositories(repositories)))
                if self.state.removed(id) != Ok(true) =>
            {
                return self.begin_removal(id, repositories, false);
            }
            // Its record unread, a workspace it may have is not known.
            Err(e) => self.app.note(format!(
                "the conversation's record could not be read, so a repository workspace it may have is left: {e}"
            )),
            Ok(_) => {}
        }
        // The window's open conversation, whether or not its process runs.
        let was_open = self.app.active() == Some(id);
        let held = self.supervisor.remove(id);
        // Its process has ended, as a crash ends one.
        self.ledger.forget(id);
        self.app.withdraw(id, None);
        self.delete_now(id, was_open, held);
    }

    /// Deletes conversation `id`, its processes stopped and `held` the
    /// human's messages they had not taken, opening the next when
    /// `was_open`; whether it is deleted.
    fn delete_now(&mut self, id: &Id, was_open: bool, held: Vec<(String, String)>) -> bool {
        // The workspace's own rules go with it, a directory's staying,
        // it being the human's and others able to work in it; and its
        // crossings.
        let ruled = match self.state.workspace(id) {
            Ok(Some(crate::workspace::Workspace::Directory(_))) | Ok(None) | Err(_) => None,
            Ok(Some(workspace)) => Some(workspace.key(id)),
        };
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
                return false;
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
        match forget_rules(&self.state, ruled.as_deref(), id) {
            Ok(false) => {}
            Ok(true) => self.repolicy(),
            Err(e) => {
                eprintln!("td-agent: forgetting the rules for {id}: {e}");
                said.push_str(&format!("; its rules stay in your rules file: {e}"));
            }
        }
        self.app.remove_row(id);
        self.app.note(said);
        if was_open {
            if let Some(next) = self.app.most_recent() {
                self.app.set_active(next.clone());
                self.open(next, None);
            }
        }
        true
    }

    /// Starts deleting conversation `id`, or archiving it when `archive`,
    /// its repository workspace going with it (DESIGN.md §7): its
    /// processes are stopped and it neither opens nor takes a message
    /// while each prepared worktree is asked, on a thread, what removing
    /// it would lose.
    fn begin_removal(
        &mut self,
        id: &Id,
        repositories: crate::workspace::Repositories,
        archive: bool,
    ) {
        if self.removals.iter().any(|removal| &removal.id == id) {
            return self
                .app
                .note("that conversation is already being archived or deleted");
        }
        let doing = if archive { "archiving" } else { "deleting" };
        let was_open = self.app.active() == Some(id);
        let held = self.supervisor.remove(id);
        self.ledger.forget(id);
        self.app.withdraw(id, None);
        self.app.set_closing(id, Some(doing));
        let title = self
            .app
            .rows()
            .iter()
            .find(|r| &r.id == id)
            .map(|r| r.title.clone())
            .unwrap_or_default();
        let (tell, survey) = mpsc::channel();
        let (state, of, asked) = (self.state.clone(), id.clone(), repositories.clone());
        let spawned = std::thread::Builder::new()
            .name("td-agent-survey".into())
            .spawn(move || {
                let programs = Programs::from_env();
                let _ = tell.send(crate::removal::survey(&state, &of, &asked, programs));
            });
        self.app.note(format!(
            "{title:?}: asking its workspace what removing it would lose, before {doing} it"
        ));
        let survey = match spawned {
            Ok(_) => Some(survey),
            Err(e) => {
                self.app.ask_removal(
                    id.clone(),
                    title.clone(),
                    vec![format!("its worktrees could not be asked: {e}")],
                    archive,
                );
                None
            }
        };
        self.removals.push(Removal {
            id: id.clone(),
            archive,
            title,
            repositories,
            held,
            survey,
        });
        if was_open {
            if let Some(next) = self.app.most_recent() {
                self.app.set_active(next.clone());
                self.open(next, None);
            }
        }
    }

    /// What the surveys under way found: a workspace that reports nothing
    /// to lose goes with its conversation at once; one that reports
    /// anything is asked about.
    fn surveyed(&mut self) {
        let mut answered = Vec::new();
        for removal in &mut self.removals {
            let found = match removal.survey.as_ref().map(Receiver::try_recv) {
                Some(Ok(found)) => Ok(found),
                Some(Err(mpsc::TryRecvError::Disconnected)) => Err(()),
                Some(Err(mpsc::TryRecvError::Empty)) | None => continue,
            };
            removal.survey = None;
            let lost = match found {
                Ok(found) => crate::removal::lost(&found),
                Err(()) => vec!["the survey of its worktrees ended without an answer".into()],
            };
            answered.push((
                removal.id.clone(),
                removal.title.clone(),
                lost,
                removal.archive,
            ));
        }
        for (id, title, lost, archive) in answered {
            if lost.is_empty() {
                self.finish_removal(&id, true);
            } else {
                self.app.ask_removal(id, title, lost, archive);
            }
        }
    }

    /// Ends conversation `id`'s removal: deleted, or archived, with its
    /// workspace removed when `remove`, else both kept as they were, its
    /// messages held again.
    fn finish_removal(&mut self, id: &Id, remove: bool) {
        let Some(at) = self.removals.iter().position(|removal| &removal.id == id) else {
            return;
        };
        let removal = self.removals.remove(at);
        self.app.set_closing(id, None);
        let done = if removal.archive {
            "archived"
        } else {
            "deleted"
        };
        if !remove {
            self.supervisor.park(id.clone(), removal.held);
            return self.app.note(format!(
                "{:?} is not {done}: it and its workspace stay as they were",
                removal.title
            ));
        }
        // Out of the way first, so a crash past here leaves nothing the
        // next start's sweep does not take; put back if the conversation
        // stays.
        let doomed = crate::removal::doom(&removal.repositories);
        let name = &removal.repositories.name;
        let ended = if removal.archive {
            self.archive_now(id, true, true, false, removal.held)
        } else {
            self.delete_now(id, false, removal.held)
        };
        let said = if ended {
            match doomed.finish() {
                Ok(()) => format!("its workspace {name} is removed"),
                Err(e) => format!("its workspace {name} was not all removed: {e}"),
            }
        } else {
            match doomed.restore() {
                Ok(()) => format!("its workspace {name} stays with it"),
                Err(e) => format!("its workspace {name} could not all be put back: {e}"),
            }
        };
        eprintln!("td-agent: {said}");
        self.app.note(said);
    }

    /// Archives conversation `id`, or brings it back (DESIGN.md §7). An
    /// archived conversation has no process: its own and its background
    /// ones end first, as a deletion's do, and the human's messages it had
    /// not taken wait for its next. Messages for it wait in the outbox.
    fn archive(&mut self, id: &Id, archived: bool) {
        if self.removals.iter().any(|removal| &removal.id == id) {
            return self
                .app
                .note("that conversation is being archived or deleted already");
        }
        // One still being made has no `meta` to mark: stopping its
        // process would leave it half made.
        if let Err(e) = self.state.archived(id) {
            return self
                .app
                .note(format!("the conversation cannot be archived yet: {e}"));
        }
        // Its repository workspace goes with it, asked first (DESIGN.md
        // §7), unless it went with an earlier archive.
        if archived {
            match self.state.removed(id).and_then(|removed| {
                self.state
                    .workspace(id)
                    .map(|workspace| workspace.filter(|_| !removed))
            }) {
                Ok(Some(crate::workspace::Workspace::Repositories(repositories))) => {
                    return self.begin_removal(id, repositories, true);
                }
                Err(e) => self.app.note(format!(
                    "the conversation's record could not be read, so a repository workspace it may have is left: {e}"
                )),
                Ok(_) => {}
            }
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
        self.archive_now(id, archived, false, was_open, held);
    }

    /// Archives conversation `id`, or brings it back, its processes
    /// stopped and `held` the human's messages they had not taken, its
    /// workspace marked gone with it when `removed`, opening the next
    /// when `was_open`; whether it is.
    fn archive_now(
        &mut self,
        id: &Id,
        archived: bool,
        removed: bool,
        was_open: bool,
        held: Vec<(String, String)>,
    ) -> bool {
        let done = if archived { "archived" } else { "unarchived" };
        let mut trouble = None;
        if let Err(e) = self
            .state
            .set_archived(id, archived, removed, Duration::from_secs(2))
        {
            eprintln!("td-agent: {done} {id}: {e}");
            // A failure after the new `meta` was put in place, syncing
            // its directory, still marked it: what is stored decides.
            if self.state.archived(id) != Ok(archived)
                || (removed && self.state.removed(id) != Ok(true))
            {
                self.app
                    .note(format!("the conversation was not {done}: {e}"));
                self.supervisor.park(id.clone(), held);
                if was_open {
                    self.open(id.clone(), None);
                }
                return false;
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
        true
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
        self.refresh();
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
    app.set_without_jev(!config.client.jev_required);
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
    // here; without the jail there are none, and asking says why.
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
        client: Box::new(client.clone()),
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
        removals: Vec::new(),
        fetch_interval: config.fetch_interval(),
        next_refresh: Instant::now(),
        refreshing: Vec::new(),
        pushes: Vec::new(),
        heads: crate::upstream::Heads::default(),
        troubles: std::collections::BTreeMap::new(),
        mode: config.mode,
        held: Vec::new(),
    };
    // Before any conversation starts.
    session.repolicy();
    // What removals a crash cut short left (`removal::sweep`).
    if let Ok(data) = &session.data {
        let mut doomed = vec![data.join("ws")];
        if let Some(home) = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|home| home.is_absolute())
        {
            doomed.push(config.workspace_root(&home));
        }
        crate::removal::sweep(&doomed);
    }
    // A cached list serves until the provider's comes.
    match Models::load(session.state.root()) {
        Ok(Some(models)) => session.show_models(&models),
        Ok(None) => {}
        Err(e) => session.app.note(format!("the models cache: {e}")),
    }
    if let Some(fetcher) = session.fetcher.as_mut() {
        let _ = fetcher.jobs.send(Job::Models(session.client.wanted()));
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

/// Adds `bodies`, as allows or denies, to the human's rules for
/// `conversation`'s workspace, or for every workspace: what was added,
/// as the approval's reason says it.
fn remember(
    state: &StateDir,
    conversation: &Id,
    allow: bool,
    everywhere: bool,
    bodies: &[String],
) -> Result<String, String> {
    let scope = if everywhere {
        crate::rules::Scope::Everywhere
    } else {
        let (metas, _) = state.list();
        let workspace = metas
            .into_iter()
            .find(|meta| &meta.id == conversation)
            .and_then(|meta| meta.workspace)
            .ok_or("its conversation has no workspace")?;
        crate::rules::Scope::Workspace(workspace.key(conversation))
    };
    let effect = if allow {
        crate::rules::Effect::Allow
    } else {
        crate::rules::Effect::Deny
    };
    let text = crate::rules::add(&state.load_rules()?, &scope, effect, bodies)?;
    state.save_rules(&text)?;
    let rules: Vec<String> = bodies
        .iter()
        .map(|body| format!("`{} {body}`", effect.name()))
        .collect();
    let whose = if everywhere {
        "every workspace"
    } else {
        "this workspace"
    };
    Ok(format!("{} in your rules for {whose}", rules.join(", ")))
}

/// Puts `conversation`'s workspace in `mode` in the human's rules.
fn set_mode(state: &StateDir, conversation: &Id, mode: crate::config::Mode) -> Result<(), String> {
    let key = workspace_key(state, conversation)?;
    let text = crate::rules::set_mode(&state.load_rules()?, &key, mode)?;
    state.save_rules(&text)
}

/// The human's rules as read, with each workspace of `keys` put in
/// `ask` mode; refused when they are, or such a line cannot be added.
fn held(rules: Result<String, String>, keys: &[String]) -> Result<String, String> {
    rules.and_then(|text| {
        keys.iter().try_fold(text, |text, key| {
            crate::rules::set_mode(&text, key, crate::config::Mode::Ask)
        })
    })
}

/// `conversation`'s workspace's key, as its record names the workspace.
fn workspace_key(state: &StateDir, conversation: &Id) -> Result<String, String> {
    let (metas, _) = state.list();
    let workspace = metas
        .into_iter()
        .find(|meta| &meta.id == conversation)
        .and_then(|meta| meta.workspace)
        .ok_or("its conversation has no workspace")?;
    Ok(workspace.key(conversation))
}

/// Takes what deleted conversation `id` leaves out of the human's rules,
/// its workspace `key`'s sections and its crossings: whether there were
/// any.
fn forget_rules(state: &StateDir, key: Option<&str>, id: &Id) -> Result<bool, String> {
    match crate::rules::forget(&state.load_rules()?, key, id.as_str())? {
        Some(text) => state.save_rules(&text).map(|()| true),
        None => Ok(false),
    }
}

/// Adds the human's standing answer for `conversation` doing `op` to
/// conversation `to`, that way only: what was added, as the approval's
/// reason says it.
fn remember_crossing(
    state: &StateDir,
    conversation: &Id,
    allow: bool,
    op: crate::rules::Crossed,
    to: &str,
) -> Result<String, String> {
    let crossing = crate::rules::Crossing {
        allow,
        op,
        from: conversation.as_str().to_string(),
        to: to.to_string(),
    };
    // As the file reads it: `to` a conversation and not this one, and
    // one that is still here.
    let crossing = crate::rules::Crossing::parse(&crossing.text())?;
    let (metas, _) = state.list();
    if !metas.iter().any(|meta| meta.id.as_str() == crossing.to) {
        return Err(format!("there is no conversation {}", crossing.to));
    }
    let text = crate::rules::add_crossing(&state.load_rules()?, &crossing)?;
    state.save_rules(&text)?;
    Ok(format!("`{}` in your rules for crossings", crossing.text()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::{
        as_staged, default_model, ended_pushes, forget_rules, held, names, pushing_entry,
        refetched, remember, remember_crossing, set_mode,
    };
    use crate::protocol::Down;
    use crate::workspace::Workspace;

    /// A card's "always" answer adds its rules under the conversation's
    /// workspace's header, or every workspace's, and says what it added;
    /// never an allow for every workspace, nor into a file refused.
    #[test]
    fn an_always_answer_is_added_to_the_humans_rules() {
        let scratch = crate::store::tests::Scratch::new("remember");
        let state = scratch.state();
        let id = crate::store::Id::random().unwrap();
        let workspace = Workspace::Directory("/home/u/my notes".into());
        crate::store::Conversation::create(
            &state,
            &id,
            crate::store::Role::Conversation,
            Some(workspace),
            std::time::Duration::from_secs(5),
        )
        .unwrap();
        let bodies = vec!["shell cargo test".to_string(), "shell rm".to_string()];
        assert_eq!(
            remember(&state, &id, true, false, &bodies).unwrap(),
            "`allow shell cargo test`, `allow shell rm` in your rules for this workspace"
        );
        assert_eq!(
            remember(&state, &id, false, true, bodies.get(1..).unwrap()).unwrap(),
            "`deny shell rm` in your rules for every workspace"
        );
        let text = "[directory /home/u/my%20notes]\nallow shell cargo test\nallow shell rm\n\n[everywhere]\ndeny shell rm\n";
        assert_eq!(state.load_rules().unwrap(), text);
        assert!(remember(&state, &id, true, true, &bodies).is_err());
        let stranger = crate::store::Id::random().unwrap();
        assert!(remember(&state, &stranger, false, false, &bodies).is_err());
        assert_eq!(state.load_rules().unwrap(), text);
        let other = crate::store::Id::random().unwrap();
        assert!(remember_crossing(
            &state,
            &id,
            true,
            crate::rules::Crossed::Read,
            other.as_str()
        )
        .is_err());
        crate::store::Conversation::create(
            &state,
            &other,
            crate::store::Role::Conversation,
            None,
            std::time::Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(
            remember_crossing(
                &state,
                &id,
                true,
                crate::rules::Crossed::Read,
                other.as_str()
            )
            .unwrap(),
            format!(
                "`allow read {} {}` in your rules for crossings",
                id.as_str(),
                other.as_str()
            )
        );
        assert!(
            remember_crossing(&state, &id, true, crate::rules::Crossed::Read, id.as_str()).is_err()
        );
        assert!(remember_crossing(&state, &id, true, crate::rules::Crossed::Read, "x").is_err());
        // A workspace the breaker holds is sent in `ask`, the file as it
        // was; a file not read stays refused.
        let file = "[workspace td-1-ab]\nmode auto\n".to_string();
        let sent = held(Ok(file.clone()), &["workspace td-1-ab".to_string()]).unwrap();
        assert_eq!(
            crate::rules::parse_policy(&sent)
                .unwrap()
                .mode("workspace td-1-ab"),
            Some(crate::config::Mode::Ask)
        );
        assert_eq!(held(Ok(file.clone()), &[]).unwrap(), file);
        assert!(held(Err("unread".into()), &["workspace td-1-ab".to_string()]).is_err());
        // A workspace's mode, set and set again, one line.
        set_mode(&state, &id, crate::config::Mode::Auto).unwrap();
        set_mode(&state, &id, crate::config::Mode::Ask).unwrap();
        assert!(set_mode(&state, &stranger, crate::config::Mode::Auto).is_err());
        let policy = crate::rules::parse_policy(&state.load_rules().unwrap()).unwrap();
        assert_eq!(
            policy.modes,
            [(
                "directory /home/u/my%20notes".to_string(),
                crate::config::Mode::Ask
            )]
        );
        assert!(!forget_rules(&state, Some("workspace td-1-ab"), &stranger).unwrap());
        assert!(forget_rules(&state, Some("directory /home/u/my%20notes"), &id).unwrap());
        assert_eq!(
            state.load_rules().unwrap(),
            "[everywhere]\ndeny shell rm\n\n[crossings]\n"
        );
        let path = state.root().join(crate::rules::HUMAN_FILE);
        std::fs::write(&path, "[x]\n").unwrap();
        assert!(remember(&state, &id, false, true, &bodies).is_err());
        assert!(forget_rules(&state, Some("workspace td-1-ab"), &id).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[x]\n");
    }

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
        let workspace = Workspace::Repositories(made.clone());
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
        // A `git_fetch`'s answer pairs each base with what it found.
        let found = Ok(vec![Ok("a".repeat(40)), Err("gone".to_string())]);
        assert_eq!(
            refetched(3, remote, &bases(&["main", "next"]), &found),
            Down::Refetched {
                call: 3,
                remote: remote.into(),
                result: Ok(vec![
                    ("main".into(), Ok("a".repeat(40))),
                    ("next".into(), Err("gone".into()))
                ]),
            }
        );
        assert_eq!(
            refetched(4, remote, &bases(&["main"]), &Err("unreachable".into())),
            Down::Refetched {
                call: 4,
                remote: remote.into(),
                result: Err("unreachable".into()),
            }
        );
        // A push is from one of its own worktrees, whose publish
        // repository sits beside its workspace repository.
        let first = made.entries.first().unwrap();
        let (name, entry) = pushing_entry(Some(&workspace), &first.id).unwrap();
        assert_eq!((name, entry), (made.name.as_str(), first));
        assert!(pushing_entry(Some(&workspace), "elsewhere").is_none());
        assert!(pushing_entry(Some(&Workspace::Scratch), &first.id).is_none());
        assert!(pushing_entry(None, &first.id).is_none());
        let publish = crate::workspace::publish_repository("/d".as_ref(), name, entry).unwrap();
        assert_eq!(
            publish,
            std::path::Path::new("/d/publish")
                .join(name)
                .join(entry.repository.file_name().unwrap())
        );
        assert!(entry
            .repository
            .starts_with(std::path::Path::new("/d/ws").join(name)));
    }

    /// A push goes only as staged: the same worktree, commit and branch,
    /// staged already, and forced only against the tip it was staged at.
    #[test]
    fn a_push_goes_only_as_it_was_staged() {
        let tip = "c".repeat(40);
        let pushing = |staged: Option<Option<String>>| super::Pushing {
            worktree: "td".into(),
            commit: "a".repeat(40),
            branch: "agent".into(),
            staged,
            live: true,
        };
        let push = |commit: &str, branch: &str, lease: Option<&str>| crate::git::Push {
            id: commit.to_string(),
            branch: branch.to_string(),
            lease: lease.map(str::to_string),
        };
        let a = "a".repeat(40);
        let staged = pushing(Some(Some(tip.clone())));
        assert!(as_staged(Some(&staged), &push(&a, "agent", None), "td").is_ok());
        assert!(as_staged(Some(&staged), &push(&a, "agent", Some(&tip)), "td").is_ok());
        for (pushing, push, worktree) in [
            (None, push(&a, "agent", None), "td"),
            (Some(pushing(None)), push(&a, "agent", None), "td"),
            (Some(staged.clone()), push(&a, "agent", None), "other"),
            (
                Some(staged.clone()),
                push(&"b".repeat(40), "agent", None),
                "td",
            ),
            (Some(staged.clone()), push(&a, "main", None), "td"),
            (Some(staged.clone()), push(&a, "agent", Some(&a)), "td"),
            (
                Some(pushing(Some(None))),
                push(&a, "agent", Some(&tip)),
                "td",
            ),
            (
                Some(super::Pushing {
                    live: false,
                    ..staged.clone()
                }),
                push(&a, "agent", None),
                "td",
            ),
        ] {
            assert!(
                as_staged(pushing.as_ref(), &push, worktree).is_err(),
                "{push:?}"
            );
        }
        // A process gone ends its push: one staged goes, one being
        // staged stays, dead; another conversation's stays as it was.
        let (one, other) = (
            crate::store::Id::random().unwrap(),
            crate::store::Id::random().unwrap(),
        );
        let mut pushes = vec![
            (one.clone(), staged.clone()),
            (other.clone(), staged.clone()),
        ];
        ended_pushes(&mut pushes, &one);
        assert_eq!(pushes, vec![(other.clone(), staged.clone())]);
        let mut pushes = vec![(one.clone(), pushing(None)), (other.clone(), pushing(None))];
        ended_pushes(&mut pushes, &one);
        assert_eq!(
            pushes,
            vec![
                (
                    one,
                    super::Pushing {
                        live: false,
                        ..pushing(None)
                    }
                ),
                (other, pushing(None)),
            ]
        );
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

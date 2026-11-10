//! `td-agent serve` (DESIGN.md §2, §4): the broker. It holds the conversation
//! processes and what crosses them, whether or not a window is there. Each turn
//! it tells the window, by `view::View` messages, what those processes said,
//! answers their reservations from the day's ledger, and carries out what the
//! window asked. It listens only on two sockets in the private state directory,
//! serves one window at a time, and ends when none is attached and nothing is
//! under way or scheduled.
//!
//! It also holds what crosses conversations: the API key, read once at
//! startup and handed to each conversation process over its socketpair,
//! and stored from the key dialog, which hands every running process the
//! new one over its socketpair;
//! the day's spending (`accounts::Ledger`); the messages between
//! conversations, which it routes (`post`); and the provider's models
//! list and the key's credit, fetched on a thread of their own
//! (`Fetcher`) so the window never waits on the network.

use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use crate::accounts::Ledger;
use crate::config::{Client, Config, Template, TemplateShared};
use crate::diagnostics::{self, Exported};
use crate::jail::Programs;
use crate::key::{self, Secret, Unwritten};
use crate::models::{Credit, Models};
use crate::peer::Peer;
use crate::picker::Offer;
use crate::post::{Outbox, Post};
use crate::protocol::{Down, Up};
use crate::store::{self, Event, Id, Kind, Role, StateDir};
use crate::supervisor::{Opened, Supervisor, Update};
use crate::ui::{Answer, Card, Request, Row, RowState};
use crate::view::Port;
use crate::window_wire::Showing;
use crate::workspace::{self, Places, Workspace};

/// The longest a turn waits, with a window to answer and with none.
const BUSY_POLL: Duration = Duration::from_millis(20);
const POLL: Duration = Duration::from_millis(50);
/// How long nothing has to be going on before the broker ends: a window starting
/// one has yet to dial it, and one closing may be opened again.
const LINGER: Duration = Duration::from_secs(5);
/// How often the broker looks at how long `serve.log` has grown.
const LOOK_AT_LOG: Duration = Duration::from_secs(60);
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
    /// What the window is told, and what the session knows of it.
    view: Port,
    supervisor: Supervisor,
    state: StateDir,
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
    /// The diagnostics export under way, on a thread of its own.
    export: Option<Receiver<Result<Exported, String>>>,
    /// The sub-agents each conversation's log records, read at start-up
    /// on a thread of their own for the list (DESIGN.md §4).
    tasks: Option<Receiver<TasksRead>>,
    /// Every key this window has held, which the export looks for.
    keys: Vec<Secret>,
    /// Where no workspace may be, or why there are no workspaces.
    places: Result<Places, String>,
    /// Why there is no jail, when `unconfined_without_jail` makes Empty
    /// and Directory… workspaces unconfined instead (DESIGN.md §8).
    jailless: Option<String>,
    /// The templates the chooser lists (DESIGN.md §7): the configured
    /// ones, then those made in the window.
    templates: Vec<Template>,
    /// The configured templates.
    configured: Vec<Template>,
    /// The templates file as read, which a save rewrites from: every
    /// template in it, listed or not; or why none can be saved until the
    /// file is mended.
    saved: Result<Vec<Template>, String>,
    /// The remotes admitted (DESIGN.md §7): the configuration's, then
    /// those the human admitted on a card.
    remotes: Vec<crate::git::Admission>,
    /// td-agent's data directory, or why there is none, and the store
    /// fetches done there for repository workspaces.
    data: Result<PathBuf, String>,
    stores: Option<crate::git::Service>,
    /// Deletions of repository workspaces' conversations under way.
    removals: Vec<Removal>,
    /// The schedules (DESIGN.md §3), and when the soonest fires next:
    /// at once when the window starts, so a time missed while it did not
    /// run is handled first.
    schedules: crate::schedule::Schedules,
    next_fire: Option<u64>,
    /// The person's commands' directory, beside the configuration
    /// (DESIGN.md §15).
    commands: Option<PathBuf>,
    /// The person's skills' directory, beside it (DESIGN.md §12).
    skills: Option<PathBuf>,
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
    /// What was last said of prompts that cannot be used, not to say it again.
    prompt_troubles: Vec<String>,
    /// The conversation open after the last request for a new one was served.
    made: Option<Id>,
    /// What reads folders for the chooser.
    folders: crate::chooser::FolderReader,
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
    held: Vec<crate::supervisor::Pending>,
    survey: Option<Receiver<Vec<crate::removal::Found>>>,
}

impl Session {
    /// Carries out what the app asked.
    fn serve(&mut self, requests: Vec<Request>) {
        self.view.asked(&requests);
        for request in requests {
            match request {
                // `asked` took it, or found it had closed before the window
                // heard, and that window has cleared what it showed.
                Request::Open(id) => {
                    if !self.view.take_refused_open() {
                        self.open(id, None);
                    } else {
                        self.view.note("that conversation has closed, so it did not open");
                        self.serve(vec![Request::Redraw]);
                    }
                }
                Request::New => self.start(None, false),
                Request::NewScratch(unconfined) => self.start_in(None, unconfined),
                Request::NewIn(folder, unconfined) => self.start_in(Some(folder), unconfined),
                Request::NewFrom(name) => self.start_from(&name),
                Request::SetPrompt {
                    ask,
                    name,
                    from,
                    text,
                } => self.set_prompt(ask, name, &from, &text),
                Request::ListPrompts => self.send_prompts(),
                Request::SaveTemplate {
                    template,
                    replacing,
                } => match self.save_template(template, replacing.as_deref()) {
                    Ok(said) => self.view.template_saved(said),
                    Err(why) => self.view.template_refused(why),
                },
                Request::RemoveTemplate(name) => match self.remove_template(&name) {
                    Ok(said) => self.view.template_saved(said),
                    Err(why) => self.view.template_refused(why),
                },
                Request::Send(text, pictures) => self.send_message(text, pictures),
                // The window's own, on its own machine.
                Request::OpenLink(_) | Request::Quit => {}
                Request::Command {
                    name,
                    args,
                    typed,
                    pictures,
                } => {
                    let expanded = match &self.commands {
                        Some(dir) => crate::commands::expand(dir, &name, &args, crate::protocol::MAX_TEXT),
                        None => Ok(None),
                    };
                    // The pictures the typed command named go with what
                    // it expands to.
                    match expanded {
                        Ok(text) => self.send_message(text.unwrap_or(typed), pictures),
                        Err(why) => self.view.give_back(&typed, pictures, why),
                    }
                }
                Request::Received => self.view.received(),
                // Said first, and heard then.
                Request::Claim { .. } => {}
                Request::ListFolders {
                    candidates,
                    or_home,
                    from,
                } => {
                    let home = std::env::var_os("HOME").map(PathBuf::from);
                    if let Some(refused) = self.folders.ask(candidates, or_home, from, home) {
                        self.view.folders(refused);
                    }
                }
                Request::Commands => match self.commands.as_deref().map(crate::commands::list) {
                    Some(Ok((listed, more))) => self.view.show_commands(&listed, more),
                    Some(Err(why)) => self.view.note(why),
                    None => self.view.note(
                        "neither XDG_CONFIG_HOME nor HOME is an absolute path, so there are no commands",
                    ),
                },
                Request::Skills => match self.skills.as_deref().map(crate::skills::scan) {
                    Some(Ok(found)) => self.view.show_skills(&found),
                    Some(Err(why)) => self.view.note(why),
                    None => self.view.note(
                        "neither XDG_CONFIG_HOME nor HOME is an absolute path, so there are no skills",
                    ),
                },
                Request::Retry => {
                    if let Err(e) = self.supervisor.retry() {
                        self.view.note(e);
                    }
                }
                Request::Interrupt => {
                    if let Err(e) = self.supervisor.interrupt() {
                        self.view.note(e);
                    }
                }
                Request::Pause(paused) => {
                    if let Err(e) = self.supervisor.tell(&Down::Pause { paused }) {
                        self.view.note(e);
                    }
                }
                Request::Compact(focus) => {
                    if let Err(e) = self.supervisor.compact(focus) {
                        self.view.note(e);
                    }
                }
                Request::Review(revision) => {
                    if let Err(e) = self.supervisor.review(revision) {
                        self.view.note(e);
                    }
                }
                Request::Schedule(command) => self.schedule(command),
                Request::ClearTodo => {
                    if let Err(e) = self.supervisor.tell(&Down::ClearTodo) {
                        self.view.note(e);
                    }
                }
                Request::Choose { model, effort } => {
                    if let Err(e) = self.supervisor.tell(&Down::Choose { model, effort }) {
                        self.view.note(e);
                    }
                }
                Request::Route { routing } => {
                    if let Err(e) = self.supervisor.tell(&Down::Route { routing }) {
                        self.view.note(e);
                    }
                }
                // The open conversation's log read again and replayed, as
                // opening an adopted one does.
                Request::Redraw => {
                    if let Some(id) = self.view.active().cloned() {
                        match store::read_log(&self.state, &id) {
                            Ok(events) => {
                                let prefix = store::read_prefix(&self.state, &id);
                                self.view
                                    .replay(prefix.as_deref().map_err(String::as_str), events);
                            }
                            Err(e) => self.view.note(format!("the conversation's log: {e}")),
                        }
                    }
                }
                Request::SaveShare(first, total) => {
                    match self.state.save_share(first, total) {
                        Ok(()) => self.view.saved_share(Some((first, total))),
                        Err(e) => self.view.note(e),
                    }
                }
                Request::SaveKey { secret, replace } => self.save_key(&secret, replace),
                Request::Export => self.export(None),
                Request::ExportConversation(id) => self.export(Some(id)),
                Request::Workspace(id) => {
                    let record = self.card_record(&id);
                    self.view.show_workspace(&id, &record);
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
                    self.view.show_output(&id, number, read);
                }
                // Read here, the whole log and one record, once a press:
                // they are the conversation's, only read meanwhile
                // (DESIGN.md §6). Every key held is looked for.
                Request::Debug { id, request } => {
                    let view = match self.known_keys() {
                        Ok((keys, _)) => {
                            let forms: Vec<String> = keys
                                .iter()
                                .flat_map(|key| crate::wire::key_forms(key.expose()))
                                .collect();
                            crate::wire::view(&self.state, &id, request, &forms)
                        }
                        Err(problem) => Err(format!(
                            "the stored key cannot be read to keep it out of the view: {problem}"
                        )),
                    };
                    self.view.show_debug(&id, request, view);
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
                Request::SetAllAllowed { conversation, on } => {
                    self.set_all_allowed(&conversation, on)
                }
                Request::Trust {
                    conversation,
                    digest,
                } => self.trust(&conversation, digest.as_deref()),
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
            Answer::AllowAll => (
                true,
                Some(
                    set_all_allowed(&self.state, conversation, true)
                        .map(|()| "all calls in this conversation".to_string()),
                ),
            ),
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
                self.view.note(format!("remembered {said}"));
                Some(said)
            }
            Some(Err(e)) => {
                let said = format!(
                    "your answer holds this once; it could not be added to your rules: {e}"
                );
                logged!("td-agent: {said}");
                self.view.note(said);
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
            Ok(text) => {
                let policy = crate::rules::parse_policy(text).ok();
                self.view.set_all_allowed(
                    policy
                        .as_ref()
                        .map_or_else(Vec::new, |policy| policy.all_allowed.clone()),
                );
                self.view.set_modes(policy.map(|policy| policy.modes));
            }
            Err(e) => {
                let said = format!("your rules could not be read, so every call that changes a workspace or runs a command asks: {e}");
                logged!("td-agent: {said}");
                self.view.note(said);
                self.view.set_modes(None);
                self.view.set_all_allowed(Vec::new());
            }
        }
        self.supervisor.repolicy(rules, self.mode);
        self.send_prompts();
    }

    /// Tells the window the prompts as they are, so its dialog opens on them.
    fn send_prompts(&mut self) {
        let entries = crate::prompts::listing(self.state.root());
        let troubles: Vec<String> = entries
            .iter()
            .filter_map(|entry| entry.trouble.clone())
            .collect();
        for why in troubles
            .iter()
            .filter(|why| !self.prompt_troubles.contains(why))
        {
            let said = format!(
                "turns stop until File > Prompts\u{2026} replaces a prompt that cannot be used: {why}"
            );
            logged!("td-agent: {said}");
            self.view.note(said);
        }
        self.prompt_troubles = troubles;
        self.view.set_prompts(entries);
    }

    /// Puts `text` in force as the prompt `name`; each conversation reads it
    /// from its next request (DESIGN.md §13).
    fn set_prompt(&mut self, ask: u64, name: crate::prompts::Name, from: &str, text: &str) {
        // A window told before the file moved on overwrites nothing it did not
        // show; a save made again after a lost answer finds its own text there.
        let listed = crate::prompts::listing(self.state.root());
        let now = listed.iter().find(|entry| entry.name == name);
        if now.is_some_and(|entry| entry.text != from && entry.text != text) {
            self.send_prompts();
            return self.view.prompt_refused(ask, format!(
                "{} changed since this dialog opened, so it stays as it is: copy what you want to keep, then leave and open it again",
                name.title()
            ));
        }
        match crate::prompts::save(self.state.root(), name, Some(text)) {
            Ok(()) => {
                self.send_prompts();
                self.view.prompt_saved(
                    ask,
                    format!(
                        "{} saved; it holds from each conversation's next request",
                        name.title()
                    ),
                );
            }
            Err(why) => self
                .view
                .prompt_refused(ask, format!("{} stays as it was: {why}", name.title())),
        }
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
                self.view.note(match digest {
                    Some(_) => "the classifier is now given this workspace's project instructions, as the card shows them",
                    None => "the classifier is no longer given this workspace's project instructions",
                });
                self.repolicy();
            }
            Err(e) => self
                .view
                .note(format!("the workspace's trust mark is unchanged: {e}")),
        }
        let record = self.card_record(conversation);
        self.view.reshow_workspace(conversation, &record);
    }

    /// Allows all in `conversation`, or stops, in the human's rules (DESIGN.md §11),
    /// and sends every conversation the rules.
    fn set_all_allowed(&mut self, conversation: &Id, on: bool) {
        match set_all_allowed(&self.state, conversation, on) {
            Ok(()) => {
                self.view.note(if on {
                    "all calls in this conversation are allowed"
                } else {
                    "this conversation's calls are judged again"
                });
                self.repolicy();
            }
            Err(e) => self.view.note(format!("nothing changed: {e}")),
        }
    }

    fn set_mode(&mut self, conversation: &Id, mode: crate::config::Mode) {
        match set_mode(&self.state, conversation, mode) {
            Ok(()) => {
                // The human's own choice ends any breaker's hold.
                if let Ok(key) = workspace_key(&self.state, conversation) {
                    self.held.retain(|held| held != &key);
                }
                self.view
                    .note(format!("this workspace is now in {} mode", mode.word()));
                self.repolicy();
            }
            Err(e) => self
                .view
                .note(format!("the workspace's mode is unchanged: {e}")),
        }
    }

    /// Stores the key from the dialog (DESIGN.md §6) and hands it to every
    /// conversation process, running or started later, and to the credit
    /// fetch; a stored key is replaced only when `replace` says so.
    fn save_key(&mut self, secret: &Secret, replace: bool) {
        let Some(path) = self.key_path.clone() else {
            self.view.key_refused(
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
                self.view.key_saved(&path.display().to_string());
            }
            Err(Unwritten::Exists) => self.view.key_exists(),
            Err(Unwritten::Refused(why)) => {
                logged!("td-agent: saving the key: {why}");
                self.view.key_refused(why);
            }
        }
    }

    /// A window has attached: it is told all there is, the person's rules are
    /// read again since they may have opened it to have them read, and it is
    /// shown the open conversation from its log.
    fn attached(&mut self) {
        self.view.attach();
        self.repolicy();
        self.serve(vec![Request::Redraw]);
    }

    /// Whether a schedule keeps the broker for its firing.
    fn schedule_can_fire(&self) -> bool {
        let directory = self.view.directory();
        self.schedules
            .can_fire(&|id| crate::schedule::target(&directory, id))
    }

    /// Whether nothing would be lost, or left undone, by ending now.
    fn idle(&self) -> bool {
        !self.supervisor.working()
            && !self.view.given_back_waits()
            && !self.supervisor.held_for(|id| self.view.opens(id))
            && self.removals.is_empty()
            && !self
                .pushes
                .iter()
                .any(|(_, push)| push.staged.is_none() && push.live)
            && self.export.is_none()
            && !self.schedule_can_fire()
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
                self.view.update(Update::Failed { reason }, store::now_ms());
            }
            return;
        };
        match opened {
            Ok(Opened::Started) => {}
            Ok(Opened::Adopted) => match store::read_log(&self.state, &id) {
                Ok(events) => {
                    let prefix = store::read_prefix(&self.state, &id);
                    self.view
                        .replay(prefix.as_deref().map_err(String::as_str), events);
                }
                Err(e) => self.view.note(format!("the conversation's log: {e}")),
            },
            Err(reason) => self.view.update(Update::Failed { reason }, store::now_ms()),
        }
    }

    /// A new conversation in a workspace: a scratch one, or `folder`,
    /// admitted first (DESIGN.md §8), its refusal said by name.
    fn start_in(&mut self, folder: Option<PathBuf>, unconfined: bool) {
        let places = match &self.places {
            Ok(places) => places,
            Err(why) => return self.view.note(why.clone()),
        };
        if let (Some(why), false) = (&self.jailless, unconfined) {
            return self
                .view
                .note(format!("no jailed workspace can be made: {why}"));
        }
        let workspace = match folder {
            None => Workspace::Scratch,
            Some(folder) => {
                match workspace::admit_directory(&folder, places, &self.client.shared) {
                    Ok(admitted) => Workspace::Directory(admitted),
                    Err(why) => {
                        self.view.note(format!("no conversation there: {why}"));
                        // A repository is a template's remote: the dialog
                        // offers one (DESIGN.md §7).
                        if let Some(repository) = workspace::repository_remote(&folder) {
                            match repository.to_str() {
                                Some(remote) => self.view.open_new_template(Some(remote)),
                                None => self.view.note(format!(
                                    "{} cannot be a remote: it is not UTF-8",
                                    repository.display()
                                )),
                            }
                        }
                        return;
                    }
                }
            }
        };
        self.start(Some(workspace), unconfined);
    }

    /// A new conversation from the configured template `name`
    /// (DESIGN.md §7). One naming repositories makes their workspace's
    /// record, every remote admitted, which its conversation's process
    /// then prepares; refused, nothing is made.
    fn start_from(&mut self, name: &str) {
        let root = match &self.places {
            Ok(places) => places.root.clone(),
            Err(why) => return self.view.note(why.clone()),
        };
        if let Some(why) = &self.jailless {
            return self.view.note(format!(
                "a template's workspace is always jailed, and there is no jail: {why}"
            ));
        }
        let template = match chosen(&self.templates, name) {
            Ok(template) => template,
            Err(why) => return self.view.note(why),
        };
        if template.repos.is_empty() {
            return self.start(Some(Workspace::Template(name.to_string())), false);
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
                self.start_as(id, Some(Workspace::Repositories(repositories)), false)
            }
            Ok(Err(unadmitted)) => self.view.ask_admission(name.to_string(), unadmitted),
            Err(why) => self
                .view
                .note(format!("no conversation from template {name:?}: {why}")),
        }
    }

    /// Keeps `template`, made in the window, in place of the one so made
    /// named `replacing` (DESIGN.md §7): checked against every template's
    /// name, planned where its workspaces would be made, written, then
    /// listed, each conversation told its shared directories.
    fn save_template(
        &mut self,
        template: Template,
        replacing: Option<&str>,
    ) -> Result<String, String> {
        let saved = self.saved.as_ref().map_err(Clone::clone)?;
        let next = with_template(&self.configured, saved, template.clone(), replacing)?;
        // Workspaces find their template by name: a new name that
        // conversations of an earlier template still carry would hand
        // them this one's folders and network.
        if replacing != Some(template.name.as_str()) {
            let orphans = orphans(&self.state.list().0, &template.name);
            if orphans > 0 {
                return Err(format!(
                    "{orphans} conversation(s) made from an earlier template named {:?} are kept, and would take this one's shared folders and network; choose another name, or delete them first",
                    template.name
                ));
            }
        }
        // Only the template saved is admitted again, so saving one never
        // changes what another's workspaces reach.
        let (own, refused) = own_shared(&template, self.places.as_ref().ok());
        if let (Ok(places), Ok(data)) = (&self.places, &self.data) {
            if !template.repos.is_empty() {
                let shared = own.as_ref().map_or(self.client.shared.len(), Vec::len);
                let id = Id::random()?;
                workspace::plan(&template, &id, data, &places.root, shared)?;
            }
        }
        self.keep_templates(next, Some((template.name.as_str(), own)))?;
        let refused = match refused.as_slice() {
            [] => String::new(),
            notes => format!("; its workspaces are not given: {}", notes.join("; ")),
        };
        Ok(match replacing {
            Some(old) if old != template.name => format!(
                "the template {old:?} is now {:?}; workspaces made from it are kept, and bind the shared directories and reach the network no more",
                template.name
            ),
            Some(_) => format!("the template {:?} is saved", template.name),
            None => format!("the template {:?} is saved; File \u{2192} New conversation\u{2026} lists it", template.name),
        } + &refused)
    }

    /// Removes the template made in the window named `name`.
    fn remove_template(&mut self, name: &str) -> Result<String, String> {
        let saved = self.saved.as_ref().map_err(Clone::clone)?;
        if !saved.iter().any(|t| t.name == name) {
            return Err(format!("no template made in the window is named {name:?}"));
        }
        let next = saved.iter().filter(|t| t.name != name).cloned().collect();
        self.keep_templates(next, None)?;
        Ok(format!(
            "the template {name:?} is removed; workspaces made from it are kept, and bind the shared directories and reach the network no more"
        ))
    }

    /// `saved` written as the templates file, then the chooser's list, the
    /// edit list and every conversation's shared directories made anew
    /// from it: `admitted`'s template its admitted list, every other its
    /// entry as held, never re-admitted; refused, nothing changes.
    fn keep_templates(
        &mut self,
        saved: Vec<Template>,
        admitted: Option<(&str, Option<Vec<crate::workspace::Shared>>)>,
    ) -> Result<(), String> {
        let (templates, _) = merged_templates(&self.configured, &saved);
        let mut client = self.client.clone();
        client.template_shared = kept_template_shared(
            &self.client.template_shared,
            &self.configured,
            &templates,
            admitted,
        );
        if client.to_json().to_string().len() > SETUP_CLIENT_BYTES {
            return Err(
                "the templates' shared directories, network policies, MCP servers and runtimes would be too many to hand to conversations"
                    .into(),
            );
        }
        self.state.save_templates(&saved)?;
        self.view.set_templates(
            templates
                .iter()
                .map(|template| (template.name.clone(), !template.repos.is_empty()))
                .collect(),
        );
        self.templates = templates;
        self.saved = Ok(saved);
        self.view
            .set_saved_templates(self.saved.as_deref().unwrap_or_default().to_vec());
        self.view.set_networks(&client);
        self.client = client;
        self.supervisor.reconfigure(self.client.clone());
        Ok(())
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
                // No workspace made from now on reaches a local one.
                if let Ok(places) = &mut self.places {
                    places.locals = local_paths(&self.remotes);
                }
                self.start_from(name);
            }
            Err(why) => self
                .view
                .note(format!("no conversation from template {name:?}: {why}")),
        }
    }

    /// A new conversation, created by its own process and opened.
    fn start(&mut self, workspace: Option<Workspace>, unconfined: bool) {
        match Id::random() {
            Ok(id) => self.start_as(id, workspace, unconfined),
            Err(e) => self.view.note(format!("a new conversation: {e}")),
        }
    }

    /// `start`, with the conversation's id given.
    /// `unconfined` runs its tools outside any jail (DESIGN.md §8).
    fn start_as(&mut self, id: Id, workspace: Option<Workspace>, unconfined: bool) {
        self.view.add_row(Row {
            id: id.clone(),
            title: Role::Conversation.first_title().to_string(),
            activity: store::now_ms(),
            state: RowState::Starting,
            paused: false,
            workspace: workspace.clone(),
            unconfined,
            archived: false,
        });
        self.view.set_active(id.clone());
        self.view.focus_composer();
        match workspace {
            None => self.open(id, Some(Role::Conversation)),
            Some(workspace) => {
                let created = if unconfined {
                    self.supervisor
                        .create_unconfined(id, Role::Conversation, workspace)
                } else {
                    self.supervisor.create(id, Role::Conversation, workspace)
                };
                self.opened(created);
            }
        }
    }

    /// What the conversation processes said: reservations answered from
    /// the ledger, messages between them routed, the rest to the app.
    fn hear(&mut self) {
        let now = store::now();
        let directory = self.view.directory();
        for (id, update) in self.supervisor.poll() {
            if let Some(note) = self
                .post
                .hear(&id, &update, &mut self.supervisor, &directory)
            {
                logged!("td-agent: {note}");
                self.view.note(note);
            }
            if let Update::Up(Up::Schedule { id: ask, op }) = &update {
                self.schedule_for(&id, *ask, op.clone(), &directory);
            }
            match &update {
                // A card is the window's, whichever conversation is open.
                Update::Up(Up::Ask {
                    call,
                    title,
                    details,
                    always,
                }) => {
                    self.view.ask(Card {
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
                    self.view.ask(Card {
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
                    self.view.withdraw(&id, Some(*call));
                    continue;
                }
                Update::Up(Up::Question {
                    call,
                    question,
                    options,
                }) => {
                    self.view.question(crate::ui::Asked {
                        conversation: id.clone(),
                        call: *call,
                        question: question.clone(),
                        options: options.clone(),
                    });
                    continue;
                }
                // The classifier's breaker: only ever `ask` (DESIGN.md
                // §11).
                Update::Up(Up::Brake { why }) => {
                    let why = crate::tools::visible(why);
                    match set_mode(&self.state, &id, crate::config::Mode::Ask) {
                        Ok(()) => {
                            self.view.note(format!("the classifier's circuit breaker tripped ({why}), so this conversation's workspace is in ask mode; only you can put it back in auto mode"));
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
                            self.view.note(format!("the classifier's circuit breaker tripped ({why}), but its workspace's mode could not be written: {e}; it is held in ask mode until you choose its mode"));
                            self.repolicy();
                        }
                    }
                }
                // A process started again asks nothing yet.
                Update::Up(Up::Hello { .. }) => {
                    self.view.withdraw(&id, None);
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
                    asks,
                }) => {
                    let push = crate::git::Push {
                        id: commit.clone(),
                        branch: branch.clone(),
                        lease: lease.clone(),
                        asks: *asks,
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
                        self.view.note(e);
                    }
                }
                Update::Restarting { .. } | Update::Failed { .. } => {
                    self.ledger.forget(&id);
                    self.view.withdraw(&id, None);
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
            let open = self.supervisor.open_id() == Some(&id);
            if let (
                true,
                Update::Refused {
                    text: Some(text),
                    pictures,
                    reason,
                },
            ) = (open, &update)
            {
                // Back in the composer with its pictures, read back from
                // where they were staged or kept.
                let back = crate::picture::unstage_all(&self.state, &id, pictures, |digest| {
                    self.supervisor.stages(&id, digest)
                });
                self.view
                    .give_back(text, back, format!("refused: {reason}"));
            } else if open {
                self.view.update(update, store::now_ms());
            } else if let Update::Refused {
                text,
                pictures,
                reason,
            } = &update
            {
                // No composer of its own to go back to: said, its text
                // kept whole in serve.log and its pictures where they
                // were staged.
                let note = match text {
                    Some(text) => {
                        logged!(
                            "td-agent: conversation {id} refused a message ({reason}):\n{text}"
                        );
                        let kept = if pictures.is_empty() {
                            String::new()
                        } else {
                            let dir = crate::picture::staging(&self.state, &id);
                            // One the conversation took before refusing
                            // is where it keeps its own.
                            let kept = crate::picture::kept(&self.state.conversation(&id));
                            for picture in pictures {
                                let staged = dir.join(&picture.digest);
                                let at = if staged.exists() {
                                    staged
                                } else {
                                    kept.join(&picture.digest)
                                };
                                logged!(
                                    "td-agent: its picture {} is kept as {}",
                                    picture.stand_in(),
                                    at.display()
                                );
                            }
                            format!(", its {} picture(s) where serve.log says", pictures.len())
                        };
                        format!(
                            "conversation {id} refused a message: {reason}; its text is in serve.log error{kept}"
                        )
                    }
                    None => format!("conversation {id} refused: {reason}"),
                };
                self.view.note(note);
            } else {
                self.view.background(&id, &update, store::now_ms());
            }
        }
        let directory = self.view.directory();
        for note in self.post.deliver(&mut self.supervisor, &directory) {
            logged!("td-agent: {note}");
            self.view.note(note);
        }
        let today = self.ledger.today(now);
        self.view.set_today(today);
        self.view.set_queued(self.supervisor.queued());
        self.fetched();
        self.exported();
        self.surveyed();
        self.tasks_read();
    }

    /// Reads, on a thread of its own, the sub-agents each of `ids`'s
    /// logs records, for the list (DESIGN.md §4), in the order given.
    fn read_tasks(&mut self, ids: Vec<Id>) {
        let state = self.state.clone();
        let (send, receive) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("td-agent-tasks".into())
            .spawn(move || {
                for id in ids {
                    let read = store::read_tasks(&state, &id);
                    if send.send((id, read)).is_err() {
                        return;
                    }
                }
            });
        match spawned {
            Ok(_) => self.tasks = Some(receive),
            Err(e) => self.view.note(format!("the list's sub-agents: {e}")),
        }
    }

    /// The sub-agents read since the last poll, into the list at once.
    fn tasks_read(&mut self) {
        let Some(receive) = self.tasks.as_ref() else {
            return;
        };
        let mut read = Vec::new();
        loop {
            match receive.try_recv() {
                Ok((id, Ok(starts))) => read.push((id, starts)),
                Ok((id, Err(e))) => logged!("td-agent: the sub-agents of {id}: {e}"),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.tasks = None;
                    break;
                }
            }
        }
        if !read.is_empty() {
            self.view.set_sub_agents(read);
        }
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
                logged!("td-agent: the configuration: {e}");
                self.model_key.clone()
            }
        };
        if let Err(e) = self.state.save_default_model(&model, over.as_deref()) {
            logged!("td-agent: the default model: {e}");
            return self
                .view
                .note(format!("the default model was not saved: {e}"));
        }
        self.client.model = model;
        self.supervisor.reconfigure(self.client.clone());
        self.view.set_default_model(&self.client.model);
        self.view.note(format!(
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
        if bases.len() > 48
            || bases
                .iter()
                .any(|base| crate::git::tracking_ref(base).is_err())
        {
            return Err("invalid or excessive fetch refs".into());
        }
        let parsed = self.admitted(id, remote, &[])?;
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
        self.local(&parsed)?;
        Ok(parsed)
    }

    /// Where a workspace reaches (`reached`).
    fn reached(&self) -> Vec<PathBuf> {
        reached(&self.state.list().0, &self.client)
    }

    /// Whether `remote` may be fetched or pushed where it lies
    /// (`local_remote`), checked at every fetch and push.
    fn local(&self, remote: &crate::git::Remote) -> Result<(), String> {
        local_remote(remote, &self.places, &self.reached())
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
            logged!("td-agent: {note}");
            self.view.note(note);
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
                    logged!("td-agent: {said}");
                    self.troubles.insert(what, said);
                }
            }
        }
    }

    /// Queues each schedule's firing that is due (DESIGN.md §3), which the
    /// next poll delivers as it does any message. One to a conversation
    /// the window sees running a turn, or paused, is delivered to be
    /// logged skipped; one to a conversation archived, failed or gone
    /// fires nothing.
    fn fire(&mut self) {
        let now = store::now();
        if self.next_fire.is_none_or(|at| now < at) {
            return;
        }
        let directory = self.view.directory();
        let target = |id: &Id| crate::schedule::target(&directory, id);
        match self.schedules.due(now, &target) {
            Ok(due) => {
                for due in due {
                    let schedule = due.schedule.clone();
                    if let Err(e) = self.post.fire(due) {
                        logged!("td-agent: schedule {schedule} could not fire: {e}");
                        self.view
                            .note(format!("schedule {schedule} could not fire: {e}"));
                    }
                }
                self.next_fire = self.schedules.soonest();
            }
            // Nothing fires that is not journaled first: tried again in a
            // minute, said each time.
            Err(e) => {
                logged!("td-agent: the schedules could not be saved, so none fired: {e}");
                self.view.note(format!(
                    "the schedules could not be saved, so none fired: {e}"
                ));
                self.next_fire = now.checked_add(60);
            }
        }
    }

    /// A conversation's `schedule`, which the person allowed on its card,
    /// `schedules` or `cancel_schedule` answered (DESIGN.md §3); one made
    /// or cancelled is said to the person.
    fn schedule_for(
        &mut self,
        from: &Id,
        ask: u64,
        op: crate::protocol::ScheduleOp,
        directory: &[crate::post::Entry],
    ) {
        use crate::schedule::Change;
        let archived = |id: &Id| directory.iter().find(|e| &e.id == id).map(|e| e.archived);
        let (answer, change) = self.schedules.answer(from, op, &archived, store::now());
        match &change {
            Some(Change::Made {
                id,
                to,
                written,
                next,
            }) => self.view.note(format!(
                "{} made schedule {id}, to {}: {written}, next {next}; /unschedule {id} removes it",
                self.view.named(from),
                self.view.named(to),
            )),
            Some(Change::Cancelled(id)) => self
                .view
                .note(format!("{} cancelled schedule {id}", self.view.named(from))),
            None => {}
        }
        if change.is_some() {
            self.next_fire = self.schedules.soonest();
        }
        self.supervisor
            .answer(from, &Down::Scheduled { id: ask, answer });
    }

    /// The person's message, to the open conversation, put back in the
    /// composer when it cannot go.
    fn send_message(
        &mut self,
        text: String,
        pictures: Vec<(crate::picture::Reference, std::sync::Arc<[u8]>)>,
    ) {
        let references: Vec<_> = pictures.iter().map(|(r, _)| r.clone()).collect();
        // Each picture staged before the message goes, so its process
        // finds them (DESIGN.md §6, Pictures).
        let sent = match self.supervisor.open_id().cloned() {
            None => Err("no conversation is open".to_string()),
            Some(id) => pictures
                .iter()
                .try_for_each(|(reference, bytes)| {
                    crate::picture::stage(&self.state, &id, reference, bytes)
                })
                .map_err(|e| format!("a picture could not be staged: {e}"))
                .and_then(|()| self.supervisor.send(text.clone(), references.clone()))
                .inspect_err(|_| {
                    // A picture another pending message names stays.
                    for reference in &references {
                        if !self.supervisor.stages(&id, &reference.digest) {
                            crate::picture::unstage(&self.state, &id, reference);
                        }
                    }
                }),
        };
        if let Err(e) = sent {
            self.view.give_back(&text, pictures, e);
        }
        self.view.set_queued(self.supervisor.queued());
    }

    /// The composer's schedule commands (DESIGN.md §3).
    fn schedule(&mut self, command: crate::schedule::Command) {
        use crate::schedule::Command;
        let now = store::now();
        match command {
            Command::Add {
                when,
                text,
                catch_up,
            } => {
                let Some(to) = self.view.active().cloned() else {
                    self.view.note("no conversation is open");
                    return;
                };
                match self
                    .schedules
                    .add(&when, to, text.clone(), catch_up, None, now)
                {
                    Ok(made) => {
                        let next = self.schedules.upcoming(&made, now, 3).join(", ");
                        let catching = if made.catch_up {
                            "; a time missed while td-agent is not running fires once at startup"
                        } else {
                            ""
                        };
                        self.view.note(format!(
                            "schedule {} made: {}, next {next}{catching}",
                            made.id, made.written
                        ));
                        self.next_fire = self.schedules.soonest();
                    }
                    // The command goes back to the composer, as a message
                    // that could not be sent does.
                    Err(e) => {
                        let catching = if catch_up { "catch-up " } else { "" };
                        self.view.restore(
                            &format!("/schedule {catching}{when} {text}"),
                            format!("no schedule was made: {e}"),
                        );
                    }
                }
            }
            Command::List => {
                let directory = self.view.directory();
                let entries: Vec<(String, String)> = self
                    .schedules
                    .list()
                    .iter()
                    .map(|s| {
                        let to = match directory.iter().find(|e| e.id == s.to) {
                            None => "a conversation since deleted, so it fires nothing".into(),
                            Some(e) if e.archived => {
                                format!("{}, archived, so it fires nothing", self.view.named(&s.to))
                            }
                            Some(e) if e.failed => format!(
                                "{}, whose process failed, so it fires nothing until reopened",
                                self.view.named(&s.to)
                            ),
                            Some(_) => self.view.named(&s.to),
                        };
                        let next = self.schedules.upcoming(s, now, 3);
                        let next = match next.is_empty() {
                            true => "never again".to_string(),
                            false => next.join(", "),
                        };
                        let by = match &s.author {
                            None => "you".to_string(),
                            Some(author) => self.view.named(author),
                        };
                        let catching = if s.catch_up { "; catches up" } else { "" };
                        // The expression in the body: a header has a bound
                        // a long one passes.
                        (
                            format!("schedule {}", s.id),
                            format!(
                                "{}\nto {to}; made by {by}{catching}\nnext: {next}\n{}",
                                s.written, s.text
                            ),
                        )
                    })
                    .collect();
                self.view.show_schedules(&entries);
            }
            Command::Remove(id) => match self.schedules.remove(&id) {
                Ok(removed) => {
                    self.view.note(format!("schedule {} removed", removed.id));
                    self.next_fire = self.schedules.soonest();
                }
                Err(e) => self.view.note(format!("no schedule was removed: {e}")),
            },
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
        if self.stores.is_none() {
            return;
        }
        let (metas, _) = self.state.list();
        for (remote, bases) in crate::upstream::in_use(&metas, &self.remotes, &self.refreshing) {
            let url = remote.url();
            // Said once while it stands, not at every interval.
            let refused = self.local(&remote).err();
            let skip = refused.is_some();
            self.trouble(
                format!("local {url}"),
                refused.map(|why| format!("not fetching in the background: {why}")),
            );
            let Some(stores) = self.stores.as_ref().filter(|_| !skip) else {
                continue;
            };
            match stores.refresh(remote, bases) {
                Ok(()) => self.refreshing.push(url),
                Err(e) => logged!("td-agent: fetching {url} in the background: {e}"),
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
            Err(e) => self.view.note(format!(
                "the conversation's record could not be read, so a repository workspace it may have is left: {e}"
            )),
            Ok(_) => {}
        }
        // The window's open conversation, whether or not its process runs.
        let was_open = self.view.active() == Some(id);
        let held = self.supervisor.remove(id);
        // Its process has ended, as a crash ends one.
        self.ledger.forget(id);
        self.view.withdraw(id, None);
        self.delete_now(id, was_open, held);
    }

    /// Deletes conversation `id`, its processes stopped and `held` the
    /// human's messages they had not taken, opening the next when
    /// `was_open`; whether it is deleted.
    fn delete_now(
        &mut self,
        id: &Id,
        was_open: bool,
        held: Vec<crate::supervisor::Pending>,
    ) -> bool {
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
                logged!("td-agent: deleting {id}: {e}");
                self.view
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
            logged!("td-agent: deleting {id}: {problem}");
            said.push_str(&format!(
                "; what is left of its files goes at the next start: {problem}"
            ));
        }
        if let Err(e) = self.post.forget(id) {
            logged!("td-agent: the outbox: {e}");
            said.push_str(&format!("; the outbox: {e}"));
        }
        match forget_rules(&self.state, ruled.as_deref(), id) {
            Ok(false) => {}
            Ok(true) => self.repolicy(),
            Err(e) => {
                logged!("td-agent: forgetting the rules for {id}: {e}");
                said.push_str(&format!("; its rules stay in your rules file: {e}"));
            }
        }
        self.view.remove_row(id);
        self.view.note(said);
        if was_open {
            if let Some(next) = self.view.most_recent() {
                self.view.set_active(next.clone());
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
                .view
                .note("that conversation is already being archived or deleted");
        }
        let doing = if archive { "archiving" } else { "deleting" };
        let was_open = self.view.active() == Some(id);
        let held = self.supervisor.remove(id);
        self.ledger.forget(id);
        self.view.withdraw(id, None);
        self.view.set_closing(id, Some(doing));
        let title = self.view.title(id);
        let (tell, survey) = mpsc::channel();
        let (state, of, asked) = (self.state.clone(), id.clone(), repositories.clone());
        let spawned = std::thread::Builder::new()
            .name("td-agent-survey".into())
            .spawn(move || {
                let programs = Programs::from_env();
                let _ = tell.send(crate::removal::survey(&state, &of, &asked, programs));
            });
        self.view.note(format!(
            "{title:?}: asking its workspace what removing it would lose, before {doing} it"
        ));
        let survey = match spawned {
            Ok(_) => Some(survey),
            Err(e) => {
                self.view.ask_removal(
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
            if let Some(next) = self.view.most_recent() {
                self.view.set_active(next.clone());
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
                self.view.ask_removal(id, title, lost, archive);
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
        self.view.set_closing(id, None);
        let done = if removal.archive {
            "archived"
        } else {
            "deleted"
        };
        if !remove {
            self.supervisor.park(id.clone(), removal.held);
            return self.view.note(format!(
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
        logged!("td-agent: {said}");
        self.view.note(said);
    }

    /// Archives conversation `id`, or brings it back (DESIGN.md §7). An
    /// archived conversation has no process: its own and its background
    /// ones end first, as a deletion's do, and the human's messages it had
    /// not taken wait for its next. Messages for it wait in the outbox.
    fn archive(&mut self, id: &Id, archived: bool) {
        if self.removals.iter().any(|removal| &removal.id == id) {
            return self
                .view
                .note("that conversation is being archived or deleted already");
        }
        // One still being made has no `meta` to mark: stopping its
        // process would leave it half made.
        if let Err(e) = self.state.archived(id) {
            return self
                .view
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
                Err(e) => self.view.note(format!(
                    "the conversation's record could not be read, so a repository workspace it may have is left: {e}"
                )),
                Ok(_) => {}
            }
        }
        let was_open = self.view.active() == Some(id);
        let held = if archived {
            let held = self.supervisor.remove(id);
            self.ledger.forget(id);
            self.view.withdraw(id, None);
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
        held: Vec<crate::supervisor::Pending>,
    ) -> bool {
        let done = if archived { "archived" } else { "unarchived" };
        let mut trouble = None;
        if let Err(e) = self
            .state
            .set_archived(id, archived, removed, Duration::from_secs(2))
        {
            logged!("td-agent: {done} {id}: {e}");
            // A failure after the new `meta` was put in place, syncing
            // its directory, still marked it: what is stored decides.
            if self.state.archived(id) != Ok(archived)
                || (removed && self.state.removed(id) != Ok(true))
            {
                self.view
                    .note(format!("the conversation was not {done}: {e}"));
                self.supervisor.park(id.clone(), held);
                if was_open {
                    self.open(id.clone(), None);
                }
                return false;
            }
            trouble = Some(e);
        }
        let waiting = held.len();
        self.supervisor.park(id.clone(), held);
        self.view.set_archived(id, archived);
        let title = self.view.title(id);
        self.view.note(match trouble {
            None => format!("{title:?} is {done}"),
            Some(e) => format!("{title:?} is {done}, though writing it said: {e}"),
        });
        if archived && waiting > 0 {
            // Nothing else says so, and it does not keep the broker.
            self.view.note(format!(
                "{waiting} message(s) it had not taken wait for {title:?} to be unarchived, and are in serve.log if td-agent serve ends first"
            ));
        }
        if was_open {
            if let Some(next) = self.view.most_recent() {
                self.view.set_active(next.clone());
                self.open(next, None);
            }
        }
        true
    }

    /// Every key this window has held, and the stored one, only to look
    /// for, with why the stored one could not be read; refused when it
    /// cannot be and no other is held.
    fn known_keys(&self) -> Result<(Vec<Secret>, Option<String>), String> {
        let mut keys = self.keys.clone();
        let mut key_problem = None;
        if let Some(path) = self.key_path.as_deref() {
            match key::read(path) {
                Ok(stored) if !keys.contains(&stored) => keys.push(stored),
                Ok(_) | Err(key::Problem::Missing(_)) => {}
                Err(problem) if keys.is_empty() => return Err(problem.to_string()),
                Err(problem) => key_problem = Some(problem.to_string()),
            }
        }
        Ok((keys, key_problem))
    }

    /// Starts the diagnostics export (DESIGN.md §4), of every
    /// conversation or of `conversation` alone, on a thread, into
    /// `~/Downloads`, else the home directory: never where a workspace
    /// reaches, since the archive holds conversations' logs.
    fn export(&mut self, conversation: Option<Id>) {
        if self.export.is_some() {
            return self.view.note("a diagnostics export is already under way");
        }
        let Some(home) = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|home| home.is_absolute())
        else {
            return self
                .view
                .note("no diagnostics: HOME is not an absolute path to write them under");
        };
        let downloads = home.join("Downloads");
        // Not where a workspace reaches.
        let directories = self.reached();
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
        // No file holding a key is taken.
        let (keys, key_problem) = match self.known_keys() {
            Ok(known) => known,
            Err(problem) => {
                return self.view.note(format!(
                "no diagnostics: the stored key cannot be read to keep it out of them: {problem}"
            ))
            }
        };
        let sources = diagnostics::Sources {
            state: self.state.root().to_path_buf(),
            config,
            key_file: self.key_path.clone(),
            keys,
            key_problem,
            conversation,
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
                self.view.note("exporting diagnostics\u{2026}");
            }
            Err(e) => self.view.note(format!("the diagnostics export: {e}")),
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
                let holds = if exported.one {
                    "that conversation's files"
                } else {
                    "your conversations"
                };
                let mut note = format!(
                    "diagnostics written to {} ({} files, {} left out, as its MANIFEST lists); it holds {holds} and your configuration, never the key file: read it before sharing",
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
        logged!("td-agent: {note}");
        self.view.note(note);
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
                    logged!("td-agent: the models list: {e}");
                    self.view.note(format!("the models list: {e}"));
                }
                Fetched::Credit(Ok(credit)) => self.view.set_credit(Some(credit.show())),
                Fetched::Credit(Err(e)) => {
                    self.view.set_credit(Some("credit unknown".into()));
                    logged!("td-agent: the key's credit: {e}");
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
        self.view.set_models(&self.client.model, contexts);
        let offers = models.models.iter().map(Offer::of).collect();
        self.view.set_offers(offers, &self.client.reasoning_effort);
    }
}

/// The window attached, if any.
struct Attached {
    peer: Peer,
    /// Which window, for a far one.
    token: Option<String>,
}

/// What a broker says of a fetch service it did not find: for serve.log and for
/// the window. One may yet appear under a runtime directory; none can without one.
fn no_fetch_service(runtime: Option<&std::ffi::OsStr>) -> (String, &'static str) {
    match runtime.filter(|runtime| !runtime.is_empty()) {
        Some(runtime) => (
            format!(
                "no fetch service under XDG_RUNTIME_DIR={} yet, and each request looks \
                 again: on td it is the fetchd unit; on a host, run td-agent as \
                 ./install-apps installs it, after td-agent serve --stop ends this one",
                Path::new(runtime).display()
            ),
            "no fetch service here yet, so no turn can reach the model; see serve.log",
        ),
        None => (
            "no fetch service: XDG_RUNTIME_DIR is not set, so none can be found; \
             td-agent serve --stop ends this one, for one started with it set"
                .to_string(),
            "no fetch service, so no turn can reach the model; see serve.log",
        ),
    }
}

/// How long a stop waits for connections still saying hello.
const GREETING_AT_STOP: Duration = Duration::from_secs(1);

/// How long a far window has to say which it is.
const CLAIMING: Duration = Duration::from_secs(30);

/// What a far window said of itself, when it has.
fn claim_of(peer: &Peer) -> Result<Option<(String, bool)>, String> {
    let Some(bytes) = peer.recv()? else {
        return Ok(None);
    };
    match crate::window_wire::decode_asked(&bytes)? {
        (_, Request::Claim { token, asked }) => Ok(Some((token, asked))),
        _ => Err("its first word was not who it is".to_string()),
    }
}

/// Says why this window is being let go, ahead of what is queued for it.
fn goodbye(peer: Peer, why: &str) {
    peer.goodbye(crate::window_wire::encode_view(&crate::view::View::Bye(
        why.to_string(),
    )));
}

/// `goodbye`, with this loop going on: the window may have stopped reading.
fn goodbye_aside(peer: Peer, why: &'static str) {
    let _ = std::thread::Builder::new()
        .name("td-agent-bye".into())
        .spawn(move || goodbye(peer, why));
}

/// One of the broker's ways in.
struct Door {
    listener: UnixListener,
    path: PathBuf,
    far: bool,
}

impl Door {
    /// Binds `name`, where nothing is listening: this process holds the lock,
    /// so what is there was left by one that did not end cleanly.
    fn bind(state: &StateDir, name: &str, far: bool) -> Result<Self, String> {
        let path = state.root().join(name);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("{}: {e}", path.display())),
        }
        let listener = UnixListener::bind(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        listener
            .set_nonblocking(true)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Self {
            listener,
            path,
            far,
        })
    }

    /// Starts each connection's hello on a thread: one that says nothing must
    /// not stop the conversations here. How many it started.
    fn accept(&self, hellos: &mpsc::Sender<(bool, Result<Peer, String>)>) -> usize {
        let mut started = 0;
        while let Ok((stream, _)) = self.listener.accept() {
            let (hellos, far) = (hellos.clone(), self.far);
            let thread = std::thread::Builder::new()
                .name("td-agent-hello".into())
                .spawn(move || {
                    let _ = hellos.send((far, Peer::over(stream)));
                });
            match thread {
                Ok(_) => started += 1,
                Err(e) => logged!("td-agent: a thread for a connection: {e}"),
            }
        }
        started
    }

    fn close(self) {
        if let Err(e) = std::fs::remove_file(&self.path) {
            logged!("td-agent: {}: {e}", self.path.display());
        }
    }
}

/// The state directory, as the place the sockets are: this account's, and shut
/// to every other, so that a socket in it is the account's to reach whatever its
/// own mode. It is made so; one left open by hand is shut.
fn private_root(state: &StateDir) -> Result<(), String> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let root = state.root();
    let named = |e: std::io::Error| format!("{}: {e}", root.display());
    let me = std::fs::metadata("/proc/self").map_err(named)?.uid();
    let meta = std::fs::metadata(root).map_err(named)?;
    if meta.uid() != me {
        return Err(format!(
            "{} is another account's, so td-agent serve does not listen in it",
            root.display()
        ));
    }
    if meta.mode() & 0o077 != 0 {
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700)).map_err(named)?;
    }
    Ok(())
}

/// Whether `request` is of whichever conversation is open, naming none. Such a
/// request means the one the window was showing, which the broker may have left.
fn of_the_shown(request: &Request) -> bool {
    matches!(
        request,
        Request::Send(..)
            | Request::Retry
            | Request::Interrupt
            | Request::Pause(_)
            | Request::ClearTodo
            | Request::Compact(_)
            | Request::Review(_)
            | Request::Schedule(crate::schedule::Command::Add { .. })
            | Request::Command { .. }
            | Request::Choose { .. }
            | Request::Route { .. }
    )
}

impl Session {
    /// Serves until no window is attached and nothing is under way, or something
    /// dials `stop`.
    fn run(&mut self, doors: &[Door], stop: &Door) {
        let (hellos, said) = mpsc::channel();
        let mut window: Option<Attached> = None;
        // Connections still saying hello.
        let mut greeting = 0usize;
        // Far ones yet to say which they are.
        let mut claiming: Vec<(Peer, Instant)> = Vec::new();
        let mut last_needed = Instant::now();
        let mut next_look_at_log = Instant::now();
        loop {
            if stop.listener.accept().is_ok() {
                logged!("td-agent: asked to stop");
                if let Some(window) = window.take() {
                    self.view.detach();
                    goodbye(window.peer, "td-agent serve was stopped");
                }
                // Those yet to say which they are, or hello, are told too: they
                // would try again all the same, and start another.
                for door in doors {
                    greeting = greeting.saturating_add(door.accept(&hellos));
                }
                let deadline = Instant::now() + GREETING_AT_STOP;
                while greeting > 0 {
                    let left = deadline.saturating_duration_since(Instant::now());
                    let Ok((_, peer)) = said.recv_timeout(left) else {
                        break;
                    };
                    greeting -= 1;
                    claiming.extend(peer.ok().map(|peer| (peer, Instant::now())));
                }
                for (peer, _) in claiming {
                    goodbye(peer, "td-agent serve was stopped");
                }
                return;
            }
            if Instant::now() >= next_look_at_log {
                next_look_at_log = Instant::now() + LOOK_AT_LOG;
                crate::window::rotate_log(self.state.root());
            }
            for door in doors {
                greeting = greeting.saturating_add(door.accept(&hellos));
            }
            for (far, peer) in said.try_iter() {
                greeting = greeting.saturating_sub(1);
                match peer {
                    // A far one first says which it is.
                    Ok(peer) if far => claiming.push((peer, Instant::now())),
                    Ok(peer) => self.take_over(&mut window, peer, false, None),
                    Err(why) => logged!("td-agent: a window could not attach: {why}"),
                }
            }
            for (peer, since) in std::mem::take(&mut claiming) {
                match claim_of(&peer) {
                    Ok(None) if since.elapsed() < CLAIMING => claiming.push((peer, since)),
                    Ok(None) => logged!("td-agent: a window said nothing of itself"),
                    Err(why) => logged!("td-agent: a window could not attach: {why}"),
                    Ok(Some((token, asked))) => {
                        let another = window
                            .as_ref()
                            .is_some_and(|window| window.token.as_ref() != Some(&token));
                        // Trying again by itself, it does not put out a window in
                        // use: its own dead connection is another matter.
                        if another && !asked {
                            goodbye_aside(peer, "another window is open there");
                        } else {
                            self.take_over(&mut window, peer, true, Some(token));
                        }
                    }
                }
            }
            self.heard(&mut window);
            self.hear();
            self.stored();
            self.refresh();
            self.fire();
            if let Some(answer) = self.folders.answered() {
                self.view.folders(answer);
            }
            let views = self.view.take();
            if let Some(attached) = &window {
                let mut unsent = Vec::new();
                for view in views {
                    let encoded = || crate::window_wire::encode_view(&view);
                    if !unsent.is_empty() || !attached.peer.send(encoded()) {
                        unsent.push(view);
                    }
                }
                if !unsent.is_empty() {
                    logged!("td-agent: the window is not keeping up, so it is let go");
                    self.view.lost(unsent);
                    window = None;
                }
            }
            if window.is_some() || greeting > 0 || !claiming.is_empty() || !self.idle() {
                last_needed = Instant::now();
            } else if last_needed.elapsed() >= LINGER {
                // One that dials just now is closed unanswered, and tries again.
                return;
            }
            std::thread::sleep(if window.is_some() { BUSY_POLL } else { POLL });
        }
    }

    /// One window at a time: the newest is the person's.
    fn take_over(
        &mut self,
        window: &mut Option<Attached>,
        peer: Peer,
        far: bool,
        token: Option<String>,
    ) {
        // What the last had asked counts: it had cleared it.
        self.heard(window);
        if let Some(old) = window.take() {
            goodbye_aside(old.peer, "another window has taken over");
        }
        self.view.remember_told(far);
        *window = Some(Attached { peer, token });
        self.attached();
    }

    /// Carries out what the window asked, one at a time, each against what it
    /// left true, and notices the window going.
    fn heard(&mut self, window: &mut Option<Attached>) {
        let Some(attached) = window else {
            return;
        };
        let gone = loop {
            match attached.peer.recv() {
                Ok(Some(bytes)) => match crate::window_wire::decode_asked(&bytes) {
                    Ok((showing, request)) => {
                        self.asked_by_window(showing, request);
                    }
                    Err(why) => break Some(format!("it said something unreadable: {why}")),
                },
                Ok(None) => break None,
                Err(why) => break Some(why),
            }
        };
        if let Some(why) = gone {
            logged!("td-agent: the window has gone: {why}");
            self.view.detach();
            *window = None;
        }
    }

    /// One request of a window showing `of`.
    fn asked_by_window(&mut self, showing: Showing, request: Request) {
        let moved = match &showing {
            Showing::Nothing => self.view.active().is_some(),
            Showing::Conversation(of) => self.view.active() != Some(of),
            Showing::New(prior) => {
                let active = self.view.active();
                active != prior.as_ref() && active != self.made.as_ref()
            }
        };
        if moved && of_the_shown(&request) {
            const WHY: &str = "the open conversation changed before this reached it";
            return match request.as_typed() {
                Some((text, pictures)) => self.view.give_back(&text, pictures, WHY.to_string()),
                None => self.view.note(format!("{WHY}, so it was not done")),
            };
        }
        let makes_one = matches!(
            request,
            Request::New
                | Request::NewScratch(_)
                | Request::NewIn(..)
                | Request::NewFrom(_)
                | Request::Admit { .. }
        );
        let before = self.view.active().cloned();
        self.serve(vec![request]);
        if makes_one {
            // Refused, it made nothing: what is open was not its doing.
            let after = self.view.active().cloned();
            self.made = after.filter(|after| Some(after) != before.as_ref());
        }
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
/// none without them, unless `unconfined_without_jail` makes Empty and
/// Directory… workspaces unconfined instead, when the second is why
/// there is no jail.
fn places(config: &Config, state: &StateDir) -> (Result<Places, String>, Option<String>) {
    let made = |programs: &[&std::path::Path]| {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|home| home.is_absolute())
            .ok_or("HOME is not an absolute path, so no workspace can be admitted")?;
        Places::from_env(state.root(), &config.workspace_root(&home), programs)
    };
    match Programs::from_env() {
        Ok(programs) => (made(&programs.all()), None),
        Err(why) if config.unconfined_without_jail => match crate::jail::Unjailed::from_env() {
            Ok(unjailed) => (made(&unjailed.all()), Some(why)),
            Err(also) => (Err(format!("{why}; and unconfined: {also}")), None),
        },
        Err(why) => (Err(why), None),
    }
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
                unconfined: meta.unconfined,
                archived: meta.archived,
            }
        })
        .collect();
    (rows, problems)
}

/// A conversation's sub-agents as its log was read to have them.
type TasksRead = (Id, Result<Vec<store::TaskStart>, String>);

/// Runs the broker over the state directory until no window is attached and
/// nothing is under way: it takes the lock, opens the most recently active
/// conversation when there is one, and listens. `key` is the API key, or why
/// there is none, which every turn then says; `key_path` is where the key
/// dialog stores one.
pub fn run(
    config: Config,
    key: Result<Secret, String>,
    key_path: Option<PathBuf>,
    state: StateDir,
    program: PathBuf,
) -> Result<(), String> {
    state.ensure()?;
    let _lock = state.lock_window()?;
    private_root(&state)?;
    let doors = [
        Door::bind(&state, crate::window::WINDOW_SOCKET, false)?,
        Door::bind(&state, crate::window::ATTACH_SOCKET, true)?,
    ];
    let stop = Door::bind(&state, crate::window::STOP_SOCKET, false)?;
    let mut view = Port::new(state.clone());
    view.set_share(state.load_share());
    view.set_mode(config.mode);
    view.set_without_jev(!config.client.jev_required);
    if td_fetch_client::socket_path().is_none() {
        let (log, note) = no_fetch_service(std::env::var_os("XDG_RUNTIME_DIR").as_deref());
        logged!("td-agent: {log}");
        view.note(note);
    }
    for note in &config.notes {
        logged!("td-agent: configuration: {note}");
    }
    match config.notes.as_slice() {
        [] => {}
        [note] => view.note(format!("configuration: {note}")),
        more => view.note(format!(
            "{} configuration key(s) are not read or do nothing here; see serve.log",
            more.len()
        )),
    }
    if let Err(why) = &key {
        logged!("td-agent: {why}");
        view.note(why.clone());
    }
    view.set_keyed(key.is_ok());
    view.set_key_path(key_path.as_ref().map(|p| p.display().to_string()));
    let (rows, problems) = rows(&state);
    for problem in &problems {
        logged!("td-agent: the store: {problem}");
    }
    if let Some(problem) = problems.last() {
        view.note(format!("the store: {problem}"));
    }
    // The most recently active first, whose sub-agents are read first.
    let mut ids: Vec<(u64, Id)> = rows.iter().map(|r| (r.activity, r.id.clone())).collect();
    ids.sort_by(|a, b| b.cmp(a));
    view.set_rows(rows);
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
            logged!("td-agent: {said}");
            view.note(said);
        }
    }
    // The configuration's templates, then those made in the window.
    let saved = state.load_templates().or_else(|e| {
        let (said, saved) = match state.set_templates_aside() {
            Ok(to) => (
                format!(
                    "the templates made in the window are set aside, as {}: {e}",
                    to.display()
                ),
                Ok(Vec::new()),
            ),
            Err(moved) => (
                format!(
                    "the templates made in the window are not listed, and none can be saved until the file is mended: {e}; {moved}"
                ),
                Err(format!("the templates file cannot be read: {e}")),
            ),
        };
        logged!("td-agent: {said}");
        view.note(said);
        saved
    });
    let (templates, clashes) =
        merged_templates(&config.templates, saved.as_deref().unwrap_or_default());
    for clash in clashes {
        logged!("td-agent: {clash}");
        view.note(clash);
    }
    view.set_saved_templates(saved.as_deref().unwrap_or_default().to_vec());
    let mut client = config.client.clone();
    // Workspaces, and the shared directories each gets, admitted once
    // here; without the jail there are none, and asking says why. No
    // workspace may reach an admitted local repository.
    let (places, jailless) = places(&config, &state);
    let places = places.map(|places| crate::workspace::Places {
        locals: local_paths(&remotes),
        ..places
    });
    if let Some(why) = &jailless {
        let note = format!(
            "there is no jail, so a new Empty or Directory\u{2026} workspace is unconfined (unconfined_without_jail): {why}"
        );
        logged!("td-agent: {note}");
        view.note(note);
        view.set_jailless(Some(why.clone()));
    }
    match &places {
        Ok(places) => {
            let (shared, mut notes) = workspace::admit_shared(&config.shared(&places.home), places);
            // A template's own list, admitted as the top-level one is; one
            // that names none takes the top level's.
            for template in &templates {
                let (shared, refused) = own_shared(template, Some(places));
                notes.extend(
                    refused
                        .into_iter()
                        .map(|note| format!("template {:?}: {note}", template.name)),
                );
                client
                    .template_shared
                    .push(template_entry(template, shared));
            }
            for note in &notes {
                logged!("td-agent: {note}");
            }
            match notes.as_slice() {
                [] => {}
                [note] => view.note(note.clone()),
                more => view.note(format!(
                    "{} shared directories are not given to workspaces; see serve.log",
                    more.len()
                )),
            }
            client.shared = shared;
            // Every conversation process is handed the lists in its setup
            // frame: past this the templates' own go, and their
            // workspaces bind none, rather than no process starting.
            if client.to_json().to_string().len() > SETUP_CLIENT_BYTES {
                client.template_shared.clear();
                let note = "the templates' shared directories, network policies, MCP servers and runtimes are too many to hand to conversations; their workspaces bind none, reach no network, run no server and bind no runtime";
                logged!("td-agent: {note}");
                view.note(note);
            }
        }
        Err(why) => {
            logged!("td-agent: {why}");
            view.set_no_workspaces(Some(why.clone()));
        }
    }
    view.set_networks(&client);
    view.set_templates(
        templates
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
        logged!("td-agent: {said}");
        view.note(said);
        if let Err(e) = state.forget_default_model() {
            logged!("td-agent: the default model: {e}");
        }
    }
    view.set_models(&client.model, Vec::new());
    view.set_offers(Vec::new(), &client.reasoning_effort);
    view.set_default_routing(&client.routing);
    view.set_limits(client.limits);
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
            view.note(e);
            None
        }
    };
    let ledger = Ledger::load(Some(state.root()), client.limits.day, store::now());
    let (schedules, problem) = crate::schedule::Schedules::load(&state, td_civil::Zone::local());
    if let Some(problem) = problem {
        logged!("td-agent: the schedules: {problem}");
        view.note(format!("the schedules: {problem}"));
    }
    let (outbox, problems) = Outbox::load(&state);
    for problem in &problems {
        logged!("td-agent: the outbox: {problem}");
    }
    if let Some(problem) = problems.last() {
        view.note(format!("the outbox: {problem}"));
    }
    // The stores and workspace repositories live in the data directory;
    // the git worker's own files in the state directory.
    let data = workspace::data_dir();
    let git_dir = state.root().join("git");
    let mut session = Session {
        view,
        supervisor,
        state,
        ledger,
        fetcher,
        client,
        model_key,
        post: Post::new(outbox),
        key_path,
        export: None,
        tasks: None,
        keys: key.iter().cloned().collect(),
        places,
        jailless,
        templates,
        configured: config.templates.clone(),
        saved,
        remotes,
        stores: data.as_ref().ok().map(|data| {
            crate::git::Service::start(
                git_dir,
                data.join("store"),
                crate::git::kept_env(),
                crate::git::Prompt::here(),
            )
        }),
        data,
        removals: Vec::new(),
        schedules,
        next_fire: Some(0),
        commands: crate::config::path(
            std::env::var_os("XDG_CONFIG_HOME"),
            std::env::var_os("HOME"),
        )
        .and_then(|file| file.parent().map(|dir| dir.join(crate::commands::DIR))),
        skills: crate::skills::dir(),
        fetch_interval: config.fetch_interval(),
        next_refresh: Instant::now(),
        refreshing: Vec::new(),
        pushes: Vec::new(),
        heads: crate::upstream::Heads::default(),
        troubles: std::collections::BTreeMap::new(),
        mode: config.mode,
        made: None,
        folders: crate::chooser::FolderReader::default(),
        held: Vec::new(),
        prompt_troubles: Vec::new(),
    };
    session.read_tasks(ids.into_iter().map(|(_, id)| id).collect());
    // Before any conversation starts.
    session.repolicy();
    // What removals a crash cut short left (`removal::sweep`).
    if let Ok(data) = &session.data {
        let mut doomed = vec![data.join("ws"), data.join("publish")];
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
        Err(e) => session.view.note(format!("the models cache: {e}")),
    }
    if let Some(fetcher) = session.fetcher.as_mut() {
        let _ = fetcher.jobs.send(Job::Models(session.client.wanted()));
        fetcher.credit(Instant::now());
    }
    // Nothing is made here: with none yet, the human starts one.
    if let Some(id) = session.view.most_recent() {
        session.view.set_active(id.clone());
        session.open(id, None);
    }
    session.run(&doors, &stop);
    for door in doors {
        door.close();
    }
    // Last but for the lock: `--stop` waits for this to go.
    drop(session);
    stop.close();
    Ok(())
}

/// The templates the chooser lists (DESIGN.md §7): `configured`, then
/// those made in the window, `saved`, but for one named as a configured
/// one is, ASCII case aside, which is said and left out; at most
/// `config::MAX_TEMPLATES` of each.
fn merged_templates(configured: &[Template], saved: &[Template]) -> (Vec<Template>, Vec<String>) {
    let mut templates = configured.to_vec();
    let mut clashes = Vec::new();
    for template in saved {
        if templates
            .iter()
            .any(|t| t.name.eq_ignore_ascii_case(&template.name))
        {
            clashes.push(format!(
                "the template {:?} made in the window is not listed: the configuration names one so",
                template.name
            ));
        } else {
            templates.push(template.clone());
        }
    }
    (templates, clashes)
}

/// `saved` with `template` in place of the one named `replacing`, or
/// added: refused when a configured template, or another made in the
/// window, has its name, ASCII case aside, or when there would be more
/// than `config::MAX_TEMPLATES`.
fn with_template(
    configured: &[Template],
    saved: &[Template],
    template: Template,
    replacing: Option<&str>,
) -> Result<Vec<Template>, String> {
    let name = template.name.clone();
    if configured
        .iter()
        .any(|t| t.name.eq_ignore_ascii_case(&name))
    {
        return Err(format!(
            "the configuration names a template {name:?}; choose another name"
        ));
    }
    if saved
        .iter()
        .any(|t| t.name.eq_ignore_ascii_case(&name) && Some(t.name.as_str()) != replacing)
    {
        return Err(format!(
            "a template is named {name:?} already; choose another name"
        ));
    }
    let mut next = saved.to_vec();
    match replacing {
        Some(old) => match next.iter_mut().find(|t| t.name == old) {
            Some(slot) => *slot = template,
            None => return Err(format!("no template made in the window is named {old:?}")),
        },
        None if next.len() >= crate::config::MAX_TEMPLATES => {
            return Err(format!(
                "{} templates are made in the window already; remove one first",
                crate::config::MAX_TEMPLATES
            ))
        }
        None => next.push(template),
    }
    Ok(next)
}

/// `template`'s own shared directories admitted (DESIGN.md §8,
/// Admission), with why each refused one is, or none when it names none;
/// without `places`, where no workspace is made, an empty list, never
/// the top level's.
fn own_shared(
    template: &Template,
    places: Option<&Places>,
) -> (Option<Vec<crate::workspace::Shared>>, Vec<String>) {
    match (places, template.shared.is_some()) {
        (_, false) => (None, Vec::new()),
        (None, true) => (Some(Vec::new()), Vec::new()),
        (Some(places), true) => {
            let (admitted, refused) =
                workspace::admit_shared(&template.shared(&places.home).unwrap_or_default(), places);
            (Some(admitted), refused)
        }
    }
}

/// How many conversations' workspaces name the template `name`.
fn orphans(metas: &[crate::store::Meta], name: &str) -> usize {
    metas
        .iter()
        .filter(|meta| match &meta.workspace {
            Some(Workspace::Template(of)) => of == name,
            Some(Workspace::Repositories(repositories)) => repositories.template == name,
            _ => false,
        })
        .count()
}

/// What conversations are handed of `template`, `shared` its own shared
/// directories as admitted: its network policy, system view, MCP servers
/// and runtime (DESIGN.md §8, Runtime) as it names them.
fn template_entry(
    template: &Template,
    shared: Option<Vec<crate::workspace::Shared>>,
) -> TemplateShared {
    TemplateShared {
        system: template.system,
        name: template.name.clone(),
        shared,
        network: template.network,
        mcp: template.mcp.clone(),
        runtime: template.runtime.clone(),
    }
}

/// The conversations' template entries once the templates made in the
/// window change to those listed in `templates`: `admitted`'s template its
/// admitted list, every other its entry as `held`, never re-admitted, so a
/// configured template keeps its runtime; or none, binding nothing and
/// reaching no network, when start withheld the lists.
fn kept_template_shared(
    held: &[TemplateShared],
    configured: &[Template],
    templates: &[Template],
    admitted: Option<(&str, Option<Vec<crate::workspace::Shared>>)>,
) -> Vec<TemplateShared> {
    template_shared(held, configured, templates, |template| match &admitted {
        Some((name, own)) if *name == template.name => Some(template_entry(template, own.clone())),
        _ => held
            .iter()
            .find(|entry| entry.name == template.name)
            .cloned(),
    })
}

/// Each listed template's shared directories and network for the
/// conversations: a configured one's as `held` has them, one made in the
/// window's as `entry` gives it, or none, when its workspaces bind
/// nothing and reach no network.
fn template_shared(
    held: &[TemplateShared],
    configured: &[Template],
    listed: &[Template],
    entry: impl FnMut(&Template) -> Option<TemplateShared>,
) -> Vec<TemplateShared> {
    let mut shared: Vec<TemplateShared> = held
        .iter()
        .filter(|entry| configured.iter().any(|t| t.name == entry.name))
        .cloned()
        .collect();
    shared.extend(
        listed
            .iter()
            .filter(|template| !configured.iter().any(|t| t.name == template.name))
            .filter_map(entry),
    );
    shared
}

/// The local repositories `remotes` admit, each by its path.
fn local_paths(remotes: &[crate::git::Admission]) -> Vec<PathBuf> {
    remotes
        .iter()
        .filter_map(|admission| match admission {
            crate::git::Admission::Exact(remote) => remote.local_path(),
            crate::git::Admission::Prefix { .. } => None,
        })
        .collect()
}

/// The configured template `name` a new conversation starts from, or
/// why none may: none is named so.
fn chosen(templates: &[Template], name: &str) -> Result<Template, String> {
    templates
        .iter()
        .find(|t| t.name == name)
        .cloned()
        .ok_or_else(|| format!("no template is named {name:?}"))
}

/// Where a workspace reaches: a conversation's own directory, or a
/// shared directory, the top level's or a template's.
fn reached(metas: &[crate::store::Meta], client: &Client) -> Vec<PathBuf> {
    metas
        .iter()
        .filter_map(|meta| match &meta.workspace {
            Some(Workspace::Directory(path)) => Some(path.clone()),
            _ => None,
        })
        .chain(
            metas
                .iter()
                .flat_map(|meta| meta.directory_grants.iter())
                .map(|grant| grant.path.clone()),
        )
        .chain(
            metas
                .iter()
                .flat_map(|meta| meta.directory_exposures.iter().cloned()),
        )
        .chain(client.shared.iter().map(|shared| shared.path.clone()))
        .chain(
            client
                .template_shared
                .iter()
                .flat_map(|template| template.shared.iter().flatten())
                .map(|shared| shared.path.clone()),
        )
        .collect()
}

/// Whether `remote`, when a local repository, lies where no jail reaches
/// (DESIGN.md §7, Admitted remotes): none of `places`, nor overlapping
/// any of `reached`; a remote on another host always may be.
fn local_remote(
    remote: &crate::git::Remote,
    places: &Result<Places, String>,
    reached: &[PathBuf],
) -> Result<(), String> {
    let Some(path) = remote.local_path() else {
        return Ok(());
    };
    let places = places.as_ref().map_err(Clone::clone)?;
    crate::workspace::local_repository(&path, places, reached)
        .map_err(|why| format!("the local repository {}: {why}", path.display()))
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

/// Allows all calls in `conversation` in the human's rules, or stops.
fn set_all_allowed(state: &StateDir, conversation: &Id, on: bool) -> Result<(), String> {
    let text = crate::rules::set_all_allowed(&state.load_rules()?, conversation.as_str(), on)?;
    state.save_rules(&text)
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
        as_staged, default_model, ended_pushes, forget_rules, held, local_remote, merged_templates,
        names, orphans, own_shared, pushing_entry, reached, refetched, remember, remember_crossing,
        set_mode, template_shared, with_template, Client, TemplateShared,
    };
    use super::{kept_template_shared, template_entry};
    use crate::protocol::Down;
    use crate::workspace::Workspace;
    use std::path::PathBuf;

    /// "Yet" only where one could still appear.
    #[test]
    fn a_missing_fetch_service_is_called_what_it_is() {
        let (log, note) = super::no_fetch_service(Some(std::ffi::OsStr::new("/run/user/7")));
        assert!(log.contains("/run/user/7 yet"), "{log}");
        assert!(note.contains("yet"), "{note}");
        for unset in [None, Some(std::ffi::OsStr::new(""))] {
            let (log, note) = super::no_fetch_service(unset);
            assert!(log.contains("XDG_RUNTIME_DIR is not set"), "{log}");
            assert!(
                !log.contains("yet") && !note.contains("yet"),
                "{log} {note}"
            );
        }
    }

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

    /// A template saved in the window is refused a configured one's name,
    /// or another's made in the window, case aside, but for its own when
    /// edited; it takes the place of the one edited, renamed or not; and
    /// there are at most `MAX_TEMPLATES`. Each listed template's shared
    /// directories follow: a configured one's kept, one made in the
    /// window the top level's, one removed none.
    #[test]
    fn a_template_saved_in_the_window_is_named_once_and_shares_the_top_level() {
        let repo = crate::config::checked_repo("/srv/td", "main", "a", None).unwrap();
        let template = |name: &str| crate::config::Template {
            mcp: Vec::new(),
            system: false,
            network: None,
            name: name.into(),
            repos: vec![repo.clone()],
            shared: None,
            runtime: None,
        };
        let configured = [template("td")];
        let saved = [template("notes"), template("mail")];
        let names = |list: &[crate::config::Template]| -> Vec<String> {
            list.iter().map(|t| t.name.clone()).collect()
        };
        assert_eq!(
            names(&with_template(&configured, &saved, template("own"), None).unwrap()),
            ["notes", "mail", "own"]
        );
        for (name, replacing, why) in [
            ("TD", None, "the configuration"),
            ("Notes", None, "already"),
            ("mail", Some("notes"), "already"),
            ("own", Some("gone"), "no template"),
        ] {
            let e = with_template(&configured, &saved, template(name), replacing).unwrap_err();
            assert!(e.contains(why), "{name}: {e}");
        }
        // Edited in place, its own name kept or changed, case and all.
        assert_eq!(
            names(&with_template(&configured, &saved, template("Notes"), Some("notes")).unwrap()),
            ["Notes", "mail"]
        );
        assert_eq!(
            names(&with_template(&configured, &saved, template("work"), Some("notes")).unwrap()),
            ["work", "mail"]
        );
        let full: Vec<_> = (0..crate::config::MAX_TEMPLATES)
            .map(|n| template(&format!("t{n}")))
            .collect();
        assert!(with_template(&configured, &full, template("one"), None)
            .unwrap_err()
            .contains("remove one"));
        with_template(&configured, &full, template("one"), Some("t0")).unwrap();

        let own = Some(Vec::new());
        let held = vec![
            TemplateShared {
                mcp: Vec::new(),
                system: false,
                name: "td".into(),
                shared: own.clone(),
                network: None,
                runtime: None,
            },
            TemplateShared {
                mcp: Vec::new(),
                system: false,
                name: "notes".into(),
                shared: None,
                network: None,
                runtime: None,
            },
        ];
        let mut own_folder = template("own");
        own_folder.shared = Some(Vec::new());
        let listed = [template("td"), template("mail"), own_folder];
        let listed = [listed.as_slice(), &[template("gone")]].concat();
        let shared = template_shared(&held, &configured, &listed, |t| {
            (t.name != "gone").then(|| TemplateShared {
                mcp: Vec::new(),
                system: t.system,
                name: t.name.clone(),
                shared: t.shared.as_ref().map(|_| {
                    vec![crate::workspace::Shared {
                        path: "/admitted".into(),
                        write: false,
                    }]
                }),
                network: t.network,
                runtime: None,
            })
        });
        let entries: Vec<(&str, Option<usize>)> = shared
            .iter()
            .map(|entry| (entry.name.as_str(), entry.shared.as_ref().map(Vec::len)))
            .collect();
        assert_eq!(entries, [("td", Some(0)), ("mail", None), ("own", Some(1))]);
    }

    /// A configured template's runtime reaches the entry conversations are
    /// handed at start, and saving a template made in the window keeps
    /// it, as held, while the saved one, which names none, has none
    /// (DESIGN.md §8, Runtime).
    #[test]
    fn a_configured_templates_runtime_is_handed_on_and_kept() {
        let template = |name: &str, runtime: Option<&str>| crate::config::Template {
            mcp: Vec::new(),
            system: false,
            network: None,
            name: name.into(),
            repos: Vec::new(),
            shared: None,
            runtime: runtime.map(str::to_string),
        };
        let sdk = template("sdk", Some("freedesktop-sdk-25-08"));
        let entry = template_entry(&sdk, None);
        assert_eq!(entry.runtime.as_deref(), Some("freedesktop-sdk-25-08"));
        assert_eq!(entry.name, "sdk");
        assert_eq!(template_entry(&template("plain", None), None).runtime, None);
        let configured = [sdk.clone()];
        let held = vec![entry.clone()];
        let saved = template("notes", None);
        let listed = [sdk, saved];
        let own = Some(vec![crate::workspace::Shared {
            path: "/admitted".into(),
            write: false,
        }]);
        let kept = kept_template_shared(&held, &configured, &listed, Some(("notes", own.clone())));
        let runtimes: Vec<(&str, Option<&str>)> = kept
            .iter()
            .map(|entry| (entry.name.as_str(), entry.runtime.as_deref()))
            .collect();
        assert_eq!(
            runtimes,
            [("sdk", Some("freedesktop-sdk-25-08")), ("notes", None)]
        );
        assert_eq!(kept.last().unwrap().shared, own);
        // Removing the saved one keeps the configured one as held.
        assert_eq!(
            kept_template_shared(&held, &configured, &listed[..1], None),
            held
        );
    }

    /// The chooser lists the configuration's templates, then those made
    /// in the window, but for one named as a configured one is.
    #[test]
    fn templates_made_in_the_window_follow_the_configurations() {
        let template = |name: &str| crate::config::Template {
            mcp: Vec::new(),
            system: false,
            network: None,
            name: name.into(),
            repos: Vec::new(),
            shared: None,
            runtime: None,
        };
        let (merged, clashes) = merged_templates(
            &[template("td"), template("notes")],
            &[template("TD"), template("own")],
        );
        let names: Vec<&str> = merged.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["td", "notes", "own"]);
        assert_eq!(clashes.len(), 1);
        assert!(clashes.first().unwrap().contains("\"TD\""), "{clashes:?}");
    }

    /// A template's own shared folders are admitted as the configured
    /// ones are: one that resolves is bound at its real path, one that
    /// does not is said and left out, and one whose every folder is
    /// refused binds nothing, never the top level's; one naming none
    /// takes the top level's, and without places nothing is admitted.
    #[test]
    fn a_templates_own_folders_are_admitted_and_never_widen() {
        let scratch = crate::store::tests::Scratch::new("own-shared");
        let root = std::fs::canonicalize(scratch.state().root()).unwrap();
        let home = root.join("home");
        std::fs::create_dir_all(home.join("notes")).unwrap();
        let places = crate::workspace::Places {
            home: home.clone(),
            state: root.join("state"),
            root: home.join("td-agent"),
            ..crate::workspace::Places::default()
        };
        let shared = |path: &str, write: bool| crate::workspace::Shared {
            path: path.into(),
            write,
        };
        let template = |own: Option<Vec<crate::workspace::Shared>>| crate::config::Template {
            mcp: Vec::new(),
            system: false,
            network: None,
            name: "own".into(),
            repos: Vec::new(),
            shared: own,
            runtime: None,
        };
        let (admitted, refused) = own_shared(
            &template(Some(vec![shared("~/notes", true), shared("~/gone", false)])),
            Some(&places),
        );
        assert_eq!(
            admitted,
            Some(vec![crate::workspace::Shared {
                path: home.join("notes"),
                write: true
            }])
        );
        assert_eq!(refused.len(), 1, "{refused:?}");
        assert!(refused.first().unwrap().contains("gone"), "{refused:?}");
        let (admitted, refused) = own_shared(
            &template(Some(vec![shared("~/gone", false)])),
            Some(&places),
        );
        assert_eq!((admitted, refused.len()), (Some(Vec::new()), 1));
        assert_eq!(
            own_shared(&template(None), Some(&places)),
            (None, Vec::new())
        );
        assert_eq!(
            own_shared(&template(Some(vec![shared("~/notes", false)])), None),
            (Some(Vec::new()), Vec::new())
        );
    }

    /// The template a new conversation starts from, as `start_from`
    /// finds it, whether or not it names MCP servers; a name no template
    /// has is refused.
    #[test]
    fn a_template_is_chosen_by_its_name() {
        let mut template = crate::config::Template {
            name: "tools".into(),
            repos: Vec::new(),
            shared: None,
            network: None,
            system: false,
            mcp: Vec::new(),
            runtime: None,
        };
        let chosen = |templates: &[crate::config::Template], name: &str| {
            crate::broker::chosen(templates, name)
        };
        let plain = template.clone();
        assert_eq!(
            chosen(std::slice::from_ref(&plain), "tools"),
            Ok(plain.clone())
        );
        assert!(chosen(&[plain], "other")
            .unwrap_err()
            .contains("no template is named"));
        template.mcp =
            vec![crate::config::mcp_server("gh", vec!["gh-mcp".into()], None, None).unwrap()];
        assert_eq!(
            chosen(std::slice::from_ref(&template), "tools"),
            Ok(template.clone())
        );
    }

    /// Conversations whose workspace names a template, by the exact name
    /// it is found by, are counted; others are not.
    #[test]
    fn orphaned_workspaces_are_counted_by_their_templates_name() {
        let meta = |workspace: Option<Workspace>| crate::store::Meta {
            id: crate::store::Id::random().unwrap(),
            role: crate::store::Role::Conversation,
            title: String::new(),
            created: 0,
            paused: false,
            model: None,
            effort: None,
            routing: None,
            workspace,
            archived: false,
            prepared: Vec::new(),
            directory_grants: Vec::new(),
            directory_exposures: Vec::new(),
            removed: false,
            unconfined: false,
            tracked: Vec::new(),
        };
        let metas = [
            meta(Some(Workspace::Template("Notes".into()))),
            meta(Some(Workspace::Template("other".into()))),
            meta(Some(Workspace::Scratch)),
            meta(None),
        ];
        assert_eq!(orphans(&metas, "Notes"), 1);
        assert_eq!(orphans(&metas, "notes"), 0);
        assert_eq!(orphans(&metas, "td"), 0);
    }

    /// A local remote is fetched and pushed only where no workspace
    /// reaches: a directory workspace or a shared directory, the top
    /// level's or a template's, over it refuses it, as no places do; a
    /// remote on another host is never asked.
    #[test]
    fn a_local_remote_lies_where_no_workspace_reaches() {
        let scratch = crate::store::tests::Scratch::new("local-remote");
        let root = std::fs::canonicalize(scratch.state().root()).unwrap();
        let repo = root.join("srv/td.git");
        for dir in ["objects", "refs"] {
            std::fs::create_dir_all(repo.join(dir)).unwrap();
        }
        std::fs::write(repo.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let remote = crate::git::Remote::parse(repo.to_str().unwrap()).unwrap();
        let places: Result<crate::workspace::Places, String> = Ok(crate::workspace::Places {
            home: root.join("home"),
            state: root.join("state"),
            root: root.join("home/td-agent"),
            ..crate::workspace::Places::default()
        });
        let client = Client::default();
        local_remote(&remote, &places, &reached(&[], &client)).unwrap();
        let meta = crate::store::Meta {
            routing: None,
            id: crate::store::Id::random().unwrap(),
            role: crate::store::Role::Conversation,
            title: String::new(),
            created: 0,
            paused: false,
            model: None,
            effort: None,
            workspace: Some(Workspace::Directory(root.join("srv"))),
            archived: false,
            prepared: Vec::new(),
            directory_grants: Vec::new(),
            directory_exposures: Vec::new(),
            removed: false,
            unconfined: false,
            tracked: Vec::new(),
        };
        let e = local_remote(&remote, &places, &reached(&[meta], &client)).unwrap_err();
        assert!(e.contains("overlaps"), "{e}");
        let shared = |path: PathBuf| crate::workspace::Shared { path, write: false };
        let top = Client {
            shared: vec![shared(root.join("srv"))],
            ..Client::default()
        };
        assert!(local_remote(&remote, &places, &reached(&[], &top)).is_err());
        let templated = Client {
            template_shared: vec![crate::config::TemplateShared {
                mcp: Vec::new(),
                system: false,
                name: "t".into(),
                shared: Some(vec![shared(repo.join("refs"))]),
                network: None,
                runtime: None,
            }],
            ..Client::default()
        };
        assert!(local_remote(&remote, &places, &reached(&[], &templated)).is_err());
        let none: Result<crate::workspace::Places, String> = Err("no places".into());
        assert!(local_remote(&remote, &none, &[]).is_err());
        let https = crate::git::Remote::parse("https://example.org/a/td").unwrap();
        local_remote(&https, &none, std::slice::from_ref(&root)).unwrap();
    }

    #[test]
    fn a_conversation_asks_only_what_its_record_names() {
        let template = crate::config::Template {
            mcp: Vec::new(),
            system: false,
            network: None,
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
            runtime: None,
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
            asks: false,
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

//! What the session tells the window, as messages (DESIGN.md §2). `Port` is
//! the session's end: every change to what the window shows is a `View`, and
//! what the session reads of the conversations it answers from its own `Book`.
//! The window's end is `App::apply`, and what it asks comes back as `Request`s,
//! so neither side holds the other.

use std::collections::BTreeMap;

use crate::book::{Book, Row};
use crate::config::{Client, Mode, Template};
use crate::post::Entry;
use crate::store::{Event, Id, Kind, StateDir, TaskStart};
use crate::supervisor::Update;
use crate::ui::{App, Asked, Attached, Card, Request};

/// The pictures a message shows, by digest: each one's bytes as its
/// conversation keeps them, or why not.
pub type Pictures = BTreeMap<String, Result<Vec<u8>, String>>;

/// One change to what the window shows.
#[derive(Clone, Debug, PartialEq)]
pub enum View {
    /// The session took the app's opening of a row: everything before this
    /// was said without it, so the app takes this book for its own.
    Opened(Box<Book>),
    /// The list and transcript's share of the window, as the person left it.
    SetShare(Option<(u32, u32)>),
    /// The configuration's mode.
    SetMode(Mode),
    /// What a conversation's log has said of its processes and sub-agents, and
    /// the sub-agents' calls that have ended, for a window that has just
    /// attached and has not seen that log.
    Counted(Id, Vec<Event>, Vec<u64>),
    Note(String),
    Withdraw(Id, Option<u64>),
    SetActive(Id),
    /// Of the conversation named, the one shown when the session said it.
    Update(Option<Id>, Update, u64, Pictures),
    Background(Id, Update, u64),
    Replay(Option<Id>, Result<String, String>, Vec<Event>, Pictures),
    GiveBack(String, Vec<Attached>, String),
    Restore(String, String),
    PromptSaved(u64, String),
    PromptRefused(u64, String),
    TemplateSaved(String),
    TemplateRefused(String),
    SetTemplates(Vec<(String, bool)>),
    SetSavedTemplates(Vec<Template>),
    SetQueued(Vec<String>),
    SetOffers(Vec<crate::picker::Offer>, String),
    SetNetworks(Box<Client>),
    SetModes(Option<Vec<(String, Mode)>>),
    SetAllAllowed(Vec<String>),
    SetPrompts(Vec<crate::prompts::Entry>),
    SetModels(String, Vec<(String, u64)>),
    SetCredit(Option<String>),
    SetClosing(Id, Option<&'static str>),
    KeySaved(String),
    KeyExists,
    KeyRefused(String),
    SetKeyPath(Option<String>),
    SetKeyed(bool),
    AskRemoval(Id, String, Vec<String>, bool),
    Ask(Card),
    Question(Asked),
    AskAdmission(String, Vec<String>),
    ShowWorkspace(Id, Box<crate::card::Record>),
    ReshowWorkspace(Id, Box<crate::card::Record>),
    ShowSkills(crate::skills::Found),
    ShowSchedules(Vec<(String, String)>),
    ShowCommands(Vec<(String, String)>, usize),
    ShowOutput(Id, u64, Result<crate::output::Output, String>),
    ShowDebug(Id, u64, Result<Vec<(String, String)>, String>),
    SetWithoutJev(bool),
    SetToday(u64),
    SetSubAgents(Vec<(Id, Vec<TaskStart>)>),
    SetRows(Vec<Row>),
    AddRow(Row),
    RemoveRow(Id),
    SetArchived(Id, bool),
    SetNoWorkspaces(Option<String>),
    SetJailless(Option<String>),
    SetLimits(crate::cost::Limits),
    SetDefaultRouting(String),
    SetDefaultModel(String),
    OpenNewTemplate(Option<String>),
    FocusComposer,
    /// What came of a `Request::ListFolders`: for the window that asked, so
    /// not kept for another.
    Folders(crate::chooser::FoldersAnswer),
    /// The broker is letting this window go, on purpose, and says why: not a
    /// failed link, for a far window to come back from unasked.
    Bye(String),
}

/// The session's end: what it tells the window, kept in order until the
/// window takes it, and its own copy of what those messages say of the
/// conversations.
pub struct Port {
    book: Book,
    /// Where each conversation keeps its pictures.
    state: StateDir,
    /// Whether a window is there to tell.
    attached: bool,
    out: Vec<View>,
    /// The last of each setting said, in the order said, and what is still
    /// asked of the person: what a window that attaches is told at once.
    settings: Vec<View>,
    asked: Vec<View>,
    /// What each conversation has said of its processes and sub-agents.
    counted: std::collections::BTreeMap<Id, Counted>,
    /// The last event each conversation's process read back from its log when it
    /// started, which is not news.
    replaying: std::collections::BTreeMap<Id, u64>,
    /// What was said for a person who was not there, to tell them when one is.
    later: Vec<View>,
    /// The last open was of what has closed.
    remember_told: bool,
    /// Given back to a far window and not yet said to have arrived.
    told: Vec<View>,
    refused_open: bool,
}

/// How many notes wait for a window; the oldest go first.
const LATER_NOTES: usize = 256;

/// What a window needs of a log it has not seen to list its conversation.
#[derive(Default)]
struct Counted {
    events: Vec<Event>,
    ended: Vec<u64>,
}

impl Counted {
    /// As `App::heard_process` takes them.
    fn heard(&mut self, event: &Event) {
        match &event.kind {
            Kind::Process { .. } => self.events.push(event.clone()),
            Kind::Task { .. } => self.events.push(event.clone()),
            Kind::Ended { number, .. } => self.events.retain(
                |kept| !matches!(&kept.kind, Kind::Process { number: of, .. } if of == number),
            ),
            Kind::ToolResult { call, .. } => {
                let of_a_task = self
                    .events
                    .iter()
                    .any(|kept| matches!(&kept.kind, Kind::Task { call: of, .. } if of == call));
                if of_a_task {
                    self.ended.push(*call);
                }
            }
            _ => {}
        }
    }

    /// Its process has gone, and its background processes with it. One started
    /// again says its whole log: `Port::count` starts afresh for that.
    fn process_gone(&mut self) {
        self.events
            .retain(|kept| !matches!(kept.kind, Kind::Process { .. }));
    }
}

impl Port {
    pub fn new(state: StateDir) -> Self {
        Self {
            book: Book::default(),
            state,
            attached: false,
            out: Vec::new(),
            settings: Vec::new(),
            asked: Vec::new(),
            counted: std::collections::BTreeMap::new(),
            replaying: std::collections::BTreeMap::new(),
            later: Vec::new(),
            remember_told: false,
            told: Vec::new(),
            refused_open: false,
        }
    }

    /// A window has attached: it is told all there is to know. What it is shown
    /// of the open conversation comes after, as the log is read again.
    pub fn attach(&mut self) {
        // What the last window's last requests brought is the person's still.
        for view in std::mem::take(&mut self.out) {
            self.keep(view);
        }
        self.told_may_not_have_arrived();
        self.attached = true;
        self.out.push(View::Opened(Box::new(self.book.clone())));
        self.out.extend(self.settings.iter().cloned());
        self.out.extend(self.asked.iter().cloned());
        for (id, counted) in &self.counted {
            self.out.push(View::Counted(
                id.clone(),
                counted.events.clone(),
                counted.ended.clone(),
            ));
        }
        self.out.append(&mut self.later);
    }

    /// The window has gone: what is said is kept as far as `attach` needs it.
    pub fn detach(&mut self) {
        self.attached = false;
        self.told_may_not_have_arrived();
        for view in std::mem::take(&mut self.out) {
            self.keep(view);
        }
    }

    /// The window has gone with these unsent.
    pub fn lost(&mut self, views: Vec<View>) {
        self.told.retain(|view| !views.contains(view));
        self.detach();
        for view in views {
            self.keep(view);
        }
    }

    /// What must reach the person is kept for when one is there.
    fn keep(&mut self, view: View) {
        match &view {
            View::GiveBack(..) | View::Restore(..) => {}
            View::Note(_) => {
                let notes = self
                    .later
                    .iter()
                    .filter(|kept| matches!(kept, View::Note(_)))
                    .count();
                if notes >= LATER_NOTES {
                    if let Some(oldest) = self
                        .later
                        .iter()
                        .position(|kept| matches!(kept, View::Note(_)))
                    {
                        self.later.remove(oldest);
                    }
                }
            }
            _ => return,
        }
        self.later.push(view);
    }

    /// Whether a message given back waits for a person to take it.
    pub fn given_back_waits(&self) -> bool {
        self.later
            .iter()
            .any(|view| matches!(view, View::GiveBack(..) | View::Restore(..)))
    }

    /// The split the window saved: it knows, and the next is to be told.
    pub fn saved_share(&mut self, share: Option<(u32, u32)>) {
        self.settings
            .retain(|said| !matches!(said, View::SetShare(_)));
        self.settings.push(View::SetShare(share));
    }

    /// Whether the last open was of what has closed, asked once.
    pub fn take_refused_open(&mut self) -> bool {
        std::mem::take(&mut self.refused_open)
    }

    /// Conversation `id`'s process ended, saying nothing more, as it is archived
    /// or closed.
    fn gone_with_them(&mut self, id: &Id) {
        if let Some(counted) = self.counted.get_mut(id) {
            counted.process_gone();
        }
    }

    /// Whether a conversation may open.
    pub fn opens(&self, id: &Id) -> bool {
        self.book.opens(id)
    }

    /// What `update` says of `id`'s processes and sub-agents.
    fn count(&mut self, id: &Id, update: &Update) {
        match update {
            Update::Up(crate::protocol::Up::Event(event)) => {
                self.counted.entry(id.clone()).or_default().heard(event);
            }
            Update::Up(crate::protocol::Up::Hello { last, .. }) => {
                self.counted.remove(id);
                self.replaying.insert(id.clone(), *last);
            }
            Update::Failed { .. } => {
                if let Some(counted) = self.counted.get_mut(id) {
                    counted.process_gone();
                }
            }
            _ => {}
        }
    }

    fn say(&mut self, view: View) {
        match &view {
            View::SetShare(_)
            | View::SetMode(_)
            | View::SetTemplates(_)
            | View::SetSavedTemplates(_)
            | View::SetQueued(_)
            | View::SetOffers(..)
            | View::SetNetworks(_)
            | View::SetModes(_)
            | View::SetAllAllowed(_)
            | View::SetPrompts(_)
            | View::SetModels(..)
            | View::SetCredit(_)
            | View::SetKeyPath(_)
            | View::SetKeyed(_)
            | View::SetWithoutJev(_)
            | View::SetToday(_)
            | View::SetNoWorkspaces(_)
            | View::SetJailless(_)
            | View::SetLimits(_)
            | View::SetDefaultRouting(_)
            | View::SetDefaultModel(_) => {
                // Said every turn, most of these, and worth saying when changed:
                // a window at the far end of ssh pays for each.
                if self.settings.contains(&view) {
                    return;
                }
                let kind = std::mem::discriminant(&view);
                self.settings
                    .retain(|said| std::mem::discriminant(said) != kind);
                self.settings.push(view.clone());
            }
            // Each says more than the last; the window keeps what it has.
            View::SetSubAgents(_) => {
                if self.settings.contains(&view) {
                    return;
                }
                self.settings.push(view.clone());
            }
            View::Ask(_) | View::Question(_) | View::AskRemoval(..) => {
                self.asked.push(view.clone())
            }
            View::Withdraw(conversation, call) => {
                self.asked.retain(|asked| match asked {
                    View::Ask(card) => {
                        &card.conversation != conversation
                            || call.is_some_and(|call| call != card.call)
                    }
                    View::Question(question) => {
                        &question.conversation != conversation
                            || call.is_some_and(|call| call != question.call)
                    }
                    _ => true,
                });
            }
            _ => {}
        }
        if self.attached {
            self.out.push(view);
        } else {
            self.keep(view);
        }
    }

    /// A card, or a question about a removal or an admission, is answered.
    fn answered(&mut self, request: &Request) {
        self.asked.retain(|asked| match (request, asked) {
            (
                Request::Decide {
                    conversation, call, ..
                },
                View::Ask(card),
            ) => &card.conversation != conversation || card.call != *call,
            (
                Request::Resume {
                    conversation, turn, ..
                },
                View::Ask(card),
            ) => &card.conversation != conversation || card.call != *turn,
            (Request::Remove { id, .. }, View::AskRemoval(of, ..)) => of != id,
            _ => true,
        });
    }

    /// What is waiting for the window, in order.
    pub fn take(&mut self) -> Vec<View> {
        let out = std::mem::take(&mut self.out);
        if self.remember_told {
            let given_back = |view: &&View| matches!(view, View::GiveBack(..) | View::Restore(..));
            self.told.extend(out.iter().filter(given_back).cloned());
        }
        out
    }

    /// Whether what is given back to this window is kept until it is heard from:
    /// a far one may have lost its link without either end yet knowing.
    pub fn remember_told(&mut self, remember: bool) {
        self.remember_told = remember;
    }

    /// The far window has what was given back longest ago.
    pub fn received(&mut self) {
        if !self.told.is_empty() {
            self.told.remove(0);
        }
    }

    fn told_may_not_have_arrived(&mut self) {
        for view in std::mem::take(&mut self.told) {
            self.keep(view);
        }
    }

    /// What the window asks changes what it shows before it is served, so
    /// the book follows first: a row it opened stands for every request of
    /// the batch, whichever came first, as it did when the session read the
    /// window's own state. What was on its way to the window meanwhile was
    /// said without that, so the window is sent this book to take.
    pub fn asked(&mut self, requests: &[Request]) {
        for request in requests {
            self.answered(request);
            if let Request::Open(id) = request {
                // It may have closed before the window heard: the answer says.
                // A sub-agent may be there before the list has heard of it.
                let listed = self.book.rows.iter().any(|row| &row.id == id);
                if self.book.opens(id) || (!listed && self.state.conversation(id).is_dir()) {
                    self.book.set_active(id.clone());
                } else {
                    self.refused_open = true;
                }
                // Until it hears this the window ignores what is queued, so it
                // is said again, even if it is the same.
                self.settings
                    .retain(|said| !matches!(said, View::SetQueued(_)));
                self.say(View::Opened(Box::new(self.book.clone())));
            }
        }
    }

    pub fn active(&self) -> Option<&Id> {
        self.book.active()
    }

    pub fn directory(&self) -> Vec<Entry> {
        self.book.directory()
    }

    pub fn most_recent(&self) -> Option<Id> {
        self.book.most_recent()
    }

    pub fn named(&self, id: &Id) -> String {
        self.book.named(id)
    }

    /// Conversation `id`'s title, whole.
    pub fn title(&self, id: &Id) -> String {
        self.book
            .rows
            .iter()
            .find(|r| &r.id == id)
            .map(|r| r.title.clone())
            .unwrap_or_default()
    }

    /// The bytes of the pictures `events` show, read from where the open
    /// conversation keeps them.
    fn pictures<'a>(&self, events: impl Iterator<Item = &'a Event>) -> Pictures {
        let mut read = Pictures::new();
        let Some(id) = self.book.active() else {
            return read;
        };
        let dir = self.state.conversation(id);
        for event in events {
            let references = match &event.kind {
                Kind::User { pictures, .. } | Kind::ToolResult { pictures, .. } => pictures,
                _ => continue,
            };
            for reference in references {
                read.entry(reference.digest.clone())
                    .or_insert_with(|| crate::picture::read(&dir, reference));
            }
        }
        read
    }

    pub fn set_mode(&mut self, mode: Mode) {
        self.say(View::SetMode(mode));
    }

    pub fn set_share(&mut self, share: Option<(u32, u32)>) {
        self.say(View::SetShare(share));
    }

    pub fn note(&mut self, message: impl Into<String>) {
        self.say(View::Note(message.into()));
    }

    pub fn withdraw(&mut self, conversation: &Id, call: Option<u64>) {
        self.say(View::Withdraw(conversation.clone(), call));
    }

    pub fn set_active(&mut self, id: Id) {
        self.book.set_active(id.clone());
        self.say(View::SetActive(id));
    }

    pub fn update(&mut self, update: Update, now: u64) {
        let pictures = match &update {
            Update::Up(crate::protocol::Up::Event(event)) => self.pictures(std::iter::once(event)),
            _ => Pictures::new(),
        };
        self.book.update(&update, now);
        let of = self.book.active().cloned();
        if let Some(of) = &of {
            self.count(of, &update);
        }
        self.say(View::Update(of, update, now, pictures));
    }

    pub fn background(&mut self, id: &Id, update: &Update, now: u64) {
        self.book.background(id, update, now);
        self.count(id, update);
        // The window makes a note of what td-agent tells the person there.
        if !self.attached {
            let read_back = self.replaying.get(id).copied().unwrap_or(0);
            if let Update::Up(crate::protocol::Up::Event(Event {
                seq,
                kind: Kind::Notice { text } | Kind::Notification { text, .. },
                ..
            })) = update
            {
                if *seq <= read_back {
                    return self.say(View::Background(id.clone(), update.clone(), now));
                }
                let title = self
                    .book
                    .rows
                    .iter()
                    .find(|row| &row.id == id)
                    .map_or(id.as_str(), |row| row.title.as_str());
                let said = format!("{title}: {text}");
                self.keep(View::Note(said));
            }
        }
        self.say(View::Background(id.clone(), update.clone(), now));
    }

    pub fn replay(&mut self, prefix: Result<&str, &str>, events: Vec<Event>) {
        let pictures = self.pictures(events.iter());
        self.book.replay(&events);
        let prefix = prefix.map(str::to_string).map_err(str::to_string);
        let of = self.book.active().cloned();
        if let Some(of) = &of {
            // The whole log, so what was counted is counted afresh.
            let counted = self.counted.entry(of.clone()).or_default();
            *counted = Counted::default();
            for event in &events {
                counted.heard(event);
            }
        }
        self.say(View::Replay(of, prefix, events, pictures));
    }

    /// Said whole to the log too: it may never reach a window, one that takes it
    /// and goes before showing it among them.
    pub fn give_back(&mut self, text: &str, pictures: Vec<Attached>, why: String) {
        logged!("td-agent: {why}; a message given back:\n{text}");
        self.say(View::GiveBack(text.into(), pictures, why));
    }

    /// Said whole to the log too, as `give_back` is.
    pub fn restore(&mut self, text: &str, why: String) {
        logged!("td-agent: {why}; a message given back:\n{text}");
        self.say(View::Restore(text.into(), why));
    }

    pub fn prompt_saved(&mut self, ask: u64, said: String) {
        self.say(View::PromptSaved(ask, said));
    }

    pub fn prompt_refused(&mut self, ask: u64, why: String) {
        self.say(View::PromptRefused(ask, why));
    }

    pub fn template_saved(&mut self, said: String) {
        self.say(View::TemplateSaved(said));
    }

    pub fn template_refused(&mut self, why: String) {
        self.say(View::TemplateRefused(why));
    }

    pub fn set_templates(&mut self, templates: Vec<(String, bool)>) {
        self.say(View::SetTemplates(templates));
    }

    pub fn set_saved_templates(&mut self, templates: Vec<Template>) {
        self.say(View::SetSavedTemplates(templates));
    }

    pub fn set_queued(&mut self, queued: Vec<String>) {
        self.say(View::SetQueued(queued));
    }

    pub fn set_offers(&mut self, offers: Vec<crate::picker::Offer>, effort: &str) {
        self.say(View::SetOffers(offers, effort.into()));
    }

    pub fn set_networks(&mut self, client: &Client) {
        self.say(View::SetNetworks(Box::new(client.clone())));
    }

    pub fn set_modes(&mut self, modes: Option<Vec<(String, Mode)>>) {
        self.say(View::SetModes(modes));
    }

    pub fn set_prompts(&mut self, prompts: Vec<crate::prompts::Entry>) {
        self.say(View::SetPrompts(prompts));
    }

    pub fn set_all_allowed(&mut self, all_allowed: Vec<String>) {
        self.say(View::SetAllAllowed(all_allowed));
    }

    pub fn set_models(&mut self, default: &str, contexts: Vec<(String, u64)>) {
        self.say(View::SetModels(default.into(), contexts));
    }

    pub fn set_credit(&mut self, credit: Option<String>) {
        self.say(View::SetCredit(credit));
    }

    pub fn set_closing(&mut self, id: &Id, doing: Option<&'static str>) {
        self.book.set_closing(id, doing);
        if doing.is_some() {
            self.gone_with_them(id);
        }
        self.say(View::SetClosing(id.clone(), doing));
    }

    pub fn key_saved(&mut self, path: &str) {
        self.set_keyed(true);
        self.say(View::KeySaved(path.into()));
    }

    pub fn key_exists(&mut self) {
        self.say(View::KeyExists);
    }

    pub fn key_refused(&mut self, why: String) {
        self.say(View::KeyRefused(why));
    }

    pub fn set_key_path(&mut self, path: Option<String>) {
        self.say(View::SetKeyPath(path));
    }

    pub fn set_keyed(&mut self, keyed: bool) {
        self.say(View::SetKeyed(keyed));
    }

    pub fn ask_removal(&mut self, id: Id, title: String, lost: Vec<String>, archive: bool) {
        self.say(View::AskRemoval(id, title, lost, archive));
    }

    pub fn ask(&mut self, card: Card) {
        self.say(View::Ask(card));
    }

    pub fn question(&mut self, asked: Asked) {
        self.say(View::Question(asked));
    }

    pub fn ask_admission(&mut self, template: String, remotes: Vec<String>) {
        self.say(View::AskAdmission(template, remotes));
    }

    pub fn show_workspace(&mut self, id: &Id, record: &crate::card::Record) {
        self.say(View::ShowWorkspace(id.clone(), Box::new(record.clone())));
    }

    pub fn reshow_workspace(&mut self, id: &Id, record: &crate::card::Record) {
        self.say(View::ReshowWorkspace(id.clone(), Box::new(record.clone())));
    }

    pub fn show_skills(&mut self, found: &crate::skills::Found) {
        self.say(View::ShowSkills(found.clone()));
    }

    pub fn show_schedules(&mut self, entries: &[(String, String)]) {
        self.say(View::ShowSchedules(entries.to_vec()));
    }

    pub fn show_commands(&mut self, commands: &[(String, String)], more: usize) {
        self.say(View::ShowCommands(commands.to_vec(), more));
    }

    pub fn show_output(
        &mut self,
        id: &Id,
        number: u64,
        read: Result<crate::output::Output, String>,
    ) {
        self.say(View::ShowOutput(id.clone(), number, read));
    }

    pub fn show_debug(
        &mut self,
        id: &Id,
        request: u64,
        view: Result<Vec<(String, String)>, String>,
    ) {
        self.say(View::ShowDebug(id.clone(), request, view));
    }

    pub fn set_without_jev(&mut self, without: bool) {
        self.say(View::SetWithoutJev(without));
    }

    pub fn set_today(&mut self, today: u64) {
        self.say(View::SetToday(today));
    }

    pub fn set_sub_agents(&mut self, read: Vec<(Id, Vec<TaskStart>)>) {
        self.say(View::SetSubAgents(read));
    }

    pub fn set_rows(&mut self, rows: Vec<Row>) {
        self.book.set_rows(rows.clone());
        self.say(View::SetRows(rows));
    }

    pub fn add_row(&mut self, row: Row) {
        self.book.add_row(row.clone());
        self.say(View::AddRow(row));
    }

    pub fn remove_row(&mut self, id: &Id) {
        self.book.remove_row(id);
        self.counted.remove(id);
        self.replaying.remove(id);
        self.say(View::RemoveRow(id.clone()));
    }

    pub fn set_archived(&mut self, id: &Id, archived: bool) {
        self.book.set_archived(id, archived);
        if archived {
            self.gone_with_them(id);
        }
        self.say(View::SetArchived(id.clone(), archived));
    }

    pub fn set_no_workspaces(&mut self, why: Option<String>) {
        self.say(View::SetNoWorkspaces(why));
    }

    pub fn set_jailless(&mut self, why: Option<String>) {
        self.say(View::SetJailless(why));
    }

    pub fn set_limits(&mut self, limits: crate::cost::Limits) {
        self.say(View::SetLimits(limits));
    }

    pub fn set_default_routing(&mut self, routing: &str) {
        self.say(View::SetDefaultRouting(routing.into()));
    }

    pub fn set_default_model(&mut self, model: &str) {
        self.say(View::SetDefaultModel(model.into()));
    }

    pub fn open_new_template(&mut self, remote: Option<&str>) {
        self.say(View::OpenNewTemplate(remote.map(str::to_string)));
    }

    pub fn focus_composer(&mut self) {
        self.say(View::FocusComposer);
    }

    pub fn folders(&mut self, answer: crate::chooser::FoldersAnswer) {
        self.say(View::Folders(answer));
    }
}

impl App {
    /// Shows what the session said.
    pub fn apply(&mut self, view: View) {
        match view {
            View::Opened(book) => self.opened(*book),
            // Of what it left, or one the session opened before it heard of
            // this one: all of it but what would be drawn in this transcript.
            View::SetActive(_) if self.opening() => {}
            View::SetQueued(_) if self.opening() => {}
            View::Update(of, update, now, _) if self.opening() => {
                self.heard_of_left(of.as_ref(), update, now);
            }
            View::Replay(of, _, events, _) if self.opening() => {
                self.replayed_of_left(of.as_ref(), &events);
            }
            View::SetShare(share) => self.set_share(share),
            View::SetMode(mode) => self.set_mode(mode),
            View::Counted(id, events, ended) => self.counted(&id, &events, &ended),
            View::Note(message) => self.note(message),
            View::Withdraw(conversation, call) => self.withdraw(&conversation, call),
            View::SetActive(id) => self.set_active(id),
            View::Update(_, update, now, pictures) => {
                self.with_pictures(pictures, |app| app.update(update, now));
            }
            View::Background(id, update, now) => self.background(&id, &update, now),
            View::Replay(_, prefix, events, pictures) => {
                self.with_pictures(pictures, |app| {
                    app.replay(prefix.as_deref().map_err(String::as_str), events);
                });
            }
            View::GiveBack(text, pictures, why) => self.give_back(&text, pictures, why),
            View::Restore(text, why) => self.restore(&text, why),
            View::PromptSaved(ask, said) => self.prompt_saved(ask, said),
            View::PromptRefused(ask, why) => self.prompt_refused(ask, why),
            View::TemplateSaved(said) => self.template_saved(said),
            View::TemplateRefused(why) => self.template_refused(why),
            View::SetTemplates(templates) => self.set_templates(templates),
            View::SetSavedTemplates(templates) => self.set_saved_templates(templates),
            View::SetQueued(queued) => self.set_queued(queued),
            View::SetOffers(offers, effort) => self.set_offers(offers, &effort),
            View::SetNetworks(client) => self.set_networks(&client),
            View::SetModes(modes) => self.set_modes(modes),
            View::SetAllAllowed(all_allowed) => self.set_all_allowed(all_allowed),
            View::SetPrompts(prompts) => self.set_prompts(prompts),
            View::SetModels(default, contexts) => self.set_models(&default, contexts),
            View::SetCredit(credit) => self.set_credit(credit),
            View::SetClosing(id, doing) => self.set_closing(&id, doing),
            View::KeySaved(path) => self.key_saved(&path),
            View::KeyExists => self.key_exists(),
            View::KeyRefused(why) => self.key_refused(why),
            View::SetKeyPath(path) => self.set_key_path(path),
            View::SetKeyed(keyed) => self.set_keyed(keyed),
            View::AskRemoval(id, title, lost, archive) => {
                self.ask_removal(id, title, lost, archive);
            }
            View::Ask(card) => self.ask(card),
            View::Question(asked) => self.question(asked),
            View::AskAdmission(template, remotes) => self.ask_admission(template, remotes),
            View::ShowWorkspace(id, record) => self.show_workspace(&id, &record),
            View::ReshowWorkspace(id, record) => self.reshow_workspace(&id, &record),
            View::ShowSkills(found) => self.show_skills(&found),
            View::ShowSchedules(entries) => self.show_schedules(&entries),
            View::ShowCommands(commands, more) => self.show_commands(&commands, more),
            View::ShowOutput(id, number, read) => self.show_output(&id, number, read),
            View::ShowDebug(id, request, view) => self.show_debug(&id, request, view),
            View::SetWithoutJev(without) => self.set_without_jev(without),
            View::SetToday(today) => self.set_today(today),
            View::SetSubAgents(read) => self.set_sub_agents(read),
            View::SetRows(rows) => self.set_rows(rows),
            View::AddRow(row) => self.add_row(row),
            View::RemoveRow(id) => self.remove_row(&id),
            View::SetArchived(id, archived) => self.set_archived(&id, archived),
            View::SetNoWorkspaces(why) => self.set_no_workspaces(why),
            View::SetJailless(why) => self.set_jailless(why),
            View::SetLimits(limits) => self.set_limits(limits),
            View::SetDefaultRouting(routing) => self.set_default_routing(&routing),
            View::SetDefaultModel(model) => self.set_default_model(&model),
            View::OpenNewTemplate(remote) => self.open_new_template(remote.as_deref()),
            View::FocusComposer => self.focus_composer(),
            View::Folders(answer) => self.folders(answer),
            View::Bye(why) => self.note(why),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::unreachable)]

    use super::*;
    use crate::book::RowState;
    use crate::protocol::Up;
    use crate::store::Effect;
    use td_fs::TempDir;
    use td_ui::raster::{Scale, Surface};

    fn id(n: u8) -> Id {
        Id::parse(&format!("{n:032x}")).unwrap()
    }

    fn row(n: u8) -> Row {
        Row {
            id: id(n),
            title: format!("conversation {n}"),
            activity: u64::from(n),
            state: RowState::Closed,
            paused: false,
            workspace: None,
            unconfined: false,
            archived: false,
        }
    }

    fn event(seq: u64, kind: Kind) -> Event {
        Event { seq, time: 0, kind }
    }

    fn hello() -> Update {
        Update::Up(Up::Hello {
            title: "said".into(),
            torn: None,
            interrupted: vec![],
            paused: false,
            prefix: Some(String::new()),
            last: 0,
        })
    }

    /// A small deterministic source, std having none.
    struct Dice(u64);

    impl Dice {
        fn roll(&mut self, sides: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % sides
        }
    }

    /// What a conversation's process may say.
    fn said(dice: &mut Dice, seq: &mut u64) -> Update {
        *seq += 1;
        let at = *seq;
        let started = at.saturating_sub(dice.roll(3));
        match dice.roll(10) {
            0 => hello(),
            1 | 2 => Update::Up(Up::Event(event(
                at,
                Kind::Started {
                    effect: Effect::Turn,
                    of: 0,
                },
            ))),
            3 => Update::Up(Up::Event(event(
                at,
                Kind::Finished {
                    started,
                    outcome: "replied".into(),
                    retry: false,
                },
            ))),
            4 => Update::Up(Up::Event(event(at, Kind::Interrupted { started }))),
            5 => Update::Up(Up::Event(event(
                at,
                Kind::Pause {
                    paused: dice.roll(2) == 0,
                },
            ))),
            6 => Update::Restarting {
                reason: "gone".into(),
            },
            7 => Update::Failed {
                reason: "gone".into(),
            },
            8 => Update::Up(Up::Delivered {
                delivery: "d".into(),
            }),
            _ => Update::Up(Up::Title {
                title: "named".into(),
            }),
        }
    }

    /// The session's book and the window's read the same at every step of
    /// whatever the session says, the window opening a row itself included.
    #[test]
    fn the_sessions_book_and_the_windows_stay_the_same() {
        let scratch = TempDir::new("td-agent-view").unwrap();
        let surface = Surface::new(1280, 800, Scale::default()).unwrap();
        for seed in 1..=40u64 {
            let mut dice = Dice(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
            let mut port = Port::new(StateDir::at(scratch.path().to_path_buf()));
            port.attach();
            let mut app = App::new(surface, None, Mode::Auto).unwrap();
            let mut seq = 0;
            port.set_rows((1..=4).map(row).collect());
            for _ in 0..200 {
                let n = 1 + u8::try_from(dice.roll(5)).unwrap();
                match dice.roll(12) {
                    0 | 1 => port.set_active(id(n)),
                    2 => {
                        // With what the session said still on its way, half
                        // the time, as it can be across a socket.
                        if dice.roll(2) == 0 {
                            for view in port.take() {
                                app.apply(view);
                            }
                        }
                        app.open(id(n));
                        let mut asked = vec![Request::Interrupt];
                        asked.extend(app.take_requests());
                        port.asked(&asked);
                        seq = 0;
                    }
                    3 => port.add_row(row(n)),
                    4 => port.remove_row(&id(n)),
                    5 => port.set_closing(&id(n), Some("deleting")),
                    6 => port.set_closing(&id(n), None),
                    7 => port.set_archived(&id(n), dice.roll(2) == 0),
                    8 => {
                        let update = said(&mut dice, &mut seq);
                        port.background(&id(n), &update, seq);
                    }
                    9 => {
                        seq = 0;
                        let events = (0..dice.roll(4))
                            .filter_map(|_| match said(&mut dice, &mut seq) {
                                Update::Up(Up::Event(event)) => Some(event),
                                _ => None,
                            })
                            .collect();
                        port.replay(Ok(""), events);
                    }
                    _ => {
                        let update = said(&mut dice, &mut seq);
                        port.update(update, seq);
                    }
                }
                for view in port.take() {
                    app.apply(view);
                }
                assert_eq!(app.book(), &port.book, "seed {seed}");
            }
        }
    }

    fn card(conversation: u8, call: u64) -> Card {
        Card {
            conversation: id(conversation),
            call,
            title: "may it?".into(),
            details: Vec::new(),
            always: None,
            resume: false,
        }
    }

    /// A window that attaches is told what one there all along would know, of
    /// what is still so.
    #[test]
    fn a_window_that_attaches_is_told_the_last_of_each_setting_and_what_is_still_asked() {
        let scratch = TempDir::new("td-agent-view").unwrap();
        let mut port = Port::new(StateDir::at(scratch.path().to_path_buf()));
        port.set_rows(vec![row(1), row(2)]);
        port.set_credit(Some("old".into()));
        port.set_today(1);
        port.set_credit(Some("new".into()));
        port.note("said while no window was there");
        port.ask(card(1, 1));
        port.ask(card(1, 2));
        port.ask(card(1, 3));
        port.ask(card(2, 4));
        port.ask(card(2, 5));
        assert!(port.take().is_empty());
        port.withdraw(&id(1), Some(1));
        port.withdraw(&id(2), None);
        port.asked(&[Request::Decide {
            conversation: id(1),
            call: 2,
            answer: crate::ui::Answer::Once(true),
        }]);
        port.ask_removal(id(2), "two".into(), Vec::new(), false);
        // Asked only of a window that asked for that template, so never kept.
        port.ask_admission("template".into(), Vec::new());
        port.attach();
        let told = port.take();
        let Some(View::Opened(book)) = told.first() else {
            unreachable!("the book first");
        };
        assert_eq!(book.rows.len(), 2);
        let rest: Vec<View> = told.into_iter().skip(1).collect();
        assert_eq!(
            rest,
            [
                View::SetToday(1),
                View::SetCredit(Some("new".into())),
                View::Ask(card(1, 3)),
                View::AskRemoval(id(2), "two".into(), Vec::new(), false),
                View::Note("said while no window was there".into()),
            ]
        );
        // A setting said again unchanged is not said again.
        port.set_today(1);
        assert!(port.take().is_empty());
        // Except what the window ignored while it waited for an opening.
        port.set_queued(vec!["q".into()]);
        port.asked(&[Request::Open(id(1))]);
        port.take();
        port.set_queued(vec!["q".into()]);
        assert_eq!(port.take(), [View::SetQueued(vec!["q".into()])]);
        // And what is said to it from now on.
        port.note("heard");
        assert_eq!(port.take(), [View::Note("heard".into())]);
        port.detach();
        port.note("not heard");
        assert!(port.take().is_empty());
    }

    /// What comes of the conversation left while the session has yet to hear
    /// of the opening is not drawn in the one opened, nor lost.
    #[test]
    fn what_the_conversation_left_says_while_another_opens_reaches_the_person() {
        let scratch = TempDir::new("td-agent-view").unwrap();
        let surface = Surface::new(1280, 800, Scale::default()).unwrap();
        let mut port = Port::new(StateDir::at(scratch.path().to_path_buf()));
        let mut app = App::new(surface, None, Mode::Auto).unwrap();
        port.attach();
        port.set_rows(vec![row(1), row(2)]);
        port.set_active(id(1));
        for view in port.take() {
            app.apply(view);
        }
        port.update(
            Update::Refused {
                text: Some("the message".into()),
                pictures: Vec::new(),
                reason: "it is full".into(),
            },
            1,
        );
        port.update(
            Update::Failed {
                reason: "it fell over".into(),
            },
            2,
        );
        port.set_queued(vec!["queued in the one left".into()]);
        app.open(id(2));
        for view in port.take() {
            app.apply(view);
        }
        assert_eq!(app.composed().trim_end(), "the message");
        // The refusal's and the failure's.
        assert_eq!(app.unread(), 2);
        assert!(app.queued_lines().is_empty());
        port.asked(&app.take_requests());
        for view in port.take() {
            app.apply(view);
        }
        assert_eq!(app.book(), &port.book);
        assert!(!app.opening());
    }

    /// A person who was not there is told what was said for them, once; a
    /// message refused meanwhile is not lost.
    #[test]
    fn what_was_said_while_no_window_was_there_reaches_the_next_once() {
        let scratch = TempDir::new("td-agent-view").unwrap();
        let mut port = Port::new(StateDir::at(scratch.path().to_path_buf()));
        port.set_rows(vec![row(1)]);
        assert!(!port.given_back_waits());
        port.give_back("the message", Vec::new(), "refused".into());
        assert!(port.given_back_waits());
        port.restore("another", "refused".into());
        for n in 0..LATER_NOTES + 2 {
            port.note(n.to_string());
        }
        port.background(
            &id(1),
            &Update::Up(crate::protocol::Up::Event(Event {
                seq: 1,
                time: 0,
                kind: Kind::Notice {
                    text: "out of wakes".into(),
                },
            })),
            0,
        );
        port.attach();
        let told = port.take();
        assert!(told.contains(&View::GiveBack(
            "the message".into(),
            Vec::new(),
            "refused".into()
        )));
        assert!(told.contains(&View::Restore("another".into(), "refused".into())));
        let notes = told
            .iter()
            .filter_map(|view| match view {
                View::Note(said) => Some(said.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(notes.len(), LATER_NOTES);
        assert!(
            notes.last().unwrap().ends_with(": out of wakes"),
            "{notes:?}"
        );
        assert!(!notes.contains(&"0"), "the oldest went");
        assert!(!port.given_back_waits());
        // What a window's last request brought, as another takes over from it,
        // is the next one's.
        port.note("while taken over");
        port.attach();
        assert!(port.take().contains(&View::Note("while taken over".into())));
        // A window lost with something unsent keeps it for the next.
        port.note("unsent");
        let unsent = port.take();
        port.lost(unsent);
        port.attach();
        let told = port.take();
        assert!(told.contains(&View::Note("unsent".into())));
        assert!(!told.iter().any(|view| matches!(view, View::GiveBack(..))));
    }

    /// The folders asked for are the asker's: the next window asks for its own.
    #[test]
    fn folders_listed_are_told_to_the_window_there_and_kept_for_no_other() {
        let scratch = TempDir::new("td-agent-view").unwrap();
        let mut port = Port::new(StateDir::at(scratch.path().to_path_buf()));
        let answer = crate::chooser::FoldersAnswer {
            asked: Vec::new(),
            from: None,
            read: Err("why".into()),
        };
        let told = |port: &mut Port| {
            port.take()
                .iter()
                .filter(|view| matches!(view, View::Folders(_)))
                .count()
        };
        port.folders(answer.clone());
        port.attach();
        assert_eq!(told(&mut port), 0, "no window was there");
        port.folders(answer.clone());
        assert_eq!(told(&mut port), 1);
        port.folders(answer.clone());
        port.attach();
        assert_eq!(told(&mut port), 0, "the last window's");
        port.folders(answer);
        let unsent = port.take();
        port.lost(unsent);
        port.attach();
        assert_eq!(told(&mut port), 0, "lost with that window");
    }

    /// A message given back into a link that had died comes to the next window.
    #[test]
    fn what_a_far_window_did_not_answer_after_being_given_is_given_again() {
        let scratch = TempDir::new("td-agent-view").unwrap();
        let mut port = Port::new(StateDir::at(scratch.path().to_path_buf()));
        let given = |port: &mut Port| {
            port.take()
                .iter()
                .filter(|view| matches!(view, View::GiveBack(..)))
                .count()
        };
        port.remember_told(true);
        port.attach();
        port.take();
        port.give_back("m", Vec::new(), "w".into());
        assert_eq!(given(&mut port), 1);
        port.detach();
        assert!(port.given_back_waits());
        port.attach();
        assert_eq!(given(&mut port), 1, "again");
        port.received();
        port.detach();
        assert!(!port.given_back_waits(), "it had it");
        // Lost unsent, it is kept once.
        port.attach();
        port.take();
        port.give_back("n", Vec::new(), "w".into());
        let unsent = port.take();
        port.lost(unsent);
        port.attach();
        assert_eq!(given(&mut port), 1);
        // A window here, which shows what it is given or says why not.
        port.received();
        port.remember_told(false);
        port.give_back("o", Vec::new(), "w".into());
        port.take();
        port.detach();
        assert!(!port.given_back_waits());
    }

    /// A stored key is the next window's to know of.
    #[test]
    fn a_key_saved_is_told_to_the_next_window() {
        let scratch = TempDir::new("td-agent-view").unwrap();
        let mut port = Port::new(StateDir::at(scratch.path().to_path_buf()));
        port.set_keyed(false);
        port.key_saved("the path");
        port.attach();
        let told = port.take();
        assert!(told.contains(&View::SetKeyed(true)));
        assert!(!told.contains(&View::SetKeyed(false)));
    }

    /// What a window is told of processes goes with the process, and a process
    /// started again does not count its log twice, nor make news of it.
    #[test]
    fn what_is_told_of_processes_goes_with_them_and_a_log_read_back_is_not_news() {
        let scratch = TempDir::new("td-agent-view").unwrap();
        let mut port = Port::new(StateDir::at(scratch.path().to_path_buf()));
        let event =
            |seq, kind| Update::Up(crate::protocol::Up::Event(Event { seq, time: 0, kind }));
        let process = Kind::Process {
            number: 1,
            call: 1,
            command: "make".into(),
        };
        let task = Kind::Task {
            call: 2,
            agent: "helper".into(),
            model: "a/b".into(),
            prefix: "p".into(),
            prompt: "do it".into(),
        };
        let hello = Update::Up(crate::protocol::Up::Hello {
            title: "t".into(),
            torn: None,
            interrupted: Vec::new(),
            paused: false,
            prefix: None,
            last: 3,
        });
        let notice = Kind::Notice { text: "old".into() };
        port.set_rows(vec![row(1), row(2), row(3)]);
        for of in 1..=3 {
            port.background(&id(of), &event(1, process.clone()), 0);
        }
        port.background(&id(1), &event(2, task.clone()), 0);
        // Started again, it says all of it once more, 5 times over.
        for _ in 0..5 {
            port.background(&id(1), &hello, 0);
            port.background(&id(1), &event(1, process.clone()), 0);
            port.background(&id(1), &event(2, task.clone()), 0);
            port.background(&id(1), &event(3, notice.clone()), 0);
        }
        port.background(&id(1), &event(4, Kind::Notice { text: "new".into() }), 0);
        port.set_archived(&id(2), true);
        port.remove_row(&id(3));
        port.attach();
        let told = port.take();
        let counted = |of: u8| {
            told.iter().find_map(|view| match view {
                View::Counted(id_of, events, _) if *id_of == id(of) => Some(events.len()),
                _ => None,
            })
        };
        assert_eq!(counted(1), Some(2), "the process and the task, once each");
        assert_eq!(counted(2), Some(0), "archiving ended its process");
        assert_eq!(counted(3), None, "deleted");
        let notes: Vec<&View> = told
            .iter()
            .filter(|view| matches!(view, View::Note(_)))
            .collect();
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(matches!(notes.first(), Some(View::Note(said)) if said.ends_with(": new")));
    }

    /// The split a window saved is the next one's, unsaid to the one that saved
    /// it, which may have moved it again by then.
    #[test]
    fn a_split_saved_is_told_to_the_next_window_and_not_back_to_this_one() {
        let scratch = TempDir::new("td-agent-view").unwrap();
        let mut port = Port::new(StateDir::at(scratch.path().to_path_buf()));
        port.set_share(None);
        port.attach();
        port.take();
        port.saved_share(Some((3, 10)));
        assert!(port.take().is_empty());
        port.attach();
        let told = port.take();
        assert!(told.contains(&View::SetShare(Some((3, 10)))));
        assert!(!told.contains(&View::SetShare(None)));
    }

    /// A failed conversation has no process to finish what its log left open:
    /// reading the log again does not make it look as if it were running.
    #[test]
    fn a_replay_of_a_failed_conversation_leaves_it_failed() {
        let mut book = Book::default();
        book.set_rows(vec![row(1)]);
        book.set_active(id(1));
        book.update(
            &Update::Failed {
                reason: "it fell over".into(),
            },
            0,
        );
        book.replay(&[Event {
            seq: 1,
            time: 0,
            kind: Kind::Started {
                effect: crate::store::Effect::Turn,
                of: 1,
            },
        }]);
        assert_eq!(
            book.rows.first().unwrap().state,
            crate::book::RowState::Failed
        );
    }

    /// A window that attaches lists what each conversation is running, though it
    /// has not read that conversation's log.
    #[test]
    fn a_window_that_attaches_is_told_each_conversations_processes_and_sub_agents() {
        let scratch = TempDir::new("td-agent-view").unwrap();
        let mut port = Port::new(StateDir::at(scratch.path().to_path_buf()));
        let event =
            |seq, kind| Update::Up(crate::protocol::Up::Event(Event { seq, time: 0, kind }));
        let process = |number| Kind::Process {
            number,
            call: 1,
            command: "make".into(),
        };
        port.set_rows(vec![row(1), row(2)]);
        port.background(&id(1), &event(1, process(1)), 0);
        port.background(&id(1), &event(2, process(2)), 0);
        port.background(
            &id(1),
            &event(
                3,
                Kind::Ended {
                    number: 1,
                    how: "killed".into(),
                    held: Default::default(),
                    tail: Default::default(),
                },
            ),
            0,
        );
        port.background(&id(2), &event(1, process(3)), 0);
        port.background(
            &id(2),
            &Update::Failed {
                reason: "it fell over".into(),
            },
            0,
        );
        port.attach();
        let told = port.take();
        let counted = |of: u8| {
            told.iter().find_map(|view| match view {
                View::Counted(id_of, events, _) if *id_of == id(of) => Some(events.len()),
                _ => None,
            })
        };
        assert_eq!(counted(1), Some(1), "the one still running");
        assert_eq!(counted(2), Some(0), "its process took them with it");
    }

    /// An open that crossed a closing is answered with the closing's book.
    #[test]
    fn a_row_that_closed_before_the_window_heard_does_not_open() {
        let scratch = TempDir::new("td-agent-view").unwrap();
        let surface = Surface::new(1280, 800, Scale::default()).unwrap();
        let mut port = Port::new(StateDir::at(scratch.path().to_path_buf()));
        let mut app = App::new(surface, None, Mode::Auto).unwrap();
        port.attach();
        port.set_rows(vec![row(1), row(2)]);
        for view in port.take() {
            app.apply(view);
        }
        port.set_closing(&id(2), Some("deleting"));
        app.open(id(2));
        assert_eq!(app.active(), Some(&id(2)));
        port.asked(&app.take_requests());
        assert!(port.take_refused_open());
        for view in port.take() {
            app.apply(view);
        }
        assert_eq!(port.active(), None);
        assert_eq!(app.book(), &port.book);
        assert_eq!(app.active(), None);
    }

    /// A picture reaches the window with the message that shows it, and goes
    /// when the message has been shown.
    #[test]
    fn a_message_brings_its_pictures_and_says_why_one_cannot_be_read() {
        let scratch = TempDir::new("td-agent-view").unwrap();
        let mut port = Port::new(StateDir::at(scratch.path().to_path_buf()));
        port.attach();
        port.set_rows(vec![row(1)]);
        port.set_active(id(1));
        port.take();
        let reference = crate::picture::Reference {
            digest: "absent".into(),
            mime: "image/png",
            width: 1,
            height: 1,
            bytes: 1,
            label: "[Image #1]".into(),
        };
        port.update(
            Update::Up(Up::Event(event(
                1,
                Kind::User {
                    text: "see".into(),
                    delivery: "d".into(),
                    pictures: vec![reference],
                },
            ))),
            0,
        );
        match port.take().as_slice() {
            [View::Update(_, _, _, pictures)] => {
                let read = pictures.get("absent").unwrap();
                assert!(read.as_ref().is_err_and(|why| why.contains("absent")));
            }
            _ => unreachable!("one update"),
        }
    }
}

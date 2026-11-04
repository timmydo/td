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
    Note(String),
    Withdraw(Id, Option<u64>),
    SetActive(Id),
    Update(Update, u64, Pictures),
    Background(Id, Update, u64),
    Replay(Result<String, String>, Vec<Event>, Pictures),
    GiveBack(String, Vec<Attached>, String),
    Restore(String, String),
    TemplateSaved(String),
    TemplateRefused(String),
    SetTemplates(Vec<(String, bool)>),
    SetSavedTemplates(Vec<Template>),
    SetQueued(Vec<String>),
    SetOffers(Vec<crate::picker::Offer>, String),
    SetNetworks(Box<Client>),
    SetModes(Option<Vec<(String, Mode)>>),
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
}

/// The session's end: what it tells the window, kept in order until the
/// window takes it, and its own copy of what those messages say of the
/// conversations.
pub struct Port {
    book: Book,
    /// Where each conversation keeps its pictures.
    state: StateDir,
    out: Vec<View>,
}

impl Port {
    pub fn new(state: StateDir) -> Self {
        Self {
            book: Book::default(),
            state,
            out: Vec::new(),
        }
    }

    /// What is waiting for the window, in order.
    pub fn take(&mut self) -> Vec<View> {
        std::mem::take(&mut self.out)
    }

    /// What the window asks changes what it shows before it is served, so
    /// the book follows first: a row it opened stands for every request of
    /// the batch, whichever came first, as it did when the session read the
    /// window's own state. What was on its way to the window meanwhile was
    /// said without that, so the window is sent this book to take.
    pub fn asked(&mut self, requests: &[Request]) {
        for request in requests {
            if let Request::Open(id) = request {
                self.book.set_active(id.clone());
                self.out.push(View::Opened(Box::new(self.book.clone())));
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

    pub fn set_share(&mut self, share: Option<(u32, u32)>) {
        self.out.push(View::SetShare(share));
    }

    pub fn note(&mut self, message: impl Into<String>) {
        self.out.push(View::Note(message.into()));
    }

    pub fn withdraw(&mut self, conversation: &Id, call: Option<u64>) {
        self.out.push(View::Withdraw(conversation.clone(), call));
    }

    pub fn set_active(&mut self, id: Id) {
        self.book.set_active(id.clone());
        self.out.push(View::SetActive(id));
    }

    pub fn update(&mut self, update: Update, now: u64) {
        let pictures = match &update {
            Update::Up(crate::protocol::Up::Event(event)) => self.pictures(std::iter::once(event)),
            _ => Pictures::new(),
        };
        self.book.update(&update, now);
        self.out.push(View::Update(update, now, pictures));
    }

    pub fn background(&mut self, id: &Id, update: &Update, now: u64) {
        self.book.background(id, update, now);
        self.out
            .push(View::Background(id.clone(), update.clone(), now));
    }

    pub fn replay(&mut self, prefix: Result<&str, &str>, events: Vec<Event>) {
        let pictures = self.pictures(events.iter());
        self.book.replay(&events);
        let prefix = prefix.map(str::to_string).map_err(str::to_string);
        self.out.push(View::Replay(prefix, events, pictures));
    }

    pub fn give_back(&mut self, text: &str, pictures: Vec<Attached>, why: String) {
        self.out.push(View::GiveBack(text.into(), pictures, why));
    }

    pub fn restore(&mut self, text: &str, why: String) {
        self.out.push(View::Restore(text.into(), why));
    }

    pub fn template_saved(&mut self, said: String) {
        self.out.push(View::TemplateSaved(said));
    }

    pub fn template_refused(&mut self, why: String) {
        self.out.push(View::TemplateRefused(why));
    }

    pub fn set_templates(&mut self, templates: Vec<(String, bool)>) {
        self.out.push(View::SetTemplates(templates));
    }

    pub fn set_saved_templates(&mut self, templates: Vec<Template>) {
        self.out.push(View::SetSavedTemplates(templates));
    }

    pub fn set_queued(&mut self, queued: Vec<String>) {
        self.out.push(View::SetQueued(queued));
    }

    pub fn set_offers(&mut self, offers: Vec<crate::picker::Offer>, effort: &str) {
        self.out.push(View::SetOffers(offers, effort.into()));
    }

    pub fn set_networks(&mut self, client: &Client) {
        self.out.push(View::SetNetworks(Box::new(client.clone())));
    }

    pub fn set_modes(&mut self, modes: Option<Vec<(String, Mode)>>) {
        self.out.push(View::SetModes(modes));
    }

    pub fn set_models(&mut self, default: &str, contexts: Vec<(String, u64)>) {
        self.out.push(View::SetModels(default.into(), contexts));
    }

    pub fn set_credit(&mut self, credit: Option<String>) {
        self.out.push(View::SetCredit(credit));
    }

    pub fn set_closing(&mut self, id: &Id, doing: Option<&'static str>) {
        self.book.set_closing(id, doing);
        self.out.push(View::SetClosing(id.clone(), doing));
    }

    pub fn key_saved(&mut self, path: &str) {
        self.out.push(View::KeySaved(path.into()));
    }

    pub fn key_exists(&mut self) {
        self.out.push(View::KeyExists);
    }

    pub fn key_refused(&mut self, why: String) {
        self.out.push(View::KeyRefused(why));
    }

    pub fn set_key_path(&mut self, path: Option<String>) {
        self.out.push(View::SetKeyPath(path));
    }

    pub fn set_keyed(&mut self, keyed: bool) {
        self.out.push(View::SetKeyed(keyed));
    }

    pub fn ask_removal(&mut self, id: Id, title: String, lost: Vec<String>, archive: bool) {
        self.out.push(View::AskRemoval(id, title, lost, archive));
    }

    pub fn ask(&mut self, card: Card) {
        self.out.push(View::Ask(card));
    }

    pub fn question(&mut self, asked: Asked) {
        self.out.push(View::Question(asked));
    }

    pub fn ask_admission(&mut self, template: String, remotes: Vec<String>) {
        self.out.push(View::AskAdmission(template, remotes));
    }

    pub fn show_workspace(&mut self, id: &Id, record: &crate::card::Record) {
        self.out
            .push(View::ShowWorkspace(id.clone(), Box::new(record.clone())));
    }

    pub fn reshow_workspace(&mut self, id: &Id, record: &crate::card::Record) {
        self.out
            .push(View::ReshowWorkspace(id.clone(), Box::new(record.clone())));
    }

    pub fn show_skills(&mut self, found: &crate::skills::Found) {
        self.out.push(View::ShowSkills(found.clone()));
    }

    pub fn show_schedules(&mut self, entries: &[(String, String)]) {
        self.out.push(View::ShowSchedules(entries.to_vec()));
    }

    pub fn show_commands(&mut self, commands: &[(String, String)], more: usize) {
        self.out.push(View::ShowCommands(commands.to_vec(), more));
    }

    pub fn show_output(
        &mut self,
        id: &Id,
        number: u64,
        read: Result<crate::output::Output, String>,
    ) {
        self.out.push(View::ShowOutput(id.clone(), number, read));
    }

    pub fn show_debug(
        &mut self,
        id: &Id,
        request: u64,
        view: Result<Vec<(String, String)>, String>,
    ) {
        self.out.push(View::ShowDebug(id.clone(), request, view));
    }

    pub fn set_without_jev(&mut self, without: bool) {
        self.out.push(View::SetWithoutJev(without));
    }

    pub fn set_today(&mut self, today: u64) {
        self.out.push(View::SetToday(today));
    }

    pub fn set_sub_agents(&mut self, read: Vec<(Id, Vec<TaskStart>)>) {
        self.out.push(View::SetSubAgents(read));
    }

    pub fn set_rows(&mut self, rows: Vec<Row>) {
        self.book.set_rows(rows.clone());
        self.out.push(View::SetRows(rows));
    }

    pub fn add_row(&mut self, row: Row) {
        self.book.add_row(row.clone());
        self.out.push(View::AddRow(row));
    }

    pub fn remove_row(&mut self, id: &Id) {
        self.book.remove_row(id);
        self.out.push(View::RemoveRow(id.clone()));
    }

    pub fn set_archived(&mut self, id: &Id, archived: bool) {
        self.book.set_archived(id, archived);
        self.out.push(View::SetArchived(id.clone(), archived));
    }

    pub fn set_no_workspaces(&mut self, why: Option<String>) {
        self.out.push(View::SetNoWorkspaces(why));
    }

    pub fn set_jailless(&mut self, why: Option<String>) {
        self.out.push(View::SetJailless(why));
    }

    pub fn set_limits(&mut self, limits: crate::cost::Limits) {
        self.out.push(View::SetLimits(limits));
    }

    pub fn set_default_routing(&mut self, routing: &str) {
        self.out.push(View::SetDefaultRouting(routing.into()));
    }

    pub fn set_default_model(&mut self, model: &str) {
        self.out.push(View::SetDefaultModel(model.into()));
    }

    pub fn open_new_template(&mut self, remote: Option<&str>) {
        self.out
            .push(View::OpenNewTemplate(remote.map(str::to_string)));
    }

    pub fn focus_composer(&mut self) {
        self.out.push(View::FocusComposer);
    }
}

impl App {
    /// Shows what the session said.
    pub fn apply(&mut self, view: View) {
        match view {
            View::Opened(book) => self.opened(*book),
            // The conversation left's, or one the session opened before it
            // heard of this one.
            View::SetActive(_) | View::Update(..) | View::Replay(..) if self.opening() => {}
            View::SetShare(share) => self.set_share(share),
            View::Note(message) => self.note(message),
            View::Withdraw(conversation, call) => self.withdraw(&conversation, call),
            View::SetActive(id) => self.set_active(id),
            View::Update(update, now, pictures) => {
                self.with_pictures(pictures, |app| app.update(update, now));
            }
            View::Background(id, update, now) => self.background(&id, &update, now),
            View::Replay(prefix, events, pictures) => {
                self.with_pictures(pictures, |app| {
                    app.replay(prefix.as_deref().map_err(String::as_str), events);
                });
            }
            View::GiveBack(text, pictures, why) => self.give_back(&text, pictures, why),
            View::Restore(text, why) => self.restore(&text, why),
            View::TemplateSaved(said) => self.template_saved(said),
            View::TemplateRefused(why) => self.template_refused(why),
            View::SetTemplates(templates) => self.set_templates(templates),
            View::SetSavedTemplates(templates) => self.set_saved_templates(templates),
            View::SetQueued(queued) => self.set_queued(queued),
            View::SetOffers(offers, effort) => self.set_offers(offers, &effort),
            View::SetNetworks(client) => self.set_networks(&client),
            View::SetModes(modes) => self.set_modes(modes),
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

    /// A picture reaches the window with the message that shows it, and goes
    /// when the message has been shown.
    #[test]
    fn a_message_brings_its_pictures_and_says_why_one_cannot_be_read() {
        let scratch = TempDir::new("td-agent-view").unwrap();
        let mut port = Port::new(StateDir::at(scratch.path().to_path_buf()));
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
            [View::Update(_, _, pictures)] => {
                let read = pictures.get("absent").unwrap();
                assert!(read.as_ref().is_err_and(|why| why.contains("absent")));
            }
            _ => unreachable!("one update"),
        }
    }
}

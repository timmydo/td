//! Which conversations there are and how each stands (DESIGN.md §2): their
//! rows, which is open, and which are being closed. It is one reducer over
//! the messages the window is sent, so the session and the app each keep a
//! copy that reads the same: the session's answers what it asks itself, such
//! as where a message may go, without asking the window.

use td_json::Json;

use crate::post::Entry;
use crate::protocol::{maybe, number, string, Up};
use crate::store::{Event, Id, Kind};
use crate::supervisor::Update;
use crate::window_wire::{flag, id_from, list, numbers, workspace_from_json, workspace_json};
use crate::workspace::Workspace;

/// A conversation's process as the list shows it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RowState {
    /// Not open: it has no process.
    Closed,
    Starting,
    Idle,
    /// A turn is under way.
    Running,
    Restarting,
    Failed,
}

impl RowState {
    pub fn word(self) -> &'static str {
        match self {
            Self::Closed => "",
            Self::Starting => "starting",
            Self::Idle => "idle",
            Self::Running => "running",
            Self::Restarting => "restarting",
            Self::Failed => "failed",
        }
    }

    fn parse(word: &str) -> Option<Self> {
        [
            Self::Closed,
            Self::Starting,
            Self::Idle,
            Self::Running,
            Self::Restarting,
            Self::Failed,
        ]
        .into_iter()
        .find(|state| state.word() == word)
    }
}

/// What the list calls a conversation being closed: the session's two
/// words for it, the only ones a message may carry.
const CLOSING: &[&str] = &["archiving", "deleting"];

/// The closing word that reads as `word`, none when nobody uses it.
pub(crate) fn closing_from(word: &str) -> Option<&'static str> {
    CLOSING.iter().find(|closing| **closing == word).copied()
}

/// One conversation in the list.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Row {
    pub id: Id,
    pub title: String,
    /// When it was last active, in milliseconds since the epoch: what the
    /// list orders the conversations by, then their ids.
    pub activity: u64,
    pub state: RowState,
    /// The human paused it: messages from other conversations wait.
    pub paused: bool,
    /// What it works in; the list groups conversations by workspace.
    pub workspace: Option<Workspace>,
    /// Its tools run outside any jail (DESIGN.md §8): the list marks it.
    pub unconfined: bool,
    /// The human archived it: the list hides it unless archived ones
    /// show, and it opens only once unarchived.
    pub archived: bool,
}

impl Row {
    /// Its state as the list names it.
    pub fn word(&self) -> &'static str {
        match self.state {
            _ if self.archived => "archived",
            RowState::Closed | RowState::Idle if self.paused => "paused",
            state => state.word(),
        }
    }

    /// As a message carries it: its state as such, not as the list names it.
    pub(crate) fn to_json(&self) -> Json {
        Json::Obj(vec![
            ("id".into(), Json::Str(self.id.to_string())),
            ("title".into(), Json::Str(self.title.clone())),
            ("activity".into(), Json::from(self.activity)),
            ("state".into(), Json::Str(self.state.word().into())),
            ("paused".into(), Json::Bool(self.paused)),
            (
                "workspace".into(),
                self.workspace.as_ref().map_or(Json::Null, workspace_json),
            ),
            ("unconfined".into(), Json::Bool(self.unconfined)),
            ("archived".into(), Json::Bool(self.archived)),
        ])
    }

    /// `to_json`'s value back.
    pub(crate) fn from_json(value: &Json) -> Result<Self, String> {
        Ok(Self {
            id: id_from(value, "id")?,
            title: string(value, "title")?,
            activity: number(value, "activity")?,
            state: RowState::parse(&string(value, "state")?).ok_or("a row in no known state")?,
            paused: flag(value, "paused")?,
            workspace: match value.get("workspace") {
                Some(Json::Null) => None,
                Some(workspace) => Some(workspace_from_json(workspace)?),
                None => return Err("no workspace".into()),
            },
            unconfined: flag(value, "unconfined")?,
            archived: flag(value, "archived")?,
        })
    }
}

/// The conversations, as the messages to the window have them.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Book {
    pub(crate) rows: Vec<Row>,
    pub(crate) active: Option<Id>,
    /// Conversations being deleted or archived whose workspaces are being
    /// asked what removing them would lose, each with the list's word for
    /// it: none opens or takes a message meanwhile.
    closing: Vec<(Id, &'static str)>,
    /// The turn each conversation in the background is running.
    background_turns: Vec<(Id, u64)>,
    /// The turns the open conversation's log has started, as opposed to its
    /// requests, whose finishes say nothing of its state.
    turns: Vec<u64>,
    /// The last event of the open conversation's log taken.
    seen: u64,
}

impl Book {
    /// All of it, as a message carries it.
    pub(crate) fn to_json(&self) -> Json {
        let turns = |turns: &[u64]| Json::Arr(turns.iter().map(|n| Json::from(*n)).collect());
        Json::Obj(vec![
            (
                "rows".into(),
                Json::Arr(self.rows.iter().map(Row::to_json).collect()),
            ),
            (
                "active".into(),
                self.active
                    .as_ref()
                    .map_or(Json::Null, |id| Json::Str(id.to_string())),
            ),
            (
                "closing".into(),
                Json::Arr(
                    self.closing
                        .iter()
                        .map(|(id, doing)| {
                            Json::Arr(vec![
                                Json::Str(id.to_string()),
                                Json::Str((*doing).to_string()),
                            ])
                        })
                        .collect(),
                ),
            ),
            (
                "background_turns".into(),
                Json::Arr(
                    self.background_turns
                        .iter()
                        .map(|(id, turn)| {
                            Json::Arr(vec![Json::Str(id.to_string()), Json::from(*turn)])
                        })
                        .collect(),
                ),
            ),
            ("turns".into(), turns(&self.turns)),
            ("seen".into(), Json::from(self.seen)),
        ])
    }

    /// `to_json`'s value back, as it stood: nothing is put in order again.
    pub(crate) fn from_json(value: &Json) -> Result<Self, String> {
        let conversation = |text: &str| Id::parse(text).ok_or("a book names no conversation");
        Ok(Self {
            rows: list(value, "rows")?
                .iter()
                .map(Row::from_json)
                .collect::<Result<_, _>>()?,
            active: match maybe(value, "active")? {
                Some(text) => Some(conversation(&text)?),
                None => None,
            },
            closing: list(value, "closing")?
                .iter()
                .map(|item| match item.as_arr() {
                    Some([Json::Str(id), Json::Str(word)]) => Ok((
                        conversation(id)?,
                        closing_from(word).ok_or("a closing word nobody uses")?,
                    )),
                    _ => Err("a closing conversation is not an id and a word"),
                })
                .collect::<Result<_, _>>()?,
            background_turns: list(value, "background_turns")?
                .iter()
                .map(|item| match item.as_arr() {
                    Some([Json::Str(id), turn]) => Ok((
                        conversation(id)?,
                        turn.as_u64().ok_or("a background turn is not a number")?,
                    )),
                    _ => Err("a background turn is not an id and a number"),
                })
                .collect::<Result<_, _>>()?,
            turns: numbers(value, "turns")?,
            seen: number(value, "seen")?,
        })
    }

    /// The conversations, the most recently active first.
    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    /// The conversation shown, if any.
    pub fn active(&self) -> Option<&Id> {
        self.active.as_ref()
    }

    /// The conversations, as the post office and schedules see them.
    pub fn directory(&self) -> Vec<Entry> {
        self.rows
            .iter()
            .map(|row| Entry {
                id: row.id.clone(),
                state: match row.word() {
                    "" => "idle",
                    word => word,
                }
                .to_string(),
                failed: row.state == RowState::Failed || self.closing(&row.id),
                archived: row.archived,
            })
            .collect()
    }

    /// Whether `started` is a turn of the open conversation, as opposed to
    /// a request of one.
    pub fn is_turn(&self, started: u64) -> bool {
        self.turns.contains(&started)
    }

    /// Whether a conversation may open: one listed, not archived or closing.
    pub fn opens(&self, id: &Id) -> bool {
        !self.closing(id) && self.rows.iter().any(|row| &row.id == id && !row.archived)
    }

    /// The first conversation listed that may open.
    pub fn most_recent(&self) -> Option<Id> {
        self.rows
            .iter()
            .find(|r| !r.archived && !self.closing(&r.id))
            .map(|r| r.id.clone())
    }

    /// Conversation `id` as a message's header names it: its title, short,
    /// and the start of its id.
    pub fn named(&self, id: &Id) -> String {
        let short: String = id.as_str().chars().take(8).collect();
        match self.rows.iter().find(|r| &r.id == id) {
            Some(row) => {
                let title: String = row.title.chars().take(48).collect();
                format!("{title} ({short})")
            }
            None => short,
        }
    }

    /// Whether conversation `id` is being deleted or archived, its
    /// workspace asked.
    pub fn closing(&self, id: &Id) -> bool {
        self.closing.iter().any(|(of, _)| of == id)
    }

    /// The list's word for a conversation being deleted or archived.
    pub fn closing_word(&self, id: &Id) -> Option<&'static str> {
        self.closing
            .iter()
            .find(|(of, _)| of == id)
            .map(|(_, doing)| *doing)
    }

    /// The most recently active first, then by id: what the list shows and
    /// what `most_recent` finds.
    fn order(&mut self) {
        self.rows
            .sort_by(|a, b| b.activity.cmp(&a.activity).then(a.id.cmp(&b.id)));
    }

    pub fn set_rows(&mut self, rows: Vec<Row>) {
        self.rows = rows;
        self.order();
    }

    pub fn add_row(&mut self, row: Row) {
        self.rows.retain(|r| r.id != row.id);
        self.rows.push(row);
        self.order();
    }

    /// Nothing of the open conversation's log has been taken.
    fn clear_open(&mut self) {
        self.turns.clear();
        self.seen = 0;
    }

    fn close_if_open(&mut self, id: &Id) {
        if self.active.as_ref() == Some(id) {
            self.active = None;
            self.clear_open();
        }
    }

    /// Conversation `id` is deleted.
    pub fn remove_row(&mut self, id: &Id) {
        self.rows.retain(|row| &row.id != id);
        self.background_turns.retain(|(of, _)| of != id);
        self.close_if_open(id);
    }

    /// Conversation `id` is being deleted or archived, `doing` saying
    /// which, or no longer: while it is, it is closed.
    pub fn set_closing(&mut self, id: &Id, doing: Option<&'static str>) {
        self.closing.retain(|(other, _)| other != id);
        if let Some(doing) = doing {
            self.closing.push((id.clone(), doing));
            self.background_turns.retain(|(of, _)| of != id);
            if let Some(row) = self.rows.iter_mut().find(|r| &r.id == id) {
                row.state = RowState::Closed;
            }
            self.close_if_open(id);
        }
    }

    /// Conversation `id` is archived, and closed, or back.
    pub fn set_archived(&mut self, id: &Id, archived: bool) {
        if let Some(row) = self.rows.iter_mut().find(|r| &r.id == id) {
            row.archived = archived;
            if archived {
                row.state = RowState::Closed;
            }
        }
        if archived {
            self.background_turns.retain(|(of, _)| of != id);
            self.close_if_open(id);
        }
    }

    /// Conversation `id` is shown, its process starting, and every other
    /// closed but one left running its turn.
    pub fn set_active(&mut self, id: Id) {
        let running = self.turns.last().copied().filter(|_| {
            self.active_row()
                .is_some_and(|r| r.state == RowState::Running)
        });
        if let (Some(left), Some(turn)) = (self.active.clone(), running) {
            if left != id {
                self.background_turns.retain(|(of, _)| *of != left);
                self.background_turns.push((left, turn));
            }
        }
        self.background_turns.retain(|(of, _)| *of != id);
        let background = &self.background_turns;
        for row in &mut self.rows {
            if row.id == id {
                row.state = RowState::Starting;
            } else if !background.iter().any(|(of, _)| *of == row.id)
                && row.state != RowState::Failed
            {
                row.state = RowState::Closed;
            }
        }
        self.active = Some(id);
        self.clear_open();
    }

    fn active_row(&self) -> Option<&Row> {
        let id = self.active.as_ref()?;
        self.rows.iter().find(|r| &r.id == id)
    }

    fn active_row_mut(&mut self) -> Option<&mut Row> {
        let id = self.active.as_ref()?;
        self.rows.iter_mut().find(|r| &r.id == id)
    }

    /// The open conversation's log, read again from its start.
    pub fn replay(&mut self, events: &[Event]) {
        self.clear_open();
        // One with no process stays so, whatever its log's last turn left.
        let failed = self
            .active_row()
            .is_some_and(|row| row.state == RowState::Failed);
        for event in events {
            self.event(event);
        }
        if failed {
            if let Some(row) = self.active_row_mut() {
                row.state = RowState::Failed;
            }
        }
        // The replayed turns set the state: `running` while the last is
        // open, `idle` once it has ended (it may have ended between the
        // window's last poll and the opening).
        if let Some(row) = self.active_row_mut() {
            if row.state == RowState::Starting {
                row.state = RowState::Idle;
            }
        }
    }

    /// What a conversation that is not open said: its state and activity;
    /// `now` is in milliseconds.
    pub fn background(&mut self, id: &Id, update: &Update, now: u64) {
        let Some(row) = self.rows.iter_mut().find(|r| &r.id == id) else {
            return;
        };
        let state = match update {
            // A turn's start, or the human's compaction's.
            Update::Up(Up::Event(Event {
                seq,
                kind: Kind::Started { .. },
                ..
            })) => {
                self.background_turns.retain(|(turn_of, _)| turn_of != id);
                self.background_turns.push((id.clone(), *seq));
                Some(RowState::Running)
            }
            // The turn's end, not one of its requests': its process is
            // let go, so the row is closed.
            Update::Up(Up::Event(Event {
                kind: Kind::Finished { started, .. } | Kind::Interrupted { started },
                ..
            })) => {
                row.activity = now;
                let at = self
                    .background_turns
                    .iter()
                    .position(|(turn_of, turn)| turn_of == id && turn == started);
                at.map(|at| {
                    self.background_turns.swap_remove(at);
                    RowState::Closed
                })
            }
            Update::Up(Up::Delivered { .. }) => {
                row.activity = now;
                None
            }
            Update::Up(Up::Event(Event {
                kind: Kind::Pause { paused },
                ..
            })) => {
                row.paused = *paused;
                None
            }
            Update::Failed { .. } => Some(RowState::Failed),
            Update::Up(Up::Title { title }) => {
                row.title = crate::store::title(title);
                None
            }
            _ => None,
        };
        if let Some(state) = state {
            row.state = state;
        }
        self.order();
    }

    /// What the open conversation's process said, or what became of it;
    /// `now` is in milliseconds.
    pub fn update(&mut self, update: &Update, now: u64) {
        match update {
            Update::Up(Up::Hello { title, paused, .. }) => {
                self.clear_open();
                if let Some(row) = self.active_row_mut() {
                    row.title = crate::store::title(title);
                    row.state = RowState::Idle;
                    row.paused = *paused;
                }
            }
            Update::Up(Up::Event(event)) => self.event(event),
            Update::Up(Up::Delivered { .. }) => {
                if let Some(row) = self.active_row_mut() {
                    row.activity = now;
                }
                self.order();
            }
            Update::Up(Up::Title { title }) => {
                if let Some(row) = self.active_row_mut() {
                    row.title = crate::store::title(title);
                }
            }
            Update::Restarting { .. } => {
                if let Some(row) = self.active_row_mut() {
                    row.state = RowState::Restarting;
                }
            }
            Update::Failed { .. } => {
                if let Some(row) = self.active_row_mut() {
                    row.state = RowState::Failed;
                }
            }
            _ => {}
        }
    }

    /// One event of the open conversation's log.
    fn event(&mut self, event: &Event) {
        if event.seq <= self.seen {
            return;
        }
        self.seen = event.seq;
        match &event.kind {
            Kind::Started { .. } => {
                self.turns.push(event.seq);
                if let Some(row) = self.active_row_mut() {
                    row.state = RowState::Running;
                }
            }
            // A request's end says nothing here: its turn's does.
            Kind::Finished { started, .. } | Kind::Interrupted { started } => {
                if self.turns.contains(started) {
                    if let Some(row) = self.active_row_mut() {
                        if row.state == RowState::Running {
                            row.state = RowState::Idle;
                        }
                    }
                }
            }
            Kind::Pause { paused } => {
                if let Some(row) = self.active_row_mut() {
                    row.paused = *paused;
                }
            }
            _ => {}
        }
    }
}

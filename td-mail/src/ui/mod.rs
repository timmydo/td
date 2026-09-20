//! The client in a window: td-ui's widget window drives the view stack
//! with the keys it reads from its chords, a press on a row, and the
//! wheel's travel; polls the backend's channel each turn; and presents
//! the frame the top view's scene lays out (`frame`): the toolkit's
//! action bar, text entry and list, and td-editor's document pane,
//! read-only for a message and editable for a draft. The window owns
//! the Wayland connection; the session owns the views, the pane, the
//! backend and the account.

pub mod frame;
pub mod input;
pub mod views;

use crate::backend::{self, BackendCommand, BackendResponse};
use crate::compose;
use crate::config::{AccountConfig, RetentionPolicyConfig, SpamConfig};
use crate::regex::UserRegex;
use crate::rules::CompiledRule;
use frame::{Draft, Frame, Layout, Pane};
use input::Key;
use std::io;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};
use td_editor::ui::Outcome;
use td_ui::raster::{Raster, Surface};
use td_ui::window::{Flow, Handler, Input, PointerPhase};
use views::compose::ComposeView;
use views::mailbox_list::MailboxListView;
use views::{Body, Scroll, ViewAction, ViewStack};

/// The tenth of a second the terminal's read timed out at, kept as the
/// longest a turn waits for the backend's answer or the idle-sync clock.
const POLL_MS: u64 = 100;

/// The most scroll keys one wheel frame becomes, so a fling is bounded.
const WHEEL_EVENTS: usize = 64;

/// The most scalars of a view's title the window is given: a subject
/// is the title, and one of any length must fit the protocol's message.
const TITLE_SCALARS: usize = 256;

/// Wait on a fire-and-forget child in a detached thread so it does not linger
/// as a zombie. `Child` has no reaping `Drop` impl, so a dropped handle leaks a
/// PID slot for the lifetime of td-mail.
pub fn reap_in_background(mut child: std::process::Child) {
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

/// What a session keeps of the configuration to open an account's views.
struct Setup {
    accounts: Vec<AccountConfig>,
    account_names: Vec<String>,
    page_size: u32,
    browser: Option<String>,
    reply_from: Option<String>,
    archive_folder: String,
    deleted_folder: String,
    retention_policies: Vec<RetentionPolicyConfig>,
    sync_interval_secs: Option<u64>,
    rules: Arc<Vec<CompiledRule>>,
    custom_headers: Arc<Vec<String>>,
    rules_mailbox_regex: Arc<UserRegex>,
    my_email_regex: Arc<UserRegex>,
    spam_config: SpamConfig,
    offline: bool,
    /// Where drafts are retained: a test's directory, or none for the
    /// state directory the environment names, read as each draft is
    /// retained.
    draft_dir: Option<PathBuf>,
    /// None offline: no connection is ever asked for.
    connector: Option<Sender<ConnectRequest>>,
}

impl Setup {
    fn mailbox_view(
        &self,
        cmd_tx: &Sender<BackendCommand>,
        account: &AccountConfig,
    ) -> MailboxListView {
        MailboxListView::new(
            cmd_tx.clone(),
            account.username.clone(),
            self.reply_from.clone(),
            self.browser.clone(),
            self.page_size,
            self.account_names.clone(),
            account.name.clone(),
            self.archive_folder.clone(),
            self.deleted_folder.clone(),
            self.retention_policies.clone(),
            self.sync_interval_secs,
        )
    }

    /// Opens an account: its backend starts with no connection, so the
    /// window opens at once on the mailbox list's loading state. The
    /// session offline fetches the mailboxes from the cache now; otherwise
    /// the connector is asked for the account's connection, which can
    /// take minutes to make, and the backend holds the fetch, and every
    /// command a view sends meanwhile, until the connection is decided,
    /// so nothing is answered from the cache that the connection then
    /// supersedes and the window never says the client is offline while
    /// it connects.
    fn open_account(
        &self,
        account: &AccountConfig,
        origin: &str,
    ) -> (Sender<BackendCommand>, Receiver<BackendResponse>) {
        let (cmd_tx, resp_rx) = backend::spawn(
            None,
            !self.offline,
            true,
            account.name.clone(),
            self.rules.clone(),
            self.custom_headers.clone(),
            self.rules_mailbox_regex.clone(),
            self.my_email_regex.clone(),
            self.spam_config.clone(),
        );
        let _ = cmd_tx.send(BackendCommand::FetchMailboxes {
            origin: origin.to_string(),
        });
        if let Some(connector) = &self.connector {
            let _ = connector.send((account.clone(), cmd_tx.clone()));
        }
        (cmd_tx, resp_rx)
    }
}

/// A connection to make: the account, and the backend it is for.
type ConnectRequest = (AccountConfig, Sender<BackendCommand>);

/// The connector: the session's one thread for the credential and
/// discovery round trips, so however many times the account is switched
/// while a connection is pending, one is in flight (the package caps the
/// jail's tasks). It serves the latest request, since an account switched
/// away from needs no connection, and sends the backend its connection,
/// or the failure, on which the backend answers what it held; a backend
/// shut down meanwhile has dropped its receiver, and the send fails
/// unheard. A connection that fails leaves the account on its cache,
/// logged; selecting the account again retries. The thread ends with the
/// session, which holds the sender.
fn spawn_connector() -> Sender<ConnectRequest> {
    let (tx, rx) = mpsc::channel::<ConnectRequest>();
    std::thread::spawn(move || {
        while let Ok(mut request) = rx.recv() {
            while let Ok(newer) = rx.try_recv() {
                request = newer;
            }
            let (account, cmd_tx) = request;
            crate::log_info!(
                "[Connect] connecting to {} ({})",
                account.name,
                account.well_known_url
            );
            let decision = match crate::connect_account(&account) {
                Ok(client) => {
                    crate::log_info!("[Connect] connected to {}", account.name);
                    BackendCommand::Connect(Box::new(client))
                }
                Err(e) => {
                    crate::log_error!(
                        "[Connect] {} failed: {}; on the cache until it is selected again",
                        account.name,
                        e
                    );
                    BackendCommand::ConnectFailed(e)
                }
            };
            let _ = cmd_tx.send(decision);
        }
    });
    tx
}

/// The frame's shape, read from the top view's scene before an input is
/// handled, so the scene's borrow of the view ends before the view is
/// changed.
#[derive(Clone, Copy)]
struct Shape {
    layout: Layout,
    /// The list's rows, for a press; none over a text.
    total: Option<usize>,
    keys: &'static [Key],
}

struct Session {
    setup: Setup,
    stack: ViewStack,
    pane: Pane,
    cmd_tx: Sender<BackendCommand>,
    resp_rx: Receiver<BackendResponse>,
    mouse: bool,
    /// The surface the frame is laid out for, as the window last gave it.
    surface: Surface,
    /// The window's title: the top view's, as the frame last showed it.
    title: String,
    sync_interval: Option<Duration>,
    last_user_activity: Instant,
    last_idle_sync: Instant,
    dirty: bool,
    quitting: bool,
    /// The window's close was asked with a draft unsaved: the draft's
    /// question is up, and its answer closes the window or keeps it.
    closing: bool,
}

impl Session {
    fn new(
        setup: Setup,
        stack: ViewStack,
        cmd_tx: Sender<BackendCommand>,
        resp_rx: Receiver<BackendResponse>,
        mouse: bool,
    ) -> Result<Self, String> {
        let surface = Surface::new(800, 600, Default::default()).map_err(|e| e.to_string())?;
        let mut session = Session {
            sync_interval: setup.sync_interval_secs.map(Duration::from_secs),
            setup,
            stack,
            pane: Pane::new()?,
            cmd_tx,
            resp_rx,
            mouse,
            surface,
            title: String::new(),
            last_user_activity: Instant::now(),
            last_idle_sync: Instant::now(),
            dirty: true,
            quitting: false,
            closing: false,
        };
        // The window reads the title at binding, before any poll.
        session.refresh_title();
        Ok(session)
    }

    fn redraw(&mut self) {
        self.dirty = true;
    }

    fn shape(&self) -> Option<Shape> {
        let view = self.stack.current()?;
        let scene = view.scene();
        let total = match &scene.body {
            Body::List { total, .. } => Some(*total),
            Body::Text { .. } | Body::Edit { .. } => None,
        };
        Some(Shape {
            layout: Layout::new(self.surface, &scene),
            total,
            keys: scene.keys,
        })
    }

    /// The rows the body shows, which the page keys move by.
    fn page(&mut self) -> usize {
        match self.shape() {
            Some(shape) => shape.layout.page(&mut self.pane, self.surface),
            None => frame::PAGE_ROWS,
        }
    }

    fn act(&mut self, action: ViewAction) {
        match action {
            ViewAction::Continue => self.redraw(),
            ViewAction::Push(new_view) => {
                self.stack.push(new_view);
                // The pane holds the new view's document at once, so a
                // reading key before the next paint moves it, not the
                // view's under it.
                self.prepare_frame();
                self.redraw();
            }
            ViewAction::Pop => {
                let Some(slot) = self.stack.pop() else {
                    self.quitting = true;
                    return;
                };
                self.pane.close(slot.text);
                // The draft the window's close asked about is closed:
                // the window follows.
                if self.closing {
                    self.quitting = true;
                    return;
                }
                // Let the revealed view refresh state that may have changed
                // while it was hidden (e.g. mailbox unread counts).
                if let Some(view) = self.stack.current_mut() {
                    view.on_reveal();
                }
                // The revealed view's document is the pane's again at
                // once, as a pushed view's is, so a reading key before
                // the next paint is not dropped.
                self.prepare_frame();
                self.redraw();
            }
            ViewAction::Quit => self.quitting = true,
            ViewAction::Compose(draft) => self.compose(&draft),
            ViewAction::SwitchAccount(name) => self.switch_account(&name),
            ViewAction::Scroll(scroll) => {
                let changed = match scroll {
                    Scroll::Lines(rows) => self.pane.scroll(rows),
                    Scroll::Pages(pages) => {
                        let rows = self.page() as isize;
                        self.pane.scroll(rows.saturating_mul(pages))
                    }
                    Scroll::Chord(chord) => self.pane.chord(chord) == Outcome::Changed,
                };
                if changed {
                    self.redraw();
                }
            }
            ViewAction::Request(name) => self.request(name),
        }
    }

    /// Retains the draft as a file, as the `$EDITOR` child was handed
    /// it, and opens it in the pane to edit; a draft that cannot be
    /// retained is logged and not opened, since there would be nothing
    /// to save it over.
    fn compose(&mut self, draft: &compose::ComposeDraft) {
        let prepared = match &self.setup.draft_dir {
            Some(dir) => compose::write_compose_draft_in(draft, dir),
            None => compose::write_compose_draft(draft),
        };
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(e) => {
                crate::log_error!("Failed to retain draft: {}", e);
                self.redraw();
                return;
            }
        };
        crate::log_info!("Draft retained at {}", prepared.draft_path.display());
        if let Some(path) = &prepared.attachment_dir {
            crate::log_info!("Draft attachments retained at {}", path.display());
        }
        match ComposeView::open(prepared.draft_path, prepared.attachment_dir) {
            Ok(view) => self.act(ViewAction::Push(Box::new(view))),
            Err(e) => {
                crate::log_error!("Failed to read the retained draft back: {}", e);
                self.redraw();
            }
        }
    }

    /// Whether the top view's body is a draft being edited, so that
    /// every chord is the pane's; `asking` too when the view holds
    /// the draft but has the keys, asking about it.
    fn editing(&self, asking: bool) -> bool {
        self.stack.current().is_some_and(|view| {
            matches!(
                view.scene().body,
                Body::Edit { focused, .. } if focused || asking
            )
        })
    }

    /// The top view's draft: the document its slot holds, whether or
    /// not the pane shows it this frame.
    fn draft(&mut self) -> Draft<'_> {
        let tab = self
            .stack
            .top()
            .and_then(|slot| slot.text.as_ref().map(frame::Shown::tab));
        Draft::new(&mut self.pane, tab)
    }

    /// A request of the pane's kind, from its chord or a bar label: the
    /// clipboard's are the kill ring's, and the rest are the view's,
    /// with its draft in hand.
    fn request(&mut self, name: &str) {
        let changed = match name {
            "cut" => self.pane.cut(),
            "copy" => {
                self.pane.copy();
                false
            }
            "paste" => self.pane.paste(),
            _ => {
                let Session { stack, pane, .. } = self;
                let tab = stack
                    .top()
                    .and_then(|slot| slot.text.as_ref().map(frame::Shown::tab));
                let mut draft = Draft::new(pane, tab);
                let action = stack
                    .current_mut()
                    .map(|view| view.request(name, &mut draft));
                if let Some(action) = action {
                    self.act(action);
                }
                return;
            }
        };
        if changed {
            self.redraw();
        }
    }

    /// A chord to the pane, and what it asked for served.
    fn pane_chord(&mut self, chord: &str) {
        match self.pane.chord(chord) {
            Outcome::Changed => self.redraw(),
            Outcome::Request { name, .. } => self.request(name),
            Outcome::Created(_) | Outcome::Prefix | Outcome::Ignored => {}
        }
    }

    /// Switches to the named account as the session started on the first:
    /// its mailbox list at once, its connection when the connector has
    /// it. The old backend is told to shut down first; one still finishing
    /// a request holds its store's lock a while longer, and the backend of
    /// an account switched away from and back opens the store once it is
    /// free, blocking nothing meanwhile.
    fn switch_account(&mut self, name: &str) {
        let Some(account) = self.setup.accounts.iter().find(|a| a.name == name) else {
            return;
        };
        let _ = self.cmd_tx.send(BackendCommand::Shutdown);
        let (cmd_tx, resp_rx) = self.setup.open_account(account, "switch_account");
        let mailbox_view = self.setup.mailbox_view(&cmd_tx, account);
        self.cmd_tx = cmd_tx;
        self.resp_rx = resp_rx;
        let old = std::mem::replace(&mut self.stack, ViewStack::new(Box::new(mailbox_view)));
        for slot in old.into_slots() {
            self.pane.close(slot.text);
        }
        self.last_idle_sync = Instant::now();
        self.redraw();
    }

    /// The window's title is the top view's, as its scene names it now,
    /// bounded and on one line whatever a server put in a subject or a
    /// folder's name; read after every input and poll, since the window
    /// sends the title before it asks for the paint.
    fn refresh_title(&mut self) {
        let Some(view) = self.stack.current() else {
            return;
        };
        let title = view.scene().title;
        let shown = title
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .take(TITLE_SCALARS);
        if self.title.chars().eq(shown.clone()) {
            return;
        }
        self.title = shown.collect();
    }

    /// A pending action the top view raised from a response or a press
    /// that rendered its feedback first: whatever it is, it is acted
    /// on, so an open raised by a press is not lost to a response
    /// drained in the same turn.
    fn take_pending(&mut self) {
        let pending = self
            .stack
            .current_mut()
            .and_then(|view| view.take_pending_action());
        if let Some(action) = pending {
            self.act(action);
        }
    }

    /// One of the client's keys to the top view; a bar label's request
    /// is served as the pane's own.
    fn key(&mut self, key: Key) {
        if self.quitting {
            return;
        }
        self.last_user_activity = Instant::now();
        if let Key::Request(name) = key {
            self.request(name);
            return;
        }
        let page = self.page();
        match self.stack.handle_key(key, page) {
            Some(action) => self.act(action),
            None => self.quitting = true,
        }
    }

    /// A chord: every one is the pane's while a draft is edited, and
    /// none is while its view asks about it, when only the client's
    /// keys reach the view; otherwise the client's key when it names
    /// one, and else the pane's, when a text is shown, so a chord the
    /// client does not claim (an arrow with Shift, Tab, a copy) reaches
    /// the document.
    fn chord(&mut self, chord: &str) {
        if self.editing(false) {
            self.last_user_activity = Instant::now();
            self.pane_chord(chord);
            return;
        }
        if self.editing(true) {
            if let Some(key) = input::key(chord) {
                self.key(key);
            }
            return;
        }
        match input::key(chord) {
            Some(key) => self.key(key),
            None => {
                if self
                    .shape()
                    .is_some_and(|shape| shape.layout.pane.is_some())
                {
                    self.pane_chord(chord);
                }
            }
        }
    }

    /// The compositor asks the window to close: it closes at once
    /// unless a draft is unsaved, when the draft's own question is put
    /// instead and its answer decides (a save or a discard closes the
    /// window; Escape keeps it, with the draft), so nothing typed is
    /// lost to the close and a save that fails is seen.
    fn close_requested(&mut self) -> Flow {
        if self.editing(true) && self.draft().dirty() {
            self.closing = true;
            self.request("close-tab");
            self.redraw();
            Flow::Continue
        } else {
            Flow::Quit
        }
    }

    fn pointer(&mut self, phase: PointerPhase, x: i64, y: i64, extend: bool) {
        let Some(shape) = self.shape() else {
            return;
        };
        // A view asking about its draft has the pointer too: the bar's
        // labels answer, and the pane is not touched meanwhile.
        let asking = self.editing(true) && !self.editing(false);
        let Some(rect) = shape.layout.pane.filter(|_| !asking) else {
            // A drag begun in a pane the view no longer shows ends here.
            if self.pane.drag {
                self.pane.cancel_pointer();
            }
            return self.pointer_widgets(shape, phase, x, y);
        };
        if self.pane.drag || (phase == PointerPhase::Press && rect.contains(x, y)) {
            self.pane.place(rect, self.surface);
            if self.pane.pointer(phase, x, y, extend) {
                self.redraw();
            }
            return;
        }
        self.pointer_widgets(shape, phase, x, y);
    }

    /// The pointer over the widgets around the body: a press on a bar
    /// label is its key, on a list row the row's.
    fn pointer_widgets(&mut self, shape: Shape, phase: PointerPhase, x: i64, y: i64) {
        if phase != PointerPhase::Press {
            return;
        }
        if let Some(index) = shape.layout.bar.hit(x, y) {
            if let Some(&key) = shape.keys.get(index) {
                self.key(key);
            }
            return;
        }
        let Some(row) = shape.layout.list.and_then(|list| list.hit(x, y)) else {
            return;
        };
        let first = self.stack.top().map_or(0, |slot| slot.first);
        let index = first.saturating_add(row);
        if shape.total.is_some_and(|total| index < total) {
            self.key(Key::Click(index));
        }
    }

    /// Wheel travel in rows: over a list, one scroll key per row, bounded;
    /// over a text, the pane's scroll.
    fn wheel(&mut self, rows: isize) {
        let Some(shape) = self.shape() else {
            return;
        };
        if shape.layout.pane.is_some() {
            if self.pane.scroll(rows) {
                self.redraw();
            }
            return;
        }
        let key = if rows < 0 {
            Key::ScrollUp
        } else {
            Key::ScrollDown
        };
        for _ in 0..rows.unsigned_abs().min(WHEEL_EVENTS) {
            self.key(key);
        }
    }

    /// Lays the frame out: the list reveals its selection, the entry its
    /// caret, the pane is placed and shows the view's document, and the
    /// title is the view's. Before every paint and every read of the
    /// frame.
    fn prepare_frame(&mut self) {
        self.refresh_title();
        let surface = self.surface;
        let Some(slot) = self.stack.top_mut() else {
            return;
        };
        let scene = slot.view.scene();
        let layout = Layout::new(surface, &scene);
        match &scene.body {
            Body::List {
                total, selected, ..
            } => {
                if let Some(list) = layout.list {
                    slot.first = list.reveal(*total, *selected, slot.first);
                }
            }
            Body::Text { key, text } => {
                if let Some(rect) = layout.pane {
                    self.pane.place(rect, surface);
                    let (_, columns) = self.pane.grid();
                    self.pane
                        .show(&mut slot.text, key, columns, || text(columns));
                }
            }
            // The draft is loaded whether or not the surface has room
            // for the pane, so its view's save is always its own.
            Body::Edit { key, text, .. } => {
                if let Some(rect) = layout.pane {
                    self.pane.place(rect, surface);
                }
                self.pane.edit(&mut slot.text, key, text);
            }
        }
        if let (Some(field), Some(entry)) = (layout.entry, &scene.entry) {
            let len = entry.text.chars().count();
            slot.entry_first = field.reveal(len, len, slot.entry_first);
        }
    }

    /// The frame as text, after `prepare_frame`: what a test reads back.
    #[cfg(test)]
    fn shown(&mut self) -> String {
        self.prepare_frame();
        let slot = self.stack.top().expect("a view");
        let scene = slot.view.scene();
        let frame = Frame {
            surface: self.surface,
            scene: &scene,
            first: slot.first,
            entry_first: slot.entry_first,
            pane: &self.pane,
        };
        td_ui::driven::text(&frame).expect("text").2
    }
}

impl Handler for Session {
    fn title(&self) -> &str {
        &self.title
    }

    fn app_id(&self) -> &str {
        "td-mail"
    }

    fn input(&mut self, input: Input<'_>) -> Flow {
        match input {
            Input::Close => {
                if self.close_requested() == Flow::Quit {
                    return Flow::Quit;
                }
            }
            Input::Resize(surface) => {
                self.surface = surface;
                self.redraw();
            }
            Input::Key { chord, .. } => self.chord(chord),
            Input::Pointer {
                phase,
                x,
                y,
                extend,
            } => {
                if self.mouse {
                    self.pointer(phase, x, y, extend);
                }
            }
            Input::CancelPointer => self.pane.cancel_pointer(),
            Input::Wheel { rows, .. } => {
                if self.mouse {
                    self.wheel(rows);
                }
            }
            Input::Focus(focused) => {
                self.pane.focus(focused);
                self.redraw();
            }
        }
        // The question the close put was answered with the draft kept:
        // the next close asks again.
        if self.closing && !self.quitting && self.editing(false) {
            self.closing = false;
        }
        self.refresh_title();
        if self.quitting {
            Flow::Quit
        } else {
            Flow::Continue
        }
    }

    fn poll(&mut self, now: u64) -> Flow {
        // A backend that is gone answers nothing more; the views stay up
        // on what they hold, as they did in the terminal.
        while let Ok(response) = self.resp_rx.try_recv() {
            if self.stack.handle_response(&response) {
                self.redraw();
            }
            self.take_pending();
        }
        self.take_pending();
        // The caret's blink is a paint only where the pane is shown.
        if self.pane.tick(now)
            && self
                .shape()
                .is_some_and(|shape| shape.layout.pane.is_some())
        {
            self.redraw();
        }
        if let Some(interval) = self.sync_interval {
            if self.last_user_activity.elapsed() >= interval
                && self.last_idle_sync.elapsed() >= interval
            {
                if let Some(view) = self.stack.current_mut() {
                    if view.trigger_idle_sync() {
                        self.last_idle_sync = Instant::now();
                    }
                }
            }
        }
        self.refresh_title();
        if self.quitting {
            Flow::Quit
        } else {
            Flow::Continue
        }
    }

    fn wait_ms(&self, _now: u64) -> u64 {
        POLL_MS
    }

    fn needs_redraw(&self) -> bool {
        self.dirty
    }

    fn paint(&mut self, raster: &mut Raster<'_, '_>, surface: Surface) -> Result<(), String> {
        self.surface = surface;
        self.prepare_frame();
        if let Some(slot) = self.stack.top() {
            let scene = slot.view.scene();
            let frame = Frame {
                surface,
                scene: &scene,
                first: slot.first,
                entry_first: slot.entry_first,
                pane: &self.pane,
            };
            frame::paint(raster, &frame)?;
        }
        self.dirty = false;
        Ok(())
    }

    fn notice(&mut self, message: &str) {
        crate::log_error!("window: {}", message);
    }
}

/// Runs the client in a window on the compositor the environment names,
/// until the window closes or the top view quits. The window opens before
/// the account connects, its mailbox list loading until the connector
/// has decided the connection, so the toplevel is up within the unit's
/// readiness wait however long discovery takes.
#[allow(clippy::too_many_arguments)]
pub fn run(
    accounts: Vec<AccountConfig>,
    current_account_idx: usize,
    page_size: u32,
    browser: Option<String>,
    mouse: bool,
    sync_interval_secs: Option<u64>,
    archive_folder: String,
    deleted_folder: String,
    reply_from: Option<String>,
    rules_mailbox_regex: UserRegex,
    my_email_regex: UserRegex,
    retention_policies: Vec<RetentionPolicyConfig>,
    rules: Vec<CompiledRule>,
    custom_headers: Vec<String>,
    spam_config: SpamConfig,
    offline: bool,
) -> io::Result<()> {
    // Read before the backend thread is spawned and the window opened, so
    // an index the account list does not hold fails with nothing to close.
    let Some(first) = accounts.get(current_account_idx) else {
        return Err(io::Error::other(format!(
            "no account at index {} of {}",
            current_account_idx,
            accounts.len()
        )));
    };
    let first = first.clone();
    let setup = Setup {
        account_names: accounts.iter().map(|a| a.name.clone()).collect(),
        accounts,
        page_size,
        browser,
        reply_from,
        archive_folder,
        deleted_folder,
        retention_policies,
        sync_interval_secs,
        rules: Arc::new(rules),
        custom_headers: Arc::new(custom_headers),
        // Both patterns were compiled where the configuration was read.
        rules_mailbox_regex: Arc::new(rules_mailbox_regex),
        my_email_regex: Arc::new(my_email_regex),
        spam_config,
        offline,
        draft_dir: None,
        connector: (!offline).then(spawn_connector),
    };

    let endpoint = td_ui::wayland::endpoint(
        std::env::var_os("WAYLAND_SOCKET"),
        std::env::var_os("WAYLAND_DISPLAY"),
        std::env::var_os("XDG_RUNTIME_DIR"),
    )
    .map_err(io::Error::other)?;
    let stream = td_ui::wayland::connect(endpoint).map_err(io::Error::other)?;

    let (cmd_tx, resp_rx) = setup.open_account(&first, "startup");
    let mailbox_view = setup.mailbox_view(&cmd_tx, &first);
    let stack = ViewStack::new(Box::new(mailbox_view));
    let mut session =
        Session::new(setup, stack, cmd_tx, resp_rx, mouse).map_err(io::Error::other)?;
    let outcome = td_ui::window::run(&mut session, stream, std::env::temp_dir());
    let _ = session.cmd_tx.send(BackendCommand::Shutdown);
    // The backend answers what it held from the cache on the way out;
    // wait for it to go, up to two seconds of silence, so a mutation it
    // queues is written before the process ends. One mid-request is left
    // to the exit.
    let mut silent = 0;
    while silent < 20 {
        match session.resp_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(_) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => silent += 1,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    outcome.map_err(io::Error::other)
}

#[cfg(test)]
mod frame_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::config::PasswordSource;
    use crate::jmap::types::Mailbox;
    use td_ui::window::PointerPhase;

    fn account() -> AccountConfig {
        AccountConfig {
            name: "test".to_string(),
            well_known_url: String::new(),
            username: "me@example.com".to_string(),
            password: PasswordSource::Command("true".to_string()),
        }
    }

    fn setup() -> Setup {
        let regex = |pattern: &str| Arc::new(UserRegex::compile(pattern).unwrap());
        Setup {
            accounts: vec![account()],
            account_names: vec!["test".to_string()],
            page_size: 50,
            browser: None,
            reply_from: None,
            archive_folder: "Archive".to_string(),
            deleted_folder: "Trash".to_string(),
            retention_policies: Vec::new(),
            sync_interval_secs: None,
            rules: Arc::new(Vec::new()),
            custom_headers: Arc::new(Vec::new()),
            rules_mailbox_regex: regex("^INBOX$"),
            my_email_regex: regex("^$"),
            spam_config: SpamConfig {
                enabled: false,
                threshold: 0.9,
                ham_threshold: 0.2,
                min_training: 20,
            },
            offline: true,
            draft_dir: Some(std::env::temp_dir().join(format!(
                "td-mail-drafts-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ))),
            connector: None,
        }
    }

    /// The text of the pane's shown document.
    fn text(session: &Session) -> String {
        let tab = session.pane.tab().expect("a document");
        session
            .pane
            .editor()
            .document(tab)
            .unwrap()
            .text()
            .to_string()
    }

    fn mailbox(id: &str, name: &str, role: &str, total: u32, unread: u32) -> Mailbox {
        Mailbox {
            id: id.to_string(),
            name: name.to_string(),
            parent_id: None,
            role: Some(role.to_string()),
            total_emails: total,
            unread_emails: unread,
            sort_order: 0,
        }
    }

    /// A session on the mailbox list, with the backend's channel ends in
    /// the test's hands, holding three mailboxes, which the list orders
    /// by role: the inbox, the trash, the archive.
    fn session(mouse: bool) -> (Session, Receiver<BackendCommand>, Sender<BackendResponse>) {
        let setup = setup();
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (resp_tx, resp_rx) = mpsc::channel();
        let view = setup.mailbox_view(&cmd_tx, &account());
        let stack = ViewStack::new(Box::new(view));
        let mut session = Session::new(setup, stack, cmd_tx, resp_rx, mouse).unwrap();
        resp_tx
            .send(BackendResponse::Mailboxes(Ok(vec![
                mailbox("1", "INBOX", "inbox", 12, 3),
                mailbox("2", "Archive", "archive", 40, 0),
                mailbox("3", "Trash", "trash", 0, 0),
            ])))
            .unwrap();
        assert_eq!(session.poll(0), Flow::Continue);
        (session, cmd_rx, resp_tx)
    }

    fn key(session: &mut Session, chord: &str) {
        let input = Input::Key {
            chord,
            repeat: false,
        };
        session.input(input);
    }

    fn press(session: &mut Session, x: i64, y: i64) {
        for phase in [PointerPhase::Press, PointerPhase::Release] {
            session.input(Input::Pointer {
                phase,
                x,
                y,
                extend: false,
            });
        }
    }

    /// The frame reads back as text: the mailboxes with their counts,
    /// the title the header row was, then the folder opened by Return
    /// with the backend asked for its messages, then the help in the
    /// pane, read-only, where a chord the client does not claim edits
    /// nothing, and back.
    #[test]
    fn the_frame_shows_the_mailboxes_and_opens_one_and_the_help_in_the_pane() {
        let (mut session, cmd_rx, _resp_tx) = session(true);
        let text = session.shown();
        for expected in ["INBOX", "3/12", "Archive", "40", "Trash"] {
            assert!(text.contains(expected), "{expected}: {text}");
        }
        assert!(
            session
                .title()
                .starts_with("td-mail - Timmy's Mail Console (refreshed "),
            "{}",
            session.title()
        );
        assert!(text.contains("1/3"), "the status counts: {text}");
        key(&mut session, "Down");
        key(&mut session, "Return");
        assert_eq!(session.stack.depth(), 2);
        assert!(matches!(
            cmd_rx.try_recv(),
            Ok(BackendCommand::QueryEmails { mailbox_id, .. }) if mailbox_id == "3"
        ));
        let text = session.shown();
        assert!(session.title().starts_with("Trash"), "{}", session.title());
        assert!(text.contains("Loading emails"), "{text}");
        key(&mut session, "q");
        assert_eq!(session.stack.depth(), 1);
        // Delete-confirm's question is the title, read by the window
        // before it asks for the paint: it is current after the key.
        key(&mut session, "d");
        assert_eq!(session.title(), "Delete folder 'Trash'?");
        key(&mut session, "Escape");
        key(&mut session, "?");
        assert_eq!(session.stack.depth(), 2);
        assert_eq!(session.title(), "Help");
        let text = session.shown();
        assert!(text.contains("Mailbox List"), "{text}");
        let tab = session.pane.tab().expect("the pane holds the help");
        let document = session.pane.editor().document(tab).unwrap();
        assert!(document.read_only());
        let revision = document.revision();
        key(&mut session, "Tab");
        key(&mut session, "Delete");
        key(&mut session, "C-c");
        let document = session.pane.editor().document(tab).unwrap();
        assert_eq!(document.revision(), revision, "the pane is not edited");
        assert_eq!(session.pane.editor().tabs().count(), 1);
        // The caret's blink: a tick that changes the pane is a paint.
        session.dirty = false;
        for now in [100, 600, 1100] {
            session.poll(now);
        }
        assert!(session.dirty, "the caret blinked");
        key(&mut session, "q");
        assert_eq!(session.stack.depth(), 1);
        assert_eq!(
            session.pane.editor().tabs().count(),
            0,
            "closed with the view"
        );
        assert!(!session.quitting);
        key(&mut session, "q");
        assert!(session.quitting);
    }

    /// A subject of any length is a bounded title.
    #[test]
    fn the_title_is_bounded() {
        let (mut session, _cmd_rx, resp_tx) = session(true);
        let long = "s".repeat(70_000);
        resp_tx
            .send(BackendResponse::Mailboxes(Ok(vec![mailbox(
                "1", &long, "inbox", 1, 0,
            )])))
            .unwrap();
        session.poll(0);
        key(&mut session, "d");
        assert_eq!(session.title().chars().count(), TITLE_SCALARS);
        assert!(session.title().starts_with("Delete folder 'sss"));
        key(&mut session, "Escape");
        resp_tx
            .send(BackendResponse::Mailboxes(Ok(vec![mailbox(
                "1",
                "two\nlines\u{7}",
                "inbox",
                1,
                0,
            )])))
            .unwrap();
        session.poll(0);
        key(&mut session, "d");
        assert_eq!(session.title(), "Delete folder 'two lines '?");
    }

    /// A press on a row opens that mailbox once the frame has shown the
    /// selection, a bar label is its key, the wheel moves the selection
    /// by its rows, and with the mouse off the pointer is nothing.
    #[test]
    fn a_press_opens_the_row_under_it_a_bar_label_is_its_key_and_the_wheel_moves() {
        let (mut session, cmd_rx, resp_tx) = session(true);
        let layout = session.shape().unwrap().layout;
        let list = layout.list.expect("a list");
        let row = |index: usize| list.row(index).expect("row");
        press(&mut session, 10, row(2).y + 3);
        assert_eq!(
            session.stack.depth(),
            1,
            "the press selects; the open is pending"
        );
        // A response drained in the same turn does not lose the open.
        resp_tx
            .send(BackendResponse::MailboxMarkedRead {
                mailbox_id: "1".to_string(),
                mailbox_name: "INBOX".to_string(),
                updated: 0,
                result: Ok(()),
            })
            .unwrap();
        session.poll(0);
        assert_eq!(session.stack.depth(), 2);
        // The response asked for a refresh first; the open follows it.
        assert!(matches!(
            cmd_rx.try_recv(),
            Ok(BackendCommand::FetchMailboxes { .. })
        ));
        assert!(matches!(
            cmd_rx.try_recv(),
            Ok(BackendCommand::QueryEmails { mailbox_id, .. }) if mailbox_id == "2"
        ));
        session.shown();
        assert!(
            session.title().starts_with("Archive"),
            "{}",
            session.title()
        );
        // The email list's bar: its last label is "Back", the `q` key.
        let bar = session.shape().unwrap().layout.bar;
        let labels = session.stack.current().unwrap().scene().labels;
        let back = bar.header(labels.len() - 1).expect("back label");
        press(&mut session, back.x + 2, back.y + 2);
        assert_eq!(session.stack.depth(), 1);
        // Below the last row nothing is pressed.
        press(&mut session, 10, row(3).y + 3);
        session.poll(0);
        assert_eq!(session.stack.depth(), 1);
        // The wheel: two rows down from the first, then Return opens the
        // third; the page keys move by the rows the list shows.
        session.input(Input::Wheel {
            rows: 2,
            columns: 0,
        });
        key(&mut session, "Return");
        session.shown();
        assert!(
            session.title().starts_with("Archive"),
            "{}",
            session.title()
        );
        key(&mut session, "q");
        session.input(Input::Wheel {
            rows: -isize::MAX,
            columns: 0,
        });
        key(&mut session, "Return");
        session.shown();
        assert!(session.title().starts_with("INBOX"), "{}", session.title());
        key(&mut session, "q");
        assert_eq!(session.page(), list.rows());
        assert!(session.page() > 3, "{}", session.page());
        // The mouse off: a press and the wheel are nothing.
        session.mouse = false;
        press(&mut session, 10, row(2).y + 3);
        session.poll(0);
        assert_eq!(session.stack.depth(), 1);
        session.input(Input::Wheel {
            rows: 2,
            columns: 0,
        });
        key(&mut session, "Return");
        session.shown();
        assert!(session.title().starts_with("INBOX"), "{}", session.title());
    }

    /// A text entry: the folder name typed shows in the band, Escape
    /// leaves it; over the help, the wheel and the reading keys scroll
    /// the pane and Escape closes it.
    #[test]
    fn an_entry_shows_what_is_typed_and_a_text_scrolls_in_the_pane() {
        let (mut session, _cmd_rx, resp_tx) = session(true);
        key(&mut session, "+");
        // A `q` is a letter here, where the terminal's entry read it as
        // its cancel; Escape alone leaves.
        for chord in ["q", "u", "e", "u", "e"] {
            key(&mut session, chord);
        }
        let text = session.shown();
        assert!(text.contains("queue"), "{text}");
        assert!(session.shape().unwrap().layout.entry.is_some());
        assert_eq!(session.stack.depth(), 1);
        key(&mut session, "Escape");
        assert!(session.shape().unwrap().layout.entry.is_none());
        key(&mut session, "?");
        session.shown();
        let page = session.page();
        assert!(page > 1, "{page}");
        assert_eq!(session.pane.first_row(), Some(0));
        session.input(Input::Wheel {
            rows: 3,
            columns: 0,
        });
        assert_eq!(session.pane.first_row(), Some(3));
        key(&mut session, "PageDown");
        assert_eq!(session.pane.first_row(), Some(3 + page));
        key(&mut session, "n");
        assert_eq!(session.pane.first_row(), Some(4 + page));
        key(&mut session, "Home");
        assert_eq!(session.pane.first_row(), Some(0));
        // End is the caret's, along its line: the view stays.
        key(&mut session, "End");
        assert_eq!(session.pane.first_row(), Some(0));
        key(&mut session, "PageDown");
        key(&mut session, "PageUp");
        assert_eq!(session.pane.first_row(), Some(0));
        assert_eq!(session.pane.editor().tabs().count(), 1);
        key(&mut session, "Escape");
        assert_eq!(session.stack.depth(), 1);
        // A text over a text: the preview's rows arrive, the help opens
        // over it and closes, and a reading key right after, before any
        // paint, moves the preview, whose document the pane holds again.
        let candidates = (0..40)
            .map(|n| crate::backend::RetentionCandidate {
                id: n.to_string(),
                mailbox: "Trash".to_string(),
                policy: "trash".to_string(),
                received_at: "2026-01-01".to_string(),
                from: "a@example.com".to_string(),
                subject: format!("old {n}"),
            })
            .collect();
        resp_tx
            .send(BackendResponse::RetentionPreview {
                result: Ok(crate::backend::RetentionPreviewResult { candidates }),
            })
            .unwrap();
        session.poll(0);
        assert_eq!(session.stack.depth(), 2);
        assert_eq!(session.title(), "Retention expiry preview");
        key(&mut session, "j");
        assert_eq!(
            session.pane.first_row(),
            Some(1),
            "pushed: the pane's at once"
        );
        // The preview binds no help key; the push is the dispatcher's.
        session.act(ViewAction::Push(Box::new(views::help::HelpView::new())));
        assert_eq!(session.pane.editor().tabs().count(), 2);
        assert_eq!(session.title(), "Help");
        key(&mut session, "q");
        key(&mut session, "j");
        assert_eq!(
            session.pane.first_row(),
            Some(2),
            "popped: the pane's again"
        );
        assert_eq!(session.pane.editor().tabs().count(), 1);
    }

    /// `c` retains a draft and opens it in the pane, editable and
    /// auto-filled, where every chord types (a `q` too); Ctrl-S writes
    /// the pane's text over the file, as the Save label does; Ctrl-W
    /// pops a saved draft and asks about an unsaved one, where Escape
    /// returns to it, `y` saves and pops and `n` pops keeping the file
    /// as last saved, and the question takes no edits; a message's
    /// selection copied in its read-only pane pastes into the draft;
    /// a window too small for the pane still gives the view its own
    /// draft; and the window's close asks about an unsaved draft and
    /// closes on the answer.
    #[test]
    fn composing_edits_the_retained_draft_in_the_pane() {
        let (mut session, _cmd_rx, _resp_tx) = session(true);
        let draft_dir = session.setup.draft_dir.clone().unwrap();
        let path_of =
            |session: &Session| draft_dir.join(session.title.trim_start_matches("Draft "));
        key(&mut session, "c");
        assert_eq!(session.stack.depth(), 2);
        assert!(
            session.title.starts_with("Draft td-mail-draft-"),
            "{}",
            session.title
        );
        let tab = session.pane.tab().expect("the draft");
        let document = session.pane.editor().document(tab).unwrap();
        assert!(!document.read_only());
        assert!(document.auto_fill());
        let template = document.text().to_string();
        assert!(
            template.starts_with("From: me@example.com\nTo: \n"),
            "{template}"
        );
        let first = path_of(&session);
        assert_eq!(std::fs::read_to_string(&first).unwrap(), template);
        // Every chord is the pane's: q types rather than quits.
        for chord in ["C-End", "h", "i", "q"] {
            key(&mut session, chord);
        }
        assert_eq!(text(&session), format!("{template}hiq"));
        assert_eq!(session.stack.depth(), 2);
        assert_eq!(
            std::fs::read_to_string(&first).unwrap(),
            template,
            "not written yet"
        );
        // Close asks; Escape is back to the draft, whose keys are the pane's again.
        key(&mut session, "C-w");
        assert!(
            session.title.starts_with("Save td-mail-draft-"),
            "{}",
            session.title
        );
        assert_eq!(
            session.stack.current().unwrap().scene().labels,
            ["Save", "Discard", "Cancel"]
        );
        key(&mut session, "Escape");
        assert!(session.title.starts_with("Draft "), "{}", session.title);
        key(&mut session, "!");
        assert_eq!(text(&session), format!("{template}hiq!"));
        // Save writes the file; Close then pops the saved draft.
        key(&mut session, "C-s");
        assert_eq!(
            std::fs::read_to_string(&first).unwrap(),
            format!("{template}hiq!")
        );
        assert!(session
            .stack
            .current()
            .unwrap()
            .scene()
            .status
            .starts_with("Saved "));
        key(&mut session, "C-w");
        assert_eq!(session.stack.depth(), 1);
        assert_eq!(
            session.pane.editor().tabs().count(),
            0,
            "the draft's document closed"
        );
        // Discard keeps the file as it was last saved.
        key(&mut session, "c");
        let second = path_of(&session);
        assert_ne!(second, first);
        key(&mut session, "x");
        key(&mut session, "C-w");
        key(&mut session, "n");
        assert_eq!(session.stack.depth(), 1);
        assert_eq!(std::fs::read_to_string(&second).unwrap(), template);
        // Save from the question writes and pops.
        key(&mut session, "c");
        let third = path_of(&session);
        key(&mut session, "y");
        key(&mut session, "C-w");
        key(&mut session, "y");
        assert_eq!(session.stack.depth(), 1);
        assert_eq!(
            std::fs::read_to_string(&third).unwrap(),
            format!("y{template}")
        );
        // The bar's labels are the same requests.
        key(&mut session, "c");
        let fourth = path_of(&session);
        key(&mut session, "z");
        let bar = session.shape().unwrap().layout.bar;
        let save = bar.header(0).expect("save label");
        press(&mut session, save.x + 2, save.y + 2);
        assert_eq!(
            std::fs::read_to_string(&fourth).unwrap(),
            format!("z{template}")
        );
        let close = bar.header(1).expect("close label");
        press(&mut session, close.x + 2, close.y + 2);
        assert_eq!(session.stack.depth(), 1);
        // The question's labels answer it: Discard, pressed, pops with
        // the file as it was saved; a press in the pane meanwhile is
        // not the pane's.
        key(&mut session, "c");
        key(&mut session, "Z");
        press(&mut session, close.x + 2, close.y + 2);
        assert!(session.title.starts_with("Save "), "{}", session.title);
        let pane = session.shape().unwrap().layout.pane.expect("the pane");
        press(&mut session, pane.x + 4, pane.y + 4);
        assert!(session.title.starts_with("Save "), "still asking");
        let discard = session
            .shape()
            .unwrap()
            .layout
            .bar
            .header(1)
            .expect("discard label");
        press(&mut session, discard.x + 2, discard.y + 2);
        assert_eq!(session.stack.depth(), 1);
        assert_eq!(
            std::fs::read_to_string(&fourth).unwrap(),
            format!("z{template}"),
            "the fourth is unchanged"
        );
        // A selection copied in the help's read-only pane is pasted into a draft.
        key(&mut session, "?");
        key(&mut session, "C-a");
        key(&mut session, "C-c");
        assert_eq!(session.pane.editor().tabs().count(), 1);
        key(&mut session, "q");
        key(&mut session, "c");
        let fifth = path_of(&session);
        key(&mut session, "C-End");
        key(&mut session, "C-v");
        assert!(text(&session).contains("Timmy's Mail Console"));
        // The question takes no edits: a chord the client does not
        // claim is dropped, and a letter is not an answer.
        key(&mut session, "C-w");
        assert!(session.title.starts_with("Save "), "{}", session.title);
        let asked = text(&session);
        key(&mut session, "Tab");
        key(&mut session, "x");
        key(&mut session, "C-v");
        assert_eq!(text(&session), asked);
        key(&mut session, "Escape");
        // The window's close with the draft unsaved asks; Escape keeps
        // the window and the draft; a save closes it, the file written.
        assert_eq!(session.input(Input::Close), Flow::Continue);
        assert!(session.title.starts_with("Save "), "{}", session.title);
        key(&mut session, "Escape");
        assert!(session.title.starts_with("Draft "), "{}", session.title);
        assert!(!session.closing && !session.quitting);
        assert_eq!(session.input(Input::Close), Flow::Continue);
        assert_eq!(
            session.input(Input::Key {
                chord: "y",
                repeat: false
            }),
            Flow::Quit
        );
        assert!(std::fs::read_to_string(&fifth)
            .unwrap()
            .contains("Timmy's Mail Console"));
        // A window too small for the pane: the draft is still loaded,
        // the view's own, and a save writes it, not the text under it.
        let (mut small, _cmd_rx, _resp_tx) = self::session(true);
        key(&mut small, "?");
        small.input(Input::Resize(
            Surface::new(800, 40, Default::default()).unwrap(),
        ));
        assert!(small.shape().unwrap().layout.pane.is_none());
        key(&mut small, "q");
        key(&mut small, "c");
        let small_dir = small.setup.draft_dir.clone().unwrap();
        let sixth = small_dir.join(small.title.trim_start_matches("Draft "));
        assert_eq!(small.pane.editor().tabs().count(), 1, "the draft, loaded");
        key(&mut small, "w");
        key(&mut small, "C-s");
        assert_eq!(
            std::fs::read_to_string(&sixth).unwrap(),
            format!("w{template}")
        );
        // A clean draft lets the window close at once.
        assert_eq!(small.input(Input::Close), Flow::Quit);
        let _ = std::fs::remove_dir_all(&draft_dir);
        let _ = std::fs::remove_dir_all(&small_dir);
    }
}

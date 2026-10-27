//! The client in a window: td-ui's widget window drives the view stack
//! with the keys it reads from its chords, a press on a row, and the
//! wheel's travel; polls the backend's channel each turn; and presents
//! the frame the top view's scene lays out (`frame`): the toolkit's
//! action bar, text entry and list, and the editor core's document pane,
//! read-only for a message and editable for a draft. The window owns
//! the Wayland connection; the session owns the views, the pane, the
//! backend and the account, and the finder a draft's Attach opens over
//! the body.

pub mod frame;
pub mod input;
pub mod sidebar;
pub mod views;

use crate::attach;
use crate::backend::{self, BackendCommand, BackendResponse};
use crate::compose;
use crate::config::{AccountConfig, PasswordSource, RetentionPolicyConfig, SpamConfig};
use crate::regex::UserRegex;
use crate::rules::CompiledRule;
use crate::spam::SharedModel;
use frame::{Draft, Dropdown, Frame, Layout, Pane, Side};
use input::{Key, Menu};
use sidebar::{Entry, Sidebar};
use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};
use td_ui::editor::Outcome;
use td_ui::editor_model::TabId;
use td_ui::finder;
use td_ui::menus;
use td_ui::raster::{Raster, Surface};
use td_ui::window::{Clipboard, Flow, Handler, Input, PointerPhase, Refusal};
use views::compose::ComposeView;
use views::drafts::DraftsView;
use views::mailbox_list::{AccountSetup, MailboxListView};
use views::unified_inbox::{Inbox, UnifiedInboxView};
use views::{Body, Scene, Scroll, Slot, View, ViewAction, ViewStack};

/// The tenth of a second the terminal's read timed out at, kept as the
/// longest a turn waits for the backend's answer or the idle-sync clock.
const POLL_MS: u64 = 100;

/// The most scroll keys one wheel frame becomes, so a fling is bounded.
const WHEEL_EVENTS: usize = 64;

/// The cells `<` and `>` narrow and widen the account tree by.
const TREE_STEP: usize = 2;

/// The pixels, at scale one, on each side of the tree's edge that a press
/// takes to resize the tree.
const EDGE_GRAB: i64 = 4;

/// The most scalars of a view's title the window is given: a subject
/// is the title, and one of any length must fit the protocol's message.
const TITLE_SCALARS: usize = 256;

/// The chord that sends the draft being edited, Ctrl-Enter, which the
/// pane does not bind; every other chord is the pane's while editing.
const SEND_CHORD: &str = "C-Return";

/// The chord that opens the finder for a file to attach to the draft
/// being edited, Ctrl-Shift-A, which the pane does not bind either.
const ATTACH_CHORD: &str = "C-S-a";

/// How soon a second press on the finder's row must follow the first to
/// choose it, as Return does.
const DOUBLE_PRESS: Duration = Duration::from_millis(400);

/// Wait on a fire-and-forget child in a detached thread so it does not linger
/// as a zombie. `Child` has no reaping `Drop` impl, so a dropped handle leaks a
/// PID slot for the lifetime of td-mail. A thread the jail's task cap
/// refuses leaves the zombie, logged, rather than aborting the window.
pub fn reap_in_background(mut child: std::process::Child) {
    let spawned = std::thread::Builder::new().spawn(move || {
        let _ = child.wait();
    });
    if let Err(e) = spawned {
        crate::log_error!("no thread to reap a child: {}", e);
    }
}

/// What a session keeps of the configuration to open an account's views.
struct Setup {
    accounts: Vec<AccountConfig>,
    account_names: Vec<String>,
    page_size: u32,
    browser: Option<String>,
    /// Every account's policies and rules; each account's backend and
    /// views get those that apply to it.
    retention_policies: Vec<RetentionPolicyConfig>,
    sync_interval_secs: Option<u64>,
    rules: Vec<CompiledRule>,
    rules_mailbox_regex: Arc<UserRegex>,
    my_email_regex: Arc<UserRegex>,
    spam_config: SpamConfig,
    /// The model every account's backend scores and trains with.
    spam_model: SharedModel,
    offline: bool,
    /// Where drafts are retained: a test's directory, or none for the
    /// state directory the environment names, read as each draft is
    /// retained.
    draft_dir: Option<PathBuf>,
    /// None offline: no connection is ever asked for.
    connector: Option<Sender<ConnectRequest>>,
    /// The configuration the session was started from, which setting up
    /// the placeholder account writes; none in a test.
    config_path: Option<PathBuf>,
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
            account.mail.reply_from.clone(),
            self.browser.clone(),
            self.page_size,
            self.account_names.clone(),
            account.name.clone(),
            account.mail.archive_folder.clone(),
            account.mail.deleted_folder.clone(),
            crate::config::retention_for(&self.retention_policies, &account.name),
            self.sync_interval_secs,
        )
        .with_setup(AccountSetup {
            placeholder: account.placeholder(),
            portal: match &account.password {
                PasswordSource::Portal(name) => Some(name.clone()),
                PasswordSource::Command(_) => None,
            },
            server: account.well_known_url.clone(),
            username: account.username.clone(),
            online: !self.offline,
            refusal: if self.offline {
                Some("td-mail was started --offline".to_string())
            } else {
                match &self.config_path {
                    Some(path) => crate::config::editable(path, &account.name).err(),
                    None => Some("td-mail was given no configuration file".to_string()),
                }
            },
        })
    }

    /// Opens an account for the session: its backend starts with no
    /// connection, so the window opens at once on the mailbox list's
    /// loading state. The session offline fetches the mailboxes from the
    /// cache now; otherwise the connector is asked for the account's
    /// connection, which can take minutes to make, and the backend holds
    /// the fetch, and every command a view sends meanwhile, until the
    /// connection is decided, so nothing is answered from the cache that
    /// the connection then supersedes and the window never says the
    /// client is offline while it connects.
    fn open_account(&self, account: &AccountConfig, origin: &str) -> Result<Live, String> {
        let rules = crate::rules::for_account(&self.rules, &account.name);
        let custom_headers = crate::rules::extract_custom_headers(&rules);
        let (cmd_tx, resp_rx) = backend::spawn(
            None,
            !self.offline,
            true,
            account.name.clone(),
            Arc::new(rules),
            Arc::new(custom_headers),
            self.rules_mailbox_regex.clone(),
            self.my_email_regex.clone(),
            self.spam_config.clone(),
            self.spam_model.clone(),
        )?;
        let _ = cmd_tx.send(BackendCommand::FetchMailboxes {
            origin: origin.to_string(),
        });
        if !self.offline {
            self.ask_connection(account, &cmd_tx, false);
        }
        Ok(Live { cmd_tx, resp_rx })
    }

    /// Asks the connector for the account's connection, for the backend
    /// `cmd_tx` reaches; `urgent` puts it first in the connector's queue.
    /// A connector that is gone is the connection's failure, so the
    /// backend never holds for a decision nothing will send.
    fn ask_connection(
        &self,
        account: &AccountConfig,
        cmd_tx: &Sender<BackendCommand>,
        urgent: bool,
    ) {
        let request = ConnectRequest {
            account: account.clone(),
            cmd_tx: cmd_tx.clone(),
            urgent,
        };
        let sent = match &self.connector {
            Some(connector) => connector.send(request).is_ok(),
            None => false,
        };
        if !sent {
            crate::log_error!("[Connect] no connector for {}", account.name);
            let _ = cmd_tx.send(BackendCommand::ConnectFailed(
                "td-mail's connector is not running".to_string(),
            ));
        }
    }
}

/// The From an account's drafts and replies start with.
fn reply_from(account: &AccountConfig) -> String {
    account
        .mail
        .reply_from
        .clone()
        .unwrap_or_else(|| account.username.clone())
}

/// An account the session has open, at its place in `Setup::accounts`:
/// its backend's two channel ends, the same for the whole session, so a
/// view keeps the sender it was made with whatever is shown meanwhile.
struct Live {
    cmd_tx: Sender<BackendCommand>,
    resp_rx: Receiver<BackendResponse>,
}

/// A connection to make: the account, the backend it is for, and whether
/// someone is waiting on it (a reconnect, a setup) rather than the
/// session opening.
struct ConnectRequest {
    account: AccountConfig,
    cmd_tx: Sender<BackendCommand>,
    urgent: bool,
}

/// Queues `request` for the connector. It replaces one queued for the
/// same account, whose backend is the same for the session, in that
/// one's place; an urgent request goes first instead, so the account
/// shown waits on no other account's round trips.
fn enqueue(queue: &mut VecDeque<ConnectRequest>, request: ConnectRequest) {
    let at = queue
        .iter()
        .position(|queued| queued.account.name == request.account.name);
    if let Some(at) = at {
        queue.remove(at);
    }
    match (request.urgent, at) {
        (true, _) => queue.push_front(request),
        (false, Some(at)) => queue.insert(at, request),
        (false, None) => queue.push_back(request),
    }
}

/// The connector: the session's one thread for the credential and
/// discovery round trips, so one is in flight whatever the accounts (the
/// package caps the jail's tasks). It serves the queue `enqueue` keeps
/// and sends each backend its connection, or the failure, on which a
/// backend without a client answers what it held. A failure is not sent
/// while a newer request for the account is queued, so the backend's
/// hold lasts until that request's decision rather than ending on a
/// stale one. A connection that fails leaves the account on its cache,
/// logged; a reconnect asks again. The thread ends with the session,
/// which holds the sender.
fn spawn_connector() -> Result<Sender<ConnectRequest>, String> {
    let (tx, rx) = mpsc::channel::<ConnectRequest>();
    std::thread::Builder::new()
        .spawn(move || {
            let mut queue: VecDeque<ConnectRequest> = VecDeque::new();
            loop {
                if queue.is_empty() {
                    match rx.recv() {
                        Ok(request) => enqueue(&mut queue, request),
                        Err(_) => break,
                    }
                }
                for request in rx.try_iter() {
                    enqueue(&mut queue, request);
                }
                let Some(ConnectRequest {
                    account, cmd_tx, ..
                }) = queue.pop_front()
                else {
                    continue;
                };
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
                            "[Connect] {} failed: {}; on the cache until it is reconnected",
                            account.name,
                            e
                        );
                        for request in rx.try_iter() {
                            enqueue(&mut queue, request);
                        }
                        if queue.iter().any(|newer| newer.account.name == account.name) {
                            continue;
                        }
                        BackendCommand::ConnectFailed(e)
                    }
                };
                let _ = cmd_tx.send(decision);
            }
        })
        .map_err(|e| format!("no thread for the connector: {e}"))?;
    Ok(tx)
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

/// What the clipboard is asked while an input is handled, served with
/// the window's clipboard before the input returns, so a copy is made
/// at the press it answers, as the clipboard requires.
enum Ask {
    Copy(Arc<str>),
    /// A paste; `clipboard` is whether the window's may serve it, which a
    /// repeat's may not.
    Paste {
        clipboard: bool,
    },
}

/// The finder open over the body for a file to attach: the toolkit's
/// controller, the folder it lists, the draft it was opened for, and
/// the last press on an entry of its list, when and which, for a second
/// on the same entry.
struct Chooser {
    finder: finder::Controller,
    folder: PathBuf,
    tab: Option<TabId>,
    press: Option<(Instant, usize)>,
}

/// A drag the press began on the account tree's chrome, until release.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TreeDrag {
    /// The edge between the tree and the views: the tree's width follows.
    Edge,
    /// The scrollbar's thumb, held this many pixels below its top.
    Thumb(i64),
}

struct Session {
    setup: Setup,
    stack: ViewStack,
    pane: Pane,
    /// Every account's backend, by its place in `setup.accounts`, open
    /// for the whole session.
    live: Vec<Live>,
    /// The account the stack shows; the others' answers are drained,
    /// their folder lists kept by the tree, their backends connected.
    current: usize,
    /// The account tree beside the views, with two or more accounts.
    sidebar: Sidebar,
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
    /// The clipboard's requests from the input being handled.
    asks: Vec<Ask>,
    /// The draft a paste was asked for: the text arrives only into it,
    /// while it is still the one being edited.
    paste_target: Option<TabId>,
    /// What the clipboard last refused, or the window last noticed, in
    /// the status row until the next key or press.
    note: Option<String>,
    /// The dropdown a bar label opened, over the frame and taking every
    /// key and press until it activates one of the view's keys or is
    /// dismissed.
    menu: Option<Dropdown>,
    /// The finder a draft's Attach opened, over the body and taking
    /// every key and press until a file is chosen or it is closed.
    chooser: Option<Chooser>,
    /// The folder a file was last attached from, where the next finder
    /// opens.
    attach_folder: Option<PathBuf>,
    /// Whether the finder lists names beginning `.`, Ctrl-H's toggle,
    /// kept for the next finder.
    attach_hidden: bool,
    /// Where the window last said Control is held over: the pane
    /// underlines the link a Control-press there would follow.
    hover: Option<(i64, i64)>,
    /// Where the window last said the pointer is, no button held: the
    /// wheel scrolls what lies under it.
    pointer_at: Option<(i64, i64)>,
    /// The tree's edge or thumb being dragged.
    tree_drag: Option<TreeDrag>,
    /// The folders the tree asked to mark read, by account and id, until
    /// each answers.
    marking: Vec<(usize, String)>,
    /// A view's `?` asked for the window's key list, which the window
    /// takes after the input.
    show_keys: bool,
}

impl Session {
    /// A session over the accounts `live` holds open, one per account,
    /// showing the mailbox list of the one at `current`.
    fn new(setup: Setup, live: Vec<Live>, current: usize, mouse: bool) -> Result<Self, String> {
        let surface = Surface::new(800, 600, Default::default()).map_err(|e| e.to_string())?;
        let (Some(account), Some(open)) = (setup.accounts.get(current), live.get(current)) else {
            return Err(format!(
                "no account at index {} of {}",
                current,
                setup.accounts.len()
            ));
        };
        let stack = ViewStack::new(
            Box::new(setup.mailbox_view(&open.cmd_tx, account)),
            Some(current),
        );
        let sidebar = Sidebar::new(&setup.account_names, current);
        let mut session = Session {
            sync_interval: setup.sync_interval_secs.map(Duration::from_secs),
            setup,
            stack,
            pane: Pane::new()?,
            live,
            current,
            sidebar,
            mouse,
            surface,
            title: String::new(),
            last_user_activity: Instant::now(),
            last_idle_sync: Instant::now(),
            dirty: true,
            quitting: false,
            closing: false,
            asks: Vec::new(),
            paste_target: None,
            note: None,
            menu: None,
            chooser: None,
            attach_folder: None,
            attach_hidden: false,
            hover: None,
            pointer_at: None,
            tree_drag: None,
            marking: Vec::new(),
            show_keys: false,
        };
        // The window reads the title at binding, before any poll.
        session.refresh_title();
        Ok(session)
    }

    fn redraw(&mut self) {
        self.dirty = true;
    }

    /// Where the account tree goes on `surface`: with two or more
    /// accounts, a column beside the views, as wide as it was resized to,
    /// in a window three default trees wide, and over the body of a
    /// narrower one while the tree has the keys, which it never has while
    /// a draft is being edited.
    fn side_on(&self, surface: Surface) -> Side {
        if self.setup.accounts.len() < 2 {
            return Side::None;
        }
        let cell = td_ui::CELL_WIDTH * surface.scale.value();
        if surface.width >= 3 * sidebar::CELLS * cell {
            let cells = sidebar::fit(self.sidebar.cells, surface.width / cell);
            Side::Column((cells * cell) as u32)
        } else if self.sidebar.focused && !self.editing(true) {
            Side::Full
        } else {
            Side::None
        }
    }

    fn side(&self) -> Side {
        self.side_on(self.surface)
    }

    /// The tree is drawn and has the keys.
    fn tree_focused(&self) -> bool {
        self.sidebar.focused
            && !self.editing(true)
            && self
                .shape()
                .is_some_and(|shape| shape.layout.sidebar.is_some())
    }

    fn shape(&self) -> Option<Shape> {
        let view = self.stack.current()?;
        let scene = view.scene();
        let total = match &scene.body {
            Body::List { total, .. } => Some(*total),
            Body::Text { .. } | Body::Edit { .. } => None,
        };
        Some(Shape {
            layout: Layout::new(self.surface, &scene, self.side()),
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
            ViewAction::PushFor(account, new_view) => {
                self.stack.push_for(Some(account), new_view);
                self.prepare_frame();
                self.redraw();
            }
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
                    self.quit(if self.closing {
                        "the compositor asked the window to close; its draft is closed"
                    } else {
                        "the last view closed"
                    });
                    return;
                };
                self.pane.close(slot.text);
                // The draft the window's close asked about is closed:
                // the window follows.
                if self.closing {
                    self.quit("the compositor asked the window to close; its draft is closed");
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
            ViewAction::Quit => self.quit("q or the bar's Quit"),
            ViewAction::Compose(draft) => self.compose(&draft),
            ViewAction::Reconnect => {
                self.reconnect(self.current);
                // After the reconnect's ask, so the backend holds it until
                // the connection is decided; offline, the cache answers.
                if let Some(cmd_tx) = self.cmd_tx() {
                    let _ = cmd_tx.send(BackendCommand::FetchMailboxes {
                        origin: "reconnect".to_string(),
                    });
                }
                self.redraw();
            }
            ViewAction::SetUpAccount {
                account,
                server,
                username,
            } => self.set_up_account(&account, &server, &username),
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
            ViewAction::ChooseAttachment => self.open_chooser(),
            ViewAction::Drafts => self.drafts(),
            ViewAction::ShowKeys => self.show_keys = true,
        }
    }

    /// The top view's document in the pane: its draft, when it edits one.
    fn top_tab(&self) -> Option<TabId> {
        self.stack
            .top()
            .and_then(|slot| slot.text.as_ref().map(frame::Shown::tab))
    }

    /// Opens the finder over the body for a file to attach to the top
    /// view's draft, on the folder one was last attached from while it
    /// can be listed, else the start folder; one that cannot be listed,
    /// or a body that cannot hold the finder, is the view's `attach` with
    /// nothing and the status row's note.
    fn open_chooser(&mut self) {
        let Some(shape) = self.shape() else {
            return;
        };
        let hidden = self.attach_hidden;
        let remembered = self.attach_folder.clone().and_then(|folder| {
            match attach::list_folder(&folder, attach::CEILING, hidden) {
                Ok(listing) => Some((folder, listing)),
                Err(_) => {
                    self.attach_folder = None;
                    None
                }
            }
        });
        let (folder, listed) = match remembered {
            Some((folder, listing)) => (folder, Ok(listing)),
            None => {
                let folder = attach::start_folder();
                let listed = attach::list_folder(&folder, attach::CEILING, hidden);
                (folder, listed)
            }
        };
        let opened = listed.and_then(|listing| {
            finder::Controller::new(
                listing,
                finder::Choose::File,
                self.surface,
                shape.layout.body,
                None,
            )
            .map_err(|e| format!("the window cannot show the finder: {e}"))
        });
        let tab = self.top_tab();
        match opened {
            Ok(finder) => {
                // The pointer is the finder's now; a drag cannot go on.
                if self.pane.drag {
                    self.pane.cancel_pointer();
                }
                self.chooser = Some(Chooser {
                    finder,
                    folder,
                    tab,
                    press: None,
                });
                self.redraw();
            }
            Err(why) => {
                self.attach_chosen(tab, None);
                self.note(format!("attach: {why}"));
            }
        }
    }

    /// What the finder was closed on, to the view whose draft it was
    /// opened for while that draft is still the one shown; dropped with
    /// a note otherwise.
    fn attach_chosen(&mut self, tab: Option<TabId>, chosen: Option<&Path>) {
        self.redraw();
        if tab != self.top_tab() {
            self.note("attach dropped: the draft it was chosen for is not the one shown".into());
            return;
        }
        let Session { stack, pane, .. } = self;
        let mut draft = Draft::new(pane, tab);
        let action = stack
            .current_mut()
            .map(|view| view.attach(chosen, &mut draft));
        self.hold_draft();
        if let Some(action) = action {
            self.act(action);
        }
    }

    /// Closes the finder when the draft it was opened for is no longer
    /// the one shown, with a note.
    fn drop_stale_chooser(&mut self) {
        if self
            .chooser
            .as_ref()
            .is_some_and(|chooser| chooser.tab != self.top_tab())
        {
            self.chooser = None;
            self.note("attach closed: the draft it was opened for is not the one shown".into());
        }
    }

    /// An input while the finder is open, which is the finder's: its keys
    /// as the chord names them (Return opens a folder or chooses the
    /// file, Ctrl-Return chooses it, Backspace on an empty filter, Alt-Up
    /// and `^` go up, Ctrl-H shows or hides names beginning `.`, Escape
    /// closes it, and a single printable character filters) and every
    /// other chord consumed; the pointer's press, move
    /// and release, a second press on the same row of its list soon after
    /// the first choosing it as Return does; and the wheel over its list;
    /// with the mouse off, the pointer and the wheel are consumed unread.
    /// A resize lays it out again and is the frame's too; the window's
    /// close closes it with nothing chosen and is then the session's; a
    /// focus change, a paste and a pointer cancel are not the finder's.
    /// True when the input went no further.
    fn chooser_input(&mut self, input: &Input<'_>) -> bool {
        self.drop_stale_chooser();
        let Some(list) = self.chooser.as_ref().map(|c| c.finder.list_rect()) else {
            return false;
        };
        let key = |key, repeated| finder::Event::Key { key, repeated };
        let event =
            match *input {
                Input::Key { chord, repeat } => match chord {
                    "Up" => key(finder::Key::Up, repeat),
                    "Down" => key(finder::Key::Down, repeat),
                    "PageUp" => key(finder::Key::PageUp, repeat),
                    "PageDown" => key(finder::Key::PageDown, repeat),
                    "Home" => key(finder::Key::Home, repeat),
                    "End" => key(finder::Key::End, repeat),
                    "Return" => key(finder::Key::Activate, repeat),
                    "C-Return" => key(finder::Key::Accept, repeat),
                    "Backspace" => key(finder::Key::Backspace, repeat),
                    "M-Up" | "^" => key(finder::Key::Parent, repeat),
                    "Escape" => key(finder::Key::Escape, repeat),
                    "Space" => finder::Event::Insert(' '),
                    "C-h" => {
                        if !repeat {
                            self.last_user_activity = Instant::now();
                            self.toggle_hidden();
                        }
                        return true;
                    }
                    _ => {
                        let mut chars = chord.chars();
                        match (chars.next(), chars.next()) {
                            (Some(c), None) if !c.is_control() => finder::Event::Insert(c),
                            _ => finder::Event::Other,
                        }
                    }
                },
                Input::Pointer { phase, x, y, .. } => match phase {
                    PointerPhase::Press => finder::Event::Press { x, y },
                    PointerPhase::Move => finder::Event::Move { x, y },
                    PointerPhase::Release => finder::Event::Release { x, y },
                },
                Input::Wheel { rows, .. } => finder::Event::Wheel {
                    x: list.x,
                    y: list.y,
                    rows,
                },
                Input::Resize(surface) => {
                    let Some(rect) = self.stack.current().map(|view| {
                        Layout::new(surface, &view.scene(), self.side_on(surface)).body
                    }) else {
                        return false;
                    };
                    finder::Event::Resize { surface, rect }
                }
                Input::Close => {
                    if let Some(chooser) = self.chooser.take() {
                        self.attach_chosen(chooser.tab, None);
                    }
                    return false;
                }
                Input::Focus(_)
                | Input::Paste(_)
                | Input::PasteImage { .. }
                | Input::CancelPointer
                | Input::Hover(_)
                | Input::Context { .. } => return false,
            };
        let pointed = matches!(input, Input::Pointer { .. } | Input::Wheel { .. });
        if pointed && !self.mouse {
            return true;
        }
        let surface = self.surface;
        if matches!(
            input,
            Input::Key { .. } | Input::Pointer { .. } | Input::Wheel { .. }
        ) {
            self.last_user_activity = Instant::now();
        }
        let Some(chooser) = self.chooser.as_mut() else {
            return false;
        };
        // The entry a press lands on, by the listing's index, which a
        // filter typed between two presses does not move; a press off
        // the rows lands on none.
        let hit = match event {
            finder::Event::Press { x, y } => td_ui::chrome::List::new(surface, list)
                .and_then(|rows| rows.hit(x, y))
                .and_then(|row| chooser.finder.first().checked_add(row))
                .and_then(|position| chooser.finder.shown().get(position).copied()),
            _ => None,
        };
        let mut outcome = chooser.finder.event(event);
        match event {
            finder::Event::Press { .. } => {
                let now = Instant::now();
                let again = hit.is_some()
                    && chooser.press.is_some_and(|(at, entry)| {
                        Some(entry) == hit && now.duration_since(at) <= DOUBLE_PRESS
                    });
                chooser.press = if again {
                    None
                } else {
                    hit.map(|entry| (now, entry))
                };
                if again {
                    outcome = chooser.finder.event(key(finder::Key::Activate, false));
                }
            }
            finder::Event::Move { .. } | finder::Event::Release { .. } => {}
            _ => chooser.press = None,
        }
        self.chooser_outcome(outcome);
        !matches!(input, Input::Resize(_))
    }

    /// What the finder made of an input: a folder to list, its parent to
    /// list with the folder it came from selected, or its close, a file
    /// chosen or nothing, which is the view's.
    fn chooser_outcome(&mut self, outcome: finder::Outcome) {
        match outcome {
            finder::Outcome::Ignored | finder::Outcome::Consumed => {}
            finder::Outcome::Changed => self.redraw(),
            finder::Outcome::Descend(index) => {
                let Some(folder) = self.chooser.as_ref().and_then(|chooser| {
                    let entry = chooser.finder.listing().entries().get(index)?;
                    Some(chooser.folder.join(entry.name()))
                }) else {
                    return;
                };
                self.chooser_list(folder, None);
            }
            finder::Outcome::Ascend => {
                let Some((parent, from)) = self.chooser.as_ref().and_then(|chooser| {
                    let parent = chooser.folder.parent()?.to_path_buf();
                    let from = chooser.folder.file_name()?.to_str()?.to_string();
                    Some((parent, from))
                }) else {
                    return;
                };
                self.chooser_list(parent, Some(&from));
            }
            finder::Outcome::Closed(choice) => {
                let Some(chooser) = self.chooser.take() else {
                    return;
                };
                match choice {
                    finder::Choice::Entry(index) => {
                        let chosen = chooser
                            .finder
                            .listing()
                            .entries()
                            .get(index)
                            .map(|entry| chooser.folder.join(entry.name()));
                        self.attach_folder = Some(chooser.folder);
                        self.attach_chosen(chooser.tab, chosen.as_deref());
                    }
                    finder::Choice::Unavailable(error) => {
                        self.attach_chosen(chooser.tab, None);
                        self.note(format!(
                            "attach: the window cannot show the finder: {error}"
                        ));
                    }
                    finder::Choice::Here | finder::Choice::Cancelled => {
                        self.attach_chosen(chooser.tab, None);
                    }
                }
            }
        }
    }

    /// Ctrl-H in the finder: names beginning `.` shown when they were
    /// left out and left out when shown, here and in the next finder,
    /// the folder listed again with the selected entry kept when it is
    /// still listed, and the status row saying which; a folder that
    /// cannot be listed again leaves the setting as it was, the reason
    /// in the status row.
    fn toggle_hidden(&mut self) {
        let Some((folder, select)) = self.chooser.as_ref().map(|chooser| {
            let select = chooser
                .finder
                .selected_entry()
                .map(|entry| entry.name().to_string());
            (chooser.folder.clone(), select)
        }) else {
            return;
        };
        self.attach_hidden = !self.attach_hidden;
        if !self.chooser_list(folder, select.as_deref()) {
            self.attach_hidden = !self.attach_hidden;
            return;
        }
        let note = if self.attach_hidden {
            "Hidden files shown; Ctrl-H hides them"
        } else {
            "Hidden files left out; Ctrl-H shows them"
        };
        if let Some(chooser) = self.chooser.as_mut() {
            if let Err(e) = chooser.finder.set_note(note) {
                crate::log_error!("finder note: {}", e);
            }
        }
    }

    /// The finder shows `folder`, `select` selected when listed; a folder
    /// that cannot be listed leaves it where it was, the reason in its
    /// status row. Whether it was listed.
    fn chooser_list(&mut self, folder: PathBuf, select: Option<&str>) -> bool {
        let listed = attach::list_folder(&folder, attach::CEILING, self.attach_hidden);
        let Some(chooser) = self.chooser.as_mut() else {
            return false;
        };
        let installed = listed.and_then(|listing| {
            chooser
                .finder
                .set_listing(listing, select)
                .map_err(|e| format!("{}: {e}", folder.display()))
        });
        let listed = match installed {
            Ok(()) => {
                chooser.folder = folder;
                true
            }
            Err(why) => {
                if let Err(e) = chooser.finder.set_note(&fit_note(&why)) {
                    crate::log_error!("finder note: {}", e);
                }
                false
            }
        };
        chooser.press = None;
        self.redraw();
        listed
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
        let account = self.draft_account();
        let Some(cmd_tx) = self.live.get(account).map(|live| live.cmd_tx.clone()) else {
            return;
        };
        match ComposeView::open(prepared.draft_path, prepared.attachment_dir, cmd_tx) {
            Ok(view) => self.act(ViewAction::PushFor(account, Box::new(view))),
            Err(e) => {
                crate::log_error!("Failed to read the retained draft back: {}", e);
                self.redraw();
            }
        }
    }

    /// The drafts retained where `compose` retains them, and those sent
    /// beside them, listed; a directory the environment does not name is
    /// logged and nothing opens.
    fn drafts(&mut self) {
        let dir = match &self.setup.draft_dir {
            Some(dir) => Ok(dir.clone()),
            None => compose::draft_dir(),
        };
        let account = self.draft_account();
        let Some(cmd_tx) = self.live.get(account).map(|live| live.cmd_tx.clone()) else {
            return;
        };
        match dir {
            Ok(dir) => self.act(ViewAction::PushFor(
                account,
                Box::new(DraftsView::new(dir, cmd_tx)),
            )),
            Err(e) => {
                crate::log_error!("No drafts to list: {}", e);
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

    /// The close the window asked is over once the draft it asked about
    /// is the pane's again, kept: the question answered so, or the send
    /// refused and the draft handed back. The next close asks again. A
    /// draft still with the backend keeps the close waiting.
    fn settle_close(&mut self) {
        let waiting = self.stack.current().is_some_and(|view| view.waiting());
        if self.closing && !self.quitting && !waiting && self.editing(false) {
            self.closing = false;
        }
    }

    /// The top view's draft is held read-only while the view says so:
    /// while it waits on the backend for it, and once the server has
    /// taken it, so the file sent is the file retired; the pane keeps
    /// its keys, and its save and close requests still reach the view.
    /// Only a draft is held: a text shown read-only stays so.
    fn hold_draft(&mut self) {
        let Some(view) = self.stack.current() else {
            return;
        };
        if !matches!(view.scene().body, Body::Edit { .. }) {
            return;
        }
        let held = view.held();
        self.draft().hold(held);
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

    /// A request of the pane's kind, from its chord or a bar label: a
    /// cut or a copy is the kill ring's and then the clipboard's, so a
    /// paste after either brings the same text back whichever serves it;
    /// a paste is the clipboard's or the kill ring's once the clipboard
    /// is in hand; and the rest are the view's, with its draft in hand.
    fn request(&mut self, name: &str) {
        let changed = match name {
            "cut" | "copy" => {
                let kept = if name == "cut" {
                    self.pane.cut()
                } else {
                    self.pane.copy()
                };
                match kept {
                    Ok(true) => {
                        if let Some(text) = self.pane.killed() {
                            self.asks.push(Ask::Copy(text));
                        }
                        name == "cut"
                    }
                    Ok(false) => false,
                    Err(why) => {
                        self.note(format!("{name} refused: {why}"));
                        false
                    }
                }
            }
            "paste" => {
                if self.pane.editable() {
                    self.asks.push(Ask::Paste { clipboard: true });
                }
                false
            }
            _ => {
                let Session { stack, pane, .. } = self;
                let tab = stack
                    .top()
                    .and_then(|slot| slot.text.as_ref().map(frame::Shown::tab));
                let mut draft = Draft::new(pane, tab);
                let action = stack
                    .current_mut()
                    .map(|view| view.request(name, &mut draft));
                self.hold_draft();
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

    /// The clipboard's requests the input raised, served while the
    /// input is still being delivered: a copy is offered as the
    /// selection, which the kill ring keeps either way, so a compositor
    /// without a clipboard is no refusal; a paste asks the clipboard for
    /// its text when it offers one, which arrives as `Input::Paste` for
    /// the draft shown now, and the kill ring's otherwise. What the
    /// clipboard refused is the status row's note.
    fn serve(&mut self, clipboard: &mut dyn Clipboard) {
        for ask in std::mem::take(&mut self.asks) {
            match ask {
                Ask::Copy(text) => match clipboard.copy(text) {
                    Ok(()) | Err(Refusal::NoDevice) => {}
                    Err(refusal) => self.note(format!("copy kept in td-mail only: {refusal}")),
                },
                Ask::Paste { clipboard: asked } => {
                    if asked && clipboard.has_text() {
                        match clipboard.paste() {
                            Ok(()) => self.paste_target = self.pane.tab(),
                            Err(refusal) => self.note(format!("paste refused: {refusal}")),
                        }
                    } else {
                        match self.pane.paste() {
                            Ok(true) => self.redraw(),
                            Ok(false) => {}
                            Err(why) => self.note(format!("paste refused: {why}")),
                        }
                    }
                }
            }
        }
    }

    fn note(&mut self, text: String) {
        self.note = Some(text);
        self.redraw();
    }

    /// The top view's scene, its status row the clipboard's note while
    /// one is up.
    fn scene<'a>(&'a self, slot: &'a Slot) -> Scene<'a> {
        let mut scene = slot.view.scene();
        if self.tree_focused() {
            // Over the whole body the tree has no width to change; the
            // narrow window's status row shows 48 cells of the rest.
            let width = if self.side() == Side::Full {
                ""
            } else {
                " </>:width"
            };
            scene.status = format!("Accounts | n/p RET:open u:mark read{width} Tab:back");
        }
        if let Some(note) = &self.note {
            scene.status = note.clone();
        }
        scene
    }

    /// A chord to the pane, and what it asked for served.
    fn pane_chord(&mut self, chord: &str) {
        match self.pane.chord(chord) {
            Outcome::Changed => self.redraw(),
            Outcome::Request { name, .. } => self.request(name),
            Outcome::Created(_) | Outcome::Prefix | Outcome::Ignored => {}
        }
    }

    /// The account the top view is bound to, or over every account, the
    /// account last shown: whose a draft composed or replied to there is,
    /// and whose backend sends it.
    fn draft_account(&self) -> usize {
        self.stack.top_account().unwrap_or(self.current)
    }

    /// The backend of `draft_account`.
    fn cmd_tx(&self) -> Option<Sender<BackendCommand>> {
        self.live
            .get(self.draft_account())
            .map(|live| live.cmd_tx.clone())
    }

    /// The mailbox list of the account at `index`, and its folders'
    /// fetch sent; `seeded`, it shows the folders the tree holds until
    /// the fetch answers, which a set-up does not, its old folders being
    /// another server's. Its backend stays as it is.
    fn account_view(&self, index: usize, origin: &str, seeded: bool) -> Option<MailboxListView> {
        let account = self.setup.accounts.get(index)?;
        let live = self.live.get(index)?;
        let mut view = self.setup.mailbox_view(&live.cmd_tx, account);
        if let Some(folders) = self.sidebar.folders(index).filter(|_| seeded) {
            view.seed(folders);
        }
        let _ = live.cmd_tx.send(BackendCommand::FetchMailboxes {
            origin: origin.to_string(),
        });
        Some(view)
    }

    /// The stack replaced by `view`, bound to the account at `index`, or
    /// over every account.
    fn replace_stack(&mut self, index: Option<usize>, view: Box<dyn View>) {
        if let Some(index) = index {
            self.current = index;
        }
        let old = std::mem::replace(&mut self.stack, ViewStack::new(view, index));
        for slot in old.into_slots() {
            self.pane.close(slot.text);
        }
        self.last_idle_sync = Instant::now();
        self.redraw();
    }

    /// The stack replaced by the mailbox list of the account at `index`,
    /// unseeded: a set-up's, whose old folders are another server's.
    fn show_account(&mut self, index: usize, origin: &str) {
        if let Some(view) = self.account_view(index, origin, false) {
            self.replace_stack(Some(index), Box::new(view));
        }
    }

    /// The account at `index`'s page, and the folder `id` opened over it
    /// as Return on its row opens it, so `q` from the folder is the
    /// page. The account shown keeps its page, popped to, with what it
    /// caches; another's replaces the stack, seeded from the tree.
    fn open_page(&mut self, index: usize, id: Option<&str>) {
        if index == self.current && self.stack.root_account() == Some(index) {
            for slot in self.stack.pop_to_root() {
                self.pane.close(slot.text);
            }
            // The page is refreshed as a pop reveals it, its cached
            // folders marked stale, so the folder opened over it asks.
            if let Some(root) = self.stack.root_mut() {
                root.on_reveal();
            }
        } else {
            let Some(view) = self.account_view(index, "sidebar", true) else {
                return;
            };
            self.replace_stack(Some(index), Box::new(view));
        }
        let opened = id.and_then(|id| self.stack.root_mut()?.open_folder(id));
        if let Some(action) = opened {
            self.act(action);
        }
        self.redraw();
    }

    /// Opens a tree entry in place of the views, an account's page or a
    /// folder over it, and gives the views the keys. A draft open on the
    /// stack keeps it, so nothing being written is closed unasked.
    fn open_entry(&mut self, entry: Entry) {
        if self.stack.has_draft() {
            self.note("Not opened: close the draft first".to_string());
            return;
        }
        self.sidebar.select(entry.clone());
        self.sidebar.focused = false;
        match entry {
            // Shown already, it is kept and refreshed as a pop reveals it.
            Entry::Unified if self.stack.root_account().is_none() => {
                for slot in self.stack.pop_to_root() {
                    self.pane.close(slot.text);
                }
                if let Some(root) = self.stack.root_mut() {
                    root.on_reveal();
                }
                self.redraw();
            }
            Entry::Unified => self.open_unified(),
            Entry::Account(index) => self.open_page(index, None),
            Entry::Folder { account, id } => self.open_page(account, Some(&id)),
        }
    }

    /// The stack replaced by the unified inbox over every account, each
    /// account's inbox found among the folders the tree holds or, not
    /// yet listed, in its next listing. A draft composed there is from
    /// the account last shown.
    fn open_unified(&mut self) {
        let inboxes = self
            .setup
            .accounts
            .iter()
            .zip(&self.live)
            .enumerate()
            .map(|(index, (account, live))| Inbox {
                account: index,
                name: account.name.clone(),
                cmd_tx: live.cmd_tx.clone(),
                reply_from: reply_from(account),
                archive_folder: account.mail.archive_folder.clone(),
                deleted_folder: account.mail.deleted_folder.clone(),
                mailboxes: self
                    .sidebar
                    .folders(index)
                    .map(<[_]>::to_vec)
                    .unwrap_or_default(),
            })
            .collect();
        let (compose_name, compose_from) = self
            .setup
            .accounts
            .get(self.current)
            .map(|account| (account.name.clone(), reply_from(account)))
            .unwrap_or_default();
        let view = UnifiedInboxView::new(
            inboxes,
            self.setup.page_size,
            self.setup.browser.clone(),
            compose_name,
            compose_from,
        );
        self.replace_stack(None, Box::new(view));
    }

    /// A key while the tree has the keys: its moves, and the entry
    /// opened; whether it took the key, any other being the view's.
    fn tree_key(&mut self, key: Key) -> bool {
        let rows = self
            .shape()
            .and_then(|shape| shape.layout.sidebar)
            .map_or(frame::PAGE_ROWS, |list| list.rows().max(1)) as isize;
        match key {
            Key::Char('n' | 'j') | Key::Down => self.sidebar.move_by(1),
            Key::Char('p' | 'k') | Key::Up => self.sidebar.move_by(-1),
            Key::PageDown => self.sidebar.move_by(rows),
            Key::PageUp => self.sidebar.move_by(-rows),
            Key::Home => self.sidebar.move_by(isize::MIN),
            Key::End => self.sidebar.move_by(isize::MAX),
            Key::Enter | Key::Char(' ') => self.open_entry(self.sidebar.selected()),
            Key::Escape | Key::Char('q') => self.sidebar.focused = false,
            Key::Char('<') => self.resize_tree(self.tree_cells().saturating_sub(TREE_STEP)),
            Key::Char('>') => self.resize_tree(self.tree_cells().saturating_add(TREE_STEP)),
            Key::Char('u') => self.mark_tree_read(),
            _ => return false,
        }
        self.redraw();
        true
    }

    /// The tree's width in cells as the window shows it.
    fn tree_cells(&self) -> usize {
        let cell = td_ui::CELL_WIDTH * self.surface.scale.value();
        sidebar::fit(self.sidebar.cells, self.surface.width / cell)
    }

    /// The tree resized to `cells`, held to what the window can show; a
    /// tree over the whole body has no width to change, and says so.
    fn resize_tree(&mut self, cells: usize) {
        if !matches!(self.side(), Side::Column(_)) {
            self.note("Widen the window to resize the tree".to_string());
            return;
        }
        let cell = td_ui::CELL_WIDTH * self.surface.scale.value();
        self.sidebar.cells = sidebar::fit(cells, self.surface.width / cell);
        self.redraw();
    }

    /// Marks every message in the folder selected in the tree read, as
    /// `u` on its account's page does; the tree's counts follow the
    /// answer.
    fn mark_tree_read(&mut self) {
        let Entry::Folder { account, id } = self.sidebar.selected() else {
            self.note("Select a folder to mark read".to_string());
            return;
        };
        let Some(folder) = self
            .sidebar
            .folders(account)
            .and_then(|folders| folders.iter().find(|f| f.id == id))
        else {
            return;
        };
        let name = folder.name.replace(['\n', '\r'], " ");
        if folder.unread_emails == 0 {
            self.note(format!("Folder '{name}' already read"));
            return;
        }
        let Some(live) = self.live.get(account) else {
            return;
        };
        let sent = live.cmd_tx.send(BackendCommand::MarkMailboxRead {
            mailbox_id: id.clone(),
            mailbox_name: folder.name.clone(),
        });
        match sent {
            Ok(()) => {
                if !self.marking.contains(&(account, id.clone())) {
                    self.marking.push((account, id));
                }
                self.note(format!("Marking folder '{name}' read..."));
            }
            Err(e) => self.note(format!("Mark folder read failed to send: {e}")),
        }
    }

    /// An account's answer to a folder marked read: the tree's own asks
    /// are said in the status row, and the account's folders are asked
    /// for again so its counts follow, unless its page is on the stack,
    /// which asks for them itself.
    fn marked_read(&mut self, account: usize, response: &BackendResponse) {
        let BackendResponse::MailboxMarkedRead {
            mailbox_id,
            mailbox_name,
            updated,
            result,
        } = response
        else {
            return;
        };
        let asked = self.marking.len();
        self.marking
            .retain(|(a, id)| !(*a == account && id == mailbox_id));
        if self.marking.len() != asked {
            let name = mailbox_name.replace(['\n', '\r'], " ");
            self.note(match result {
                Ok(()) if *updated == 0 => format!("Folder '{name}' already read"),
                Ok(()) => format!("Marked {updated} message(s) read in '{name}'"),
                Err(e) => format!("Mark all read failed for '{name}': {e}"),
            });
        }
        if result.is_ok() && self.stack.root_account() != Some(account) {
            if let Some(live) = self.live.get(account) {
                let _ = live.cmd_tx.send(BackendCommand::FetchMailboxes {
                    origin: "sidebar.marked_read".to_string(),
                });
            }
        }
    }

    /// Asks the connector for the account at `index`'s connection again,
    /// first in its queue, for its backend: one without a client holds
    /// what it is sent until the connection is decided, one with a client
    /// goes on serving with it and keeps it if the new one fails. Whether
    /// one was asked for: a session started offline asks for none.
    fn reconnect(&mut self, index: usize) -> bool {
        if self.setup.offline {
            self.note("Not reconnected: td-mail was started --offline".to_string());
            return false;
        }
        let (Some(account), Some(live)) = (self.setup.accounts.get(index), self.live.get(index))
        else {
            return false;
        };
        let _ = live.cmd_tx.send(BackendCommand::Connecting);
        self.setup.ask_connection(account, &live.cmd_tx, true);
        true
    }

    /// Writes the server and address the setup form was given into the
    /// configuration for the account, and opens the account again as set
    /// up, which connects it. A refusal leaves the file and the account as
    /// they were and says why in the status row.
    fn set_up_account(&mut self, account: &str, server: &str, username: &str) {
        let current = self.setup.accounts.iter().find(|a| a.name == account);
        let result = match (&self.setup.config_path, current) {
            (Some(path), Some(current)) => {
                crate::config::set_up_account(path, current, server, username)
            }
            (None, _) => Err("no configuration file to write".to_string()),
            (_, None) => Err(format!("no account {account} in this session")),
        };
        match result {
            Ok(updated) => {
                crate::log_info!(
                    "[Setup] {} set up for {}",
                    updated.name,
                    updated.well_known_url
                );
                let Some(index) = self.setup.accounts.iter().position(|a| a.name == account) else {
                    return;
                };
                if let Some(slot) = self.setup.accounts.get_mut(index) {
                    *slot = updated;
                }
                // The connection is asked for first, so the list's fetch
                // waits for it rather than answering as the placeholder.
                self.reconnect(index);
                self.show_account(index, "set_up_account");
            }
            Err(e) => {
                crate::log_error!("[Setup] {} not set up: {}", account, e);
                self.note = Some(format!("Not set up: {e}"));
                self.redraw();
            }
        }
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
        if let Key::Menu(menu) = key {
            self.open_menu(menu);
            return;
        }
        if key == Key::ShowKeys {
            self.act(ViewAction::ShowKeys);
            return;
        }
        // Any other key gives the views the keys back and is theirs, so a
        // form or a draft it opens has them.
        if self.tree_focused() {
            if self.tree_key(key) {
                return;
            }
            self.sidebar.focused = false;
            self.redraw();
        }
        self.view_key(key);
    }

    /// A key to the top view, whichever side has the keys, as the wheel
    /// over the views gives them.
    fn view_key(&mut self, key: Key) {
        let page = self.page();
        match self.stack.handle_key(key, page) {
            Some(action) => self.act(action),
            None => self.quit("a key with no view open"),
        }
    }

    /// A chord: every one is the pane's while a draft is edited, but
    /// Ctrl-Enter, which sends it, and none is while its view asks about
    /// it, when only the client's keys reach the view; otherwise Tab,
    /// with two or more accounts, moves the keys to the account tree or
    /// back, then the client's key when it names one, and else the
    /// pane's, when a text is shown, so a chord the client does not claim
    /// (an arrow with Shift, a copy) reaches the document.
    fn chord(&mut self, chord: &str) {
        if self.editing(false) {
            self.last_user_activity = Instant::now();
            if chord == SEND_CHORD {
                self.request("send");
            } else if chord == ATTACH_CHORD {
                self.request("attach");
            } else {
                self.pane_chord(chord);
            }
            return;
        }
        if self.editing(true) {
            if let Some(key) = input::key(chord) {
                self.key(key);
            }
            return;
        }
        // Tab moves the keys between the views and the account tree.
        if chord == "Tab" && self.setup.accounts.len() > 1 {
            self.last_user_activity = Instant::now();
            self.sidebar.focused = !self.sidebar.focused;
            if self.sidebar.focused {
                self.sidebar.reveal();
            }
            self.redraw();
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

    /// Opens a bar label's dropdown under the label, a context menu of
    /// the view's keys, a row that needs the list to have rows disabled
    /// without them; a view whose bar has no such label opens nothing,
    /// and a surface without room for the dropdown says so in the status
    /// row instead.
    fn open_menu(&mut self, menu: Menu) {
        let Some((anchor, rows)) = self.shape().and_then(|shape| {
            let index = shape.keys.iter().position(|&key| key == Key::Menu(menu))?;
            let anchor = shape.layout.bar.header(index)?;
            Some((anchor, shape.total.is_some_and(|total| total > 0)))
        }) else {
            return;
        };
        let (x, y) = (anchor.x, anchor.y.saturating_add(i64::from(anchor.height)));
        let nodes: Vec<menus::Node<'static, Key>> = menu
            .rows()
            .iter()
            .map(|row| menus::Node {
                parent: None,
                row: td_ui::chrome::Row {
                    label: row.label,
                    shortcut: row.shortcut,
                    enabled: rows || !row.needs_row,
                    checked: false,
                },
                item: menus::Item::Action(row.key),
            })
            .collect();
        let opened = menus::Model::new(menus::Kind::Context, menu, &nodes)
            .and_then(|model| menus::Controller::new(model, self.surface, menus::Fit::Adaptive))
            .and_then(|mut widget| widget.open_context(x, y).map(|()| widget));
        match opened {
            Ok(widget) => self.menu = Some(widget),
            Err(error) => self.note(format!("menu: {error}")),
        }
        self.redraw();
    }

    /// Whether the open dropdown is still the shown view's: the view it
    /// was opened for may have gone under it (a response pushed another,
    /// or the view changed its mode), and its rows are then nobody's.
    fn menu_current(&self) -> Option<Menu> {
        let menu = self.menu.as_ref()?.model().revision();
        self.shape()
            .filter(|shape| shape.keys.contains(&Key::Menu(menu)))
            .map(|_| menu)
    }

    /// Closes the dropdown when the view it was opened for is no longer
    /// the one shown.
    fn drop_stale_menu(&mut self) {
        if self.menu.is_some() && self.menu_current().is_none() {
            self.menu = None;
            self.redraw();
        }
    }

    /// An input while a dropdown is open, which is the dropdown's: the
    /// toolkit's keys as the chord names them and every other chord
    /// consumed, the pointer's press, move and release (a press outside
    /// it, or a right press anywhere, dismisses it), the wheel scrolling
    /// its panel, and a focus loss or a resize dismissing it. The key it
    /// activates is the
    /// view's, as the label would have been. True when the input went
    /// no further; a resize and a focus loss are the frame's and the
    /// pane's as well, and a close, a paste, a pointer cancel and a
    /// focus gained are not the dropdown's at all. A dropdown whose
    /// view has gone under it is stale to the controller, which closes
    /// it, and the input is the view's.
    fn menu_input(&mut self, input: &Input<'_>) -> bool {
        let current = self.menu_current();
        let panel = self.menu.as_ref().and_then(|menu| menu.panel(0));
        let event = match *input {
            Input::Key { chord, repeat } => {
                let key = match chord {
                    "Up" => Some(menus::Key::Up),
                    "Down" => Some(menus::Key::Down),
                    "Left" => Some(menus::Key::Left),
                    "Right" => Some(menus::Key::Right),
                    "Return" | "Space" | " " => Some(menus::Key::Activate),
                    "Escape" => Some(menus::Key::Escape),
                    _ => None,
                };
                key.map_or(menus::Event::Other, |key| menus::Event::Key {
                    key,
                    repeated: repeat,
                })
            }
            Input::Pointer {
                phase: PointerPhase::Press,
                x,
                y,
                ..
            } => menus::Event::Press { x, y },
            Input::Pointer {
                phase: PointerPhase::Move,
                x,
                y,
                ..
            } => menus::Event::Move { x, y },
            Input::Pointer {
                phase: PointerPhase::Release,
                ..
            } => menus::Event::Release,
            // An open menu takes the wheel wherever the pointer is: the
            // panel's own place is what the controller scrolls by.
            Input::Wheel { rows, .. } => match panel {
                Some(panel) => menus::Event::Wheel {
                    x: panel.x,
                    y: panel.y,
                    rows,
                },
                None => menus::Event::Other,
            },
            Input::Focus(false) => menus::Event::FocusLost,
            Input::Resize(surface) => menus::Event::Resize(surface),
            // A right press dismisses it, as a press outside it does.
            Input::Context { .. } => menus::Event::Key {
                key: menus::Key::Dismiss,
                repeated: false,
            },
            Input::Focus(true)
            | Input::Close
            | Input::Paste(_)
            | Input::PasteImage { .. }
            | Input::CancelPointer
            | Input::Hover(_) => {
                return false;
            }
        };
        if matches!(
            input,
            Input::Key { .. } | Input::Pointer { .. } | Input::Wheel { .. } | Input::Context { .. }
        ) {
            self.last_user_activity = Instant::now();
        }
        let Some(widget) = self.menu.as_mut() else {
            return false;
        };
        let outcome = widget.event(current, event);
        if current.is_none() {
            self.menu = None;
            self.redraw();
            return false;
        }
        match outcome {
            Ok(menus::Outcome::Activated(key)) => {
                self.menu = None;
                self.redraw();
                self.key(key);
            }
            Ok(menus::Outcome::Dismissed | menus::Outcome::Stale) => {
                self.menu = None;
                self.redraw();
            }
            Ok(menus::Outcome::Changed) => self.redraw(),
            Ok(menus::Outcome::Ignored | menus::Outcome::Consumed) => {}
            Err(error) => {
                self.menu = None;
                self.redraw();
                self.note(format!("menu: {error}"));
            }
        }
        !matches!(input, Input::Resize(_) | Input::Focus(false))
    }

    /// The compositor asks the window to close: it closes at once
    /// unless a draft is unsaved, when the draft's own question is put
    /// instead and its answer decides (a save or a discard closes the
    /// window; Escape keeps it, with the draft), so nothing typed is
    /// lost to the close and a save that fails is seen.
    fn close_requested(&mut self) -> Flow {
        let waiting = self.stack.current().is_some_and(|view| view.waiting());
        if self.editing(true) && (waiting || self.draft().dirty()) {
            self.closing = true;
            self.request("close-tab");
            self.redraw();
            Flow::Continue
        } else {
            self.quit("the compositor asked the window to close");
            Flow::Quit
        }
    }

    /// Ends the window, logging why once: a quit is an exit 0 that
    /// leaves no other trace.
    fn quit(&mut self, why: &str) {
        if !self.quitting {
            crate::log_info!("[UI] quitting: {}", why);
        }
        self.quitting = true;
    }

    /// Points the pane's underline at the link under the hover point,
    /// where a Control-press would reach the pane and follow it: with the
    /// mouse on, no dropdown or finder over the frame, and the pane not
    /// asking about its draft.
    fn underline_link(&mut self) {
        let open = self.mouse && self.menu.is_none() && self.chooser.is_none();
        let at = self.hover.filter(|_| open);
        let rect = at.and_then(|(x, y)| {
            let rect = self
                .shape()?
                .layout
                .pane
                .filter(|rect| rect.contains(x, y))?;
            let asking = self.editing(true) && !self.editing(false);
            (!asking).then_some(rect)
        });
        if let Some(rect) = rect {
            self.pane.place(rect, self.surface);
        }
        if self.pane.hover(rect.and(at)) {
            self.redraw();
        }
    }

    fn pointer(&mut self, phase: PointerPhase, x: i64, y: i64, extend: bool, follow: bool) {
        let Some(shape) = self.shape() else {
            return;
        };
        if self.tree_pointer(shape, phase, x, y) {
            return;
        }
        // A press on the tree opens the entry under it; one anywhere else
        // gives the views the keys, a bar label being the view's.
        if let Some(row) = shape.layout.sidebar.and_then(|list| list.hit(x, y)) {
            if phase == PointerPhase::Press && !self.pane.drag {
                if let Some(entry) = self.sidebar.entry(self.sidebar.first.saturating_add(row)) {
                    self.open_entry(entry);
                }
                return;
            }
        }
        if phase == PointerPhase::Press && self.sidebar.focused {
            self.sidebar.focused = false;
            self.redraw();
        }
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
        // A Control-press on a link opens it and is no press of the
        // pane's; the drag and release that follow find no drag.
        if follow && phase == PointerPhase::Press && rect.contains(x, y) {
            self.pane.place(rect, self.surface);
            if let Some(link) = self.pane.link(x, y) {
                let note = match td_ui::open::link(&link, self.setup.browser.as_deref()) {
                    Ok(program) => format!("opened {link} with {program}"),
                    Err(why) => format!("link not opened: {why}"),
                };
                self.note(note);
                // The followed press is no click: the next press starts anew.
                self.pane.cancel_pointer();
                return;
            }
        }
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

    /// The pointer on the tree's chrome, whichever side has the keys: a
    /// press on its edge with the views resizes it as it drags, one on its
    /// scrollbar's thumb scrolls its rows as it drags, and one on the
    /// track above or below the thumb pages them. Whether it took the
    /// pointer.
    fn tree_pointer(&mut self, shape: Shape, phase: PointerPhase, x: i64, y: i64) -> bool {
        let Some(list) = shape.layout.sidebar else {
            self.tree_drag = None;
            return false;
        };
        let s = self.surface.scale.value() as i64;
        let first = self.sidebar.first;
        let bar = list.scrollbar(self.sidebar.len(), first);
        let rect = list.rect();
        if phase == PointerPhase::Press {
            // A drag whose release went to a menu or the finder ends here.
            self.tree_drag = None;
            if self.pane.drag {
                return false;
            }
            let edge = rect.x + i64::from(rect.width);
            let beside = y >= rect.y && y < rect.y + i64::from(rect.height);
            // The tree's side of the edge, clear of the scrollbar's track.
            let grip = x >= edge - EDGE_GRAB * s && x <= edge;
            if matches!(self.side(), Side::Column(_)) && beside && grip {
                self.tree_drag = Some(TreeDrag::Edge);
                return true;
            }
            if !bar.enabled() || !bar.track.contains(x, y) {
                return false;
            }
            if bar.thumb.contains(x, y) {
                self.tree_drag = Some(TreeDrag::Thumb(y - bar.thumb.y));
            } else {
                let page = list.rows().max(1) as isize;
                self.sidebar
                    .scroll(if y < bar.thumb.y { -page } else { page }, list);
                self.redraw();
            }
            return true;
        }
        let Some(drag) = self.tree_drag else {
            return false;
        };
        match drag {
            TreeDrag::Edge => {
                let cell = (td_ui::CELL_WIDTH as i64) * s;
                self.resize_tree(((x - rect.x + cell / 2) / cell).max(0) as usize);
            }
            TreeDrag::Thumb(grab) => {
                self.sidebar
                    .scroll_to(bar.position_at(y, grab, first), list);
                self.redraw();
            }
        }
        if phase == PointerPhase::Release {
            self.tree_drag = None;
        }
        true
    }

    /// Wheel travel in rows, to what lies under the pointer: over the
    /// tree, its rows; over a list, one scroll key per row, bounded; over
    /// a text, the pane's scroll. With no pointer place known, the side
    /// with the keys takes it; a tree over the whole body takes it all.
    fn wheel(&mut self, rows: isize) {
        let Some(shape) = self.shape() else {
            return;
        };
        self.last_user_activity = Instant::now();
        if let Some(list) = shape.layout.sidebar {
            let over = match self.pointer_at {
                _ if self.side() == Side::Full => true,
                Some((x, y)) => list.rect().contains(x, y),
                None => self.tree_focused(),
            };
            if over {
                let bound = WHEEL_EVENTS as isize;
                self.sidebar.scroll(rows.clamp(-bound, bound), list);
                self.redraw();
                return;
            }
        }
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
            if self.quitting {
                return;
            }
            self.view_key(key);
        }
    }

    /// Lays the frame out: the list reveals its selection, the entry its
    /// caret, the pane is placed and shows the view's document, underlined
    /// where Control is held over a link, and the title is the view's.
    /// Before every paint and every read of the frame.
    fn prepare_frame(&mut self) {
        self.lay_out();
        // The frame may show another view or layout than the input did.
        self.underline_link();
    }

    fn lay_out(&mut self) {
        self.refresh_title();
        let surface = self.surface;
        let side = self.side();
        let Some(slot) = self.stack.top_mut() else {
            return;
        };
        let scene = slot.view.scene();
        let layout = Layout::new(surface, &scene, side);
        if let Some(list) = layout.sidebar {
            self.sidebar.lay_out(list);
        }
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
        let scene = self.scene(slot);
        let frame = Frame {
            surface: self.surface,
            scene: &scene,
            sidebar: &self.sidebar,
            side: self.side(),
            first: slot.first,
            entry_first: slot.entry_first,
            pane: &self.pane,
            chooser: self.chooser.as_ref().map(|chooser| &chooser.finder),
            menu: self.menu.as_ref(),
        };
        td_ui::driven::text(&frame).expect("text").2
    }
}

/// A note for the finder's status row: control characters blanked and,
/// past its bound, the tail kept after an ellipsis, since the reason
/// follows the path.
fn fit_note(note: &str) -> String {
    let fitted: String = note
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if fitted.len() <= finder::NOTE_BYTES {
        return fitted;
    }
    let keep = finder::NOTE_BYTES - '\u{2026}'.len_utf8();
    let mut start = fitted.len() - keep;
    while !fitted.is_char_boundary(start) {
        start += 1;
    }
    format!("\u{2026}{}", fitted.get(start..).unwrap_or_default())
}

impl Handler for Session {
    fn title(&self) -> &str {
        &self.title
    }

    fn app_id(&self) -> &str {
        "td-mail"
    }

    fn keys(&self) -> Vec<td_ui::keys::Section> {
        if self.tree_focused() {
            return crate::keybindings::sections(Some(&crate::keybindings::ACCOUNT_TREE));
        }
        crate::keybindings::sections(self.stack.current().and_then(|view| view.keys()))
    }

    fn take_show_keys(&mut self) -> bool {
        std::mem::take(&mut self.show_keys)
    }

    fn motion(&mut self, at: Option<(i64, i64)>) {
        self.pointer_at = at;
    }

    fn input(&mut self, input: Input<'_>, clipboard: &mut dyn Clipboard) -> Flow {
        // The note is up until the next key or press.
        let presses = matches!(
            input,
            Input::Key { .. }
                | Input::Pointer {
                    phase: PointerPhase::Press,
                    ..
                }
        );
        if presses && self.note.take().is_some() {
            self.redraw();
        }
        // A dropdown open over the frame takes the input first, and then
        // the finder open over the body.
        let taken = self.menu.is_some() && self.menu_input(&input);
        let taken = taken || (self.chooser.is_some() && self.chooser_input(&input));
        if !taken {
            match input {
                // td-mail asks for no picture.
                Input::PasteImage { .. } => {}
                // The clipboard's text, into the draft it was asked for, over
                // its selection, while that draft is still the one being
                // edited: closed, or under its save question, or another
                // draft's, it goes nowhere, with a note.
                Input::Paste(text) => {
                    let target = self.paste_target.take();
                    if target.is_none() || target != self.pane.tab() || !self.editing(false) {
                        self.note(
                            "paste dropped: the draft it was asked for is not being edited".into(),
                        );
                    } else {
                        match self.pane.insert(text) {
                            Ok(true) => self.redraw(),
                            Ok(false) => {}
                            Err(why) => self.note(format!("paste refused: {why}")),
                        }
                    }
                }
                Input::Close => {
                    if self.close_requested() == Flow::Quit {
                        return Flow::Quit;
                    }
                }
                Input::Resize(surface) => {
                    self.surface = surface;
                    self.redraw();
                }
                // A repeated Cut must not delete the next line. Other
                // repeats remain kill-ring-only while Paste is pending.
                Input::Key { chord, repeat } => {
                    if !(repeat && chord == "C-x") {
                        self.chord(chord);
                    }
                    if repeat && chord != "C-x" {
                        self.asks = std::mem::take(&mut self.asks)
                            .into_iter()
                            .filter_map(|ask| match ask {
                                Ask::Copy(_) => None,
                                Ask::Paste { .. } => Some(Ask::Paste { clipboard: false }),
                            })
                            .collect();
                    }
                }
                Input::Pointer {
                    phase,
                    x,
                    y,
                    extend,
                    follow,
                } => {
                    if self.mouse {
                        self.pointer(phase, x, y, extend, follow);
                    }
                }
                Input::CancelPointer => {
                    self.tree_drag = None;
                    self.pane.cancel_pointer();
                }
                // Nothing here opens at a right press; the Folder menu
                // opens from the action bar.
                Input::Context { .. } => {}
                Input::Hover(at) => self.hover = at,
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
        }
        self.serve(clipboard);
        self.settle_close();
        self.refresh_title();
        self.underline_link();
        if self.quitting {
            Flow::Quit
        } else {
            Flow::Continue
        }
    }

    fn poll(&mut self, now: u64) -> Flow {
        // A backend that is gone answers nothing more; the views stay up
        // on what they hold, as they did in the terminal. An account not
        // shown has no view to answer.
        for index in 0..self.live.len() {
            while let Some(response) = self.live.get(index).and_then(|l| l.resp_rx.try_recv().ok())
            {
                if let BackendResponse::Mailboxes(result) = &response {
                    self.sidebar.listed(index, result);
                    if self.side() != Side::None {
                        self.redraw();
                    }
                }
                self.marked_read(index, &response);
                if self.stack.handle_response(index, &response) {
                    self.redraw();
                }
                self.hold_draft();
                self.take_pending();
            }
        }
        self.take_pending();
        self.settle_close();
        self.drop_stale_menu();
        self.drop_stale_chooser();
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
                let synced = self
                    .stack
                    .current_mut()
                    .is_some_and(|view| view.trigger_idle_sync());
                // The tree's counts, whatever view is on top: the accounts
                // not shown, and the shown one while a view is over its
                // page. On top, the page syncs itself, and declines while
                // it asks a question a listing under it could change.
                let several = self.setup.accounts.len() > 1;
                if several {
                    let page_on_top =
                        self.stack.depth() == 1 && self.stack.root_account() == Some(self.current);
                    for (index, live) in self.live.iter().enumerate() {
                        if index != self.current || !page_on_top {
                            let _ = live.cmd_tx.send(BackendCommand::FetchMailboxes {
                                origin: "idle_sync.sidebar".to_string(),
                            });
                        }
                    }
                }
                if synced || several {
                    self.last_idle_sync = Instant::now();
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
            let scene = self.scene(slot);
            let frame = Frame {
                surface,
                scene: &scene,
                sidebar: &self.sidebar,
                side: self.side(),
                first: slot.first,
                entry_first: slot.entry_first,
                pane: &self.pane,
                chooser: self.chooser.as_ref().map(|chooser| &chooser.finder),
                menu: self.menu.as_ref(),
            };
            frame::paint(raster, &frame)?;
        }
        self.dirty = false;
        Ok(())
    }

    /// The window's diagnostics, a paste that failed or was cancelled
    /// among them: logged, and the status row's note.
    fn notice(&mut self, message: &str) {
        crate::log_error!("window: {}", message);
        self.note(message.to_string());
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
    rules_mailbox_regex: UserRegex,
    my_email_regex: UserRegex,
    retention_policies: Vec<RetentionPolicyConfig>,
    rules: Vec<CompiledRule>,
    spam_config: SpamConfig,
    offline: bool,
    config_path: PathBuf,
) -> io::Result<()> {
    // Read before the backend threads are spawned and the window opened,
    // so an index the account list does not hold fails with nothing to
    // close.
    if current_account_idx >= accounts.len() {
        return Err(io::Error::other(format!(
            "no account at index {} of {}",
            current_account_idx,
            accounts.len()
        )));
    }
    let setup = Setup {
        account_names: accounts.iter().map(|a| a.name.clone()).collect(),
        accounts,
        page_size,
        browser,
        retention_policies,
        sync_interval_secs,
        rules,
        // Both patterns were compiled where the configuration was read.
        rules_mailbox_regex: Arc::new(rules_mailbox_regex),
        my_email_regex: Arc::new(my_email_regex),
        spam_config,
        spam_model: crate::spam::load_shared(),
        offline,
        draft_dir: None,
        connector: if offline {
            None
        } else {
            Some(spawn_connector().map_err(io::Error::other)?)
        },
        config_path: Some(config_path),
    };

    let endpoint = td_ui::wayland::endpoint(
        std::env::var_os("WAYLAND_SOCKET"),
        std::env::var_os("WAYLAND_DISPLAY"),
        std::env::var_os("XDG_RUNTIME_DIR"),
    )
    .map_err(io::Error::other)?;
    let stream = td_ui::wayland::connect(endpoint).map_err(io::Error::other)?;

    // Every account is opened, the shown one first, so its connection is
    // the connector's first.
    let mut opened: Vec<Option<Live>> = setup.accounts.iter().map(|_| None).collect();
    let order = std::iter::once(current_account_idx)
        .chain((0..setup.accounts.len()).filter(|&index| index != current_account_idx));
    for index in order {
        if let (Some(account), Some(slot)) = (setup.accounts.get(index), opened.get_mut(index)) {
            *slot = Some(
                setup
                    .open_account(account, "startup")
                    .map_err(io::Error::other)?,
            );
        }
    }
    let live: Vec<Live> = opened.into_iter().flatten().collect();
    let mut session =
        Session::new(setup, live, current_account_idx, mouse).map_err(io::Error::other)?;
    let typeface = td_ui::pinned_face::load_or_note(
        "td-mail",
        std::env::var_os(td_ui::pinned_face::SETTING).as_deref(),
    );
    let outcome = td_ui::window::run(&mut session, stream, std::env::temp_dir(), typeface);
    for live in &session.live {
        let _ = live.cmd_tx.send(BackendCommand::Shutdown);
    }
    // Each backend answers what it held from the cache on the way out;
    // wait for them to go, up to two seconds of silence from all, so a
    // mutation one queues is written before the process ends. One
    // mid-request is left to the exit.
    let mut silent = 0;
    while silent < 20 {
        let mut heard = false;
        let mut open = false;
        for live in &session.live {
            loop {
                match live.resp_rx.try_recv() {
                    Ok(_) => heard = true,
                    Err(mpsc::TryRecvError::Empty) => {
                        open = true;
                        break;
                    }
                    Err(mpsc::TryRecvError::Disconnected) => break,
                }
            }
        }
        if !open {
            break;
        }
        if heard {
            silent = 0;
        } else {
            silent += 1;
            std::thread::sleep(Duration::from_millis(100));
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
    use std::sync::atomic::{AtomicU64, Ordering};
    use td_ui::window::{NoClipboard, PointerPhase};

    /// How many `setup`s this process has made: parallel tests can read
    /// one clock tick, so the time alone does not keep their state apart.
    static SETUPS: AtomicU64 = AtomicU64::new(0);

    fn account() -> AccountConfig {
        AccountConfig {
            tls_ca_file: None,
            name: "test".to_string(),
            well_known_url: String::new(),
            username: "me@example.com".to_string(),
            password: PasswordSource::Command("true".to_string()),
            mail: crate::config::AccountMail::for_test(),
        }
    }

    fn setup() -> Setup {
        let regex = |pattern: &str| Arc::new(UserRegex::compile(pattern).unwrap());
        Setup {
            accounts: vec![account()],
            account_names: vec!["test".to_string()],
            page_size: 50,
            browser: None,
            retention_policies: Vec::new(),
            sync_interval_secs: None,
            rules: Vec::new(),
            rules_mailbox_regex: regex("^INBOX$"),
            my_email_regex: regex("^$"),
            spam_config: SpamConfig {
                enabled: false,
                threshold: 0.9,
                ham_threshold: 0.2,
                min_training: 20,
            },
            offline: true,
            draft_dir: Some(
                std::env::temp_dir()
                    .join(format!(
                        "td-mail-state-{}-{}-{}",
                        std::process::id(),
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap()
                            .as_nanos(),
                        SETUPS.fetch_add(1, Ordering::Relaxed)
                    ))
                    .join("drafts"),
            ),
            connector: None,
            spam_model: Arc::new(std::sync::Mutex::new(crate::spam::SpamModel::default())),
            config_path: None,
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
        let mut session = Session::new(setup, vec![Live { cmd_tx, resp_rx }], 0, mouse).unwrap();
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

    /// A setup the configuration refuses leaves the account and the view
    /// as they were and says why in the status row; no backend is
    /// replaced.
    #[test]
    fn a_refused_setup_is_a_note_and_changes_nothing() {
        let (mut session, cmd_rx, _resp_tx) = session(true);
        session.set_up_account("test", "mx.td.dev", "me@td.dev");
        assert_eq!(
            session.note.as_deref(),
            Some("Not set up: no configuration file to write")
        );
        assert_eq!(session.setup.accounts[0].well_known_url, "");

        let dir = crate::testing::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let text = "[account.test]\nwell_known_url = \"https://mx.td.dev/.well-known/jmap\"\nusername = \"u@td.dev\"\nsecret = \"portal\"\n";
        std::fs::write(&path, text).unwrap();
        session.setup.config_path = Some(path.clone());
        session.set_up_account("test", "other.td.dev", "me@td.dev");
        let note = session.note.clone().unwrap();
        assert!(note.contains("has changed since td-mail read it"), "{note}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        assert!(cmd_rx.try_recv().is_err(), "a backend was replaced");
    }

    /// Every account keeps its backend for the session: Tab gives the
    /// account tree the keys, and Return on the next account's heading
    /// shows its list over its own backend, which fetches its folders;
    /// only the shown account's answers reach a view, and the tree keeps
    /// every account's folders. `R`
    /// asks the connector for the shown account again behind
    /// `Connecting`, the folders' fetch after it, so the backend holds
    /// the fetch until the connection is decided; setting an account up
    /// asks the same way, before its list's fetch.
    #[test]
    fn accounts_keep_their_backends_and_r_reconnects_the_shown_one() {
        let mut setup = setup();
        let mut work = account();
        work.name = "work".to_string();
        work.well_known_url = "https://mail.example.com/.well-known/jmap".to_string();
        setup.accounts = vec![account(), work];
        setup.account_names = vec!["test".to_string(), "work".to_string()];
        let (connector, requests) = mpsc::channel();
        setup.connector = Some(connector);
        setup.offline = false;
        let (test_tx, test_cmds) = mpsc::channel();
        let (test_answer, test_rx) = mpsc::channel();
        let (work_tx, work_cmds) = mpsc::channel();
        let (work_answer, work_rx) = mpsc::channel();
        let live = vec![
            Live {
                cmd_tx: test_tx,
                resp_rx: test_rx,
            },
            Live {
                cmd_tx: work_tx,
                resp_rx: work_rx,
            },
        ];
        let mut session = Session::new(setup, live, 0, false).unwrap();
        key(&mut session, "Tab");
        assert!(session.shown().contains("Accounts | "));
        key(&mut session, "j");
        key(&mut session, "Return");
        assert_eq!(session.current, 1);
        assert!(!session.sidebar.focused);
        assert!(matches!(
            work_cmds.try_recv(),
            Ok(BackendCommand::FetchMailboxes { .. })
        ));
        test_answer
            .send(BackendResponse::Mailboxes(Ok(vec![mailbox(
                "t", "TestOnly", "inbox", 1, 1,
            )])))
            .unwrap();
        work_answer
            .send(BackendResponse::Mailboxes(Ok(vec![mailbox(
                "w",
                "WorkInbox",
                "inbox",
                2,
                0,
            )])))
            .unwrap();
        assert_eq!(session.poll(0), Flow::Continue);
        let shown = session.shown();
        assert!(
            shown.contains("WorkInbox") && shown.contains("TestOnly"),
            "{shown}"
        );
        let scene = session.stack.current().unwrap().scene();
        let rows: Vec<String> = match &scene.body {
            Body::List { total, row, .. } => (0..*total).map(|i| row(i).label).collect(),
            _ => panic!("expected the folder list"),
        };
        assert_eq!(rows, ["WorkInbox"]);
        drop(scene);
        // The account switched away from is not shut down.
        assert!(test_cmds.try_recv().is_err());

        key(&mut session, "R");
        assert!(matches!(
            work_cmds.try_recv(),
            Ok(BackendCommand::Connecting)
        ));
        let asked = requests.try_recv().unwrap();
        assert_eq!(asked.account.name, "work");
        assert!(asked.urgent);
        assert!(matches!(
            work_cmds.try_recv(),
            Ok(BackendCommand::FetchMailboxes { origin }) if origin == "reconnect"
        ));

        let dir = crate::testing::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[account.test]\nwell_known_url = \"\"\nusername = \"me@example.com\"\npassword_command = \"true\"\n\
             [account.work]\nwell_known_url = \"https://mail.example.com/.well-known/jmap\"\nusername = \"me@example.com\"\npassword_command = \"true\"\n",
        )
        .unwrap();
        session.setup.config_path = Some(path);
        session.set_up_account("work", "mx.td.dev", "me@td.dev");
        assert_eq!(session.note, None);
        assert!(matches!(
            work_cmds.try_recv(),
            Ok(BackendCommand::Connecting)
        ));
        let asked = requests.try_recv().unwrap();
        assert_eq!(
            asked.account.well_known_url,
            "https://mx.td.dev/.well-known/jmap"
        );
        assert!(matches!(
            work_cmds.try_recv(),
            Ok(BackendCommand::FetchMailboxes { origin }) if origin == "set_up_account"
        ));
        assert!(test_cmds.try_recv().is_err());
    }

    /// The account tree lists every account's folders beside the views.
    /// Return on a folder opens it over its account's page on that
    /// account's backend, so `q` from it is the account's page; a window
    /// too narrow for the tree beside the views shows it over them while
    /// it has the keys; and with a draft open a press on the tree keeps
    /// the views and says why.
    #[test]
    fn the_account_tree_opens_an_accounts_folder_and_spares_a_draft() {
        let state = crate::testing::tempdir().unwrap();
        let mut setup = setup();
        setup.draft_dir = Some(state.path().join("drafts"));
        let mut work = account();
        work.name = "work".to_string();
        setup.accounts = vec![account(), work];
        setup.account_names = vec!["test".to_string(), "work".to_string()];
        let (test_tx, _test_cmds) = mpsc::channel();
        let (test_answer, test_rx) = mpsc::channel();
        let (work_tx, work_cmds) = mpsc::channel();
        let (work_answer, work_rx) = mpsc::channel();
        let live = vec![
            Live {
                cmd_tx: test_tx,
                resp_rx: test_rx,
            },
            Live {
                cmd_tx: work_tx,
                resp_rx: work_rx,
            },
        ];
        let mut session = Session::new(setup, live, 0, true).unwrap();
        test_answer
            .send(BackendResponse::Mailboxes(Ok(vec![mailbox(
                "t1", "INBOX", "inbox", 3, 1,
            )])))
            .unwrap();
        work_answer
            .send(BackendResponse::Mailboxes(Ok(vec![
                mailbox("w2", "Projects", "", 4, 0),
                mailbox("w1", "INBOX", "inbox", 5, 2),
            ])))
            .unwrap();
        assert_eq!(session.poll(0), Flow::Continue);
        let shown = session.shown();
        assert!(shown.contains("  Projects"), "{shown}");

        key(&mut session, "Tab");
        key(&mut session, "End");
        key(&mut session, "Return");
        assert_eq!(session.current, 1);
        assert_eq!(session.stack.depth(), 2);
        let sent: Vec<_> = work_cmds.try_iter().collect();
        assert!(
            sent.iter().any(|command| matches!(
                command,
                BackendCommand::QueryEmails { mailbox_id, .. } if mailbox_id == "w2"
            )),
            "the folder was not asked of the work account's backend"
        );
        key(&mut session, "q");
        assert_eq!(session.stack.depth(), 1);
        assert_eq!(session.current, 1);

        session.surface = Surface::new(400, 600, Default::default()).unwrap();
        assert_eq!(session.side(), Side::None);
        key(&mut session, "Tab");
        assert_eq!(session.side(), Side::Full);
        key(&mut session, "Escape");
        assert_eq!(session.side(), Side::None);
        session.surface = Surface::new(800, 600, Default::default()).unwrap();

        key(&mut session, "c");
        assert!(session.editing(false));
        let row = session
            .shape()
            .and_then(|shape| shape.layout.sidebar)
            .and_then(|list| list.row(0))
            .unwrap();
        press(&mut session, row.x + 4, row.y + 4);
        assert_eq!(session.current, 1);
        assert!(session.editing(false));
        assert_eq!(
            session.note.as_deref(),
            Some("Not opened: close the draft first")
        );
    }

    /// A session over the accounts `test` and `work`, the first shown,
    /// each listed with an inbox and `work` with a second folder too;
    /// the state directory, and each account's commands.
    fn two_accounts() -> (
        Session,
        crate::testing::TempDir,
        Receiver<BackendCommand>,
        Receiver<BackendCommand>,
    ) {
        let (session, state, test_cmds, work_cmds, _) = two_accounts_answering();
        (session, state, test_cmds, work_cmds)
    }

    /// `two_accounts`, with each account's answer sender kept open.
    #[allow(clippy::type_complexity)]
    fn two_accounts_answering() -> (
        Session,
        crate::testing::TempDir,
        Receiver<BackendCommand>,
        Receiver<BackendCommand>,
        [Sender<BackendResponse>; 2],
    ) {
        let state = crate::testing::tempdir().unwrap();
        let mut setup = setup();
        setup.draft_dir = Some(state.path().join("drafts"));
        let mut work = account();
        work.name = "work".to_string();
        setup.accounts = vec![account(), work];
        setup.account_names = vec!["test".to_string(), "work".to_string()];
        let (test_tx, test_cmds) = mpsc::channel();
        let (test_answer, test_rx) = mpsc::channel();
        let (work_tx, work_cmds) = mpsc::channel();
        let (work_answer, work_rx) = mpsc::channel();
        let live = vec![
            Live {
                cmd_tx: test_tx,
                resp_rx: test_rx,
            },
            Live {
                cmd_tx: work_tx,
                resp_rx: work_rx,
            },
        ];
        let mut session = Session::new(setup, live, 0, true).unwrap();
        test_answer
            .send(BackendResponse::Mailboxes(Ok(vec![mailbox(
                "t1", "INBOX", "inbox", 3, 1,
            )])))
            .unwrap();
        work_answer
            .send(BackendResponse::Mailboxes(Ok(vec![
                mailbox("w2", "Projects", "", 4, 0),
                mailbox("w1", "INBOX", "inbox", 5, 2),
            ])))
            .unwrap();
        assert_eq!(session.poll(0), Flow::Continue);
        (
            session,
            state,
            test_cmds,
            work_cmds,
            [test_answer, work_answer],
        )
    }

    /// A key the tree does not take gives the views the keys and is
    /// theirs: `+` opens the folder entry, which then takes `n`, `j` and
    /// Return; `c` opens a draft the tree never hides, even in a narrow
    /// window, where the view shows again; and `q` in the tree only
    /// gives the keys back.
    #[test]
    fn a_key_the_tree_does_not_take_is_the_views() {
        let (mut session, _state, test_cmds, _work_cmds) = two_accounts();
        while test_cmds.try_recv().is_ok() {}
        key(&mut session, "Tab");
        assert!(session.tree_focused());
        assert_eq!(session.keys()[0].title, "Account Tree");
        key(&mut session, "q");
        assert!(!session.quitting);
        assert!(!session.sidebar.focused);

        key(&mut session, "Tab");
        key(&mut session, "+");
        assert!(!session.sidebar.focused);
        for chord in ["n", "j", "Return"] {
            key(&mut session, chord);
        }
        assert_eq!(session.current, 0);
        assert!(matches!(
            test_cmds.try_recv(),
            Ok(BackendCommand::CreateMailbox { name }) if name == "nj"
        ));

        session.surface = Surface::new(400, 600, Default::default()).unwrap();
        key(&mut session, "Tab");
        assert_eq!(session.side(), Side::Full);
        key(&mut session, "c");
        assert!(session.editing(false));
        assert!(!session.sidebar.focused);
        assert_eq!(session.side(), Side::None);
        assert!(session.shape().unwrap().layout.pane.is_some());
        assert!(!session.shown().contains("Accounts | "));
    }

    /// A press on a tree row opens its entry; a folder of the account
    /// shown opens over that account's page, and a name the page was
    /// being given is left unconfirmed; a press on a bar label is the
    /// view's even while the tree has the keys; and the idle interval
    /// asks every account for its folders whatever view is on top, the
    /// shown one too while a folder is open.
    #[test]
    fn a_press_opens_a_tree_entry_and_the_idle_sync_asks_every_account() {
        let (mut session, _state, test_cmds, work_cmds) = two_accounts();
        while test_cmds.try_recv().is_ok() {}
        // The tree: All inboxes, test, its INBOX, work, its INBOX and Projects.
        let row = |session: &Session, index: usize| {
            session
                .shape()
                .and_then(|shape| shape.layout.sidebar)
                .and_then(|list| list.row(index))
                .unwrap()
        };
        key(&mut session, "+");
        key(&mut session, "f");
        let inbox = row(&session, 2);
        press(&mut session, inbox.x + 4, inbox.y + 4);
        assert_eq!(session.current, 0);
        assert_eq!(session.stack.depth(), 2);
        let sent: Vec<_> = test_cmds.try_iter().collect();
        assert!(sent.iter().any(|command| matches!(
            command,
            BackendCommand::QueryEmails { mailbox_id, .. } if mailbox_id == "t1"
        )));
        assert!(!sent
            .iter()
            .any(|command| matches!(command, BackendCommand::CreateMailbox { .. })));

        session.sync_interval = Some(Duration::ZERO);
        session.poll(0);
        assert!(test_cmds.try_iter().any(|command| matches!(
            command,
            BackendCommand::FetchMailboxes { origin } if origin == "idle_sync.sidebar"
        )));
        session.sync_interval = None;

        // The bar's first label is the email list's Open, not the tree's,
        // whose selection is now the work account's heading.
        key(&mut session, "Tab");
        key(&mut session, "j");
        let label = session.shape().unwrap().layout.bar.header(0).unwrap();
        press(&mut session, label.x + 2, label.y + 2);
        assert!(!session.sidebar.focused);
        assert_eq!(session.current, 0);
        assert_eq!(session.stack.depth(), 2);

        let heading = row(&session, 3);
        press(&mut session, heading.x + 4, heading.y + 4);
        assert_eq!(session.current, 1);
        assert_eq!(session.stack.depth(), 1);
        while work_cmds.try_recv().is_ok() {}

        // The shown page asking whether to delete a folder is not listed
        // again under the question.
        key(&mut session, "d");
        session.sync_interval = Some(Duration::ZERO);
        session.poll(0);
        session.sync_interval = None;
        assert!(!work_cmds
            .try_iter()
            .any(|command| matches!(command, BackendCommand::FetchMailboxes { .. })));
        key(&mut session, "Escape");

        session.sync_interval = Some(Duration::ZERO);
        session.poll(0);
        assert!(test_cmds.try_iter().any(|command| matches!(
            command,
            BackendCommand::FetchMailboxes { origin } if origin == "idle_sync.sidebar"
        )));
        assert!(work_cmds
            .try_iter()
            .any(|command| matches!(command, BackendCommand::FetchMailboxes { .. })));
    }

    /// `two_accounts_answering`, `test` listing an inbox and 40 folders
    /// more, more than the tree or the folder list shows at once.
    fn two_accounts_with_many_folders() -> (
        Session,
        crate::testing::TempDir,
        Receiver<BackendCommand>,
        Receiver<BackendCommand>,
        [Sender<BackendResponse>; 2],
    ) {
        let (mut session, state, test_cmds, work_cmds, answers) = two_accounts_answering();
        let mut folders = vec![mailbox("t1", "INBOX", "inbox", 3, 1)];
        folders.extend((0..40).map(|n| mailbox(&format!("f{n}"), &format!("F{n:02}"), "", 1, 0)));
        answers[0]
            .send(BackendResponse::Mailboxes(Ok(folders)))
            .unwrap();
        session.poll(0);
        session.shown();
        (session, state, test_cmds, work_cmds, answers)
    }

    fn pointer(session: &mut Session, phase: PointerPhase, x: i64, y: i64) {
        session.input(
            Input::Pointer {
                phase,
                x,
                y,
                extend: false,
                follow: false,
            },
            &mut NoClipboard,
        );
    }

    fn wheel(session: &mut Session, rows: isize) {
        session.input(Input::Wheel { rows, columns: 0 }, &mut NoClipboard);
        session.shown();
    }

    /// The top view's selected row.
    fn view_selected(session: &Session) -> usize {
        match session.stack.current().unwrap().scene().body {
            Body::List { selected, .. } => selected,
            Body::Text { .. } | Body::Edit { .. } => panic!("no list"),
        }
    }

    /// The wheel scrolls the side under the pointer, whichever has the
    /// keys: over the tree its rows, leaving its selection and the keys
    /// where they were; over the views the view's list, the tree keeping
    /// its keys. With no place known, the side with the keys takes it.
    #[test]
    fn the_wheel_scrolls_the_side_under_the_pointer() {
        let (mut session, _state, _test_cmds, _work_cmds, _answers) =
            two_accounts_with_many_folders();
        let shape = session.shape().unwrap();
        let tree = shape.layout.sidebar.unwrap().rect();
        let list = shape.layout.list.unwrap().rect();
        let selected = session.sidebar.selected();

        session.motion(Some((tree.x + 8, tree.y + 8)));
        wheel(&mut session, 3);
        assert_eq!(session.sidebar.first, 3);
        assert_eq!(session.sidebar.selected(), selected);
        assert!(!session.sidebar.focused);
        assert_eq!(view_selected(&session), 0);

        // Taking the keys shows the cursor that says so: the selection,
        // the test account's heading, is the first row again.
        key(&mut session, "Tab");
        session.shown();
        assert_eq!(session.sidebar.first, 1);
        session.motion(Some((list.x + 8, list.y + 8)));
        wheel(&mut session, 2);
        assert_eq!(session.sidebar.first, 1);
        assert_eq!(view_selected(&session), 2);
        assert!(session.tree_focused());

        session.motion(None);
        wheel(&mut session, 1);
        assert_eq!(session.sidebar.first, 2);
        assert_eq!(view_selected(&session), 2);
        key(&mut session, "Tab");
        wheel(&mut session, 1);
        assert_eq!(session.sidebar.first, 2);
        assert_eq!(view_selected(&session), 3);
    }

    /// A drag from the edge between the tree and the views resizes the
    /// tree, held between 12 cells and half the window, as `<` and `>`
    /// in the tree do; a press on the tree's scrollbar track pages its
    /// rows and a drag of its thumb scrolls them, the keys staying where
    /// they were; and a name too long for the tree stops short of its
    /// count.
    #[test]
    fn the_trees_edge_resizes_it_and_its_scrollbar_scrolls_it() {
        let (mut session, _state, _test_cmds, _work_cmds, _answers) =
            two_accounts_with_many_folders();
        let cell = td_ui::CELL_WIDTH as i64;
        let tree = session.shape().unwrap().layout.sidebar.unwrap().rect();
        assert_eq!(session.side(), Side::Column(24 * cell as u32));
        let y = tree.y + 20;
        let edge = tree.x + i64::from(tree.width);
        // The views' side of the edge is theirs; a drag whose release
        // went elsewhere does not outlive the next press.
        session.tree_drag = Some(TreeDrag::Edge);
        pointer(&mut session, PointerPhase::Press, edge + 2, y);
        pointer(&mut session, PointerPhase::Move, 30 * cell, y);
        pointer(&mut session, PointerPhase::Release, 30 * cell, y);
        assert_eq!(session.tree_drag, None);
        assert_eq!(session.side(), Side::Column(24 * cell as u32));
        pointer(&mut session, PointerPhase::Press, edge - 1, y);
        pointer(&mut session, PointerPhase::Move, 30 * cell + 3, y);
        assert_eq!(session.side(), Side::Column(30 * cell as u32));
        pointer(&mut session, PointerPhase::Move, 2000, y);
        pointer(&mut session, PointerPhase::Release, 2000, y);
        assert_eq!(session.side(), Side::Column(50 * cell as u32));
        pointer(&mut session, PointerPhase::Move, 0, y);
        assert_eq!(session.side(), Side::Column(50 * cell as u32), "released");
        assert_eq!(session.stack.depth(), 1);
        assert!(!session.sidebar.focused);

        key(&mut session, "Tab");
        key(&mut session, "<");
        assert_eq!(session.side(), Side::Column(48 * cell as u32));
        for _ in 0..30 {
            key(&mut session, "<");
        }
        assert_eq!(session.side(), Side::Column(12 * cell as u32));
        key(&mut session, ">");
        assert_eq!(session.side(), Side::Column(14 * cell as u32));
        assert!(session.tree_focused());
        assert!(session
            .shown()
            .contains("Accounts | n/p RET:open u:mark read </>:width Tab:back"));
        // A tree over the whole of a narrow window has no width to change,
        // and its status row shows the whole of the rest.
        session.surface = Surface::new(400, 600, Default::default()).unwrap();
        assert!(session
            .shown()
            .contains("Accounts | n/p RET:open u:mark read Tab:back"));
        key(&mut session, ">");
        assert_eq!(
            session.note.as_deref(),
            Some("Widen the window to resize the tree")
        );
        session.surface = Surface::new(800, 600, Default::default()).unwrap();
        assert_eq!(session.side(), Side::Column(14 * cell as u32));
        key(&mut session, "Tab");
        // At 14 cells a row holds 12 of text: the inset, the mark's two,
        // the label, a gap, the count and the inset again.
        let shown = session.shown();
        assert!(shown.contains("  INB\u{2026}"), "{shown}");
        assert!(shown.contains("  F03"), "{shown}");

        let list = session.shape().unwrap().layout.sidebar.unwrap();
        let rows = list.rows();
        let bar = list.scrollbar(session.sidebar.len(), session.sidebar.first);
        assert!(bar.enabled());
        let below = bar.thumb.y + i64::from(bar.thumb.height) + 2;
        press(&mut session, bar.track.x + 2, below);
        assert_eq!(session.sidebar.first, rows);
        assert_eq!(session.stack.depth(), 1);
        assert!(!session.sidebar.focused);
        let bar = list.scrollbar(session.sidebar.len(), session.sidebar.first);
        let (x, top) = (bar.thumb.x + 2, bar.thumb.y + 2);
        pointer(&mut session, PointerPhase::Press, x, top);
        pointer(&mut session, PointerPhase::Move, x, top - 4000);
        assert_eq!(session.sidebar.first, 0);
        pointer(&mut session, PointerPhase::Move, x, top + 4000);
        pointer(&mut session, PointerPhase::Release, x, top + 4000);
        assert_eq!(session.sidebar.first, session.sidebar.len() - rows);
        assert_eq!(session.tree_drag, None);
    }

    /// `u` in the tree marks the selected folder read on its own
    /// account's backend, says so, and when the account answers says how
    /// many and asks it for its folders again, its page not being on the
    /// stack; a folder already read, or a heading, asks nothing.
    #[test]
    fn u_in_the_tree_marks_its_folder_read() {
        let (mut session, _state, test_cmds, work_cmds, [_test_answer, work_answer]) =
            two_accounts_answering();
        while test_cmds.try_recv().is_ok() {}
        while work_cmds.try_recv().is_ok() {}
        // The tree: All inboxes, test, its INBOX, work, its INBOX and Projects.
        key(&mut session, "Tab");
        key(&mut session, "End");
        key(&mut session, "u");
        assert_eq!(
            session.note.as_deref(),
            Some("Folder 'Projects' already read")
        );
        key(&mut session, "p");
        key(&mut session, "p");
        key(&mut session, "u");
        assert_eq!(
            session.note.as_deref(),
            Some("Select a folder to mark read")
        );
        assert!(work_cmds.try_recv().is_err());
        assert!(session.tree_focused());

        key(&mut session, "n");
        key(&mut session, "u");
        assert!(matches!(
            work_cmds.try_recv(),
            Ok(BackendCommand::MarkMailboxRead { mailbox_id, .. }) if mailbox_id == "w1"
        ));
        assert_eq!(
            session.note.as_deref(),
            Some("Marking folder 'INBOX' read...")
        );
        work_answer
            .send(BackendResponse::MailboxMarkedRead {
                mailbox_id: "w1".to_string(),
                mailbox_name: "INBOX".to_string(),
                updated: 2,
                result: Ok(()),
            })
            .unwrap();
        session.poll(0);
        assert_eq!(
            session.note.as_deref(),
            Some("Marked 2 message(s) read in 'INBOX'")
        );
        assert!(session.marking.is_empty());
        assert!(matches!(
            work_cmds.try_recv(),
            Ok(BackendCommand::FetchMailboxes { origin }) if origin == "sidebar.marked_read"
        ));
        assert!(test_cmds.try_recv().is_err());
    }

    /// "All inboxes" heads the tree and opens the unified inbox over
    /// every account: each account's inbox is asked of its own backend
    /// and both answers fill it, newest first; a message opens over it
    /// bound to its own account, whose backend a reply or draft there
    /// then uses, and another account's answers do not reach it.
    #[test]
    fn all_inboxes_merge_every_account_and_a_message_opens_on_its_own() {
        let (mut session, _state, test_cmds, work_cmds, [test_answer, work_answer]) =
            two_accounts_answering();
        while test_cmds.try_recv().is_ok() {}
        while work_cmds.try_recv().is_ok() {}
        key(&mut session, "Tab");
        key(&mut session, "Home");
        key(&mut session, "Return");
        assert_eq!(session.stack.root_account(), None);
        let asked = |commands: &Receiver<BackendCommand>, inbox: &str| {
            commands.try_iter().any(|command| {
                matches!(command, BackendCommand::QueryEmails { mailbox_id, .. } if mailbox_id == inbox)
            })
        };
        assert!(asked(&test_cmds, "t1"));
        assert!(asked(&work_cmds, "w1"));

        let message = |id: &str, received: &str| crate::jmap::types::Email {
            id: id.to_string(),
            thread_id: None,
            from: None,
            to: None,
            cc: None,
            reply_to: None,
            subject: Some(format!("About {id}")),
            received_at: Some(received.to_string()),
            sent_at: None,
            preview: None,
            text_body: None,
            html_body: None,
            body_values: std::collections::HashMap::new(),
            keywords: std::collections::HashMap::new(),
            mailbox_ids: std::collections::HashMap::new(),
            message_id: None,
            references: None,
            attachments: None,
            extra: std::collections::HashMap::new(),
        };
        let answer = |inbox: &str, email| BackendResponse::Emails {
            mailbox_id: inbox.to_string(),
            emails: Ok(vec![email]),
            total: None,
            position: 0,
            loaded: 1,
            thread_counts: std::collections::HashMap::new(),
        };
        test_answer
            .send(answer("t1", message("m-test", "2026-02-01T09:00:00Z")))
            .unwrap();
        work_answer
            .send(answer("w1", message("m-work", "2026-02-02T09:00:00Z")))
            .unwrap();
        assert_eq!(session.poll(0), Flow::Continue);
        let shown = session.shown();
        assert!(
            shown.contains("About m-test") && shown.contains("About m-work"),
            "{shown}"
        );

        key(&mut session, "Home");
        key(&mut session, "Return");
        assert_eq!(session.stack.depth(), 2);
        assert_eq!(session.stack.top_account(), Some(1));
        assert!(work_cmds
            .try_iter()
            .any(|command| matches!(command, BackendCommand::GetEmail { id } if id == "m-work")));
        assert!(test_cmds.try_recv().is_err());
        // A draft from the message is the work account's to send.
        session
            .cmd_tx()
            .unwrap()
            .send(BackendCommand::FetchMailboxes {
                origin: "probe".to_string(),
            })
            .unwrap();
        assert!(work_cmds.try_iter().any(|command| matches!(
            command,
            BackendCommand::FetchMailboxes { origin } if origin == "probe"
        )));
        assert!(test_cmds.try_recv().is_err());
        // The test account's answer for the same id is not the message's.
        test_answer
            .send(BackendResponse::EmailBody {
                id: "m-work".to_string(),
                result: Box::new(Ok(message("m-work", "2026-02-02T09:00:00Z"))),
            })
            .unwrap();
        assert_eq!(session.poll(0), Flow::Continue);
        assert!(
            session.shown().contains("Loading email..."),
            "{}",
            session.shown()
        );
        // Its own account's answer is.
        work_answer
            .send(BackendResponse::EmailBody {
                id: "m-work".to_string(),
                result: Box::new(Ok(message("m-work", "2026-02-02T09:00:00Z"))),
            })
            .unwrap();
        assert_eq!(session.poll(0), Flow::Continue);
        assert!(
            !session.shown().contains("Loading email..."),
            "{}",
            session.shown()
        );

        // "All inboxes" again keeps the view, popped to; a draft composed
        // there is the account last shown's, and says so.
        key(&mut session, "Tab");
        key(&mut session, "Home");
        key(&mut session, "Return");
        assert_eq!(session.stack.depth(), 1);
        assert_eq!(session.stack.root_account(), None);
        assert!(session.shown().contains("c:compose from test"));
        key(&mut session, "c");
        assert!(session.editing(false));
        assert_eq!(session.stack.top_account(), Some(0));
        assert!(
            text(&session).starts_with("From: me@example.com\n"),
            "{}",
            text(&session)
        );
    }

    /// The connector's queue keeps one request per account: a newer one
    /// takes the older's place, and an urgent one goes first.
    #[test]
    fn the_connector_queue_keeps_one_request_per_account_urgent_first() {
        let (cmd_tx, _cmd_rx) = mpsc::channel();
        let request = |name: &str, url: &str, urgent: bool| {
            let mut account = account();
            account.name = name.to_string();
            account.well_known_url = url.to_string();
            ConnectRequest {
                account,
                cmd_tx: cmd_tx.clone(),
                urgent,
            }
        };
        let order = |queue: &VecDeque<ConnectRequest>| {
            queue
                .iter()
                .map(|r| format!("{}={}", r.account.name, r.account.well_known_url))
                .collect::<Vec<_>>()
        };
        let mut queue = VecDeque::new();
        enqueue(&mut queue, request("a", "1", false));
        enqueue(&mut queue, request("b", "1", false));
        enqueue(&mut queue, request("c", "1", false));
        enqueue(&mut queue, request("a", "2", false));
        assert_eq!(order(&queue), ["a=2", "b=1", "c=1"]);
        enqueue(&mut queue, request("c", "2", true));
        assert_eq!(order(&queue), ["c=2", "a=2", "b=1"]);
        enqueue(&mut queue, request("d", "1", true));
        assert_eq!(order(&queue), ["d=1", "c=2", "a=2", "b=1"]);
    }

    /// A connector that is gone is the connection's failure: the backend
    /// is told, so it holds nothing for a decision nothing would send.
    #[test]
    fn a_missing_connector_fails_the_connection() {
        let mut setup = setup();
        setup.offline = false;
        let (cmd_tx, cmd_rx) = mpsc::channel();
        setup.ask_connection(&account(), &cmd_tx, true);
        assert!(matches!(
            cmd_rx.try_recv(),
            Ok(BackendCommand::ConnectFailed(_))
        ));
        let (connector, requests) = mpsc::channel();
        drop(requests);
        setup.connector = Some(connector);
        setup.ask_connection(&account(), &cmd_tx, false);
        assert!(matches!(
            cmd_rx.try_recv(),
            Ok(BackendCommand::ConnectFailed(_))
        ));
    }

    /// Offline, `R` asks for no connection and says so; the list's fetch
    /// is answered from the cache.
    #[test]
    fn r_offline_says_so_and_fetches_from_the_cache() {
        let (mut session, cmd_rx, _resp_tx) = session(false);
        key(&mut session, "R");
        assert_eq!(
            session.note.as_deref(),
            Some("Not reconnected: td-mail was started --offline")
        );
        assert!(matches!(
            cmd_rx.try_recv(),
            Ok(BackendCommand::FetchMailboxes { origin }) if origin == "reconnect"
        ));
    }

    fn key(session: &mut Session, chord: &str) {
        key_with(session, chord, &mut NoClipboard);
    }

    /// `count` messages the retention policies would expire, `old 0`
    /// onward.
    fn candidates(count: usize) -> Vec<crate::backend::RetentionCandidate> {
        (0..count)
            .map(|n| crate::backend::RetentionCandidate {
                id: n.to_string(),
                mailbox: "Trash".to_string(),
                policy: "trash".to_string(),
                received_at: "2026-01-01".to_string(),
                from: "a@example.com".to_string(),
                subject: format!("old {n}"),
            })
            .collect()
    }

    /// A read-only text pushed over the stack: the retention preview of
    /// `count` messages.
    fn preview(count: usize) -> ViewAction {
        ViewAction::Push(Box::new(
            views::retention_preview::RetentionPreviewView::new(candidates(count)),
        ))
    }

    fn key_with(session: &mut Session, chord: &str, clipboard: &mut dyn Clipboard) {
        let input = Input::Key {
            chord,
            repeat: false,
        };
        session.input(input, clipboard);
    }

    /// A clipboard that records what it is asked, refused or not, and
    /// answers as told.
    struct Board {
        text: bool,
        refuse: Option<Refusal>,
        copies: Vec<Arc<str>>,
        pastes: usize,
        attempts: usize,
    }

    impl Board {
        fn new() -> Self {
            Board {
                text: false,
                refuse: None,
                copies: Vec::new(),
                pastes: 0,
                attempts: 0,
            }
        }
    }

    impl Clipboard for Board {
        fn available(&self) -> bool {
            true
        }
        fn has_text(&self) -> bool {
            self.text
        }
        fn pasting(&self) -> bool {
            false
        }
        fn copy(&mut self, text: Arc<str>) -> Result<(), Refusal> {
            self.attempts += 1;
            if let Some(refusal) = self.refuse {
                return Err(refusal);
            }
            self.copies.push(text);
            Ok(())
        }
        fn paste(&mut self) -> Result<(), Refusal> {
            self.attempts += 1;
            if let Some(refusal) = self.refuse {
                return Err(refusal);
            }
            self.pastes += 1;
            Ok(())
        }
    }

    /// Plays the backend's part for the attach commands sent so far: each
    /// copy made as the backend makes it, under the fetch service's bound,
    /// answered and polled; the other commands drained are answered.
    fn serve_attach(
        session: &mut Session,
        cmd_rx: &Receiver<BackendCommand>,
        resp_tx: &Sender<BackendResponse>,
    ) -> Vec<BackendCommand> {
        let mut others = Vec::new();
        while let Ok(command) = cmd_rx.try_recv() {
            match command {
                BackendCommand::AttachFile {
                    draft,
                    sidecar,
                    source,
                } => {
                    let result =
                        attach::attach_file(&draft, sidecar.as_deref(), &source, attach::CEILING)
                            .map_err(|e| e.to_string());
                    resp_tx
                        .send(BackendResponse::FileAttached {
                            draft,
                            source,
                            result,
                        })
                        .unwrap();
                }
                other => others.push(other),
            }
        }
        session.poll(0);
        others
    }

    fn press(session: &mut Session, x: i64, y: i64) {
        for phase in [PointerPhase::Press, PointerPhase::Release] {
            session.input(
                Input::Pointer {
                    phase,
                    x,
                    y,
                    extend: false,
                    follow: false,
                },
                &mut NoClipboard,
            );
        }
    }

    /// A press and release with Control held.
    fn follow(session: &mut Session, x: i64, y: i64) {
        for phase in [PointerPhase::Press, PointerPhase::Release] {
            session.input(
                Input::Pointer {
                    phase,
                    x,
                    y,
                    extend: false,
                    follow: phase == PointerPhase::Press,
                },
                &mut NoClipboard,
            );
        }
    }

    /// Pushes a message's view and answers its body with `preview`, so
    /// the pane shows it.
    fn show_message(session: &mut Session, resp_tx: &Sender<BackendResponse>, preview: String) {
        let view = views::email_view::EmailView::new(
            session.cmd_tx().unwrap(),
            "me@example.com".to_string(),
            "e1".to_string(),
            Vec::new(),
            0,
            false,
            Vec::new(),
            "Archive".to_string(),
            "Trash".to_string(),
            None,
        );
        session.act(ViewAction::Push(Box::new(view)));
        let email = crate::jmap::types::Email {
            id: "e1".to_string(),
            thread_id: None,
            from: None,
            to: None,
            cc: None,
            reply_to: None,
            subject: Some("Hello".to_string()),
            received_at: Some("2025-01-01".to_string()),
            sent_at: None,
            preview: Some(preview),
            text_body: None,
            html_body: None,
            body_values: std::collections::HashMap::new(),
            keywords: std::collections::HashMap::new(),
            mailbox_ids: std::collections::HashMap::new(),
            message_id: None,
            references: None,
            attachments: None,
            extra: std::collections::HashMap::new(),
        };
        resp_tx
            .send(BackendResponse::EmailBody {
                id: "e1".to_string(),
                result: Box::new(Ok(email)),
            })
            .unwrap();
        session.poll(0);
    }

    /// A Control-press on a link in a message opens it through the
    /// configured browser, the note saying what the opener answered, and
    /// is no press of the pane's: the caret stays. Off a link it is a
    /// plain press, as a press without Control on the link is.
    #[test]
    fn a_control_press_on_a_link_opens_it_and_elsewhere_is_a_plain_press() {
        let (mut session, _cmd_rx, resp_tx) = session(true);
        // A browser that cannot start: the test opens nothing.
        let browser = "/nonexistent/td-mail-browser";
        // Past the pane's columns: the wrap must not break it.
        const LONG: &str = "https://e.example/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        session.setup.browser = Some(browser.to_string());
        show_message(
            &mut session,
            &resp_tx,
            format!("see https://e.example/x now {LONG} end"),
        );
        let text = session.shown();
        assert!(text.contains("https://e.example/x"), "{text}");
        let rect = session.shape().unwrap().layout.pane.expect("a pane");
        let points = (0..i64::from(rect.height)).step_by(4).flat_map(|dy| {
            (0..i64::from(rect.width))
                .step_by(4)
                .map(move |dx| (rect.x + dx, rect.y + dy))
        });
        let mut on = Vec::new();
        let mut off = Vec::new();
        let mut long = 0;
        for (x, y) in points {
            match session.pane.link(x, y).as_deref() {
                Some("https://e.example/x") => on.push((x, y)),
                Some(LONG) => long += 1,
                Some(other) => panic!("a link cut short: {other}"),
                None => off.push((x, y)),
            }
        }
        assert!(
            long > 4 * i64::from(rect.width) / 8,
            "the long link is shown whole on its rows: {long}"
        );
        let first = *on.first().expect("the link is shown");
        let row: Vec<_> = on.iter().filter(|point| point.1 == first.1).collect();
        // Past the link's end on its row.
        let end = row[row.len() - 1].0;
        let off = *off
            .iter()
            .find(|point| point.1 == first.1 && point.0 > end)
            .expect("the row goes on past the link");
        let selection = |session: &Session| {
            let tab = session.pane.tab().unwrap();
            session.pane.editor().document(tab).unwrap().selection()
        };
        let tab = session.pane.tab().unwrap();
        let shown = session
            .pane
            .editor()
            .document(tab)
            .unwrap()
            .text()
            .to_string();
        // Without Control a press on the link is the pane's: the caret
        // lands inside it. The press is one whose caret sits between two
        // letters, where a second click would select a word; the clock
        // steps between the tries so no two of them pair.
        let mut now = 0;
        let mut pick = None;
        for &&point in &row {
            now += 1000;
            session.poll(now);
            press(&mut session, point.0, point.1);
            let caret = selection(&session).caret;
            let letters = shown
                .get(caret - 1..caret + 1)
                .is_some_and(|pair| pair.bytes().all(|b| b.is_ascii_alphabetic()));
            if letters {
                pick = Some(point);
                break;
            }
        }
        let middle = pick.expect("a press between two letters of the link");
        let placed = selection(&session);
        assert!(
            shown
                .match_indices("https://e.example/x")
                .any(|(start, _)| placed.caret > start && placed.caret < start + 19),
            "{placed:?} {shown}"
        );
        assert!(session.note.is_none());
        // With Control it opens the link, and the caret stays.
        follow(&mut session, first.0, first.1);
        let note = session.note.clone().unwrap_or_default();
        assert!(
            note.starts_with(&format!("link not opened: {browser}: ")),
            "{note}"
        );
        assert_eq!(selection(&session), placed, "the press was not the pane's");
        assert!(!session.pane.drag);
        // A plain press on the link just after the followed one is a first
        // click, not the second of a pair with the press before it.
        press(&mut session, middle.0, middle.1);
        assert_eq!(selection(&session), placed, "a first click, not a word");
        // With Control off a link it is a plain press.
        follow(&mut session, off.0, off.1);
        assert_ne!(selection(&session), placed);
        assert!(session.note.is_none());
    }

    /// While Control is held over a link in a message the pane underlines
    /// the link a Control-press there would follow, asking a redraw only
    /// when what is underlined changes; off the link, outside the pane,
    /// with the mouse off or once Control is up, nothing is.
    #[test]
    fn control_held_over_a_link_underlines_it_in_the_pane() {
        let (mut session, _cmd_rx, resp_tx) = session(true);
        show_message(&mut session, &resp_tx, "see https://e.example/x now".into());
        assert!(session.shown().contains("https://e.example/x"));
        let rect = session.shape().unwrap().layout.pane.expect("a pane");
        let points: Vec<_> = (0..i64::from(rect.height))
            .step_by(4)
            .flat_map(|dy| {
                (0..i64::from(rect.width))
                    .step_by(4)
                    .map(move |dx| (rect.x + dx, rect.y + dy))
            })
            .collect();
        let on: Vec<_> = points
            .iter()
            .copied()
            .filter(|&(x, y)| session.pane.link(x, y).is_some())
            .collect();
        let off = points
            .iter()
            .copied()
            .find(|&(x, y)| session.pane.link(x, y).is_none())
            .expect("text off the link");
        assert!(on.len() > 1, "the link is shown");
        let hover = |session: &mut Session, at: Option<(i64, i64)>| {
            session.dirty = false;
            session.input(Input::Hover(at), &mut NoClipboard);
            session.dirty
        };
        let link = Some("https://e.example/x".to_string());
        assert!(hover(&mut session, Some(on[0])));
        assert_eq!(session.pane.hovered(), link);
        assert!(!hover(&mut session, Some(on[1])), "the same link");
        assert!(hover(&mut session, Some(off)));
        assert_eq!(session.pane.hovered(), None);
        assert!(hover(&mut session, Some(on[0])));
        assert!(hover(&mut session, Some((rect.x - 1, rect.y))));
        assert_eq!(session.pane.hovered(), None, "outside the pane");
        assert!(hover(&mut session, Some(on[0])));
        assert!(hover(&mut session, None));
        assert_eq!(session.pane.hovered(), None);
        // With the mouse off a press follows nothing, so nothing shows.
        session.mouse = false;
        assert!(!hover(&mut session, Some(on[0])));
        assert_eq!(session.pane.hovered(), None);
        // Turned on again, the next input finds the point Control is over.
        session.mouse = true;
        assert!(hover(&mut session, Some(on[0])));
        assert_eq!(session.pane.hovered(), link);
    }

    /// In a draft the underline keeps to where a Control-press would
    /// follow: the finder over the body and the draft's save question
    /// each take the press, so nothing is underlined under them, and the
    /// frame finds a gate lifted without another input.
    #[test]
    fn a_draft_underlines_a_link_only_where_a_press_would_follow() {
        let (mut session, _cmd_rx, _resp_tx) = session(true);
        let folder = session.setup.draft_dir.clone().unwrap();
        session.attach_folder = Some(folder);
        key(&mut session, "c");
        session.shown();
        assert!(session
            .pane
            .insert("see https://e.example/x now\n")
            .unwrap());
        session.shown();
        let rect = session.shape().unwrap().layout.pane.expect("a pane");
        let on = (0..i64::from(rect.height))
            .step_by(4)
            .flat_map(|dy| {
                (0..i64::from(rect.width))
                    .step_by(4)
                    .map(move |dx| (rect.x + dx, rect.y + dy))
            })
            .find(|&(x, y)| session.pane.link(x, y).is_some())
            .expect("the link is shown");
        let link = Some("https://e.example/x".to_string());
        session.input(Input::Hover(Some(on)), &mut NoClipboard);
        assert_eq!(session.pane.hovered(), link);
        key(&mut session, "C-S-a");
        assert!(session.chooser.is_some());
        assert_eq!(session.pane.hovered(), None, "the finder takes the press");
        key(&mut session, "Escape");
        assert!(session.chooser.is_none());
        assert_eq!(session.pane.hovered(), link);
        key(&mut session, "C-w");
        assert!(session.editing(true) && !session.editing(false), "asking");
        assert_eq!(session.pane.hovered(), None, "the question takes the press");
        key(&mut session, "Escape");
        assert!(session.editing(false));
        assert_eq!(session.pane.hovered(), link);
        // The mouse turned on between inputs: the frame shows the link.
        session.mouse = false;
        session.input(Input::Hover(Some(on)), &mut NoClipboard);
        assert_eq!(session.pane.hovered(), None);
        session.mouse = true;
        session.shown();
        assert_eq!(session.pane.hovered(), link);
    }

    /// The frame reads back as text: the mailboxes with their counts,
    /// the title the header row was, then the folder opened by Return
    /// with the backend asked for its messages; `?` asks the window for
    /// its key list, the shown view's keys first, and changes nothing;
    /// then a text in the pane, read-only, where a chord the client does
    /// not claim edits nothing, and back.
    #[test]
    fn the_frame_shows_the_mailboxes_and_opens_one_and_a_text_in_the_pane() {
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
        assert!(!session.take_show_keys());
        let title = session.title().to_string();
        let text = session.shown();
        key(&mut session, "?");
        assert!(session.take_show_keys(), "`?` asks for the key list");
        assert!(!session.take_show_keys(), "once");
        assert_eq!(session.stack.depth(), 1);
        assert_eq!(session.title(), title);
        assert_eq!(session.shown(), text, "the view is as it was");
        let first = |session: &Session| session.keys().first().map(|section| section.title);
        assert_eq!(first(&session), Some("Mailbox List"));
        assert_eq!(session.keys().len(), 10);
        key(&mut session, "Return");
        assert_eq!(first(&session), Some("Email List"));
        key(&mut session, "?");
        assert!(session.take_show_keys());
        assert_eq!(session.stack.depth(), 2);
        key(&mut session, "q");
        session.act(preview(3));
        assert_eq!(session.stack.depth(), 2);
        assert_eq!(session.title(), "Retention expiry preview");
        let text = session.shown();
        assert!(text.contains("old 2"), "{text}");
        let tab = session.pane.tab().expect("the pane holds the preview");
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
        session.input(
            Input::Wheel {
                rows: 2,
                columns: 0,
            },
            &mut NoClipboard,
        );
        key(&mut session, "Return");
        session.shown();
        assert!(
            session.title().starts_with("Archive"),
            "{}",
            session.title()
        );
        key(&mut session, "q");
        session.input(
            Input::Wheel {
                rows: -isize::MAX,
                columns: 0,
            },
            &mut NoClipboard,
        );
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
        session.input(
            Input::Wheel {
                rows: 2,
                columns: 0,
            },
            &mut NoClipboard,
        );
        key(&mut session, "Return");
        session.shown();
        assert!(session.title().starts_with("INBOX"), "{}", session.title());
    }

    /// A text entry: the folder name typed shows in the band, Escape
    /// leaves it; over a text, the wheel and the reading keys scroll
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
        session.act(preview(200));
        session.shown();
        let page = session.page();
        assert!(page > 1, "{page}");
        assert_eq!(session.pane.first_row(), Some(0));
        session.input(
            Input::Wheel {
                rows: 3,
                columns: 0,
            },
            &mut NoClipboard,
        );
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
        // A text over a text: the preview's rows arrive, another text
        // opens over it and closes, and a reading key right after, before
        // any paint, moves the preview, whose document the pane holds
        // again.
        resp_tx
            .send(BackendResponse::RetentionPreview {
                result: Ok(crate::backend::RetentionPreviewResult {
                    candidates: candidates(40),
                }),
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
        session.act(preview(5));
        assert_eq!(session.pane.editor().tabs().count(), 2);
        assert_eq!(session.stack.depth(), 3);
        key(&mut session, "q");
        key(&mut session, "j");
        assert_eq!(
            session.pane.first_row(),
            Some(2),
            "popped: the pane's again"
        );
        assert_eq!(session.pane.editor().tabs().count(), 1);
    }

    /// Ctrl-Enter, as the Send label, saves an unsaved draft and hands
    /// its path to the backend as `SendDraft`; while the answer is
    /// awaited a second send, a save and a close are refused in the
    /// status row and the draft is held read-only, so the file sent is
    /// the file retired; a refusal gives the draft back with the reason;
    /// a sent answer retires the draft and its sidecar to the `sent`
    /// directory beside `drafts` and pops the view; and a draft that
    /// cannot be retired stays open, sent and held, saying so.
    #[test]
    fn sending_a_draft_hands_it_to_the_backend_and_retires_it_when_sent() {
        let (mut session, cmd_rx, resp_tx) = session(true);
        let draft_dir = session.setup.draft_dir.clone().unwrap();
        let sent_dir = draft_dir.parent().unwrap().join("sent");
        let path_of =
            |session: &Session| draft_dir.join(session.title.trim_start_matches("Draft "));
        let status = |session: &Session| session.stack.current().unwrap().scene().status;
        // The command channel also carries the mailbox list's refreshes:
        // the send is the `SendDraft` among what has been sent, if any.
        let sent_draft = |cmd_rx: &Receiver<BackendCommand>| -> Option<PathBuf> {
            let mut found = None;
            while let Ok(command) = cmd_rx.try_recv() {
                if let BackendCommand::SendDraft { path } = command {
                    found = Some(path);
                }
            }
            found
        };
        key(&mut session, "c");
        let path = path_of(&session);
        let template = std::fs::read_to_string(&path).unwrap();
        key(&mut session, "C-End");
        key(&mut session, "h");
        // While the view asks about the draft, Send is not a key of its
        // and the bar has no Send label: nothing is sent.
        key(&mut session, "C-w");
        assert!(session.title().starts_with("Save "), "{}", session.title());
        key(&mut session, "C-Return");
        assert_eq!(sent_draft(&cmd_rx), None, "not while asking");
        key(&mut session, "Escape");
        assert!(session.title().starts_with("Draft "), "{}", session.title());
        key(&mut session, "C-Return");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("{template}h"),
            "saved before sending"
        );
        assert_eq!(sent_draft(&cmd_rx), Some(path.clone()));
        assert!(
            status(&session).starts_with("Sending "),
            "{}",
            status(&session)
        );
        // Meanwhile: no second send, no save, no close, and the draft is
        // held read-only, typing ignored, the window's close request
        // put to the view too.
        key(&mut session, "C-Return");
        assert_eq!(sent_draft(&cmd_rx), None, "one send at a time");
        assert!(status(&session).contains("already"), "{}", status(&session));
        key(&mut session, "C-w");
        assert_eq!(session.stack.depth(), 2);
        assert!(
            status(&session).contains("closes when"),
            "{}",
            status(&session)
        );
        key(&mut session, "i");
        key(&mut session, "q");
        assert_eq!(text(&session), format!("{template}h"), "held read-only");
        assert_eq!(session.stack.depth(), 2, "the pane's keys, not the list's");
        key(&mut session, "C-s");
        assert!(
            status(&session).contains("wait for the server"),
            "{}",
            status(&session)
        );
        assert_eq!(
            session.input(Input::Close, &mut NoClipboard),
            Flow::Continue
        );
        assert!(session.closing && !session.quitting, "the close waits");
        assert!(
            status(&session).contains("closes when"),
            "{}",
            status(&session)
        );
        // A refusal: the draft is the pane's again with the reason shown,
        // the window's close is over, and Send works again, saving the
        // new edit first.
        resp_tx
            .send(BackendResponse::DraftSent {
                path: path.clone(),
                result: Err("no identity sends as me@example.com".to_string()),
            })
            .unwrap();
        assert_eq!(session.poll(0), Flow::Continue);
        assert!(!session.closing && !session.quitting, "the close is over");
        assert_eq!(session.stack.depth(), 2);
        assert_eq!(
            status(&session),
            "Not sent: no identity sends as me@example.com"
        );
        key(&mut session, "i");
        assert_eq!(text(&session), format!("{template}hi"));
        key(&mut session, "C-Return");
        assert_eq!(sent_draft(&cmd_rx), Some(path.clone()));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("{template}hi")
        );
        // Sent: the draft is retired beside drafts and the view pops.
        resp_tx
            .send(BackendResponse::DraftSent {
                path: path.clone(),
                result: Ok(backend::SentDraft {
                    email_id: "e1".to_string(),
                    submission_id: "s1".to_string(),
                    kept_in: "Sent".to_string(),
                }),
            })
            .unwrap();
        session.poll(0);
        assert_eq!(session.stack.depth(), 1);
        assert_eq!(session.pane.editor().tabs().count(), 0);
        assert!(!path.exists());
        let retired = sent_dir.join(path.file_name().unwrap());
        assert_eq!(
            std::fs::read_to_string(&retired).unwrap(),
            format!("{template}hi")
        );
        // An answer for a draft no view holds is nothing.
        resp_tx
            .send(BackendResponse::DraftSent {
                path: path.clone(),
                result: Err("late".to_string()),
            })
            .unwrap();
        session.poll(0);
        assert_eq!(session.stack.depth(), 1);
        // A draft that cannot be retired (its name already in sent) stays
        // open, sent, the status saying so; Close then pops it.
        key(&mut session, "c");
        let second = path_of(&session);
        std::fs::write(sent_dir.join(second.file_name().unwrap()), "taken").unwrap();
        key(&mut session, "C-Return");
        assert_eq!(sent_draft(&cmd_rx), Some(second.clone()));
        resp_tx
            .send(BackendResponse::DraftSent {
                path: second.clone(),
                result: Ok(backend::SentDraft {
                    email_id: "e2".to_string(),
                    submission_id: "s2".to_string(),
                    kept_in: "Drafts".to_string(),
                }),
            })
            .unwrap();
        session.poll(0);
        assert_eq!(session.stack.depth(), 2);
        assert!(second.exists());
        assert!(
            status(&session).starts_with("Sent, kept in Drafts; not retired: "),
            "{}",
            status(&session)
        );
        // Sent, the draft is held: what is retired is what went.
        let sent_text = text(&session);
        key(&mut session, "i");
        assert_eq!(text(&session), sent_text, "held read-only once sent");
        // Send again sends nothing: it retries the move, which succeeds
        // once the name is free, and the view pops.
        key(&mut session, "C-Return");
        assert_eq!(sent_draft(&cmd_rx), None, "not sent twice");
        assert_eq!(session.stack.depth(), 2);
        std::fs::remove_file(sent_dir.join(second.file_name().unwrap())).unwrap();
        key(&mut session, "C-Return");
        assert_eq!(sent_draft(&cmd_rx), None, "still not sent twice");
        assert_eq!(session.stack.depth(), 1);
        assert!(!second.exists());
        assert!(sent_dir.join(second.file_name().unwrap()).exists());
        // The bar's Send label is the same request.
        key(&mut session, "c");
        let send = session
            .shape()
            .unwrap()
            .layout
            .bar
            .header(0)
            .expect("send label");
        press(&mut session, send.x + 2, send.y + 2);
        assert_eq!(sent_draft(&cmd_rx), Some(path_of(&session)));
        // The window's close asked during that send is kept: sent, the
        // draft retires, the view pops and the window follows.
        let third = path_of(&session);
        assert_eq!(
            session.input(Input::Close, &mut NoClipboard),
            Flow::Continue
        );
        assert!(session.closing && !session.quitting);
        resp_tx
            .send(BackendResponse::DraftSent {
                path: third.clone(),
                result: Ok(backend::SentDraft {
                    email_id: "e3".to_string(),
                    submission_id: "s3".to_string(),
                    kept_in: "Sent".to_string(),
                }),
            })
            .unwrap();
        assert_eq!(session.poll(0), Flow::Quit);
        assert!(session.quitting);
        assert!(!third.exists() && sent_dir.join(third.file_name().unwrap()).exists());
    }

    /// Ctrl-Shift-A, or the Attach label, opens the finder over the body
    /// on the folder given, where the keys are the finder's: a letter
    /// filters, Return descends into a folder and chooses a file, a
    /// second press on a row chooses it; the file chosen is copied into
    /// the draft's sidecar, its tag added at the draft's end, the next
    /// finder opening where it came from; Escape attaches nothing; the
    /// sidecar retires with the sent draft; and while the send is
    /// awaited nothing can be attached.
    #[test]
    fn attach_copies_the_file_chosen_into_the_drafts_sidecar_and_tags_it() {
        let (mut session, cmd_rx, resp_tx) = session(true);
        let draft_dir = session.setup.draft_dir.clone().unwrap();
        let state = draft_dir.parent().unwrap().to_path_buf();
        let status = |session: &Session| session.stack.current().unwrap().scene().status;
        key(&mut session, "c");
        let path = draft_dir.join(session.title.trim_start_matches("Draft "));
        let template = std::fs::read_to_string(&path).unwrap();
        let id = path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .trim_start_matches("td-mail-draft-")
            .trim_end_matches(".eml")
            .to_string();
        let sidecar = draft_dir.join(format!("td-mail-att-{id}"));
        let files = state.join("files");
        std::fs::create_dir_all(files.join("docs")).unwrap();
        std::fs::write(files.join("docs/report.pdf"), b"%PDF").unwrap();
        std::fs::write(files.join("notes.txt"), b"n").unwrap();
        session.attach_folder = Some(files.clone());

        key(&mut session, "C-S-a");
        assert_eq!(session.chooser.as_ref().unwrap().folder, files);
        let shown = session.shown();
        assert!(
            shown.contains("docs") && shown.contains("notes.txt"),
            "{shown}"
        );
        assert!(
            status(&session).starts_with("Attach: Return"),
            "{}",
            status(&session)
        );
        key(&mut session, "d");
        key(&mut session, "o");
        assert_eq!(text(&session), template, "the letters filtered");
        key(&mut session, "Return");
        assert_eq!(session.chooser.as_ref().unwrap().folder, files.join("docs"));
        // Backspace on the empty filter goes up, onto the folder it came
        // from; Return goes back in.
        key(&mut session, "Backspace");
        let chooser = session.chooser.as_ref().unwrap();
        assert_eq!(chooser.folder, files);
        assert_eq!(chooser.finder.selected_entry().unwrap().name(), "docs");
        key(&mut session, "Return");
        assert_eq!(session.chooser.as_ref().unwrap().folder, files.join("docs"));
        key(&mut session, "Return");
        assert!(session.chooser.is_none());
        // The copy is the backend's: until it answers nothing is tagged,
        // and the draft is not sent, closed or attached to again, though
        // it is still the pane's to type in.
        assert_eq!(text(&session), template);
        let attaching = |session: &Session| status(session).starts_with("Attaching report.pdf");
        assert!(attaching(&session), "{}", status(&session));
        key(&mut session, "C-Return");
        assert!(attaching(&session), "{}", status(&session));
        key(&mut session, "C-w");
        assert!(attaching(&session), "{}", status(&session));
        assert_eq!(session.stack.depth(), 2);
        // The window's close waits for the copy too, the draft clean.
        assert!(!session.draft().dirty());
        assert_eq!(session.close_requested(), Flow::Continue);
        assert_eq!(session.stack.depth(), 2);
        key(&mut session, "C-S-a");
        assert!(session.chooser.is_none() && attaching(&session));
        let others = serve_attach(&mut session, &cmd_rx, &resp_tx);
        assert!(
            !others
                .iter()
                .any(|c| matches!(c, BackendCommand::SendDraft { .. })),
            "nothing is sent before its tag is in"
        );
        let tag = |name: &str, shown: &str| {
            format!(
                "<#part type=\"application/pdf\" filename=\"{}\" disposition=\"attachment\"{shown}>\n<#/part>\n",
                sidecar.join(name).display()
            )
        };
        assert_eq!(std::fs::read(sidecar.join("report.pdf")).unwrap(), b"%PDF");
        assert_eq!(
            text(&session),
            format!("{template}{}", tag("report.pdf", ""))
        );
        assert!(
            status(&session).starts_with("Attached report.pdf (4 B)"),
            "{}",
            status(&session)
        );

        // The Attach label opens it again where the file came from, and
        // a second press on the row chooses: a second copy, named for
        // the recipient as the file was.
        let shape = session.shape().unwrap();
        let label = shape
            .keys
            .iter()
            .position(|&key| key == Key::Request("attach"))
            .and_then(|index| shape.layout.bar.header(index))
            .expect("the Attach label");
        press(&mut session, label.x + 2, label.y + 2);
        let list = session
            .chooser
            .as_ref()
            .expect("the finder")
            .finder
            .list_rect();
        assert_eq!(session.chooser.as_ref().unwrap().folder, files.join("docs"));
        press(&mut session, list.x + 4, list.y + 4);
        assert!(session.chooser.is_some(), "one press selects");
        press(&mut session, list.x + 4, list.y + 4);
        assert!(session.chooser.is_none(), "the second chooses");
        serve_attach(&mut session, &cmd_rx, &resp_tx);
        assert_eq!(
            text(&session),
            format!(
                "{template}{}{}",
                tag("report.pdf", ""),
                tag("report-2.pdf", " name=\"report.pdf\"")
            )
        );

        let attached = text(&session);

        // A copy the backend refuses tags nothing, the reason in the status;
        // an answer for another file than the one awaited is not this one.
        key(&mut session, "C-S-a");
        key(&mut session, "Return");
        while cmd_rx.try_recv().is_ok() {}
        let answer = |source: &Path, result: Result<attach::Attached, String>| {
            BackendResponse::FileAttached {
                draft: path.clone(),
                source: source.to_path_buf(),
                result,
            }
        };
        resp_tx
            .send(answer(&files.join("other.pdf"), Err("stray".to_string())))
            .unwrap();
        session.poll(0);
        assert!(attaching(&session), "{}", status(&session));
        resp_tx
            .send(answer(
                &files.join("docs/report.pdf"),
                Err("attachment is 4 bytes, past the 3 the server takes".to_string()),
            ))
            .unwrap();
        session.poll(0);
        assert_eq!(
            status(&session),
            "Not attached: attachment is 4 bytes, past the 3 the server takes"
        );
        assert_eq!(text(&session), attached);

        // A press on the row and then on the list below the rows, or two
        // with the mouse off, choose nothing.
        key(&mut session, "C-S-a");
        let list = session.chooser.as_ref().unwrap().finder.list_rect();
        press(&mut session, list.x + 4, list.y + 4);
        press(
            &mut session,
            list.x + 4,
            list.y + i64::from(list.height) - 2,
        );
        assert!(session.chooser.is_some(), "the blank is not the row");
        session.mouse = false;
        press(&mut session, list.x + 4, list.y + 4);
        press(&mut session, list.x + 4, list.y + 4);
        assert!(session.chooser.is_some(), "the mouse is off");
        session.mouse = true;

        // The window's close closes the finder with nothing attached and
        // then asks about the unsaved draft; Escape keeps both.
        assert_eq!(
            session.input(Input::Close, &mut NoClipboard),
            Flow::Continue
        );
        assert!(session.chooser.is_none());
        assert!(session.title.starts_with("Save "), "{}", session.title);
        key(&mut session, "Escape");
        assert!(session.title.starts_with("Draft "), "{}", session.title);

        // A finder whose draft is no longer shown is closed with a note,
        // and the draft, shown again, says nothing was attached.
        key(&mut session, "C-S-a");
        session.act(preview(1));
        session.poll(0);
        assert!(session.chooser.is_none());
        session.act(ViewAction::Pop);
        assert_eq!(status(&session), "Nothing attached");
        assert_eq!(text(&session), attached);

        // Escape attaches nothing, and the next finder still opens where
        // the last file came from; one that can no longer be listed is
        // forgotten for the start folder.
        key(&mut session, "C-S-a");
        assert_eq!(session.chooser.as_ref().unwrap().folder, files.join("docs"));
        key(&mut session, "Escape");
        assert!(session.chooser.is_none());
        assert_eq!(status(&session), "Nothing attached");
        key(&mut session, "C-S-a");
        assert_eq!(session.chooser.as_ref().unwrap().folder, files.join("docs"));
        key(&mut session, "Escape");
        session.attach_folder = Some(files.join("gone"));
        key(&mut session, "C-S-a");
        assert_eq!(
            session.chooser.as_ref().unwrap().folder,
            attach::start_folder()
        );
        assert_eq!(session.attach_folder, None);
        key(&mut session, "Escape");

        // Sent: the sidecar retires with the draft; while the send is
        // awaited, Attach is refused.
        key(&mut session, "C-Return");
        let mut sent = false;
        while let Ok(command) = cmd_rx.try_recv() {
            sent |= matches!(command, BackendCommand::SendDraft { .. });
        }
        assert!(sent);
        key(&mut session, "C-S-a");
        assert!(session.chooser.is_none());
        assert!(
            status(&session).contains("wait for the server"),
            "{}",
            status(&session)
        );
        resp_tx
            .send(BackendResponse::DraftSent {
                path: path.clone(),
                result: Ok(backend::SentDraft {
                    email_id: "e1".to_string(),
                    submission_id: "s1".to_string(),
                    kept_in: "Sent".to_string(),
                }),
            })
            .unwrap();
        session.poll(0);
        assert_eq!(session.stack.depth(), 1);
        let retired = state.join("sent").join(sidecar.file_name().unwrap());
        assert!(!sidecar.exists());
        assert_eq!(
            std::fs::read(retired.join("report-2.pdf")).unwrap(),
            b"%PDF"
        );
    }

    /// With the caret in the body the tag lands at the caret's line when
    /// the copy is made, the caret staying where it was typing.
    #[test]
    fn an_attachment_goes_at_the_carets_line_in_the_body() {
        let (mut session, cmd_rx, resp_tx) = session(true);
        let state = session
            .setup
            .draft_dir
            .clone()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        key(&mut session, "c");
        let files = state.join("files");
        std::fs::create_dir_all(&files).unwrap();
        std::fs::write(files.join("notes.txt"), b"n").unwrap();
        session.attach_folder = Some(files.clone());
        key(&mut session, "C-End");
        for chord in ["h", "i", "Return", "b", "y", "e", "Up"] {
            key(&mut session, chord);
        }
        key(&mut session, "C-S-a");
        key(&mut session, "Return");
        serve_attach(&mut session, &cmd_rx, &resp_tx);
        let body = text(&session);
        let tail = body
            .split("--text follows this line--\n")
            .nth(1)
            .unwrap_or_default()
            .to_string();
        assert!(tail.trim_start().starts_with("hi\n<#part "), "{tail}");
        assert!(tail.ends_with("<#/part>\nbye"), "{tail}");
        key(&mut session, "x");
        assert!(
            text(&session).contains("hix\n<#part "),
            "{}",
            text(&session)
        );
    }

    /// Ctrl-H in the finder lists the names beginning `.` and leaves them
    /// out again, the filter cleared and the selection kept (the first
    /// when it is hidden), the status row saying which, the draft
    /// untouched and a held key's repeats ignored; a folder that cannot
    /// be listed again keeps the setting; the next finder opens as the
    /// last was left.
    #[test]
    fn ctrl_h_in_the_finder_shows_and_hides_hidden_files() {
        let (mut session, _cmd_rx, _resp_tx) = session(true);
        let state = session
            .setup
            .draft_dir
            .clone()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        key(&mut session, "c");
        let template = text(&session);
        let files = state.join("files");
        std::fs::create_dir_all(files.join(".config")).unwrap();
        std::fs::write(files.join(".profile"), b"p").unwrap();
        std::fs::write(files.join("notes.txt"), b"n").unwrap();
        std::fs::write(files.join("zeta.txt"), b"z").unwrap();
        session.attach_folder = Some(files.clone());
        let names = |session: &Session| -> Vec<String> {
            let chooser = session.chooser.as_ref().unwrap();
            chooser
                .finder
                .listing()
                .entries()
                .iter()
                .map(|entry| entry.name().to_string())
                .collect()
        };
        let note = |session: &Session| session.chooser.as_ref().unwrap().finder.note().to_string();

        key(&mut session, "C-S-a");
        assert_eq!(names(&session), ["notes.txt", "zeta.txt"]);
        key(&mut session, "n");
        assert_eq!(session.chooser.as_ref().unwrap().finder.query(), "n");
        key(&mut session, "C-h");
        assert_eq!(
            names(&session),
            [".config", ".profile", "notes.txt", "zeta.txt"]
        );
        assert_eq!(session.chooser.as_ref().unwrap().finder.query(), "");
        let chooser = session.chooser.as_ref().unwrap();
        assert_eq!(chooser.folder, files);
        assert_eq!(chooser.finder.selected_entry().unwrap().name(), "notes.txt");
        assert_eq!(note(&session), "Hidden files shown; Ctrl-H hides them");
        assert!(session.shown().contains(".profile"), "{}", session.shown());
        assert_eq!(text(&session), template, "the chord is the finder's");
        // A held Ctrl-H's repeats change nothing.
        session.input(
            Input::Key {
                chord: "C-h",
                repeat: true,
            },
            &mut NoClipboard,
        );
        assert_eq!(names(&session).len(), 4);
        // Hidden again in the same finder with a dot file selected: the
        // selection goes to the first listed.
        key(&mut session, "Up");
        assert_eq!(
            session
                .chooser
                .as_ref()
                .unwrap()
                .finder
                .selected_entry()
                .unwrap()
                .name(),
            ".profile"
        );
        key(&mut session, "C-h");
        assert_eq!(names(&session), ["notes.txt", "zeta.txt"]);
        assert_eq!(note(&session), "Hidden files left out; Ctrl-H shows them");
        let chooser = session.chooser.as_ref().unwrap();
        assert_eq!(chooser.finder.selected_entry().unwrap().name(), "notes.txt");
        key(&mut session, "C-h");
        assert_eq!(names(&session).len(), 4);
        // A folder that cannot be listed again keeps the setting and the
        // listing, the reason in the status row.
        std::fs::rename(&files, state.join("moved")).unwrap();
        key(&mut session, "C-h");
        assert!(session.attach_hidden, "still shown");
        assert_eq!(names(&session).len(), 4);
        assert!(
            note(&session).contains("No such file"),
            "the reason, not the toggle's note: {}",
            note(&session)
        );
        std::fs::rename(state.join("moved"), &files).unwrap();
        key(&mut session, "Escape");
        assert!(session.chooser.is_none());

        key(&mut session, "C-S-a");
        assert_eq!(
            names(&session),
            [".config", ".profile", "notes.txt", "zeta.txt"]
        );
        key(&mut session, "C-h");
        assert_eq!(names(&session), ["notes.txt", "zeta.txt"]);
        assert_eq!(note(&session), "Hidden files left out; Ctrl-H shows them");
        key(&mut session, "Escape");
        assert_eq!(text(&session), template);
    }

    /// An account's own `reply_from` is the From of a draft composed
    /// from its mailbox list.
    #[test]
    fn an_accounts_own_reply_from_is_the_drafts_from() {
        let state = crate::testing::tempdir().unwrap();
        let mut setup = setup();
        setup.draft_dir = Some(state.path().join("drafts"));
        let mut work = account();
        work.mail.reply_from = Some("Work <me@work.test>".to_string());
        setup.accounts = vec![work.clone()];
        let (cmd_tx, _cmd_rx) = mpsc::channel();
        let (_resp_tx, resp_rx) = mpsc::channel();
        let mut session = Session::new(setup, vec![Live { cmd_tx, resp_rx }], 0, false).unwrap();
        key(&mut session, "c");
        assert!(
            text(&session).starts_with("From: Work <me@work.test>\nTo: \n"),
            "{}",
            text(&session)
        );
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
        assert_eq!(
            session.stack.current().unwrap().scene().labels,
            ["Send", "Attach", "Save", "Help", "Close"]
        );
        let save = bar.header(2).expect("save label");
        press(&mut session, save.x + 2, save.y + 2);
        assert_eq!(
            std::fs::read_to_string(&fourth).unwrap(),
            format!("z{template}")
        );
        let close = bar.header(4).expect("close label");
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
        // A selection copied in a read-only pane is pasted into a draft.
        session.act(preview(3));
        key(&mut session, "C-a");
        key(&mut session, "C-c");
        assert_eq!(session.pane.editor().tabs().count(), 1);
        key(&mut session, "q");
        key(&mut session, "c");
        let fifth = path_of(&session);
        key(&mut session, "C-End");
        key(&mut session, "C-v");
        assert!(text(&session).contains("old 2"));
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
        assert_eq!(
            session.input(Input::Close, &mut NoClipboard),
            Flow::Continue
        );
        assert!(session.title.starts_with("Save "), "{}", session.title);
        key(&mut session, "Escape");
        assert!(session.title.starts_with("Draft "), "{}", session.title);
        assert!(!session.closing && !session.quitting);
        assert_eq!(
            session.input(Input::Close, &mut NoClipboard),
            Flow::Continue
        );
        assert_eq!(
            session.input(
                Input::Key {
                    chord: "y",
                    repeat: false
                },
                &mut NoClipboard
            ),
            Flow::Quit
        );
        assert!(std::fs::read_to_string(&fifth).unwrap().contains("old 2"));
        // A window too small for the pane: the draft is still loaded,
        // the view's own, and a save writes it, not the text under it.
        let (mut small, _cmd_rx, _resp_tx) = self::session(true);
        small.act(preview(3));
        small.input(
            Input::Resize(Surface::new(800, 40, Default::default()).unwrap()),
            &mut NoClipboard,
        );
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
        assert_eq!(small.input(Input::Close, &mut NoClipboard), Flow::Quit);
        let _ = std::fs::remove_dir_all(&draft_dir);
        let _ = std::fs::remove_dir_all(&small_dir);
    }

    /// A selection copied in a read-only text or in the draft reaches
    /// the window's clipboard at the chord that asked it, and the kill
    /// ring too, as a cut one does; a paste asks the clipboard when it
    /// offers text and the text arrives as an input into the draft it
    /// was asked for, and is the kill ring's otherwise; what the
    /// clipboard refuses, and what the window notices, is the status
    /// row's note until the next key, a compositor without a clipboard
    /// excepted; and a paste arriving for a draft no longer edited goes
    /// nowhere.
    #[test]
    fn a_copy_reaches_the_clipboard_and_a_paste_comes_from_it_or_the_kill_ring() {
        let (mut session, _cmd_rx, _resp_tx) = session(true);
        let draft_dir = session.setup.draft_dir.clone().unwrap();
        let mut board = Board::new();
        // A preview, read-only: its whole text is copied out.
        session.act(preview(3));
        assert_eq!(session.title(), "Retention expiry preview");
        key_with(&mut session, "C-c", &mut board);
        assert!(board.copies.is_empty(), "reader copy needs a selection");
        assert!(session.pane.killed().is_none());
        key_with(&mut session, "C-a", &mut board);
        key_with(&mut session, "C-c", &mut board);
        assert_eq!(board.copies.len(), 1);
        assert!(board.copies[0].contains("old 2"), "{}", board.copies[0]);
        assert_eq!(session.pane.killed(), Some(board.copies[0].clone()));
        assert!(!session.shown().contains("copy kept"), "no note");
        // A paste into the preview pastes nothing and asks nothing.
        board.text = true;
        key_with(&mut session, "C-v", &mut board);
        assert_eq!(board.pastes, 0);
        key(&mut session, "q");
        // The draft: the clipboard offers text, so a paste asks it and
        // the text arrives as an input over the selection.
        key(&mut session, "c");
        assert!(session.title.starts_with("Draft "), "{}", session.title);
        let template = text(&session);
        key_with(&mut session, "C-a", &mut board);
        key_with(&mut session, "C-v", &mut board);
        assert_eq!(board.pastes, 1);
        assert_eq!(text(&session), template, "nothing yet");
        session.input(Input::Paste("pasted"), &mut board);
        assert_eq!(text(&session), "pasted");
        // Without text on the clipboard the kill ring is pasted.
        board.text = false;
        key_with(&mut session, "C-v", &mut board);
        assert_eq!(board.pastes, 1);
        assert!(text(&session).starts_with("pasted"), "{}", text(&session));
        assert!(text(&session).contains("old 2"), "{}", text(&session));
        // A cut is the clipboard's too, so a paste after it brings the
        // cut text back whichever serves it; an empty text arriving
        // changes nothing and says nothing.
        key_with(&mut session, "C-a", &mut board);
        key_with(&mut session, "C-x", &mut board);
        assert_eq!(board.copies.len(), 2);
        assert!(board.copies[1].starts_with("pasted"), "{}", board.copies[1]);
        assert_eq!(text(&session), "");
        board.text = true;
        key_with(&mut session, "C-v", &mut board);
        assert_eq!(board.pastes, 2);
        session.input(Input::Paste(""), &mut board);
        assert_eq!(text(&session), "");
        assert!(!session.shown().contains("paste"), "{}", session.shown());
        key_with(&mut session, "C-v", &mut board);
        assert_eq!(board.pastes, 3);
        session.input(Input::Paste(&board.copies[1].clone()), &mut board);
        assert!(text(&session).starts_with("pasted"), "{}", text(&session));
        // A repeat of either chord, a held key, asks the clipboard
        // nothing: the copy stays the kill ring's and the paste is the
        // kill ring's, with no note.
        let attempts = board.attempts;
        key_with(&mut session, "C-a", &mut board);
        session.input(
            Input::Key {
                chord: "C-c",
                repeat: true,
            },
            &mut board,
        );
        assert_eq!(board.attempts, attempts);
        assert!(session
            .pane
            .killed()
            .as_deref()
            .is_some_and(|k| k.starts_with("pasted")));
        session.input(
            Input::Key {
                chord: "C-v",
                repeat: true,
            },
            &mut board,
        );
        assert_eq!(board.attempts, attempts);
        assert!(text(&session).starts_with("pasted"), "{}", text(&session));
        assert!(!session.shown().contains("refused"), "{}", session.shown());
        // The window's own notices, a paste cancelled among them, reach
        // the status row too.
        session.notice("paste cancelled: focus lost");
        assert!(
            session.shown().contains("paste cancelled: focus lost"),
            "{}",
            session.shown()
        );
        // A refusal is the note, and the next key clears it.
        board.refuse = Some(Refusal::NoSerial);
        key_with(&mut session, "C-a", &mut board);
        key_with(&mut session, "C-c", &mut board);
        assert_eq!(board.copies.len(), 2);
        let shown = session.shown();
        assert!(
            shown.contains(
                "copy kept in td-mail only: the clipboard answers a key or button press only"
            ),
            "{shown}"
        );
        key_with(&mut session, "Right", &mut board);
        assert!(!session.shown().contains("copy kept"), "cleared");
        board.text = true;
        key_with(&mut session, "C-v", &mut board);
        assert!(
            session
                .shown()
                .contains("paste refused: the clipboard answers"),
            "{}",
            session.shown()
        );
        // No clipboard at all is no note: the kill ring is the clipboard.
        // The copy is asked all the same.
        board.refuse = Some(Refusal::NoDevice);
        let attempts = board.attempts;
        key_with(&mut session, "C-a", &mut board);
        key_with(&mut session, "C-c", &mut board);
        assert_eq!(board.attempts, attempts + 1);
        assert!(
            !session.shown().contains("copy kept"),
            "{}",
            session.shown()
        );
        // A paste asked in this draft arrives once it is under its save
        // question, then once it is closed and another draft is edited:
        // dropped with a note both times, the other draft untouched.
        board.refuse = None;
        key_with(&mut session, "C-v", &mut board);
        assert_eq!(board.pastes, 4);
        key(&mut session, "C-w");
        assert!(session.title.starts_with("Save "), "{}", session.title);
        session.input(Input::Paste("under the question"), &mut board);
        assert!(
            session.shown().contains("paste dropped"),
            "{}",
            session.shown()
        );
        key(&mut session, "n");
        assert_eq!(session.stack.depth(), 1);
        key(&mut session, "c");
        assert!(session.title.starts_with("Draft "), "{}", session.title);
        let other = text(&session);
        key_with(&mut session, "C-v", &mut board);
        assert_eq!(board.pastes, 5);
        key(&mut session, "C-w");
        key(&mut session, "n");
        key(&mut session, "c");
        let third = text(&session);
        session.input(Input::Paste("late"), &mut board);
        assert!(
            session.shown().contains("paste dropped"),
            "{}",
            session.shown()
        );
        assert_eq!(text(&session), third);
        assert_eq!(other, third, "two fresh drafts from the same template");
        key(&mut session, "C-w");
        key(&mut session, "n");
        assert_eq!(session.stack.depth(), 1);
        let _ = std::fs::remove_dir_all(&draft_dir);
    }

    #[test]
    fn repeated_cut_does_not_delete_successive_draft_lines() {
        let (mut session, _cmd_rx, _resp_tx) = session(true);
        let mut board = Board::new();
        key(&mut session, "c");
        let original = text(&session);
        let first = original.split_inclusive('\n').next().unwrap_or(&original);
        key_with(&mut session, "C-Home", &mut board);
        key_with(&mut session, "C-x", &mut board);
        let after_cut = text(&session);
        assert_eq!(after_cut, original.strip_prefix(first).unwrap_or(""));
        assert_eq!(board.copies.len(), 1);
        assert_eq!(board.copies[0].as_ref(), first);
        session.input(
            Input::Key {
                chord: "C-x",
                repeat: true,
            },
            &mut board,
        );
        assert_eq!(text(&session), after_cut);
        assert_eq!(board.copies.len(), 1);
        assert_eq!(session.pane.killed().as_deref(), Some(first));
    }

    /// The Folder label opens a dropdown of the folder actions under it,
    /// which has every key and press while it is open: an unmapped key
    /// is consumed, Escape and a press outside dismiss it, Return on a
    /// row and a press on one are that row's key to the view, and a
    /// focus loss or a resize closes it; the frame shows its rows.
    #[test]
    fn the_folder_label_opens_a_dropdown_of_the_folder_actions() {
        let (mut session, _cmd_rx, _resp_tx) = session(true);
        fn labels(session: &Session) -> &'static [&'static str] {
            session.stack.current().unwrap().scene().labels
        }
        let index = labels(&session)
            .iter()
            .position(|&label| label == "Folder")
            .expect("a Folder label");
        let folder = session
            .shape()
            .unwrap()
            .layout
            .bar
            .header(index)
            .expect("the label's header");
        let open = |session: &mut Session| {
            press(session, folder.x + 2, folder.y + 2);
            assert!(session.menu.is_some(), "the press opens the dropdown");
        };
        open(&mut session);
        let shown = session.shown();
        assert!(
            shown.contains("New folder") && shown.contains("Delete folder"),
            "{shown}"
        );
        // A key the dropdown does not map is its own, consumed: `c` does
        // not compose.
        key(&mut session, "c");
        assert_eq!(session.stack.depth(), 1);
        assert!(session.menu.is_some());
        // Escape dismisses it, and the frame is as it was.
        key(&mut session, "Escape");
        assert!(session.menu.is_none());
        assert!(!session.shown().contains("Delete folder"));
        // So does a right press, wherever it is.
        open(&mut session);
        session.input(Input::Context { x: 2, y: 2 }, &mut NoClipboard);
        assert!(session.menu.is_none());
        assert_eq!(session.stack.depth(), 1);
        // The first row, New folder, is selected as it opens, so Return
        // is its key, which opens the name entry.
        open(&mut session);
        key(&mut session, "Return");
        assert!(session.menu.is_none());
        assert_eq!(labels(&session), &["Create", "Cancel"]);
        key(&mut session, "Escape");
        assert!(labels(&session).contains(&"Folder"));
        // Down is the second row, Delete folder, which puts the question
        // for the selected mailbox.
        open(&mut session);
        key(&mut session, "Down");
        key(&mut session, "Return");
        assert!(session.menu.is_none());
        assert!(
            session.title().starts_with("Delete folder 'INBOX'?"),
            "{}",
            session.title()
        );
        key(&mut session, "Escape");
        // A press on a row is that row's key as well.
        open(&mut session);
        let row = session
            .menu
            .as_ref()
            .and_then(|menu| menu.row_rect(0))
            .expect("the first row");
        press(&mut session, row.x + 4, row.y + 2);
        assert!(session.menu.is_none());
        assert_eq!(labels(&session), &["Create", "Cancel"]);
        key(&mut session, "Escape");
        // A press outside the dropdown dismisses it and goes no further:
        // the row under it is not opened.
        open(&mut session);
        let panel = session
            .menu
            .as_ref()
            .and_then(|menu| menu.panel(0))
            .expect("the panel");
        let list = session.shape().unwrap().layout.list.expect("a list");
        let outside = list.row(2).expect("a row");
        assert!(!panel.contains(outside.x + 2, outside.y + 2));
        press(&mut session, outside.x + 2, outside.y + 2);
        session.poll(0);
        assert!(session.menu.is_none());
        assert_eq!(session.stack.depth(), 1);
        // Space, as the keymap spells it unmodified, activates too, and
        // Left is the controller's dismissal.
        open(&mut session);
        key(&mut session, " ");
        assert!(session.menu.is_none());
        assert_eq!(labels(&session), &["Create", "Cancel"]);
        key(&mut session, "Escape");
        open(&mut session);
        key(&mut session, "Left");
        assert!(session.menu.is_none());
        // The dropdown's keys count as activity for the idle-sync clock.
        open(&mut session);
        session.last_user_activity = Instant::now() - Duration::from_secs(3600);
        key(&mut session, "Down");
        assert!(session.last_user_activity.elapsed() < Duration::from_secs(60));
        key(&mut session, "Escape");
        // A focus loss closes it, as does a resize, the pane and the
        // frame taking those as ever.
        open(&mut session);
        session.input(Input::Focus(false), &mut NoClipboard);
        assert!(session.menu.is_none());
        session.input(Input::Focus(true), &mut NoClipboard);
        open(&mut session);
        session.input(
            Input::Resize(Surface::new(900, 700, Default::default()).unwrap()),
            &mut NoClipboard,
        );
        assert!(session.menu.is_none());
        assert_eq!(session.surface.width, 900);
    }

    /// A dropdown is the view's it was opened for: a view pushed under
    /// it, by a row's pending open completing or otherwise, closes it,
    /// and a key that finds it stale is the new view's. A row that
    /// needs the list to have rows is disabled without them, so it is
    /// skipped and never activated. On a surface that shows the panel
    /// one row short, the wheel scrolls it.
    #[test]
    fn a_dropdown_closes_under_a_pushed_view_and_disables_delete_without_folders() {
        let (mut session, _cmd_rx, _resp_tx) = session(true);
        let folder_index = |session: &Session| {
            session
                .stack
                .current()
                .unwrap()
                .scene()
                .labels
                .iter()
                .position(|&label| label == "Folder")
                .expect("a Folder label")
        };
        let open = |session: &mut Session| {
            let index = folder_index(session);
            let folder = session
                .shape()
                .unwrap()
                .layout
                .bar
                .header(index)
                .expect("the label's header");
            press(session, folder.x + 2, folder.y + 2);
            assert!(session.menu.is_some(), "the press opens the dropdown");
        };
        // A row pressed, its open pending; then Folder: the open
        // completes at the poll, under the dropdown, which closes.
        let list = session.shape().unwrap().layout.list.expect("a list");
        let row = list.row(1).expect("a row");
        press(&mut session, row.x + 2, row.y + 2);
        open(&mut session);
        assert_eq!(session.stack.depth(), 1);
        session.poll(0);
        assert_eq!(session.stack.depth(), 2);
        assert!(
            session.menu.is_none(),
            "the dropdown is not the email list's"
        );
        key(&mut session, "q");
        assert_eq!(session.stack.depth(), 1);
        // A view pushed with no poll between: the next key finds the
        // dropdown stale, closes it, and is the pushed view's (Escape
        // pops the preview).
        open(&mut session);
        session.act(preview(1));
        assert_eq!(session.stack.depth(), 2);
        key(&mut session, "Escape");
        assert!(session.menu.is_none());
        assert_eq!(session.stack.depth(), 1);
        // On a surface with one row of room the panel shows one row:
        // the wheel brings the second into view.
        session.input(
            Input::Resize(Surface::new(800, 40, Default::default()).unwrap()),
            &mut NoClipboard,
        );
        open(&mut session);
        let menu = session.menu.as_ref().unwrap();
        assert!(menu.row_rect(0).is_some() && menu.row_rect(1).is_none());
        session.input(
            Input::Wheel {
                rows: 1,
                columns: 0,
            },
            &mut NoClipboard,
        );
        let menu = session.menu.as_ref().expect("still open");
        assert!(menu.row_rect(0).is_none() && menu.row_rect(1).is_some());
        key(&mut session, "Escape");
        // Without mailboxes, Delete folder is disabled: Down stays on
        // New folder, and Return is its key.
        let setup = setup();
        let (cmd_tx, _cmd_rx) = mpsc::channel();
        let (resp_tx, resp_rx) = mpsc::channel();
        let mut empty = Session::new(setup, vec![Live { cmd_tx, resp_rx }], 0, true).unwrap();
        resp_tx
            .send(BackendResponse::Mailboxes(Ok(Vec::new())))
            .unwrap();
        assert_eq!(empty.poll(0), Flow::Continue);
        assert!(empty.shown().contains("No mailboxes found."));
        open(&mut empty);
        let menu = empty.menu.as_ref().unwrap();
        assert!(menu.model().node(0).unwrap().row.enabled);
        assert!(!menu.model().node(1).unwrap().row.enabled);
        key(&mut empty, "Down");
        assert_eq!(
            empty.menu.as_ref().unwrap().selection(),
            menus::Selection::Node(0)
        );
        key(&mut empty, "Return");
        assert!(empty.menu.is_none());
        assert_eq!(
            empty.stack.current().unwrap().scene().labels,
            &["Create", "Cancel"]
        );
    }

    /// D lists the drafts retained and those sent beside them, drafts
    /// first; Return on a draft reopens it to edit, with its sidecar,
    /// and on a sent one shows it read-only; the list is read again as
    /// a view over it closes.
    #[test]
    fn the_drafts_view_lists_retained_and_sent_drafts_and_reopens_one() {
        let (mut session, _cmd_rx, _resp_tx) = session(true);
        let draft_dir = session.setup.draft_dir.clone().unwrap();
        let sent_dir = draft_dir.parent().unwrap().join("sent");
        let status = |session: &Session| session.stack.current().unwrap().scene().status;
        key(&mut session, "D");
        assert_eq!(session.title, "Drafts");
        assert!(
            text(&session).starts_with("No drafts or sent drafts in "),
            "{}",
            text(&session)
        );
        key(&mut session, "q");
        assert_eq!(session.stack.depth(), 1);

        key(&mut session, "c");
        let path = draft_dir.join(session.title.trim_start_matches("Draft "));
        key(&mut session, "C-w");
        assert_eq!(session.stack.depth(), 1);
        let draft = "From: me@example.com\nTo: bob@example.com\nSubject: Plans\n\
                     --text follows this line--\nhi\n";
        std::fs::write(&path, draft).unwrap();
        let sidecar = attach::sidecar_for(&path).unwrap();
        std::fs::create_dir(&sidecar).unwrap();
        std::fs::create_dir_all(&sent_dir).unwrap();
        let went = sent_dir.join("td-mail-draft-1-1.eml");
        std::fs::write(&went, "Subject: Went\n\nbody\n").unwrap();

        let selected = |session: &Session| match session.stack.current().unwrap().scene().body {
            Body::List { selected, row, .. } => row(selected),
            _ => panic!("the list"),
        };
        // A draft whose send lost its answer is marked, and says what its
        // next Send does.
        let record = compose::lost_record_for(&path);
        std::fs::write(&record, "x").unwrap();
        key(&mut session, "D");
        assert_eq!(session.stack.depth(), 2);
        assert!(
            selected(&session)
                .meta
                .starts_with("draft, send unsettled "),
            "{:?}",
            selected(&session)
        );
        assert!(
            status(&session).starts_with("A send of this draft is on record, unsettled: "),
            "{}",
            status(&session)
        );
        std::fs::remove_file(&record).unwrap();
        key(&mut session, "g");
        assert!(
            status(&session).starts_with("1 draft, 1 sent"),
            "{}",
            status(&session)
        );
        let shown = session.shown();
        let (plans, gone) = (shown.find("Plans — bob@example.com"), shown.find("Went"));
        assert!(plans.is_some() && gone.is_some() && plans < gone, "{shown}");

        // A draft reopens to edit, its sidecar with it.
        key(&mut session, "Return");
        assert_eq!(session.stack.depth(), 3);
        assert_eq!(
            session.title,
            format!("Draft {}", path.file_name().unwrap().to_string_lossy())
        );
        assert_eq!(text(&session), draft);
        assert!(
            status(&session).contains(&format!("attachments at {}", sidecar.display())),
            "{}",
            status(&session)
        );
        key(&mut session, "C-w");
        assert_eq!(session.stack.depth(), 2);
        assert_eq!(session.title, "Drafts");

        // Retired while it is open, the draft is listed as sent when the
        // view closes, the selection on it, behind the newer sent one.
        key(&mut session, "Return");
        std::fs::rename(&path, sent_dir.join(path.file_name().unwrap())).unwrap();
        key(&mut session, "C-w");
        assert_eq!(session.title, "Drafts");
        assert!(
            status(&session).starts_with("0 drafts, 2 sent"),
            "{}",
            status(&session)
        );
        let on = selected(&session);
        assert!(
            on.label.starts_with("Plans") && on.meta.starts_with("sent "),
            "{on:?}"
        );

        // A sent one shows read-only.
        key(&mut session, "k");
        key(&mut session, "Return");
        assert_eq!(session.title, "Sent td-mail-draft-1-1.eml");
        let tab = session.pane.tab().expect("the sent draft");
        assert!(session.pane.editor().document(tab).unwrap().read_only());
        assert!(
            text(&session).starts_with("Subject: Went"),
            "{}",
            text(&session)
        );
        key(&mut session, "q");
        assert_eq!(session.title, "Drafts");
        key(&mut session, "q");
        assert_eq!(session.stack.depth(), 1);
    }

    /// The bar's Help label on the views whose bars lacked it, the
    /// thread view, the drafts, a sent draft and a draft being edited:
    /// a press on it asks for the key list and is nothing else, the
    /// view staying up and the draft, which types `?`, typing nothing.
    #[test]
    fn the_help_label_asks_for_the_key_list_and_types_nothing() {
        fn help(session: &mut Session) {
            let labels = session.stack.current().unwrap().scene().labels;
            let index = labels
                .iter()
                .position(|label| *label == td_ui::keys::BUTTON)
                .unwrap_or_else(|| panic!("no Help label in {labels:?}"));
            let rect = session.shape().unwrap().layout.bar.header(index).unwrap();
            press(session, rect.x + 2, rect.y + 2);
        }
        let (mut session, _cmd_rx, _resp_tx) = session(true);
        let (cmd_tx, _thread_rx) = mpsc::channel();
        session.act(ViewAction::Push(Box::new(
            views::thread_view::ThreadView::new(
                cmd_tx,
                "me@example.com".to_string(),
                "thread-1".to_string(),
                "Roof repair".to_string(),
                Vec::new(),
                "Archive".to_string(),
                "Trash".to_string(),
                false,
                None,
                None,
            ),
        )));
        assert!(!session.take_show_keys());
        help(&mut session);
        assert!(session.take_show_keys(), "the thread view's Help");
        assert_eq!(session.stack.depth(), 2);
        key(&mut session, "q");
        assert_eq!(session.stack.depth(), 1);

        key(&mut session, "D");
        assert_eq!(session.title, "Drafts");
        help(&mut session);
        assert!(session.take_show_keys(), "the drafts' Help");
        assert_eq!(session.title, "Drafts");

        let dir = crate::testing::tempdir().unwrap();
        let sent = dir.path().join("td-mail-draft-1-1.eml");
        std::fs::write(&sent, "Subject: Went\n").unwrap();
        let view = views::drafts::SentView::open(&sent).unwrap();
        session.act(ViewAction::Push(Box::new(view)));
        help(&mut session);
        assert!(session.take_show_keys(), "a sent draft's Help");
        assert_eq!(session.title, "Sent td-mail-draft-1-1.eml");
        key(&mut session, "q");
        key(&mut session, "q");
        assert_eq!(session.stack.depth(), 1);

        key(&mut session, "c");
        assert!(session.title.starts_with("Draft "), "{}", session.title);
        let typed = text(&session);
        help(&mut session);
        assert!(session.take_show_keys(), "the draft's Help");
        assert_eq!(text(&session), typed, "Help types nothing");
        assert!(session.title.starts_with("Draft "), "{}", session.title);
        // `?` itself is the draft's character, not the list.
        key(&mut session, "?");
        assert!(!session.take_show_keys());
        assert_ne!(text(&session), typed, "`?` types");
    }
}

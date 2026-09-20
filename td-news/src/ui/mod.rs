//! The reader's views over the widget window: the toolkit's lists for
//! the feeds, the articles and an article's links, td-editor's read-only
//! document pane for an article, a log window and the help, a search
//! field, an action bar and a status row, laid out over the surface each
//! frame. The window owns the Wayland connection; the app owns every view
//! and the pane's controller. A press in the pane is handed on at the
//! pointer's pixel as both the caret and the cell coordinate, as the
//! toolkit's replay does, so the caret lands before the glyph under the
//! pointer rather than at its nearer edge.

mod input;
pub mod views;
mod window;

use std::cell::Cell;
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;

use td_editor::clipboard::Snapshot;
use td_editor::model::TabId;
use td_editor::ui::{Controller, Event, Outcome, PointerPhase as PanePhase};
use td_ui::chrome::{Bar, Field, Item, List, Status, Strip, TabHit, TextEntry, ROW};
use td_ui::raster::{Composition, Draw, Primitive, Raster, Rect, Surface, PAPER};
use td_ui::window::{Clipboard, Input, PointerPhase};

use crate::backend::{BackendCommand, BackendResponse, FeedRefreshReport};
use crate::cache::Cache;
use crate::config::Config;
use crate::feed::{datetime_sort_key, normalize_datetime_to_local, Article};
use crate::keybindings;

use input::Key;
pub use window::run;

/// The rows a page key moves a list's selection or a pane's window by
/// when the layout has none to say: the terminal's fifteen.
const PAGE_ROWS: usize = 15;

#[derive(Clone)]
struct FeedRow {
    name: String,
    url: String,
    total: usize,
    unread: usize,
    last_updated: Option<String>,
    last_error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum View {
    FeedList,
    ArticleList,
    Article,
    Log,
    Help,
}

#[derive(Clone)]
enum FeedScope {
    All,
    Unread,
    Feed(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LogTab {
    News,
    Debug,
}

/// What is known of a log's lines: the length counted so far, the
/// newlines in it, and whether it ended in one, so a log that grew is
/// counted onward from where the last count stopped.
#[derive(Clone, Copy)]
struct LogCount {
    len: u64,
    newlines: usize,
    ends_with_newline: bool,
}

impl LogCount {
    const NONE: LogCount = LogCount {
        len: 0,
        newlines: 0,
        ends_with_newline: true,
    };

    /// Lines as `str::lines` counts them: an unterminated last line is one.
    fn total(self) -> usize {
        self.newlines + usize::from(self.len > 0 && !self.ends_with_newline)
    }
}

fn log_tab_index(tab: LogTab) -> usize {
    match tab {
        LogTab::News => 0,
        LogTab::Debug => 1,
    }
}

/// What the document pane holds, so a frame reloads it only when what it
/// should show changed.
#[derive(Clone, Debug, PartialEq, Eq)]
enum PaneText {
    None,
    NoArticle,
    Article {
        hash: String,
        columns: usize,
    },
    Log {
        tab: usize,
        start: usize,
        rows: usize,
        columns: usize,
        total: Option<usize>,
    },
    Help,
}

/// The action bar's labels for a view, and the key each stands for; a
/// click on a label is that key.
const FEED_LABELS: &[&str] = &[
    "Open",
    "Refresh",
    "Refresh all",
    "Mark read",
    "Help",
    "Quit",
];
const FEED_KEYS: &[Key] = &[
    Key::Enter,
    Key::Char('g'),
    Key::Char('G'),
    Key::Char('u'),
    Key::Char('?'),
    Key::Char('q'),
];
const ARTICLE_LIST_LABELS: &[&str] = &[
    "Back",
    "Refresh",
    "Refresh all",
    "Read, next",
    "Digest",
    "Open link",
    "Help",
];
const ARTICLE_LIST_KEYS: &[Key] = &[
    Key::Char('q'),
    Key::Char('g'),
    Key::Char('G'),
    Key::Char('u'),
    Key::Char('H'),
    Key::Char('o'),
    Key::Char('?'),
];
const ARTICLE_LABELS: &[&str] = &[
    "Back",
    "Previous",
    "Next",
    "Links",
    "Open link",
    "Toggle read",
    "Help",
];
const ARTICLE_KEYS: &[Key] = &[
    Key::Char('q'),
    Key::Char('p'),
    Key::Char('n'),
    Key::Char('b'),
    Key::Char('o'),
    Key::Char('u'),
    Key::Char('?'),
];
const LINKS_LABELS: &[&str] = &["Cancel", "Open"];
const LINKS_KEYS: &[Key] = &[Key::Char('q'), Key::Enter];
const LOG_LABELS: &[&str] = &["Back", "News", "Debug", "Help"];
const LOG_KEYS: &[Key] = &[
    Key::Char('q'),
    Key::Char('n'),
    Key::Char('d'),
    Key::Char('?'),
];
const HELP_LABELS: &[&str] = &["Back"];
const HELP_KEYS: &[Key] = &[Key::Char('q')];

/// The frame's regions, laid out over the surface for the view: the
/// action bar, then the view's body between it and the status row.
struct Layout {
    bar: Bar<'static>,
    search: Option<TextEntry>,
    list: Option<List>,
    strip: Option<Strip>,
    pane: Option<Rect>,
    status: Status,
}

struct App {
    view: View,
    /// The view the help returns to.
    help_from: View,
    feeds: Vec<FeedRow>,
    selected_feed: usize,
    feed_first: usize,
    selected_article: usize,
    article_first: usize,
    selected_feed_scope: Option<FeedScope>,
    articles: Vec<Article>,
    open_article: Option<Article>,
    search: String,
    search_mode: bool,
    search_first: usize,
    status: String,
    last_updated: Option<String>,
    /// The surface the window last laid the reader out for.
    surface: Surface,
    log_tab: LogTab,
    /// The first log line asked for; `usize::MAX` follows the log's end.
    log_scroll: usize,
    /// The first line the last frame showed: `log_scroll` clamped.
    log_start: usize,
    /// The count the shown log window was read under; none when the
    /// window could not be read, whatever a kept count says.
    log_shown_total: Option<usize>,
    /// The line count of each log, so a frame counts only what grew.
    log_count: [Cell<Option<LogCount>>; 2],
    /// Where each log view's first line began, so a frame resumes its
    /// scan there rather than at the file's start.
    log_window_hint: [Cell<Option<(usize, u64)>>; 2],
    quitting: bool,
    pending_redraw: bool,
    mouse_config: bool,
    browser: Option<String>,
    article_urls: Vec<String>,
    url_picking: bool,
    url_cursor: usize,
    url_first: usize,
    pending_user_fetches: usize,
    pending_read_mutations: HashMap<String, bool>,
    /// The document pane, read-only: an article, a log window or the help.
    pane: Controller,
    pane_tab: Option<TabId>,
    pane_text: PaneText,
    /// Whether a press in the pane began a drag the pane still receives.
    pane_drag: bool,
    /// The selection a copy chord asked to put on the window's
    /// clipboard, offered once the chord is handled, while it is still
    /// being delivered.
    copy: Option<Arc<str>>,
    /// What the clipboard answered, or that nothing was selected, in
    /// the status row until the next key or press.
    note: Option<String>,
}

impl App {
    fn new(config: &Config, cache: &Cache, offline: bool) -> Result<Self, String> {
        let mut feeds = Vec::with_capacity(config.feeds.len());
        let mut last_updated = None;
        let mut last_updated_ts = None;
        for feed in &config.feeds {
            let hashes = cache.get_feed_index(&feed.url).unwrap_or_default();
            let unread = hashes
                .iter()
                .filter(|hash| cache.get_article(hash).map(|a| !a.read).unwrap_or(false))
                .count();
            if let Some(meta) = cache.get_feed_meta(&feed.url) {
                if let Some(ts) = datetime_sort_key(Some(&meta.last_fetched)) {
                    if last_updated_ts.map(|current| ts > current).unwrap_or(true) {
                        last_updated_ts = Some(ts);
                        last_updated = normalize_datetime_to_local(&meta.last_fetched);
                    }
                }
            }
            feeds.push(FeedRow {
                name: feed.name.clone(),
                url: feed.url.clone(),
                total: hashes.len(),
                unread,
                last_updated: cache
                    .get_feed_meta(&feed.url)
                    .and_then(|m| normalize_datetime_to_local(&m.last_fetched)),
                last_error: None,
            });
        }
        let pane = Controller::pane().map_err(|e| format!("document pane: {e}"))?;
        Ok(Self {
            view: View::FeedList,
            help_from: View::FeedList,
            feeds,
            selected_feed: 0,
            feed_first: 0,
            selected_article: 0,
            article_first: 0,
            selected_feed_scope: None,
            articles: Vec::new(),
            open_article: None,
            search: String::new(),
            search_mode: false,
            search_first: 0,
            status: if offline {
                "Offline mode: browsing cache".to_string()
            } else {
                "Ready".to_string()
            },
            last_updated,
            surface: pane.geometry().surface(),
            log_tab: LogTab::News,
            log_scroll: 0,
            log_start: 0,
            log_shown_total: None,
            log_count: [Cell::new(None), Cell::new(None)],
            log_window_hint: [Cell::new(None), Cell::new(None)],
            quitting: false,
            pending_redraw: true,
            mouse_config: config.ui.mouse,
            browser: config.ui.browser.clone(),
            article_urls: Vec::new(),
            url_picking: false,
            url_cursor: 0,
            url_first: 0,
            pending_user_fetches: 0,
            pending_read_mutations: HashMap::new(),
            pane,
            pane_tab: None,
            pane_text: PaneText::None,
            pane_drag: false,
            copy: None,
            note: None,
        })
    }

    fn handle_backend(&mut self, msg: BackendResponse, cache: &Cache) {
        match msg {
            BackendResponse::RefreshCompleted { reports } => {
                let user_refresh = self.pending_user_fetches > 0;
                if user_refresh {
                    self.pending_user_fetches -= 1;
                } else {
                    let mut errors = 0usize;
                    for report in &reports {
                        if let Some(error) = report.error.as_ref() {
                            errors += 1;
                            if let Some(row) =
                                self.feeds.iter_mut().find(|f| f.url == report.feed_url)
                            {
                                row.last_error = Some(error.clone());
                            }
                        }
                    }
                    if errors > 0 {
                        self.status = if errors == 1 {
                            reports
                                .iter()
                                .find_map(|report| report.error.as_ref())
                                .map(|error| format!("Fetch error: {}", error))
                                .unwrap_or_else(|| "Fetch error".to_string())
                        } else {
                            format!("Fetch errors: {} feeds", errors)
                        };
                        self.pending_redraw = true;
                    }
                    // Auto-sync successes are kept in cache and shown after an
                    // explicit user refresh. Errors remain visible so failed
                    // feeds are not silent.
                    return;
                }

                let mut new_total = 0usize;
                let mut errors = 0usize;
                let mut successes = 0usize;
                let mut latest = None::<(i64, String)>;

                for report in &reports {
                    if let Some(error) = report.error.as_ref() {
                        errors += 1;
                        if let Some(row) = self.feeds.iter_mut().find(|f| f.url == report.feed_url)
                        {
                            row.last_error = Some(error.clone());
                        }
                        continue;
                    }

                    successes += 1;
                    new_total += report.new_articles;
                    let effective_unread =
                        self.feed_unread_count_from_cache(cache, &report.feed_url);
                    if let Some(row) = self.feeds.iter_mut().find(|f| f.url == report.feed_url) {
                        row.total = report.total;
                        row.unread = effective_unread;
                        row.last_updated = report
                            .fetched_at
                            .as_deref()
                            .and_then(normalize_datetime_to_local);
                        row.last_error = None;
                    }
                    if let Some(fetched_at) = report.fetched_at.as_ref() {
                        if let Some(ts) = datetime_sort_key(Some(fetched_at)) {
                            if latest
                                .as_ref()
                                .map(|(current, _)| ts > *current)
                                .unwrap_or(true)
                            {
                                latest = Some((ts, fetched_at.clone()));
                            }
                        }
                    }
                }

                if let Some((_, fetched_at)) = latest {
                    self.last_updated = Some(fetched_at);
                }

                if self.selected_feed_scope.is_some() {
                    self.reload_articles(cache);
                }
                self.status = refresh_status(&reports, successes, errors, new_total);
                self.pending_redraw = true;
            }
            BackendResponse::ArticleMutation { hash, read } => {
                let effective_read = match self.pending_read_mutations.get(&hash).copied() {
                    Some(pending_read) if pending_read == read => {
                        self.pending_read_mutations.remove(&hash);
                        read
                    }
                    Some(pending_read) => pending_read,
                    None => read,
                };
                self.apply_article_read(&hash, effective_read);
                self.recount_current_feed_unread();
                self.pending_redraw = true;
            }
            BackendResponse::FeedMarkedRead { feed_url } => {
                if let Some(row) = self.feeds.iter_mut().find(|f| f.url == feed_url) {
                    row.unread = 0;
                }
                if let Some(scope) = self.selected_feed_scope.as_ref() {
                    match scope {
                        FeedScope::Feed(url) if url == &feed_url => {
                            for article in &mut self.articles {
                                article.read = true;
                            }
                        }
                        FeedScope::All | FeedScope::Unread => self.reload_articles(cache),
                        _ => {}
                    }
                }
                self.status = "Marked feed as read".to_string();
                self.pending_redraw = true;
            }
        }
    }

    // ---- the window's inputs ------------------------------------------

    /// One input from the window; whether the reader is quitting.
    fn input(
        &mut self,
        input: Input<'_>,
        cache: &Cache,
        cmd_tx: &mpsc::Sender<BackendCommand>,
    ) -> bool {
        if self.quitting {
            return true;
        }
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
            self.pending_redraw = true;
        }
        match input {
            Input::Close => self.quitting = true,
            Input::Resize(surface) => {
                self.surface = surface;
                self.pending_redraw = true;
            }
            Input::Focus(focused) => {
                self.pane_event(Event::Focus(focused));
                self.pending_redraw = true;
            }
            // A repeat, a held key, has no press for the clipboard to
            // take a selection at: the chord is handled, the ask dropped.
            Input::Key { chord, repeat } => {
                self.chord(chord, cache, cmd_tx);
                if repeat {
                    self.copy = None;
                }
            }
            Input::Pointer {
                phase,
                x,
                y,
                extend,
            } if self.mouse_config => self.pointer(phase, x, y, extend, cache, cmd_tx),
            Input::CancelPointer => {
                if self.pane_drag {
                    self.pane_drag = false;
                    if self.pane_event(Event::CancelPointer) == Outcome::Changed {
                        self.pending_redraw = true;
                    }
                }
            }
            Input::Wheel { rows, .. } if self.mouse_config => self.wheel(rows),
            // Nothing here is editable: a paste has nowhere to go, and
            // the reader asks for none.
            Input::Pointer { .. } | Input::Wheel { .. } | Input::Paste(_) => {}
        }
        self.quitting
    }

    /// The copy the input asked for, offered to the window's clipboard
    /// while the input is still being delivered, so it is made at the
    /// press's serial; what the clipboard answered is the status row's
    /// note.
    fn serve_clipboard(&mut self, clipboard: &mut dyn Clipboard) {
        let Some(text) = self.copy.take() else {
            return;
        };
        self.note = Some(match clipboard.copy(text) {
            Ok(()) => "Copied to the clipboard".to_string(),
            Err(refusal) => format!("Copy refused: {refusal}"),
        });
        self.pending_redraw = true;
    }

    /// The pane's selection, for the clipboard: none selected, or one
    /// that cannot be captured (past the clipboard's ceiling), is said
    /// in the status row.
    fn copy_selection(&mut self) {
        let Some((tab, revision)) = self.pane_target() else {
            return;
        };
        match Snapshot::capture(self.pane.editor(), tab, revision) {
            Ok(Some(snapshot)) => self.copy = Some(snapshot.text()),
            Ok(None) => {
                self.note = Some("Nothing selected to copy".to_string());
                self.pending_redraw = true;
            }
            Err(td_editor::Error::Limit) => {
                self.note = Some(format!(
                    "Copy refused: the selection is past the clipboard's {} KiB ceiling",
                    td_editor::clipboard::MAX_BYTES / 1024
                ));
                self.pending_redraw = true;
            }
            Err(error) => {
                crate::log::error(format!("document pane: copy: {error}"));
                self.note = Some(format!("Copy refused: {error}"));
                self.pending_redraw = true;
            }
        }
    }

    /// The loop's clock, for the pane's caret.
    fn tick(&mut self, now: u64) {
        if self.pane_event(Event::Tick(now)) == Outcome::Changed && self.pane_shown() {
            self.pending_redraw = true;
        }
    }

    fn pane_shown(&self) -> bool {
        matches!(self.view, View::Help | View::Log)
            || (self.view == View::Article && !self.url_picking)
    }

    /// A chord: one of the reader's keys, or the pane's when one is shown.
    fn chord(&mut self, chord: &str, cache: &Cache, cmd_tx: &mpsc::Sender<BackendCommand>) {
        match input::key(chord) {
            Some(key) => self.handle_key(key, chord, cache, cmd_tx),
            None => self.pane_chord(chord),
        }
    }

    /// A key of the reader's; `chord` is its spelling, for the pane, or
    /// empty for a key the action bar stood for.
    fn handle_key(
        &mut self,
        key: Key,
        chord: &str,
        cache: &Cache,
        cmd_tx: &mpsc::Sender<BackendCommand>,
    ) {
        if key == Key::Char('?') && self.view != View::Help && !self.search_mode {
            self.help_from = self.view;
            self.view = View::Help;
            self.pending_redraw = true;
            return;
        }
        if self.search_mode {
            match key {
                Key::Enter => {
                    self.search_mode = false;
                    self.selected_article = 0;
                    self.pending_redraw = true;
                }
                Key::Backspace => {
                    self.search.pop();
                    self.selected_article = 0;
                    self.pending_redraw = true;
                }
                Key::Char(c) if !c.is_control() => {
                    self.search.push(c);
                    self.selected_article = 0;
                    self.pending_redraw = true;
                }
                _ => {}
            }
            return;
        }
        match self.view {
            View::FeedList => self.handle_feed_keys(key, cache, cmd_tx),
            View::ArticleList => self.handle_article_list_keys(key, cache, cmd_tx),
            View::Article => self.handle_article_view_keys(key, chord, cache, cmd_tx),
            View::Log => self.handle_log_keys(key),
            View::Help => {
                if key == Key::Char('q') {
                    self.view = self.help_from;
                    self.pending_redraw = true;
                } else {
                    self.pane_key(key, chord);
                }
            }
        }
    }

    fn handle_feed_keys(&mut self, key: Key, cache: &Cache, cmd_tx: &mpsc::Sender<BackendCommand>) {
        match key {
            Key::Char('q') => self.quitting = true,
            Key::Down | Key::Char('j') | Key::Char('n') => self.move_feed(1),
            Key::Up | Key::Char('k') | Key::Char('p') => self.move_feed(-1),
            Key::PageDown => self.move_feed(self.list_rows() as isize),
            Key::PageUp => self.move_feed(-(self.list_rows() as isize)),
            Key::Home => self.move_feed(isize::MIN),
            Key::End => self.move_feed(isize::MAX),
            Key::Enter => self.open_feed(cache),
            Key::Char('g') => {
                self.reload_feeds_from_cache(cache);
                match self.scope_for_selected_feed() {
                    FeedScope::Feed(url) => {
                        let feed_name = self
                            .feeds
                            .iter()
                            .find(|f| f.url == url)
                            .map(|f| f.name.as_str())
                            .unwrap_or("feed");
                        self.pending_user_fetches += 1;
                        let _ = cmd_tx.send(BackendCommand::FetchFeed { url });
                        self.status = format!("Refreshing {}...", feed_name);
                    }
                    FeedScope::All | FeedScope::Unread => {
                        self.pending_user_fetches += 1;
                        let _ = cmd_tx.send(BackendCommand::FetchAllFeeds);
                        self.status = "Refreshing feeds...".to_string();
                    }
                }
                self.pending_redraw = true;
            }
            Key::Char('G') => {
                self.reload_feeds_from_cache(cache);
                self.pending_user_fetches += 1;
                let _ = cmd_tx.send(BackendCommand::FetchAllFeeds);
                self.status = "Refreshing feeds...".to_string();
                self.pending_redraw = true;
            }
            Key::Char('u') if self.selected_feed != self.log_row_index() => {
                match self.scope_for_selected_feed() {
                    FeedScope::Feed(url) => {
                        if let Some(feed) = self.feeds.iter().find(|f| f.url == url) {
                            let _ = cmd_tx.send(BackendCommand::MarkFeedRead {
                                feed_url: feed.url.clone(),
                            });
                            self.status = format!("Marking {} read...", feed.name);
                            self.pending_redraw = true;
                        }
                    }
                    FeedScope::All | FeedScope::Unread => {
                        for feed in &self.feeds {
                            let _ = cmd_tx.send(BackendCommand::MarkFeedRead {
                                feed_url: feed.url.clone(),
                            });
                        }
                        self.status = "Marking all feeds read...".to_string();
                        self.pending_redraw = true;
                    }
                }
            }
            _ => {}
        }
    }

    /// Moves the feed selection by `by` rows, clamped; the extremes go to
    /// the ends.
    fn move_feed(&mut self, by: isize) {
        let count = self.feed_row_count();
        let target = step(self.selected_feed, by, count);
        if target != self.selected_feed {
            self.selected_feed = target;
            self.pending_redraw = true;
        }
    }

    fn open_feed(&mut self, cache: &Cache) {
        if self.selected_feed == self.log_row_index() {
            self.view = View::Log;
            self.log_scroll = 0;
        } else {
            self.selected_feed_scope = Some(self.scope_for_selected_feed());
            self.selected_article = 0;
            self.article_first = 0;
            self.search.clear();
            self.search_mode = false;
            self.reload_articles(cache);
            self.view = View::ArticleList;
        }
        self.pending_redraw = true;
    }

    fn handle_article_list_keys(
        &mut self,
        key: Key,
        cache: &Cache,
        cmd_tx: &mpsc::Sender<BackendCommand>,
    ) {
        match key {
            Key::Char('q') => {
                self.view = View::FeedList;
                self.selected_feed_scope = None;
                self.pending_redraw = true;
            }
            Key::Down | Key::Char('j') | Key::Char('n') => self.move_article(1),
            Key::Up | Key::Char('k') | Key::Char('p') => self.move_article(-1),
            Key::PageDown => self.move_article(self.list_rows() as isize),
            Key::PageUp => self.move_article(-(self.list_rows() as isize)),
            Key::Home => self.move_article(isize::MIN),
            Key::End => self.move_article(isize::MAX),
            Key::Enter => {
                if !self.filtered_article_indices().is_empty() {
                    self.enter_article_view(cmd_tx);
                    self.pending_redraw = true;
                }
            }
            Key::Char('/') => {
                self.search_mode = true;
                self.pending_redraw = true;
            }
            Key::Char('g') => {
                self.reload_feeds_from_cache(cache);
                self.reload_articles(cache);
                if let Some(scope) = self.selected_feed_scope.as_ref() {
                    match scope {
                        FeedScope::Feed(url) => {
                            self.pending_user_fetches += 1;
                            let _ = cmd_tx.send(BackendCommand::FetchFeed { url: url.clone() });
                            self.status = "Refreshing feed...".to_string();
                        }
                        FeedScope::All | FeedScope::Unread => {
                            self.pending_user_fetches += 1;
                            let _ = cmd_tx.send(BackendCommand::FetchAllFeeds);
                            self.status = "Refreshing feeds...".to_string();
                        }
                    }
                    self.pending_redraw = true;
                }
            }
            Key::Char('G') => {
                self.reload_feeds_from_cache(cache);
                self.reload_articles(cache);
                self.pending_user_fetches += 1;
                let _ = cmd_tx.send(BackendCommand::FetchAllFeeds);
                self.status = "Refreshing feeds...".to_string();
                self.pending_redraw = true;
            }
            Key::Char('u') => {
                // Mark as read and advance to the next unread; toggle a read one.
                let was_unread = self.current_article().map(|a| !a.read).unwrap_or(false);
                let visible_before = self.filtered_article_indices();
                let selected_before = self.selected_article;
                self.toggle_current_read(cache, cmd_tx);
                if was_unread {
                    if let Some(target_article_idx) =
                        self.find_next_unread_article_idx(&visible_before, selected_before)
                    {
                        let visible_after = self.filtered_article_indices();
                        if let Some(target_selected_idx) = visible_after
                            .iter()
                            .position(|&article_idx| article_idx == target_article_idx)
                        {
                            self.selected_article = target_selected_idx;
                        }
                    }
                    self.pending_redraw = true;
                }
            }
            Key::Char('H') => self.open_html_digest(),
            Key::Char('o') => self.open_current_article(),
            _ => {}
        }
    }

    fn move_article(&mut self, by: isize) {
        let count = self.filtered_article_indices().len();
        let target = step(self.selected_article, by, count);
        if target != self.selected_article {
            self.selected_article = target;
            self.pending_redraw = true;
        }
    }

    fn handle_article_view_keys(
        &mut self,
        key: Key,
        chord: &str,
        cache: &Cache,
        cmd_tx: &mpsc::Sender<BackendCommand>,
    ) {
        if self.url_picking {
            match key {
                Key::Char('q') => {
                    self.url_picking = false;
                    self.pending_redraw = true;
                }
                Key::Down | Key::Char('j') | Key::Char('n') => self.move_url(1),
                Key::Up | Key::Char('k') | Key::Char('p') => self.move_url(-1),
                Key::PageDown => self.move_url(self.list_rows() as isize),
                Key::PageUp => self.move_url(-(self.list_rows() as isize)),
                Key::Home => self.move_url(isize::MIN),
                Key::End => self.move_url(isize::MAX),
                Key::Enter => self.open_url(self.url_cursor),
                Key::Char(c) if c.is_ascii_digit() && c != '0' => {
                    self.open_url((c as usize) - ('1' as usize));
                }
                _ => {}
            }
            return;
        }
        match key {
            Key::Char('q') => {
                self.leave_article_view();
                self.pending_redraw = true;
            }
            Key::Char('j' | 'k' | ' ')
            | Key::PageDown
            | Key::PageUp
            | Key::Up
            | Key::Down
            | Key::Home
            | Key::End => self.pane_key(key, chord),
            Key::Char('n') => {
                let visible = self.filtered_article_indices();
                if self.selected_article + 1 < visible.len() {
                    self.selected_article += 1;
                    self.sync_open_article_to_selection();
                    self.mark_current_article_read(cmd_tx);
                    self.extract_current_article_urls();
                    self.pending_redraw = true;
                }
            }
            Key::Char('p') => {
                if self.selected_article > 0 {
                    self.selected_article -= 1;
                    self.sync_open_article_to_selection();
                    self.mark_current_article_read(cmd_tx);
                    self.extract_current_article_urls();
                    self.pending_redraw = true;
                }
            }
            Key::Char('u') => self.toggle_current_read(cache, cmd_tx),
            Key::Char('o') => self.open_current_article(),
            Key::Char('b') => {
                if self.article_urls.is_empty() {
                    self.status = "No URLs in article".to_string();
                } else if self.article_urls.len() == 1 {
                    self.open_url(0);
                } else {
                    self.url_picking = true;
                    self.url_cursor = 0;
                    self.url_first = 0;
                }
                self.pending_redraw = true;
            }
            Key::Char(c) if c.is_ascii_digit() && c != '0' => {
                self.open_url((c as usize) - ('1' as usize));
            }
            _ => {}
        }
    }

    fn move_url(&mut self, by: isize) {
        let target = step(self.url_cursor, by, self.article_urls.len());
        if target != self.url_cursor {
            self.url_cursor = target;
            self.pending_redraw = true;
        }
    }

    /// Opens link `index` of the article's, leaving the picker.
    fn open_url(&mut self, index: usize) {
        if let Some(url) = self.article_urls.get(index).cloned() {
            self.status = match open_in_browser(&url, self.browser.as_deref()) {
                Ok(()) => format!("Opened [{}]", index + 1),
                Err(e) => format!("Failed to open browser: {}", e),
            };
            self.url_picking = false;
            self.pending_redraw = true;
        }
    }

    fn handle_log_keys(&mut self, key: Key) {
        let page = self.pane_rows().max(1);
        match key {
            Key::Char('q') => {
                self.view = View::FeedList;
                self.pending_redraw = true;
            }
            Key::Down | Key::Char('j') => self.scroll_log(1),
            Key::Up | Key::Char('k') => self.scroll_log(-1),
            Key::PageDown => self.scroll_log(page as isize),
            Key::PageUp => self.scroll_log(-(page as isize)),
            Key::Home => self.scroll_log(isize::MIN),
            Key::End => self.scroll_log(isize::MAX),
            Key::Char('n') => self.select_log(LogTab::News),
            Key::Char('d') => self.select_log(LogTab::Debug),
            _ => {}
        }
    }

    /// Moves the log's window; the extremes go to the ends, and the end
    /// is followed until the window moves from where it was last shown.
    fn scroll_log(&mut self, by: isize) {
        self.log_scroll = match by {
            isize::MIN => 0,
            isize::MAX => usize::MAX,
            _ if self.log_scroll == usize::MAX => self.log_start.saturating_add_signed(by),
            _ => self.log_scroll.saturating_add_signed(by),
        };
        self.pending_redraw = true;
    }

    fn select_log(&mut self, tab: LogTab) {
        self.log_tab = tab;
        self.log_scroll = 0;
        self.pending_redraw = true;
    }

    /// A left-button phase in surface pixels: the pane's while a press in
    /// it is held; otherwise a press on the bar, the search field, the
    /// log's tabs or a list row.
    fn pointer(
        &mut self,
        phase: PointerPhase,
        x: i64,
        y: i64,
        extend: bool,
        cache: &Cache,
        cmd_tx: &mpsc::Sender<BackendCommand>,
    ) {
        let layout = self.layout();
        let in_pane = layout.pane.is_some_and(|rect| rect.contains(x, y));
        if self.pane_drag || (phase == PointerPhase::Press && in_pane) {
            self.place_pane(&layout);
            match phase {
                PointerPhase::Press => self.pane_drag = true,
                PointerPhase::Release => self.pane_drag = false,
                PointerPhase::Move => {}
            }
            let phase = match phase {
                PointerPhase::Press => PanePhase::Press,
                PointerPhase::Move => PanePhase::Move,
                PointerPhase::Release => PanePhase::Release,
            };
            if let Some((tab, revision)) = self.pane_target() {
                if self.pane_event(Event::Pointer {
                    tab,
                    revision,
                    phase,
                    x,
                    cell_x: x,
                    y,
                    extend,
                }) == Outcome::Changed
                {
                    self.pending_redraw = true;
                }
            }
            return;
        }
        if phase != PointerPhase::Press {
            return;
        }
        if let Some(index) = layout.bar.hit(x, y) {
            if let Some(&key) = self.keys().get(index) {
                self.search_mode = false;
                self.handle_key(key, "", cache, cmd_tx);
            }
            return;
        }
        if layout
            .search
            .is_some_and(|field| field.rect().contains(x, y))
        {
            self.search_mode = true;
            self.pending_redraw = true;
            return;
        }
        if let Some(strip) = layout.strip {
            if let Some(TabHit::Select(index)) = strip.hit(x, y) {
                self.select_log(if index == 0 {
                    LogTab::News
                } else {
                    LogTab::Debug
                });
            }
            return;
        }
        let Some(row) = layout.list.and_then(|list| list.hit(x, y)) else {
            return;
        };
        match self.view {
            View::FeedList => {
                let index = self.feed_first + row;
                if index < self.feed_row_count() {
                    self.selected_feed = index;
                    self.pending_redraw = true;
                }
            }
            View::ArticleList => {
                let index = self.article_first + row;
                if index < self.filtered_article_indices().len() {
                    self.selected_article = index;
                    self.search_mode = false;
                    self.enter_article_view(cmd_tx);
                    self.pending_redraw = true;
                }
            }
            View::Article => {
                let index = self.url_first + row;
                if index < self.article_urls.len() {
                    self.url_cursor = index;
                    self.open_url(index);
                }
            }
            View::Log | View::Help => {}
        }
    }

    /// Wheel travel in rows: a list's selection, the log's window, or the
    /// pane's scroll.
    fn wheel(&mut self, rows: isize) {
        match self.view {
            View::FeedList => self.move_feed(rows),
            View::ArticleList => self.move_article(rows),
            View::Article if self.url_picking => self.move_url(rows),
            View::Article | View::Help => self.pane_scroll(rows),
            View::Log => self.scroll_log(rows),
        }
    }

    // ---- the document pane ---------------------------------------------

    /// Dispatches to the pane; an error is a diagnostic, not the reader's
    /// state, and reads as ignored.
    fn pane_event(&mut self, event: Event<'_>) -> Outcome {
        match self.pane.dispatch(event) {
            Ok(outcome) => outcome,
            Err(error) => {
                crate::log::error(format!("document pane: {error}"));
                Outcome::Ignored
            }
        }
    }

    fn pane_target(&self) -> Option<(TabId, u64)> {
        let tab = self.pane_tab?;
        let revision = self.pane.editor().document(tab).ok()?.revision();
        Some((tab, revision))
    }

    /// A reading key over the pane: j, k, Space and the page keys scroll
    /// it; the arrows, Home and End are its own chords.
    fn pane_key(&mut self, key: Key, chord: &str) {
        match key {
            Key::Char('j') => self.pane_scroll(1),
            Key::Char('k') => self.pane_scroll(-1),
            Key::PageDown | Key::Char(' ') => {
                let page = self.pane_rows() as isize;
                self.pane_scroll(page);
            }
            Key::PageUp => {
                let page = self.pane_rows() as isize;
                self.pane_scroll(-page);
            }
            _ => self.pane_chord(chord),
        }
    }

    /// A chord to the pane: a copy it asks for is the selection's way
    /// to the window's clipboard; its other requests, a read-only
    /// text's, are nothing here.
    fn pane_chord(&mut self, chord: &str) {
        if chord.is_empty() || !self.pane_shown() {
            return;
        }
        if let Some((tab, revision)) = self.pane_target() {
            match self.pane_event(Event::Key {
                tab,
                revision,
                chord,
            }) {
                Outcome::Changed => self.pending_redraw = true,
                Outcome::Request { name: "copy", .. } => self.copy_selection(),
                Outcome::Request { .. }
                | Outcome::Created(_)
                | Outcome::Prefix
                | Outcome::Ignored => {}
            }
        }
    }

    fn pane_scroll(&mut self, rows: isize) {
        if let Some((tab, revision)) = self.pane_target() {
            if self.pane_event(Event::Scroll {
                tab,
                revision,
                rows,
                columns: 0,
            }) == Outcome::Changed
            {
                self.pending_redraw = true;
            }
        }
    }

    /// The pane's rectangle, from the layout.
    fn place_pane(&mut self, layout: &Layout) {
        if let Some(rect) = layout.pane {
            let surface = self.surface;
            self.pane_event(Event::Frame { rect, surface });
        }
    }

    /// Shows `text` in the pane when `key` is not what it shows already,
    /// as `pane_source` admits it; a load the pane refuses all the same
    /// shows that it did, so the pane is never left blank.
    fn set_pane_text(&mut self, key: PaneText, text: &str) {
        if self.pane_text == key {
            return;
        }
        if let Some((tab, revision)) = self.pane_target() {
            self.pane_event(Event::Close { tab, revision });
        }
        self.pane_tab = None;
        self.pane_text = PaneText::None;
        let source = pane_source(text);
        let mut loaded = self.pane_event(Event::Load(source.as_bytes()));
        if !matches!(loaded, Outcome::Created(_)) {
            loaded = self.pane_event(Event::Load(b"This text cannot be shown."));
        }
        if let Outcome::Created(tab) = loaded {
            self.pane_event(Event::ReadOnly { tab, enabled: true });
            self.pane_tab = Some(tab);
            self.pane_text = key;
        }
    }

    /// The document rows and text columns the pane shows once placed
    /// for the layout, as td-editor lays its document out in the rect.
    fn pane_grid(&mut self, layout: &Layout) -> (usize, usize) {
        self.place_pane(layout);
        let (columns, rows) = self.pane.geometry().grid();
        (rows, columns)
    }

    fn pane_rows(&mut self) -> usize {
        let layout = self.layout();
        if layout.pane.is_none() {
            return PAGE_ROWS;
        }
        self.pane_grid(&layout).0.max(1)
    }

    fn list_rows(&self) -> usize {
        self.layout()
            .list
            .map_or(PAGE_ROWS, |list| list.rows().max(1))
    }

    // ---- the frame -------------------------------------------------------

    fn labels(&self) -> &'static [&'static str] {
        match self.view {
            View::FeedList => FEED_LABELS,
            View::ArticleList => ARTICLE_LIST_LABELS,
            View::Article if self.url_picking => LINKS_LABELS,
            View::Article => ARTICLE_LABELS,
            View::Log => LOG_LABELS,
            View::Help => HELP_LABELS,
        }
    }

    fn keys(&self) -> &'static [Key] {
        match self.view {
            View::FeedList => FEED_KEYS,
            View::ArticleList => ARTICLE_LIST_KEYS,
            View::Article if self.url_picking => LINKS_KEYS,
            View::Article => ARTICLE_KEYS,
            View::Log => LOG_KEYS,
            View::Help => HELP_KEYS,
        }
    }

    /// The view's regions over the surface.
    fn layout(&self) -> Layout {
        let surface = self.surface;
        let s = surface.scale.value();
        let row = (ROW * s) as u32;
        let bar = Bar::new(surface, self.labels());
        let status = Status::new(surface);
        let top = i64::from(row);
        let bottom = status.rect().y.max(top);
        let body = Rect {
            x: 0,
            y: top,
            width: surface.width as u32,
            height: (bottom - top) as u32,
        };
        let mut layout = Layout {
            bar,
            search: None,
            list: None,
            strip: None,
            pane: None,
            status,
        };
        // A body too short for a band gets none of it.
        let band = |body: Rect| -> (Rect, Rect) {
            let height = row.min(body.height);
            (
                Rect { height, ..body },
                Rect {
                    y: body.y + i64::from(height),
                    height: body.height - height,
                    ..body
                },
            )
        };
        match self.view {
            View::FeedList => layout.list = List::new(surface, body),
            View::ArticleList => {
                let (field, rest) = band(body);
                layout.search = TextEntry::new(surface, field);
                layout.list = List::new(surface, rest);
            }
            View::Article if self.url_picking => layout.list = List::new(surface, body),
            View::Article | View::Help => layout.pane = pane_rect(body),
            View::Log => {
                let (tabs, rest) = band(body);
                if tabs.height == row {
                    layout.strip = Strip::new(surface, tabs.y, log_tab_index(self.log_tab), 2)
                        .map(|strip| strip.with_close_buttons(false));
                }
                layout.pane = pane_rect(rest);
            }
        }
        layout
    }

    /// Lays the frame out: the lists reveal their selections, the pane is
    /// placed and holds what the view shows. Before every paint and every
    /// read of the frame.
    fn prepare_frame(&mut self) {
        let layout = self.layout();
        if let Some(list) = layout.list {
            match self.view {
                View::FeedList => {
                    self.feed_first =
                        list.reveal(self.feed_row_count(), self.selected_feed, self.feed_first);
                }
                View::ArticleList => {
                    let total = self.filtered_article_indices().len();
                    self.article_first =
                        list.reveal(total, self.selected_article, self.article_first);
                }
                View::Article => {
                    self.url_first =
                        list.reveal(self.article_urls.len(), self.url_cursor, self.url_first);
                }
                View::Log | View::Help => {}
            }
        }
        if let Some(field) = layout.search {
            let len = self.search.chars().count();
            self.search_first = field.reveal(len, len, self.search_first);
        }
        if layout.pane.is_none() {
            return;
        }
        let (rows, columns) = self.pane_grid(&layout);
        match self.view {
            View::Article => {
                let Some(article) = self.current_article().cloned() else {
                    self.set_pane_text(PaneText::NoArticle, "No article selected");
                    return;
                };
                let key = PaneText::Article {
                    hash: article.hash.clone(),
                    columns,
                };
                if self.pane_text != key {
                    let text = article_text(&article, &self.article_urls, columns);
                    self.set_pane_text(key, &text);
                }
            }
            View::Log => {
                let total = self.current_log_entry_count();
                let rows = rows.max(1);
                let start = self.log_scroll.min(total.unwrap_or(0).saturating_sub(rows));
                self.log_start = start;
                let key = PaneText::Log {
                    tab: log_tab_index(self.log_tab),
                    start,
                    rows,
                    columns,
                    total,
                };
                if self.pane_text != key {
                    let text = self.log_text(total, start, rows, columns.max(1));
                    self.log_shown_total = text.as_ref().and(total);
                    let text = text
                        .unwrap_or_else(|| format!("Could not read {}", self.log_path().display()));
                    self.set_pane_text(key, &text);
                }
            }
            View::Help => {
                if self.pane_text != PaneText::Help {
                    self.set_pane_text(PaneText::Help, &help_text());
                }
            }
            View::FeedList | View::ArticleList => {}
        }
    }

    /// The log view shows one window of the file, read line by line, so a
    /// log of any size costs a frame one window's worth of memory: the
    /// whole file in one string was the allocation that ended a session.
    fn log_text(
        &self,
        total: Option<usize>,
        start: usize,
        rows: usize,
        width: usize,
    ) -> Option<String> {
        let path = self.log_path();
        // Resume from the last frame's first line when it is not past this
        // one's; the count drops the hint of a log that shrank.
        let hint = self.log_window_hint.get(log_tab_index(self.log_tab));
        let from = hint
            .and_then(Cell::get)
            .filter(|&(index, _)| index <= start)
            .unwrap_or((0, 0));
        let window = match total {
            Some(0) => Some(vec!["Log is empty".to_string()]),
            Some(_) => log_window(&path, start, rows, width, from).map(|(window, first)| {
                if let Some(hint) = hint {
                    hint.set(Some(first));
                }
                window
            }),
            None => None,
        };
        window.map(|window| window.join("\n"))
    }

    /// The status row's text for the view, or the note while one is up.
    fn status_line(&self) -> String {
        if let Some(note) = &self.note {
            return note.clone();
        }
        match self.view {
            View::Article if self.url_picking => format!(
                "Link [{}] of {} (Enter open, 1-9 jump, q cancel)",
                self.url_cursor + 1,
                self.article_urls.len()
            ),
            // The read state is here, recomputed each frame, rather than
            // in the document, so a toggle reloads nothing; it leads, so
            // the link, which may be long, is what the row's end elides.
            View::Article => match self.current_article() {
                Some(article) => {
                    let state = if article.read { "read" } else { "unread" };
                    match article.published.as_ref() {
                        Some(published) => format!(
                            "{} | {} | {}",
                            state,
                            normalize_datetime_to_local(published)
                                .unwrap_or_else(|| "No date".to_string()),
                            article.link
                        ),
                        None => format!("{} | {}", state, article.link),
                    }
                }
                None => self.status.clone(),
            },
            View::Log => format!(
                "{} lines | {}",
                self.log_shown_total
                    .map_or_else(|| "?".to_string(), |total| total.to_string()),
                self.log_path().display()
            ),
            // M3: the scope the list is in, which the old header named.
            View::ArticleList => {
                let shown = self.filtered_article_indices().len();
                let scope = self.scope_label();
                if shown == 0 {
                    format!("{scope}: no matching articles")
                } else if self.search.is_empty() {
                    format!("{scope}: {shown} articles · {}", self.status)
                } else {
                    format!("{scope}: {shown} matching · {}", self.status)
                }
            }
            View::FeedList | View::Help => self.status.clone(),
        }
    }

    /// The article list's scope as the feed list names it.
    fn scope_label(&self) -> String {
        match self.selected_feed_scope.as_ref() {
            Some(FeedScope::All) | None => "[All]".to_string(),
            Some(FeedScope::Unread) => "[Unread]".to_string(),
            Some(FeedScope::Feed(url)) => self
                .feeds
                .iter()
                .find(|feed| &feed.url == url)
                .map_or_else(|| "Feed".to_string(), |feed| feed.name.clone()),
        }
    }

    /// The list's rows as shown: label, meta, marked, from its first.
    fn list_rows_shown(&self, list: List) -> Vec<(String, String, bool)> {
        let rows = list.rows();
        let mut shown = Vec::with_capacity(rows);
        match self.view {
            View::FeedList => {
                let count = self.feed_row_count();
                for index in self.feed_first..count.min(self.feed_first + rows) {
                    shown.push(self.feed_row(index));
                }
            }
            View::ArticleList => {
                let visible = self.filtered_article_indices();
                let show_feed = matches!(
                    self.selected_feed_scope.as_ref(),
                    Some(FeedScope::All | FeedScope::Unread)
                );
                for article in visible
                    .iter()
                    .skip(self.article_first)
                    .take(rows)
                    .filter_map(|&index| self.articles.get(index))
                {
                    let published = article
                        .published
                        .as_deref()
                        .and_then(normalize_datetime_to_local)
                        .unwrap_or_else(|| "No date".to_string());
                    let meta = if show_feed {
                        format!(
                            "{} · {}",
                            published,
                            views::strip_newlines(&article.feed_name)
                        )
                    } else {
                        published
                    };
                    shown.push((views::strip_newlines(&article.title), meta, !article.read));
                }
            }
            View::Article => {
                for (index, url) in self
                    .article_urls
                    .iter()
                    .enumerate()
                    .skip(self.url_first)
                    .take(rows)
                {
                    shown.push((url.clone(), format!("[{}]", index + 1), false));
                }
            }
            View::Log | View::Help => {}
        }
        shown
    }

    /// Feed row `index`: `[All]`, `[Unread]`, the feeds in order, `[Log]`.
    fn feed_row(&self, index: usize) -> (String, String, bool) {
        let updated = |value: Option<&String>| value.cloned().unwrap_or_else(|| "-".to_string());
        if index == 0 {
            (
                "[All]".to_string(),
                format!(
                    "{} unread of {} · {}",
                    self.total_unread(),
                    self.total_articles(),
                    updated(self.last_updated.as_ref())
                ),
                false,
            )
        } else if index == 1 {
            (
                "[Unread]".to_string(),
                format!("{} unread", self.total_unread()),
                false,
            )
        } else if index == self.log_row_index() {
            (
                "[Log]".to_string(),
                format!(
                    "{} lines",
                    self.current_log_entry_count()
                        .map_or_else(|| "?".to_string(), |count| count.to_string())
                ),
                false,
            )
        } else {
            match index.checked_sub(2).and_then(|i| self.feeds.get(i)) {
                Some(feed) => (
                    feed.name.clone(),
                    format!(
                        "{} unread of {} · {}{}",
                        feed.unread,
                        feed.total,
                        updated(feed.last_updated.as_ref()),
                        if feed.last_error.is_some() { " !" } else { "" }
                    ),
                    feed.last_error.is_some(),
                ),
                None => (String::new(), String::new(), false),
            }
        }
    }

    fn list_selection(&self) -> (usize, usize, usize) {
        match self.view {
            View::FeedList => (self.feed_first, self.selected_feed, self.feed_row_count()),
            View::ArticleList => (
                self.article_first,
                self.selected_article,
                self.filtered_article_indices().len(),
            ),
            View::Article => (self.url_first, self.url_cursor, self.article_urls.len()),
            View::Log | View::Help => (0, 0, 0),
        }
    }

    /// Paints the frame whole into the raster, after `prepare_frame`.
    fn paint(&mut self, raster: &mut Raster<'_, '_>) -> Result<(), String> {
        self.prepare_frame();
        let surface = self.surface;
        raster
            .paint(&Frame { app: self }, surface.bounds())
            .map_err(|e| e.to_string())?;
        self.pending_redraw = false;
        Ok(())
    }

    fn enter_article_view(&mut self, cmd_tx: &mpsc::Sender<BackendCommand>) {
        self.sync_open_article_to_selection();
        self.view = View::Article;
        self.mark_current_article_read(cmd_tx);
        self.extract_current_article_urls();
        self.url_picking = false;
    }

    fn leave_article_view(&mut self) {
        self.view = View::ArticleList;
        self.open_article = None;
        self.article_urls.clear();
        self.url_picking = false;
    }

    fn extract_current_article_urls(&mut self) {
        self.article_urls.clear();
        let Some(article) = self.current_article().cloned() else {
            return;
        };
        let mut seen = std::collections::HashSet::new();
        // Always include the article's own link as [1]
        if !article.link.is_empty() {
            seen.insert(article.link.clone());
            self.article_urls.push(article.link.clone());
        }
        let html_source = if article.content.trim().is_empty() {
            &article.description
        } else {
            &article.content
        };
        // Extract href URLs from HTML source
        for url in extract_href_urls(html_source) {
            if seen.insert(url.clone()) {
                self.article_urls.push(url);
            }
        }
        // Also scan the plain text rendering for URLs
        let text = crate::html::to_text(html_source.as_bytes(), 200);
        for url in extract_plain_urls(&text) {
            if seen.insert(url.clone()) {
                self.article_urls.push(url);
            }
        }
    }

    fn toggle_current_read(&mut self, _cache: &Cache, cmd_tx: &mpsc::Sender<BackendCommand>) {
        if let Some(article) = self.current_article().cloned() {
            let read = !article.read;
            let _ = cmd_tx.send(BackendCommand::MarkRead {
                hash: article.hash.clone(),
                read,
            });
            self.pending_read_mutations
                .insert(article.hash.clone(), read);
            self.apply_article_read(&article.hash, read);
            self.recount_current_feed_unread();
            self.pending_redraw = true;
        }
    }

    fn open_current_article(&mut self) {
        if let Some(article) = self.current_article() {
            let link = article.link.clone();
            self.status = match open_in_browser(&link, self.browser.as_deref()) {
                Ok(()) => "Opened in browser".to_string(),
                Err(e) => format!("Failed to open browser: {}", e),
            };
            self.pending_redraw = true;
        }
    }

    fn open_html_digest(&mut self) {
        let visible = self.filtered_article_indices();
        if visible.is_empty() {
            self.status = "No articles to export".to_string();
            self.pending_redraw = true;
            return;
        }

        let feed_name = match self.selected_feed_scope.as_ref() {
            Some(FeedScope::All) => "[All]",
            Some(FeedScope::Unread) => "[Unread]",
            Some(FeedScope::Feed(url)) => self
                .feeds
                .iter()
                .find(|f| &f.url == url)
                .map(|f| f.name.as_str())
                .unwrap_or("Articles"),
            None => "Articles",
        };

        let mut html = String::new();
        html.push_str("<!DOCTYPE html>\n<html>\n<head>\n");
        html.push_str(&format!(
            "    <title>{} - td-news digest</title>\n",
            html_escape(feed_name)
        ));
        html.push_str(concat!(
            "    <style>\n",
            "        body { font-family: sans-serif; max-width: 900px; margin: 2rem auto; line-height: 1.6; }\n",
            "        ul { list-style-type: none; padding: 0; }\n",
            "        li { margin-bottom: 1rem; border-bottom: 1px solid #eee; padding-bottom: 0.5rem; display: flex; align-items: baseline; flex-wrap: wrap; }\n",
            "        .date { font-family: monospace; color: #666; font-size: 0.9rem; margin-right: 1rem; white-space: nowrap; }\n",
            "        .feed { font-family: monospace; color: #999; font-size: 0.85rem; margin-right: 0.5rem; }\n",
            "        .subject { font-weight: bold; font-size: 1.1rem; margin-right: 0.5rem; }\n",
            "        .subject a { text-decoration: none; color: #0066cc; }\n",
            "        .subject a:hover { text-decoration: underline; }\n",
            "        .unread { font-weight: bold; }\n",
            "    </style>\n",
        ));
        html.push_str("</head>\n<body>\n");
        html.push_str(&format!(
            "    <h1>{} - td-news digest</h1>\n    <ul>\n",
            html_escape(feed_name)
        ));

        let show_feed_source = matches!(
            self.selected_feed_scope.as_ref(),
            Some(FeedScope::All | FeedScope::Unread)
        );

        for article in visible.iter().filter_map(|&idx| self.articles.get(idx)) {
            let date = article
                .published
                .as_deref()
                .and_then(normalize_datetime_to_local)
                .unwrap_or_else(|| "No date".to_string());
            let unread_class = if !article.read { " unread" } else { "" };
            let feed_span = if show_feed_source {
                format!(
                    "<span class=\"feed\">[{}]</span>",
                    html_escape(&article.feed_name)
                )
            } else {
                String::new()
            };
            let title_html = if article.link.is_empty() {
                html_escape(&article.title)
            } else {
                format!(
                    "<a href=\"{}\">{}</a>",
                    html_escape(&article.link),
                    html_escape(&article.title)
                )
            };
            html.push_str(&format!(
                "        <li class=\"{}\"><span class=\"date\">{}</span>{}<span class=\"subject\">{}</span></li>\n",
                unread_class.trim(),
                html_escape(&date),
                feed_span,
                title_html,
            ));
        }

        html.push_str("    </ul>\n</body>\n</html>\n");

        // Write to temp file and open in browser
        let dir = std::env::temp_dir();
        let path = dir.join("td-news-digest.html");
        match std::fs::write(&path, &html) {
            Ok(()) => {
                let url = format!("file://{}", path.display());
                self.status = match open_in_browser(&url, self.browser.as_deref()) {
                    Ok(()) => format!("Opened digest ({} articles)", visible.len()),
                    Err(e) => format!("Failed to open browser: {}", e),
                };
            }
            Err(e) => {
                self.status = format!("Failed to write digest: {}", e);
            }
        }
        self.pending_redraw = true;
    }

    fn mark_current_article_read(&mut self, cmd_tx: &mpsc::Sender<BackendCommand>) {
        if let Some(article) = self.current_article().cloned() {
            if article.read {
                return;
            }
            let _ = cmd_tx.send(BackendCommand::MarkRead {
                hash: article.hash.clone(),
                read: true,
            });
            self.pending_read_mutations
                .insert(article.hash.clone(), true);
            self.apply_article_read(&article.hash, true);
            self.recount_current_feed_unread();
        }
    }

    fn find_next_unread_article_idx(
        &self,
        visible_indices: &[usize],
        selected_visible_idx: usize,
    ) -> Option<usize> {
        find_next_unread_visible_index(visible_indices, selected_visible_idx, |article_idx| {
            self.articles
                .get(article_idx)
                .map(|article| !article.read)
                .unwrap_or(false)
        })
        .and_then(|visible_idx| visible_indices.get(visible_idx).copied())
    }

    fn reload_feeds_from_cache(&mut self, cache: &Cache) {
        let mut latest_ts = None;
        let mut latest_str = None;
        let pending_read_mutations = &self.pending_read_mutations;
        for row in &mut self.feeds {
            let hashes = cache.get_feed_index(&row.url).unwrap_or_default();
            row.total = hashes.len();
            row.unread = hashes
                .iter()
                .filter(|hash| {
                    cache
                        .get_article(hash)
                        .map(|a| Self::is_unread_with_pending(&a, pending_read_mutations))
                        .unwrap_or(false)
                })
                .count();
            if let Some(meta) = cache.get_feed_meta(&row.url) {
                row.last_updated = normalize_datetime_to_local(&meta.last_fetched);
                row.last_error = None;
                if let Some(ts) = datetime_sort_key(Some(&meta.last_fetched)) {
                    if latest_ts.map(|cur| ts > cur).unwrap_or(true) {
                        latest_ts = Some(ts);
                        latest_str = Some(meta.last_fetched.clone());
                    }
                }
            }
        }
        if let Some(s) = latest_str {
            self.last_updated = Some(s);
        }
    }

    fn reload_articles(&mut self, cache: &Cache) {
        let selected_hash = self.selected_list_article_hash();
        self.articles.clear();
        if let Some(scope) = self.selected_feed_scope.as_ref() {
            match scope {
                FeedScope::Feed(url) => {
                    let hashes = cache.get_feed_index(url).unwrap_or_default();
                    self.articles = hashes
                        .iter()
                        .filter_map(|hash| cache.get_article(hash))
                        .collect();
                }
                FeedScope::All | FeedScope::Unread => {
                    let unread_only = matches!(scope, FeedScope::Unread);
                    let pending_read_mutations = &self.pending_read_mutations;
                    let per_feed_articles: Vec<Vec<Article>> = self
                        .feeds
                        .iter()
                        .map(|feed| {
                            cache
                                .get_feed_index(&feed.url)
                                .unwrap_or_default()
                                .iter()
                                .filter_map(|hash| cache.get_article(hash))
                                .filter(|a| {
                                    !unread_only
                                        || Self::is_unread_with_pending(a, pending_read_mutations)
                                })
                                .collect::<Vec<_>>()
                        })
                        .collect();
                    let mut positions = vec![0usize; per_feed_articles.len()];
                    loop {
                        let mut progressed = false;
                        for (feed_articles, pos) in per_feed_articles.iter().zip(&mut positions) {
                            if let Some(article) = feed_articles.get(*pos).cloned() {
                                self.articles.push(article);
                                *pos += 1;
                                progressed = true;
                            }
                        }
                        if !progressed {
                            break;
                        }
                    }
                }
            }
        }
        Self::apply_pending_read_mutations(&mut self.articles, &self.pending_read_mutations);
        self.refresh_open_article(cache);
        let unread_oldest_first =
            matches!(self.selected_feed_scope.as_ref(), Some(FeedScope::Unread));
        self.articles.sort_by(|a, b| {
            let a_ts = datetime_sort_key(a.published.as_deref()).unwrap_or(i64::MIN);
            let b_ts = datetime_sort_key(b.published.as_deref()).unwrap_or(i64::MIN);
            if unread_oldest_first {
                a_ts.cmp(&b_ts).then_with(|| a.title.cmp(&b.title))
            } else {
                b_ts.cmp(&a_ts).then_with(|| a.title.cmp(&b.title))
            }
        });
        if let Some(hash) = selected_hash {
            self.select_visible_article_by_hash(&hash);
        }
        let visible_len = self.filtered_article_indices().len();
        if self.selected_article >= visible_len {
            self.selected_article = visible_len.saturating_sub(1);
        }
    }

    fn feed_unread_count_from_cache(&self, cache: &Cache, feed_url: &str) -> usize {
        cache
            .get_feed_index(feed_url)
            .unwrap_or_default()
            .iter()
            .filter(|hash| {
                cache
                    .get_article(hash)
                    .map(|a| Self::is_unread_with_pending(&a, &self.pending_read_mutations))
                    .unwrap_or(false)
            })
            .count()
    }

    fn is_unread_with_pending(
        article: &Article,
        pending_read_mutations: &HashMap<String, bool>,
    ) -> bool {
        !pending_read_mutations
            .get(&article.hash)
            .copied()
            .unwrap_or(article.read)
    }

    fn apply_pending_read_mutations(
        articles: &mut [Article],
        pending_read_mutations: &HashMap<String, bool>,
    ) {
        for article in articles {
            if let Some(read) = pending_read_mutations.get(&article.hash).copied() {
                article.read = read;
            }
        }
    }

    fn refresh_open_article(&mut self, cache: &Cache) {
        let Some(hash) = self
            .open_article
            .as_ref()
            .map(|article| article.hash.clone())
        else {
            return;
        };
        let mut article = self
            .articles
            .iter()
            .find(|article| article.hash == hash)
            .cloned()
            .or_else(|| cache.get_article(&hash));
        if let Some(article) = article.as_mut() {
            if let Some(read) = self.pending_read_mutations.get(&hash).copied() {
                article.read = read;
            }
        }
        if article.is_some() {
            self.open_article = article;
        }
    }

    fn select_visible_article_by_hash(&mut self, hash: &str) -> bool {
        let visible = self.filtered_article_indices();
        if let Some(selected) = visible
            .iter()
            .position(|&idx| self.articles.get(idx).map(|a| a.hash.as_str()) == Some(hash))
        {
            self.selected_article = selected;
            true
        } else {
            false
        }
    }

    fn recount_current_feed_unread(&mut self) {
        let Some(scope) = self.selected_feed_scope.as_ref() else {
            return;
        };
        match scope {
            FeedScope::Feed(url) => {
                let unread = self.articles.iter().filter(|a| !a.read).count();
                if let Some(row) = self.feeds.iter_mut().find(|f| &f.url == url) {
                    row.unread = unread;
                    row.total = self.articles.len();
                }
            }
            FeedScope::All | FeedScope::Unread => {
                let mut unread_by_feed_name: HashMap<&str, usize> = HashMap::new();
                for article in &self.articles {
                    if !article.read {
                        *unread_by_feed_name
                            .entry(article.feed_name.as_str())
                            .or_insert(0) += 1;
                    }
                }
                for row in &mut self.feeds {
                    row.unread = unread_by_feed_name
                        .get(row.name.as_str())
                        .copied()
                        .unwrap_or(0);
                }
            }
        }
    }

    fn filtered_article_indices(&self) -> Vec<usize> {
        if self.search.is_empty() {
            return (0..self.articles.len()).collect();
        }
        let query = self.search.to_lowercase();
        self.articles
            .iter()
            .enumerate()
            .filter_map(|(idx, article)| {
                let hay = format!(
                    "{} {} {}",
                    article.title.to_lowercase(),
                    article.description.to_lowercase(),
                    article.content.to_lowercase()
                );
                if hay.contains(&query) {
                    Some(idx)
                } else {
                    None
                }
            })
            .collect()
    }

    fn current_article(&self) -> Option<&Article> {
        if matches!(self.view, View::Article) {
            if let Some(article) = self.open_article.as_ref() {
                return Some(article);
            }
        }
        self.selected_list_article()
    }

    fn selected_list_article(&self) -> Option<&Article> {
        let visible = self.filtered_article_indices();
        let idx = *visible.get(self.selected_article)?;
        self.articles.get(idx)
    }

    fn selected_list_article_hash(&self) -> Option<String> {
        self.selected_list_article()
            .map(|article| article.hash.clone())
    }

    fn sync_open_article_to_selection(&mut self) {
        self.open_article = self.selected_list_article().cloned();
    }

    fn apply_article_read(&mut self, hash: &str, read: bool) {
        if let Some(article) = self.articles.iter_mut().find(|a| a.hash == hash) {
            article.read = read;
        }
        if let Some(article) = self
            .open_article
            .as_mut()
            .filter(|article| article.hash == hash)
        {
            article.read = read;
        }
    }

    fn feed_row_count(&self) -> usize {
        self.feeds.len() + 3
    }

    fn log_row_index(&self) -> usize {
        self.feeds.len() + 2
    }

    fn scope_for_selected_feed(&self) -> FeedScope {
        if self.selected_feed == 0 {
            FeedScope::All
        } else if self.selected_feed == 1 {
            FeedScope::Unread
        } else if self.selected_feed >= self.log_row_index() {
            FeedScope::All
        } else {
            // Rows 2.. are the feeds; a row past them scopes to everything.
            self.selected_feed
                .checked_sub(2)
                .and_then(|i| self.feeds.get(i))
                .map_or(FeedScope::All, |feed| FeedScope::Feed(feed.url.clone()))
        }
    }

    fn total_articles(&self) -> usize {
        self.feeds.iter().map(|f| f.total).sum()
    }

    fn total_unread(&self) -> usize {
        self.feeds.iter().map(|f| f.unread).sum()
    }

    fn log_path(&self) -> PathBuf {
        match self.log_tab {
            LogTab::News => crate::log::news_log_path(),
            LogTab::Debug => crate::log::debug_log_path(),
        }
    }

    /// How many lines the shown log has, or nothing for one that cannot be
    /// read: `count_log` over this log's kept count and resume point.
    fn current_log_entry_count(&self) -> Option<usize> {
        let tab = log_tab_index(self.log_tab);
        count_log(
            &self.log_path(),
            self.log_count.get(tab)?,
            self.log_window_hint.get(tab)?,
        )
    }
}

/// `text` as the pane's document admits it: CRLF is one newline, a
/// control scalar other than newline and tab (a feed may carry one in a
/// title) is the replacement character, a leading byte order mark goes,
/// and the text is cut at the document's size ceiling.
fn pane_source(text: &str) -> String {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut source = String::with_capacity(text.len());
    for c in text.replace("\r\n", "\n").chars() {
        let shown = match c {
            '\n' | '\t' => c,
            c if c <= '\u{1f}' || c == '\u{7f}' => '\u{fffd}',
            c => c,
        };
        if source.len() + shown.len_utf8() > td_editor::text::MAX_FILE_BYTES {
            break;
        }
        source.push(shown);
    }
    source
}

/// The document pane's rectangle, none for a body with no room.
fn pane_rect(body: Rect) -> Option<Rect> {
    (body.width > 0 && body.height > 0).then_some(body)
}

/// `index` moved by `by` within `count`, the extremes meaning the ends.
fn step(index: usize, by: isize, count: usize) -> usize {
    let last = count.saturating_sub(1);
    match by {
        isize::MIN => 0,
        isize::MAX => last,
        _ => index.saturating_add_signed(by).min(last),
    }
}

/// The reader's frame as a composition: what the window paints and what
/// a test reads back.
struct Frame<'a> {
    app: &'a App,
}

impl Composition for Frame<'_> {
    fn surface(&self) -> Surface {
        self.app.surface
    }

    fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        let app = self.app;
        let layout = app.layout();
        if let Some(clip) = damage.intersection(app.surface.bounds()) {
            sink(Draw {
                clip,
                primitive: Primitive::Fill {
                    rect: app.surface.bounds(),
                    color: PAPER,
                },
            });
        }
        layout.bar.emit(damage, sink);
        if let Some(field) = layout.search {
            let caret = app.search.chars().count();
            field.emit(
                Field {
                    text: &app.search,
                    placeholder: "Search",
                    caret,
                    anchor: None,
                    first: app.search_first,
                    masked: false,
                    focused: app.search_mode,
                    caret_visible: app.search_mode,
                },
                damage,
                sink,
            );
        }
        if let Some(list) = layout.list {
            let rows = app.list_rows_shown(list);
            let (first, selected, total) = app.list_selection();
            list.emit(
                rows.iter().map(|(label, meta, marked)| Item {
                    label: label.as_str(),
                    meta: meta.as_str(),
                    enabled: true,
                    marked: *marked,
                }),
                first,
                selected,
                total,
                damage,
                sink,
            );
        }
        if let Some(strip) = layout.strip {
            strip.emit([("News", false), ("Debug", false)], damage, sink);
        }
        if layout.pane.is_some() {
            if let Ok(scene) = app.pane.scene(&[]) {
                scene.emit(damage, sink);
            }
        }
        layout.status.emit(app.status_line().chars(), damage, sink);
    }
}

/// An article as the pane shows it: its title, source and link, the
/// rendered body, and the links it carries, numbered as the picker does.
fn article_text(article: &Article, urls: &[String], columns: usize) -> String {
    let mut text = String::new();
    text.push_str(&views::strip_newlines(&article.title));
    text.push('\n');
    let published = article
        .published
        .as_deref()
        .and_then(normalize_datetime_to_local)
        .unwrap_or_else(|| "No date".to_string());
    text.push_str(&format!(
        "{} · {}\n",
        views::strip_newlines(&article.feed_name),
        published
    ));
    text.push('\n');
    let html = if article.content.trim().is_empty() {
        article.description.as_str()
    } else {
        article.content.as_str()
    };
    let body = crate::html::to_text(html.as_bytes(), columns.max(20));
    // The renderer's reference definitions (`[1]: https://...`) are
    // dropped: the Links section below lists the same URLs.
    let mut rendered: Vec<&str> = body
        .lines()
        .filter(|line| !is_reference_link_def(line))
        .collect();
    while rendered.last().is_some_and(|line| line.is_empty()) {
        rendered.pop();
    }
    for line in rendered {
        text.push_str(line);
        text.push('\n');
    }
    if !urls.is_empty() {
        text.push_str("\nLinks:\n");
        for (index, url) in urls.iter().enumerate() {
            text.push_str(&format!("  [{}] {}\n", index + 1, url));
        }
    }
    text
}

/// The help as the pane shows it.
fn help_text() -> String {
    let mut text = String::from("Keys\n\n");
    for (label, items) in [
        ("Everywhere", keybindings::GLOBAL),
        ("Feeds", keybindings::FEED_LIST),
        ("Articles", keybindings::ARTICLE_LIST),
        ("Article", keybindings::ARTICLE_VIEW),
        ("Log", keybindings::LOG_VIEW),
    ] {
        text.push_str(label);
        text.push('\n');
        for item in items {
            text.push_str("  ");
            text.push_str(item);
            text.push('\n');
        }
        text.push('\n');
    }
    text.push_str("Mouse\n");
    for item in keybindings::MOUSE {
        text.push_str("  ");
        text.push_str(item);
        text.push('\n');
    }
    text
}

/// The shell script that runs the configured browser command with the URL
/// as its `$1`. The URL is not written into the script, so nothing a link
/// carries is read as shell, however the command is written: a `{url}`
/// becomes `"$1"` (quotes a person put around the placeholder are taken
/// off first, since inside them the expansion would be unquoted); without
/// a placeholder the URL is appended as one word. The caller runs it as
/// `sh -f -c <script> sh <url>`, `-f` so a `?` or `*` in the link is not a
/// pattern either.
fn browser_script(cmd: &str) -> String {
    let cmd = cmd
        .replace("\"{url}\"", "{url}")
        .replace("'{url}'", "{url}");
    if cmd.contains("{url}") {
        cmd.replace("{url}", "\"$1\"")
    } else {
        format!("{cmd} \"$1\"")
    }
}

fn open_in_browser(url: &str, browser_config: Option<&str>) -> Result<(), String> {
    // 1. Config browser command (supports {url} template, executed via sh -c)
    if let Some(cmd) = browser_config {
        let status = std::process::Command::new("sh")
            .arg("-f")
            .arg("-c")
            .arg(browser_script(cmd))
            .arg("sh")
            .arg(url)
            .status()
            .map_err(|e| e.to_string())?;
        if status.success() {
            return Ok(());
        }
        return Err(format!("non-zero exit status: {}", status));
    }

    // 2. $BROWSER env var
    if let Ok(browser) = std::env::var("BROWSER") {
        let status = std::process::Command::new(browser)
            .arg(url)
            .status()
            .map_err(|e| e.to_string())?;
        if status.success() {
            return Ok(());
        }
        return Err(format!("non-zero exit status: {}", status));
    }

    // 3. Fallback openers
    for opener in ["xdg-open", "open"] {
        match std::process::Command::new(opener).arg(url).status() {
            Ok(status) if status.success() => return Ok(()),
            _ => continue,
        }
    }

    Err("no browser opener available".to_string())
}

/// Detect markdown reference link definitions like `[1]: https://example.com`
/// the renderer emits, so we can strip them from rendered output.
fn is_reference_link_def(line: &str) -> bool {
    let trimmed = line.trim();
    if !trimmed.starts_with('[') {
        return false;
    }
    if let Some(bracket_end) = trimmed.find("]: ") {
        let label = &trimmed[1..bracket_end];
        // Reference labels are numeric, as the renderer numbers them
        label.chars().all(|c| c.is_ascii_digit()) && !label.is_empty()
    } else {
        false
    }
}

fn extract_href_urls(html: &str) -> Vec<String> {
    let mut urls = Vec::new();
    let needle = "href=\"";
    let mut pos = 0;
    while let Some(start) = html[pos..].find(needle) {
        let url_start = pos + start + needle.len();
        pos = url_start;
        if let Some(end) = html[url_start..].find('"') {
            let url = &html[url_start..url_start + end];
            if url.starts_with("http://") || url.starts_with("https://") {
                urls.push(url.to_string());
            }
            pos = url_start + end + 1;
        }
    }
    urls
}

fn extract_plain_urls(text: &str) -> Vec<String> {
    let mut urls = Vec::new();
    for word in text.split(|c: char| c.is_whitespace() || c == '<' || c == '>' || c == '"') {
        let word = word.trim_end_matches(['.', ',', ')', ']', ';']);
        if (word.starts_with("http://") || word.starts_with("https://")) && word.len() > 10 {
            urls.push(word.to_string());
        }
    }
    urls
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn find_next_unread_visible_index<F>(
    visible_indices: &[usize],
    selected_visible_idx: usize,
    mut is_unread: F,
) -> Option<usize>
where
    F: FnMut(usize) -> bool,
{
    if selected_visible_idx >= visible_indices.len() {
        return None;
    }

    for (idx, &vi) in visible_indices
        .iter()
        .enumerate()
        .skip(selected_visible_idx + 1)
    {
        if is_unread(vi) {
            return Some(idx);
        }
    }

    visible_indices
        .iter()
        .enumerate()
        .take(selected_visible_idx)
        .rev()
        .find(|&(_, &vi)| is_unread(vi))
        .map(|(idx, _)| idx)
}

fn refresh_status(
    reports: &[FeedRefreshReport],
    successes: usize,
    errors: usize,
    new_total: usize,
) -> String {
    if reports.is_empty() {
        return "No feeds refreshed".to_string();
    }
    if let [report] = reports {
        if let Some(error) = report.error.as_ref() {
            return format!("Fetch error: {}", error);
        }
        if report.new_articles == 0 {
            format!("{}: no new items", report.feed_name)
        } else {
            format!(
                "{}: {} new item(s), {} total ({} unread)",
                report.feed_name, report.new_articles, report.total, report.unread
            )
        }
    } else if errors == 0 {
        if new_total == 0 {
            format!("Refresh complete: {} feeds, no new items", successes)
        } else {
            format!(
                "Refresh complete: {} feeds, {} new item(s)",
                successes, new_total
            )
        }
    } else {
        format!(
            "Refresh complete: {} feeds, {} new item(s), {} error(s)",
            successes, new_total, errors
        )
    }
}

/// The scanner's read size: the most of a log a frame holds at once.
const LOG_CHUNK: usize = 64 * 1024;

/// The log's line count, kept in `known` and counted onward from the
/// length already counted, the log being written only by appending. A
/// log shorter than the one counted was truncated: its count and the
/// view's resume point in `hint` are dropped, since the bytes there are
/// not what they were, and it is counted afresh. A log truncated and
/// regrown past its counted length between two frames is not told
/// apart. Nothing for a log that cannot be read: one still as long as
/// the one counted keeps its count, the bytes counted being there yet,
/// and one that is not there keeps nothing.
fn count_log(
    path: &std::path::Path,
    known: &Cell<Option<LogCount>>,
    hint: &Cell<Option<(usize, u64)>>,
) -> Option<usize> {
    // A log that is not there is not the one counted: what comes to the
    // path next is counted from its start.
    let Some(len) = std::fs::metadata(path).ok().map(|meta| meta.len()) else {
        known.set(None);
        hint.set(None);
        return None;
    };
    let counted = known.get().filter(|counted| counted.len <= len);
    if counted.is_none() {
        known.set(None);
        hint.set(None);
    }
    if let Some(counted) = counted.filter(|counted| counted.len == len) {
        return Some(counted.total());
    }
    let mut count = counted.unwrap_or(LogCount::NONE);
    let (newlines, last, read) = count_newlines_from(path, count.len)?;
    count.newlines += newlines;
    count.len += read;
    if let Some(last) = last {
        count.ends_with_newline = last;
    }
    known.set(Some(count));
    Some(count.total())
}

/// Newlines from byte `from` of the file to its end, whether the last byte
/// read was one, and how many bytes were read; nothing for a file that
/// cannot be read, which is not a count.
fn count_newlines_from(path: &std::path::Path, from: u64) -> Option<(usize, Option<bool>, u64)> {
    let mut file = std::fs::File::open(path).ok()?;
    file.seek(SeekFrom::Start(from)).ok()?;
    let mut buf = vec![0u8; LOG_CHUNK];
    let (mut newlines, mut last, mut read_total) = (0usize, None, 0u64);
    loop {
        let read = match file.read(&mut buf) {
            Ok(0) => break,
            Ok(read) => read,
            Err(_) => return None,
        };
        let chunk = buf.get(..read).unwrap_or_default();
        newlines += chunk.iter().filter(|&&byte| byte == b'\n').count();
        last = chunk.last().map(|&byte| byte == b'\n');
        read_total += read as u64;
    }
    Some((newlines, last, read_total))
}

/// `rows` lines of the log from line `start`, each cut to `width` columns,
/// read through one buffer from `from`, a line's index and the offset it
/// begins at, which may be the file's start or a line already found: a
/// line keeps at most the bytes `width` columns can need, so a line of any
/// length costs a frame a bounded amount, the lines skipped keep nothing,
/// and a line the log wrote in bytes that are not UTF-8 shows with the
/// replacement character. With the window comes where its first line
/// began, for the next frame to resume from. Nothing for a file that
/// cannot be read.
fn log_window(
    path: &std::path::Path,
    start: usize,
    rows: usize,
    width: usize,
    from: (usize, u64),
) -> Option<(Vec<String>, (usize, u64))> {
    let mut file = std::fs::File::open(path).ok()?;
    let (mut index, mut position) = from;
    file.seek(SeekFrom::Start(position)).ok()?;
    // Four bytes a character is the most UTF-8 needs.
    let keep = width.saturating_mul(4);
    let mut buf = vec![0u8; LOG_CHUNK];
    let mut window = Vec::with_capacity(rows);
    let mut line: Vec<u8> = Vec::new();
    let mut line_start = position;
    let mut first = None;
    let mut open = false;
    while window.len() < rows {
        let read = match file.read(&mut buf) {
            Ok(0) => break,
            Ok(read) => read,
            Err(_) => return None,
        };
        let mut chunk = buf.get(..read).unwrap_or_default();
        while !chunk.is_empty() && window.len() < rows {
            let end = chunk.iter().position(|&byte| byte == b'\n');
            let piece = chunk.get(..end.unwrap_or(chunk.len())).unwrap_or_default();
            if index >= start {
                first.get_or_insert((index, line_start));
                let room = keep.saturating_sub(line.len());
                line.extend_from_slice(piece.get(..piece.len().min(room)).unwrap_or_default());
            }
            open = true;
            match end {
                Some(at) => {
                    if index >= start {
                        window.push(finish_log_line(&mut line, width));
                    }
                    index += 1;
                    open = false;
                    position += at as u64 + 1;
                    line_start = position;
                    chunk = chunk.get(at + 1..).unwrap_or_default();
                }
                None => {
                    position += piece.len() as u64;
                    chunk = &[];
                }
            }
        }
    }
    if open && index >= start && window.len() < rows {
        first.get_or_insert((index, line_start));
        window.push(finish_log_line(&mut line, width));
    }
    Some((window, first.unwrap_or((index, line_start))))
}

fn finish_log_line(line: &mut Vec<u8>, width: usize) -> String {
    let text = String::from_utf8_lossy(line);
    let shown = views::truncate(text.trim_end_matches('\r'), width);
    line.clear();
    shown
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// A link reaches the browser as one argument whichever way the command
    /// is written, and none of it is read as shell: the script is run
    /// through sh here, with a link that would print INJECTED if it were.
    #[test]
    fn a_link_reaches_the_browser_as_one_argument_however_the_command_is_written() {
        let link = "https://x/$(printf INJECTED);'a\"b?*&c";
        for (cmd, expected) in [
            ("printf '%s\\n' {url}", link.to_string()),
            ("printf '%s\\n' \"{url}\"", link.to_string()),
            ("printf '%s\\n' '{url}'", link.to_string()),
            ("printf '%s\\n' --url={url}", format!("--url={link}")),
            ("printf '%s\\n'", link.to_string()),
        ] {
            let script = browser_script(cmd);
            assert!(!script.contains("INJECTED"), "{cmd}: {script}");
            let out = std::process::Command::new("sh")
                .arg("-f")
                .arg("-c")
                .arg(&script)
                .arg("sh")
                .arg(link)
                .output()
                .expect("sh");
            assert!(
                out.status.success(),
                "{cmd}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert_eq!(
                String::from_utf8_lossy(&out.stdout).trim_end(),
                expected,
                "{cmd}"
            );
        }
    }

    use crate::cache::Cache;
    use crate::config::{Config, FeedConfig, UiConfig};
    use crate::feed::FeedMeta;
    use crate::testing::tempdir;
    use std::sync::mpsc;
    use td_ui::window::Refusal;

    /// A clipboard that records the copies it is asked, or refuses them.
    #[derive(Default)]
    pub(super) struct Board {
        pub(super) refuse: bool,
        pub(super) copies: Vec<Arc<str>>,
    }

    impl Clipboard for Board {
        fn available(&self) -> bool {
            true
        }
        fn has_text(&self) -> bool {
            false
        }
        fn pasting(&self) -> bool {
            false
        }
        fn copy(&mut self, text: Arc<str>) -> Result<(), Refusal> {
            if self.refuse {
                return Err(Refusal::NoSerial);
            }
            self.copies.push(text);
            Ok(())
        }
        fn paste(&mut self) -> Result<(), Refusal> {
            Err(Refusal::NoSelection)
        }
    }

    const FEED_URL: &str = "https://example.com/feed";
    const FEED_NAME: &str = "Example Feed";
    const ARTICLE_HASH: &str = "a1";

    /// The article body is rendered here and the reference definitions the
    /// renderer appends are stripped, because the Links section below the
    /// body lists the same URLs. That only works while the definitions look
    /// like `[n]: url`, which is a property of the renderer, so it is
    /// asserted against the renderer rather than assumed.
    #[test]
    fn the_renderer_marks_links_the_way_the_stripper_reads_them() {
        let html = concat!(
            r#"<p>Hello <a href="https://example.com/x">there</a>"#,
            r#" and <a href="https://example.com/y">here</a>.</p>"#
        );
        let text = crate::html::to_text(html.as_bytes(), 60);
        let definitions: Vec<&str> = text
            .lines()
            .filter(|line| is_reference_link_def(line))
            .collect();
        assert_eq!(
            definitions,
            ["[1]: https://example.com/x", "[2]: https://example.com/y"]
        );
        // What is left is the prose, with the link text in place.
        let body: Vec<&str> = text
            .lines()
            .filter(|line| !is_reference_link_def(line) && !line.is_empty())
            .collect();
        assert_eq!(body, ["Hello [there][1] and [here][2]."]);
        // Nothing else is mistaken for a definition.
        assert!(!is_reference_link_def("[a]: https://example.com/x"));
        assert!(!is_reference_link_def("[1] https://example.com/x"));
    }

    #[test]
    fn a_log_is_counted_and_shown_in_bounded_pieces() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("log");
        let count = |from: u64| count_newlines_from(&path, from).unwrap();
        let total = |from: u64, known: LogCount| {
            let (newlines, last, read) = count(from);
            LogCount {
                len: known.len + read,
                newlines: known.newlines + newlines,
                ends_with_newline: last.unwrap_or(known.ends_with_newline),
            }
        };
        for (content, expected) in [("", 0), ("a\n", 1), ("a\nb", 2), ("a\nb\n", 2), ("\n\n", 2)] {
            std::fs::write(&path, content).unwrap();
            assert_eq!(total(0, LogCount::NONE).total(), expected, "{content:?}");
            assert_eq!(content.lines().count(), expected, "{content:?}");
        }
        // A log that grew is counted onward from where the count stopped.
        std::fs::write(&path, "a\nb").unwrap();
        let known = total(0, LogCount::NONE);
        assert_eq!((known.len, known.total()), (3, 2));
        std::fs::write(&path, "a\nb\nc\n").unwrap();
        let grown = total(known.len, known);
        assert_eq!((grown.len, grown.total()), (6, 3));
        // A window: cut to the width, a carriage return dropped, bytes that
        // are not UTF-8 replaced, past the read size, and the last line
        // unterminated.
        let mut big = "x\n".repeat(100_000).into_bytes();
        big.extend_from_slice(b"wide\xffline\r\nlast");
        std::fs::write(&path, &big).unwrap();
        assert_eq!(total(0, LogCount::NONE).total(), 100_002);
        let window =
            |start, rows, width, from| log_window(&path, start, rows, width, from).unwrap();
        assert_eq!(
            window(99_999, 5, 3, (0, 0)),
            (
                vec!["x".to_string(), "wid".into(), "las".into()],
                (99_999, 199_998)
            )
        );
        // Resumed from a line already found, the same window.
        assert_eq!(
            window(100_000, 1, 80, (99_999, 199_998)),
            (vec!["wide\u{FFFD}line".to_string()], (100_000, 200_000))
        );
        assert_eq!(window(100_002, 5, 80, (0, 0)).0, Vec::<String>::new());
        // The resume point is used, not merely consistent with a scan from
        // the start; and with three-byte lines, 65,536 being one past a
        // multiple of three, window lines straddle the reads.
        std::fs::write(&path, "a\nb\nc\n").unwrap();
        assert_eq!(window(0, 3, 80, (0, 4)).0, ["c"]);
        std::fs::write(&path, "ab\n".repeat(60_000)).unwrap();
        let found = window(50_000, 3, 80, (0, 0));
        assert_eq!(found, (vec!["ab".to_string(); 3], (50_000, 150_000)));
        assert_eq!(window(50_001, 1, 80, found.1).1, (50_001, 150_003));
        // Lines that straddle the read size, terminated and not: a count
        // that closed each chunk's last line would be off by the chunks.
        let straddling = "ab\n".repeat(50_000);
        std::fs::write(&path, &straddling).unwrap();
        assert_eq!(total(0, LogCount::NONE).total(), straddling.lines().count());
        let unterminated = format!("{}tail", "y\n".repeat(50_000));
        std::fs::write(&path, &unterminated).unwrap();
        assert_eq!(total(0, LogCount::NONE).total(), 50_001);
        assert_eq!(window(50_000, 2, 80, (0, 0)).0, ["tail"]);
        // A file that cannot be read is not a count of zero.
        let missing = dir.path().join("missing");
        assert!(count_newlines_from(&missing, 0).is_none());
        assert!(log_window(&missing, 0, 1, 1, (0, 0)).is_none());
        // The kept count follows growth and starts over, dropping the
        // view's resume point, when the log shrank.
        let known = Cell::new(None);
        let hint = Cell::new(None);
        std::fs::write(&path, "a\nb\nc\n").unwrap();
        assert_eq!(count_log(&path, &known, &hint), Some(3));
        hint.set(Some((2, 4)));
        std::fs::write(&path, "a\nb\nc\nd").unwrap();
        assert_eq!(count_log(&path, &known, &hint), Some(4));
        assert_eq!(hint.get(), Some((2, 4)));
        std::fs::write(&path, "x\n").unwrap();
        assert_eq!(count_log(&path, &known, &hint), Some(1));
        assert_eq!(hint.get(), None);
        assert_eq!(count_log(&missing, &known, &hint), None);
        // A shrunk log whose recount fails keeps no count to resume from:
        // a directory has a length, and a read of it fails. Regrown past
        // the counted length, the log is counted from its start.
        let known = Cell::new(None);
        let hint = Cell::new(None);
        std::fs::write(&path, "ab\n".repeat(3_000)).unwrap();
        assert_eq!(count_log(&path, &known, &hint), Some(3_000));
        hint.set(Some((1_500, 4_500)));
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(std::fs::metadata(&path).unwrap().len() < 9_000);
        assert_eq!(count_log(&path, &known, &hint), None);
        assert_eq!(
            (known.get().map(|count| count.len), hint.get()),
            (None, None)
        );
        std::fs::remove_dir(&path).unwrap();
        std::fs::write(&path, "abcd\n".repeat(2_000)).unwrap();
        assert_eq!(count_log(&path, &known, &hint), Some(2_000));
        // Gone, then back and longer: counted from its start too.
        hint.set(Some((1_000, 5_000)));
        std::fs::remove_file(&path).unwrap();
        assert_eq!(count_log(&path, &known, &hint), None);
        assert_eq!(
            (known.get().map(|count| count.len), hint.get()),
            (None, None)
        );
        std::fs::write(&path, "abcde\n".repeat(2_000)).unwrap();
        assert_eq!(count_log(&path, &known, &hint), Some(2_000));
    }

    #[test]
    fn next_unread_scans_downward_first() {
        let visible = vec![0, 1, 2, 3, 4];
        let unread = [false, false, false, true, true];
        let next = find_next_unread_visible_index(&visible, 1, |article_idx| unread[article_idx]);
        assert_eq!(next, Some(3));
    }

    #[test]
    fn next_unread_falls_back_upward() {
        let visible = vec![0, 1, 2, 3, 4];
        let unread = [false, true, false, false, false];
        let next = find_next_unread_visible_index(&visible, 2, |article_idx| unread[article_idx]);
        assert_eq!(next, Some(1));
    }

    #[test]
    fn next_unread_returns_none_when_missing() {
        let visible = vec![0, 1, 2, 3, 4];
        let unread = [false, false, false, false, false];
        let next = find_next_unread_visible_index(&visible, 2, |article_idx| unread[article_idx]);
        assert_eq!(next, None);
    }

    #[test]
    fn pending_read_survives_refresh_reload_before_backend_ack() {
        let dir = tempdir().expect("tempdir");
        let cache = Cache::open_at(dir.path().join("test.tdkv")).expect("cache");
        seed_cache(&cache, false);
        let config = test_config();
        let (cmd_tx, _cmd_rx) = mpsc::channel();
        let mut app = App::new(&config, &cache, true).expect("app");
        app.selected_feed_scope = Some(FeedScope::Feed(FEED_URL.to_string()));
        app.reload_articles(&cache);

        app.toggle_current_read(&cache, &cmd_tx);
        assert!(app.articles[0].read);

        app.pending_user_fetches = 1;
        app.handle_backend(
            BackendResponse::RefreshCompleted {
                reports: vec![FeedRefreshReport {
                    feed_url: FEED_URL.to_string(),
                    feed_name: FEED_NAME.to_string(),
                    fetched_at: Some("2026-04-29 12:00:00".to_string()),
                    new_articles: 0,
                    total: 1,
                    unread: 1,
                    error: None,
                }],
            },
            &cache,
        );

        assert!(app.articles[0].read);
        assert_eq!(app.feeds[0].unread, 0);
        assert_eq!(
            app.pending_read_mutations.get(ARTICLE_HASH).copied(),
            Some(true)
        );
    }

    /// A press on a list row selects it, the action bar's labels stand
    /// for keys, a press below the last row is nothing, and with the
    /// mouse off in the configuration a press is nothing at all.
    #[test]
    fn a_press_selects_the_feed_row_under_it_and_a_bar_label_is_its_key() {
        let dir = tempdir().expect("tempdir");
        let cache = Cache::open_at(dir.path().join("test.tdkv")).expect("cache");
        seed_cache(&cache, false);
        let config = test_config();
        let (cmd_tx, _cmd_rx) = mpsc::channel();
        let mut app = App::new(&config, &cache, true).expect("app");
        app.selected_feed = 1;
        let layout = app.layout();
        let list = layout.list.expect("list");
        let press = |app: &mut App, x: i64, y: i64| {
            for phase in [PointerPhase::Press, PointerPhase::Release] {
                let input = Input::Pointer {
                    phase,
                    x,
                    y,
                    extend: false,
                };
                app.input(input, &cache, &cmd_tx);
            }
        };
        let row = |index: usize| list.row(index).expect("row");
        press(&mut app, 10, row(0).y + 3);
        assert_eq!(app.selected_feed, 0);
        let last = app.feed_row_count() - 1;
        press(&mut app, 10, row(last).y + 3);
        assert_eq!(app.selected_feed, last);
        press(&mut app, 10, row(last + 1).y + 3);
        assert_eq!(app.selected_feed, last, "below the last row");
        // The bar's "Help" is the `?` key, and the help's "Back" its `q`.
        let help = layout.bar.header(4).expect("help label");
        press(&mut app, help.x + 2, help.y + 2);
        assert_eq!(app.view, View::Help);
        let back = app.layout().bar.header(0).expect("back label");
        press(&mut app, back.x + 2, back.y + 2);
        assert_eq!(app.view, View::FeedList);
        app.mouse_config = false;
        press(&mut app, 10, row(0).y + 3);
        assert_eq!(app.selected_feed, last, "the mouse off");
    }

    /// A wheel frame moves a list's selection by its rows and the log's
    /// window likewise; the page keys move by what the layout shows.
    #[test]
    fn a_wheel_frame_moves_the_selection_and_the_log_window_by_its_rows() {
        let dir = tempdir().expect("tempdir");
        let cache = Cache::open_at(dir.path().join("test.tdkv")).expect("cache");
        seed_cache(&cache, false);
        let config = test_config();
        let (cmd_tx, _cmd_rx) = mpsc::channel();
        let mut app = App::new(&config, &cache, true).expect("app");
        let wheel = |app: &mut App, rows: isize| {
            app.input(Input::Wheel { rows, columns: 0 }, &cache, &cmd_tx);
        };
        let key = |app: &mut App, chord: &str| {
            let input = Input::Key {
                chord,
                repeat: false,
            };
            app.input(input, &cache, &cmd_tx);
        };
        wheel(&mut app, 1);
        assert_eq!(app.selected_feed, 1);
        wheel(&mut app, -3);
        assert_eq!(app.selected_feed, 0);
        key(&mut app, "End");
        assert_eq!(app.selected_feed, app.feed_row_count() - 1);
        app.view = View::Log;
        wheel(&mut app, 1);
        assert_eq!(app.log_scroll, 1);
        let page = app.pane_rows();
        let pane = app.layout().pane.expect("pane");
        assert_eq!(page, pane.height as usize / 16, "rows of sixteen pixels");
        assert!(page > 1, "{page}");
        key(&mut app, "PageDown");
        assert_eq!(app.log_scroll, 1 + page);
        wheel(&mut app, -1);
        assert_eq!(app.log_scroll, page);
        key(&mut app, "Home");
        assert_eq!(app.log_scroll, 0);
        // End follows the log's end across frames; a step up leaves it
        // from the window last shown.
        key(&mut app, "End");
        app.prepare_frame();
        assert_eq!(app.log_scroll, usize::MAX);
        let start = app.log_start;
        wheel(&mut app, -1);
        assert_eq!(
            app.log_scroll,
            start.saturating_sub(1),
            "from the window shown"
        );
    }

    /// What the pane is given is what it admits: a control scalar in a
    /// title, a CRLF body and a leading byte order mark load, shown with
    /// the replacement character and one newline; the read state is the
    /// status row's, so a toggle changes it without reloading the
    /// document.
    #[test]
    fn the_pane_admits_a_feeds_text_and_the_status_follows_the_read_state() {
        assert_eq!(
            pane_source("\u{feff}A\u{7f}B\r\nC\u{1}\tD\n"),
            "A\u{fffd}B\nC\u{fffd}\tD\n"
        );
        let dir = tempdir().expect("tempdir");
        let cache = Cache::open_at(dir.path().join("test.tdkv")).expect("cache");
        seed_cache(&cache, false);
        let config = test_config();
        let (cmd_tx, _cmd_rx) = mpsc::channel();
        let mut app = App::new(&config, &cache, true).expect("app");
        app.set_pane_text(PaneText::Help, "x\u{7f}y");
        let tab = app.pane_tab.expect("loaded");
        let document = app.pane.editor().document(tab).expect("document");
        assert_eq!(document.text(), "x\u{fffd}y");
        app.selected_feed_scope = Some(FeedScope::Feed(FEED_URL.to_string()));
        app.reload_articles(&cache);
        app.enter_article_view(&cmd_tx);
        let shown = |app: &mut App| {
            app.prepare_frame();
            td_ui::driven::text(&Frame { app }).expect("text").2
        };
        // The status row is the frame's last line, the state leading it.
        let status = |text: &str| text.lines().last().unwrap_or("").trim().to_string();
        let text = shown(&mut app);
        assert!(status(&text).starts_with("read | "), "{text}");
        let tab = app.pane_tab;
        app.input(
            Input::Key {
                chord: "u",
                repeat: false,
            },
            &cache,
            &cmd_tx,
        );
        let text = shown(&mut app);
        assert!(status(&text).starts_with("unread | "), "{text}");
        assert_eq!(app.pane_tab, tab, "the document is not reloaded");
        assert_eq!(app.pane.editor().tabs().count(), 1);
    }

    /// The frame reads back as text: the feed rows with their counts and
    /// the status, then the article list, then an opened article in the
    /// pane, read-only, with its links numbered as the picker numbers
    /// them; a chord the reader does not claim reaches the pane and edits
    /// nothing there.
    #[test]
    fn the_frame_shows_the_feeds_and_an_opened_article_in_the_pane() {
        let dir = tempdir().expect("tempdir");
        let cache = Cache::open_at(dir.path().join("test.tdkv")).expect("cache");
        seed_cache(&cache, false);
        let config = test_config();
        let (cmd_tx, _cmd_rx) = mpsc::channel();
        let mut app = App::new(&config, &cache, true).expect("app");
        let key = |app: &mut App, chord: &str| {
            let input = Input::Key {
                chord,
                repeat: false,
            };
            app.input(input, &cache, &cmd_tx);
        };
        let shown = |app: &mut App| {
            app.prepare_frame();
            td_ui::driven::text(&Frame { app }).expect("text").2
        };
        let text = shown(&mut app);
        for expected in [
            "[All]",
            "[Unread]",
            FEED_NAME,
            "1 unread of 1",
            "[Log]",
            "Offline mode",
        ] {
            assert!(text.contains(expected), "{expected}: {text}");
        }
        key(&mut app, "Down");
        key(&mut app, "Down");
        key(&mut app, "Return");
        assert_eq!(app.view, View::ArticleList);
        let text = shown(&mut app);
        assert!(text.contains("First"), "{text}");
        assert!(text.contains("Search"), "{text}");
        key(&mut app, "Return");
        assert_eq!(app.view, View::Article);
        let text = shown(&mut app);
        for expected in ["First", FEED_NAME, "content", "[1] https://example.com/1"] {
            assert!(text.contains(expected), "{expected}: {text}");
        }
        let tab = app.pane_tab.expect("the pane holds the article");
        let document = app.pane.editor().document(tab).expect("document");
        assert!(document.read_only());
        let revision = document.revision();
        key(&mut app, "Tab");
        key(&mut app, "Delete");
        let document = app.pane.editor().document(tab).expect("document");
        assert_eq!(document.revision(), revision, "the pane is not edited");
        assert_eq!(
            app.pane.editor().tabs().count(),
            1,
            "one document at a time"
        );
        // A copy with nothing selected is said; the whole article
        // selected and copied is the clipboard's, offered while the
        // chord is delivered, and what the clipboard answered is the
        // status.
        key(&mut app, "C-c");
        assert!(app.copy.is_none());
        assert!(shown(&mut app).contains("Nothing selected to copy"));
        key(&mut app, "C-a");
        key(&mut app, "C-c");
        let copied = app.copy.clone().expect("the selection");
        assert!(copied.contains("content"), "{copied}");
        assert!(copied.contains("[1] https://example.com/1"), "{copied}");
        let mut board = Board::default();
        app.serve_clipboard(&mut board);
        assert_eq!(board.copies, [copied]);
        assert!(app.copy.is_none());
        assert!(shown(&mut app).contains("Copied to the clipboard"));
        key(&mut app, "C-c");
        board.refuse = true;
        app.serve_clipboard(&mut board);
        assert_eq!(board.copies.len(), 1);
        let text = shown(&mut app);
        assert!(
            text.contains("Copy refused: the clipboard answers a key or button press only"),
            "{text}"
        );
        key(&mut app, "Down");
        assert!(
            !shown(&mut app).contains("Copy refused"),
            "the next key clears the note"
        );
        // No clipboard at all is said too: the reader has no kill ring to
        // fall back on.
        key(&mut app, "C-a");
        key(&mut app, "C-c");
        assert!(app.copy.is_some());
        app.serve_clipboard(&mut td_ui::window::NoClipboard);
        assert!(app.copy.is_none());
        assert!(
            shown(&mut app).contains("Copy refused: the compositor offers no clipboard"),
            "{}",
            shown(&mut app)
        );
        // Back to the list: the pane's article is closed with the view.
        key(&mut app, "q");
        assert_eq!(app.view, View::ArticleList);
        assert!(app.open_article.is_none());
        let text = shown(&mut app);
        assert!(text.contains(&format!("{FEED_NAME}: 1 articles")), "{text}");
        // A selection past the clipboard's ceiling is refused before the
        // clipboard is asked, with the reason: the help's pane, holding
        // a text that long for the chord.
        key(&mut app, "?");
        assert_eq!(app.view, View::Help);
        app.set_pane_text(
            PaneText::NoArticle,
            &"x".repeat(td_editor::clipboard::MAX_BYTES + 1),
        );
        key(&mut app, "C-a");
        key(&mut app, "C-c");
        assert!(app.copy.is_none());
        let text = shown(&mut app);
        assert!(
            text.contains("Copy refused: the selection is past the clipboard's 1024 KiB ceiling"),
            "{text}"
        );
        // A repeat of the chord, a held key, asks the clipboard nothing:
        // the clipboard takes a selection at a press only.
        key(&mut app, "q");
        key(&mut app, "Return");
        assert_eq!(app.view, View::Article);
        key(&mut app, "C-a");
        app.input(
            Input::Key {
                chord: "C-c",
                repeat: true,
            },
            &cache,
            &cmd_tx,
        );
        assert!(app.copy.is_none(), "a repeat asks nothing");
        key(&mut app, "C-c");
        assert!(app.copy.is_some(), "the press asks");
    }

    #[test]
    fn stale_article_mutation_does_not_override_newer_pending_toggle() {
        let dir = tempdir().expect("tempdir");
        let cache = Cache::open_at(dir.path().join("test.tdkv")).expect("cache");
        seed_cache(&cache, false);
        let config = test_config();
        let (cmd_tx, _cmd_rx) = mpsc::channel();
        let mut app = App::new(&config, &cache, true).expect("app");
        app.selected_feed_scope = Some(FeedScope::Feed(FEED_URL.to_string()));
        app.reload_articles(&cache);

        app.toggle_current_read(&cache, &cmd_tx);
        app.toggle_current_read(&cache, &cmd_tx);

        app.handle_backend(
            BackendResponse::ArticleMutation {
                hash: ARTICLE_HASH.to_string(),
                read: true,
            },
            &cache,
        );
        assert!(!app.articles[0].read);
        assert_eq!(
            app.pending_read_mutations.get(ARTICLE_HASH).copied(),
            Some(false)
        );

        app.handle_backend(
            BackendResponse::ArticleMutation {
                hash: ARTICLE_HASH.to_string(),
                read: false,
            },
            &cache,
        );
        assert!(!app.articles[0].read);
        assert!(!app.pending_read_mutations.contains_key(ARTICLE_HASH));
    }

    #[test]
    fn article_view_keeps_open_article_when_unread_reload_drops_it() {
        let dir = tempdir().expect("tempdir");
        let cache = Cache::open_at(dir.path().join("test.tdkv")).expect("cache");
        seed_two_unread_articles(&cache);
        let config = test_config();
        let (cmd_tx, _cmd_rx) = mpsc::channel();
        let mut app = App::new(&config, &cache, true).expect("app");
        app.selected_feed_scope = Some(FeedScope::Unread);
        app.reload_articles(&cache);
        assert!(app.select_visible_article_by_hash(ARTICLE_HASH));

        app.sync_open_article_to_selection();
        app.view = View::Article;
        app.mark_current_article_read(&cmd_tx);
        app.reload_articles(&cache);

        assert_eq!(
            app.current_article().map(|article| article.hash.as_str()),
            Some(ARTICLE_HASH)
        );
        assert_eq!(
            app.selected_list_article()
                .map(|article| article.hash.as_str()),
            Some("a2")
        );
    }

    pub(super) fn test_config() -> Config {
        Config {
            ui: UiConfig::default(),
            feeds: vec![FeedConfig {
                name: FEED_NAME.to_string(),
                url: FEED_URL.to_string(),
            }],
        }
    }

    pub(super) fn seed_cache(cache: &Cache, read: bool) {
        cache.put_articles(&[Article {
            hash: ARTICLE_HASH.to_string(),
            title: "First".to_string(),
            link: "https://example.com/1".to_string(),
            description: "desc".to_string(),
            content: "content".to_string(),
            published: Some("2026-04-29 10:00:00".to_string()),
            feed_name: FEED_NAME.to_string(),
            read,
        }]);
        cache.put_feed_index(FEED_URL, &[ARTICLE_HASH.to_string()]);
        cache.put_feed_meta(&FeedMeta {
            url: FEED_URL.to_string(),
            title: FEED_NAME.to_string(),
            last_fetched: "2026-04-29 11:00:00".to_string(),
        });
    }

    fn seed_two_unread_articles(cache: &Cache) {
        let second_hash = "a2".to_string();
        cache.put_articles(&[
            Article {
                hash: ARTICLE_HASH.to_string(),
                title: "First".to_string(),
                link: "https://example.com/1".to_string(),
                description: "desc".to_string(),
                content: "content".to_string(),
                published: Some("2026-04-29 10:00:00".to_string()),
                feed_name: FEED_NAME.to_string(),
                read: false,
            },
            Article {
                hash: second_hash.clone(),
                title: "Second".to_string(),
                link: "https://example.com/2".to_string(),
                description: "desc".to_string(),
                content: "content".to_string(),
                published: Some("2026-04-29 09:00:00".to_string()),
                feed_name: FEED_NAME.to_string(),
                read: false,
            },
        ]);
        cache.put_feed_index(FEED_URL, &[ARTICLE_HASH.to_string(), second_hash]);
        cache.put_feed_meta(&FeedMeta {
            url: FEED_URL.to_string(),
            title: FEED_NAME.to_string(),
            last_fetched: "2026-04-29 11:00:00".to_string(),
        });
    }
}

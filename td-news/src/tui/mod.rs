mod input;
mod screen;
pub mod views;

use std::cell::Cell;
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::os::fd::{AsFd, BorrowedFd};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use crate::backend::{BackendCommand, BackendResponse, FeedRefreshReport};
use crate::cache::Cache;
use crate::config::Config;
use crate::feed::{datetime_sort_key, normalize_datetime_to_local, Article};
use crate::keybindings;

use input::{InputEvent, Key, MouseEvent};
use screen::Terminal;

#[derive(Clone)]
struct FeedRow {
    name: String,
    url: String,
    total: usize,
    unread: usize,
    last_updated: Option<String>,
    last_error: Option<String>,
}

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

#[derive(Clone, Copy, Default)]
struct UiTheme {
    selection_bg: Option<(u8, u8, u8)>,
    selection_fg: Option<(u8, u8, u8)>,
    status_bg: Option<(u8, u8, u8)>,
    status_fg: Option<(u8, u8, u8)>,
    header_fg: Option<(u8, u8, u8)>,
    bold_fg: Option<(u8, u8, u8)>,
}

pub fn run(
    config: &Config,
    cache: &Cache,
    cmd_tx: &mpsc::Sender<BackendCommand>,
    resp_rx: &mpsc::Receiver<BackendResponse>,
    offline: bool,
) -> Result<(), String> {
    // The two handles outlive the screen, which borrows their descriptors:
    // the raw-mode guard restores the terminal from `Drop`, and a restore
    // issued on a descriptor that had been closed and reused would land on
    // whatever took its number.
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let outcome = run_screen(
        config,
        cache,
        cmd_tx,
        resp_rx,
        offline,
        stdin.as_fd(),
        stdout.as_fd(),
    );
    // After the screen is gone, so the message is not drawn over.
    if let Some(why) = screen::restore_failure() {
        eprintln!("td-news: {}", why);
    }
    outcome
}

#[allow(clippy::too_many_arguments)]
fn run_screen(
    config: &Config,
    cache: &Cache,
    cmd_tx: &mpsc::Sender<BackendCommand>,
    resp_rx: &mpsc::Receiver<BackendResponse>,
    offline: bool,
    tty: BorrowedFd<'_>,
    out: BorrowedFd<'_>,
) -> Result<(), String> {
    let mut terminal = Terminal::enter(tty, out, config.ui.mouse, &config.theme)?;
    let (input_tx, input_rx) = mpsc::channel::<InputEvent>();
    input::spawn_input_thread(input_tx);

    let mut app = App::new(config, cache, offline);

    loop {
        while let Ok(resp) = resp_rx.try_recv() {
            app.handle_backend(resp, cache);
        }

        app.draw(&terminal)?;

        match input_rx.recv_timeout(Duration::from_millis(120)) {
            Ok(input) => {
                if app.handle_input(input, cache, cmd_tx, &mut terminal) {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    Ok(())
}

struct App {
    view: View,
    feeds: Vec<FeedRow>,
    selected_feed: usize,
    feed_list_scroll: usize,
    selected_article: usize,
    article_list_scroll: usize,
    article_scroll: usize,
    selected_feed_scope: Option<FeedScope>,
    articles: Vec<Article>,
    open_article: Option<Article>,
    search: String,
    search_mode: bool,
    status: String,
    last_updated: Option<String>,
    last_size: (usize, usize),
    page_size: usize,
    scrolloff: usize,
    theme: UiTheme,
    log_tab: LogTab,
    log_scroll: usize,
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
    pending_user_fetches: usize,
    pending_read_mutations: HashMap<String, bool>,
}

impl UiTheme {
    fn from_config(theme: &crate::config::Theme) -> Self {
        Self {
            selection_bg: parse_color_opt(&theme.selection_bg),
            selection_fg: parse_color_opt(&theme.selection_fg),
            status_bg: parse_color_opt(&theme.status_bg),
            status_fg: parse_color_opt(&theme.status_fg),
            header_fg: parse_color_opt(&theme.header_fg),
            bold_fg: parse_color_opt(&theme.bold_fg),
        }
    }
}

impl App {
    fn new(config: &Config, cache: &Cache, offline: bool) -> Self {
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

        Self {
            view: View::FeedList,
            feeds,
            selected_feed: 0,
            feed_list_scroll: 0,
            selected_article: 0,
            article_list_scroll: 0,
            article_scroll: 0,
            selected_feed_scope: None,
            articles: Vec::new(),
            open_article: None,
            search: String::new(),
            search_mode: false,
            status: if offline {
                "Offline mode: browsing cache".to_string()
            } else {
                "Ready".to_string()
            },
            last_updated,
            last_size: (80, 24),
            page_size: config.ui.page_size.max(1),
            scrolloff: config.ui.scrolloff,
            theme: UiTheme::from_config(&config.theme),
            log_tab: LogTab::News,
            log_scroll: 0,
            log_count: [Cell::new(None), Cell::new(None)],
            log_window_hint: [Cell::new(None), Cell::new(None)],
            quitting: false,
            pending_redraw: true,
            mouse_config: config.ui.mouse,
            browser: config.ui.browser.clone(),
            article_urls: Vec::new(),
            url_picking: false,
            url_cursor: 0,
            pending_user_fetches: 0,
            pending_read_mutations: HashMap::new(),
        }
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

    fn handle_input(
        &mut self,
        input: InputEvent,
        cache: &Cache,
        cmd_tx: &mpsc::Sender<BackendCommand>,
        terminal: &mut Terminal<'_>,
    ) -> bool {
        if self.quitting {
            return true;
        }

        if let InputEvent::Key(Key::Char('?')) = input {
            if !matches!(self.view, View::Help) {
                self.view = View::Help;
                self.pending_redraw = true;
                return false;
            }
        }

        if matches!(self.view, View::Help) {
            if let InputEvent::Key(Key::Char('q')) = input {
                self.view = if self.selected_feed_scope.is_some() {
                    View::ArticleList
                } else {
                    View::FeedList
                };
                self.pending_redraw = true;
            }
            return false;
        }

        if self.search_mode {
            if let InputEvent::Key(key) = input {
                match key {
                    Key::Enter => {
                        self.search_mode = false;
                        self.selected_article = 0;
                        self.article_list_scroll = 0;
                        self.pending_redraw = true;
                    }
                    Key::Backspace => {
                        self.search.pop();
                        self.selected_article = 0;
                        self.article_list_scroll = 0;
                        self.pending_redraw = true;
                    }
                    Key::Char(c) if !c.is_control() => {
                        self.search.push(c);
                        self.selected_article = 0;
                        self.article_list_scroll = 0;
                        self.pending_redraw = true;
                    }
                    _ => {}
                }
            }
            return false;
        }

        match self.view {
            View::FeedList => self.handle_feed_keys(input, cache, cmd_tx),
            View::ArticleList => self.handle_article_list_keys(input, cache, cmd_tx, terminal),
            View::Article => self.handle_article_view_keys(input, cache, cmd_tx, terminal),
            View::Log => self.handle_log_keys(input),
            View::Help => {}
        }

        self.quitting
    }

    fn handle_feed_keys(
        &mut self,
        input: InputEvent,
        cache: &Cache,
        cmd_tx: &mpsc::Sender<BackendCommand>,
    ) {
        match input {
            InputEvent::Key(Key::Char('q')) => {
                self.quitting = true;
            }
            InputEvent::Key(Key::Down)
            | InputEvent::Key(Key::Char('j'))
            | InputEvent::Key(Key::Char('n')) => {
                if self.selected_feed + 1 < self.feed_row_count() {
                    self.selected_feed += 1;
                    self.ensure_selected_feed_visible();
                    self.pending_redraw = true;
                }
            }
            InputEvent::Key(Key::Up)
            | InputEvent::Key(Key::Char('k'))
            | InputEvent::Key(Key::Char('p')) => {
                if self.selected_feed > 0 {
                    self.selected_feed -= 1;
                    self.ensure_selected_feed_visible();
                    self.pending_redraw = true;
                }
            }
            InputEvent::Mouse(MouseEvent::LeftClick { row }) => {
                if row >= 2 {
                    let start = self.feed_list_start();
                    let idx = start + (row - 2);
                    if idx < self.feed_row_count() {
                        self.selected_feed = idx;
                        self.ensure_selected_feed_visible();
                        self.pending_redraw = true;
                    }
                }
            }
            InputEvent::Key(Key::Enter) => {
                if self.selected_feed == self.log_row_index() {
                    self.view = View::Log;
                    self.log_scroll = 0;
                } else {
                    self.selected_feed_scope = Some(self.scope_for_selected_feed());
                    self.selected_article = 0;
                    self.article_list_scroll = 0;
                    self.article_scroll = 0;
                    self.search.clear();
                    self.reload_articles(cache);
                    self.view = View::ArticleList;
                }
                self.pending_redraw = true;
            }
            InputEvent::Key(Key::Char('g')) => {
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
            InputEvent::Key(Key::Char('G')) => {
                self.reload_feeds_from_cache(cache);
                self.pending_user_fetches += 1;
                let _ = cmd_tx.send(BackendCommand::FetchAllFeeds);
                self.status = "Refreshing feeds...".to_string();
                self.pending_redraw = true;
            }
            InputEvent::Key(Key::Char('u')) if self.selected_feed != self.log_row_index() => {
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

    fn handle_article_list_keys(
        &mut self,
        input: InputEvent,
        cache: &Cache,
        cmd_tx: &mpsc::Sender<BackendCommand>,
        terminal: &mut Terminal<'_>,
    ) {
        match input {
            InputEvent::Key(Key::Char('q')) => {
                self.view = View::FeedList;
                self.selected_feed_scope = None;
                self.pending_redraw = true;
            }
            InputEvent::Key(Key::Down)
            | InputEvent::Key(Key::Char('j'))
            | InputEvent::Key(Key::Char('n')) => {
                let visible = self.filtered_article_indices();
                if self.selected_article + 1 < visible.len() {
                    self.selected_article += 1;
                    self.ensure_selected_article_visible(visible.len());
                    self.pending_redraw = true;
                }
            }
            InputEvent::Key(Key::Up)
            | InputEvent::Key(Key::Char('k'))
            | InputEvent::Key(Key::Char('p')) => {
                if self.selected_article > 0 {
                    self.selected_article -= 1;
                    self.ensure_selected_article_visible(self.filtered_article_indices().len());
                    self.pending_redraw = true;
                }
            }
            InputEvent::Mouse(MouseEvent::LeftClick { row }) => {
                if row >= 2 {
                    let visible = self.filtered_article_indices();
                    let start = self.article_list_start(visible.len());
                    let idx = start + (row - 2);
                    if idx < visible.len() {
                        self.selected_article = idx;
                        self.ensure_selected_article_visible(visible.len());
                        self.article_scroll = 0;
                        self.enter_article_view(cmd_tx, terminal);
                        self.pending_redraw = true;
                    }
                }
            }
            InputEvent::Mouse(MouseEvent::ScrollDown) => {
                let visible = self.filtered_article_indices();
                if self.selected_article + 1 < visible.len() {
                    self.selected_article += 1;
                    self.ensure_selected_article_visible(visible.len());
                    self.pending_redraw = true;
                }
            }
            InputEvent::Mouse(MouseEvent::ScrollUp) => {
                if self.selected_article > 0 {
                    self.selected_article -= 1;
                    self.ensure_selected_article_visible(self.filtered_article_indices().len());
                    self.pending_redraw = true;
                }
            }
            InputEvent::Key(Key::Enter) => {
                if !self.filtered_article_indices().is_empty() {
                    self.article_scroll = 0;
                    self.enter_article_view(cmd_tx, terminal);
                    self.pending_redraw = true;
                }
            }
            InputEvent::Key(Key::PageDown) => {
                let visible = self.filtered_article_indices();
                if !visible.is_empty() {
                    let step = self.article_list_rows().max(1);
                    self.selected_article = (self.selected_article + step).min(visible.len() - 1);
                    self.ensure_selected_article_visible(visible.len());
                    self.pending_redraw = true;
                }
            }
            InputEvent::Key(Key::PageUp) => {
                let visible = self.filtered_article_indices();
                if !visible.is_empty() {
                    let step = self.article_list_rows().max(1);
                    self.selected_article = self.selected_article.saturating_sub(step);
                    self.ensure_selected_article_visible(visible.len());
                    self.pending_redraw = true;
                }
            }
            InputEvent::Key(Key::Home) => {
                if !self.filtered_article_indices().is_empty() {
                    self.selected_article = 0;
                    self.article_list_scroll = 0;
                    self.pending_redraw = true;
                }
            }
            InputEvent::Key(Key::End) => {
                let visible = self.filtered_article_indices();
                if !visible.is_empty() {
                    self.selected_article = visible.len() - 1;
                    self.ensure_selected_article_visible(visible.len());
                    self.pending_redraw = true;
                }
            }
            InputEvent::Key(Key::Char('/')) => {
                self.search_mode = true;
                self.pending_redraw = true;
            }
            InputEvent::Key(Key::Char('g')) => {
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
            InputEvent::Key(Key::Char('G')) => {
                self.reload_feeds_from_cache(cache);
                self.reload_articles(cache);
                self.pending_user_fetches += 1;
                let _ = cmd_tx.send(BackendCommand::FetchAllFeeds);
                self.status = "Refreshing feeds...".to_string();
                self.pending_redraw = true;
            }
            InputEvent::Key(Key::Char('u')) => {
                // Mark as read and advance to next article; if already read, toggle unread
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
                            self.ensure_selected_article_visible(visible_after.len());
                            self.pending_redraw = true;
                        }
                    } else {
                        let visible_after = self.filtered_article_indices();
                        self.ensure_selected_article_visible(visible_after.len());
                        self.pending_redraw = true;
                    }
                }
            }
            InputEvent::Key(Key::Char('H')) => {
                self.open_html_digest();
            }
            InputEvent::Key(Key::Char('o')) => self.open_current_article(),
            _ => {}
        }
    }

    fn handle_article_view_keys(
        &mut self,
        input: InputEvent,
        cache: &Cache,
        cmd_tx: &mpsc::Sender<BackendCommand>,
        terminal: &mut Terminal<'_>,
    ) {
        // URL picker mode
        if self.url_picking {
            match input {
                InputEvent::Key(Key::Char('q')) => {
                    self.url_picking = false;
                    self.pending_redraw = true;
                }
                InputEvent::Key(Key::Down) | InputEvent::Key(Key::Char('j')) => {
                    if self.url_cursor + 1 < self.article_urls.len() {
                        self.url_cursor += 1;
                        self.pending_redraw = true;
                    }
                }
                InputEvent::Key(Key::Up) | InputEvent::Key(Key::Char('k')) => {
                    if self.url_cursor > 0 {
                        self.url_cursor -= 1;
                        self.pending_redraw = true;
                    }
                }
                InputEvent::Key(Key::Enter) => {
                    if let Some(url) = self.article_urls.get(self.url_cursor).cloned() {
                        self.status = match open_in_browser(&url, self.browser.as_deref()) {
                            Ok(()) => format!("Opened [{}]", self.url_cursor + 1),
                            Err(e) => format!("Failed to open browser: {}", e),
                        };
                    }
                    self.url_picking = false;
                    self.pending_redraw = true;
                }
                InputEvent::Key(Key::Char(c)) if c.is_ascii_digit() && c != '0' => {
                    let idx = (c as usize) - ('1' as usize);
                    if let Some(url) = self.article_urls.get(idx).cloned() {
                        self.status = match open_in_browser(&url, self.browser.as_deref()) {
                            Ok(()) => format!("Opened [{}]", idx + 1),
                            Err(e) => format!("Failed to open browser: {}", e),
                        };
                    }
                    self.url_picking = false;
                    self.pending_redraw = true;
                }
                _ => {}
            }
            return;
        }

        match input {
            InputEvent::Key(Key::Char('q')) => {
                self.leave_article_view(terminal);
                self.pending_redraw = true;
            }
            InputEvent::Key(Key::Down) | InputEvent::Key(Key::Char('j')) => {
                self.article_scroll = self.article_scroll.saturating_add(1);
                self.pending_redraw = true;
            }
            InputEvent::Key(Key::Up) | InputEvent::Key(Key::Char('k')) => {
                self.article_scroll = self.article_scroll.saturating_sub(1);
                self.pending_redraw = true;
            }
            InputEvent::Mouse(MouseEvent::ScrollDown)
            | InputEvent::Key(Key::PageDown)
            | InputEvent::Key(Key::Char(' ')) => {
                self.article_scroll = self.article_scroll.saturating_add(15);
                self.pending_redraw = true;
            }
            InputEvent::Mouse(MouseEvent::ScrollUp) | InputEvent::Key(Key::PageUp) => {
                self.article_scroll = self.article_scroll.saturating_sub(15);
                self.pending_redraw = true;
            }
            InputEvent::Key(Key::Char('n')) => {
                let visible = self.filtered_article_indices();
                if self.selected_article + 1 < visible.len() {
                    self.selected_article += 1;
                    self.article_scroll = 0;
                    self.sync_open_article_to_selection();
                    self.mark_current_article_read(cmd_tx);
                    self.extract_current_article_urls();
                    self.pending_redraw = true;
                }
            }
            InputEvent::Key(Key::Char('p')) => {
                if self.selected_article > 0 {
                    self.selected_article -= 1;
                    self.article_scroll = 0;
                    self.sync_open_article_to_selection();
                    self.mark_current_article_read(cmd_tx);
                    self.extract_current_article_urls();
                    self.pending_redraw = true;
                }
            }
            InputEvent::Key(Key::Char('u')) => self.toggle_current_read(cache, cmd_tx),
            InputEvent::Key(Key::Char('o')) => self.open_current_article(),
            InputEvent::Key(Key::Char('b')) => {
                if self.article_urls.is_empty() {
                    self.status = "No URLs in article".to_string();
                } else if let [url] = self.article_urls.as_slice() {
                    let url = url.clone();
                    self.status = match open_in_browser(&url, self.browser.as_deref()) {
                        Ok(()) => "Opened [1]".to_string(),
                        Err(e) => format!("Failed to open browser: {}", e),
                    };
                } else {
                    self.url_picking = true;
                    self.url_cursor = 0;
                }
                self.pending_redraw = true;
            }
            InputEvent::Key(Key::Char(c)) if c.is_ascii_digit() && c != '0' => {
                let idx = (c as usize) - ('1' as usize);
                if let Some(url) = self.article_urls.get(idx).cloned() {
                    self.status = match open_in_browser(&url, self.browser.as_deref()) {
                        Ok(()) => format!("Opened [{}]", idx + 1),
                        Err(e) => format!("Failed to open browser: {}", e),
                    };
                    self.pending_redraw = true;
                }
            }
            _ => {}
        }
    }

    fn handle_log_keys(&mut self, input: InputEvent) {
        match input {
            InputEvent::Key(Key::Char('q')) => {
                self.view = View::FeedList;
                self.pending_redraw = true;
            }
            InputEvent::Key(Key::Down) | InputEvent::Key(Key::Char('j')) => {
                self.log_scroll = self.log_scroll.saturating_add(1);
                self.pending_redraw = true;
            }
            InputEvent::Key(Key::Up) | InputEvent::Key(Key::Char('k')) => {
                self.log_scroll = self.log_scroll.saturating_sub(1);
                self.pending_redraw = true;
            }
            InputEvent::Mouse(MouseEvent::ScrollDown) | InputEvent::Key(Key::PageDown) => {
                self.log_scroll = self.log_scroll.saturating_add(15);
                self.pending_redraw = true;
            }
            InputEvent::Mouse(MouseEvent::ScrollUp) | InputEvent::Key(Key::PageUp) => {
                self.log_scroll = self.log_scroll.saturating_sub(15);
                self.pending_redraw = true;
            }
            InputEvent::Key(Key::Home) => {
                self.log_scroll = 0;
                self.pending_redraw = true;
            }
            InputEvent::Key(Key::End) => {
                self.log_scroll = usize::MAX;
                self.pending_redraw = true;
            }
            InputEvent::Key(Key::Char('n')) => {
                self.log_tab = LogTab::News;
                self.log_scroll = 0;
                self.pending_redraw = true;
            }
            InputEvent::Key(Key::Char('d')) => {
                self.log_tab = LogTab::Debug;
                self.log_scroll = 0;
                self.pending_redraw = true;
            }
            _ => {}
        }
    }

    fn enter_article_view(
        &mut self,
        cmd_tx: &mpsc::Sender<BackendCommand>,
        terminal: &mut Terminal<'_>,
    ) {
        self.sync_open_article_to_selection();
        self.view = View::Article;
        self.mark_current_article_read(cmd_tx);
        self.extract_current_article_urls();
        self.url_picking = false;
        if self.mouse_config {
            terminal.set_mouse(false);
        }
    }

    fn leave_article_view(&mut self, terminal: &mut Terminal<'_>) {
        self.view = View::ArticleList;
        self.open_article = None;
        self.article_urls.clear();
        self.url_picking = false;
        if self.mouse_config {
            terminal.set_mouse(true);
        }
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

    fn draw(&mut self, terminal: &Terminal<'_>) -> Result<(), String> {
        if !self.pending_redraw {
            return Ok(());
        }

        let (width, height) = terminal.size();
        self.last_size = (width, height);
        let mut lines = Vec::with_capacity(height);
        lines.push(self.style_header(views::truncate(
            &format!(
                "Timmy's News Console - Last Updated {}",
                self.last_updated.as_deref().unwrap_or("never")
            ),
            width,
        )));
        let content_height = height.saturating_sub(1);
        let mut content_lines = Vec::with_capacity(content_height);
        self.ensure_selected_feed_visible();
        self.ensure_selected_article_visible(self.filtered_article_indices().len());

        match self.view {
            View::FeedList => self.render_feed_list(width, content_height, &mut content_lines),
            View::ArticleList => {
                self.render_article_list(width, content_height, &mut content_lines)
            }
            View::Article => self.render_article_view(width, content_height, &mut content_lines),
            View::Log => self.render_log_view(width, content_height, &mut content_lines),
            View::Help => self.render_help(width, content_height, &mut content_lines),
        }
        lines.extend(content_lines);

        terminal.draw(&lines)?;
        self.pending_redraw = false;
        Ok(())
    }

    fn render_feed_list(&self, width: usize, height: usize, lines: &mut Vec<String>) {
        lines.push(self.style_header(views::truncate(
            "Feeds (Enter open feed/log, g refresh feed, G refresh all, u mark read, ? help, q quit)",
            width,
        )));

        let list_rows = height.saturating_sub(2);
        let start = self.feed_list_start();
        for idx in start..self.feed_row_count().min(start + list_rows) {
            let (name, updated, total, unread, has_error) = if idx == 0 {
                (
                    "[All]".to_string(),
                    self.last_updated.clone().unwrap_or_else(|| "-".to_string()),
                    self.total_articles().to_string(),
                    self.total_unread(),
                    false,
                )
            } else if idx == 1 {
                (
                    "[Unread]".to_string(),
                    self.last_updated.clone().unwrap_or_else(|| "-".to_string()),
                    self.total_unread().to_string(),
                    self.total_unread(),
                    false,
                )
            } else if idx == self.log_row_index() {
                (
                    "[Log]".to_string(),
                    "-".to_string(),
                    self.current_log_entry_count()
                        .map_or_else(|| "?".to_string(), |count| count.to_string()),
                    0,
                    false,
                )
            } else {
                // Rows 2.. are the feeds, one each, in order.
                let Some(feed) = idx.checked_sub(2).and_then(|i| self.feeds.get(i)) else {
                    continue;
                };
                (
                    feed.name.clone(),
                    feed.last_updated.clone().unwrap_or_else(|| "-".to_string()),
                    feed.total.to_string(),
                    feed.unread,
                    feed.last_error.is_some(),
                )
            };
            let marker = if idx == self.selected_feed { ">" } else { " " };
            let err = if has_error { " !" } else { "" };
            let line = format!(
                "{} {:<22} {:<19} {:>5} total {:>5} unread{}",
                marker, name, updated, total, unread, err
            );
            let line = views::truncate(&line, width);
            if idx == self.selected_feed {
                lines.push(self.style_selection(line, false));
            } else {
                lines.push(line);
            }
        }

        while lines.len() + 1 < height {
            lines.push(String::new());
        }
        lines.push(self.style_status(views::truncate(&self.status, width)));
    }

    fn render_article_list(&self, width: usize, height: usize, lines: &mut Vec<String>) {
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
        let mut header = format!(
            "{} (Enter open, / search, g refresh, G all refresh, u toggle read, o open, q back)",
            feed_name
        );
        if !self.search.is_empty() {
            header.push_str(&format!(" [search: {}]", self.search));
        }
        if self.search_mode {
            header.push_str(" (typing search)");
        }
        lines.push(self.style_header(views::truncate(&header, width)));

        let visible = self.filtered_article_indices();
        let show_feed_source = matches!(
            self.selected_feed_scope.as_ref(),
            Some(FeedScope::All | FeedScope::Unread)
        );
        let list_rows = height.saturating_sub(2);
        let start = self.article_list_start(visible.len());
        for (list_idx, article_idx) in visible
            .iter()
            .copied()
            .enumerate()
            .skip(start)
            .take(list_rows)
        {
            let Some(article) = self.articles.get(article_idx) else {
                continue;
            };
            let marker = if list_idx == self.selected_article {
                ">"
            } else {
                " "
            };
            let unread = if article.read { " " } else { "*" };
            let published = article
                .published
                .as_deref()
                .and_then(normalize_datetime_to_local)
                .unwrap_or_else(|| "No date".to_string());
            let title = views::strip_newlines(&article.title);
            let line = if show_feed_source {
                format!(
                    "{}{} [{}] [{}] {}",
                    marker,
                    unread,
                    published,
                    views::strip_newlines(&article.feed_name),
                    title
                )
            } else {
                format!("{}{} [{}] {}", marker, unread, published, title)
            };
            let line = views::truncate(&line, width);
            if list_idx == self.selected_article {
                lines.push(self.style_selection(line, !article.read));
            } else if !article.read {
                lines.push(self.style_unread(line));
            } else {
                lines.push(line);
            }
        }

        while lines.len() + 1 < height {
            lines.push(String::new());
        }
        lines.push(if visible.is_empty() {
            self.style_status(views::truncate("No matching articles", width))
        } else {
            self.style_status(views::truncate(&self.status, width))
        });
    }

    fn render_article_view(&self, width: usize, height: usize, lines: &mut Vec<String>) {
        let Some(article) = self.current_article() else {
            lines.push(views::truncate("No article selected", width));
            return;
        };

        let header = format!(
            "{} [{}] (q back, j/k scroll, n/p nav, b urls, o open, u toggle)",
            views::strip_newlines(&article.title),
            if article.read { "read" } else { "unread" }
        );
        lines.push(self.style_header(views::truncate(&header, width)));

        let html = if article.content.trim().is_empty() {
            article.description.as_str()
        } else {
            article.content.as_str()
        };
        let text = crate::html::to_text(html.as_bytes(), width.max(20));
        // Strip reference link definitions the renderer emits (e.g. "[1]: https://...")
        // since we append our own Links section with extracted URLs.
        let mut rendered: Vec<String> = text
            .lines()
            .filter(|line| !is_reference_link_def(line))
            .map(|line| views::truncate(line, width))
            .collect();
        // Trim trailing blank lines left after stripping reference defs
        while rendered.last().is_some_and(|l| l.is_empty()) {
            rendered.pop();
        }

        // Append Links section
        if !self.article_urls.is_empty() {
            rendered.push(String::new());
            rendered.push("Links:".to_string());
            for (i, url) in self.article_urls.iter().enumerate() {
                rendered.push(views::truncate(&format!("  [{}] {}", i + 1, url), width));
            }
        }

        let body_rows = height.saturating_sub(2);
        let start = self.article_scroll.min(rendered.len());
        for line in rendered.iter().skip(start).take(body_rows) {
            lines.push(line.clone());
        }
        while lines.len() + 1 < height {
            lines.push(String::new());
        }

        if self.url_picking {
            // Show URL picker in status bar
            let picker = format!(
                "URL [{}]: {} (j/k move, Enter open, 1-9 jump, q cancel)",
                self.url_cursor + 1,
                self.article_urls
                    .get(self.url_cursor)
                    .map(|s| s.as_str())
                    .unwrap_or("")
            );
            lines.push(self.style_status(views::truncate(&picker, width)));
        } else {
            let footer = match article.published.as_ref() {
                Some(published) => format!(
                    "{} | {}",
                    article.link,
                    normalize_datetime_to_local(published).unwrap_or_else(|| "No date".to_string())
                ),
                None => article.link.clone(),
            };
            lines.push(self.style_status(views::truncate(&footer, width)));
        }
    }

    fn render_help(&self, width: usize, height: usize, lines: &mut Vec<String>) {
        lines.push(self.style_header(views::truncate("Help (q to close)", width)));
        for item in keybindings::GLOBAL {
            lines.push(views::truncate(&format!("Global: {}", item), width));
        }
        for item in keybindings::FEED_LIST {
            lines.push(views::truncate(&format!("Feed list: {}", item), width));
        }
        for item in keybindings::ARTICLE_LIST {
            lines.push(views::truncate(&format!("Article list: {}", item), width));
        }
        for item in keybindings::ARTICLE_VIEW {
            lines.push(views::truncate(&format!("Article view: {}", item), width));
        }
        for item in keybindings::LOG_VIEW {
            lines.push(views::truncate(&format!("Log view: {}", item), width));
        }
        lines.push(views::truncate(
            "Mouse: feed click selects, article click opens, wheel scrolls lists/view",
            width,
        ));

        while lines.len() + 1 < height {
            lines.push(String::new());
        }
        lines.push(self.style_status(views::truncate(&self.status, width)));
    }

    /// The log view shows one window of the file, read line by line, so a
    /// log of any size costs a frame one window's worth of memory: the
    /// whole file in one string was the allocation that ended a session.
    fn render_log_view(&self, width: usize, height: usize, lines: &mut Vec<String>) {
        let label = match self.log_tab {
            LogTab::News => "[News Log]",
            LogTab::Debug => "[Debug Log]",
        };
        let path = self.log_path();
        lines.push(self.style_header(views::truncate(
            &format!("{} (n news, d debug, j/k scroll, PgUp/PgDn, q back)", label),
            width,
        )));

        let total = self.current_log_entry_count();
        let body_rows = height.saturating_sub(2);
        let start = self
            .log_scroll
            .min(total.unwrap_or(0).saturating_sub(body_rows.max(1)));
        // Resume from the last frame's first line when it is not past this
        // one's; the count drops the hint of a log that shrank.
        let hint = self.log_window_hint.get(log_tab_index(self.log_tab));
        let from = hint
            .and_then(Cell::get)
            .filter(|&(index, _)| index <= start)
            .unwrap_or((0, 0));
        let window = match total {
            // No rows for a body: nothing read, and nothing said of it.
            _ if body_rows == 0 => Some(Vec::new()),
            Some(0) => Some(vec!["Log is empty".to_string()]),
            Some(_) => log_window(&path, start, body_rows, width, from).map(|(window, first)| {
                if let Some(hint) = hint {
                    hint.set(Some(first));
                }
                window
            }),
            None => None,
        };
        // A count kept from a frame that could read the log says nothing
        // for one that cannot.
        let total = if window.is_none() { None } else { total };
        match window {
            Some(window) => lines.extend(window),
            None => lines.push(views::truncate(
                &format!("Could not read {}", path.display()),
                width,
            )),
        }
        while lines.len() + 1 < height {
            lines.push(String::new());
        }
        lines.push(self.style_status(views::truncate(
            &format!(
                "{} lines | {}",
                total.map_or_else(|| "?".to_string(), |total| total.to_string()),
                path.display()
            ),
            width,
        )));
    }

    fn style_header(&self, s: String) -> String {
        style_line(&s, self.theme.header_fg, None, true, false)
    }

    fn style_status(&self, s: String) -> String {
        style_line(&s, self.theme.status_fg, self.theme.status_bg, false, true)
    }

    fn style_selection(&self, s: String, bold: bool) -> String {
        style_line(
            &s,
            self.theme.selection_fg,
            self.theme.selection_bg,
            bold,
            true,
        )
    }

    fn style_unread(&self, s: String) -> String {
        style_line(&s, self.theme.bold_fg, None, true, false)
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
        self.ensure_selected_article_visible(visible_len);
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

    fn feed_list_rows(&self) -> usize {
        self.last_size.1.saturating_sub(3).min(self.page_size)
    }

    fn article_list_rows(&self) -> usize {
        self.last_size.1.saturating_sub(3).min(self.page_size)
    }

    fn feed_list_start(&self) -> usize {
        let list_rows = self.feed_list_rows().max(1);
        self.feed_list_scroll
            .min(self.feed_row_count().saturating_sub(list_rows))
    }

    fn article_list_start(&self, visible_len: usize) -> usize {
        let list_rows = self.article_list_rows().max(1);
        self.article_list_scroll
            .min(visible_len.saturating_sub(list_rows))
    }

    fn ensure_selected_feed_visible(&mut self) {
        let total_rows = self.feed_row_count();
        if total_rows == 0 {
            self.selected_feed = 0;
            self.feed_list_scroll = 0;
            return;
        }
        self.selected_feed = self.selected_feed.min(total_rows - 1);
        let list_rows = self.feed_list_rows().max(1);
        let scrolloff = self.scrolloff.min(list_rows.saturating_sub(1));
        self.feed_list_scroll = self
            .feed_list_scroll
            .min(total_rows.saturating_sub(list_rows));
        if self.selected_feed < self.feed_list_scroll.saturating_add(scrolloff) {
            self.feed_list_scroll = self.selected_feed.saturating_sub(scrolloff);
        } else if self.selected_feed.saturating_add(scrolloff)
            >= self.feed_list_scroll.saturating_add(list_rows)
        {
            self.feed_list_scroll = self
                .selected_feed
                .saturating_add(scrolloff)
                .saturating_add(1)
                .saturating_sub(list_rows);
        }
        self.feed_list_scroll = self
            .feed_list_scroll
            .min(total_rows.saturating_sub(list_rows));
    }

    fn ensure_selected_article_visible(&mut self, visible_len: usize) {
        if visible_len == 0 {
            self.selected_article = 0;
            self.article_list_scroll = 0;
            return;
        }
        self.selected_article = self.selected_article.min(visible_len - 1);
        let list_rows = self.article_list_rows().max(1);
        let scrolloff = self.scrolloff.min(list_rows.saturating_sub(1));
        self.article_list_scroll = self
            .article_list_scroll
            .min(visible_len.saturating_sub(list_rows));
        if self.selected_article < self.article_list_scroll.saturating_add(scrolloff) {
            self.article_list_scroll = self.selected_article.saturating_sub(scrolloff);
        } else if self.selected_article.saturating_add(scrolloff)
            >= self.article_list_scroll.saturating_add(list_rows)
        {
            self.article_list_scroll = self
                .selected_article
                .saturating_add(scrolloff)
                .saturating_add(1)
                .saturating_sub(list_rows);
        }
        self.article_list_scroll = self
            .article_list_scroll
            .min(visible_len.saturating_sub(list_rows));
    }
}

fn open_in_browser(url: &str, browser_config: Option<&str>) -> Result<(), String> {
    // 1. Config browser command (supports {url} template, executed via sh -c)
    if let Some(cmd) = browser_config {
        let shell_cmd = if cmd.contains("{url}") {
            cmd.replace("{url}", url)
        } else {
            format!("{} {}", cmd, shell_quote(url))
        };
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(&shell_cmd)
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

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
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
        if let Some(end) = html[url_start..].find('"') {
            let url = &html[url_start..url_start + end];
            if url.starts_with("http://") || url.starts_with("https://") {
                urls.push(url.to_string());
            }
        }
        pos = url_start;
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

fn parse_color_opt(v: &Option<String>) -> Option<(u8, u8, u8)> {
    v.as_deref()
        .and_then(|hex| crate::config::Theme::parse_color(hex).ok())
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

fn style_line(
    text: &str,
    fg: Option<(u8, u8, u8)>,
    bg: Option<(u8, u8, u8)>,
    bold: bool,
    reverse_fallback: bool,
) -> String {
    let mut seq = String::new();
    let has_colors = fg.is_some() || bg.is_some();
    if bold {
        seq.push_str("\x1b[1m");
    }
    if let Some((r, g, b)) = fg {
        seq.push_str(&format!("\x1b[38;2;{};{};{}m", r, g, b));
    }
    if let Some((r, g, b)) = bg {
        seq.push_str(&format!("\x1b[48;2;{};{};{}m", r, g, b));
    }
    if reverse_fallback && !has_colors {
        seq.push_str("\x1b[7m");
    }

    if seq.is_empty() {
        text.to_string()
    } else {
        format!("{}{}", seq, text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::Cache;
    use crate::config::{Config, FeedConfig, Theme, UiConfig};
    use crate::feed::FeedMeta;
    use crate::testing::tempdir;
    use std::sync::mpsc;

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
        let mut app = App::new(&config, &cache, true);
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

    #[test]
    fn stale_article_mutation_does_not_override_newer_pending_toggle() {
        let dir = tempdir().expect("tempdir");
        let cache = Cache::open_at(dir.path().join("test.tdkv")).expect("cache");
        seed_cache(&cache, false);
        let config = test_config();
        let (cmd_tx, _cmd_rx) = mpsc::channel();
        let mut app = App::new(&config, &cache, true);
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
        let mut app = App::new(&config, &cache, true);
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

    fn test_config() -> Config {
        Config {
            ui: UiConfig::default(),
            theme: Theme::default(),
            feeds: vec![FeedConfig {
                name: FEED_NAME.to_string(),
                url: FEED_URL.to_string(),
            }],
        }
    }

    fn seed_cache(cache: &Cache, read: bool) {
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

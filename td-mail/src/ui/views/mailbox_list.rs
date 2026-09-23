use crate::backend::{BackendCommand, BackendResponse, RetentionCandidate};
use crate::compose;
use crate::config::RetentionPolicyConfig;
use crate::jmap::types::Mailbox;
use crate::ui::input::{Key, Menu};
use crate::ui::views::email_list::{CachedEmailListState, EmailListView};
use crate::ui::views::help::HelpView;
use crate::ui::views::retention_preview::RetentionPreviewView;
use crate::ui::views::{
    format_system_time, strip_newlines, Body, Entry, Row, Scene, View, ViewAction,
};
use std::collections::{HashMap, HashSet};
use std::sync::mpsc;
use std::time::SystemTime;

/// The action bar's labels for each of the view's modes, and the key each
/// stands for; a press on a label is that key, and Folder's is the
/// dropdown of the folder actions (`+`, `d`), which stay keys as well.
const LABELS: &[&str] = &[
    "Open",
    "Refresh",
    "Compose",
    "Drafts",
    "Folder",
    "Mark read",
    "Account",
    "Help",
    "Quit",
];
const KEYS: &[Key] = &[
    Key::Enter,
    Key::Char('g'),
    Key::Char('c'),
    Key::Char('D'),
    Key::Menu(Menu::Folder),
    Key::Char('u'),
    Key::Char('a'),
    Key::Char('?'),
    Key::Char('q'),
];
const CREATE_LABELS: &[&str] = &["Create", "Cancel"];
const CREATE_KEYS: &[Key] = &[Key::Enter, Key::Escape];
const DELETE_LABELS: &[&str] = &["Delete", "Cancel"];
const DELETE_KEYS: &[Key] = &[Key::Char('y'), Key::Escape];

pub struct MailboxListView {
    cmd_tx: mpsc::Sender<BackendCommand>,
    from_address: String,
    reply_from_address: Option<String>,
    browser: Option<String>,
    page_size: u32,
    mailboxes: Vec<Mailbox>,
    cursor: usize,
    loading: bool,
    error: Option<String>,
    account_names: Vec<String>,
    current_account: String,
    pending_click: bool,
    archive_folder: String,
    deleted_folder: String,
    retention_policies: Vec<RetentionPolicyConfig>,
    status_message: Option<String>,
    pending_retention_preview: Option<Vec<RetentionCandidate>>,
    create_mode: bool,
    create_input: String,
    delete_confirm_mode: bool,
    last_refreshed: Option<SystemTime>,
    sync_interval_secs: Option<u64>,
    email_cache: HashMap<String, CachedEmailListState>,
}

impl MailboxListView {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cmd_tx: mpsc::Sender<BackendCommand>,
        from_address: String,
        reply_from_address: Option<String>,
        browser: Option<String>,
        page_size: u32,
        account_names: Vec<String>,
        current_account: String,
        archive_folder: String,
        deleted_folder: String,
        retention_policies: Vec<RetentionPolicyConfig>,
        sync_interval_secs: Option<u64>,
    ) -> Self {
        MailboxListView {
            cmd_tx,
            from_address,
            reply_from_address,
            browser,
            page_size,
            mailboxes: Vec::new(),
            cursor: 0,
            loading: true,
            error: None,
            account_names,
            current_account,
            pending_click: false,
            archive_folder,
            deleted_folder,
            retention_policies,
            status_message: None,
            pending_retention_preview: None,
            create_mode: false,
            create_input: String::new(),
            delete_confirm_mode: false,
            last_refreshed: None,
            sync_interval_secs,
            email_cache: HashMap::new(),
        }
    }

    fn is_cached_emails_fresh(&self, mailbox_id: &str) -> bool {
        let Some(sync_interval_secs) = self.sync_interval_secs else {
            return false;
        };
        let Some(cached) = self.email_cache.get(mailbox_id) else {
            return false;
        };
        match SystemTime::now().duration_since(cached.last_refreshed) {
            Ok(age) => age.as_secs() < sync_interval_secs,
            Err(_) => false,
        }
    }

    fn request_refresh(&mut self, origin: &str) {
        self.loading = true;
        let _ = self.cmd_tx.send(BackendCommand::FetchMailboxes {
            origin: origin.to_string(),
        });
    }

    /// The account after the current one, round; with one account it is
    /// that account, so selecting it again reopens it, which is how a
    /// connection that failed is retried.
    fn next_account_name(&self) -> Option<String> {
        if self.account_names.is_empty() {
            return None;
        }
        let current_idx = self
            .account_names
            .iter()
            .position(|n| n == &self.current_account)
            .unwrap_or(0);
        let next_idx = (current_idx + 1) % self.account_names.len();
        self.account_names.get(next_idx).cloned()
    }

    fn sort_mailboxes(mailboxes: &mut [Mailbox]) {
        mailboxes.sort_by(|a, b| {
            let rank = |m: &Mailbox| -> u32 {
                match m.role.as_deref() {
                    Some("inbox") => 0,
                    Some("drafts") => 1,
                    Some("sent") => 2,
                    Some("junk") => 3,
                    Some("trash") => 4,
                    Some("archive") => 5,
                    Some(_) => 6,
                    None => 7,
                }
            };
            let ra = rank(a);
            let rb = rank(b);
            if ra != rb {
                ra.cmp(&rb)
            } else {
                a.name.to_lowercase().cmp(&b.name.to_lowercase())
            }
        });
    }

    /// The window's title: the account when there is more than one to
    /// name, and when the counts were last refreshed. Confirming a delete,
    /// the title asks the question.
    fn title(&self) -> String {
        if self.delete_confirm_mode {
            let name = self
                .mailboxes
                .get(self.cursor)
                .map_or("(unknown)", |m| m.name.as_str());
            return format!("Delete folder '{}'?", name);
        }
        let title = if self.account_names.len() > 1 {
            format!("td-mail - {}", self.current_account)
        } else {
            "td-mail - Timmy's Mail Console".to_string()
        };
        match self.last_refreshed {
            Some(ts) => format!("{} (refreshed {})", title, format_system_time(ts)),
            None => title,
        }
    }

    /// Folder `index`: its name, its unread of its total at the right, and
    /// marked while it holds unread mail.
    fn mailbox_row(&self, index: usize) -> Row {
        let Some(mailbox) = self.mailboxes.get(index) else {
            return Row::default();
        };
        let meta = if mailbox.unread_emails > 0 {
            format!("{}/{}", mailbox.unread_emails, mailbox.total_emails)
        } else if mailbox.total_emails > 0 {
            mailbox.total_emails.to_string()
        } else {
            String::new()
        };
        Row {
            label: strip_newlines(&mailbox.name),
            meta,
            marked: mailbox.unread_emails > 0,
        }
    }

    /// The status row: the mode's keys, behind any message the view has.
    fn status_line(&self) -> String {
        // With one account the key reopens it, which is the reconnect.
        let account_hint = if self.account_names.len() > 1 {
            " a:account"
        } else {
            " a:reconnect"
        };
        let base = if self.create_mode {
            "New folder name | Enter:create Esc:cancel".to_string()
        } else if self.delete_confirm_mode {
            "Confirm delete | y:delete n/Esc:cancel".to_string()
        } else if self.loading {
            format!(
                "Loading... | q:quit g:refresh c:compose D:drafts +:new-folder d:delete-folder u:read-all x:preview-expire X:expire{}",
                account_hint
            )
        } else if self.mailboxes.is_empty() {
            format!(
                "q:quit g:refresh c:compose D:drafts +:new-folder x:preview-expire X:expire{}",
                account_hint
            )
        } else {
            format!(
                "{}/{} | q:quit n/p:navigate RET:open g:refresh c:compose D:drafts +:new-folder d:delete-folder u:read-all x:preview-expire X:expire ?:help{}",
                self.cursor + 1,
                self.mailboxes.len(),
                account_hint,
            )
        };
        match self.status_message {
            Some(ref msg) => format!("{} | {}", msg, base),
            None => base,
        }
    }

    fn build_email_list_view(&self, mailbox: &Mailbox) -> EmailListView {
        let reply_from = self
            .reply_from_address
            .clone()
            .unwrap_or_else(|| self.from_address.clone());
        let mut view = EmailListView::new(
            self.cmd_tx.clone(),
            reply_from,
            mailbox.id.clone(),
            mailbox.name.clone(),
            self.page_size,
            self.mailboxes.clone(),
            self.archive_folder.clone(),
            self.deleted_folder.clone(),
            self.browser.clone(),
        );
        // Always hydrate from any cached snapshot we have, even if stale.
        // Freshness only controls whether we skip a background refresh.
        if let Some(cached) = self.email_cache.get(&mailbox.id) {
            view.apply_cached_state(cached);
        }
        view
    }

    fn maybe_query_on_open(&self, mailbox: &Mailbox, origin: &str) {
        // Query only on cold miss or stale snapshot.
        if self.is_cached_emails_fresh(&mailbox.id) {
            return;
        }
        let _ = self.cmd_tx.send(BackendCommand::QueryEmails {
            origin: origin.to_string(),
            mailbox_id: mailbox.id.clone(),
            page_size: self.page_size,
            position: 0,
            search_query: None,
            received_after: None,
            received_before: None,
        });
    }
}

impl View for MailboxListView {
    fn scene(&self) -> Scene<'_> {
        // The list stays under the naming band and the delete question, so
        // the folder each is about is the one shown selected.
        let body = if self.loading && self.mailboxes.is_empty() {
            Body::message("Loading mailboxes...".to_string())
        } else if let Some(ref err) = self.error {
            Body::message(err.clone())
        } else if self.mailboxes.is_empty() {
            Body::message("No mailboxes found.".to_string())
        } else {
            Body::List {
                total: self.mailboxes.len(),
                selected: self.cursor,
                row: Box::new(move |index| self.mailbox_row(index)),
            }
        };
        let (labels, keys) = if self.create_mode {
            (CREATE_LABELS, CREATE_KEYS)
        } else if self.delete_confirm_mode {
            (DELETE_LABELS, DELETE_KEYS)
        } else {
            (LABELS, KEYS)
        };
        Scene {
            title: self.title(),
            labels,
            keys,
            entry: self.create_mode.then_some(Entry {
                placeholder: "New folder name",
                text: &self.create_input,
            }),
            body,
            status: self.status_line(),
        }
    }

    fn handle_key(&mut self, key: Key, page: usize) -> ViewAction {
        if self.create_mode {
            match key {
                Key::Enter => {
                    let name = self.create_input.trim().to_string();
                    if name.is_empty() {
                        self.status_message = Some("Folder name cannot be empty".to_string());
                    } else if let Err(e) = self
                        .cmd_tx
                        .send(BackendCommand::CreateMailbox { name: name.clone() })
                    {
                        self.status_message = Some(format!("Create folder failed to send: {}", e));
                    } else {
                        self.status_message = Some(format!("Creating folder '{}'", name));
                        self.create_mode = false;
                        self.create_input.clear();
                    }
                }
                // Only Escape leaves: the name is typed into the entry,
                // and a `q` in it is a letter.
                Key::Escape => {
                    self.create_mode = false;
                    self.create_input.clear();
                }
                Key::Backspace => {
                    self.create_input.pop();
                }
                Key::Char(c) => {
                    self.create_input.push(c);
                }
                _ => {}
            }
            return ViewAction::Continue;
        }

        if self.delete_confirm_mode {
            match key {
                Key::Char('y') | Key::Char('Y') => {
                    if let Some(mailbox) = self.mailboxes.get(self.cursor) {
                        if let Err(e) = self.cmd_tx.send(BackendCommand::DeleteMailbox {
                            id: mailbox.id.clone(),
                            name: mailbox.name.clone(),
                        }) {
                            self.status_message =
                                Some(format!("Delete folder failed to send: {}", e));
                        } else {
                            self.status_message =
                                Some(format!("Deleting folder '{}'", mailbox.name));
                        }
                    }
                    self.delete_confirm_mode = false;
                }
                Key::Escape | Key::Char('q') | Key::Char('n') | Key::Char('N') | Key::Enter => {
                    self.delete_confirm_mode = false;
                }
                _ => {}
            }
            return ViewAction::Continue;
        }

        match key {
            Key::Char('q') => ViewAction::Quit,
            Key::Char('n') | Key::Char('j') | Key::Down => {
                if !self.mailboxes.is_empty() && self.cursor + 1 < self.mailboxes.len() {
                    self.cursor += 1;
                }
                ViewAction::Continue
            }
            Key::Char('p') | Key::Char('k') | Key::Up => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                }
                ViewAction::Continue
            }
            Key::PageDown => {
                if !self.mailboxes.is_empty() {
                    self.cursor = (self.cursor + page).min(self.mailboxes.len() - 1);
                }
                ViewAction::Continue
            }
            Key::PageUp => {
                self.cursor = self.cursor.saturating_sub(page);
                ViewAction::Continue
            }
            Key::Home => {
                self.cursor = 0;
                ViewAction::Continue
            }
            Key::End => {
                if !self.mailboxes.is_empty() {
                    self.cursor = self.mailboxes.len() - 1;
                }
                ViewAction::Continue
            }
            Key::Enter => {
                if let Some(mailbox) = self.mailboxes.get(self.cursor) {
                    let view = self.build_email_list_view(mailbox);
                    self.maybe_query_on_open(mailbox, "mailbox_list.open_enter");
                    ViewAction::Push(Box::new(view))
                } else {
                    ViewAction::Continue
                }
            }
            Key::Char('g') => {
                self.request_refresh("mailbox_list.key_g");
                ViewAction::Continue
            }
            Key::Char('+') => {
                self.create_mode = true;
                self.create_input.clear();
                ViewAction::Continue
            }
            Key::Char('d') => {
                if !self.mailboxes.is_empty() {
                    self.delete_confirm_mode = true;
                }
                ViewAction::Continue
            }
            Key::Char('u') => {
                if let Some(mailbox) = self.mailboxes.get(self.cursor) {
                    if mailbox.unread_emails == 0 {
                        self.status_message =
                            Some(format!("Folder '{}' already read", mailbox.name));
                    } else if let Err(e) = self.cmd_tx.send(BackendCommand::MarkMailboxRead {
                        mailbox_id: mailbox.id.clone(),
                        mailbox_name: mailbox.name.clone(),
                    }) {
                        self.status_message =
                            Some(format!("Mark folder read failed to send: {}", e));
                    } else {
                        self.status_message =
                            Some(format!("Marking folder '{}' read...", mailbox.name));
                    }
                }
                ViewAction::Continue
            }
            Key::Char('x') => {
                let _ = self.cmd_tx.send(BackendCommand::PreviewRetentionExpiry {
                    policies: self.retention_policies.clone(),
                });
                self.status_message = Some("Building retention preview...".to_string());
                ViewAction::Continue
            }
            Key::Char('X') => {
                let _ = self.cmd_tx.send(BackendCommand::ExecuteRetentionExpiry {
                    policies: self.retention_policies.clone(),
                });
                self.status_message = Some("Expiring retained mail...".to_string());
                ViewAction::Continue
            }
            Key::Char('c') => {
                let from = self
                    .reply_from_address
                    .as_deref()
                    .unwrap_or(&self.from_address);
                let draft = compose::build_compose_draft(from);
                ViewAction::Compose(draft.into())
            }
            Key::Char('D') => ViewAction::Drafts,
            Key::Char('a') => {
                if let Some(next) = self.next_account_name() {
                    ViewAction::SwitchAccount(next)
                } else {
                    ViewAction::Continue
                }
            }
            Key::Char('?') => ViewAction::Push(Box::new(HelpView::new())),
            Key::ScrollUp => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                }
                ViewAction::Continue
            }
            Key::ScrollDown => {
                if !self.mailboxes.is_empty() && self.cursor + 1 < self.mailboxes.len() {
                    self.cursor += 1;
                }
                ViewAction::Continue
            }
            // The press selects the folder; opening it is the pending
            // action, so the selection is shown before the folder loads.
            Key::Click(index) => {
                if index < self.mailboxes.len() {
                    self.cursor = index;
                    self.pending_click = true;
                }
                ViewAction::Continue
            }
            _ => ViewAction::Continue,
        }
    }

    fn take_pending_action(&mut self) -> Option<ViewAction> {
        if self.pending_click {
            self.pending_click = false;
            if let Some(mailbox) = self.mailboxes.get(self.cursor) {
                let view = self.build_email_list_view(mailbox);
                self.maybe_query_on_open(mailbox, "mailbox_list.open_click");
                return Some(ViewAction::Push(Box::new(view)));
            }
        }
        if let Some(candidates) = self.pending_retention_preview.take() {
            return Some(ViewAction::Push(Box::new(RetentionPreviewView::new(
                candidates,
            ))));
        }
        None
    }

    fn on_response(&mut self, response: &BackendResponse) -> bool {
        match response {
            BackendResponse::Mailboxes(result) => {
                self.loading = false;
                match result {
                    Ok(mailboxes) => {
                        let mut mailboxes = mailboxes.clone();
                        Self::sort_mailboxes(&mut mailboxes);
                        self.mailboxes = mailboxes;
                        self.error = None;
                        self.last_refreshed = Some(SystemTime::now());
                        if self.cursor >= self.mailboxes.len() && !self.mailboxes.is_empty() {
                            self.cursor = self.mailboxes.len() - 1;
                        }
                    }
                    Err(e) => {
                        self.error = Some(format!("Failed to fetch mailboxes: {}", e));
                    }
                }
                true
            }
            BackendResponse::Emails {
                mailbox_id,
                emails,
                total,
                position,
                loaded,
                thread_counts,
            } => {
                if let Ok(emails) = emails {
                    let now = SystemTime::now();
                    let entry = self
                        .email_cache
                        .entry(mailbox_id.clone())
                        .or_insert_with(|| CachedEmailListState {
                            emails: Vec::new(),
                            total: None,
                            next_query_position: 0,
                            last_loaded_count: 0,
                            thread_counts: HashMap::new(),
                            last_refreshed: now,
                        });

                    if *position == 0 {
                        entry.emails = emails.clone();
                        entry.thread_counts = thread_counts.clone();
                    } else {
                        entry
                            .thread_counts
                            .extend(thread_counts.iter().map(|(k, v)| (k.clone(), *v)));
                        let mut existing_ids: HashSet<String> =
                            entry.emails.iter().map(|e| e.id.clone()).collect();
                        for email in emails {
                            if existing_ids.insert(email.id.clone()) {
                                entry.emails.push(email.clone());
                            }
                        }
                    }

                    entry.total = *total;
                    entry.last_loaded_count = *loaded;
                    entry.next_query_position = position.saturating_add(*loaded);
                    entry.last_refreshed = now;
                }
                false
            }
            BackendResponse::RetentionPreview { result } => {
                match result {
                    Ok(preview) => {
                        self.status_message = Some(format!(
                            "{} message(s) eligible for expiry",
                            preview.candidates.len()
                        ));
                        self.pending_retention_preview = Some(preview.candidates.clone());
                    }
                    Err(e) => {
                        self.status_message = Some(format!("Retention preview failed: {}", e));
                    }
                }
                true
            }
            BackendResponse::MailboxCreated { name, result } => {
                match result {
                    Ok(()) => {
                        self.status_message = Some(format!("Created folder '{}'", name));
                        self.request_refresh("mailbox_list.mailbox_created");
                    }
                    Err(e) => {
                        self.status_message =
                            Some(format!("Create folder '{}' failed: {}", name, e));
                    }
                }
                true
            }
            BackendResponse::MailboxDeleted { name, result } => {
                match result {
                    Ok(()) => {
                        self.status_message = Some(format!("Deleted folder '{}'", name));
                        self.request_refresh("mailbox_list.mailbox_deleted");
                    }
                    Err(e) => {
                        self.status_message =
                            Some(format!("Delete folder '{}' failed: {}", name, e));
                    }
                }
                true
            }
            BackendResponse::RetentionExecuted { result } => {
                match result {
                    Ok(exec) => {
                        if exec.failed_batches.is_empty() {
                            self.status_message =
                                Some(format!("Expired {} message(s)", exec.deleted));
                        } else {
                            self.status_message = Some(format!(
                                "Expired {} message(s), {} batch(es) failed",
                                exec.deleted,
                                exec.failed_batches.len()
                            ));
                        }
                        self.request_refresh("mailbox_list.retention_executed");
                    }
                    Err(e) => {
                        self.status_message = Some(format!("Retention expiry failed: {}", e));
                    }
                }
                true
            }
            BackendResponse::EmailMutation { .. } => {
                // No refresh: optimistic update + cache already reflect the
                // correct state. The user can press 'g' to refresh manually.
                false
            }
            BackendResponse::ThreadMarkedRead { .. } => false,
            BackendResponse::MailboxMarkedRead {
                mailbox_id,
                mailbox_name,
                updated,
                result,
            } => {
                match result {
                    Ok(()) => {
                        if *updated == 0 {
                            self.status_message =
                                Some(format!("Folder '{}' already read", mailbox_name));
                        } else {
                            self.status_message = Some(format!(
                                "Marked {} message(s) read in '{}'",
                                updated, mailbox_name
                            ));
                        }
                        self.request_refresh(&format!(
                            "mailbox_list.mailbox_marked_read:{}",
                            mailbox_id
                        ));
                    }
                    Err(e) => {
                        self.status_message = Some(format!(
                            "Mark all read failed for '{}': {}",
                            mailbox_name, e
                        ));
                    }
                }
                true
            }
            _ => false,
        }
    }

    fn trigger_idle_sync(&mut self) -> bool {
        if self.loading || self.create_mode || self.delete_confirm_mode {
            return false;
        }
        self.request_refresh("mailbox_list.idle_sync");
        true
    }

    fn on_reveal(&mut self) -> bool {
        // Returning from a folder: reads/moves/deletes there may have changed
        // both the unread/total counts and the cached email snapshots. Refetch
        // counts, and expire snapshot freshness so reopening a folder re-queries
        // instead of showing stale rows (it still hydrates from cache instantly).
        for entry in self.email_cache.values_mut() {
            entry.last_refreshed = SystemTime::UNIX_EPOCH;
        }
        self.request_refresh("mailbox_list.reveal");
        true
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn make_mailbox(id: &str, name: &str, role: Option<&str>, total: u32, unread: u32) -> Mailbox {
        Mailbox {
            id: id.to_string(),
            name: name.to_string(),
            parent_id: None,
            role: role.map(str::to_string),
            total_emails: total,
            unread_emails: unread,
            sort_order: 0,
        }
    }

    fn make_view() -> (MailboxListView, mpsc::Receiver<BackendCommand>) {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let view = MailboxListView::new(
            cmd_tx,
            "me@example.com".to_string(),
            None,
            None,
            50,
            vec!["personal".to_string()],
            "personal".to_string(),
            "Archive".to_string(),
            "Trash".to_string(),
            Vec::new(),
            None,
        );
        (view, cmd_rx)
    }

    /// The folders in the order the view sorts them: the inbox, the
    /// archive, the roleless one last.
    fn folders() -> BackendResponse {
        BackendResponse::Mailboxes(Ok(vec![
            make_mailbox("m1", "Inbox", Some("inbox"), 10, 2),
            make_mailbox("m2", "Archive", Some("archive"), 100, 0),
            make_mailbox("m3", "Notes", None, 0, 0),
        ]))
    }

    fn rows(view: &MailboxListView) -> Vec<Row> {
        let scene = view.scene();
        let Body::List { total, row, .. } = scene.body else {
            return Vec::new();
        };
        let mut rows = Vec::with_capacity(total);
        for index in 0..total {
            rows.push(row(index));
        }
        rows
    }

    #[test]
    fn the_folders_are_rows_of_name_and_unread_of_total() {
        let (mut view, _cmd_rx) = make_view();
        // Nothing fetched yet: the pane says the folders are loading.
        assert!(matches!(view.scene().body, Body::Text { .. }));
        assert!(view.on_response(&folders()));
        assert_eq!(
            rows(&view),
            vec![
                Row {
                    label: "Inbox".to_string(),
                    meta: "2/10".to_string(),
                    marked: true,
                },
                Row {
                    label: "Archive".to_string(),
                    meta: "100".to_string(),
                    marked: false,
                },
                Row {
                    label: "Notes".to_string(),
                    meta: String::new(),
                    marked: false,
                },
            ]
        );
        let scene = view.scene();
        assert!(
            scene
                .title
                .starts_with("td-mail - Timmy's Mail Console (refreshed "),
            "{}",
            scene.title
        );
        assert_eq!(scene.labels, LABELS);
        assert_eq!(scene.keys, KEYS);
        assert!(scene.entry.is_none());
        assert!(scene.status.starts_with("1/3 | "), "{}", scene.status);
    }

    #[test]
    fn a_press_on_a_row_selects_it_and_opens_that_folder() {
        let (mut view, cmd_rx) = make_view();
        view.on_response(&folders());
        view.handle_key(Key::Click(1), 10);
        assert_eq!(view.cursor, 1);
        assert!(matches!(
            view.take_pending_action(),
            Some(ViewAction::Push(_))
        ));
        let mut queried = None;
        while let Ok(cmd) = cmd_rx.try_recv() {
            if let BackendCommand::QueryEmails { mailbox_id, .. } = cmd {
                queried = Some(mailbox_id);
            }
        }
        assert_eq!(queried.as_deref(), Some("m2"));
        // A press past the last row is no row.
        view.handle_key(Key::Click(9), 10);
        assert_eq!(view.cursor, 1);
        assert!(view.take_pending_action().is_none());
    }

    #[test]
    fn naming_a_folder_shows_the_entry_and_a_delete_asks_in_the_title() {
        let (mut view, _cmd_rx) = make_view();
        view.on_response(&folders());
        view.handle_key(Key::Char('+'), 10);
        view.handle_key(Key::Char('P'), 10);
        view.handle_key(Key::Char('r'), 10);
        {
            let scene = view.scene();
            let entry = scene.entry.expect("the folder is being named");
            assert_eq!(entry.placeholder, "New folder name");
            assert_eq!(entry.text, "Pr");
            assert_eq!(scene.labels, CREATE_LABELS);
            assert_eq!(scene.keys, CREATE_KEYS);
            assert!(scene.status.starts_with("New folder name | "));
        }
        view.handle_key(Key::Escape, 10);
        assert!(view.scene().entry.is_none());

        view.handle_key(Key::Char('d'), 10);
        let scene = view.scene();
        assert_eq!(scene.title, "Delete folder 'Inbox'?");
        assert_eq!(scene.labels, DELETE_LABELS);
        assert_eq!(scene.keys, DELETE_KEYS);
        assert!(scene.entry.is_none());
        assert!(scene.status.ends_with("y:delete n/Esc:cancel"));
        // The folder the question is about is the one still shown selected.
        assert!(matches!(
            scene.body,
            Body::List {
                total: 3,
                selected: 0,
                ..
            }
        ));
    }
}

use crate::backend::{BackendCommand, BackendResponse, EmailMutationAction, RulesDryRunResult};
use crate::compose;
use crate::jmap::types::{Email, Mailbox};
use crate::rules;
use crate::ui::input::Key;
use crate::ui::views::email_view::{EmailNavEntry, EmailView};
use crate::ui::views::help::HelpView;
use crate::ui::views::rules_preview::RulesPreviewView;
use crate::ui::views::thread_view::ThreadView;
use crate::ui::views::{
    format_system_time, strip_newlines, Body, Entry, Row, Scene, View, ViewAction,
};
use std::collections::{HashMap, HashSet};
use std::sync::mpsc;
use std::time::SystemTime;

/// The action bar's labels for each mode, and the key each stands for; a
/// click on a label is that key.
const LIST_LABELS: &[&str] = &[
    "Read", "Refresh", "Reply", "Archive", "Delete", "Move", "Search", "Compose", "Help", "Back",
];
const LIST_KEYS: &[Key] = &[
    Key::Enter,
    Key::Char('g'),
    Key::Char('r'),
    Key::Char('a'),
    Key::Char('d'),
    Key::Char('m'),
    Key::Char('s'),
    Key::Char('c'),
    Key::Char('?'),
    Key::Char('q'),
];
const SEARCH_LABELS: &[&str] = &["Cancel"];
const SEARCH_KEYS: &[Key] = &[Key::Escape];
const MOVE_LABELS: &[&str] = &["Move", "Cancel"];
const MOVE_KEYS: &[Key] = &[Key::Enter, Key::Escape];

enum PendingWriteOp {
    Flag {
        email_id: String,
        old_flagged: bool,
    },
    Seen {
        email_id: String,
        old_seen: bool,
    },
    Move {
        email: Box<Email>,
        from_index: usize,
    },
}

#[derive(Clone)]
pub struct CachedEmailListState {
    pub emails: Vec<Email>,
    pub total: Option<u32>,
    pub next_query_position: u32,
    pub last_loaded_count: u32,
    pub thread_counts: HashMap<String, (usize, usize)>,
    pub last_refreshed: SystemTime,
}

pub struct EmailListView {
    cmd_tx: mpsc::Sender<BackendCommand>,
    reply_from_address: String,
    mailbox_id: String,
    mailbox_name: String,
    page_size: u32,
    emails: Vec<Email>,
    cursor: usize,
    total: Option<u32>,
    next_query_position: u32,
    last_loaded_count: u32,
    loading: bool,
    loading_more: bool,
    error: Option<String>,
    pending_click: bool,
    pending_reply_request: Option<(String, bool)>,
    pending_compose: Option<String>,
    pending_rules_preview: Option<(String, RulesDryRunResult)>,
    mailboxes: Vec<Mailbox>,
    move_mode: bool,
    move_cursor: usize,
    search_mode: bool,
    search_input: String,
    active_search: Option<String>,
    status_message: Option<String>,
    next_write_op_id: u64,
    pending_write_ops: HashMap<u64, PendingWriteOp>,
    thread_counts: HashMap<String, (usize, usize)>,
    /// On-demand spam verdicts keyed by email id (from the `S` key), used to tag
    /// rows in the list. Not persisted; populated as the user scores messages.
    spam_verdicts: HashMap<String, String>,
    archive_folder: String,
    deleted_folder: String,
    browser: Option<String>,
    last_refreshed: Option<SystemTime>,
}

impl EmailListView {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cmd_tx: mpsc::Sender<BackendCommand>,
        reply_from_address: String,
        mailbox_id: String,
        mailbox_name: String,
        page_size: u32,
        mailboxes: Vec<Mailbox>,
        archive_folder: String,
        deleted_folder: String,
        browser: Option<String>,
    ) -> Self {
        EmailListView {
            cmd_tx,
            reply_from_address,
            mailbox_id,
            mailbox_name,
            page_size,
            emails: Vec::new(),
            cursor: 0,
            total: None,
            next_query_position: 0,
            last_loaded_count: 0,
            loading: true,
            loading_more: false,
            error: None,
            pending_click: false,
            pending_reply_request: None,
            pending_compose: None,
            pending_rules_preview: None,
            mailboxes,
            move_mode: false,
            move_cursor: 0,
            search_mode: false,
            search_input: String::new(),
            active_search: None,
            status_message: None,
            next_write_op_id: 1,
            pending_write_ops: HashMap::new(),
            thread_counts: HashMap::new(),
            spam_verdicts: HashMap::new(),
            archive_folder,
            deleted_folder,
            browser,
            last_refreshed: None,
        }
    }

    pub fn apply_cached_state(&mut self, state: &CachedEmailListState) {
        self.emails = state.emails.clone();
        self.total = state.total;
        self.next_query_position = state.next_query_position;
        self.last_loaded_count = state.last_loaded_count;
        self.thread_counts = state.thread_counts.clone();
        self.last_refreshed = Some(state.last_refreshed);
        self.loading = false;
        self.loading_more = false;
        self.error = None;
        self.pending_write_ops.clear();
        if self.cursor >= self.emails.len() && !self.emails.is_empty() {
            self.cursor = self.emails.len() - 1;
        }
    }

    fn request_refresh(&mut self, origin: &str) {
        self.next_query_position = 0;
        self.last_loaded_count = 0;
        self.loading = true;
        self.loading_more = false;
        let _ = self.cmd_tx.send(BackendCommand::QueryEmails {
            origin: origin.to_string(),
            mailbox_id: self.mailbox_id.clone(),
            page_size: self.page_size,
            position: 0,
            search_query: self.active_search.clone(),
            received_after: None,
            received_before: None,
        });
    }

    fn can_load_more(&self) -> bool {
        if self.loading || self.emails.is_empty() {
            return false;
        }

        if let Some(total) = self.total {
            (self.emails.len() as u32) < total
        } else {
            self.last_loaded_count >= self.page_size
        }
    }

    fn request_load_more(&mut self) -> bool {
        if !self.can_load_more() {
            return false;
        }

        self.loading = true;
        self.loading_more = true;
        match self.cmd_tx.send(BackendCommand::QueryEmails {
            origin: "email_list.load_more".to_string(),
            mailbox_id: self.mailbox_id.clone(),
            page_size: self.page_size,
            position: self.next_query_position,
            search_query: self.active_search.clone(),
            received_after: None,
            received_before: None,
        }) {
            Ok(()) => true,
            Err(e) => {
                self.loading = false;
                self.loading_more = false;
                self.status_message = Some(format!("Load more failed: {}", e));
                false
            }
        }
    }

    fn is_unread(email: &Email) -> bool {
        !email.keywords.contains_key("$seen")
    }

    fn is_flagged(email: &Email) -> bool {
        email.keywords.contains_key("$flagged")
    }

    /// Single-char spam tag for the row: S=spam, ?=unsure, blank=ham/unscored.
    fn spam_marker(&self, email: &Email) -> &'static str {
        match self.spam_verdicts.get(&email.id).map(|s| s.as_str()) {
            Some("spam") => "S",
            Some("unsure") => "?",
            _ => " ",
        }
    }

    /// Message `index` as a row: the subject, led by the flag and the spam
    /// verdict and followed by the thread's read count, over the sender and
    /// the day it arrived. Unread is the mark; the widget elides what does
    /// not fit, so nothing here is laid out to a width.
    fn email_row(&self, index: usize) -> Row {
        let Some(email) = self.emails.get(index) else {
            return Row::default();
        };
        let mut label = String::new();
        if Self::is_flagged(email) {
            label.push_str("F ");
        }
        let spam = self.spam_marker(email).trim();
        if !spam.is_empty() {
            label.push_str(spam);
            label.push(' ');
        }
        label.push_str(&strip_newlines(
            email.subject.as_deref().unwrap_or("(no subject)"),
        ));
        if let Some((unread, total)) = self.get_thread_counts(email) {
            if total > 1 {
                label.push_str(&format!(" [{}/{}]", total.saturating_sub(unread), total));
            }
        }

        let from = email
            .from
            .as_ref()
            .and_then(|addrs| addrs.first())
            .map(|a| {
                a.name
                    .as_deref()
                    .unwrap_or_else(|| a.email.as_deref().unwrap_or("(unknown)"))
            })
            .unwrap_or("(unknown)");
        // The day of an ISO timestamp, and a shorter one whole.
        let date = email
            .received_at
            .as_deref()
            .map_or("", |d| d.get(..10).unwrap_or(d));

        Row {
            label,
            meta: format!("{} · {}", strip_newlines(from), date),
            marked: Self::is_unread(email),
        }
    }

    /// The window's title: the mailbox, what it was searched for, and when
    /// it was last refreshed.
    fn title(&self) -> String {
        let base = if let Some(ref query) = self.active_search {
            match self.total {
                Some(total) => format!(
                    "{} [search: {}] ({} results)",
                    self.mailbox_name, query, total
                ),
                None => format!("{} [search: {}]", self.mailbox_name, query),
            }
        } else {
            match self.total {
                Some(total) => format!("{} ({} messages)", self.mailbox_name, total),
                None => self.mailbox_name.clone(),
            }
        };
        match self.last_refreshed {
            Some(ts) => format!("{} (refreshed {})", base, format_system_time(ts)),
            None => base,
        }
    }

    /// The messages, the mailbox picker while moving, or the word for a
    /// list there is nothing to show of.
    fn body(&self) -> Body<'_> {
        if self.move_mode {
            return Body::List {
                total: self.mailboxes.len(),
                selected: self.move_cursor,
                row: Box::new(move |index| Row {
                    label: self
                        .mailboxes
                        .get(index)
                        .map(|mailbox| mailbox.name.clone())
                        .unwrap_or_default(),
                    ..Row::default()
                }),
            };
        }
        if self.loading && self.emails.is_empty() {
            return Body::message("Loading emails...".to_string());
        }
        if let Some(error) = &self.error {
            return Body::message(error.clone());
        }
        if self.emails.is_empty() {
            return Body::message("No messages.".to_string());
        }
        Body::List {
            total: self.emails.len(),
            selected: self.cursor,
            row: Box::new(move |index| self.email_row(index)),
        }
    }

    /// The status row: where the cursor is and the keys that act on it,
    /// behind whatever the last command had to say.
    fn status(&self) -> String {
        // Searching and moving say their keys only: the entry shows what is
        // typed, and the picker its place in the mailboxes.
        let base = if self.search_mode {
            "RET:search Esc:cancel".to_string()
        } else if self.move_mode {
            format!(
                "Move to mailbox: {}/{} | n/p:navigate RET:move Esc:cancel",
                self.move_cursor + 1,
                self.mailboxes.len()
            )
        } else if self.loading {
            if self.loading_more {
                "Loading more... | q:back".to_string()
            } else {
                "Loading... | q:back".to_string()
            }
        } else if self.emails.is_empty() {
            "q:back g:refresh s:search".to_string()
        } else {
            let search_hint = if self.active_search.is_some() {
                " Esc:clear-search"
            } else {
                ""
            };
            let load_more_hint = if self.can_load_more() { " l:more" } else { "" };
            let expire_hint = if self.is_in_deleted_folder() {
                " D:expire"
            } else {
                ""
            };
            format!(
                "{}/{} | q:back n/p:nav RET:read g:refresh r:reply R:reply-all e:dry-run E:run-rules a:archive d:delete{} J:spam H:ham S:score f:flag u:unread m:move s:search{}{}",
                self.cursor + 1,
                self.total.unwrap_or(self.emails.len() as u32),
                expire_hint,
                search_hint,
                load_more_hint
            )
        };
        match &self.status_message {
            Some(message) => format!("{} | {}", message, base),
            None => base,
        }
    }

    fn get_thread_counts(&self, email: &Email) -> Option<(usize, usize)> {
        email
            .thread_id
            .as_ref()
            .and_then(|tid| self.thread_counts.get(tid))
            .copied()
    }

    fn next_op_id(&mut self) -> u64 {
        let id = self.next_write_op_id;
        self.next_write_op_id = self.next_write_op_id.wrapping_add(1);
        id
    }

    fn set_email_flag_state(&mut self, email_id: &str, flagged: bool) {
        if let Some(email) = self.emails.iter_mut().find(|e| e.id == email_id) {
            if flagged {
                email.keywords.insert("$flagged".to_string(), true);
            } else {
                email.keywords.remove("$flagged");
            }
        }
    }

    fn set_email_seen_state(&mut self, email_id: &str, seen: bool) {
        if let Some(email) = self.emails.iter_mut().find(|e| e.id == email_id) {
            if seen {
                email.keywords.insert("$seen".to_string(), true);
            } else {
                email.keywords.remove("$seen");
            }
        }
    }

    fn rollback_pending_write(&mut self, op: PendingWriteOp) {
        match op {
            PendingWriteOp::Flag {
                email_id,
                old_flagged,
            } => self.set_email_flag_state(&email_id, old_flagged),
            PendingWriteOp::Seen { email_id, old_seen } => {
                self.set_email_seen_state(&email_id, old_seen)
            }
            PendingWriteOp::Move { email, from_index } => {
                let insert_at = from_index.min(self.emails.len());
                self.emails.insert(insert_at, *email);
                self.cursor = insert_at;
                if let Some(ref mut total) = self.total {
                    *total = total.saturating_add(1);
                }
            }
        }
    }

    fn open_selected(&mut self) -> Option<ViewAction> {
        let email = self.emails.get(self.cursor)?;
        let thread_total = self
            .get_thread_counts(email)
            .map(|(_, total)| total)
            .unwrap_or(1);
        let can_expire_now = self.is_in_deleted_folder();

        if thread_total > 1 {
            // Open concatenated thread reading view
            let thread_id = email.thread_id.clone().unwrap_or_default();
            let subject = email
                .subject
                .clone()
                .unwrap_or_else(|| "(no subject)".to_string());
            let view = EmailView::new_thread(
                self.cmd_tx.clone(),
                self.reply_from_address.clone(),
                thread_id,
                subject,
                can_expire_now,
                self.mailboxes.clone(),
                self.archive_folder.clone(),
                self.deleted_folder.clone(),
                self.browser.clone(),
            );
            Some(ViewAction::Push(Box::new(view)))
        } else {
            self.open_single_email()
        }
    }

    fn open_thread_list(&mut self, cross_folder: bool) -> Option<ViewAction> {
        let email = self.emails.get(self.cursor)?;
        let thread_total = self
            .get_thread_counts(email)
            .map(|(_, total)| total)
            .unwrap_or(1);
        let can_expire_now = self.is_in_deleted_folder();

        if thread_total > 1 || cross_folder {
            let thread_id = email.thread_id.clone().unwrap_or_default();
            let subject = email
                .subject
                .clone()
                .unwrap_or_else(|| "(no subject)".to_string());
            let filter_mailbox_id = if cross_folder {
                None
            } else {
                Some(self.mailbox_id.clone())
            };
            let view = ThreadView::new(
                self.cmd_tx.clone(),
                self.reply_from_address.clone(),
                thread_id,
                subject,
                self.mailboxes.clone(),
                self.archive_folder.clone(),
                self.deleted_folder.clone(),
                can_expire_now,
                filter_mailbox_id,
                self.browser.clone(),
            );
            Some(ViewAction::Push(Box::new(view)))
        } else {
            self.open_single_email()
        }
    }

    fn open_single_email(&mut self) -> Option<ViewAction> {
        let email = self.emails.get(self.cursor)?;
        let email_id = email.id.clone();
        let was_seen = email.keywords.contains_key("$seen");
        let nav_entries: Vec<EmailNavEntry> = self
            .emails
            .iter()
            .map(|e| EmailNavEntry {
                id: e.id.clone(),
                unread: !e.keywords.contains_key("$seen"),
            })
            .collect();
        let view = EmailView::new(
            self.cmd_tx.clone(),
            self.reply_from_address.clone(),
            email_id.clone(),
            nav_entries,
            self.cursor,
            self.is_in_deleted_folder(),
            self.mailboxes.clone(),
            self.archive_folder.clone(),
            self.deleted_folder.clone(),
            self.browser.clone(),
        );
        let _ = self.cmd_tx.send(BackendCommand::GetEmail {
            id: email_id.clone(),
        });
        if !was_seen {
            let op_id = self.next_op_id();
            self.pending_write_ops.insert(
                op_id,
                PendingWriteOp::Seen {
                    email_id: email_id.clone(),
                    old_seen: false,
                },
            );
            self.set_email_seen_state(&email_id, true);
            if let Err(e) = self.cmd_tx.send(BackendCommand::MarkEmailRead {
                op_id,
                id: email_id.clone(),
            }) {
                self.record_send_failure(
                    op_id,
                    PendingWriteOp::Seen {
                        email_id,
                        old_seen: false,
                    },
                    "Mark read",
                    e.to_string(),
                );
            }
        }
        Some(ViewAction::Push(Box::new(view)))
    }

    fn record_send_failure(&mut self, op_id: u64, op: PendingWriteOp, action: &str, err: String) {
        self.pending_write_ops.remove(&op_id);
        self.rollback_pending_write(op);
        self.status_message = Some(format!("{} failed: {}", action, err));
    }

    fn is_in_deleted_folder(&self) -> bool {
        if self.mailbox_name.eq_ignore_ascii_case(&self.deleted_folder) {
            return true;
        }
        rules::resolve_mailbox_id(&self.deleted_folder, &self.mailboxes)
            .is_some_and(|id| id == self.mailbox_id)
    }

    fn current_mailbox_has_role(&self, role: &str) -> bool {
        self.mailboxes
            .iter()
            .any(|m| m.id == self.mailbox_id && m.role.as_deref() == Some(role))
    }

    /// Train the spam classifier on the selected message, and relocate it only
    /// when that actually changes folders. Spam files to Junk unless already in
    /// Junk; ham rescues to Inbox only from Junk, and otherwise just trains in
    /// place. Relocating into the folder you're already viewing would
    /// optimistically remove the row, then have it reappear on refresh.
    fn mark_selected_spam(&mut self, is_spam: bool) {
        let Some(email) = self.emails.get(self.cursor) else {
            return;
        };
        let _ = self.cmd_tx.send(BackendCommand::TrainMessage {
            origin: "email_list".to_string(),
            id: email.id.clone(),
            spam: is_spam,
        });
        if is_spam {
            if !self.current_mailbox_has_role("junk") {
                self.move_selected_to_folder("junk", "Mark spam");
            }
        } else if self.current_mailbox_has_role("junk") {
            self.move_selected_to_folder("inbox", "Mark not-spam");
        }
    }

    fn move_selected_to_folder(&mut self, folder: &str, action_label: &str) {
        let Some(target_id) = rules::resolve_mailbox_id(folder, &self.mailboxes) else {
            self.status_message = Some(format!(
                "{} failed: could not resolve folder '{}'",
                action_label, folder
            ));
            return;
        };

        let Some(email) = self.emails.get(self.cursor).cloned() else {
            return;
        };
        let op_id = self.next_op_id();
        let from_index = self.cursor;
        self.pending_write_ops.insert(
            op_id,
            PendingWriteOp::Move {
                email: Box::new(email.clone()),
                from_index,
            },
        );

        // Always move only this single email (not the whole thread) so that
        // archive/delete/move in the email list only affect the current folder.
        let send_result = self.cmd_tx.send(BackendCommand::MoveEmail {
            op_id,
            id: email.id.clone(),
            to_mailbox_id: target_id,
        });

        self.emails.remove(from_index);
        if self.cursor >= self.emails.len() && self.cursor > 0 {
            self.cursor -= 1;
        }
        if let Some(ref mut total) = self.total {
            *total = total.saturating_sub(1);
        }
        if let Err(e) = send_result {
            self.record_send_failure(
                op_id,
                PendingWriteOp::Move {
                    email: Box::new(email),
                    from_index,
                },
                action_label,
                e.to_string(),
            );
        }
    }

    fn expire_selected_now(&mut self) {
        let Some(email) = self.emails.get(self.cursor).cloned() else {
            return;
        };
        let op_id = self.next_op_id();
        let from_index = self.cursor;
        self.pending_write_ops.insert(
            op_id,
            PendingWriteOp::Move {
                email: Box::new(email.clone()),
                from_index,
            },
        );
        // Always destroy only this single email (not the whole thread) so that
        // expire in the email list only affects the current folder.
        let send_result = self.cmd_tx.send(BackendCommand::DestroyEmail {
            op_id,
            id: email.id.clone(),
        });
        self.emails.remove(from_index);
        if self.cursor >= self.emails.len() && self.cursor > 0 {
            self.cursor -= 1;
        }
        if let Some(ref mut total) = self.total {
            *total = total.saturating_sub(1);
        }
        if let Err(e) = send_result {
            self.record_send_failure(
                op_id,
                PendingWriteOp::Move {
                    email: Box::new(email),
                    from_index,
                },
                "Expire",
                e.to_string(),
            );
        }
    }
}

impl View for EmailListView {
    fn scene(&self) -> Scene<'_> {
        let (labels, keys) = if self.search_mode {
            (SEARCH_LABELS, SEARCH_KEYS)
        } else if self.move_mode {
            (MOVE_LABELS, MOVE_KEYS)
        } else {
            (LIST_LABELS, LIST_KEYS)
        };
        Scene {
            title: self.title(),
            labels,
            keys,
            entry: self.search_mode.then(|| Entry {
                placeholder: "Search",
                text: &self.search_input,
            }),
            body: self.body(),
            status: self.status(),
        }
    }

    fn handle_key(&mut self, key: Key, page: usize) -> ViewAction {
        // Search mode: capture text input
        if self.search_mode {
            match key {
                Key::Enter => {
                    self.search_mode = false;
                    if self.search_input.is_empty() {
                        // Empty search clears active search
                        self.active_search = None;
                    } else {
                        self.active_search = Some(self.search_input.clone());
                    }
                    self.search_input.clear();
                    self.request_refresh("email_list.search_submit");
                }
                Key::Escape => {
                    self.search_mode = false;
                    self.search_input.clear();
                }
                Key::Backspace => {
                    self.search_input.pop();
                }
                Key::Char(c) => {
                    self.search_input.push(c);
                }
                _ => {}
            }
            return ViewAction::Continue;
        }

        // Move mode: mailbox picker
        if self.move_mode {
            match key {
                Key::Escape | Key::Char('q') => {
                    self.move_mode = false;
                }
                Key::Char('n') | Key::Char('j') | Key::Down => {
                    if !self.mailboxes.is_empty() && self.move_cursor + 1 < self.mailboxes.len() {
                        self.move_cursor += 1;
                    }
                }
                Key::Char('p') | Key::Char('k') | Key::Up => {
                    if self.move_cursor > 0 {
                        self.move_cursor -= 1;
                    }
                }
                Key::Enter => {
                    if let Some(target_id) =
                        self.mailboxes.get(self.move_cursor).map(|m| m.id.clone())
                    {
                        if let Some(email) = self.emails.get(self.cursor).cloned() {
                            let op_id = self.next_op_id();
                            let from_index = self.cursor;
                            self.pending_write_ops.insert(
                                op_id,
                                PendingWriteOp::Move {
                                    email: Box::new(email.clone()),
                                    from_index,
                                },
                            );
                            let send_result = self.cmd_tx.send(BackendCommand::MoveEmail {
                                op_id,
                                id: email.id.clone(),
                                to_mailbox_id: target_id,
                            });
                            self.emails.remove(from_index);
                            if self.cursor >= self.emails.len() && self.cursor > 0 {
                                self.cursor -= 1;
                            }
                            if let Some(ref mut total) = self.total {
                                *total = total.saturating_sub(1);
                            }
                            if let Err(e) = send_result {
                                self.record_send_failure(
                                    op_id,
                                    PendingWriteOp::Move {
                                        email: Box::new(email),
                                        from_index,
                                    },
                                    "Move",
                                    e.to_string(),
                                );
                            }
                        }
                        self.move_mode = false;
                    }
                }
                Key::ScrollUp => {
                    if self.move_cursor > 0 {
                        self.move_cursor -= 1;
                    }
                }
                Key::ScrollDown if self.move_cursor + 1 < self.mailboxes.len() => {
                    self.move_cursor += 1;
                }
                // A press on a mailbox picks it out; moving there is its own
                // key, so a misdirected press costs nothing.
                Key::Click(index) if index < self.mailboxes.len() => {
                    self.move_cursor = index;
                }
                _ => {}
            }
            return ViewAction::Continue;
        }

        // Normal mode
        match key {
            Key::Char('q') => ViewAction::Pop,
            Key::Char('n') | Key::Char('j') | Key::Down => {
                if !self.emails.is_empty() && self.cursor + 1 < self.emails.len() {
                    self.cursor += 1;
                } else {
                    self.request_load_more();
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
                if !self.emails.is_empty() {
                    self.cursor = (self.cursor + page).min(self.emails.len() - 1);
                    if self.cursor + 1 >= self.emails.len() {
                        self.request_load_more();
                    }
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
                if !self.emails.is_empty() {
                    self.cursor = self.emails.len() - 1;
                    self.request_load_more();
                }
                ViewAction::Continue
            }
            Key::Enter => self.open_selected().unwrap_or(ViewAction::Continue),
            Key::Char('t') => self.open_thread_list(false).unwrap_or(ViewAction::Continue),
            Key::Char('T') => self.open_thread_list(true).unwrap_or(ViewAction::Continue),
            Key::Char('g') => {
                self.request_refresh("email_list.key_g");
                ViewAction::Continue
            }
            Key::Char('R') => {
                if let Some(email) = self.emails.get(self.cursor) {
                    self.pending_reply_request = Some((email.id.clone(), true));
                    if let Err(e) = self.cmd_tx.send(BackendCommand::GetEmailForReply {
                        id: email.id.clone(),
                    }) {
                        self.pending_reply_request = None;
                        self.status_message = Some(format!("Reply all failed to send: {}", e));
                    } else {
                        self.status_message = Some("Preparing reply-all draft...".to_string());
                    }
                }
                ViewAction::Continue
            }
            Key::Char('r') => {
                if let Some(email) = self.emails.get(self.cursor) {
                    self.pending_reply_request = Some((email.id.clone(), false));
                    if let Err(e) = self.cmd_tx.send(BackendCommand::GetEmailForReply {
                        id: email.id.clone(),
                    }) {
                        self.pending_reply_request = None;
                        self.status_message = Some(format!("Reply failed to send: {}", e));
                    } else {
                        self.status_message = Some("Preparing reply draft...".to_string());
                    }
                }
                ViewAction::Continue
            }
            Key::Char('e') => {
                let mailbox_id = self.mailbox_id.clone();
                let mailbox_name = self.mailbox_name.clone();
                if let Err(e) = self.cmd_tx.send(BackendCommand::PreviewRulesForMailbox {
                    origin: "email_list.key_e_dry_run".to_string(),
                    mailbox_id,
                    mailbox_name: mailbox_name.clone(),
                }) {
                    self.status_message = Some(format!("Rules dry-run failed to send: {}", e));
                } else {
                    self.status_message =
                        Some(format!("Running rules dry-run in '{}'", mailbox_name));
                }
                ViewAction::Continue
            }
            Key::Char('E') => {
                let mailbox_id = self.mailbox_id.clone();
                let mailbox_name = self.mailbox_name.clone();
                if let Err(e) = self.cmd_tx.send(BackendCommand::RunRulesForMailbox {
                    origin: "email_list.key_E_run_rules".to_string(),
                    mailbox_id,
                    mailbox_name: mailbox_name.clone(),
                }) {
                    self.status_message = Some(format!("Run rules failed to send: {}", e));
                } else {
                    self.status_message = Some(format!("Running rules in '{}'", mailbox_name));
                }
                ViewAction::Continue
            }
            Key::Char('f') => {
                if let Some(email) = self.emails.get(self.cursor) {
                    let email_id = email.id.clone();
                    let old_flagged = email.keywords.contains_key("$flagged");
                    let new_flagged = !old_flagged;
                    let op_id = self.next_op_id();
                    self.pending_write_ops.insert(
                        op_id,
                        PendingWriteOp::Flag {
                            email_id: email_id.clone(),
                            old_flagged,
                        },
                    );
                    self.set_email_flag_state(&email_id, new_flagged);
                    if let Err(e) = self.cmd_tx.send(BackendCommand::SetEmailFlagged {
                        op_id,
                        id: email_id.clone(),
                        flagged: new_flagged,
                    }) {
                        self.record_send_failure(
                            op_id,
                            PendingWriteOp::Flag {
                                email_id,
                                old_flagged,
                            },
                            "Flag update",
                            e.to_string(),
                        );
                    }
                }
                ViewAction::Continue
            }
            Key::Char('u') => {
                if let Some(email) = self.emails.get(self.cursor) {
                    let email_id = email.id.clone();
                    let old_seen = email.keywords.contains_key("$seen");
                    let new_seen = !old_seen;
                    let marking_read = new_seen;
                    let op_id = self.next_op_id();
                    self.pending_write_ops.insert(
                        op_id,
                        PendingWriteOp::Seen {
                            email_id: email_id.clone(),
                            old_seen,
                        },
                    );
                    self.set_email_seen_state(&email_id, new_seen);
                    let send_result = if new_seen {
                        self.cmd_tx.send(BackendCommand::MarkEmailRead {
                            op_id,
                            id: email_id.clone(),
                        })
                    } else {
                        self.cmd_tx.send(BackendCommand::MarkEmailUnread {
                            op_id,
                            id: email_id.clone(),
                        })
                    };
                    if let Err(e) = send_result {
                        self.record_send_failure(
                            op_id,
                            PendingWriteOp::Seen { email_id, old_seen },
                            "Read state update",
                            e.to_string(),
                        );
                    }
                    // When marking as read, advance to the next unread email
                    if marking_read {
                        // Scan down (older items) first, then up (newer ones):
                        // the nearest unread below, else the nearest above.
                        let next_unread = self
                            .emails
                            .iter()
                            .enumerate()
                            .skip(self.cursor + 1)
                            .find(|(_, email)| Self::is_unread(email))
                            .or_else(|| {
                                self.emails
                                    .iter()
                                    .enumerate()
                                    .take(self.cursor)
                                    .rfind(|(_, email)| Self::is_unread(email))
                            })
                            .map(|(i, _)| i);
                        if let Some(i) = next_unread {
                            self.cursor = i;
                        }
                    }
                }
                ViewAction::Continue
            }
            Key::Char('m') => {
                if !self.emails.is_empty() && !self.mailboxes.is_empty() {
                    self.move_mode = true;
                    self.move_cursor = 0;
                }
                ViewAction::Continue
            }
            Key::Char('a') => {
                let target = self.archive_folder.clone();
                self.move_selected_to_folder(&target, "Archive");
                ViewAction::Continue
            }
            Key::Char('d') => {
                let target = self.deleted_folder.clone();
                self.move_selected_to_folder(&target, "Delete");
                ViewAction::Continue
            }
            Key::Char('J') => {
                self.mark_selected_spam(true);
                ViewAction::Continue
            }
            Key::Char('H') => {
                self.mark_selected_spam(false);
                ViewAction::Continue
            }
            Key::Char('S') => {
                if let Some(email) = self.emails.get(self.cursor) {
                    let _ = self.cmd_tx.send(BackendCommand::ClassifyMessage {
                        origin: "email_list".to_string(),
                        id: email.id.clone(),
                    });
                    self.status_message = Some("Scoring message...".to_string());
                }
                ViewAction::Continue
            }
            Key::Char('D') => {
                if self.is_in_deleted_folder() {
                    self.expire_selected_now();
                } else {
                    self.status_message =
                        Some("Expire is only available in the deleted folder".to_string());
                }
                ViewAction::Continue
            }
            Key::Char('s') => {
                self.search_mode = true;
                self.search_input.clear();
                ViewAction::Continue
            }
            Key::Char('l') => {
                self.request_load_more();
                ViewAction::Continue
            }
            Key::Escape => {
                if self.active_search.is_some() {
                    self.active_search = None;
                    self.request_refresh("email_list.clear_search_escape");
                }
                ViewAction::Continue
            }
            Key::Char('c') => {
                let draft = compose::build_compose_draft(&self.reply_from_address);
                ViewAction::Compose(draft.into())
            }
            Key::Char('?') => ViewAction::Push(Box::new(HelpView::new())),
            Key::ScrollUp => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                }
                ViewAction::Continue
            }
            Key::ScrollDown => {
                if !self.emails.is_empty() && self.cursor + 1 < self.emails.len() {
                    self.cursor += 1;
                } else {
                    self.request_load_more();
                }
                ViewAction::Continue
            }
            // A press selects the message; opening it is the pending
            // action, so the selection is shown before the message loads.
            Key::Click(index) => {
                if index < self.emails.len() {
                    self.cursor = index;
                    self.pending_click = true;
                }
                ViewAction::Continue
            }
            _ => ViewAction::Continue,
        }
    }

    fn take_pending_action(&mut self) -> Option<ViewAction> {
        if let Some(draft) = self.pending_compose.take() {
            return Some(ViewAction::Compose(draft.into()));
        }
        if let Some((mailbox_name, preview)) = self.pending_rules_preview.take() {
            return Some(ViewAction::Push(Box::new(RulesPreviewView::new(
                mailbox_name,
                preview,
            ))));
        }
        if self.pending_click {
            self.pending_click = false;
            return self.open_selected();
        }
        None
    }

    fn on_response(&mut self, response: &BackendResponse) -> bool {
        match response {
            BackendResponse::Emails {
                mailbox_id,
                emails,
                total,
                position,
                loaded,
                thread_counts,
            } if *mailbox_id == self.mailbox_id => {
                self.loading = false;
                self.loading_more = false;
                self.total = *total;
                self.last_loaded_count = *loaded;
                self.next_query_position = position.saturating_add(*loaded);
                match emails {
                    Ok(emails) => {
                        self.last_refreshed = Some(SystemTime::now());
                        if *position == 0 {
                            // Collect IDs of emails with in-flight move/destroy
                            // ops so we can filter them out of incoming data.
                            // These emails were optimistically removed and must
                            // stay hidden until the backend confirms or rejects.
                            let inflight_move_ids: HashSet<&str> = self
                                .pending_write_ops
                                .values()
                                .filter_map(|op| match op {
                                    PendingWriteOp::Move { email, .. } => Some(email.id.as_str()),
                                    _ => None,
                                })
                                .collect();
                            if inflight_move_ids.is_empty() {
                                self.emails = emails.clone();
                                self.pending_write_ops.clear();
                            } else {
                                self.emails = emails
                                    .iter()
                                    .filter(|e| !inflight_move_ids.contains(e.id.as_str()))
                                    .cloned()
                                    .collect();
                                // Only clear flag/seen ops — move ops are still
                                // in flight and must be kept for rollback.
                                self.pending_write_ops
                                    .retain(|_, op| matches!(op, PendingWriteOp::Move { .. }));
                            }
                            self.thread_counts = thread_counts.clone();
                        } else {
                            self.thread_counts
                                .extend(thread_counts.iter().map(|(k, v)| (k.clone(), *v)));
                            let mut existing_ids: HashSet<String> =
                                self.emails.iter().map(|e| e.id.clone()).collect();
                            for email in emails {
                                if existing_ids.insert(email.id.clone()) {
                                    self.emails.push(email.clone());
                                }
                            }
                        }
                        self.error = None;
                        if self.cursor >= self.emails.len() && !self.emails.is_empty() {
                            self.cursor = self.emails.len() - 1;
                        }
                    }
                    Err(e) => {
                        if *position == 0 {
                            self.error = Some(format!("Failed to fetch emails: {}", e));
                        } else {
                            self.status_message = Some(format!("Load more failed: {}", e));
                        }
                    }
                }
                true
            }
            BackendResponse::EmailMutation {
                op_id,
                id: _,
                action,
                result,
            } => {
                if let Some(pending) = self.pending_write_ops.remove(op_id) {
                    match result {
                        Ok(()) => {
                            // No refresh needed for any mutation type: optimistic
                            // update + cache update already show the correct state.
                            // The user can press 'g' to refresh manually. Auto-
                            // refreshing after moves raced with in-flight mutations
                            // and overwrote optimistic state with stale server data.
                        }
                        Err(e) => {
                            self.rollback_pending_write(pending);
                            let action_label = match action {
                                EmailMutationAction::MarkRead => "Mark read",
                                EmailMutationAction::MarkUnread => "Mark unread",
                                EmailMutationAction::SetFlagged(_) => "Flag update",
                                EmailMutationAction::Move => "Move",
                                EmailMutationAction::Destroy => "Expire",
                            };
                            self.status_message = Some(format!("{} failed: {}", action_label, e));
                        }
                    }
                    true
                } else {
                    false
                }
            }
            BackendResponse::EmailForReply { id, result } => {
                // Taken only when it is the request this reply answers, which
                // makes the match and the take one step with nothing to unwrap.
                if let Some((_, reply_all)) = self
                    .pending_reply_request
                    .take_if(|(pending_id, _)| pending_id.as_str() == id.as_str())
                {
                    match result.as_ref() {
                        Ok(email) => {
                            let draft = compose::build_reply_draft(
                                email,
                                reply_all,
                                &self.reply_from_address,
                            );
                            self.pending_compose = Some(draft);
                        }
                        Err(e) => {
                            let action = if reply_all { "Reply all" } else { "Reply" };
                            self.status_message = Some(format!("{} failed: {}", action, e));
                        }
                    }
                    true
                } else {
                    false
                }
            }
            BackendResponse::ThreadMarkedRead { result, .. } => {
                if result.is_ok() {
                    self.request_refresh("email_list.thread_marked_read");
                    true
                } else {
                    false
                }
            }
            BackendResponse::RulesDryRun {
                mailbox_id,
                mailbox_name,
                result,
            } if *mailbox_id == self.mailbox_id => {
                match result {
                    Ok(preview) => {
                        self.status_message = Some(format!(
                            "Rules dry-run in '{}': scanned {}, matched {}, actions {}",
                            mailbox_name, preview.scanned, preview.matched_rules, preview.actions
                        ));
                        self.pending_rules_preview = Some((mailbox_name.clone(), preview.clone()));
                    }
                    Err(e) => {
                        self.status_message = Some(format!("Rules dry-run failed: {}", e));
                    }
                }
                true
            }
            BackendResponse::RulesRun {
                mailbox_id,
                mailbox_name,
                result,
            } if *mailbox_id == self.mailbox_id => {
                match result {
                    Ok(summary) => {
                        self.status_message = Some(format!(
                            "Rules run in '{}': scanned {}, matched {}, actions {}",
                            mailbox_name, summary.scanned, summary.matched_rules, summary.actions
                        ));
                        self.request_refresh("email_list.rules_run_followup");
                    }
                    Err(e) => {
                        self.status_message = Some(format!("Rules run failed: {}", e));
                    }
                }
                true
            }
            BackendResponse::MessageTrained { spam, result, .. } => {
                let label = if *spam { "spam" } else { "not-spam" };
                self.status_message = Some(match result {
                    Ok(()) => format!("Trained classifier: marked as {}", label),
                    Err(e) => format!("Training as {} failed: {}", label, e),
                });
                true
            }
            BackendResponse::MessageClassified { id, result } => {
                match result {
                    Ok((score, verdict)) => {
                        self.spam_verdicts.insert(id.clone(), verdict.clone());
                        self.status_message =
                            Some(format!("Spam score: {:.3} -> {}", score, verdict));
                    }
                    Err(e) => {
                        self.status_message = Some(format!("Classify failed: {}", e));
                    }
                }
                true
            }
            _ => false,
        }
    }

    fn trigger_idle_sync(&mut self) -> bool {
        if self.loading || self.move_mode || self.search_mode {
            return false;
        }
        self.request_refresh("email_list.idle_sync");
        true
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic, clippy::expect_used, clippy::unwrap_used)]
    use super::*;
    use crate::jmap::types::{Email, EmailAddress, Mailbox};

    fn make_email(id: &str, thread_id: &str) -> Email {
        Email {
            id: id.to_string(),
            thread_id: Some(thread_id.to_string()),
            from: None,
            to: None,
            cc: None,
            reply_to: None,
            subject: Some(format!("Subject {}", id)),
            received_at: Some("2025-01-01".to_string()),
            sent_at: None,
            preview: None,
            text_body: None,
            html_body: None,
            body_values: HashMap::new(),
            keywords: HashMap::new(),
            mailbox_ids: HashMap::new(),
            message_id: None,
            references: None,
            attachments: None,
            extra: HashMap::new(),
        }
    }

    fn make_mailboxes() -> Vec<Mailbox> {
        vec![
            Mailbox {
                id: "mbox-inbox".to_string(),
                name: "Inbox".to_string(),
                parent_id: None,
                role: Some("inbox".to_string()),
                total_emails: 10,
                unread_emails: 2,
                sort_order: 0,
            },
            Mailbox {
                id: "mbox-archive".to_string(),
                name: "Archive".to_string(),
                parent_id: None,
                role: Some("archive".to_string()),
                total_emails: 100,
                unread_emails: 0,
                sort_order: 0,
            },
            Mailbox {
                id: "mbox-trash".to_string(),
                name: "Trash".to_string(),
                parent_id: None,
                role: Some("trash".to_string()),
                total_emails: 5,
                unread_emails: 0,
                sort_order: 0,
            },
        ]
    }

    fn make_view() -> (EmailListView, mpsc::Receiver<BackendCommand>) {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let mailboxes = make_mailboxes();
        let mut view = EmailListView::new(
            cmd_tx,
            "me@example.com".to_string(),
            "mbox-inbox".to_string(),
            "Inbox".to_string(),
            50,
            mailboxes,
            "Archive".to_string(),
            "Trash".to_string(),
            None,
        );
        view.loading = false;

        // Add emails: two in the same thread, one standalone
        let e1 = make_email("email-1", "thread-A");
        let e2 = make_email("email-2", "thread-A");
        let e3 = make_email("email-3", "thread-B");
        view.emails = vec![e1, e2, e3];
        view.total = Some(3);
        // Mark thread-A as having 2 emails (so it's a multi-email thread)
        view.thread_counts.insert("thread-A".to_string(), (0, 2));
        view.thread_counts.insert("thread-B".to_string(), (0, 1));

        (view, cmd_rx)
    }

    /// Every row the scene's list names, in order.
    fn list_rows(scene: &Scene<'_>) -> Vec<Row> {
        match &scene.body {
            Body::List { total, row, .. } => (0..*total).map(|index| row(index)).collect(),
            Body::Text { .. } => panic!("the scene shows a list"),
        }
    }

    /// What the scene's text body says, wrapped for a wide pane.
    fn message(scene: &Scene<'_>) -> String {
        match &scene.body {
            Body::Text { text, .. } => text(60),
            Body::List { .. } => panic!("the scene shows a text"),
        }
    }

    #[test]
    fn archive_sends_move_email_not_move_thread() {
        let (mut view, cmd_rx) = make_view();
        // Cursor is on email-1, which is in thread-A (2 emails in thread)
        view.cursor = 0;

        view.handle_key(Key::Char('a'), 24);

        // Drain any QueryEmails from constructor, then find MoveEmail
        let mut found_move_email = false;
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                BackendCommand::MoveEmail {
                    id, to_mailbox_id, ..
                } => {
                    assert_eq!(id, "email-1");
                    assert_eq!(to_mailbox_id, "mbox-archive");
                    found_move_email = true;
                }
                BackendCommand::MoveThread { .. } => {
                    panic!("archive should send MoveEmail, not MoveThread");
                }
                _ => {}
            }
        }
        assert!(found_move_email, "expected MoveEmail command for archive");
    }

    #[test]
    fn delete_sends_move_email_not_move_thread() {
        let (mut view, cmd_rx) = make_view();
        view.cursor = 0;

        view.handle_key(Key::Char('d'), 24);

        let mut found_move_email = false;
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                BackendCommand::MoveEmail {
                    id, to_mailbox_id, ..
                } => {
                    assert_eq!(id, "email-1");
                    assert_eq!(to_mailbox_id, "mbox-trash");
                    found_move_email = true;
                }
                BackendCommand::MoveThread { .. } => {
                    panic!("delete should send MoveEmail, not MoveThread");
                }
                _ => {}
            }
        }
        assert!(found_move_email, "expected MoveEmail command for delete");
    }

    #[test]
    fn move_mode_sends_move_email_not_move_thread() {
        let (mut view, cmd_rx) = make_view();
        view.cursor = 0;

        // Enter move mode
        view.handle_key(Key::Char('m'), 24);
        assert!(view.move_mode);

        // Select the second mailbox (Archive) and confirm
        view.handle_key(Key::Char('n'), 24);
        view.handle_key(Key::Enter, 24);

        let mut found_move_email = false;
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                BackendCommand::MoveEmail {
                    id, to_mailbox_id, ..
                } => {
                    assert_eq!(id, "email-1");
                    assert_eq!(to_mailbox_id, "mbox-archive");
                    found_move_email = true;
                }
                BackendCommand::MoveThread { .. } => {
                    panic!("move should send MoveEmail, not MoveThread");
                }
                _ => {}
            }
        }
        assert!(found_move_email, "expected MoveEmail command for move");
    }

    #[test]
    fn expire_sends_destroy_email_not_destroy_thread() {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let mailboxes = make_mailboxes();
        let mut view = EmailListView::new(
            cmd_tx,
            "me@example.com".to_string(),
            "mbox-trash".to_string(),
            "Trash".to_string(),
            50,
            mailboxes,
            "Archive".to_string(),
            "Trash".to_string(),
            None,
        );
        view.loading = false;

        let e1 = make_email("email-1", "thread-A");
        view.emails = vec![e1];
        view.total = Some(1);
        view.thread_counts.insert("thread-A".to_string(), (0, 3));
        view.cursor = 0;

        view.handle_key(Key::Char('D'), 24);

        let mut found_destroy_email = false;
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                BackendCommand::DestroyEmail { id, .. } => {
                    assert_eq!(id, "email-1");
                    found_destroy_email = true;
                }
                BackendCommand::DestroyThread { .. } => {
                    panic!("expire should send DestroyEmail, not DestroyThread");
                }
                _ => {}
            }
        }
        assert!(
            found_destroy_email,
            "expected DestroyEmail command for expire"
        );
    }

    #[test]
    fn status_bar_shows_server_total_not_loaded_count() {
        let (mut view, _cmd_rx) = make_view();
        // Simulate: server says 150 total but only 3 loaded
        view.total = Some(150);
        assert_eq!(view.emails.len(), 3);

        assert!(
            view.scene().status.starts_with("1/150 |"),
            "Y should be server total (150), not loaded count (3)"
        );
    }

    #[test]
    fn status_bar_falls_back_to_loaded_count_when_no_total() {
        let (mut view, _cmd_rx) = make_view();
        view.total = None;

        assert!(
            view.scene().status.starts_with("1/3 |"),
            "Y should fall back to loaded count when total is None"
        );
    }

    #[test]
    fn archive_decrements_total() {
        let (mut view, _cmd_rx) = make_view();
        view.total = Some(10);
        view.cursor = 0;

        view.handle_key(Key::Char('a'), 24);

        assert_eq!(view.total, Some(9), "total should decrement after archive");
        assert_eq!(view.emails.len(), 2, "email should be removed from list");
    }

    #[test]
    fn delete_decrements_total() {
        let (mut view, _cmd_rx) = make_view();
        view.total = Some(10);
        view.cursor = 0;

        view.handle_key(Key::Char('d'), 24);

        assert_eq!(view.total, Some(9), "total should decrement after delete");
        assert_eq!(view.emails.len(), 2, "email should be removed from list");
    }

    fn make_email_with_seen(id: &str, seen: bool) -> Email {
        let mut email = make_email(id, &format!("thread-{}", id));
        if seen {
            email.keywords.insert("$seen".to_string(), true);
        }
        email
    }

    #[test]
    fn mark_read_advances_to_next_unread_below() {
        let (mut view, _cmd_rx) = make_view();
        // Set up: [unread, read, unread, read, unread]
        view.emails = vec![
            make_email_with_seen("e1", false),
            make_email_with_seen("e2", true),
            make_email_with_seen("e3", false),
            make_email_with_seen("e4", true),
            make_email_with_seen("e5", false),
        ];
        view.cursor = 0;

        // Mark e1 as read, should advance to e3 (next unread below)
        view.handle_key(Key::Char('u'), 24);
        assert_eq!(view.cursor, 2, "should advance to next unread below (e3)");
    }

    #[test]
    fn mark_read_scans_up_when_no_unread_below() {
        let (mut view, _cmd_rx) = make_view();
        // Set up: [unread, read, unread, read]
        view.emails = vec![
            make_email_with_seen("e1", false),
            make_email_with_seen("e2", true),
            make_email_with_seen("e3", false),
            make_email_with_seen("e4", true),
        ];
        view.cursor = 2;

        // Mark e3 as read; no unread below, should scan up to e1
        view.handle_key(Key::Char('u'), 24);
        assert_eq!(view.cursor, 0, "should scan up to unread above (e1)");
    }

    #[test]
    fn mark_read_stays_when_no_unread_anywhere() {
        let (mut view, _cmd_rx) = make_view();
        // Set up: [read, unread, read]
        view.emails = vec![
            make_email_with_seen("e1", true),
            make_email_with_seen("e2", false),
            make_email_with_seen("e3", true),
        ];
        view.cursor = 1;

        // Mark e2 as read; no other unread emails, cursor stays
        view.handle_key(Key::Char('u'), 24);
        assert_eq!(view.cursor, 1, "should stay on same email when no unread");
    }

    #[test]
    fn mark_unread_does_not_move_cursor() {
        let (mut view, _cmd_rx) = make_view();
        // Set up: [read, read, unread]
        view.emails = vec![
            make_email_with_seen("e1", true),
            make_email_with_seen("e2", true),
            make_email_with_seen("e3", false),
        ];
        view.cursor = 0;

        // Mark e1 as unread (it's already read), cursor should not move
        view.handle_key(Key::Char('u'), 24);
        assert_eq!(view.cursor, 0, "marking unread should not move cursor");
    }

    fn make_long_view() -> (EmailListView, mpsc::Receiver<BackendCommand>) {
        let (mut view, cmd_rx) = make_view();
        view.emails = (0..20)
            .map(|i| make_email(&format!("email-{i}"), &format!("thread-{i}")))
            .collect();
        view.total = Some(20);
        view.cursor = 0;
        (view, cmd_rx)
    }

    /// The list keeps the selection in view itself, so navigating only has
    /// to move the cursor the scene selects with.
    #[test]
    fn navigating_down_moves_the_cursor_the_list_selects() {
        let (mut view, _cmd_rx) = make_long_view();

        for _ in 0..8 {
            view.handle_key(Key::Char('n'), 12);
        }

        assert_eq!(view.cursor, 8);
        let scene = view.scene();
        match &scene.body {
            Body::List {
                total, selected, ..
            } => assert_eq!((*total, *selected), (20, 8)),
            Body::Text { .. } => panic!("the messages are a list"),
        }
    }

    #[test]
    fn navigating_up_moves_the_cursor_back() {
        let (mut view, _cmd_rx) = make_long_view();
        view.cursor = 8;

        view.handle_key(Key::Char('p'), 12);
        view.handle_key(Key::Char('p'), 12);

        assert_eq!(view.cursor, 6);
    }

    #[test]
    fn the_page_keys_move_by_the_rows_the_body_shows() {
        let (mut view, _cmd_rx) = make_long_view();

        view.handle_key(Key::PageDown, 5);
        assert_eq!(view.cursor, 5);
        view.handle_key(Key::PageDown, 5);
        assert_eq!(view.cursor, 10);
        view.handle_key(Key::PageUp, 5);
        assert_eq!(view.cursor, 5);
        view.handle_key(Key::PageDown, 100);
        assert_eq!(view.cursor, 19, "a page past the end stops on the last");
    }

    #[test]
    fn the_scene_titles_the_mailbox_and_names_every_row() {
        let (mut view, _cmd_rx) = make_view();
        let mut flagged = make_email("email-1", "thread-A");
        flagged.keywords.insert("$flagged".to_string(), true);
        flagged.subject = Some("Two\nlines".to_string());
        flagged.from = Some(vec![EmailAddress {
            name: Some("Ada Lovelace".to_string()),
            email: Some("ada@example.com".to_string()),
        }]);
        let mut scored = make_email("email-3", "thread-B");
        scored.keywords.insert("$seen".to_string(), true);
        scored.subject = None;
        scored.received_at = Some("2025-03-04T05:06:07Z".to_string());
        scored.from = Some(vec![EmailAddress {
            name: None,
            email: Some("bob@example.com".to_string()),
        }]);
        view.emails = vec![flagged, scored];
        view.total = Some(2);
        view.spam_verdicts
            .insert("email-3".to_string(), "spam".to_string());

        let scene = view.scene();
        assert_eq!(scene.title, "Inbox (2 messages)");
        assert_eq!(scene.labels, LIST_LABELS);
        assert_eq!(scene.keys, LIST_KEYS);
        assert_eq!(scene.labels.len(), scene.keys.len());
        assert!(scene.entry.is_none());
        assert_eq!(
            list_rows(&scene),
            vec![
                Row {
                    // Flagged, one line, and two of the thread's two read.
                    label: "F Two lines [2/2]".to_string(),
                    meta: "Ada Lovelace · 2025-01-01".to_string(),
                    marked: true,
                },
                Row {
                    // Scored spam, no subject, and a thread of its own.
                    label: "S (no subject)".to_string(),
                    meta: "bob@example.com · 2025-03-04".to_string(),
                    marked: false,
                },
            ]
        );
    }

    #[test]
    fn the_title_says_what_was_searched_for_and_when_it_was_refreshed() {
        let (mut view, _cmd_rx) = make_view();
        view.active_search = Some("invoice".to_string());
        view.total = Some(7);
        assert_eq!(view.scene().title, "Inbox [search: invoice] (7 results)");
        view.total = None;
        assert_eq!(view.scene().title, "Inbox [search: invoice]");

        view.active_search = None;
        view.last_refreshed = Some(SystemTime::UNIX_EPOCH);
        let title = view.scene().title;
        assert!(title.starts_with("Inbox (refreshed "), "{title}");
    }

    #[test]
    fn a_press_on_a_row_selects_it_and_opens_it() {
        let (mut view, _cmd_rx) = make_view();

        view.handle_key(Key::Click(2), 10);
        assert_eq!(view.cursor, 2);
        assert!(view.pending_click);
        assert!(matches!(
            view.take_pending_action(),
            Some(ViewAction::Push(_))
        ));
        assert!(!view.pending_click, "the press is spent on the one open");

        // A press past the last row selects nothing.
        view.handle_key(Key::Click(9), 10);
        assert_eq!(view.cursor, 2);
        assert!(!view.pending_click);
    }

    #[test]
    fn searching_shows_the_entry_over_the_messages() {
        let (mut view, _cmd_rx) = make_view();
        view.handle_key(Key::Char('s'), 10);
        view.handle_key(Key::Char('h'), 10);
        view.handle_key(Key::Char('i'), 10);

        {
            let scene = view.scene();
            let entry = scene.entry.as_ref().expect("the search entry");
            assert_eq!((entry.placeholder, entry.text), ("Search", "hi"));
            assert_eq!(scene.labels, SEARCH_LABELS);
            assert_eq!(scene.keys, SEARCH_KEYS);
            assert_eq!(
                list_rows(&scene).len(),
                3,
                "the messages stay under the entry"
            );
        }

        view.handle_key(Key::Escape, 10);
        assert!(view.scene().entry.is_none());
    }

    #[test]
    fn moving_lists_the_mailboxes_and_a_press_picks_one() {
        let (mut view, _cmd_rx) = make_view();
        view.handle_key(Key::Char('m'), 10);
        view.handle_key(Key::Click(2), 10);

        let scene = view.scene();
        assert_eq!(scene.labels, MOVE_LABELS);
        assert_eq!(scene.keys, MOVE_KEYS);
        assert!(scene.entry.is_none());
        assert_eq!(
            list_rows(&scene)
                .iter()
                .map(|row| row.label.clone())
                .collect::<Vec<_>>(),
            vec!["Inbox", "Archive", "Trash"]
        );
        match &scene.body {
            Body::List { selected, .. } => assert_eq!(*selected, 2),
            Body::Text { .. } => panic!("the mailbox picker is a list"),
        }
        assert!(scene.status.starts_with("Move to mailbox: 3/3 |"));
    }

    #[test]
    fn a_list_with_nothing_to_show_says_so_in_the_pane() {
        let (mut view, _cmd_rx) = make_view();
        view.emails.clear();
        view.total = Some(0);
        assert_eq!(message(&view.scene()), "No messages.");

        view.error = Some("Failed to fetch emails: no route to host".to_string());
        assert_eq!(
            message(&view.scene()),
            "Failed to fetch emails: no route to host"
        );

        view.error = None;
        view.loading = true;
        assert_eq!(message(&view.scene()), "Loading emails...");
        assert_eq!(view.scene().status, "Loading... | q:back");
    }
}

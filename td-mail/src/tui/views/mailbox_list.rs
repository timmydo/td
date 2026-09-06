use crate::backend::{BackendCommand, BackendResponse, RetentionCandidate};
use crate::compose;
use crate::config::RetentionPolicyConfig;
use crate::jmap::types::Mailbox;
use crate::tui::input::Key;
use crate::tui::screen::Terminal;
use crate::tui::views::email_list::{CachedEmailListState, EmailListView};
use crate::tui::views::help::HelpView;
use crate::tui::views::retention_preview::RetentionPreviewView;
use crate::tui::views::{format_system_time, View, ViewAction};
use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::mpsc;
use std::time::SystemTime;

pub struct MailboxListView {
    cmd_tx: mpsc::Sender<BackendCommand>,
    from_address: String,
    reply_from_address: Option<String>,
    browser: Option<String>,
    page_size: u32,
    scrolloff: usize,
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
        scrolloff: usize,
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
            scrolloff,
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

    fn next_account_name(&self) -> Option<String> {
        if self.account_names.len() <= 1 {
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

    fn format_mailbox(m: &Mailbox) -> String {
        if m.unread_emails > 0 {
            format!("{} ({}/{})", m.name, m.unread_emails, m.total_emails)
        } else if m.total_emails > 0 {
            format!("{} ({})", m.name, m.total_emails)
        } else {
            m.name.clone()
        }
    }

    fn scroll_offset_for(&self, max_items: usize) -> usize {
        if max_items == 0 || self.mailboxes.is_empty() {
            return 0;
        }
        let max_offset = self.mailboxes.len().saturating_sub(max_items);
        let margin = self.scrolloff.min(max_items.saturating_sub(1));
        let lower_bound = margin;
        let upper_bound = max_items.saturating_sub(margin + 1);

        // Both bounds scroll to the top: above the margin there is nothing to
        // scroll past, and within the visible window the first row is still
        // the first mailbox.
        if self.cursor < lower_bound || self.cursor <= upper_bound {
            0
        } else {
            (self.cursor - upper_bound).min(max_offset)
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
            self.scrolloff,
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
    fn render(&self, term: &mut Terminal) -> io::Result<()> {
        term.clear()?;

        // Header
        term.move_to(1, 1)?;
        term.set_header()?;
        let header = {
            let title = if self.account_names.len() > 1 {
                format!("tmc - {}", self.current_account)
            } else {
                "tmc - Timmy's Mail Console".to_string()
            };
            if let Some(ts) = self.last_refreshed {
                format!("{} (refreshed {})", title, format_system_time(ts))
            } else {
                title
            }
        };
        term.write_truncated(&header, term.cols)?;
        term.reset_attr()?;

        // Separator
        term.move_to(2, 1)?;
        let sep = "-".repeat(term.cols as usize);
        term.write_str(&sep)?;

        if self.create_mode {
            term.move_to(3, 1)?;
            term.set_header()?;
            term.write_truncated("Create new folder:", term.cols)?;
            term.reset_attr()?;
            term.move_to(4, 1)?;
            let input = format!("Name: {}_", self.create_input);
            term.write_truncated(&input, term.cols)?;
        } else if self.delete_confirm_mode {
            term.move_to(3, 1)?;
            term.set_header()?;
            let name = self
                .mailboxes
                .get(self.cursor)
                .map(|m| m.name.as_str())
                .unwrap_or("(unknown)");
            let prompt = format!("Delete folder '{}'? (y/N)", name);
            term.write_truncated(&prompt, term.cols)?;
            term.reset_attr()?;
            term.move_to(4, 1)?;
            term.write_truncated("Press y to confirm, n or Esc to cancel.", term.cols)?;
        } else if self.loading && self.mailboxes.is_empty() {
            term.move_to(3, 1)?;
            term.write_truncated("Loading mailboxes...", term.cols)?;
        } else if let Some(ref err) = self.error {
            term.move_to(3, 1)?;
            term.write_truncated(err, term.cols)?;
        } else if self.mailboxes.is_empty() {
            term.move_to(3, 1)?;
            term.write_truncated("No mailboxes found.", term.cols)?;
        } else {
            let max_items = (term.rows as usize).saturating_sub(4);
            let scroll_offset = self.scroll_offset_for(max_items);

            for (i, mailbox) in self
                .mailboxes
                .iter()
                .skip(scroll_offset)
                .enumerate()
                .take(max_items)
            {
                let row = 3 + i as u16;
                term.move_to(row, 1)?;

                let display_idx = scroll_offset + i;
                let line = Self::format_mailbox(mailbox);

                if display_idx == self.cursor {
                    term.set_selection()?;
                    if mailbox.unread_emails > 0 {
                        term.set_bold_text()?;
                    }
                } else if mailbox.unread_emails > 0 {
                    term.set_bold_text()?;
                }

                term.write_truncated(&line, term.cols)?;
                term.reset_attr()?;
            }
        }

        // Status bar
        term.move_to(term.rows, 1)?;
        term.set_status()?;
        let account_hint = if self.account_names.len() > 1 {
            " a:account"
        } else {
            ""
        };
        let status = if self.create_mode {
            " New folder name | Enter:create Esc:cancel".to_string()
        } else if self.delete_confirm_mode {
            " Confirm delete | y:delete n/Esc:cancel".to_string()
        } else if self.loading {
            format!(
                " Loading... | q:quit g:refresh c:compose +:new-folder d:delete-folder u:read-all x:preview-expire X:expire{}",
                account_hint
            )
        } else if self.mailboxes.is_empty() {
            format!(
                " q:quit g:refresh c:compose +:new-folder x:preview-expire X:expire{}",
                account_hint
            )
        } else {
            format!(
                " {}/{} | q:quit n/p:navigate RET:open g:refresh c:compose +:new-folder d:delete-folder u:read-all x:preview-expire X:expire ?:help{}",
                self.cursor + 1,
                self.mailboxes.len(),
                account_hint,
            )
        };
        let full_status = if let Some(ref msg) = self.status_message {
            format!("{} | {}", msg, status)
        } else {
            status
        };
        term.write_truncated(&full_status, term.cols)?;
        let remaining = (term.cols as usize).saturating_sub(full_status.len());
        for _ in 0..remaining {
            term.write_str(" ")?;
        }
        term.reset_attr()?;

        term.flush()
    }

    fn handle_key(&mut self, key: Key, term_rows: u16) -> ViewAction {
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
                Key::Escape | Key::Char('q') => {
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

        let page = (term_rows as usize).saturating_sub(4);
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
            Key::MouseClick { row, col: _ } => {
                if row >= 3 && !self.mailboxes.is_empty() {
                    let max_items = (term_rows as usize).saturating_sub(4);
                    let scroll_offset = self.scroll_offset_for(max_items);
                    let clicked = scroll_offset + (row - 3) as usize;
                    if clicked < self.mailboxes.len() {
                        self.cursor = clicked;
                        self.pending_click = true;
                        return ViewAction::Continue;
                    }
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

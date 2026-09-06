use crate::backend::{BackendCommand, BackendResponse, EmailMutationAction};
use crate::compose;
use crate::jmap::types::{Email, Mailbox};
use crate::rules;
use crate::tui::input::Key;
use crate::tui::screen::Terminal;
use crate::tui::views::email_view::{EmailNavEntry, EmailView};
use crate::tui::views::help::HelpView;
use crate::tui::views::{View, ViewAction};
use std::collections::HashMap;
use std::io;
use std::sync::mpsc;

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

pub struct ThreadView {
    cmd_tx: mpsc::Sender<BackendCommand>,
    reply_from_address: String,
    thread_id: String,
    subject: String,
    scrolloff: usize,
    emails: Vec<Email>,
    cursor: usize,
    loading: bool,
    error: Option<String>,
    scroll_offset: usize,
    pending_click: bool,
    status_message: Option<String>,
    next_write_op_id: u64,
    pending_write_ops: HashMap<u64, PendingWriteOp>,
    mailboxes: Vec<Mailbox>,
    archive_folder: String,
    deleted_folder: String,
    can_expire_now: bool,
    /// If set, only show emails in this mailbox (same-folder mode).
    /// If None, show all emails across folders (cross-folder mode).
    filter_mailbox_id: Option<String>,
    browser: Option<String>,
}

impl ThreadView {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cmd_tx: mpsc::Sender<BackendCommand>,
        reply_from_address: String,
        thread_id: String,
        subject: String,
        scrolloff: usize,
        mailboxes: Vec<Mailbox>,
        archive_folder: String,
        deleted_folder: String,
        can_expire_now: bool,
        filter_mailbox_id: Option<String>,
        browser: Option<String>,
    ) -> Self {
        let _ = cmd_tx.send(BackendCommand::QueryThreadEmails {
            thread_id: thread_id.clone(),
        });
        ThreadView {
            cmd_tx,
            reply_from_address,
            thread_id,
            subject,
            scrolloff,
            emails: Vec::new(),
            cursor: 0,
            loading: true,
            error: None,
            scroll_offset: 0,
            pending_click: false,
            status_message: None,
            next_write_op_id: 1,
            pending_write_ops: HashMap::new(),
            mailboxes,
            archive_folder,
            deleted_folder,
            can_expire_now,
            filter_mailbox_id,
            browser,
        }
    }

    fn is_unread(email: &Email) -> bool {
        !email.keywords.contains_key("$seen")
    }

    fn is_flagged(email: &Email) -> bool {
        email.keywords.contains_key("$flagged")
    }

    fn mailbox_name_for_email(&self, email: &Email) -> String {
        for mbox_id in email.mailbox_ids.keys() {
            if let Some(mbox) = self.mailboxes.iter().find(|m| m.id == *mbox_id) {
                return mbox.name.clone();
            }
        }
        "(unknown)".to_string()
    }

    fn filter_emails(emails: &[Email], mailbox_id: &str) -> Vec<Email> {
        emails
            .iter()
            .filter(|e| e.mailbox_ids.contains_key(mailbox_id))
            .cloned()
            .collect()
    }

    fn format_email(email: &Email, width: u16) -> String {
        let unread = if Self::is_unread(email) { "N" } else { " " };
        let flagged = if Self::is_flagged(email) { "F" } else { " " };

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

        let subject = email.subject.as_deref().unwrap_or("(no subject)");

        let date = email
            .received_at
            .as_deref()
            .map(|d| if d.len() >= 10 { &d[..10] } else { d })
            .unwrap_or("");

        let w = width as usize;
        let from_width = 20.min(w.saturating_sub(19));
        let subj_width = w.saturating_sub(19 + from_width);

        let from_display = truncate(from, from_width);
        let subj_display = truncate(subject, subj_width);

        format!(
            " {}{} {} {:from_w$} {}",
            unread,
            flagged,
            date,
            from_display,
            subj_display,
            from_w = from_width
        )
    }

    fn format_email_cross_folder(email: &Email, folder_name: &str, width: u16) -> String {
        let unread = if Self::is_unread(email) { "N" } else { " " };
        let flagged = if Self::is_flagged(email) { "F" } else { " " };

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

        let subject = email.subject.as_deref().unwrap_or("(no subject)");

        let date = email
            .received_at
            .as_deref()
            .map(|d| if d.len() >= 10 { &d[..10] } else { d })
            .unwrap_or("");

        let w = width as usize;
        let folder_width = 12.min(folder_name.len());
        // prefix: " NF YYYY-MM-DD [folder] " = 4 + 10 + 3 + folder_width + 2
        let prefix_len = 19 + folder_width + 2;
        let from_width = 20.min(w.saturating_sub(prefix_len));
        let subj_width = w.saturating_sub(prefix_len + from_width);

        let folder_display = truncate(folder_name, 12);
        let from_display = truncate(from, from_width);
        let subj_display = truncate(subject, subj_width);

        format!(
            " {}{} {} [{}] {:from_w$} {}",
            unread,
            flagged,
            date,
            folder_display,
            from_display,
            subj_display,
            from_w = from_width
        )
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
            }
        }
    }

    fn open_selected(&mut self) -> Option<ViewAction> {
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
            self.can_expire_now,
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
                self.pending_write_ops.remove(&op_id);
                self.set_email_seen_state(&email_id, false);
                self.status_message = Some(format!("Mark read failed: {}", e));
            }
        }
        Some(ViewAction::Push(Box::new(view)))
    }

    fn request_refresh(&mut self) {
        self.loading = true;
        self.scroll_offset = 0;
        let _ = self.cmd_tx.send(BackendCommand::QueryThreadEmails {
            thread_id: self.thread_id.clone(),
        });
    }

    fn adjust_scroll(&mut self, max_items: usize) {
        if max_items == 0 {
            return;
        }
        let max_offset = self.emails.len().saturating_sub(max_items);
        let margin = self.scrolloff.min(max_items.saturating_sub(1));
        let min_cursor = self.scroll_offset.saturating_add(margin);
        let max_cursor = self
            .scroll_offset
            .saturating_add(max_items.saturating_sub(margin + 1));

        if self.cursor < min_cursor {
            self.scroll_offset = self.cursor.saturating_sub(margin);
        } else if self.cursor > max_cursor {
            self.scroll_offset = self
                .cursor
                .saturating_add(margin + 1)
                .saturating_sub(max_items);
        }
        self.scroll_offset = self.scroll_offset.min(max_offset);
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
        let from_index = self.cursor;
        let op_id = self.next_op_id();
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
        if let Err(e) = send_result {
            self.pending_write_ops.remove(&op_id);
            let insert_at = from_index.min(self.emails.len());
            self.emails.insert(insert_at, email);
            self.cursor = insert_at;
            self.status_message = Some(format!("{} failed: {}", action_label, e));
        }
    }

    fn expire_selected_now(&mut self) {
        let Some(email) = self.emails.get(self.cursor).cloned() else {
            return;
        };
        let from_index = self.cursor;
        let op_id = self.next_op_id();
        self.pending_write_ops.insert(
            op_id,
            PendingWriteOp::Move {
                email: Box::new(email.clone()),
                from_index,
            },
        );
        let send_result = self.cmd_tx.send(BackendCommand::DestroyEmail {
            op_id,
            id: email.id.clone(),
        });
        self.emails.remove(from_index);
        if self.cursor >= self.emails.len() && self.cursor > 0 {
            self.cursor -= 1;
        }
        if let Err(e) = send_result {
            self.pending_write_ops.remove(&op_id);
            let insert_at = from_index.min(self.emails.len());
            self.emails.insert(insert_at, email);
            self.cursor = insert_at;
            self.status_message = Some(format!("Expire failed: {}", e));
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else if max <= 3 {
        s.chars().take(max).collect()
    } else {
        let mut end = max - 3;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &s[..end])
    }
}

impl View for ThreadView {
    fn render(&self, term: &mut Terminal) -> io::Result<()> {
        term.clear()?;

        // Header
        term.move_to(1, 1)?;
        term.set_header()?;
        let mode_label = if self.filter_mailbox_id.is_some() {
            "Thread"
        } else {
            "Thread (all folders)"
        };
        let header = format!(
            "{}: {} ({} messages)",
            mode_label,
            self.subject,
            self.emails.len()
        );
        term.write_truncated(&header, term.cols)?;
        term.reset_attr()?;

        // Separator
        term.move_to(2, 1)?;
        let sep = "-".repeat(term.cols as usize);
        term.write_str(&sep)?;

        if self.loading && self.emails.is_empty() {
            term.move_to(3, 1)?;
            term.write_truncated("Loading thread...", term.cols)?;
        } else if let Some(ref err) = self.error {
            term.move_to(3, 1)?;
            term.write_truncated(err, term.cols)?;
        } else if self.emails.is_empty() {
            term.move_to(3, 1)?;
            term.write_truncated("No messages in thread.", term.cols)?;
        } else {
            let max_items = (term.rows as usize).saturating_sub(4);

            for (i, email) in self
                .emails
                .iter()
                .skip(self.scroll_offset)
                .enumerate()
                .take(max_items)
            {
                let row = 3 + i as u16;
                term.move_to(row, 1)?;

                let display_idx = self.scroll_offset + i;
                let line = if self.filter_mailbox_id.is_none() {
                    let folder = self.mailbox_name_for_email(email);
                    Self::format_email_cross_folder(email, &folder, term.cols)
                } else {
                    Self::format_email(email, term.cols)
                };

                if display_idx == self.cursor {
                    term.set_selection()?;
                    if Self::is_unread(email) {
                        term.set_bold_text()?;
                    }
                } else if Self::is_unread(email) {
                    term.set_bold_text()?;
                }

                term.write_truncated(&line, term.cols)?;
                term.reset_attr()?;
            }
        }

        // Status bar
        term.move_to(term.rows, 1)?;
        term.set_status()?;
        let base_status = if self.loading {
            " Loading... | q:back".to_string()
        } else if self.emails.is_empty() {
            " q:back g:refresh".to_string()
        } else {
            let expire_hint = if self.can_expire_now { " D:expire" } else { "" };
            format!(
                " {}/{} | q:back n/p:nav RET:read g:refresh a:archive d:delete{} f:flag u:unread",
                self.cursor + 1,
                self.emails.len(),
                expire_hint
            )
        };
        let status = if let Some(ref msg) = self.status_message {
            format!("{} | {}", msg, base_status)
        } else {
            base_status
        };
        term.write_truncated(&status, term.cols)?;
        let remaining = (term.cols as usize).saturating_sub(status.len());
        for _ in 0..remaining {
            term.write_str(" ")?;
        }
        term.reset_attr()?;

        term.flush()
    }

    fn handle_key(&mut self, key: Key, term_rows: u16) -> ViewAction {
        let max_items = (term_rows as usize).saturating_sub(4);
        let page = max_items;
        match key {
            Key::Char('q') => ViewAction::Pop,
            Key::Char('n') | Key::Char('j') | Key::Down => {
                if !self.emails.is_empty() && self.cursor + 1 < self.emails.len() {
                    self.cursor += 1;
                    self.adjust_scroll(max_items);
                }
                ViewAction::Continue
            }
            Key::Char('p') | Key::Char('k') | Key::Up => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    self.adjust_scroll(max_items);
                }
                ViewAction::Continue
            }
            Key::PageDown => {
                if !self.emails.is_empty() {
                    self.cursor = (self.cursor + page).min(self.emails.len() - 1);
                    self.adjust_scroll(max_items);
                }
                ViewAction::Continue
            }
            Key::PageUp => {
                self.cursor = self.cursor.saturating_sub(page);
                self.adjust_scroll(max_items);
                ViewAction::Continue
            }
            Key::Home => {
                self.cursor = 0;
                self.adjust_scroll(max_items);
                ViewAction::Continue
            }
            Key::End => {
                if !self.emails.is_empty() {
                    self.cursor = self.emails.len() - 1;
                    self.adjust_scroll(max_items);
                }
                ViewAction::Continue
            }
            Key::Enter => self.open_selected().unwrap_or(ViewAction::Continue),
            Key::Char('g') => {
                self.request_refresh();
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
                        self.pending_write_ops.remove(&op_id);
                        self.set_email_flag_state(&email_id, old_flagged);
                        self.status_message = Some(format!("Flag update failed: {}", e));
                    }
                }
                ViewAction::Continue
            }
            Key::Char('u') => {
                if let Some(email) = self.emails.get(self.cursor) {
                    let email_id = email.id.clone();
                    let old_seen = email.keywords.contains_key("$seen");
                    let new_seen = !old_seen;
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
                        self.pending_write_ops.remove(&op_id);
                        self.set_email_seen_state(&email_id, old_seen);
                        self.status_message = Some(format!("Read state update failed: {}", e));
                    }
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
            Key::Char('D') => {
                if self.can_expire_now {
                    self.expire_selected_now();
                } else {
                    self.status_message =
                        Some("Expire is only available in the deleted folder".to_string());
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
                    self.adjust_scroll(max_items);
                }
                ViewAction::Continue
            }
            Key::ScrollDown => {
                if !self.emails.is_empty() && self.cursor + 1 < self.emails.len() {
                    self.cursor += 1;
                    self.adjust_scroll(max_items);
                }
                ViewAction::Continue
            }
            Key::MouseClick { row, col: _ } => {
                if row >= 3 && !self.emails.is_empty() {
                    let clicked = self.scroll_offset + (row - 3) as usize;
                    if clicked < self.emails.len() {
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
            return self.open_selected();
        }
        None
    }

    fn on_response(&mut self, response: &BackendResponse) -> bool {
        match response {
            BackendResponse::ThreadEmails { thread_id, emails } if *thread_id == self.thread_id => {
                self.loading = false;
                match emails {
                    Ok(emails) => {
                        self.emails = if let Some(ref mbox_id) = self.filter_mailbox_id {
                            Self::filter_emails(emails, mbox_id)
                        } else {
                            emails.clone()
                        };
                        self.error = None;
                        self.pending_write_ops.clear();
                        if self.cursor >= self.emails.len() && !self.emails.is_empty() {
                            self.cursor = self.emails.len() - 1;
                        }
                    }
                    Err(e) => {
                        self.error = Some(format!("Failed to load thread: {}", e));
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
                            // No refresh: optimistic update + cache already
                            // reflect the correct state. Press 'g' to refresh.
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
            _ => false,
        }
    }
}

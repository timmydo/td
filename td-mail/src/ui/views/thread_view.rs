use crate::backend::{BackendCommand, BackendResponse, EmailMutationAction};
use crate::compose;
use crate::jmap::types::{Email, Mailbox};
use crate::rules;
use crate::ui::input::Key;
use crate::ui::views::email_view::{EmailNavEntry, EmailView};
use crate::ui::views::help::HelpView;
use crate::ui::views::{strip_newlines, Body, Row, Scene, View, ViewAction};
use std::collections::HashMap;
use std::sync::mpsc;

/// The action bar's labels, and the key each stands for; a press on a
/// label is that key.
const LABELS: &[&str] = &[
    "Read", "Refresh", "Archive", "Delete", "Flag", "Unread", "Back",
];
const KEYS: &[Key] = &[
    Key::Enter,
    Key::Char('g'),
    Key::Char('a'),
    Key::Char('d'),
    Key::Char('f'),
    Key::Char('u'),
    Key::Char('q'),
];

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
    emails: Vec<Email>,
    cursor: usize,
    loading: bool,
    error: Option<String>,
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
            emails: Vec::new(),
            cursor: 0,
            loading: true,
            error: None,
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

    /// Who the message is from: the name the address carries, else the
    /// address itself.
    fn sender(email: &Email) -> String {
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
        strip_newlines(from)
    }

    /// The day the message arrived: the date's leading `YYYY-MM-DD`, or
    /// whatever shorter thing the server sent.
    fn received_day(email: &Email) -> &str {
        let date = email.received_at.as_deref().unwrap_or_default();
        date.get(..10).unwrap_or(date)
    }

    /// Message `index`: who it is from, flagged first; the day at the
    /// right, with the folder it is in when the thread spans folders.
    fn email_row(&self, index: usize) -> Row {
        let Some(email) = self.emails.get(index) else {
            return Row::default();
        };
        let sender = Self::sender(email);
        let label = if Self::is_flagged(email) {
            format!("F {}", sender)
        } else {
            sender
        };
        let day = Self::received_day(email);
        let meta = if self.filter_mailbox_id.is_none() {
            format!("{} · {}", day, self.mailbox_name_for_email(email))
        } else {
            day.to_string()
        };
        Row {
            label,
            meta,
            marked: Self::is_unread(email),
        }
    }

    /// The status row: where the cursor is and the thread's keys, behind
    /// any message the view has.
    fn status_line(&self) -> String {
        let base = if self.loading {
            "Loading... | q:back".to_string()
        } else if self.emails.is_empty() {
            "q:back g:refresh".to_string()
        } else {
            let expire_hint = if self.can_expire_now { " D:expire" } else { "" };
            format!(
                "{}/{} | q:back n/p:nav RET:read g:refresh a:archive d:delete{} f:flag u:unread",
                self.cursor + 1,
                self.emails.len(),
                expire_hint
            )
        };
        match self.status_message {
            Some(ref msg) => format!("{} | {}", msg, base),
            None => base,
        }
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
        let _ = self.cmd_tx.send(BackendCommand::QueryThreadEmails {
            thread_id: self.thread_id.clone(),
        });
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

impl View for ThreadView {
    fn scene(&self) -> Scene<'_> {
        let mode_label = if self.filter_mailbox_id.is_some() {
            "Thread"
        } else {
            "Thread (all folders)"
        };
        let body = if self.loading && self.emails.is_empty() {
            Body::message("Loading thread...".to_string())
        } else if let Some(ref err) = self.error {
            Body::message(err.clone())
        } else if self.emails.is_empty() {
            Body::message("No messages in thread.".to_string())
        } else {
            Body::List {
                total: self.emails.len(),
                selected: self.cursor,
                row: Box::new(move |index| self.email_row(index)),
            }
        };
        Scene {
            title: format!(
                "{}: {} ({} messages)",
                mode_label,
                self.subject,
                self.emails.len()
            ),
            labels: LABELS,
            keys: KEYS,
            entry: None,
            body,
            status: self.status_line(),
        }
    }

    fn handle_key(&mut self, key: Key, page: usize) -> ViewAction {
        match key {
            Key::Char('q') => ViewAction::Pop,
            Key::Char('n') | Key::Char('j') | Key::Down => {
                if !self.emails.is_empty() && self.cursor + 1 < self.emails.len() {
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
                if !self.emails.is_empty() {
                    self.cursor = (self.cursor + page).min(self.emails.len() - 1);
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
                }
                ViewAction::Continue
            }
            Key::ScrollDown => {
                if !self.emails.is_empty() && self.cursor + 1 < self.emails.len() {
                    self.cursor += 1;
                }
                ViewAction::Continue
            }
            // The press selects the message; reading it is the pending
            // action, so the selection is shown before the body loads.
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::jmap::types::EmailAddress;

    fn make_mailboxes() -> Vec<Mailbox> {
        ["mbox-inbox", "mbox-archive"]
            .iter()
            .zip(["Inbox", "Archive"])
            .map(|(id, name)| Mailbox {
                id: (*id).to_string(),
                name: name.to_string(),
                parent_id: None,
                role: None,
                total_emails: 0,
                unread_emails: 0,
                sort_order: 0,
            })
            .collect()
    }

    fn make_email(
        id: &str,
        name: Option<&str>,
        address: Option<&str>,
        mailbox: &str,
        seen: bool,
        flagged: bool,
    ) -> Email {
        let mut keywords = HashMap::new();
        if seen {
            keywords.insert("$seen".to_string(), true);
        }
        if flagged {
            keywords.insert("$flagged".to_string(), true);
        }
        let mut mailbox_ids = HashMap::new();
        mailbox_ids.insert(mailbox.to_string(), true);
        Email {
            id: id.to_string(),
            thread_id: Some("thread-1".to_string()),
            from: Some(vec![EmailAddress {
                name: name.map(str::to_string),
                email: address.map(str::to_string),
            }]),
            to: None,
            cc: None,
            reply_to: None,
            subject: Some(format!("Subject {}", id)),
            received_at: Some("2025-03-04T05:06:07Z".to_string()),
            sent_at: None,
            preview: None,
            text_body: None,
            html_body: None,
            body_values: HashMap::new(),
            keywords,
            mailbox_ids,
            message_id: None,
            references: None,
            attachments: None,
            extra: HashMap::new(),
        }
    }

    /// The thread's two messages: an unread flagged one from a name, and
    /// a read one from a bare address in `second_mailbox`.
    fn messages(second_mailbox: &str) -> BackendResponse {
        BackendResponse::ThreadEmails {
            thread_id: "thread-1".to_string(),
            emails: Ok(vec![
                make_email(
                    "e1",
                    Some("Ada Lovelace"),
                    Some("ada@example.com"),
                    "mbox-inbox",
                    false,
                    true,
                ),
                make_email(
                    "e2",
                    None,
                    Some("bob@example.com"),
                    second_mailbox,
                    true,
                    false,
                ),
            ]),
        }
    }

    fn make_view(filter: Option<&str>) -> (ThreadView, mpsc::Receiver<BackendCommand>) {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let view = ThreadView::new(
            cmd_tx,
            "me@example.com".to_string(),
            "thread-1".to_string(),
            "Roof repair".to_string(),
            make_mailboxes(),
            "Archive".to_string(),
            "Trash".to_string(),
            false,
            filter.map(str::to_string),
            None,
        );
        (view, cmd_rx)
    }

    fn rows(view: &ThreadView) -> Vec<Row> {
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
    fn a_threads_messages_are_rows_of_sender_and_day() {
        let (mut view, _cmd_rx) = make_view(Some("mbox-inbox"));
        // Nothing answered yet: the pane says the thread is loading.
        assert!(matches!(view.scene().body, Body::Text { .. }));
        assert_eq!(view.scene().title, "Thread: Roof repair (0 messages)");
        assert!(view.on_response(&messages("mbox-inbox")));
        assert_eq!(
            rows(&view),
            vec![
                Row {
                    label: "F Ada Lovelace".to_string(),
                    meta: "2025-03-04".to_string(),
                    marked: true,
                },
                Row {
                    label: "bob@example.com".to_string(),
                    meta: "2025-03-04".to_string(),
                    marked: false,
                },
            ]
        );
        let scene = view.scene();
        assert_eq!(scene.title, "Thread: Roof repair (2 messages)");
        assert_eq!(scene.labels, LABELS);
        assert_eq!(scene.keys, KEYS);
        assert!(scene.entry.is_none());
        assert!(scene.status.starts_with("1/2 | "), "{}", scene.status);
    }

    #[test]
    fn across_folders_the_row_names_the_folder_its_message_is_in() {
        let (mut view, _cmd_rx) = make_view(None);
        view.on_response(&messages("mbox-archive"));
        let rows = rows(&view);
        assert_eq!(
            rows.iter().map(|row| row.meta.as_str()).collect::<Vec<_>>(),
            vec!["2025-03-04 · Inbox", "2025-03-04 · Archive"]
        );
        assert_eq!(
            view.scene().title,
            "Thread (all folders): Roof repair (2 messages)"
        );
    }

    #[test]
    fn a_press_on_a_message_selects_it_and_reads_it() {
        let (mut view, cmd_rx) = make_view(Some("mbox-inbox"));
        view.on_response(&messages("mbox-inbox"));
        view.handle_key(Key::Click(1), 10);
        assert_eq!(view.cursor, 1);
        assert!(matches!(
            view.take_pending_action(),
            Some(ViewAction::Push(_))
        ));
        let mut fetched = None;
        while let Ok(cmd) = cmd_rx.try_recv() {
            if let BackendCommand::GetEmail { id } = cmd {
                fetched = Some(id);
            }
        }
        assert_eq!(fetched.as_deref(), Some("e2"));
        // A press past the last row is no row.
        view.handle_key(Key::Click(5), 10);
        assert_eq!(view.cursor, 1);
        assert!(view.take_pending_action().is_none());
    }
}

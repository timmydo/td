//! The unified inbox: every account's inbox in one list, newest first,
//! each row its account's. A row's actions go to its own account's
//! backend and a message opens over it, bound to that account; answers
//! reach the view tagged by account (`View::on_account_response`), so two
//! accounts' ids and operation numbers never meet.
//!
//! Each account's inbox is fetched a page at a time. The list stops where
//! an account with older mail still unfetched stops, so nothing it shows
//! has an older-arriving message of another account missing above it;
//! moving to the last row fetches the next page of the account that
//! limits it.

use crate::backend::{BackendCommand, BackendResponse};
use crate::compose;
use crate::jmap::types::{Email, Mailbox};
use crate::rules;
use crate::ui::input::Key;
use crate::ui::views::email_view::{EmailNavEntry, EmailView};
use crate::ui::views::{strip_newlines, Body, Row, Scene, View, ViewAction};
use std::collections::{HashMap, HashSet};
use std::sync::mpsc;
use td_ui::keys;

const LABELS: &[&str] = &[
    "Read",
    "Refresh",
    "Archive",
    "Delete",
    "Unread",
    "Flag",
    "Compose",
    keys::BUTTON,
    "Quit",
];
const KEYS: &[Key] = &[
    Key::Enter,
    Key::Char('g'),
    Key::Char('a'),
    Key::Char('d'),
    Key::Char('u'),
    Key::Char('f'),
    Key::Char('c'),
    Key::Char('?'),
    Key::Char('q'),
];

/// The first operation number: a message view this list opens numbers
/// its own from 1 on the same account, and its late answers reach this
/// view too.
const FIRST_OP: u64 = 1 << 40;

/// The most messages a refresh asks one account for at once.
const REFRESH_MAX: u32 = 1000;

/// An account's inbox as the view is given it: the account's place in
/// the session, its name, its backend, the From its replies and drafts
/// use, the folders an archive or a delete moves to, and its folders as
/// last listed, where its inbox is found.
pub struct Inbox {
    pub account: usize,
    pub name: String,
    pub cmd_tx: mpsc::Sender<BackendCommand>,
    pub reply_from: String,
    pub archive_folder: String,
    pub deleted_folder: String,
    pub mailboxes: Vec<Mailbox>,
}

/// An account's part of the list.
struct Part {
    inbox: Inbox,
    /// The inbox's mailbox id, once the account's folders name one.
    id: Option<String>,
    /// Its messages, newest first, as fetched page by page.
    emails: Vec<Email>,
    loading: bool,
    /// How many messages its last fetch asked for.
    requested: u32,
    /// Older messages are still unfetched.
    more: bool,
    error: Option<String>,
}

impl Part {
    /// When its oldest fetched message arrived, while older ones wait.
    fn horizon(&self) -> Option<&str> {
        if !self.more {
            return None;
        }
        self.emails
            .iter()
            .filter_map(|e| e.received_at.as_deref())
            .min()
    }
}

/// A change shown before its account's backend answers, to put back if
/// it refuses; matched to the answer by account, operation and message.
enum Pending {
    Seen { id: String, seen: bool },
    Flag { id: String, flagged: bool },
    Move { part: usize, email: Box<Email> },
}

impl Pending {
    fn id(&self) -> &str {
        match self {
            Pending::Seen { id, .. } | Pending::Flag { id, .. } => id,
            Pending::Move { email, .. } => &email.id,
        }
    }
}

pub struct UnifiedInboxView {
    parts: Vec<Part>,
    /// The rows, newest first: each a part and the message's place in it.
    rows: Vec<(usize, usize)>,
    cursor: usize,
    page_size: u32,
    browser: Option<String>,
    /// The account a draft composed here is from, and its From.
    compose_name: String,
    compose_from: String,
    status_message: Option<String>,
    pending_click: bool,
    /// The part a message was last opened from, refreshed on return.
    opened: Option<usize>,
    next_op_id: u64,
    pending: HashMap<(usize, u64), Pending>,
}

impl UnifiedInboxView {
    pub fn new(
        inboxes: Vec<Inbox>,
        page_size: u32,
        browser: Option<String>,
        compose_name: String,
        compose_from: String,
    ) -> Self {
        let mut view = UnifiedInboxView {
            parts: inboxes
                .into_iter()
                .map(|inbox| Part {
                    id: inbox_id(&inbox.mailboxes),
                    inbox,
                    emails: Vec::new(),
                    loading: false,
                    requested: 0,
                    more: false,
                    error: None,
                })
                .collect(),
            rows: Vec::new(),
            cursor: 0,
            page_size,
            browser,
            compose_name,
            compose_from,
            status_message: None,
            pending_click: false,
            opened: None,
            next_op_id: FIRST_OP,
            pending: HashMap::new(),
        };
        view.refresh();
        view
    }

    /// Every part asked for its newest page, or for its folders when
    /// they name no inbox yet.
    fn refresh(&mut self) {
        for index in 0..self.parts.len() {
            self.query(index, 0);
        }
    }

    /// The part's page from `position`, or its folders without an inbox.
    /// From the top it asks for every message already fetched (and those
    /// waiting on a move), bounded, so its answer replaces them all and
    /// none is kept unchecked.
    fn query(&mut self, index: usize, position: u32) {
        let moving = self
            .pending
            .values()
            .filter(|pending| matches!(pending, Pending::Move { part, .. } if *part == index))
            .count();
        let page_size = self.page_size;
        let Some(part) = self.parts.get_mut(index) else {
            return;
        };
        let size = if position == 0 {
            let fetched =
                u32::try_from(part.emails.len().saturating_add(moving)).unwrap_or(u32::MAX);
            fetched.clamp(page_size, REFRESH_MAX.max(page_size))
        } else {
            page_size
        };
        let Some(id) = part.id.clone() else {
            let _ = part.inbox.cmd_tx.send(BackendCommand::FetchMailboxes {
                origin: "unified_inbox".to_string(),
            });
            return;
        };
        part.loading = true;
        part.requested = size;
        let _ = part.inbox.cmd_tx.send(BackendCommand::QueryEmails {
            origin: "unified_inbox".to_string(),
            mailbox_id: id,
            page_size: size,
            position,
            search_query: None,
            received_after: None,
            received_before: None,
        });
    }

    /// The next page of each account the list stops at, once the cursor
    /// is on its last row.
    fn load_more(&mut self) {
        if self.cursor + 1 < self.rows.len() {
            return;
        }
        let Some(horizon) = self.horizon().map(str::to_string) else {
            return;
        };
        let limiting: Vec<(usize, u32)> = self
            .parts
            .iter()
            .enumerate()
            .filter(|(_, p)| !p.loading && p.horizon() == Some(horizon.as_str()))
            .map(|(index, p)| (index, p.emails.len() as u32))
            .collect();
        for (index, position) in limiting {
            self.query(index, position);
        }
    }

    /// Where the list stops: the newest of the parts' horizons, below
    /// which an account's older mail is not yet fetched.
    fn horizon(&self) -> Option<&str> {
        self.parts.iter().filter_map(Part::horizon).max()
    }

    /// The account and id of the message at `index`.
    fn key_at(&self, index: usize) -> Option<(usize, String)> {
        let &(part, email) = self.rows.get(index)?;
        Some((part, self.parts.get(part)?.emails.get(email)?.id.clone()))
    }

    /// The rows merged again, newest first, down to the horizon; the
    /// cursor on `hold` while the list still shows it, else kept by
    /// place.
    fn rebuild(&mut self, hold: Option<(usize, String)>) {
        let horizon = self.horizon().map(str::to_string);
        let mut rows: Vec<(usize, usize)> = Vec::new();
        for (part, p) in self.parts.iter().enumerate() {
            for (index, email) in p.emails.iter().enumerate() {
                let shown = match (&horizon, email.received_at.as_deref()) {
                    (None, _) => true,
                    (Some(h), Some(at)) => at >= h.as_str(),
                    (Some(_), None) => false,
                };
                if shown {
                    rows.push((part, index));
                }
            }
        }
        let received = |&(part, email): &(usize, usize)| {
            self.parts
                .get(part)
                .and_then(|p| p.emails.get(email))
                .and_then(|e| e.received_at.as_deref())
        };
        // ISO timestamps order as text; one without a time goes last.
        rows.sort_by(|a, b| received(b).cmp(&received(a)).then(a.cmp(b)));
        self.rows = rows;
        if let Some((part, id)) = hold {
            let at = self.rows.iter().position(|&(p, e)| {
                p == part
                    && self
                        .parts
                        .get(p)
                        .and_then(|p| p.emails.get(e))
                        .is_some_and(|email| email.id == id)
            });
            if let Some(at) = at {
                self.cursor = at;
            }
        }
        self.cursor = self.cursor.min(self.rows.len().saturating_sub(1));
    }

    /// The selected message and its part.
    fn selected(&self) -> Option<(usize, &Email)> {
        let &(part, email) = self.rows.get(self.cursor)?;
        Some((part, self.parts.get(part)?.emails.get(email)?))
    }

    fn next_op(&mut self) -> u64 {
        let op = self.next_op_id;
        self.next_op_id = self.next_op_id.wrapping_add(1);
        op
    }

    /// `command` for the selected message's account, remembered as
    /// `pending` once sent; whether it was.
    fn send_op(
        &mut self,
        part: usize,
        command: BackendCommand,
        op_id: u64,
        pending: Pending,
    ) -> bool {
        let Some(inbox) = self.parts.get(part).map(|p| &p.inbox) else {
            return false;
        };
        let account = inbox.account;
        if let Err(e) = inbox.cmd_tx.send(command) {
            self.status_message = Some(format!("Not sent: {e}"));
            return false;
        }
        self.pending.insert((account, op_id), pending);
        true
    }

    fn edit_email(&mut self, part: usize, id: &str, edit: impl FnOnce(&mut Email)) {
        if let Some(email) = self
            .parts
            .get_mut(part)
            .and_then(|p| p.emails.iter_mut().find(|e| e.id == id))
        {
            edit(email);
        }
    }

    fn set_keyword(&mut self, part: usize, id: &str, keyword: &str, on: bool) {
        self.edit_email(part, id, |email| {
            if on {
                email.keywords.insert(keyword.to_string(), true);
            } else {
                email.keywords.remove(keyword);
            }
        });
    }

    /// Opens the selected message over the list, bound to its account:
    /// its backend, its From and folders, and its account's messages in
    /// the list's order to step through; unread, it is marked read.
    fn open(&mut self) -> ViewAction {
        let Some((part_index, email)) = self.selected() else {
            return ViewAction::Continue;
        };
        let id = email.id.clone();
        let unread = !email.keywords.contains_key("$seen");
        let nav: Vec<EmailNavEntry> = self
            .rows
            .iter()
            .filter(|&&(p, _)| p == part_index)
            .filter_map(|&(p, e)| self.parts.get(p)?.emails.get(e))
            .map(|e| EmailNavEntry {
                id: e.id.clone(),
                unread: !e.keywords.contains_key("$seen"),
            })
            .collect();
        let nav_cursor = nav.iter().position(|entry| entry.id == id).unwrap_or(0);
        let Some(inbox) = self.parts.get(part_index).map(|part| &part.inbox) else {
            return ViewAction::Continue;
        };
        let view = EmailView::new(
            inbox.cmd_tx.clone(),
            inbox.reply_from.clone(),
            id.clone(),
            nav,
            nav_cursor,
            false,
            inbox.mailboxes.clone(),
            inbox.archive_folder.clone(),
            inbox.deleted_folder.clone(),
            self.browser.clone(),
        );
        let account = inbox.account;
        let _ = inbox
            .cmd_tx
            .send(BackendCommand::GetEmail { id: id.clone() });
        if unread {
            self.mark(part_index, &id, true);
        }
        self.opened = Some(part_index);
        ViewAction::PushFor(account, Box::new(view))
    }

    /// The message marked read or unread on its account.
    fn mark(&mut self, part: usize, id: &str, seen: bool) {
        let op_id = self.next_op();
        let command = if seen {
            BackendCommand::MarkEmailRead {
                op_id,
                id: id.to_string(),
            }
        } else {
            BackendCommand::MarkEmailUnread {
                op_id,
                id: id.to_string(),
            }
        };
        let pending = Pending::Seen {
            id: id.to_string(),
            seen,
        };
        if self.send_op(part, command, op_id, pending) {
            self.set_keyword(part, id, "$seen", seen);
        }
    }

    /// The selected message read or unread, flagged or not, the other way.
    fn toggle(&mut self, flag: bool) {
        let Some((part, email)) = self.selected() else {
            return;
        };
        let id = email.id.clone();
        if !flag {
            let seen = email.keywords.contains_key("$seen");
            self.mark(part, &id, !seen);
            return;
        }
        let flagged = !email.keywords.contains_key("$flagged");
        let op_id = self.next_op();
        let command = BackendCommand::SetEmailFlagged {
            op_id,
            id: id.clone(),
            flagged,
        };
        let pending = Pending::Flag {
            id: id.clone(),
            flagged,
        };
        if self.send_op(part, command, op_id, pending) {
            self.set_keyword(part, &id, "$flagged", flagged);
        }
    }

    /// The selected message moved to its account's archive or trash,
    /// taken from the list at once, the cursor on the row after it, and
    /// put back if the backend refuses.
    fn move_selected(&mut self, archive: bool) {
        let Some(&(part_index, email_index)) = self.rows.get(self.cursor) else {
            return;
        };
        let label = if archive { "Archive" } else { "Delete" };
        let Some(part) = self.parts.get(part_index) else {
            return;
        };
        let folder = if archive {
            &part.inbox.archive_folder
        } else {
            &part.inbox.deleted_folder
        };
        let Some(target) = rules::resolve_mailbox_id(folder, &part.inbox.mailboxes) else {
            let refusal = format!(
                "{label} failed: {} has no folder '{folder}'",
                part.inbox.name
            );
            self.status_message = Some(refusal);
            return;
        };
        let Some(email) = part.emails.get(email_index).cloned() else {
            return;
        };
        let op_id = self.next_op();
        let command = BackendCommand::MoveEmail {
            op_id,
            id: email.id.clone(),
            to_mailbox_id: target,
        };
        let pending = Pending::Move {
            part: part_index,
            email: Box::new(email),
        };
        if self.send_op(part_index, command, op_id, pending) {
            if let Some(part) = self.parts.get_mut(part_index) {
                if email_index < part.emails.len() {
                    part.emails.remove(email_index);
                }
            }
            // By place: the row after the one taken is the cursor's now.
            self.rebuild(None);
        }
    }

    /// Message `index` of the merged list as a row: its subject, led by
    /// its flag, over its account, sender and day.
    fn row(&self, index: usize) -> Row {
        let Some(&(part, email)) = self.rows.get(index) else {
            return Row::default();
        };
        let Some((name, email)) = self
            .parts
            .get(part)
            .and_then(|p| Some((p.inbox.name.as_str(), p.emails.get(email)?)))
        else {
            return Row::default();
        };
        let mut label = String::new();
        if email.keywords.contains_key("$flagged") {
            label.push_str("F ");
        }
        label.push_str(&strip_newlines(email.display_subject()));
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
        let date = email
            .received_at
            .as_deref()
            .map_or("", |d| d.get(..10).unwrap_or(d));
        Row {
            label,
            meta: format!(
                "{} · {} · {}",
                strip_newlines(name),
                strip_newlines(from),
                date
            ),
            marked: !email.keywords.contains_key("$seen"),
        }
    }

    /// Each account with nothing to show yet, and why.
    fn notes(&self) -> Vec<String> {
        self.parts
            .iter()
            .filter_map(|part| {
                if let Some(e) = &part.error {
                    Some(format!("{}: {e}", part.inbox.name))
                } else if part.id.is_none() {
                    Some(format!("{}: no inbox listed yet", part.inbox.name))
                } else if part.loading && part.emails.is_empty() {
                    Some(format!("{}: loading", part.inbox.name))
                } else {
                    None
                }
            })
            .collect()
    }
}

/// The inbox among an account's folders.
fn inbox_id(mailboxes: &[Mailbox]) -> Option<String> {
    mailboxes
        .iter()
        .find(|m| m.role.as_deref() == Some("inbox"))
        .map(|m| m.id.clone())
}

impl View for UnifiedInboxView {
    fn scene(&self) -> Scene<'_> {
        let notes = self.notes();
        let body = if self.rows.is_empty() {
            let mut text = if notes.is_empty() {
                "No mail in any inbox.".to_string()
            } else {
                "No mail yet.".to_string()
            };
            for note in &notes {
                text.push('\n');
                text.push_str(note);
            }
            Body::message(text)
        } else {
            Body::List {
                total: self.rows.len(),
                selected: self.cursor,
                row: Box::new(move |index| self.row(index)),
            }
        };
        let compose = format!("c:compose from {}", self.compose_name);
        let mut status = if self.rows.is_empty() {
            format!("g:refresh {compose} q:quit")
        } else {
            // `+`: older mail of an account below the last row.
            let more = if self.horizon().is_some() { "+" } else { "" };
            format!(
                "{}/{}{more} | n/p:navigate RET:read g:refresh a:archive d:delete u:unread f:flag {compose} ?:help q:quit",
                self.cursor + 1,
                self.rows.len()
            )
        };
        if !notes.is_empty() {
            status = format!("{} | {status}", notes.join("; "));
        }
        if let Some(message) = &self.status_message {
            status = format!("{message} | {status}");
        }
        Scene {
            title: "td-mail - All inboxes".to_string(),
            labels: LABELS,
            keys: KEYS,
            entry: None,
            body,
            status,
        }
    }

    fn keys(&self) -> Option<&'static crate::keybindings::Table> {
        Some(&crate::keybindings::UNIFIED_INBOX)
    }

    fn handle_key(&mut self, key: Key, page: usize) -> ViewAction {
        self.status_message = None;
        let last = self.rows.len().saturating_sub(1);
        match key {
            Key::Char('n' | 'j') | Key::Down | Key::ScrollDown => {
                self.cursor = (self.cursor + 1).min(last);
                self.load_more();
            }
            Key::Char('p' | 'k') | Key::Up | Key::ScrollUp => {
                self.cursor = self.cursor.saturating_sub(1);
            }
            Key::PageDown => {
                self.cursor = self.cursor.saturating_add(page.max(1)).min(last);
                self.load_more();
            }
            Key::PageUp => self.cursor = self.cursor.saturating_sub(page.max(1)),
            Key::Home => self.cursor = 0,
            Key::End => {
                self.cursor = last;
                self.load_more();
            }
            Key::Click(index) if index < self.rows.len() => {
                self.cursor = index;
                self.pending_click = true;
            }
            Key::Enter => return self.open(),
            Key::Char('g') => self.refresh(),
            Key::Char('a') => self.move_selected(true),
            Key::Char('d') => self.move_selected(false),
            Key::Char('u') => self.toggle(false),
            Key::Char('f') => self.toggle(true),
            Key::Char('c') => {
                let draft = compose::build_compose_draft(&self.compose_from);
                return ViewAction::Compose(draft.into());
            }
            Key::Char('?') => return ViewAction::ShowKeys,
            Key::Char('q') => return ViewAction::Pop,
            _ => {}
        }
        ViewAction::Continue
    }

    fn take_pending_action(&mut self) -> Option<ViewAction> {
        std::mem::take(&mut self.pending_click).then(|| self.open())
    }

    fn on_response(&mut self, _response: &BackendResponse) -> bool {
        false
    }

    fn on_account_response(&mut self, account: usize, response: &BackendResponse) -> bool {
        let Some(index) = self.parts.iter().position(|p| p.inbox.account == account) else {
            return false;
        };
        match response {
            // A listing names the inbox; a changed one (the account set
            // up anew) starts the part over.
            BackendResponse::Mailboxes(result) => {
                let hold = self.key_at(self.cursor);
                let Some(part) = self.parts.get_mut(index) else {
                    return false;
                };
                match result {
                    Ok(mailboxes) => {
                        part.inbox.mailboxes = mailboxes.clone();
                        let id = inbox_id(mailboxes);
                        if id == part.id {
                            return false;
                        }
                        part.id = id;
                        part.emails.clear();
                        part.more = false;
                        part.error = None;
                        self.rebuild(hold);
                        self.query(index, 0);
                        true
                    }
                    Err(e) if part.id.is_none() => {
                        part.error = Some(e.clone());
                        true
                    }
                    Err(_) => false,
                }
            }
            BackendResponse::Emails {
                mailbox_id,
                emails,
                total,
                position,
                loaded,
                ..
            } => {
                let hold = self.key_at(self.cursor);
                let moving: HashSet<String> = self
                    .pending
                    .iter()
                    .filter(|((a, _), _)| *a == account)
                    .filter_map(|(_, pending)| match pending {
                        Pending::Move { email, .. } => Some(email.id.clone()),
                        Pending::Seen { .. } | Pending::Flag { .. } => None,
                    })
                    .collect();
                let Some(part) = self.parts.get_mut(index) else {
                    return false;
                };
                if part.id.as_deref() != Some(mailbox_id.as_str()) {
                    return false;
                }
                part.loading = false;
                match emails {
                    Ok(emails) => {
                        let fresh = emails.iter().filter(|e| !moving.contains(&e.id)).cloned();
                        if *position == 0 {
                            // It asked for all that was fetched: the whole
                            // list now, nothing older kept unchecked.
                            part.emails = fresh.collect();
                        } else {
                            let mut known: HashSet<String> =
                                part.emails.iter().map(|e| e.id.clone()).collect();
                            part.emails
                                .extend(fresh.filter(|e| known.insert(e.id.clone())));
                        }
                        // A message waiting on its move is in the total.
                        let held = moving.len() as u32;
                        part.more = match total {
                            Some(total) => (part.emails.len() as u32).saturating_add(held) < *total,
                            None => *loaded >= part.requested,
                        };
                        part.error = None;
                    }
                    Err(e) => part.error = Some(e.clone()),
                }
                self.rebuild(hold);
                true
            }
            BackendResponse::EmailMutation {
                op_id, id, result, ..
            } => {
                let key = (account, *op_id);
                if self.pending.get(&key).is_none_or(|p| p.id() != id) {
                    return false;
                }
                let Some(pending) = self.pending.remove(&key) else {
                    return false;
                };
                let Err(e) = result else {
                    return true;
                };
                match pending {
                    Pending::Seen { id, seen } => {
                        self.set_keyword(index, &id, "$seen", !seen);
                        self.status_message = Some(format!("Mark read failed: {e}"));
                    }
                    Pending::Flag { id, flagged } => {
                        self.set_keyword(index, &id, "$flagged", !flagged);
                        self.status_message = Some(format!("Flag failed: {e}"));
                    }
                    Pending::Move { part, email } => {
                        let hold = self.key_at(self.cursor);
                        if let Some(p) = self.parts.get_mut(part) {
                            p.emails.push(*email);
                            p.emails.sort_by(|a, b| b.received_at.cmp(&a.received_at));
                        }
                        self.status_message = Some(format!("Move failed: {e}"));
                        self.rebuild(hold);
                    }
                }
                true
            }
            // The account's inbox marked read from the account tree: its
            // messages shown are read now too.
            BackendResponse::MailboxMarkedRead {
                mailbox_id,
                result: Ok(()),
                ..
            } => {
                let Some(part) = self.parts.get_mut(index) else {
                    return false;
                };
                if part.id.as_deref() != Some(mailbox_id.as_str()) {
                    return false;
                }
                for email in &mut part.emails {
                    email.keywords.insert("$seen".to_string(), true);
                }
                true
            }
            _ => false,
        }
    }

    /// Back from a message, its account's inbox is asked again: it may
    /// have been moved, flagged or read there.
    fn on_reveal(&mut self) -> bool {
        match self.opened.take() {
            Some(part) => self.query(part, 0),
            None => self.refresh(),
        }
        true
    }

    fn trigger_idle_sync(&mut self) -> bool {
        self.refresh();
        true
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
    use super::*;
    use crate::backend::EmailMutationAction;

    fn email(id: &str, received: &str) -> Email {
        Email {
            id: id.to_string(),
            thread_id: None,
            from: None,
            to: None,
            cc: None,
            reply_to: None,
            subject: Some(format!("Subject {id}")),
            received_at: Some(received.to_string()),
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

    fn folder(id: &str, name: &str, role: Option<&str>) -> Mailbox {
        Mailbox {
            id: id.to_string(),
            name: name.to_string(),
            parent_id: None,
            role: role.map(str::to_string),
            total_emails: 0,
            unread_emails: 0,
            sort_order: 0,
        }
    }

    fn inbox(
        account: usize,
        name: &str,
        mailboxes: Vec<Mailbox>,
    ) -> (Inbox, mpsc::Receiver<BackendCommand>) {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        (
            Inbox {
                account,
                name: name.to_string(),
                cmd_tx,
                reply_from: format!("me@{name}.test"),
                archive_folder: "Archive".to_string(),
                deleted_folder: "Trash".to_string(),
                mailboxes,
            },
            cmd_rx,
        )
    }

    fn emails(mailbox_id: &str, list: Vec<Email>) -> BackendResponse {
        BackendResponse::Emails {
            mailbox_id: mailbox_id.to_string(),
            emails: Ok(list),
            total: None,
            position: 0,
            loaded: 0,
            thread_counts: HashMap::new(),
        }
    }

    fn labels(view: &UnifiedInboxView) -> Vec<String> {
        (0..view.rows.len()).map(|i| view.row(i).label).collect()
    }

    /// Each account's inbox is asked of its own backend, the one not yet
    /// listed once its folders name its inbox; the answers merge newest
    /// first, each row naming its account, and an answer for one
    /// account's inbox id from another account fills nothing.
    #[test]
    fn every_inbox_is_asked_of_its_account_and_merged_newest_first() {
        let (home, home_rx) = inbox(0, "home", vec![folder("h-in", "Inbox", Some("inbox"))]);
        let (work, work_rx) = inbox(1, "work", Vec::new());
        let mut view = UnifiedInboxView::new(
            vec![home, work],
            50,
            None,
            "home".into(),
            "me@home.test".into(),
        );
        assert!(matches!(
            home_rx.try_recv(),
            Ok(BackendCommand::QueryEmails { mailbox_id, .. }) if mailbox_id == "h-in"
        ));
        // The work account names no inbox yet: its folders are asked for.
        assert!(matches!(
            work_rx.try_recv(),
            Ok(BackendCommand::FetchMailboxes { .. })
        ));
        assert!(view.scene().status.contains("work: no inbox listed yet"));
        assert!(view.on_account_response(
            1,
            &BackendResponse::Mailboxes(Ok(vec![
                folder("w-in", "INBOX", Some("inbox")),
                folder("w-ar", "Archive", Some("archive")),
            ]))
        ));
        assert!(matches!(
            work_rx.try_recv(),
            Ok(BackendCommand::QueryEmails { mailbox_id, .. }) if mailbox_id == "w-in"
        ));
        assert!(!view.on_account_response(0, &emails("w-in", vec![email("x", "2026-01-09")])));
        view.on_account_response(
            0,
            &emails(
                "h-in",
                vec![email("h1", "2026-01-03"), email("h2", "2026-01-01")],
            ),
        );
        view.on_account_response(1, &emails("w-in", vec![email("w1", "2026-01-02")]));
        assert_eq!(labels(&view), ["Subject h1", "Subject w1", "Subject h2"]);
        assert!(view.row(1).meta.starts_with("work · "));
        assert!(view.row(1).marked);
    }

    /// An account's inbox marked read shows its messages read; the same
    /// inbox id answered by another account, another folder of the
    /// account, or a refusal leaves every row as it was.
    #[test]
    fn an_inbox_marked_read_shows_its_messages_read() {
        let (home, _home_rx) = inbox(0, "home", vec![folder("h-in", "Inbox", Some("inbox"))]);
        let (work, _work_rx) = inbox(1, "work", vec![folder("w-in", "INBOX", Some("inbox"))]);
        let mut view = UnifiedInboxView::new(
            vec![home, work],
            50,
            None,
            "home".into(),
            "me@home.test".into(),
        );
        view.on_account_response(0, &emails("h-in", vec![email("h1", "2026-01-01")]));
        view.on_account_response(1, &emails("w-in", vec![email("w1", "2026-01-02")]));
        let marked =
            |mailbox_id: &str, result: Result<(), String>| BackendResponse::MailboxMarkedRead {
                mailbox_id: mailbox_id.to_string(),
                mailbox_name: "Inbox".to_string(),
                updated: 1,
                result,
            };
        assert!(!view.on_account_response(1, &marked("h-in", Ok(()))));
        assert!(!view.on_account_response(0, &marked("h-other", Ok(()))));
        assert!(!view.on_account_response(0, &marked("h-in", Err("refused".into()))));
        assert!(view.row(0).marked && view.row(1).marked);
        assert!(view.on_account_response(0, &marked("h-in", Ok(()))));
        // Newest first: the work message, then the home one.
        assert!(view.row(0).marked);
        assert!(!view.row(1).marked);
    }

    /// Return opens the message over the list bound to its account,
    /// marked read on that account's backend; `a` moves the selected
    /// message to its own account's archive; a refusal answered by that
    /// account puts it back, and the same operation number from another
    /// account is not its answer.
    #[test]
    fn a_rows_actions_go_to_its_own_account() {
        let (home, home_rx) = inbox(0, "home", vec![folder("h-in", "Inbox", Some("inbox"))]);
        let (work, work_rx) = inbox(
            1,
            "work",
            vec![
                folder("w-in", "INBOX", Some("inbox")),
                folder("w-ar", "Archive", Some("archive")),
            ],
        );
        let mut view = UnifiedInboxView::new(
            vec![home, work],
            50,
            None,
            "home".into(),
            "me@home.test".into(),
        );
        view.on_account_response(0, &emails("h-in", vec![email("h1", "2026-01-01")]));
        view.on_account_response(1, &emails("w-in", vec![email("w1", "2026-01-02")]));
        while home_rx.try_recv().is_ok() {}
        while work_rx.try_recv().is_ok() {}
        // The cursor stayed on the first message listed; the newest is first.
        view.handle_key(Key::Home, 10);

        assert!(matches!(
            view.handle_key(Key::Enter, 10),
            ViewAction::PushFor(1, _)
        ));
        assert!(matches!(
            work_rx.try_recv(),
            Ok(BackendCommand::GetEmail { id }) if id == "w1"
        ));
        assert!(matches!(
            work_rx.try_recv(),
            Ok(BackendCommand::MarkEmailRead { id, .. }) if id == "w1"
        ));
        assert!(home_rx.try_recv().is_err());
        assert!(!view.row(0).marked);

        view.handle_key(Key::Char('a'), 10);
        let op = match work_rx.try_recv() {
            Ok(BackendCommand::MoveEmail {
                op_id,
                id,
                to_mailbox_id,
            }) => {
                assert_eq!((id.as_str(), to_mailbox_id.as_str()), ("w1", "w-ar"));
                op_id
            }
            _ => panic!("expected the archive on the work account"),
        };
        assert_eq!(labels(&view), ["Subject h1"]);
        let refused = |account| {
            (
                account,
                BackendResponse::EmailMutation {
                    op_id: op,
                    id: "w1".to_string(),
                    action: EmailMutationAction::Move,
                    result: Err("no".to_string()),
                },
            )
        };
        let (other, answer) = refused(0);
        assert!(!view.on_account_response(other, &answer));
        assert_eq!(labels(&view), ["Subject h1"]);
        let (own, answer) = refused(1);
        assert!(view.on_account_response(own, &answer));
        assert_eq!(labels(&view), ["Subject w1", "Subject h1"]);
        assert!(view.scene().status.starts_with("Move failed: no"));

        // The home account has no Archive folder to move to.
        view.handle_key(Key::Char('j'), 10);
        view.handle_key(Key::Char('a'), 10);
        assert!(view
            .scene()
            .status
            .starts_with("Archive failed: home has no folder 'Archive'"));
        assert!(home_rx.try_recv().is_err());
    }

    /// Two accounts with an inbox and an archive each, and the view
    /// over them with `page_size` and their commands.
    fn two(
        page_size: u32,
    ) -> (
        UnifiedInboxView,
        mpsc::Receiver<BackendCommand>,
        mpsc::Receiver<BackendCommand>,
    ) {
        let folders = |prefix: &str| {
            vec![
                folder(&format!("{prefix}-in"), "Inbox", Some("inbox")),
                folder(&format!("{prefix}-ar"), "Archive", Some("archive")),
            ]
        };
        let (home, home_rx) = inbox(0, "home", folders("h"));
        let (work, work_rx) = inbox(1, "work", folders("w"));
        let view = UnifiedInboxView::new(
            vec![home, work],
            page_size,
            None,
            "home".into(),
            "me@home.test".into(),
        );
        while home_rx.try_recv().is_ok() {}
        while work_rx.try_recv().is_ok() {}
        (view, home_rx, work_rx)
    }

    fn page(mailbox_id: &str, list: Vec<Email>, position: u32, loaded: u32) -> BackendResponse {
        BackendResponse::Emails {
            mailbox_id: mailbox_id.to_string(),
            emails: Ok(list),
            total: None,
            position,
            loaded,
            thread_counts: HashMap::new(),
        }
    }

    fn selected_id(view: &UnifiedInboxView) -> String {
        view.selected().unwrap().1.id.clone()
    }

    /// The cursor stays on its message when new mail of its account
    /// arrives above it, and an archive leaves it on the next row of the
    /// merged list, another account's.
    #[test]
    fn the_cursor_keeps_its_message_and_an_archive_leaves_it_on_the_next_row() {
        let (mut view, _home_rx, work_rx) = two(50);
        view.on_account_response(
            0,
            &emails(
                "h-in",
                vec![email("h1", "2026-01-04"), email("h2", "2026-01-02")],
            ),
        );
        view.on_account_response(
            1,
            &emails(
                "w-in",
                vec![email("w1", "2026-01-03"), email("w2", "2026-01-01")],
            ),
        );
        view.handle_key(Key::Home, 10);
        view.handle_key(Key::Char('j'), 10);
        assert_eq!(selected_id(&view), "w1");
        view.on_account_response(
            1,
            &emails(
                "w-in",
                vec![
                    email("w0", "2026-01-05"),
                    email("w1", "2026-01-03"),
                    email("w2", "2026-01-01"),
                ],
            ),
        );
        assert_eq!(selected_id(&view), "w1");
        view.handle_key(Key::Char('a'), 10);
        assert!(matches!(
            work_rx.try_recv(),
            Ok(BackendCommand::MoveEmail { id, .. }) if id == "w1"
        ));
        assert_eq!(
            labels(&view),
            ["Subject w0", "Subject h1", "Subject h2", "Subject w2"]
        );
        assert_eq!(selected_id(&view), "h2");
    }

    /// `u` and `f` toggle the selected message on its account, a refusal
    /// putting it back; an answer with the operation's number but another
    /// message (a message view's own, numbered from 1) is not its answer.
    #[test]
    fn toggles_go_to_the_messages_account_and_answers_match_the_message() {
        let (mut view, _home_rx, work_rx) = two(50);
        view.on_account_response(1, &emails("w-in", vec![email("w1", "2026-01-03")]));
        view.handle_key(Key::Char('u'), 10);
        let read = match work_rx.try_recv() {
            Ok(BackendCommand::MarkEmailRead { op_id, id }) if id == "w1" => op_id,
            _ => panic!("expected the work account to mark it read"),
        };
        assert!(read >= FIRST_OP);
        assert!(!view.row(0).marked);
        view.handle_key(Key::Char('f'), 10);
        let flag = match work_rx.try_recv() {
            Ok(BackendCommand::SetEmailFlagged {
                op_id,
                id,
                flagged: true,
            }) if id == "w1" => op_id,
            _ => panic!("expected the work account to flag it"),
        };
        assert!(view.row(0).label.starts_with("F "));
        let answer = |op_id, id: &str| BackendResponse::EmailMutation {
            op_id,
            id: id.to_string(),
            action: crate::backend::EmailMutationAction::SetFlagged(true),
            result: Err("refused".to_string()),
        };
        assert!(!view.on_account_response(1, &answer(flag, "other")));
        assert!(view.row(0).label.starts_with("F "));
        assert!(view.on_account_response(1, &answer(flag, "w1")));
        assert!(!view.row(0).label.starts_with("F "));
        assert!(view.on_account_response(1, &answer(read, "w1")));
        assert!(view.row(0).marked);
    }

    /// An account with older mail unfetched cuts the list where its
    /// fetched mail stops, so a quieter account's older mail waits below;
    /// the last row asks that account for its next page, which extends
    /// the list.
    #[test]
    fn the_list_stops_where_an_account_has_more_and_the_last_row_fetches_it() {
        let (mut view, home_rx, work_rx) = two(2);
        view.on_account_response(
            0,
            &page(
                "h-in",
                vec![email("h1", "2026-03-04"), email("h2", "2026-03-03")],
                0,
                2,
            ),
        );
        view.on_account_response(1, &page("w-in", vec![email("w1", "2026-01-01")], 0, 1));
        assert_eq!(labels(&view), ["Subject h1", "Subject h2"]);
        assert!(view.scene().status.starts_with("1/2+ | "));
        view.handle_key(Key::End, 10);
        assert!(matches!(
            home_rx.try_recv(),
            Ok(BackendCommand::QueryEmails { mailbox_id, position: 2, .. }) if mailbox_id == "h-in"
        ));
        assert!(work_rx.try_recv().is_err());
        view.on_account_response(0, &page("h-in", vec![email("h3", "2025-12-01")], 2, 1));
        assert_eq!(
            labels(&view),
            ["Subject h1", "Subject h2", "Subject w1", "Subject h3"]
        );
        assert_eq!(selected_id(&view), "h2");
        assert!(view.scene().status.starts_with("2/4 | "));

        // A refresh asks for all three fetched; a message gone from its
        // answer (archived from its own view, or by another client) is
        // gone from the list, and nothing older is kept unchecked.
        view.handle_key(Key::End, 10);
        assert_eq!(selected_id(&view), "h3");
        while home_rx.try_recv().is_ok() {}
        view.handle_key(Key::Char('g'), 10);
        assert!(matches!(
            home_rx.try_recv(),
            Ok(BackendCommand::QueryEmails {
                position: 0,
                page_size: 3,
                ..
            })
        ));
        view.on_account_response(
            0,
            &page(
                "h-in",
                vec![email("h1", "2026-03-04"), email("h2", "2026-03-03")],
                0,
                2,
            ),
        );
        assert_eq!(labels(&view), ["Subject h1", "Subject h2", "Subject w1"]);
        assert!(!view.scene().status.contains('+'));
    }

    /// A failed folder fetch for an account with no inbox yet says why
    /// and `g` asks again; a listing naming another inbox (the account
    /// set up anew) starts its part over on the new one.
    #[test]
    fn an_unlisted_account_says_why_and_a_new_inbox_starts_it_over() {
        let (home, _home_rx) = inbox(0, "home", Vec::new());
        let (work, work_rx) = inbox(1, "work", vec![folder("w-in", "Inbox", Some("inbox"))]);
        let mut view = UnifiedInboxView::new(
            vec![home, work],
            50,
            None,
            "home".into(),
            "me@home.test".into(),
        );
        assert!(view.on_account_response(0, &BackendResponse::Mailboxes(Err("down".into()))));
        assert!(view.scene().status.contains("home: down"));
        view.on_account_response(1, &emails("w-in", vec![email("w1", "2026-01-01")]));
        while work_rx.try_recv().is_ok() {}
        assert!(view.on_account_response(
            1,
            &BackendResponse::Mailboxes(Ok(vec![folder("w-new", "Inbox", Some("inbox"))]))
        ));
        assert!(labels(&view).is_empty());
        assert!(matches!(
            work_rx.try_recv(),
            Ok(BackendCommand::QueryEmails { mailbox_id, .. }) if mailbox_id == "w-new"
        ));
        assert!(!view.on_account_response(
            1,
            &BackendResponse::Mailboxes(Ok(vec![folder("w-new", "Inbox", Some("inbox"))]))
        ));
    }
}

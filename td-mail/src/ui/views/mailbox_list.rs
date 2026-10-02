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
const SETUP_SCREEN_LABELS: &[&str] = &["Set up", "Account", "Help", "Quit"];
const SETUP_SCREEN_KEYS: &[Key] = &[
    Key::Char('s'),
    Key::Char('a'),
    Key::Char('?'),
    Key::Char('q'),
];
/// A placeholder the form cannot change: only its own keys act.
const PLACEHOLDER_LABELS: &[&str] = &["Account", "Help", "Quit"];
const PLACEHOLDER_KEYS: &[Key] = &[Key::Char('a'), Key::Char('?'), Key::Char('q')];
const SETUP_LABELS: &[&str] = &["OK", "Cancel"];
const SETUP_KEYS: &[Key] = &[Key::Enter, Key::Escape];

/// The setup form's two fields, typed one after the other.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SetupStep {
    Server,
    Address,
}

/// What the mailbox list knows of its account, to say why it shows no
/// mail and to set the account up.
#[derive(Clone, Default)]
pub struct AccountSetup {
    /// The server is a reserved example name: td-firstboot's placeholder.
    pub placeholder: bool,
    /// The portal's name for the account's credential, when it comes
    /// from there.
    pub portal: Option<String>,
    /// The server and address as configured, which the form starts from
    /// for an account that is not the placeholder.
    pub server: String,
    pub username: String,
    /// A connection is asked for: the session is not offline.
    pub online: bool,
    /// Why the form cannot change the account, when it cannot.
    pub refusal: Option<String>,
}

/// Why the list is empty, when the window has more to say than the error.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Screen {
    /// The account is the placeholder.
    Placeholder,
    /// The portal did not hand over the account's password.
    Credential,
    /// A connection was tried and failed for another reason.
    Unreachable,
}

/// How a password reaches td's credential store, with the keys the
/// compositor binds; no step needs root.
fn store_steps(name: &str) -> String {
    format!(
        "1. Enroll the store, once: press Ctrl+Alt+Esc, then E, with two FIDO2 \
         security keys at hand, and follow the screen. (X enrolls one key instead, \
         and the store is then lost with it.) Enrollment needs the machine's TPM; \
         where there is none, as in td's stock VM, the store stays unenrolled and \
         td-mail cannot be given a password.\n\
         2. After each boot, unlock it: Ctrl+Alt+Esc, then U, and touch the key \
         (R uses the recovery key).\n\
         3. Store the password: put it alone in a file, run \
         td-secret set mail/{name} < FILE in a terminal, then press Ctrl+Alt+Esc, \
         then W, check the target and touch the key. Delete the file afterwards."
    )
}

/// How the account is connected again: `a` reopens the one account, and
/// moves on through several.
fn again(accounts: usize) -> &'static str {
    if accounts > 1 {
        "select this account again with a"
    } else {
        "press a"
    }
}

/// What the window says of an account that is still td-firstboot's
/// placeholder: there is nothing to connect to, and `s` sets it up.
fn placeholder_text(account: &str, setup: &AccountSetup) -> String {
    let mut text = format!(
        "The account \"{account}\" is the placeholder td provisions on first boot. \
         Its server is a reserved example name, so td-mail has nothing to connect \
         to.\n\n"
    );
    if let Some(why) = &setup.refusal {
        text.push_str(&format!("td-mail cannot set it up from here: {why}."));
        return text;
    }
    text.push_str(
        "Press s to set it up. Type the JMAP server, either its host, such as \
         api.fastmail.com, or its full https:// discovery URL, and press Enter; \
         then type the account's address and press Enter. td-mail writes both \
         into its configuration and connects.\n\n",
    );
    match &setup.portal {
        Some(name) => text.push_str(&format!(
            "The password is not typed here. td-mail asks td's credential store for \
             mail/{name}, and the steps to store it are shown if it is not there yet."
        )),
        None => {
            text.push_str("The password comes from the account's password_command, as configured.")
        }
    }
    text
}

/// What the window says when the credential portal did not hand over the
/// account's password: how a password reaches the store, and what the
/// portal said.
fn credential_text(name: &str, error: &str, accounts: usize) -> String {
    format!(
        "td-mail could not get this account's password from td's credential store.\n\n\
         The store hands an application its password only once it is enrolled with \
         hardware security keys and unlocked:\n\n\
         {}\n\
         4. Then {} here to connect again.\n\n\
         What td-mail was told: {error}",
        store_steps(name),
        again(accounts)
    )
}

/// What the window says when a connection was tried and failed for a
/// reason of the server's or the account's: what can be wrong, and how
/// each is changed from here.
fn unreachable_text(account: &str, setup: &AccountSetup, error: &str, accounts: usize) -> String {
    let mut text = format!(
        "td-mail could not connect the account \"{account}\" to {} as {}.\n\n",
        setup.server, setup.username
    );
    match &setup.refusal {
        None => text.push_str("If the server or the address is wrong, press s to change them.\n\n"),
        Some(why) => text.push_str(&format!(
            "td-mail cannot change the server or the address from here: {why}.\n\n"
        )),
    }
    if let Some(name) = &setup.portal {
        text.push_str(&format!(
            "If the password is wrong, store the right one: until one is stored, td's \
             credential store holds a placeholder password for mail/{name}.\n\n{}\n\n",
            store_steps(name)
        ));
    }
    text.push_str(&format!(
        "Then {} here to connect again.\n\nWhat td-mail was told: {error}",
        again(accounts)
    ));
    text
}

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
    setup: AccountSetup,
    /// The setup form's field being typed, while it is open.
    setup_step: Option<SetupStep>,
    /// The discovery URL the form was given, while the address is typed.
    setup_server: String,
    setup_input: String,
    /// What the form last sent, which it starts from if it is opened
    /// again after a refusal.
    setup_sent: Option<(String, String)>,
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
            setup: AccountSetup::default(),
            setup_step: None,
            setup_server: String::new(),
            setup_input: String::new(),
            setup_sent: None,
        }
    }

    /// What the view says of its account when it has no mail to show,
    /// and whether the form may change it.
    pub fn with_setup(mut self, setup: AccountSetup) -> Self {
        self.setup = setup;
        self
    }

    fn screen(&self) -> Option<Screen> {
        if !self.mailboxes.is_empty() {
            return None;
        }
        if self.setup.placeholder {
            return Some(Screen::Placeholder);
        }
        let error = self.error.as_deref()?;
        if self.setup.portal.is_some() && error.contains(crate::PORTAL_FAILURE) {
            return Some(Screen::Credential);
        }
        self.setup.online.then_some(Screen::Unreachable)
    }

    /// Whether `s` opens the form: the account has no server or could not
    /// be reached, and the form can change it.
    fn may_set_up(&self) -> bool {
        self.setup.refusal.is_none()
            && matches!(
                self.screen(),
                Some(Screen::Placeholder | Screen::Unreachable)
            )
    }

    /// The text the body shows in place of an empty list's: the setup
    /// form's step, or what the screen says; none for any other state.
    fn setup_message(&self) -> Option<String> {
        if let Some(step) = self.setup_step {
            return Some(match step {
                SetupStep::Server => "Type the JMAP server: its host, such as api.fastmail.com, \
                     or its full https:// discovery URL, then press Enter. Escape cancels."
                    .to_string(),
                SetupStep::Address => format!(
                    "Server: {}\n\nType the account's address, usually its email address, \
                     then press Enter to save. Escape cancels.",
                    self.setup_server
                ),
            });
        }
        let accounts = self.account_names.len();
        let error = self.error.as_deref().unwrap_or_default();
        match self.screen()? {
            Screen::Placeholder => Some(placeholder_text(&self.current_account, &self.setup)),
            Screen::Credential => {
                let name = self.setup.portal.as_deref()?;
                Some(credential_text(name, error, accounts))
            }
            Screen::Unreachable => Some(unreachable_text(
                &self.current_account,
                &self.setup,
                error,
                accounts,
            )),
        }
    }

    /// Opens the form on what it last sent, or on the account as
    /// configured unless that is the placeholder.
    fn open_setup(&mut self) {
        self.setup_step = Some(SetupStep::Server);
        self.setup_server.clear();
        self.setup_input = match &self.setup_sent {
            Some((server, _)) => server.clone(),
            None if !self.setup.placeholder => self.setup.server.clone(),
            None => String::new(),
        };
        self.status_message = None;
    }

    /// A key while the setup form is open: each field is checked as it is
    /// left, so only a file that cannot be written is refused later.
    fn setup_key(&mut self, step: SetupStep, key: Key) -> ViewAction {
        match key {
            Key::Enter => match step {
                SetupStep::Server => match crate::config::discovery_url(&self.setup_input) {
                    Ok(url) => {
                        self.setup_server = url;
                        self.setup_input = match &self.setup_sent {
                            Some((_, username)) => username.clone(),
                            None if !self.setup.placeholder => self.setup.username.clone(),
                            None => String::new(),
                        };
                        self.setup_step = Some(SetupStep::Address);
                        self.status_message = None;
                    }
                    Err(e) => self.status_message = Some(e),
                },
                SetupStep::Address => match crate::config::setup_username(&self.setup_input) {
                    Ok(username) => {
                        self.setup_step = None;
                        self.setup_input.clear();
                        self.status_message = None;
                        let server = std::mem::take(&mut self.setup_server);
                        self.setup_sent = Some((server.clone(), username.clone()));
                        return ViewAction::SetUpAccount {
                            account: self.current_account.clone(),
                            server,
                            username,
                        };
                    }
                    Err(e) => self.status_message = Some(e),
                },
            },
            // Only Escape leaves: a `q` in a field is a letter.
            Key::Escape => {
                self.setup_step = None;
                self.setup_input.clear();
                self.setup_server.clear();
                self.status_message = None;
            }
            Key::Backspace => {
                self.setup_input.pop();
            }
            Key::Char(c) => self.setup_input.push(c),
            _ => {}
        }
        ViewAction::Continue
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
        let base = if let Some(step) = self.setup_step {
            match step {
                SetupStep::Server => "Server | Enter:next Esc:cancel".to_string(),
                SetupStep::Address => "Address | Enter:save Esc:cancel".to_string(),
            }
        } else if self.create_mode {
            "New folder name | Enter:create Esc:cancel".to_string()
        } else if self.delete_confirm_mode {
            "Confirm delete | y:delete n/Esc:cancel".to_string()
        } else if self.may_set_up() {
            let state = if self.setup.placeholder {
                "Not set up"
            } else {
                "Not connected"
            };
            format!("{state} | s:set up{account_hint} ?:help q:quit")
        } else if self.screen() == Some(Screen::Placeholder) {
            format!("Not set up |{account_hint} ?:help q:quit")
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
        let body = if let Some(message) = self.setup_message() {
            Body::message(message)
        } else if self.loading && self.mailboxes.is_empty() {
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
        let (labels, keys) = if self.setup_step.is_some() {
            (SETUP_LABELS, SETUP_KEYS)
        } else if self.create_mode {
            (CREATE_LABELS, CREATE_KEYS)
        } else if self.delete_confirm_mode {
            (DELETE_LABELS, DELETE_KEYS)
        } else if self.may_set_up() {
            (SETUP_SCREEN_LABELS, SETUP_SCREEN_KEYS)
        } else if self.screen() == Some(Screen::Placeholder) {
            (PLACEHOLDER_LABELS, PLACEHOLDER_KEYS)
        } else {
            (LABELS, KEYS)
        };
        Scene {
            title: self.title(),
            labels,
            keys,
            entry: match self.setup_step {
                Some(SetupStep::Server) => Some(Entry {
                    placeholder: "api.fastmail.com, or https://.../.well-known/jmap",
                    text: &self.setup_input,
                }),
                Some(SetupStep::Address) => Some(Entry {
                    placeholder: "you@yourdomain",
                    text: &self.setup_input,
                }),
                None => self.create_mode.then_some(Entry {
                    placeholder: "New folder name",
                    text: &self.create_input,
                }),
            },
            body,
            status: self.status_line(),
        }
    }

    fn handle_key(&mut self, key: Key, page: usize) -> ViewAction {
        if let Some(step) = self.setup_step {
            return self.setup_key(step, key);
        }
        if !self.create_mode && !self.delete_confirm_mode {
            if key == Key::Char('s') && self.may_set_up() {
                self.open_setup();
                return ViewAction::Continue;
            }
            // A placeholder has no server: only its own keys do anything.
            if self.screen() == Some(Screen::Placeholder)
                && !matches!(key, Key::Char('a' | '?' | 'q'))
            {
                return ViewAction::Continue;
            }
        }
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

    fn body_text(view: &MailboxListView) -> String {
        match view.scene().body {
            Body::Text { text, .. } => text(10_000),
            _ => panic!("expected a text body"),
        }
    }

    fn type_text(view: &mut MailboxListView, text: &str) {
        for c in text.chars() {
            assert!(matches!(
                view.handle_key(Key::Char(c), 10),
                ViewAction::Continue
            ));
        }
    }

    fn clear_field(view: &mut MailboxListView) {
        for _ in 0..600 {
            view.handle_key(Key::Backspace, 10);
        }
    }

    fn setup(placeholder: bool) -> AccountSetup {
        AccountSetup {
            placeholder,
            portal: Some("personal".to_string()),
            server: if placeholder {
                "https://mail.example.com/.well-known/jmap".to_string()
            } else {
                "https://mx.td.dev/.well-known/jmap".to_string()
            },
            username: "me@td.dev".to_string(),
            online: true,
            refusal: None,
        }
    }

    fn failed(reason: &str) -> BackendResponse {
        BackendResponse::Mailboxes(Err(format!(
            "no cached mailboxes available (offline mode: {reason})"
        )))
    }

    fn expect_set_up(action: ViewAction) -> (String, String, String) {
        match action {
            ViewAction::SetUpAccount {
                account,
                server,
                username,
            } => (account, server, username),
            _ => panic!("expected the account to be set up"),
        }
    }

    /// The first-boot placeholder says what it is and how to set it up;
    /// the form hands the session the discovery URL and the address, each
    /// checked as its field is left, and opens again on what it sent.
    #[test]
    fn the_placeholder_is_set_up_from_the_window() {
        let (view, cmd_rx) = make_view();
        let mut view = view.with_setup(setup(true));
        view.on_response(&failed("[account.personal] is the provisioned placeholder"));
        let text = body_text(&view);
        assert!(text.contains("is the placeholder"), "{text}");
        assert!(text.contains("Press s to set it up"), "{text}");
        assert!(text.contains("mail/personal"), "{text}");
        {
            let scene = view.scene();
            assert_eq!(scene.labels, SETUP_SCREEN_LABELS);
            assert_eq!(scene.keys, SETUP_SCREEN_KEYS);
            assert!(scene
                .status
                .starts_with("Not set up | s:set up a:reconnect"));
        }
        // The placeholder has no server: the folder keys do nothing.
        for key in ['+', 'd', 'g', 'u', 'X', 'c'] {
            assert!(matches!(
                view.handle_key(Key::Char(key), 10),
                ViewAction::Continue
            ));
            assert!(view.scene().entry.is_none(), "{key}");
        }
        assert!(cmd_rx.try_recv().is_err());

        // Opened on the placeholder, the form starts empty.
        view.handle_key(Key::Char('s'), 10);
        assert_eq!(view.scene().entry.unwrap().text, "");
        type_text(&mut view, "mail.example.com");
        view.handle_key(Key::Enter, 10);
        assert!(view.scene().status.contains("reserved example name"));
        clear_field(&mut view);
        type_text(&mut view, "mx.td.dev");
        view.handle_key(Key::Enter, 10);
        {
            let scene = view.scene();
            let entry = scene.entry.expect("the address is being typed");
            assert_eq!(entry.placeholder, "you@yourdomain");
            assert_eq!(entry.text, "");
            assert_eq!(scene.labels, SETUP_LABELS);
            assert!(scene.status.starts_with("Address | "));
        }
        assert!(body_text(&view).contains("Server: https://mx.td.dev/.well-known/jmap"));
        // A `q` in a field is a letter, not the quit.
        type_text(&mut view, "q@td.dev");
        let (account, server, username) = expect_set_up(view.handle_key(Key::Enter, 10));
        assert_eq!(account, "personal");
        assert_eq!(server, "https://mx.td.dev/.well-known/jmap");
        assert_eq!(username, "q@td.dev");
        assert!(view.scene().entry.is_none());
        assert!(!view.scene().status.contains("Setting up"));

        // Refused, the view stays: the form opens on what it sent.
        view.handle_key(Key::Char('s'), 10);
        assert_eq!(
            view.scene().entry.unwrap().text,
            "https://mx.td.dev/.well-known/jmap"
        );
        view.handle_key(Key::Enter, 10);
        assert_eq!(view.scene().entry.unwrap().text, "q@td.dev");
        view.handle_key(Key::Escape, 10);
        assert!(view.scene().entry.is_none());
        assert!(body_text(&view).contains("Press s to set it up"));
    }

    /// An account whose connection failed for the server's reason says
    /// what may be wrong, and the form, opened on the account as
    /// configured, changes its server or address.
    #[test]
    fn an_account_that_cannot_connect_can_be_set_up_again() {
        let (view, _cmd_rx) = make_view();
        let mut view = view.with_setup(setup(false));
        view.on_response(&failed("JMAP discovery error: 401 Unauthorized"));
        let text = body_text(&view);
        assert!(
            text.contains("could not connect the account \"personal\" to https://mx.td.dev/.well-known/jmap as me@td.dev"),
            "{text}"
        );
        assert!(text.contains("press s to change them"), "{text}");
        assert!(
            text.contains("placeholder password for mail/personal"),
            "{text}"
        );
        assert!(
            text.contains("td-secret set mail/personal < FILE"),
            "{text}"
        );
        assert!(text.contains("Then press a here"), "{text}");
        assert!(text.contains("401 Unauthorized"), "{text}");
        assert!(view.scene().status.starts_with("Not connected | s:set up"));

        // Naming a folder there is that mode, in the bar and the status row.
        view.handle_key(Key::Char('+'), 10);
        {
            let scene = view.scene();
            assert_eq!(scene.labels, CREATE_LABELS);
            assert!(
                scene.status.starts_with("New folder name | "),
                "{}",
                scene.status
            );
        }
        type_text(&mut view, "s");
        assert_eq!(view.scene().entry.unwrap().text, "s");
        view.handle_key(Key::Escape, 10);

        view.handle_key(Key::Char('s'), 10);
        assert_eq!(
            view.scene().entry.unwrap().text,
            "https://mx.td.dev/.well-known/jmap"
        );
        clear_field(&mut view);
        type_text(&mut view, "mx2.td.dev");
        view.handle_key(Key::Enter, 10);
        assert_eq!(view.scene().entry.unwrap().text, "me@td.dev");
        let (_, server, username) = expect_set_up(view.handle_key(Key::Enter, 10));
        assert_eq!(server, "https://mx2.td.dev/.well-known/jmap");
        assert_eq!(username, "me@td.dev");

        // Mail on the cache is the list, with no screen and no `s`.
        let (view, _cmd_rx) = make_view();
        let mut view = view.with_setup(setup(false));
        view.on_response(&folders());
        view.handle_key(Key::Char('s'), 10);
        assert!(view.scene().entry.is_none());
        assert!(matches!(view.scene().body, Body::List { .. }));
    }

    /// An account whose password the portal did not hand over says how
    /// one is stored, with the portal's words, and offers no form: the
    /// server was never asked.
    #[test]
    fn a_credential_the_portal_withheld_says_how_to_store_one() {
        let refused = format!(
            "{}/app/bin/td-secret get personal: td-secret: credential portal refused the request: org.freedesktop.portal.Error.Failed: credential is unavailable; enroll or unlock through secure attention",
            crate::PORTAL_FAILURE
        );
        let (view, _cmd_rx) = make_view();
        let mut view = view.with_setup(setup(false));
        view.on_response(&failed(&refused));
        let text = body_text(&view);
        assert!(
            text.contains("td-secret set mail/personal < FILE"),
            "{text}"
        );
        assert!(text.contains("Ctrl+Alt+Esc, then E"), "{text}");
        assert!(text.contains("Ctrl+Alt+Esc, then U"), "{text}");
        assert!(text.contains("Ctrl+Alt+Esc, then W"), "{text}");
        assert!(text.contains("4. Then press a here"), "{text}");
        assert!(
            text.contains("enroll or unlock through secure attention"),
            "{text}"
        );
        assert!(!text.contains("su "), "{text}");
        assert!(view.scene().status.contains("a:reconnect"));
        view.handle_key(Key::Char('s'), 10);
        assert!(view.scene().entry.is_none());

        // With several accounts `a` moves on, and the text says so.
        let (cmd_tx, _cmd_rx) = mpsc::channel();
        let mut view = MailboxListView::new(
            cmd_tx,
            "me@td.dev".to_string(),
            None,
            None,
            50,
            vec!["personal".to_string(), "work".to_string()],
            "personal".to_string(),
            "Archive".to_string(),
            "Trash".to_string(),
            Vec::new(),
            None,
        )
        .with_setup(setup(false));
        view.on_response(&failed(&refused));
        assert!(body_text(&view).contains("select this account again with a"));

        // Not on the portal, the bare error stands.
        let (view, _cmd_rx) = make_view();
        let mut view = view.with_setup(AccountSetup {
            portal: None,
            online: false,
            ..setup(false)
        });
        view.on_response(&failed(&refused));
        assert!(!body_text(&view).contains("td-secret set"));
    }

    /// A form that could not write says why instead of offering itself:
    /// a session started offline, or a file it cannot edit.
    #[test]
    fn a_placeholder_the_form_cannot_change_says_why() {
        let (view, _cmd_rx) = make_view();
        let mut view = view.with_setup(AccountSetup {
            online: false,
            refusal: Some("td-mail was started --offline".to_string()),
            ..setup(true)
        });
        view.on_response(&BackendResponse::Mailboxes(Err(
            "no cached mailboxes available (offline mode)".to_string(),
        )));
        let text = body_text(&view);
        assert!(
            text.contains("cannot set it up from here: td-mail was started --offline"),
            "{text}"
        );
        assert!(!text.contains("Press s"), "{text}");
        view.handle_key(Key::Char('s'), 10);
        assert!(view.scene().entry.is_none());
        {
            let scene = view.scene();
            assert_eq!(scene.labels, PLACEHOLDER_LABELS);
            assert_eq!(scene.keys, PLACEHOLDER_KEYS);
            assert_eq!(scene.status, "Not set up | a:reconnect ?:help q:quit");
        }

        // An account that cannot connect says why the form is not offered.
        let (view, _cmd_rx) = make_view();
        let mut view = view.with_setup(AccountSetup {
            refusal: Some("no [account.personal] section to edit".to_string()),
            ..setup(false)
        });
        view.on_response(&failed("JMAP discovery error: refused"));
        let text = body_text(&view);
        assert!(
            text.contains("cannot change the server or the address from here: no [account.personal] section to edit"),
            "{text}"
        );
        assert!(!text.contains("press s"), "{text}");
    }
}

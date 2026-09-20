use crate::backend::{BackendCommand, BackendResponse, EmailMutationAction};
use crate::compose;
use crate::jmap::types::{Email, Mailbox};
use crate::rules;
use crate::ui::input::Key;
use crate::ui::views::help::HelpView;
use crate::ui::views::{
    strip_newlines, text_scroll, wrap_text, Body, Row, Scene, Scroll, View, ViewAction,
};
use std::collections::HashMap;
use std::sync::mpsc;

/// The action bar's labels for the view's modes, and the key each stands
/// for; a press on a label is that key.
const LABELS: &[&str] = &[
    "Back",
    "Reply",
    "Reply all",
    "Forward",
    "Archive",
    "Delete",
    "Move",
    "Links",
    "HTML",
    "Help",
];
const KEYS: &[Key] = &[
    Key::Char('q'),
    Key::Char('r'),
    Key::Char('R'),
    Key::Char('F'),
    Key::Char('a'),
    Key::Char('d'),
    Key::Char('m'),
    Key::Char('b'),
    Key::Char('h'),
    Key::Char('?'),
];
const URL_LABELS: &[&str] = &["Open", "Cancel"];
const URL_KEYS: &[Key] = &[Key::Enter, Key::Escape];
const MOVE_LABELS: &[&str] = &["Move", "Cancel"];
const MOVE_KEYS: &[Key] = &[Key::Enter, Key::Escape];
const ATTACHMENT_LABELS: &[&str] = &["Cancel"];
const ATTACHMENT_KEYS: &[Key] = &[Key::Escape];

/// Where a thread's messages part. The rule is drawn only in the text
/// closure, which alone knows the pane's columns, so until then the line
/// stands as a scalar a message body has no reason to carry (and which the
/// pane would show as U+FFFD if one did).
const SEPARATOR: &str = "\u{1}";

fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// Heuristic check: does this text look like HTML rather than plain text?
/// Checks for common HTML structural tags anywhere in the content.
fn looks_like_html(text: &str) -> bool {
    // Check a reasonable prefix to avoid scanning huge bodies.
    // Walk back to the nearest char boundary so multi-byte UTF-8 doesn't panic.
    let mut end = 2000.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let sample = &text[..end];
    let lower = sample.to_ascii_lowercase();
    lower.contains("<!doctype")
        || lower.contains("<html")
        || lower.contains("<head")
        || lower.contains("<body")
        || lower.contains("<style")
        || lower.contains("<table")
        || lower.contains("<div")
}

/// Convert HTML to the plain text the document pane shows.
///
/// `to_rich` carries the markup as tags rather than characters, and what
/// survives here is what a target is: a link's URL and an image's source,
/// after the text they belong to. Emphasis, code and colour are dropped
/// rather than turned into escape sequences: the pane shows a control
/// scalar as U+FFFD, so an escape would be read as text.
fn html_to_text(html: &str) -> String {
    use crate::html::Tag;

    let mut out = String::new();
    for line in crate::html::to_rich(html.as_bytes(), 80) {
        for span in line {
            out.push_str(&span.text);
            for tag in &span.tags {
                match tag {
                    Tag::Link(url) => out.push_str(&format!(" [{}]", url)),
                    Tag::Image(src) => out.push_str(&format!(" [img: {}]", src)),
                    _ => {}
                }
            }
        }
        out.push('\n');
    }
    out
}

/// Extract unique URLs from text content. Finds http/https URLs.
///
/// `find_urls` answers `https?://[^\s<>\]\)"'`]+` by scanning, which is what
/// this site always wanted: a literal prefix and a byte class, no engine.
fn extract_urls(text: &str) -> Vec<String> {
    let mut seen = Vec::new();
    for m in crate::regex::find_urls(text) {
        let url = m.trim_end_matches(['.', ',', ';', ':', '!', '?']);
        let url = url.to_string();
        if !seen.contains(&url) {
            seen.push(url);
        }
    }
    seen
}

enum PendingWriteOp {
    Flag { old_flagged: bool },
    Seen { old_seen: bool },
}

#[derive(Clone)]
pub struct EmailNavEntry {
    pub id: String,
    pub unread: bool,
}

pub struct EmailView {
    cmd_tx: mpsc::Sender<BackendCommand>,
    reply_from_address: String,
    can_expire_now: bool,
    email_id: String,
    email: Option<Email>,
    /// The message as lines, unwrapped: the pane's columns wrap them.
    lines: Vec<String>,
    /// Bumped whenever `lines` is rebuilt, so the text's key changes with
    /// what it says even when the message is the same one.
    text_generation: u64,
    loading: bool,
    error: Option<String>,
    pending_reply_all: Option<bool>,
    pending_forward: bool,
    pending_compose: Option<compose::ComposeDraft>,
    status_message: Option<String>,
    next_write_op_id: u64,
    pending_write_ops: HashMap<u64, PendingWriteOp>,
    attachment_picking: bool,
    show_all_headers: bool,
    raw_headers_cache: HashMap<String, String>,
    raw_headers_loading: bool,
    thread_id: Option<String>,
    /// The thread's subject as the list gave it, for the window's title.
    thread_subject: String,
    thread_emails: Vec<Email>,
    mailboxes: Vec<Mailbox>,
    archive_folder: String,
    deleted_folder: String,
    move_mode: bool,
    move_cursor: usize,
    prefer_html: bool,
    browser: Option<String>,
    urls: Vec<String>,
    url_picking: bool,
    url_cursor: usize,
    nav_entries: Vec<EmailNavEntry>,
    nav_cursor: usize,
}

impl EmailView {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cmd_tx: mpsc::Sender<BackendCommand>,
        reply_from_address: String,
        email_id: String,
        nav_entries: Vec<EmailNavEntry>,
        nav_cursor: usize,
        can_expire_now: bool,
        mailboxes: Vec<Mailbox>,
        archive_folder: String,
        deleted_folder: String,
        browser: Option<String>,
    ) -> Self {
        EmailView {
            cmd_tx,
            reply_from_address,
            can_expire_now,
            email_id,
            email: None,
            lines: Vec::new(),
            text_generation: 0,
            loading: true,
            error: None,
            pending_reply_all: None,
            pending_forward: false,
            pending_compose: None,
            status_message: None,
            next_write_op_id: 1,
            pending_write_ops: HashMap::new(),
            attachment_picking: false,
            show_all_headers: false,
            raw_headers_cache: HashMap::new(),
            raw_headers_loading: false,
            thread_id: None,
            thread_subject: String::new(),
            thread_emails: Vec::new(),
            mailboxes,
            archive_folder,
            deleted_folder,
            move_mode: false,
            move_cursor: 0,
            prefer_html: false,
            browser,
            urls: Vec::new(),
            url_picking: false,
            url_cursor: 0,
            nav_entries,
            nav_cursor,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_thread(
        cmd_tx: mpsc::Sender<BackendCommand>,
        reply_from_address: String,
        thread_id: String,
        subject: String,
        can_expire_now: bool,
        mailboxes: Vec<Mailbox>,
        archive_folder: String,
        deleted_folder: String,
        browser: Option<String>,
    ) -> Self {
        let _ = cmd_tx.send(BackendCommand::QueryThreadEmails {
            thread_id: thread_id.clone(),
        });
        EmailView {
            cmd_tx,
            reply_from_address,
            can_expire_now,
            email_id: String::new(),
            email: None,
            lines: Vec::new(),
            text_generation: 0,
            loading: true,
            error: None,
            pending_reply_all: None,
            pending_forward: false,
            pending_compose: None,
            status_message: None,
            next_write_op_id: 1,
            pending_write_ops: HashMap::new(),
            attachment_picking: false,
            show_all_headers: false,
            raw_headers_cache: HashMap::new(),
            raw_headers_loading: false,
            thread_id: Some(thread_id),
            thread_subject: subject,
            thread_emails: Vec::new(),
            mailboxes,
            archive_folder,
            deleted_folder,
            move_mode: false,
            move_cursor: 0,
            prefer_html: false,
            browser,
            urls: Vec::new(),
            url_picking: false,
            url_cursor: 0,
            nav_entries: Vec::new(),
            nav_cursor: 0,
        }
    }

    fn set_nav_unread(&mut self, id: &str, unread: bool) {
        if let Some(entry) = self.nav_entries.iter_mut().find(|e| e.id == id) {
            entry.unread = unread;
        }
    }

    fn navigate_unread(&mut self, forward: bool) -> bool {
        if self.nav_entries.is_empty() {
            return false;
        }

        let len = self.nav_entries.len();
        if self.nav_cursor >= len {
            self.nav_cursor = len - 1;
        }

        let mut candidate = None;
        for offset in 1..=len {
            let idx = if forward {
                (self.nav_cursor + offset) % len
            } else {
                (self.nav_cursor + len - (offset % len)) % len
            };
            if self.nav_entries.get(idx).is_some_and(|entry| entry.unread) {
                candidate = Some(idx);
                break;
            }
        }

        let Some(next_idx) = candidate else {
            self.status_message = Some("No unread email".to_string());
            return true;
        };

        // The candidate came from the loop above, so the entry is there; taking
        // its id through `get` keeps the index honest and costs one `Option`.
        let Some(next_id) = self.nav_entries.get(next_idx).map(|entry| entry.id.clone()) else {
            self.status_message = Some("No unread email".to_string());
            return true;
        };
        self.nav_cursor = next_idx;
        self.email_id = next_id;
        self.loading = true;
        self.error = None;
        let _ = self.cmd_tx.send(BackendCommand::GetEmail {
            id: self.email_id.clone(),
        });
        let op_id = self.next_op_id();
        let _ = self.cmd_tx.send(BackendCommand::MarkEmailRead {
            op_id,
            id: self.email_id.clone(),
        });
        if let Some(entry) = self.nav_entries.get_mut(next_idx) {
            entry.unread = false;
        }
        true
    }

    fn render_headers(email: &Email, raw_headers: Option<&str>, lines: &mut Vec<String>) {
        if let Some(raw) = raw_headers {
            for line in raw.lines() {
                lines.push(line.to_string());
            }
        } else {
            if let Some(ref from) = email.from {
                let addrs: Vec<String> = from.iter().map(|a| a.to_string()).collect();
                lines.push(format!("From: {}", addrs.join(", ")));
            }
            if let Some(ref to) = email.to {
                let addrs: Vec<String> = to.iter().map(|a| a.to_string()).collect();
                lines.push(format!("To: {}", addrs.join(", ")));
            }
            if let Some(ref cc) = email.cc {
                if !cc.is_empty() {
                    let addrs: Vec<String> = cc.iter().map(|a| a.to_string()).collect();
                    lines.push(format!("Cc: {}", addrs.join(", ")));
                }
            }
            if let Some(ref date) = email.received_at {
                lines.push(format!("Date: {}", date));
            }
            lines.push(format!(
                "Subject: {}",
                email.subject.as_deref().unwrap_or("(no subject)")
            ));
        }
    }

    /// One message as lines, and the links it carries in the order the
    /// picker numbers them.
    fn render_email(
        email: &Email,
        raw_headers: Option<&str>,
        prefer_html: bool,
    ) -> (Vec<String>, Vec<String>) {
        let mut lines = Vec::new();

        Self::render_headers(email, raw_headers, &mut lines);

        // Attachments
        if let Some(ref attachments) = email.attachments {
            if !attachments.is_empty() {
                lines.push(String::new());
                lines.push(format!("Attachments ({})", attachments.len()));
                for (i, att) in attachments.iter().enumerate() {
                    let name = att.name.as_deref().unwrap_or("unnamed");
                    let size = att.size.map(format_size).unwrap_or_default();
                    let type_str = att.r#type.as_deref().unwrap_or("application/octet-stream");
                    lines.push(format!("  [{}] {} ({}, {})", i + 1, name, type_str, size));
                }
                lines.push("  Press 'A' then 1-9 to download/open".to_string());
            }
        }

        // Separator
        lines.push(String::new());

        // Body
        let body_text = Self::extract_body(email, prefer_html);
        for line in body_text.lines() {
            lines.push(line.to_string());
        }

        // Extract and append URLs
        let urls = extract_urls(&body_text);
        if !urls.is_empty() {
            lines.push(String::new());
            lines.push("Links:".to_string());
            for (i, url) in urls.iter().enumerate() {
                lines.push(format!("  [{}] {}", i + 1, url));
            }
        }

        (lines, urls)
    }

    /// A thread's messages as lines, parted by `SEPARATOR`, with every link
    /// they carry listed once at the end.
    fn render_thread_emails(
        emails: &[Email],
        raw_headers_cache: &HashMap<String, String>,
        prefer_html: bool,
    ) -> (Vec<String>, Vec<String>) {
        let mut lines = Vec::new();
        let mut all_urls = Vec::new();
        for (i, email) in emails.iter().enumerate() {
            if i > 0 {
                lines.push(String::new());
                lines.push(SEPARATOR.to_string());
                lines.push(String::new());
            }
            let raw = raw_headers_cache.get(&email.id).map(|s| s.as_str());
            Self::render_headers(email, raw, &mut lines);
            lines.push(String::new());
            let body_text = Self::extract_body(email, prefer_html);
            for line in body_text.lines() {
                lines.push(line.to_string());
            }
            for url in extract_urls(&body_text) {
                if !all_urls.contains(&url) {
                    all_urls.push(url);
                }
            }
        }
        // Append combined URL list at end
        if !all_urls.is_empty() {
            lines.push(String::new());
            lines.push("Links:".to_string());
            for (i, url) in all_urls.iter().enumerate() {
                lines.push(format!("  [{}] {}", i + 1, url));
            }
        }
        (lines, all_urls)
    }

    fn extract_body(email: &Email, prefer_html: bool) -> String {
        if prefer_html {
            // When user explicitly requests HTML rendering
            if let Some(ref html_body) = email.html_body {
                for part in html_body {
                    if let Some(value) = email.body_values.get(&part.part_id) {
                        return html_to_text(&value.value);
                    }
                }
            }
        }
        // Prefer textBody (plain text) — it preserves the author's formatting
        // and avoids lossy HTML-to-text conversion.
        if let Some(ref text_body) = email.text_body {
            for part in text_body {
                if let Some(value) = email.body_values.get(&part.part_id) {
                    if looks_like_html(&value.value)
                        || part
                            .r#type
                            .as_deref()
                            .map(|t| t.eq_ignore_ascii_case("text/html"))
                            .unwrap_or(false)
                    {
                        return html_to_text(&value.value);
                    }
                    return value.value.clone();
                }
            }
        }
        // Fall back to htmlBody when no plain text is available
        if let Some(ref html_body) = email.html_body {
            for part in html_body {
                if let Some(value) = email.body_values.get(&part.part_id) {
                    return html_to_text(&value.value);
                }
            }
        }

        email.preview.as_deref().unwrap_or("(no body)").to_string()
    }

    fn request_reply(&mut self, reply_all: bool) {
        self.pending_reply_all = Some(reply_all);
        // Fetch the email with reply headers (messageId, references, replyTo, sentAt)
        let _ = self.cmd_tx.send(BackendCommand::GetEmailForReply {
            id: self.email_id.clone(),
        });
    }

    fn next_op_id(&mut self) -> u64 {
        let id = self.next_write_op_id;
        self.next_write_op_id = self.next_write_op_id.wrapping_add(1);
        id
    }

    fn set_flagged(&mut self, flagged: bool) {
        if let Some(ref mut email) = self.email {
            if flagged {
                email.keywords.insert("$flagged".to_string(), true);
            } else {
                email.keywords.remove("$flagged");
            }
        }
    }

    fn set_seen(&mut self, seen: bool) {
        if let Some(ref mut email) = self.email {
            if seen {
                email.keywords.insert("$seen".to_string(), true);
            } else {
                email.keywords.remove("$seen");
            }
        }
    }

    fn download_attachment(&mut self, index: usize) {
        let attachment = self
            .email
            .as_ref()
            .and_then(|e| e.attachments.as_ref())
            .and_then(|a| a.get(index));

        if let Some(att) = attachment {
            if let Some(ref blob_id) = att.blob_id {
                let name = att.name.as_deref().unwrap_or("attachment").to_string();
                let content_type = att
                    .r#type
                    .as_deref()
                    .unwrap_or("application/octet-stream")
                    .to_string();
                self.status_message = Some(format!("Downloading {}...", name));
                let _ = self.cmd_tx.send(BackendCommand::DownloadAttachment {
                    blob_id: blob_id.clone(),
                    name,
                    content_type,
                });
            } else {
                self.status_message = Some("Attachment has no blob ID".to_string());
            }
        } else {
            self.status_message = Some("Invalid attachment number".to_string());
        }
    }

    fn attachment_count(&self) -> usize {
        self.email
            .as_ref()
            .and_then(|e| e.attachments.as_ref())
            .map(|a| a.len())
            .unwrap_or(0)
    }

    fn open_url(&mut self, index: usize) {
        if let Some(url) = self.urls.get(index) {
            let browser = self
                .browser
                .clone()
                .or_else(|| std::env::var("BROWSER").ok())
                .unwrap_or_else(|| {
                    if cfg!(target_os = "macos") {
                        "open".to_string()
                    } else {
                        "xdg-open".to_string()
                    }
                });
            // A shell, so a browser command may carry arguments; the URL is
            // its `$1`, never part of the command.
            let script = browser_script(&browser);
            crate::log_info!("[Browser] running: {} with {}", script, url);
            match std::process::Command::new("sh")
                .arg("-f")
                .arg("-c")
                .arg(&script)
                .arg("sh")
                .arg(url)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
            {
                Ok(child) => {
                    crate::ui::reap_in_background(child);
                    self.status_message = Some(format!("Opening [{}]...", index + 1));
                }
                Err(e) => {
                    crate::log_error!("[Browser] failed to run '{}': {}", script, e);
                    self.status_message =
                        Some(format!("Failed to open browser '{}': {}", browser, e));
                }
            }
        } else {
            self.status_message = Some("Invalid URL number".to_string());
        }
    }

    /// Rebuilds the lines and the links from what is loaded, and bumps the
    /// generation so the pane reloads the text its key now names.
    fn rerender_lines(&mut self) {
        let rebuilt = if self.thread_id.is_some() && !self.thread_emails.is_empty() {
            let empty = HashMap::new();
            let cache = if self.show_all_headers {
                &self.raw_headers_cache
            } else {
                // Empty map = use structured headers
                &empty
            };
            Some(Self::render_thread_emails(
                &self.thread_emails,
                cache,
                self.prefer_html,
            ))
        } else if let Some(ref email) = self.email {
            let raw = if self.show_all_headers {
                self.raw_headers_cache.get(&email.id).map(|s| s.as_str())
            } else {
                None
            };
            Some(Self::render_email(email, raw, self.prefer_html))
        } else {
            None
        };
        let Some((lines, urls)) = rebuilt else {
            return;
        };
        self.lines = lines;
        self.urls = urls;
        self.text_generation = self.text_generation.wrapping_add(1);
    }

    fn rollback_pending_write(&mut self, op: PendingWriteOp) {
        match op {
            PendingWriteOp::Flag { old_flagged } => self.set_flagged(old_flagged),
            PendingWriteOp::Seen { old_seen } => {
                self.set_seen(old_seen);
                let id = self.email_id.clone();
                self.set_nav_unread(&id, !old_seen);
            }
        }
    }

    fn move_to_folder(&mut self, folder: &str, action_label: &str) -> ViewAction {
        let Some(target_id) = rules::resolve_mailbox_id(folder, &self.mailboxes) else {
            self.status_message = Some(format!(
                "{} failed: could not resolve folder '{}'",
                action_label, folder
            ));
            return ViewAction::Continue;
        };

        let op_id = self.next_op_id();
        let send_result = if let Some(thread_id) = &self.thread_id {
            self.cmd_tx.send(BackendCommand::MoveThread {
                op_id,
                thread_id: thread_id.clone(),
                to_mailbox_id: target_id,
            })
        } else {
            self.cmd_tx.send(BackendCommand::MoveEmail {
                op_id,
                id: self.email_id.clone(),
                to_mailbox_id: target_id,
            })
        };

        match send_result {
            Ok(()) => ViewAction::Pop,
            Err(e) => {
                self.status_message = Some(format!("{} failed: {}", action_label, e));
                ViewAction::Continue
            }
        }
    }

    fn move_to_mailbox_id(&mut self, target_id: String) -> ViewAction {
        let op_id = self.next_op_id();
        let send_result = if let Some(thread_id) = &self.thread_id {
            self.cmd_tx.send(BackendCommand::MoveThread {
                op_id,
                thread_id: thread_id.clone(),
                to_mailbox_id: target_id,
            })
        } else {
            self.cmd_tx.send(BackendCommand::MoveEmail {
                op_id,
                id: self.email_id.clone(),
                to_mailbox_id: target_id,
            })
        };

        match send_result {
            Ok(()) => ViewAction::Pop,
            Err(e) => {
                self.status_message = Some(format!("Move failed: {}", e));
                ViewAction::Continue
            }
        }
    }

    /// True if the displayed message currently lives in a mailbox with `role`.
    fn email_in_role(&self, role: &str) -> bool {
        let Some(ref email) = self.email else {
            return false;
        };
        self.mailboxes
            .iter()
            .any(|m| m.role.as_deref() == Some(role) && email.mailbox_ids.contains_key(&m.id))
    }

    /// Train the spam classifier on this message, relocating only when that
    /// actually changes folders: spam files to Junk (unless already there); ham
    /// rescues to Inbox only from Junk. Otherwise it just trains in place so the
    /// message doesn't needlessly move (and the view doesn't close).
    fn mark_spam(&mut self, is_spam: bool) -> ViewAction {
        let _ = self.cmd_tx.send(BackendCommand::TrainMessage {
            origin: "email_view".to_string(),
            id: self.email_id.clone(),
            spam: is_spam,
        });
        if is_spam {
            if !self.email_in_role("junk") {
                return self.move_to_folder("junk", "Mark spam");
            }
        } else if self.email_in_role("junk") {
            return self.move_to_folder("inbox", "Mark not-spam");
        }
        self.status_message = Some(format!(
            "Trained as {}",
            if is_spam { "spam" } else { "not-spam (ham)" }
        ));
        ViewAction::Continue
    }

    fn expire_now(&mut self) -> ViewAction {
        if !self.can_expire_now {
            self.status_message =
                Some("Expire is only available in the deleted folder".to_string());
            return ViewAction::Continue;
        }

        let op_id = self.next_op_id();
        let send_result = if let Some(thread_id) = &self.thread_id {
            self.cmd_tx.send(BackendCommand::DestroyThread {
                op_id,
                thread_id: thread_id.clone(),
            })
        } else {
            self.cmd_tx.send(BackendCommand::DestroyEmail {
                op_id,
                id: self.email_id.clone(),
            })
        };

        match send_result {
            Ok(()) => ViewAction::Pop,
            Err(e) => {
                self.status_message = Some(format!("Expire failed: {}", e));
                ViewAction::Continue
            }
        }
    }

    /// The subject the window names: the thread's, as the list gave it, or
    /// the loaded message's.
    fn subject(&self) -> &str {
        if !self.thread_subject.is_empty() {
            return &self.thread_subject;
        }
        self.email
            .as_ref()
            .and_then(|email| email.subject.as_deref())
            .filter(|subject| !subject.is_empty())
            .unwrap_or("(no subject)")
    }

    fn title(&self) -> String {
        match (self.loading, self.thread_id.is_some()) {
            (true, true) => "Thread".to_string(),
            (true, false) => "Email".to_string(),
            (false, true) => format!("Thread: {}", strip_newlines(self.subject())),
            (false, false) => strip_newlines(self.subject()),
        }
    }

    /// The key of the text shown: what it is of, the generation its lines
    /// were built in, and the flags that change what they say, so the pane
    /// reloads exactly when the text differs.
    fn text_key(&self) -> String {
        let id = self.thread_id.as_deref().unwrap_or(&self.email_id);
        format!(
            "email:{}:{}:{}{}",
            id,
            self.text_generation,
            if self.show_all_headers { 'v' } else { '-' },
            if self.prefer_html { 'h' } else { '-' }
        )
    }

    fn body(&self) -> Body<'_> {
        if self.loading {
            let note = if self.thread_id.is_some() {
                "Loading thread..."
            } else {
                "Loading email..."
            };
            return Body::message(note.to_string());
        }
        if let Some(ref error) = self.error {
            return Body::message(error.clone());
        }
        if self.url_picking {
            return Body::List {
                total: self.urls.len(),
                selected: self.url_cursor,
                row: Box::new(move |index| Row {
                    label: self.urls.get(index).cloned().unwrap_or_default(),
                    meta: format!("[{}]", index + 1),
                    marked: false,
                }),
            };
        }
        if self.move_mode {
            return Body::List {
                total: self.mailboxes.len(),
                selected: self.move_cursor,
                row: Box::new(move |index| Row {
                    label: self
                        .mailboxes
                        .get(index)
                        .map(|mailbox| strip_newlines(&mailbox.name))
                        .unwrap_or_default(),
                    meta: String::new(),
                    marked: false,
                }),
            };
        }
        Body::Text {
            key: self.text_key(),
            text: Box::new(move |columns| {
                let mut text = String::new();
                for (index, line) in self.lines.iter().enumerate() {
                    if index > 0 {
                        text.push('\n');
                    }
                    if line == SEPARATOR {
                        text.push_str(&"─".repeat(columns));
                    } else {
                        text.push_str(line);
                    }
                }
                wrap_text(&text, columns)
            }),
        }
    }

    fn status(&self) -> String {
        let base = if self.loading {
            "Loading... | q:back".to_string()
        } else if self.error.is_some() {
            "q:back".to_string()
        } else if self.url_picking {
            format!(
                "Open URL [1-{}] n/p:navigate RET:open or any key to cancel",
                self.urls.len()
            )
        } else if self.move_mode {
            format!(
                "{}/{} | n/p:navigate RET:move Esc:cancel",
                self.move_cursor + 1,
                self.mailboxes.len()
            )
        } else if self.attachment_picking {
            format!(
                "Pick attachment [1-{}] or any key to cancel",
                self.attachment_count()
            )
        } else if self.pending_reply_all.is_some() || self.pending_forward {
            "Loading reply data... | q:back".to_string()
        } else {
            let att_hint = if self.attachment_count() > 0 {
                " A:attach"
            } else {
                ""
            };
            let expire_hint = if self.can_expire_now { " D:expire" } else { "" };
            let url_hint = if !self.urls.is_empty() {
                " b:links"
            } else {
                ""
            };
            format!(
                "q:back n/p:unread j/k:scroll r:reply R:reply-all F:forward h:html{}{}{} a:archive d:delete m:move J:spam H:ham S:score ?:help",
                att_hint, expire_hint, url_hint
            )
        };
        match self.status_message {
            Some(ref msg) => format!("{} | {}", msg, base),
            None => base,
        }
    }
}

impl View for EmailView {
    fn scene(&self) -> Scene<'_> {
        let (labels, keys) = if self.url_picking {
            (URL_LABELS, URL_KEYS)
        } else if self.move_mode {
            (MOVE_LABELS, MOVE_KEYS)
        } else if self.attachment_picking {
            (ATTACHMENT_LABELS, ATTACHMENT_KEYS)
        } else {
            (LABELS, KEYS)
        };
        Scene {
            title: self.title(),
            labels,
            keys,
            entry: None,
            body: self.body(),
            status: self.status(),
        }
    }

    fn handle_key(&mut self, key: Key, _page: usize) -> ViewAction {
        // Attachment picking mode: waiting for digit
        if self.attachment_picking {
            self.attachment_picking = false;
            if let Key::Char(c @ '1'..='9') = key {
                let index = (c as usize) - ('1' as usize);
                self.download_attachment(index);
            } else {
                self.status_message = Some("Cancelled".to_string());
            }
            return ViewAction::Continue;
        }

        // URL picking mode: the links are a list here, so a press on one
        // opens it as Enter opens the one under the cursor, and the wheel
        // moves the cursor rather than cancelling.
        if self.url_picking {
            self.url_picking = false;
            match key {
                Key::Char(c @ '1'..='9') => {
                    let index = (c as usize) - ('1' as usize);
                    self.open_url(index);
                }
                Key::Click(index) => {
                    if index < self.urls.len() {
                        self.url_cursor = index;
                    }
                    self.open_url(index);
                }
                Key::Char('n') | Key::Char('j') | Key::Down | Key::ScrollDown => {
                    if self.url_cursor + 1 < self.urls.len() {
                        self.url_cursor += 1;
                    }
                    self.url_picking = true; // stay in picking mode
                }
                Key::Char('p') | Key::Char('k') | Key::Up | Key::ScrollUp => {
                    if self.url_cursor > 0 {
                        self.url_cursor -= 1;
                    }
                    self.url_picking = true; // stay in picking mode
                }
                Key::Enter => {
                    self.open_url(self.url_cursor);
                }
                _ => {
                    self.status_message = Some("Cancelled".to_string());
                }
            }
            return ViewAction::Continue;
        }

        // Move mode: mailbox picker
        if self.move_mode {
            match key {
                Key::Escape | Key::Char('q') => {
                    self.move_mode = false;
                }
                Key::Click(index) => {
                    if index < self.mailboxes.len() {
                        self.move_cursor = index;
                    }
                }
                Key::Char('n') | Key::Char('j') | Key::Down | Key::ScrollDown => {
                    if self.move_cursor + 1 < self.mailboxes.len() {
                        self.move_cursor += 1;
                    }
                }
                Key::Char('p') | Key::Char('k') | Key::Up | Key::ScrollUp => {
                    if self.move_cursor > 0 {
                        self.move_cursor -= 1;
                    }
                }
                Key::Enter => {
                    if let Some(target_id) =
                        self.mailboxes.get(self.move_cursor).map(|m| m.id.clone())
                    {
                        self.move_mode = false;
                        return self.move_to_mailbox_id(target_id);
                    }
                }
                _ => {}
            }
            return ViewAction::Continue;
        }

        // Reading the message is the pane's; the view keeps only the keys
        // that mean something else here.
        if let Some(scroll) = text_scroll(key) {
            return ViewAction::Scroll(scroll);
        }

        match key {
            Key::Char('q') => ViewAction::Pop,
            // n/p are next/previous unread; with no unread message to go to
            // they read on, as j/k do.
            Key::Char('n') => {
                if self.navigate_unread(true) {
                    ViewAction::Continue
                } else {
                    ViewAction::Scroll(Scroll::Lines(1))
                }
            }
            Key::Char('p') => {
                if self.navigate_unread(false) {
                    ViewAction::Continue
                } else {
                    ViewAction::Scroll(Scroll::Lines(-1))
                }
            }
            Key::Char('r') => {
                self.request_reply(false);
                ViewAction::Continue
            }
            Key::Char('R') => {
                self.request_reply(true);
                ViewAction::Continue
            }
            Key::Char('F') => {
                // Forward as attachment: fetch the full raw message and embed it
                // as a message/rfc822 part so the HTML part is preserved.
                let _ = self.cmd_tx.send(BackendCommand::GetEmailRaw {
                    id: self.email_id.clone(),
                });
                self.status_message = Some("Preparing forward (attachment)...".to_string());
                ViewAction::Continue
            }
            Key::Char('f') => {
                // Forward as inline quoted text.
                self.pending_forward = true;
                let _ = self.cmd_tx.send(BackendCommand::GetEmailForReply {
                    id: self.email_id.clone(),
                });
                ViewAction::Continue
            }
            Key::Char('*') => {
                if let Some(ref email) = self.email {
                    let old_flagged = email.keywords.contains_key("$flagged");
                    let new_flagged = !old_flagged;
                    let op_id = self.next_op_id();
                    self.pending_write_ops
                        .insert(op_id, PendingWriteOp::Flag { old_flagged });
                    self.set_flagged(new_flagged);
                    if let Err(e) = self.cmd_tx.send(BackendCommand::SetEmailFlagged {
                        op_id,
                        id: self.email_id.clone(),
                        flagged: new_flagged,
                    }) {
                        self.pending_write_ops.remove(&op_id);
                        self.set_flagged(old_flagged);
                        self.status_message = Some(format!("Flag update failed: {}", e));
                    }
                }
                ViewAction::Continue
            }
            Key::Char('u') => {
                if let Some(ref email) = self.email {
                    let old_seen = email.keywords.contains_key("$seen");
                    let new_seen = !old_seen;
                    let op_id = self.next_op_id();
                    self.pending_write_ops
                        .insert(op_id, PendingWriteOp::Seen { old_seen });
                    self.set_seen(new_seen);
                    let id = self.email_id.clone();
                    self.set_nav_unread(&id, !new_seen);
                    let send_result = if new_seen {
                        self.cmd_tx.send(BackendCommand::MarkEmailRead {
                            op_id,
                            id: self.email_id.clone(),
                        })
                    } else {
                        self.cmd_tx.send(BackendCommand::MarkEmailUnread {
                            op_id,
                            id: self.email_id.clone(),
                        })
                    };
                    if let Err(e) = send_result {
                        self.pending_write_ops.remove(&op_id);
                        self.set_seen(old_seen);
                        self.status_message = Some(format!("Read state update failed: {}", e));
                    }
                }
                ViewAction::Continue
            }
            Key::Char('D') => self.expire_now(),
            Key::Char('a') => {
                let target = self.archive_folder.clone();
                self.move_to_folder(&target, "Archive")
            }
            Key::Char('d') => {
                let target = self.deleted_folder.clone();
                self.move_to_folder(&target, "Delete")
            }
            Key::Char('J') => self.mark_spam(true),
            Key::Char('H') => self.mark_spam(false),
            Key::Char('S') => {
                let _ = self.cmd_tx.send(BackendCommand::ClassifyMessage {
                    origin: "email_view".to_string(),
                    id: self.email_id.clone(),
                });
                self.status_message = Some("Scoring message...".to_string());
                ViewAction::Continue
            }
            Key::Char('m') => {
                if !self.mailboxes.is_empty() {
                    self.move_mode = true;
                    self.move_cursor = 0;
                }
                ViewAction::Continue
            }
            Key::Char('A') => {
                let count = self.attachment_count();
                if count == 0 {
                    self.status_message = Some("No attachments".to_string());
                } else if count == 1 {
                    self.download_attachment(0);
                } else {
                    self.attachment_picking = true;
                    self.status_message = Some(format!("Download attachment [1-{}]:", count));
                }
                ViewAction::Continue
            }
            Key::Char('c') => {
                let draft = compose::build_compose_draft(&self.reply_from_address);
                ViewAction::Compose(draft.into())
            }
            Key::Char('v') => {
                self.show_all_headers = !self.show_all_headers;
                if self.show_all_headers {
                    // Fetch raw headers for emails that aren't cached yet
                    let ids_to_fetch: Vec<String> = if self.thread_id.is_some() {
                        self.thread_emails
                            .iter()
                            .filter(|e| !self.raw_headers_cache.contains_key(&e.id))
                            .map(|e| e.id.clone())
                            .collect()
                    } else {
                        let id = self.email_id.clone();
                        if self.raw_headers_cache.contains_key(&id) {
                            vec![]
                        } else {
                            vec![id]
                        }
                    };
                    if ids_to_fetch.is_empty() {
                        self.rerender_lines();
                    } else {
                        self.raw_headers_loading = true;
                        self.status_message = Some("Loading raw headers...".to_string());
                        for id in ids_to_fetch {
                            let _ = self.cmd_tx.send(BackendCommand::GetEmailRawHeaders { id });
                        }
                    }
                } else {
                    self.rerender_lines();
                }
                ViewAction::Continue
            }
            Key::Char('h') => {
                self.prefer_html = !self.prefer_html;
                self.status_message = Some(if self.prefer_html {
                    "Showing HTML body".to_string()
                } else {
                    "Showing plain text body".to_string()
                });
                self.rerender_lines();
                ViewAction::Continue
            }
            Key::Char('b') => {
                if self.urls.is_empty() {
                    self.status_message = Some("No URLs in this message".to_string());
                } else if self.urls.len() == 1 {
                    self.open_url(0);
                } else {
                    self.url_picking = true;
                    self.url_cursor = 0;
                    self.status_message = Some(format!(
                        "Open URL [1-{}] or n/p to navigate, RET to open:",
                        self.urls.len()
                    ));
                }
                ViewAction::Continue
            }
            Key::Char(c @ '1'..='9') => {
                let index = (c as usize) - ('1' as usize);
                if index < self.urls.len() {
                    self.open_url(index);
                }
                ViewAction::Continue
            }
            Key::Char('?') => ViewAction::Push(Box::new(HelpView::new())),
            _ => ViewAction::Continue,
        }
    }

    fn on_response(&mut self, response: &BackendResponse) -> bool {
        match response {
            BackendResponse::ThreadEmails { thread_id, emails }
                if self.thread_id.as_deref() == Some(thread_id) =>
            {
                self.loading = false;
                match emails {
                    Ok(emails) => {
                        self.thread_emails = emails.clone();
                        if let Some(last) = emails.last() {
                            self.email_id = last.id.clone();
                            self.email = Some(last.clone());
                        }
                        self.rerender_lines();
                        self.error = None;
                        // Mark all unread thread emails as read
                        let unread_ids: Vec<String> = emails
                            .iter()
                            .filter(|e| !e.keywords.contains_key("$seen"))
                            .map(|e| e.id.clone())
                            .collect();
                        if !unread_ids.is_empty() {
                            let _ = self.cmd_tx.send(BackendCommand::MarkThreadRead {
                                thread_id: thread_id.clone(),
                                email_ids: unread_ids,
                            });
                        }
                    }
                    Err(e) => {
                        self.error = Some(format!("Failed to load thread: {}", e));
                    }
                }
                true
            }
            BackendResponse::ThreadMarkedRead { .. } => {
                // Silently consume; no UI update needed
                false
            }
            BackendResponse::EmailBody { id, result } if *id == self.email_id => {
                self.loading = false;
                match result.as_ref() {
                    Ok(email) => {
                        self.set_nav_unread(&email.id, !email.keywords.contains_key("$seen"));
                        if let Some(idx) = self.nav_entries.iter().position(|e| e.id == email.id) {
                            self.nav_cursor = idx;
                        }
                        self.email = Some(email.clone());
                        self.rerender_lines();
                        self.error = None;
                        self.pending_write_ops.clear();
                    }
                    Err(e) => {
                        self.error = Some(format!("Failed to load email: {}", e));
                    }
                }
                true
            }
            BackendResponse::EmailForReply { id, result } if *id == self.email_id => {
                let reply_all = self.pending_reply_all.take();
                let is_forward = self.pending_forward;
                self.pending_forward = false;
                match result.as_ref() {
                    Ok(email) => {
                        self.email = Some(email.clone());
                        if is_forward {
                            let draft =
                                compose::build_forward_draft(email, &self.reply_from_address);
                            self.pending_compose = Some(draft.into());
                        } else if let Some(reply_all) = reply_all {
                            let draft = compose::build_reply_draft(
                                email,
                                reply_all,
                                &self.reply_from_address,
                            );
                            self.pending_compose = Some(draft.into());
                        }
                    }
                    Err(e) => {
                        self.error = Some(format!("Failed to load reply data: {}", e));
                    }
                }
                true
            }
            BackendResponse::EmailRaw { id, result } if *id == self.email_id => {
                match result {
                    Ok(raw) => {
                        let draft = compose::build_forward_attachment_draft(
                            self.email.as_ref(),
                            raw.clone().into_bytes(),
                            &self.reply_from_address,
                        );
                        self.pending_compose = Some(draft);
                        self.status_message = None;
                    }
                    Err(e) => {
                        self.error = Some(format!("Failed to load message for forward: {}", e));
                    }
                }
                true
            }
            BackendResponse::EmailMutation {
                op_id,
                id,
                action,
                result,
            } if *id == self.email_id => {
                if let Some(pending) = self.pending_write_ops.remove(op_id) {
                    if let Err(e) = result {
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
                    true
                } else {
                    false
                }
            }
            BackendResponse::AttachmentDownloaded { name, result } => {
                match result {
                    Ok(path) => {
                        self.status_message = Some(format!("Saved: {}", path.display()));
                        // Try to open with xdg-open / open
                        let opener = std::env::var("OPENER").unwrap_or_else(|_| {
                            if cfg!(target_os = "macos") {
                                "open".to_string()
                            } else {
                                "xdg-open".to_string()
                            }
                        });
                        match std::process::Command::new(&opener)
                            .arg(path)
                            .stdin(std::process::Stdio::null())
                            .stdout(std::process::Stdio::null())
                            .stderr(std::process::Stdio::null())
                            .spawn()
                        {
                            Ok(child) => crate::ui::reap_in_background(child),
                            Err(e) => {
                                self.status_message =
                                    Some(format!("Saved {} (could not open: {})", name, e));
                            }
                        }
                    }
                    Err(e) => {
                        self.status_message = Some(format!("Download failed: {}", e));
                    }
                }
                true
            }
            BackendResponse::EmailRawHeaders { id, result } => {
                match result {
                    Ok(headers) => {
                        self.raw_headers_cache.insert(id.clone(), headers.clone());
                    }
                    Err(e) => {
                        self.status_message = Some(format!("Failed to load raw headers: {}", e));
                    }
                }
                // Check if all requested headers have arrived
                let all_loaded = if self.thread_id.is_some() {
                    self.thread_emails
                        .iter()
                        .all(|e| self.raw_headers_cache.contains_key(&e.id))
                } else {
                    self.raw_headers_cache.contains_key(&self.email_id)
                };
                if all_loaded {
                    self.raw_headers_loading = false;
                    self.status_message = None;
                    if self.show_all_headers {
                        self.rerender_lines();
                    }
                }
                true
            }
            BackendResponse::MessageClassified { id, result } if *id == self.email_id => {
                self.status_message = Some(match result {
                    Ok((score, verdict)) => {
                        format!("Spam score: {:.3} -> {}", score, verdict)
                    }
                    Err(e) => format!("Classify failed: {}", e),
                });
                true
            }
            _ => false,
        }
    }

    fn take_pending_action(&mut self) -> Option<ViewAction> {
        self.pending_compose.take().map(ViewAction::Compose)
    }
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

#[cfg(test)]
mod browser_script_tests {
    use super::browser_script;

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
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn make_email(id: &str, subject: &str, preview: &str) -> Email {
        Email {
            id: id.to_string(),
            thread_id: None,
            from: None,
            to: None,
            cc: None,
            reply_to: None,
            subject: Some(subject.to_string()),
            received_at: Some("2025-01-01".to_string()),
            sent_at: None,
            preview: Some(preview.to_string()),
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

    /// A view on one loaded message. Its browser is `true`, so opening a
    /// link runs a command that does nothing.
    fn loaded_view(preview: &str) -> (EmailView, mpsc::Receiver<BackendCommand>) {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let mut view = EmailView::new(
            cmd_tx,
            "me@example.com".to_string(),
            "e1".to_string(),
            Vec::new(),
            0,
            false,
            Vec::new(),
            "Archive".to_string(),
            "Trash".to_string(),
            Some("true".to_string()),
        );
        view.on_response(&BackendResponse::EmailBody {
            id: "e1".to_string(),
            result: Box::new(Ok(make_email("e1", "Hello", preview))),
        });
        (view, cmd_rx)
    }

    /// The key of the text the scene shows, and the text itself.
    fn shown_text(view: &EmailView, columns: usize) -> (String, String) {
        match view.scene().body {
            Body::Text { key, text } => (key, text(columns)),
            Body::List { .. } => panic!("the message is a text"),
        }
    }

    #[test]
    fn the_message_is_the_panes_text_titled_by_its_subject() {
        let (view, _rx) = loaded_view("plain body");
        {
            let scene = view.scene();
            assert_eq!(scene.title, "Hello");
            assert_eq!(scene.labels, LABELS);
            assert_eq!(scene.labels.len(), scene.keys.len());
        }
        let (_, text) = shown_text(&view, 40);
        assert!(text.contains("Subject: Hello"), "{text}");
        assert!(text.contains("plain body"), "{text}");
    }

    #[test]
    fn the_texts_key_changes_when_the_headers_toggle_and_when_the_raw_headers_arrive() {
        let (mut view, _rx) = loaded_view("plain body");
        let (first, _) = shown_text(&view, 40);
        view.handle_key(Key::Char('v'), 20);
        let (toggled, _) = shown_text(&view, 40);
        assert_ne!(first, toggled, "the raw headers were asked for");
        view.on_response(&BackendResponse::EmailRawHeaders {
            id: "e1".to_string(),
            result: Ok("X-Mailer: td-mail\nSubject: Hello".to_string()),
        });
        let (arrived, text) = shown_text(&view, 60);
        assert_ne!(toggled, arrived, "the raw headers arrived");
        assert!(text.contains("X-Mailer: td-mail"), "{text}");
    }

    #[test]
    fn the_url_picker_lists_the_links_and_a_press_opens_the_one_pressed() {
        let (mut view, _rx) = loaded_view("see https://a.example/1 and https://b.example/2");
        view.handle_key(Key::Char('b'), 20);
        {
            let scene = view.scene();
            assert_eq!(scene.labels, URL_LABELS);
            let Body::List {
                total,
                selected,
                row,
            } = scene.body
            else {
                panic!("the links are a list");
            };
            assert_eq!((total, selected), (2, 0));
            assert_eq!(
                row(0),
                Row {
                    label: "https://a.example/1".to_string(),
                    meta: "[1]".to_string(),
                    marked: false,
                }
            );
            assert_eq!(row(1).label, "https://b.example/2");
            assert_eq!(row(1).meta, "[2]");
        }
        view.handle_key(Key::Click(1), 20);
        assert!(!view.url_picking, "the press left the picker");
        assert_eq!(view.url_cursor, 1);
        assert_eq!(view.status_message.as_deref(), Some("Opening [2]..."));
    }

    #[test]
    fn the_reading_keys_are_the_panes_and_n_reads_on_with_nothing_unread() {
        let (mut view, _rx) = loaded_view("plain body");
        assert!(matches!(
            view.handle_key(Key::Char('j'), 20),
            ViewAction::Scroll(Scroll::Lines(1))
        ));
        assert!(matches!(
            view.handle_key(Key::PageDown, 20),
            ViewAction::Scroll(Scroll::Pages(1))
        ));
        assert!(matches!(
            view.handle_key(Key::End, 20),
            ViewAction::Scroll(Scroll::Chord("End"))
        ));
        // No nav entries: n and p read on rather than jumping.
        assert!(matches!(
            view.handle_key(Key::Char('n'), 20),
            ViewAction::Scroll(Scroll::Lines(1))
        ));
        assert!(matches!(
            view.handle_key(Key::Char('p'), 20),
            ViewAction::Scroll(Scroll::Lines(-1))
        ));
    }

    #[test]
    fn a_threads_messages_are_parted_by_a_rule_across_the_pane() {
        let (cmd_tx, _cmd_rx) = mpsc::channel();
        let mut view = EmailView::new_thread(
            cmd_tx,
            "me@example.com".to_string(),
            "t1".to_string(),
            "The subject".to_string(),
            false,
            Vec::new(),
            "Archive".to_string(),
            "Trash".to_string(),
            None,
        );
        assert_eq!(view.scene().title, "Thread");
        view.on_response(&BackendResponse::ThreadEmails {
            thread_id: "t1".to_string(),
            emails: Ok(vec![
                make_email("e1", "The subject", "first"),
                make_email("e2", "Re: The subject", "second"),
            ]),
        });
        assert_eq!(view.scene().title, "Thread: The subject");
        let (_, text) = shown_text(&view, 12);
        assert!(text.contains(&"─".repeat(12)), "{text}");
        assert!(text.contains("first") && text.contains("second"), "{text}");
    }

    #[test]
    fn the_html_body_is_plain_text_carrying_the_link_and_image_targets() {
        let text = html_to_text(
            "<p><b>Bold</b> <a href=\"http://e/x\">go</a> <img src=\"p.png\" alt=\"Pic\"></p>",
        );
        assert!(!text.contains('\u{1b}'), "no escape sequences: {text}");
        assert!(text.contains("Bold"), "{text}");
        assert!(text.contains("go [http://e/x]"), "{text}");
        assert!(text.contains("Pic [img: p.png]"), "{text}");
    }
}

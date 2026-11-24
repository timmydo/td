use crate::backend::{BackendCommand, BackendResponse, EmailMutationAction};
use crate::compose;
use crate::jmap::types::{Email, Mailbox};
use crate::rules;
use crate::ui::input::Key;
use crate::ui::page::{Held, Paged};
use crate::ui::pictures::{self, Answered, Pictures};
use crate::ui::views::{
    strip_newlines, text_scroll, wrap_text, Body, Row, Scene, Scroll, View, ViewAction,
};
use std::collections::HashMap;
use std::sync::mpsc;
use td_ui::keys;
use td_ui::prose::Faces;
use td_ui::raster::{Rect, Scale};

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
    "Attachments",
    "Links",
    "HTML",
    keys::BUTTON,
];
const KEYS: &[Key] = &[
    Key::Char('q'),
    Key::Char('r'),
    Key::Char('R'),
    Key::Char('F'),
    Key::Char('a'),
    Key::Char('d'),
    Key::Char('m'),
    Key::Char('A'),
    Key::Char('b'),
    Key::Char('h'),
    Key::Char('?'),
];
const URL_LABELS: &[&str] = &["Open", "Cancel"];
const URL_KEYS: &[Key] = &[Key::Enter, Key::Escape];
const MOVE_LABELS: &[&str] = &["Move", "Cancel"];
const MOVE_KEYS: &[Key] = &[Key::Enter, Key::Escape];
const ATTACHMENT_LABELS: &[&str] = &["Open", "Cancel"];
const ATTACHMENT_KEYS: &[Key] = &[Key::Enter, Key::Escape];

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
    use td_html::Tag;

    let mut out = String::new();
    for line in td_html::to_rich_within(html.as_bytes(), 80, crate::compose::HTML_LIMITS) {
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
    let mut urls = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for m in crate::regex::find_urls(text) {
        let url = m.trim_end_matches(['.', ',', ';', ':', '!', '?']);
        if seen.insert(url) {
            urls.push(url.to_string());
        }
    }
    urls
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
    read_only: bool,
    cmd_tx: mpsc::Sender<BackendCommand>,
    reply_from_address: String,
    can_expire_now: bool,
    email_id: String,
    email: Option<Email>,
    /// The message's text, unwrapped: the pane's columns wrap it.
    text: String,
    /// Bumped whenever `text` is rebuilt, so the text's key changes with
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
    attachment_cursor: usize,
    show_all_headers: bool,
    raw_headers_cache: HashMap<String, String>,
    raw_headers_loading: bool,
    mailboxes: Vec<Mailbox>,
    archive_folder: String,
    deleted_folder: String,
    move_mode: bool,
    move_cursor: usize,
    prefer_html: bool,
    /// A message whose body is HTML alone shown as its text, not its page.
    page_as_text: bool,
    /// Where the head ends in `text`: the headers and attachments a page
    /// is shown below.
    head_end: usize,
    /// Where the HTML the body shows is (`html_at`), as `text` was made.
    html_at: Option<(bool, usize)>,
    /// The message as its pages, while the body is one: let go before
    /// any text of it is rendered (README.md, "Memory").
    page: Held,
    /// The pictures the message's HTML names among its own parts, asked
    /// for once a message, and those come.
    pictures: Option<Pictures>,
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
            read_only: false,
            cmd_tx,
            reply_from_address,
            can_expire_now,
            email_id,
            email: None,
            text: String::new(),
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
            attachment_cursor: 0,
            show_all_headers: false,
            raw_headers_cache: HashMap::new(),
            raw_headers_loading: false,
            mailboxes,
            archive_folder,
            deleted_folder,
            move_mode: false,
            move_cursor: 0,
            prefer_html: false,
            page_as_text: false,
            head_end: 0,
            html_at: None,
            page: Held::default(),
            pictures: None,
            browser,
            urls: Vec::new(),
            url_picking: false,
            url_cursor: 0,
            nav_entries,
            nav_cursor,
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
        if !self.read_only {
            let op_id = self.next_op_id();
            let _ = self.cmd_tx.send(BackendCommand::MarkEmailRead {
                op_id,
                id: self.email_id.clone(),
            });
            if let Some(entry) = self.nav_entries.get_mut(next_idx) {
                entry.unread = false;
            }
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
            lines.push(format!("Subject: {}", email.display_subject()));
        }
    }

    /// One message as lines, the links it carries in the order the
    /// picker numbers them, and how many of the lines are its head.
    fn render_email(
        email: &Email,
        raw_headers: Option<&str>,
        prefer_html: bool,
    ) -> (Vec<String>, Vec<String>, usize) {
        let mut lines = Vec::new();
        Self::render_head(email, raw_headers, &mut lines);
        let head = lines.len();

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

        (lines, urls, head)
    }

    /// The headers and the attachments, as lines: what the view shows
    /// above a message's body, as text and as td-mail's own above its
    /// page.
    fn render_head(email: &Email, raw_headers: Option<&str>, lines: &mut Vec<String>) {
        Self::render_headers(email, raw_headers, lines);

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
                lines.push("  Press A to choose an attachment to download or open".to_string());
            }
        }
    }

    /// Where the HTML the body shows is, when `extract_body` would show
    /// one as text: the HTML part when it is preferred, a text body that
    /// is HTML, or the HTML part of a message with no text; as whether it
    /// is among the HTML parts, else the text parts, and its place there.
    /// Shown as a page.
    fn html_at(email: &Email, prefer_html: bool) -> Option<(bool, usize)> {
        if email.is_unreadable() {
            return None;
        }
        // The first of the parts with a value: its place, and whether it
        // is HTML by its content or its type.
        let first = |parts: &Option<Vec<crate::jmap::types::BodyPart>>| {
            parts.iter().flatten().enumerate().find_map(|(at, part)| {
                let value = email.body_values.get(&part.part_id)?;
                let html = looks_like_html(&value.value)
                    || part
                        .r#type
                        .as_deref()
                        .is_some_and(|t| t.eq_ignore_ascii_case("text/html"));
                Some((at, html))
            })
        };
        if prefer_html {
            if let Some((at, _)) = first(&email.html_body) {
                return Some((true, at));
            }
        }
        if let Some((at, html)) = first(&email.text_body) {
            return html.then_some((false, at));
        }
        first(&email.html_body).map(|(at, _)| (true, at))
    }

    /// The HTML `html_at` found.
    fn html_in(email: &Email, (html, at): (bool, usize)) -> Option<&str> {
        let parts = if html {
            &email.html_body
        } else {
            &email.text_body
        };
        let part = parts.as_ref()?.get(at)?;
        email
            .body_values
            .get(&part.part_id)
            .map(|v| v.value.as_str())
    }

    /// The HTML the body shows as a page, when it shows one.
    fn page_html(&self) -> Option<&str> {
        if self.page_as_text {
            return None;
        }
        Self::html_in(self.email.as_ref()?, self.html_at?)
    }

    fn extract_body(email: &Email, prefer_html: bool) -> String {
        if email.is_unreadable() {
            let blob = email
                .extra
                .get("blobId")
                .and_then(td_json::Json::as_str)
                .unwrap_or("unavailable");
            return format!("This message could not be interpreted. Original blob_id: {blob}\nUse td-mail CLI download_attachment with this blob_id, an unused name ending in .eml, and content_type message/rfc822.");
        }
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
                    return crate::compose::plain_within(&value.value);
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

    /// Asks the account's server for the pictures the shown message's HTML
    /// (as far as it is laid) names by `cid:` URL among its parts, once a
    /// message, one at a time: another message's are let go.
    fn ask_pictures(&mut self) {
        let Some(email) = self.email.as_ref() else {
            return;
        };
        if self
            .pictures
            .as_ref()
            .is_some_and(|pictures| pictures.of_email(&email.id))
        {
            return;
        }
        let html = self.html_at.and_then(|at| Self::html_in(email, at));
        let html = html.map(|html| crate::compose::HTML_LIMITS.read(html));
        self.pictures = html.and_then(|html| Pictures::of(email, html));
        self.ask_next_picture();
    }

    /// Asks for the next of the message's pictures, if one is left.
    /// The pictures' own message names the request: the view may have
    /// moved on to another whose body has not come.
    fn ask_next_picture(&mut self) {
        let Some(pictures) = self.pictures.as_mut() else {
            return;
        };
        let Ok(email_id) = td_alloc::string(pictures.email()) else {
            return;
        };
        let Some(part) = pictures.next_ask() else {
            return;
        };
        let ask = (|| {
            Some(BackendCommand::FetchPicture {
                email_id,
                blob_id: td_alloc::string(&part.blob_id).ok()?,
                content_type: td_alloc::string(&part.content_type).ok()?,
                limit: pictures::MAX_BODY,
            })
        })();
        if let Some(ask) = ask {
            let _ = self.cmd_tx.send(ask);
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

    /// Opens the link numbered `index` through td-ui's opener, the one a
    /// Control-press on a link uses: the configured browser command, else
    /// `$BROWSER`, else `xdg-open`, run without a shell.
    fn open_url(&mut self, index: usize) {
        let Some(url) = self.urls.get(index) else {
            self.status_message = Some("Invalid URL number".to_string());
            return;
        };
        crate::log_info!("[Browser] opening {}", url);
        self.status_message = Some(match td_ui::open::link(url, self.browser.as_deref()) {
            Ok(_) => format!("Opening [{}]...", index + 1),
            Err(e) => {
                crate::log_error!("[Browser] {}", e);
                format!("Failed to open browser: {}", e)
            }
        });
    }

    /// Rebuilds the text and the links from what is loaded, and bumps the
    /// generation so the pane reloads the text its key now names.
    fn rerender_text(&mut self) {
        self.page.clear();
        let Some(ref email) = self.email else {
            return;
        };
        let raw = if self.show_all_headers {
            self.raw_headers_cache.get(&email.id).map(|s| s.as_str())
        } else {
            None
        };
        let (lines, urls, head) = Self::render_email(email, raw, self.prefer_html);
        // The head's lines and the newline after each but the last.
        self.head_end = lines
            .iter()
            .take(head)
            .map(|line| line.len() + 1)
            .sum::<usize>()
            .saturating_sub(1);
        self.text = lines.join("\n");
        self.html_at = Self::html_at(email, self.prefer_html);
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
        let send_result = self.cmd_tx.send(BackendCommand::MoveEmail {
            op_id,
            id: self.email_id.clone(),
            to_mailbox_id: target_id,
        });

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
        let send_result = self.cmd_tx.send(BackendCommand::MoveEmail {
            op_id,
            id: self.email_id.clone(),
            to_mailbox_id: target_id,
        });

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
        let send_result = self.cmd_tx.send(BackendCommand::DestroyEmail {
            op_id,
            id: self.email_id.clone(),
        });

        match send_result {
            Ok(()) => ViewAction::Pop,
            Err(e) => {
                self.status_message = Some(format!("Expire failed: {}", e));
                ViewAction::Continue
            }
        }
    }

    /// The subject the window names: the loaded message's.
    fn subject(&self) -> &str {
        self.email
            .as_ref()
            .map(Email::display_subject)
            .filter(|subject| !subject.is_empty())
            .unwrap_or("(no subject)")
    }

    fn title(&self) -> String {
        if self.loading {
            "Email".to_string()
        } else {
            strip_newlines(self.subject())
        }
    }

    /// The key of the text shown: what it is of, the generation its lines
    /// were built in, and the flags that change what they say, so the pane
    /// reloads exactly when the text differs.
    fn text_key(&self) -> String {
        format!(
            "email:{}:{}:{}{}",
            self.email_id,
            self.text_generation,
            if self.show_all_headers { 'v' } else { '-' },
            if self.prefer_html { 'h' } else { '-' }
        )
    }

    fn body(&self) -> Body<'_> {
        if self.loading {
            return Body::message("Loading email...".to_string());
        }
        if let Some(ref error) = self.error {
            return Body::message(error.clone());
        }
        if self.attachment_picking {
            return Body::List {
                total: self.attachment_count(),
                selected: self.attachment_cursor,
                row: Box::new(move |index| {
                    let Some(att) = self
                        .email
                        .as_ref()
                        .and_then(|email| email.attachments.as_ref())
                        .and_then(|attachments| attachments.get(index))
                    else {
                        return Row::default();
                    };
                    Row {
                        label: format!(
                            "[{}] {}",
                            index + 1,
                            strip_newlines(att.name.as_deref().unwrap_or("unnamed"))
                        ),
                        meta: format!(
                            "{} {}",
                            strip_newlines(
                                att.r#type.as_deref().unwrap_or("application/octet-stream")
                            ),
                            att.size.map(format_size).unwrap_or_default()
                        ),
                        marked: false,
                    }
                }),
            };
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
        if self.page_html().is_some() {
            return Body::Page;
        }
        Body::Text {
            key: self.text_key(),
            text: Box::new(move |columns| wrap_text(&self.text, columns)),
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
                "Attachment {}/{} | Up/Down:navigate RET:open Esc:cancel",
                self.attachment_cursor + 1,
                self.attachment_count()
            )
        } else if self.pending_reply_all.is_some() || self.pending_forward {
            "Loading reply data... | q:back".to_string()
        } else {
            let att_hint = if self.attachment_count() > 0 {
                " A:attachments"
            } else {
                ""
            };
            let expire_hint = if self.can_expire_now { " D:expire" } else { "" };
            let url_hint = if !self.urls.is_empty() {
                " b:links"
            } else {
                ""
            };
            let page_hint = if self.page_html().is_some() {
                " l/L:link"
            } else {
                ""
            };
            format!(
                "q:back n/p:unread j/k:scroll r:reply R:reply-all F:forward h:html{}{}{}{} a:archive d:delete m:move J:spam H:ham S:score ?:help",
                page_hint, att_hint, expire_hint, url_hint
            )
        };
        match self.status_message {
            Some(ref msg) => format!("{} | {}", msg, base),
            None => base,
        }
    }
}

impl View for EmailView {
    fn keys(&self) -> Option<&'static crate::keybindings::Table> {
        Some(&crate::keybindings::EMAIL_VIEW)
    }

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

    fn handle_key(&mut self, key: Key, page: usize) -> ViewAction {
        if self.attachment_picking {
            let last = self.attachment_count().saturating_sub(1);
            self.attachment_cursor = self.attachment_cursor.min(last);
            match key {
                Key::Escape | Key::Char('q') => self.attachment_picking = false,
                Key::Char('n') | Key::Char('j') | Key::Down | Key::ScrollDown => {
                    self.attachment_cursor = self.attachment_cursor.saturating_add(1).min(last);
                }
                Key::Char('p') | Key::Char('k') | Key::Up | Key::ScrollUp => {
                    self.attachment_cursor = self.attachment_cursor.saturating_sub(1);
                }
                Key::PageDown | Key::Char(' ') => {
                    self.attachment_cursor =
                        self.attachment_cursor.saturating_add(page.max(1)).min(last);
                }
                Key::PageUp => {
                    self.attachment_cursor = self.attachment_cursor.saturating_sub(page.max(1));
                }
                Key::Home => self.attachment_cursor = 0,
                Key::End => self.attachment_cursor = last,
                Key::Click(index) if index < self.attachment_count() => {
                    self.attachment_cursor = index;
                    self.attachment_picking = false;
                    self.download_attachment(index);
                }
                Key::Enter if self.attachment_count() > 0 => {
                    self.attachment_picking = false;
                    self.download_attachment(self.attachment_cursor);
                }
                Key::Char('?') => return ViewAction::ShowKeys,
                _ => {}
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
            // Over a page: l and L focus its next and previous link,
            // Return opens the one the status row names, Escape lets the
            // focus go.
            Key::Char(c @ ('l' | 'L')) if self.page_html().is_some() => {
                let stepped = self
                    .page
                    .get_mut()
                    .is_some_and(|page| page.step_link(c == 'l'));
                if !stepped {
                    self.status_message =
                        td_alloc::format(format_args!("No link on this page")).ok();
                }
                ViewAction::Continue
            }
            Key::Enter if self.page_html().is_some() => {
                let browser = self.browser.as_deref();
                if let Some(said) = self.page.get().and_then(|page| page.open_shown(browser)) {
                    self.status_message = Some(said);
                }
                ViewAction::Continue
            }
            Key::Escape if self.page_html().is_some() => {
                if let Some(page) = self.page.get_mut() {
                    page.unfocus();
                }
                ViewAction::Continue
            }
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
                } else {
                    self.attachment_picking = true;
                    self.attachment_cursor = 0;
                    self.status_message = None;
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
                    if self.raw_headers_cache.contains_key(&self.email_id) {
                        self.rerender_text();
                    } else {
                        self.raw_headers_loading = true;
                        self.status_message = Some("Loading raw headers...".to_string());
                        let _ = self.cmd_tx.send(BackendCommand::GetEmailRawHeaders {
                            id: self.email_id.clone(),
                        });
                    }
                } else {
                    self.rerender_text();
                }
                ViewAction::Continue
            }
            Key::Char('h') => {
                // A message with a plain part moves between it and its
                // HTML's page; one whose body is HTML alone between the
                // page and its text.
                let html_alone = self
                    .email
                    .as_ref()
                    .is_some_and(|email| Self::html_at(email, false).is_some());
                let shown = if html_alone {
                    self.page_as_text = !self.page_as_text;
                    if self.page_as_text {
                        "Showing the HTML as text"
                    } else {
                        "Showing the HTML as a page"
                    }
                } else {
                    self.prefer_html = !self.prefer_html;
                    if self.prefer_html {
                        "Showing HTML body"
                    } else {
                        "Showing plain text body"
                    }
                };
                self.status_message = Some(shown.to_string());
                self.rerender_text();
                // A message's HTML first shown here asks for its pictures.
                self.ask_pictures();
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
            // Over a page the numbered links are not in view: b lists them.
            Key::Char(c @ '1'..='9') => {
                let index = (c as usize) - ('1' as usize);
                if index < self.urls.len() && self.page_html().is_none() {
                    self.open_url(index);
                }
                ViewAction::Continue
            }
            Key::Char('?') => ViewAction::ShowKeys,
            _ => ViewAction::Continue,
        }
    }

    fn poll_work(&mut self) -> bool {
        let page = self.page.poll();
        let decoded = self.pictures.as_mut().and_then(Pictures::poll);
        if decoded.is_some() {
            self.ask_next_picture();
        }
        // A picture kept lays the shown message out again; one for a
        // message the view has moved on from waits for it.
        let shown = self
            .pictures
            .as_ref()
            .is_some_and(|pictures| pictures.of_email(&self.email_id));
        page || (decoded == Some(true) && shown)
    }

    fn working(&self) -> bool {
        self.page.working() || self.pictures.as_ref().is_some_and(Pictures::decoding)
    }

    #[cfg(test)]
    fn wait_work(&self) {
        self.page.wait_ready();
        if let Some(pictures) = self.pictures.as_ref() {
            pictures.wait_decoded();
        }
    }

    #[cfg(test)]
    fn settle_work(&mut self) -> bool {
        let working = self.working();
        self.page.settle();
        if let Some(pictures) = self.pictures.as_ref() {
            pictures.wait_decoded();
        }
        self.poll_work();
        working
    }

    fn lay_out_page(&mut self, rect: Option<Rect>, scale: Scale, faces: Option<&Faces>) {
        let html = if self.page_as_text {
            None
        } else {
            self.email
                .as_ref()
                .zip(self.html_at)
                .and_then(|(email, at)| Self::html_in(email, at))
        };
        match (rect, html) {
            (Some(rect), Some(html)) => {
                let key = self.text_key();
                let head = self.text.get(..self.head_end).unwrap_or_default();
                self.page
                    .show(&key, head, html, rect, scale, faces, self.pictures.as_ref());
            }
            _ => self.page.clear(),
        }
    }

    fn page(&self) -> Option<&Held> {
        Some(&self.page)
    }

    fn page_mut(&mut self) -> Option<&mut Paged> {
        self.page.get_mut()
    }

    fn on_response(&mut self, response: &BackendResponse) -> bool {
        if let BackendResponse::AccountReadOnly(value) = response {
            let changed = self.read_only != *value;
            self.read_only = *value;
            return changed;
        }
        match response {
            BackendResponse::EmailBody { id, result } if *id == self.email_id => {
                self.loading = false;
                match result.as_ref() {
                    Ok(email) => {
                        self.set_nav_unread(&email.id, !email.keywords.contains_key("$seen"));
                        if let Some(idx) = self.nav_entries.iter().position(|e| e.id == email.id) {
                            self.nav_cursor = idx;
                        }
                        // Another message is shown as its page again.
                        if self.email.as_ref().is_none_or(|shown| shown.id != email.id) {
                            self.page_as_text = false;
                        }
                        self.email = Some(email.clone());
                        self.rerender_text();
                        self.ask_pictures();
                        self.error = None;
                        self.pending_write_ops.clear();
                    }
                    Err(e) => {
                        self.error = Some(format!("Failed to load email: {}", e));
                    }
                }
                true
            }
            // Taken for the pictures' own message, though the view has
            // moved on: dropped, the rest of its pictures would never be
            // asked for when the view comes back to it.
            BackendResponse::PictureFetched {
                email_id,
                blob_id,
                result,
            } => {
                let body = result.as_ref().ok().map(Vec::as_slice);
                let answered = self
                    .pictures
                    .as_mut()
                    .filter(|pictures| pictures.of_email(email_id))
                    .map(|pictures| pictures.answered(blob_id, body));
                // A body decodes on the page thread; the next is asked for
                // once it has (`poll_work`), so one is held at a time.
                if answered == Some(Answered::Next) {
                    self.ask_next_picture();
                }
                false
            }
            BackendResponse::EmailForReply { id, result } if *id == self.email_id => {
                let reply_all = self.pending_reply_all.take();
                let is_forward = self.pending_forward;
                self.pending_forward = false;
                match result.as_ref() {
                    Ok(email) => {
                        // The quotation is rendered with the page let go.
                        self.page.clear();
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
                if self.raw_headers_cache.contains_key(&self.email_id) {
                    self.raw_headers_loading = false;
                    self.status_message = None;
                    if self.show_all_headers {
                        self.rerender_text();
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::jmap::types::BodyPart;

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

    /// A view on one message with a `plain` part and an `html` part,
    /// either absent.
    fn parts_view(plain: Option<&str>, html: Option<&str>) -> EmailView {
        let (mut view, _rx) = loaded_view("preview");
        let mut email = make_email("e1", "Hello", "preview");
        let part = |id: &str, kind: &str| BodyPart {
            part_id: id.to_string(),
            blob_id: None,
            r#type: Some(kind.to_string()),
            name: None,
            size: None,
            cid: None,
        };
        let value = |text: &str| crate::jmap::types::BodyValue {
            value: text.to_string(),
            is_encoding_problem: false,
            is_truncated: false,
        };
        if let Some(plain) = plain {
            email.text_body = Some(vec![part("1", "text/plain")]);
            email.body_values.insert("1".into(), value(plain));
        }
        if let Some(html) = html {
            email.html_body = Some(vec![part("2", "text/html")]);
            email.body_values.insert("2".into(), value(html));
            // JMAP's textBody names the HTML part when there is no text.
            if plain.is_none() {
                email.text_body = Some(vec![part("2", "text/html")]);
            }
        }
        view.on_response(&BackendResponse::EmailBody {
            id: "e1".to_string(),
            result: Box::new(Ok(email)),
        });
        view
    }

    /// A view on message `e1`, and its commands, whose body holds a
    /// plain part when `plain` and an HTML part naming `logo@e.example`
    /// and `mark@e.example` by `cid:`, which its attachments carry with
    /// one more no URL names.
    fn cid_view(plain: bool) -> (EmailView, mpsc::Receiver<BackendCommand>, Email) {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let view = EmailView::new(
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
        let part = |id: &str, kind: &str, cid: Option<&str>| BodyPart {
            part_id: id.to_string(),
            blob_id: Some(format!("blob-{id}")),
            r#type: Some(kind.to_string()),
            name: None,
            size: Some(100),
            cid: cid.map(str::to_string),
        };
        let value = |text: &str| crate::jmap::types::BodyValue {
            value: text.to_string(),
            is_encoding_problem: false,
            is_truncated: false,
        };
        let mut email = make_email("e1", "Hello", "preview");
        email.html_body = Some(vec![part("1", "text/html", None)]);
        email.body_values.insert(
            "1".into(),
            value("<p>see <img src='cid:logo%40e.example'><img src=cid:mark@e.example>"),
        );
        if plain {
            email.text_body = Some(vec![part("0", "text/plain", None)]);
            email.body_values.insert("0".into(), value("see the logo"));
        } else {
            email.text_body = Some(vec![part("1", "text/html", None)]);
        }
        email.attachments = Some(vec![
            part("2", "image/png", Some("logo@e.example")),
            part("3", "image/png", Some("unnamed@e.example")),
            part("4", "image/png", Some("mark@e.example")),
        ]);
        (view, cmd_rx, email)
    }

    /// The pictures a view's commands ask for: each message, blob and
    /// byte limit.
    fn asked_pictures(commands: &mpsc::Receiver<BackendCommand>) -> Vec<(String, String, u64)> {
        commands
            .try_iter()
            .filter_map(|command| match command {
                BackendCommand::FetchPicture {
                    email_id,
                    blob_id,
                    limit,
                    ..
                } => Some((email_id, blob_id, limit)),
                _ => None,
            })
            .collect()
    }

    fn asked(blob: &str) -> Vec<(String, String, u64)> {
        vec![("e1".to_string(), blob.to_string(), pictures::MAX_BODY)]
    }

    /// A message whose HTML names its own image parts by `cid:` asks the
    /// account for them one at a time, each within `MAX_BODY`, though its
    /// body comes twice (the cache's, then the server's): the next on the
    /// answer for the one awaited, a picture or not. The picture come is
    /// kept and redraws; an answer for another message, or one that does
    /// not decode, changes nothing. Another message's are let go.
    #[test]
    fn a_messages_cid_pictures_are_asked_for_one_at_a_time_and_kept() {
        let (mut view, cmd_rx, email) = cid_view(false);
        for _ in 0..2 {
            view.on_response(&BackendResponse::EmailBody {
                id: "e1".to_string(),
                result: Box::new(Ok(email.clone())),
            });
        }
        assert_eq!(asked_pictures(&cmd_rx), asked("blob-2"));
        let came = picture_came;
        let png = crate::ui::pictures::tests::png(4, 4);
        assert!(!came(&mut view, "e2", "blob-2", &png));
        assert!(asked_pictures(&cmd_rx).is_empty());
        // The next part is asked for only once the body has decoded.
        assert!(!view.on_response(&BackendResponse::PictureFetched {
            email_id: "e1".to_string(),
            blob_id: "blob-2".to_string(),
            result: Ok(b"not a picture".to_vec()),
        }));
        assert!(asked_pictures(&cmd_rx).is_empty());
        view.wait_work();
        assert!(!view.poll_work());
        assert_eq!(asked_pictures(&cmd_rx), asked("blob-4"));
        assert!(came(&mut view, "e1", "blob-4", &png));
        assert!(asked_pictures(&cmd_rx).is_empty());
        assert_eq!(view.pictures.as_ref().map(Pictures::count), Some(1));
        let mut other = make_email("e1b", "Other", "preview");
        other.html_body = email.html_body.clone();
        other.body_values = email.body_values.clone();
        view.email_id = "e1b".to_string();
        view.on_response(&BackendResponse::EmailBody {
            id: "e1b".to_string(),
            result: Box::new(Ok(other)),
        });
        assert!(view.pictures.is_none());
    }

    /// An answer that comes while the view has moved on to another message,
    /// whose body has not come, is still taken for the pictures' own
    /// message, without a redraw, and the next is asked in its name; back
    /// on it, the rest come.
    #[test]
    fn a_messages_pictures_keep_coming_across_a_move_away_and_back() {
        let (mut view, cmd_rx, email) = cid_view(false);
        view.on_response(&BackendResponse::EmailBody {
            id: "e1".to_string(),
            result: Box::new(Ok(email.clone())),
        });
        assert_eq!(asked_pictures(&cmd_rx), asked("blob-2"));
        view.email_id = "e2".to_string();
        let png = crate::ui::pictures::tests::png(4, 4);
        let came = |view: &mut EmailView, blob: &str| picture_came(view, "e1", blob, &png);
        assert!(!came(&mut view, "blob-2"));
        assert_eq!(asked_pictures(&cmd_rx), asked("blob-4"));
        view.email_id = "e1".to_string();
        view.on_response(&BackendResponse::EmailBody {
            id: "e1".to_string(),
            result: Box::new(Ok(email)),
        });
        assert!(asked_pictures(&cmd_rx).is_empty());
        assert!(came(&mut view, "blob-4"));
        assert_eq!(view.pictures.as_ref().map(Pictures::count), Some(2));
    }

    /// Only the HTML the page lays is read for `cid:` URLs: a picture
    /// named past td-mail's input limit, where the page is cut, is not
    /// asked for.
    #[test]
    fn a_picture_named_past_the_cut_is_not_asked_for() {
        let (mut view, cmd_rx, mut email) = cid_view(false);
        let input = crate::compose::HTML_LIMITS.input;
        let value = email.body_values.get_mut("1").unwrap();
        value.value = format!("{}<img src=cid:logo@e.example>", "x".repeat(input));
        view.on_response(&BackendResponse::EmailBody {
            id: "e1".to_string(),
            result: Box::new(Ok(email)),
        });
        assert!(asked_pictures(&cmd_rx).is_empty());
        assert!(view.pictures.is_none());
    }

    /// A message with a plain part and an HTML part shows the plain one
    /// and asks for nothing; its HTML shown with h asks for its pictures,
    /// once; and a failed answer asks for the next.
    #[test]
    fn an_alternative_messages_pictures_are_asked_for_when_its_html_is_shown() {
        let (mut view, cmd_rx, email) = cid_view(true);
        view.on_response(&BackendResponse::EmailBody {
            id: "e1".to_string(),
            result: Box::new(Ok(email)),
        });
        assert!(view.page_html().is_none());
        assert!(asked_pictures(&cmd_rx).is_empty());
        view.handle_key(Key::Char('h'), 10);
        assert!(view.page_html().is_some());
        assert_eq!(asked_pictures(&cmd_rx), asked("blob-2"));
        view.handle_key(Key::Char('h'), 10);
        view.handle_key(Key::Char('h'), 10);
        assert!(asked_pictures(&cmd_rx).is_empty());
        view.on_response(&BackendResponse::PictureFetched {
            email_id: "e1".to_string(),
            blob_id: "blob-2".to_string(),
            result: Err("offline".to_string()),
        });
        assert_eq!(asked_pictures(&cmd_rx), asked("blob-4"));
    }

    /// The HTML a scene shows as a page and the head above it, or none.
    fn page_of(view: &EmailView) -> Option<(String, String)> {
        match view.scene().body {
            Body::Page => Some((
                view.text[..view.head_end].to_string(),
                view.page_html()?.to_string(),
            )),
            _ => None,
        }
    }

    /// A message whose body is HTML alone is shown as its page, below
    /// td-mail's own head; `h` shows the HTML as text and again as the
    /// page.
    #[test]
    fn a_message_whose_body_is_html_alone_is_its_page() {
        let html = "<style>p{color:red}</style><p>Styled <b>words</b>";
        let mut view = parts_view(None, Some(html));
        let (head, shown) = page_of(&view).expect("a page");
        assert_eq!(shown, html);
        assert!(head.contains("Subject: Hello"), "{head}");
        assert!(!head.contains("Styled"), "{head}");
        view.handle_key(Key::Char('h'), 10);
        assert!(page_of(&view).is_none());
        let (_, text) = shown_text(&view, 80);
        assert!(text.contains("Styled words"), "{text}");
        assert!(!text.contains("<p>"), "{text}");
        view.handle_key(Key::Char('h'), 10);
        assert_eq!(page_of(&view).map(|(_, html)| html), Some(html.to_string()));
    }

    const RECT: Rect = Rect {
        x: 0,
        y: 24,
        width: 640,
        height: 400,
    };

    /// The view's pages, laid out for `RECT` as the session lays them.
    fn held(view: &mut EmailView) -> bool {
        view.lay_out_page(Some(RECT), Scale::default(), None);
        view.settle_work();
        view.page().and_then(Held::get).is_some()
    }

    /// A part's body come for message `email`, its decoding waited for:
    /// whether the view redraws.
    fn picture_came(view: &mut EmailView, email: &str, blob: &str, body: &[u8]) -> bool {
        let answered = view.on_response(&BackendResponse::PictureFetched {
            email_id: email.to_string(),
            blob_id: blob.to_string(),
            result: Ok(body.to_vec()),
        });
        if let Some(pictures) = view.pictures.as_ref() {
            pictures.wait_decoded();
        }
        answered | view.poll_work()
    }

    /// The view lets its pages go before it renders a text: a reply's
    /// quotation, or the message again as `h` and `v` render it; and with
    /// no body for them, or a body that is no page.
    #[test]
    fn the_view_lets_its_pages_go_before_it_renders_a_text() {
        let mut view = parts_view(None, Some("<p>the html words"));
        assert!(held(&mut view));
        view.handle_key(Key::Char('r'), 10);
        let email = view.email.clone().unwrap();
        view.on_response(&BackendResponse::EmailForReply {
            id: "e1".to_string(),
            result: Box::new(Ok(email)),
        });
        assert!(view.page().and_then(Held::get).is_none());
        assert!(view.take_pending_action().is_some(), "the reply's draft");
        assert!(held(&mut view));
        // The first v asks for the raw headers; the second renders.
        view.handle_key(Key::Char('v'), 10);
        view.handle_key(Key::Char('v'), 10);
        assert!(view.page().and_then(Held::get).is_none());
        assert!(held(&mut view));
        view.lay_out_page(None, Scale::default(), None);
        assert!(view.page().and_then(Held::get).is_none());
        assert!(held(&mut view));
        view.handle_key(Key::Char('h'), 10);
        assert!(!held(&mut view), "shown as text");
    }

    /// Over a page the digits open nothing: the numbered links are not in
    /// view. Over the text they open their link.
    #[test]
    fn over_a_page_the_digits_open_no_link() {
        let mut view = parts_view(None, Some("<p><a href='https://e.example/x'>a link</a>"));
        assert_eq!(view.urls.len(), 1);
        view.handle_key(Key::Char('1'), 10);
        assert_eq!(view.status_message, None);
        view.handle_key(Key::Char('h'), 10);
        view.handle_key(Key::Char('1'), 10);
        assert_eq!(view.status_message.as_deref(), Some("Opening [1]..."));
    }

    /// The HTML shown as text is the message's choice alone: the same
    /// message loaded again keeps it, another is its page again.
    #[test]
    fn another_message_is_its_page_again() {
        let html = "<p>the html words";
        let mut view = parts_view(None, Some(html));
        view.handle_key(Key::Char('h'), 10);
        assert!(page_of(&view).is_none());
        let again = view.email.clone().unwrap();
        view.on_response(&BackendResponse::EmailBody {
            id: "e1".to_string(),
            result: Box::new(Ok(again.clone())),
        });
        assert!(page_of(&view).is_none());
        let mut other = again;
        other.id = "e2".to_string();
        view.email_id = "e2".to_string();
        view.on_response(&BackendResponse::EmailBody {
            id: "e2".to_string(),
            result: Box::new(Ok(other)),
        });
        assert!(page_of(&view).is_some());
    }

    /// Over a page l focuses its first link and Return opens it in the
    /// configured browser; Escape lets the focus go, and Return then opens
    /// nothing. A page with no link says so.
    #[test]
    fn l_focuses_a_link_and_return_opens_it() {
        let mut view = parts_view(None, Some("<p><a href='https://e.example/x'>a link</a>"));
        assert!(held(&mut view));
        view.handle_key(Key::Enter, 10);
        assert_eq!(view.status_message, None, "no link focused");
        view.handle_key(Key::Char('l'), 10);
        view.handle_key(Key::Enter, 10);
        assert_eq!(
            view.status_message.as_deref(),
            Some("opened https://e.example/x with true")
        );
        view.status_message = None;
        view.handle_key(Key::Escape, 10);
        view.handle_key(Key::Enter, 10);
        assert_eq!(view.status_message, None);
        let mut bare = parts_view(None, Some("<p>no link here"));
        assert!(held(&mut bare));
        bare.handle_key(Key::Char('L'), 10);
        assert_eq!(bare.status_message.as_deref(), Some("No link on this page"));
    }

    /// A message with a plain part is shown as it, and `h` shows its
    /// HTML part's page and then the plain part again.
    #[test]
    fn a_message_with_a_plain_part_is_its_text_and_h_its_html_page() {
        let mut view = parts_view(Some("the plain words"), Some("<p>the html words"));
        assert!(page_of(&view).is_none());
        assert!(shown_text(&view, 80).1.contains("the plain words"));
        view.handle_key(Key::Char('h'), 10);
        assert_eq!(
            page_of(&view).map(|(_, html)| html),
            Some("<p>the html words".to_string())
        );
        view.handle_key(Key::Char('h'), 10);
        assert!(page_of(&view).is_none());
        // Plain text that only looks like markup by its tags is a page;
        // plain text is never one.
        let view = parts_view(Some("<div>a</div>"), None);
        assert!(page_of(&view).is_some());
        let view = parts_view(Some("a < b and c > d"), None);
        assert!(page_of(&view).is_none());
    }

    /// The key of the text the scene shows, and the text itself.
    #[test]
    fn read_only_unread_navigation_preserves_flags() {
        let (mut view, receiver) = loaded_view("preview");
        view.nav_entries = vec![
            EmailNavEntry {
                id: "e1".into(),
                unread: true,
            },
            EmailNavEntry {
                id: "e2".into(),
                unread: true,
            },
        ];
        view.on_response(&BackendResponse::AccountReadOnly(true));
        while receiver.try_recv().is_ok() {}
        assert!(view.navigate_unread(true));
        assert!(matches!(receiver.try_recv(), Ok(BackendCommand::GetEmail { id }) if id == "e2"));
        assert!(receiver.try_recv().is_err());
        assert!(view.nav_entries.iter().all(|entry| entry.unread));
    }

    fn shown_text(view: &EmailView, columns: usize) -> (String, String) {
        match view.scene().body {
            Body::Text { key, text } => (key, text(columns)),
            Body::List { .. } | Body::Edit { .. } | Body::Page { .. } => {
                panic!("the message is a text")
            }
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

    fn attachment_view(count: usize) -> (EmailView, mpsc::Receiver<BackendCommand>) {
        let (mut view, rx) = loaded_view("plain body");
        let mut email = make_email("e1", "Attachments", "plain body");
        email.attachments = Some(
            (1..=count)
                .map(|n| crate::jmap::types::BodyPart {
                    part_id: n.to_string(),
                    blob_id: Some(format!("blob-{n}")),
                    name: Some(format!("attachment-{n}.pdf")),
                    r#type: Some("application/pdf".to_string()),
                    size: Some(2048),
                    cid: None,
                })
                .collect(),
        );
        view.on_response(&BackendResponse::EmailBody {
            id: "e1".to_string(),
            result: Box::new(Ok(email)),
        });
        (view, rx)
    }

    #[test]
    fn attachment_selector_opens_the_twelfth_attachment_with_keyboard_or_pointer() {
        for pointer in [false, true] {
            let (mut view, rx) = attachment_view(12);
            assert!(shown_text(&view, 120).1.contains("Press A to choose"));
            let scene = view.scene();
            let action = scene
                .labels
                .iter()
                .position(|label| *label == "Attachments")
                .unwrap();
            let key = *scene.keys.get(action).unwrap();
            drop(scene);
            view.handle_key(key, 5);
            {
                let scene = view.scene();
                assert_eq!(scene.labels, &["Open", "Cancel"]);
                let Body::List {
                    total,
                    selected,
                    row,
                } = scene.body
                else {
                    panic!("attachments must be selectable rows");
                };
                assert_eq!((total, selected), (12, 0));
                assert_eq!(row(11).label, "[12] attachment-12.pdf");
                assert_eq!(row(11).meta, "application/pdf 2.0 KB");
            }
            // Digits never dispatch attachment 1 while trying to reach 12.
            view.handle_key(Key::Char('1'), 5);
            view.handle_key(Key::Char('2'), 5);
            assert!(rx.try_recv().is_err());
            assert!(view.attachment_picking);
            if pointer {
                view.handle_key(Key::Click(11), 5);
            } else {
                for _ in 0..11 {
                    view.handle_key(Key::Down, 5);
                }
                assert_eq!(view.attachment_cursor, 11);
                view.handle_key(Key::Enter, 5);
            }
            assert!(!view.attachment_picking);
            assert!(
                matches!(rx.try_recv().unwrap(), BackendCommand::DownloadAttachment {
                blob_id, name, content_type,
            } if blob_id == "blob-12" && name == "attachment-12.pdf" && content_type == "application/pdf")
            );
            assert!(rx.try_recv().is_err());
            assert!(matches!(view.scene().body, Body::Text { .. }));
        }
    }

    #[test]
    fn attachment_selector_navigation_is_bounded_and_cancel_sends_nothing() {
        let (mut view, rx) = attachment_view(12);
        view.handle_key(Key::Char('A'), 5);
        for (key, selected) in [
            (Key::Up, 0),
            (Key::PageDown, 5),
            (Key::PageDown, 10),
            (Key::PageDown, 11),
            (Key::ScrollDown, 11),
            (Key::PageUp, 6),
            (Key::ScrollUp, 5),
            (Key::Home, 0),
            (Key::End, 11),
            (Key::Char('k'), 10),
            (Key::Char('j'), 11),
            (Key::Char('p'), 10),
            (Key::Char('n'), 11),
            (Key::Click(12), 11),
        ] {
            view.handle_key(key, 5);
            assert_eq!(view.attachment_cursor, selected, "{key:?}");
            assert!(view.attachment_picking);
        }
        view.handle_key(Key::Escape, 5);
        assert!(!view.attachment_picking);
        view.handle_key(Key::Char('A'), 5);
        assert_eq!(view.attachment_cursor, 0);
        view.handle_key(Key::Char('q'), 5);
        assert!(!view.attachment_picking);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn attachment_selector_handles_empty_single_and_missing_blob_messages() {
        let (mut empty, rx) = attachment_view(0);
        empty.handle_key(Key::Char('A'), 5);
        assert!(!empty.attachment_picking);
        assert_eq!(empty.status_message.as_deref(), Some("No attachments"));
        assert!(rx.try_recv().is_err());

        let (mut single, rx) = attachment_view(1);
        single.handle_key(Key::Char('A'), 5);
        assert!(single.attachment_picking);
        assert!(rx.try_recv().is_err());
        single
            .email
            .as_mut()
            .unwrap()
            .attachments
            .as_mut()
            .unwrap()
            .get_mut(0)
            .unwrap()
            .blob_id = None;
        single.handle_key(Key::Enter, 5);
        assert_eq!(
            single.status_message.as_deref(),
            Some("Attachment has no blob ID")
        );
        assert!(rx.try_recv().is_err());
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
    fn the_html_body_is_plain_text_carrying_the_link_and_image_targets() {
        let text = html_to_text(
            "<p><b>Bold</b> <a href=\"http://e/x\">go</a> <img src=\"p.png\" alt=\"Pic\"></p>",
        );
        assert!(!text.contains('\u{1b}'), "no escape sequences: {text}");
        assert!(text.contains("Bold"), "{text}");
        assert!(text.contains("go [http://e/x]"), "{text}");
        assert!(text.contains("Pic [img: p.png]"), "{text}");
    }

    /// A plain-text body past the input limit is shown, and quoted in a
    /// reply, to the limit and then the notice.
    #[test]
    fn a_long_plain_body_is_shown_and_quoted_to_the_limit() {
        let mut body = "line\n".repeat(crate::compose::HTML_LIMITS.input / 5 + 1);
        body.push_str("beyond\n");
        let mut email = make_email("e1", "Long", "preview");
        email.text_body = Some(vec![crate::jmap::types::BodyPart {
            part_id: "1".to_string(),
            blob_id: None,
            r#type: Some("text/plain".to_string()),
            name: None,
            size: None,
            cid: None,
        }]);
        email.body_values.insert(
            "1".to_string(),
            crate::jmap::types::BodyValue {
                value: body,
                is_encoding_problem: false,
                is_truncated: false,
            },
        );
        let notice = format!("\n{}\n", td_html::CUT_NOTICE);
        for shown in [
            EmailView::extract_body(&email, false),
            crate::compose::extract_body_text(&email),
        ] {
            assert!(shown.ends_with(&notice) && shown.starts_with("line\n"));
            assert!(!shown.contains("beyond"));
        }
    }

    /// Each URL is listed once, in the order it first appears, however
    /// many the body carries.
    #[test]
    fn urls_are_listed_once_in_order() {
        let text = "see https://b.example/ and https://a.example/, then https://b.example/.";
        assert_eq!(
            extract_urls(text),
            ["https://b.example/", "https://a.example/"]
        );
        let many: String = (0..20_000)
            .map(|i| format!("https://e.example/{i} https://e.example/{i}\n"))
            .collect();
        let urls = extract_urls(&many);
        assert_eq!(urls.len(), 20_000);
        assert_eq!(
            urls.last().map(String::as_str),
            Some("https://e.example/19999")
        );
    }

    /// A message's HTML is rendered within `HTML_LIMITS`: short linked
    /// paragraphs deep in quotes, whose output grows fastest with the
    /// markup, stop at the output budget, and the view's text, each span's
    /// link target included, ends with the renderer's notice.
    #[test]
    fn the_html_body_is_rendered_within_its_limits() {
        let limits = crate::compose::HTML_LIMITS;
        let mut html = format!("<a href=\"https://e.example/{}\">", "x".repeat(200));
        html.push_str(&"<blockquote>".repeat(62));
        html.push_str(&"<p>x</p>".repeat(limits.input / 10));
        assert!(html.len() <= limits.input);
        let text = html_to_text(&html);
        let notice = format!("\n{}\n", td_html::CUT_NOTICE);
        assert!(text.ends_with(&notice), "{}", text.len());
        assert!(text.len() < limits.output, "{}", text.len());
        assert!(text.contains("x [https://e.example/"));
        let lines = td_html::to_rich_within(html.as_bytes(), 80, limits).len();
        let twice = td_html::Limits {
            output: 2 * limits.output,
            ..limits
        };
        let more = td_html::to_rich_within(html.as_bytes(), 80, twice).len();
        assert!(text.lines().count() == lines && more > lines, "{lines}");
    }

    /// `?` asks the window for its key list.
    #[test]
    fn question_mark_shows_the_keys() {
        let (mut view, _rx) = loaded_view("plain body");
        assert!(matches!(
            view.handle_key(Key::Char('?'), 10),
            ViewAction::ShowKeys
        ));
    }
}

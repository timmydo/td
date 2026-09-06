use crate::backend::{BackendCommand, BackendResponse, EmailMutationAction};
use crate::compose;
use crate::jmap::types::{Email, Mailbox};
use crate::rules;
use crate::tui::input::Key;
use crate::tui::screen::Terminal;
use crate::tui::views::help::HelpView;
use crate::tui::views::{View, ViewAction};
use std::collections::HashMap;
use std::io;
use std::sync::mpsc;

#[derive(Clone, Copy, PartialEq)]
enum LineKind {
    Header,
    Separator,
    Body,
}

fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// Word-wrap a line at `max_width` characters, preferring to break at spaces.
fn wrap_line(s: &str, max_width: usize) -> Vec<&str> {
    if max_width == 0 || s.is_empty() {
        return vec![s];
    }

    let char_len = s.chars().count();
    if char_len <= max_width {
        return vec![s];
    }

    let mut result = Vec::new();
    let mut remaining = s;

    while !remaining.is_empty() {
        let char_len = remaining.chars().count();
        if char_len <= max_width {
            result.push(remaining);
            break;
        }

        // Find the byte position of the max_width-th character
        let byte_end = remaining
            .char_indices()
            .nth(max_width)
            .map(|(pos, _)| pos)
            .unwrap_or(remaining.len());

        let segment = &remaining[..byte_end];

        // Try to find a space to break at
        if let Some(space_pos) = segment.rfind(' ') {
            result.push(&remaining[..space_pos]);
            remaining = &remaining[space_pos + 1..];
        } else {
            // Hard break at max_width
            result.push(segment);
            remaining = &remaining[byte_end..];
        }
    }

    result
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

/// Convert HTML to terminal-formatted text with ANSI escape codes for
/// bold, underline, color, etc. using the rich rendering mode.
///
/// `to_rich` hands back the same tagged spans html2text's coloured
/// rendering did, so the SGR each tag turns into is unchanged; rendering
/// cannot fail, so there is no fallback to the raw HTML any more.
fn html_to_terminal(html: &str) -> String {
    use crate::html::Tag;

    let mut out = String::new();
    for line in crate::html::to_rich(html.as_bytes(), 80) {
        for span in line {
            let mut prefix = String::new();
            let mut suffix = String::new();
            for tag in &span.tags {
                match tag {
                    Tag::Strong => {
                        prefix.push_str("\x1b[1m");
                        suffix.push_str("\x1b[22m");
                    }
                    Tag::Emphasis => {
                        prefix.push_str("\x1b[3m");
                        suffix.push_str("\x1b[23m");
                    }
                    Tag::Strikeout => {
                        prefix.push_str("\x1b[9m");
                        suffix.push_str("\x1b[29m");
                    }
                    Tag::Code | Tag::Preformat => {
                        prefix.push_str("\x1b[2m");
                        suffix.push_str("\x1b[22m");
                    }
                    Tag::Link(url) => {
                        // Show link URL after text in dim
                        suffix.push_str(&format!(" \x1b[2m[{}]\x1b[22m", url));
                    }
                    Tag::Image(src) => {
                        suffix.push_str(&format!(" \x1b[2m[img: {}]\x1b[22m", src));
                    }
                    Tag::Colour(c) => {
                        prefix.push_str(&format!("\x1b[38;2;{};{};{}m", c.r, c.g, c.b));
                        suffix.push_str("\x1b[39m");
                    }
                    Tag::BgColour(c) => {
                        prefix.push_str(&format!("\x1b[48;2;{};{};{}m", c.r, c.g, c.b));
                        suffix.push_str("\x1b[49m");
                    }
                }
            }
            out.push_str(&prefix);
            out.push_str(&span.text);
            out.push_str(&suffix);
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
    lines: Vec<String>,
    line_kinds: Vec<LineKind>,
    scroll: usize,
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
            line_kinds: Vec::new(),
            scroll: 0,
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
        _subject: String,
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
            line_kinds: Vec::new(),
            scroll: 0,
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
        self.scroll = 0;
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

    fn render_headers(
        email: &Email,
        raw_headers: Option<&str>,
        lines: &mut Vec<String>,
        kinds: &mut Vec<LineKind>,
    ) {
        if let Some(raw) = raw_headers {
            for line in raw.lines() {
                lines.push(line.to_string());
                kinds.push(LineKind::Header);
            }
        } else {
            if let Some(ref from) = email.from {
                let addrs: Vec<String> = from.iter().map(|a| a.to_string()).collect();
                lines.push(format!("From: {}", addrs.join(", ")));
                kinds.push(LineKind::Header);
            }
            if let Some(ref to) = email.to {
                let addrs: Vec<String> = to.iter().map(|a| a.to_string()).collect();
                lines.push(format!("To: {}", addrs.join(", ")));
                kinds.push(LineKind::Header);
            }
            if let Some(ref cc) = email.cc {
                if !cc.is_empty() {
                    let addrs: Vec<String> = cc.iter().map(|a| a.to_string()).collect();
                    lines.push(format!("Cc: {}", addrs.join(", ")));
                    kinds.push(LineKind::Header);
                }
            }
            if let Some(ref date) = email.received_at {
                lines.push(format!("Date: {}", date));
                kinds.push(LineKind::Header);
            }
            lines.push(format!(
                "Subject: {}",
                email.subject.as_deref().unwrap_or("(no subject)")
            ));
            kinds.push(LineKind::Header);
        }
    }

    fn render_email(
        email: &Email,
        raw_headers: Option<&str>,
        prefer_html: bool,
    ) -> (Vec<String>, Vec<LineKind>, Vec<String>) {
        let mut lines = Vec::new();
        let mut kinds = Vec::new();

        Self::render_headers(email, raw_headers, &mut lines, &mut kinds);

        // Attachments
        if let Some(ref attachments) = email.attachments {
            if !attachments.is_empty() {
                lines.push(String::new());
                kinds.push(LineKind::Body);
                lines.push(format!("Attachments ({})", attachments.len()));
                kinds.push(LineKind::Body);
                for (i, att) in attachments.iter().enumerate() {
                    let name = att.name.as_deref().unwrap_or("unnamed");
                    let size = att.size.map(format_size).unwrap_or_default();
                    let type_str = att.r#type.as_deref().unwrap_or("application/octet-stream");
                    lines.push(format!("  [{}] {} ({}, {})", i + 1, name, type_str, size));
                    kinds.push(LineKind::Body);
                }
                lines.push("  Press 'A' then 1-9 to download/open".to_string());
                kinds.push(LineKind::Body);
            }
        }

        // Separator
        lines.push(String::new());
        kinds.push(LineKind::Body);

        // Body
        let body_text = Self::extract_body(email, prefer_html);
        for line in body_text.lines() {
            lines.push(line.to_string());
            kinds.push(LineKind::Body);
        }

        // Extract and append URLs
        let urls = extract_urls(&body_text);
        if !urls.is_empty() {
            lines.push(String::new());
            kinds.push(LineKind::Body);
            lines.push("Links:".to_string());
            kinds.push(LineKind::Header);
            for (i, url) in urls.iter().enumerate() {
                lines.push(format!("  [{}] {}", i + 1, url));
                kinds.push(LineKind::Body);
            }
        }

        (lines, kinds, urls)
    }

    fn render_thread_emails(
        emails: &[Email],
        raw_headers_cache: &HashMap<String, String>,
        prefer_html: bool,
    ) -> (Vec<String>, Vec<LineKind>, Vec<String>) {
        let mut lines = Vec::new();
        let mut kinds = Vec::new();
        let mut all_urls = Vec::new();
        for (i, email) in emails.iter().enumerate() {
            if i > 0 {
                lines.push(String::new());
                kinds.push(LineKind::Body);
                lines.push(String::new());
                kinds.push(LineKind::Separator);
                lines.push(String::new());
                kinds.push(LineKind::Body);
            }
            let raw = raw_headers_cache.get(&email.id).map(|s| s.as_str());
            Self::render_headers(email, raw, &mut lines, &mut kinds);
            lines.push(String::new());
            kinds.push(LineKind::Body);
            let body_text = Self::extract_body(email, prefer_html);
            for line in body_text.lines() {
                lines.push(line.to_string());
                kinds.push(LineKind::Body);
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
            kinds.push(LineKind::Body);
            lines.push("Links:".to_string());
            kinds.push(LineKind::Header);
            for (i, url) in all_urls.iter().enumerate() {
                lines.push(format!("  [{}] {}", i + 1, url));
                kinds.push(LineKind::Body);
            }
        }
        (lines, kinds, all_urls)
    }

    fn extract_body(email: &Email, prefer_html: bool) -> String {
        if prefer_html {
            // When user explicitly requests HTML rendering
            if let Some(ref html_body) = email.html_body {
                for part in html_body {
                    if let Some(value) = email.body_values.get(&part.part_id) {
                        return html_to_terminal(&value.value);
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
                        return html_to_terminal(&value.value);
                    }
                    return value.value.clone();
                }
            }
        }
        // Fall back to htmlBody when no plain text is available
        if let Some(ref html_body) = email.html_body {
            for part in html_body {
                if let Some(value) = email.body_values.get(&part.part_id) {
                    return html_to_terminal(&value.value);
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
                    crate::tui::reap_in_background(child);
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

    fn rerender_lines(&mut self) {
        if self.thread_id.is_some() && !self.thread_emails.is_empty() {
            let cache = if self.show_all_headers {
                &self.raw_headers_cache
            } else {
                // Empty map = use structured headers
                &HashMap::new()
            };
            let (lines, kinds, urls) =
                Self::render_thread_emails(&self.thread_emails, cache, self.prefer_html);
            self.lines = lines;
            self.line_kinds = kinds;
            self.urls = urls;
        } else if let Some(ref email) = self.email {
            let raw = if self.show_all_headers {
                self.raw_headers_cache.get(&email.id).map(|s| s.as_str())
            } else {
                None
            };
            let (lines, kinds, urls) = Self::render_email(email, raw, self.prefer_html);
            self.lines = lines;
            self.line_kinds = kinds;
            self.urls = urls;
        }
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
}

impl View for EmailView {
    fn wants_mouse(&self) -> bool {
        false
    }

    fn render(&self, term: &mut Terminal) -> io::Result<()> {
        term.clear()?;

        if self.loading {
            term.move_to(1, 1)?;
            let load_msg = if self.thread_id.is_some() {
                "Loading thread..."
            } else {
                "Loading email..."
            };
            term.write_truncated(load_msg, term.cols)?;
            term.move_to(term.rows, 1)?;
            term.set_status()?;
            term.write_truncated(" Loading... | q:back", term.cols)?;
            let remaining = (term.cols as usize).saturating_sub(20);
            for _ in 0..remaining {
                term.write_str(" ")?;
            }
            term.reset_attr()?;
            return term.flush();
        }

        if let Some(ref err) = self.error {
            term.move_to(1, 1)?;
            term.write_truncated(err, term.cols)?;
            term.move_to(term.rows, 1)?;
            term.set_status()?;
            term.write_truncated(" q:back", term.cols)?;
            let remaining = (term.cols as usize).saturating_sub(7);
            for _ in 0..remaining {
                term.write_str(" ")?;
            }
            term.reset_attr()?;
            return term.flush();
        }

        if self.url_picking {
            term.move_to(1, 1)?;
            term.set_header()?;
            term.write_truncated("Open URL:", term.cols)?;
            term.reset_attr()?;

            let max_items = (term.rows as usize).saturating_sub(3);
            let url_scroll = if self.url_cursor >= max_items {
                self.url_cursor - max_items + 1
            } else {
                0
            };

            for (i, url) in self
                .urls
                .iter()
                .skip(url_scroll)
                .enumerate()
                .take(max_items)
            {
                let row = 2 + i as u16;
                term.move_to(row, 1)?;

                let display_idx = url_scroll + i;
                let line = format!("  [{}] {}", display_idx + 1, url);

                if display_idx == self.url_cursor {
                    term.set_selection()?;
                }

                term.write_truncated(&line, term.cols)?;
                term.reset_attr()?;
            }

            // Status bar
            term.move_to(term.rows, 1)?;
            term.set_status()?;
            let status = format!(
                " {}/{} | 1-{}:open n/p:navigate RET:open Esc:cancel",
                self.url_cursor + 1,
                self.urls.len(),
                self.urls.len()
            );
            term.write_truncated(&status, term.cols)?;
            let remaining = (term.cols as usize).saturating_sub(status.len());
            for _ in 0..remaining {
                term.write_str(" ")?;
            }
            term.reset_attr()?;

            return term.flush();
        }

        if self.move_mode {
            term.move_to(1, 1)?;
            term.set_header()?;
            term.write_truncated("Move to mailbox:", term.cols)?;
            term.reset_attr()?;

            let max_items = (term.rows as usize).saturating_sub(3);
            let scroll_offset = if self.move_cursor >= max_items {
                self.move_cursor - max_items + 1
            } else {
                0
            };

            for (i, mailbox) in self
                .mailboxes
                .iter()
                .skip(scroll_offset)
                .enumerate()
                .take(max_items)
            {
                let row = 2 + i as u16;
                term.move_to(row, 1)?;

                let display_idx = scroll_offset + i;
                let line = format!("  {}", mailbox.name);

                if display_idx == self.move_cursor {
                    term.set_selection()?;
                }

                term.write_truncated(&line, term.cols)?;
                term.reset_attr()?;
            }

            // Status bar
            term.move_to(term.rows, 1)?;
            term.set_status()?;
            let status = format!(
                " {}/{} | n/p:navigate RET:move Esc:cancel",
                self.move_cursor + 1,
                self.mailboxes.len()
            );
            term.write_truncated(&status, term.cols)?;
            let remaining = (term.cols as usize).saturating_sub(status.len());
            for _ in 0..remaining {
                term.write_str(" ")?;
            }
            term.reset_attr()?;

            return term.flush();
        }

        let visible_rows = (term.rows as usize).saturating_sub(1);
        let width = term.cols as usize;

        let mut row_idx = 0;
        for (i, line) in self.lines.iter().skip(self.scroll).enumerate() {
            if row_idx >= visible_rows {
                break;
            }

            let abs_idx = self.scroll + i;
            let kind = self
                .line_kinds
                .get(abs_idx)
                .copied()
                .unwrap_or(LineKind::Body);

            match kind {
                LineKind::Header => {
                    let row = 1 + row_idx as u16;
                    term.move_to(row, 1)?;
                    term.set_header()?;
                    term.write_truncated(line, term.cols)?;
                    term.reset_attr()?;
                    row_idx += 1;
                }
                LineKind::Separator => {
                    let row = 1 + row_idx as u16;
                    term.move_to(row, 1)?;
                    // Bar spanning entire width
                    term.set_status()?;
                    let pad = " ".repeat(width);
                    term.write_str(&pad)?;
                    term.reset_attr()?;
                    row_idx += 1;
                }
                LineKind::Body => {
                    if line.is_empty() {
                        let row = 1 + row_idx as u16;
                        term.move_to(row, 1)?;
                        row_idx += 1;
                    } else {
                        for segment in wrap_line(line, width) {
                            if row_idx >= visible_rows {
                                break;
                            }
                            let row = 1 + row_idx as u16;
                            term.move_to(row, 1)?;
                            term.write_truncated(segment, term.cols)?;
                            row_idx += 1;
                        }
                    }
                }
            }
        }

        // Status bar
        term.move_to(term.rows, 1)?;
        term.set_status()?;
        let total_lines = self.lines.len();
        let base_status = if self.url_picking {
            format!(
                " line {}/{} | Open URL [1-{}] n/p:navigate RET:open or any key to cancel",
                self.scroll + 1,
                total_lines,
                self.urls.len()
            )
        } else if self.attachment_picking {
            format!(
                " line {}/{} | Pick attachment [1-{}] or any key to cancel",
                self.scroll + 1,
                total_lines,
                self.attachment_count()
            )
        } else if self.pending_reply_all.is_some() || self.pending_forward {
            format!(
                " line {}/{} | Loading reply data... | q:back",
                self.scroll + 1,
                total_lines
            )
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
                " line {}/{} | q:back n/p:unread j/k:scroll r:reply R:reply-all F:forward h:html{}{}{} a:archive d:delete m:move J:spam H:ham S:score ?:help",
                self.scroll + 1,
                total_lines,
                att_hint,
                expire_hint,
                url_hint
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

        // URL picking mode
        if self.url_picking {
            self.url_picking = false;
            match key {
                Key::Char(c @ '1'..='9') => {
                    let index = (c as usize) - ('1' as usize);
                    self.open_url(index);
                }
                Key::Char('n') | Key::Char('j') | Key::Down => {
                    if !self.urls.is_empty() && self.url_cursor + 1 < self.urls.len() {
                        self.url_cursor += 1;
                    }
                    self.url_picking = true; // stay in picking mode
                }
                Key::Char('p') | Key::Char('k') | Key::Up => {
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
                Key::Char('n') | Key::Char('j') | Key::Down | Key::ScrollDown => {
                    if !self.mailboxes.is_empty() && self.move_cursor + 1 < self.mailboxes.len() {
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

        let page = (term_rows as usize).saturating_sub(1);
        match key {
            Key::Char('q') => ViewAction::Pop,
            Key::Char('n') => {
                if !self.navigate_unread(true) && self.scroll + 1 < self.lines.len() {
                    self.scroll += 1;
                }
                ViewAction::Continue
            }
            Key::Char('p') => {
                if !self.navigate_unread(false) && self.scroll > 0 {
                    self.scroll -= 1;
                }
                ViewAction::Continue
            }
            Key::Char('j') | Key::Down => {
                if self.scroll + 1 < self.lines.len() {
                    self.scroll += 1;
                }
                ViewAction::Continue
            }
            Key::Char('k') | Key::Up => {
                if self.scroll > 0 {
                    self.scroll -= 1;
                }
                ViewAction::Continue
            }
            Key::PageDown | Key::Char(' ') => {
                self.scroll = (self.scroll + page).min(self.lines.len().saturating_sub(1));
                ViewAction::Continue
            }
            Key::PageUp => {
                self.scroll = self.scroll.saturating_sub(page);
                ViewAction::Continue
            }
            Key::Home => {
                self.scroll = 0;
                ViewAction::Continue
            }
            Key::End => {
                self.scroll = self.lines.len().saturating_sub(1);
                ViewAction::Continue
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
            Key::ScrollUp => {
                if self.scroll > 0 {
                    self.scroll -= 1;
                }
                ViewAction::Continue
            }
            Key::ScrollDown => {
                if self.scroll + 1 < self.lines.len() {
                    self.scroll += 1;
                }
                ViewAction::Continue
            }
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
                        let empty = HashMap::new();
                        let cache = if self.show_all_headers {
                            &self.raw_headers_cache
                        } else {
                            &empty
                        };
                        let (lines, kinds, urls) = Self::render_thread_emails(
                            &self.thread_emails,
                            cache,
                            self.prefer_html,
                        );
                        self.lines = lines;
                        self.line_kinds = kinds;
                        self.urls = urls;
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
                        let raw = if self.show_all_headers {
                            self.raw_headers_cache.get(&email.id).map(|s| s.as_str())
                        } else {
                            None
                        };
                        let (lines, kinds, urls) = Self::render_email(email, raw, self.prefer_html);
                        self.lines = lines;
                        self.line_kinds = kinds;
                        self.urls = urls;
                        self.email = Some(email.clone());
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
                            Ok(child) => crate::tui::reap_in_background(child),
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
    let cmd = cmd.replace("\"{url}\"", "{url}").replace("'{url}'", "{url}");
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
            assert!(out.status.success(), "{cmd}: {}", String::from_utf8_lossy(&out.stderr));
            assert_eq!(String::from_utf8_lossy(&out.stdout).trim_end(), expected, "{cmd}");
        }
    }
}

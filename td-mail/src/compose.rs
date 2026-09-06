use std::fs;
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// A draft ready to hand to `$EDITOR`: the editor text plus any files that
/// should be attached via MML (Emacs message-mode) when the message is sent.
pub struct ComposeDraft {
    pub body: String,
    pub attachments: Vec<DraftAttachment>,
}

impl ComposeDraft {
    /// A plain text-only draft with no attachments.
    pub fn text(body: String) -> Self {
        ComposeDraft {
            body,
            attachments: Vec::new(),
        }
    }
}

impl From<String> for ComposeDraft {
    fn from(body: String) -> Self {
        ComposeDraft::text(body)
    }
}

/// A file to be written next to the draft and referenced by an MML `<#part>`
/// tag, so message-mode encodes it as a MIME part on send.
pub struct DraftAttachment {
    /// Suggested on-disk / display name, e.g. `forwarded.eml`.
    pub filename: String,
    pub content_type: String,
    pub description: Option<String>,
    pub data: Vec<u8>,
}

/// Build a blank compose draft template.
pub fn build_compose_draft(from: &str) -> String {
    format!(
        "From: {}\nTo: \nCc: \nSubject: \n--text follows this line--\n\n",
        from
    )
}

/// Build a reply draft from an existing email.
pub fn build_reply_draft(email: &crate::jmap::types::Email, reply_all: bool, from: &str) -> String {
    // Determine To: address
    let to = if let Some(ref reply_to) = email.reply_to {
        format_address_list(reply_to)
    } else if let Some(ref email_from) = email.from {
        format_address_list(email_from)
    } else {
        String::new()
    };

    // Determine Cc: for reply-all
    let cc = if reply_all {
        let from_email_lower = extract_email_addr(from).map(|s| s.to_lowercase());
        let mut cc_addrs = Vec::new();

        // Add original To recipients (minus self)
        if let Some(ref orig_to) = email.to {
            for addr in orig_to {
                if let Some(ref email_addr) = addr.email {
                    if from_email_lower
                        .as_ref()
                        .map(|me| email_addr.to_lowercase() != *me)
                        .unwrap_or(true)
                    {
                        cc_addrs.push(addr.to_string());
                    }
                }
            }
        }

        // Add original Cc recipients (minus self)
        if let Some(ref orig_cc) = email.cc {
            for addr in orig_cc {
                if let Some(ref email_addr) = addr.email {
                    if from_email_lower
                        .as_ref()
                        .map(|me| email_addr.to_lowercase() != *me)
                        .unwrap_or(true)
                    {
                        cc_addrs.push(addr.to_string());
                    }
                }
            }
        }

        cc_addrs.join(", ")
    } else {
        String::new()
    };

    // Subject with Re: prefix
    let subject = match email.subject.as_deref() {
        Some(s) if s.starts_with("Re: ") || s.starts_with("re: ") => s.to_string(),
        Some(s) => format!("Re: {}", s),
        None => "Re: ".to_string(),
    };

    // In-Reply-To header
    let in_reply_to = email
        .message_id
        .as_ref()
        .and_then(|ids| ids.first())
        .map(|id| format!("<{}>", id.trim_matches(|c| c == '<' || c == '>')));

    // References header
    let references = {
        let mut refs = Vec::new();
        if let Some(ref orig_refs) = email.references {
            for r in orig_refs {
                refs.push(format!("<{}>", r.trim_matches(|c| c == '<' || c == '>')));
            }
        }
        if let Some(ref msg_ids) = email.message_id {
            if let Some(id) = msg_ids.first() {
                let formatted = format!("<{}>", id.trim_matches(|c| c == '<' || c == '>'));
                if !refs.contains(&formatted) {
                    refs.push(formatted);
                }
            }
        }
        if refs.is_empty() {
            None
        } else {
            Some(refs.join(" "))
        }
    };

    // Quoted body
    let body_text = extract_body_text(email);
    let sender_display = email
        .from
        .as_ref()
        .and_then(|addrs| addrs.first())
        .map(|a| a.to_string())
        .unwrap_or_else(|| "(unknown)".to_string());
    let date = email
        .sent_at
        .as_deref()
        .or(email.received_at.as_deref())
        .unwrap_or("(unknown date)");

    let quoted: String = body_text
        .lines()
        .map(|line| format!("> {}", line))
        .collect::<Vec<_>>()
        .join("\n");

    let mut draft = format!("From: {}\nTo: {}\n", from, to);
    if !cc.is_empty() {
        draft.push_str(&format!("Cc: {}\n", cc));
    }
    draft.push_str(&format!("Subject: {}\n", subject));
    if let Some(ref irt) = in_reply_to {
        draft.push_str(&format!("In-Reply-To: {}\n", irt));
    }
    if let Some(ref refs) = references {
        draft.push_str(&format!("References: {}\n", refs));
    }
    draft.push_str("--text follows this line--\n");
    draft.push_str(&format!(
        "\nOn {}, {} wrote:\n{}\n",
        date, sender_display, quoted
    ));

    draft
}

/// Build a forward draft from an existing email.
pub fn build_forward_draft(email: &crate::jmap::types::Email, from: &str) -> String {
    // Subject with Fwd: prefix
    let subject = match email.subject.as_deref() {
        Some(s) if s.starts_with("Fwd: ") || s.starts_with("fwd: ") => s.to_string(),
        Some(s) => format!("Fwd: {}", s),
        None => "Fwd: ".to_string(),
    };

    // Original message info
    let orig_from = email
        .from
        .as_ref()
        .map(|addrs| format_address_list(addrs))
        .unwrap_or_else(|| "(unknown)".to_string());
    let orig_to = email
        .to
        .as_ref()
        .map(|addrs| format_address_list(addrs))
        .unwrap_or_default();
    let orig_cc = email
        .cc
        .as_ref()
        .map(|addrs| format_address_list(addrs))
        .unwrap_or_default();
    let date = email
        .sent_at
        .as_deref()
        .or(email.received_at.as_deref())
        .unwrap_or("(unknown date)");
    let orig_subject = email.subject.as_deref().unwrap_or("(no subject)");

    let body_text = extract_body_text(email);

    let mut draft = format!("From: {}\nTo: \nSubject: {}\n", from, subject);

    draft.push_str("--text follows this line--\n");
    draft.push_str("\n---------- Forwarded message ----------\n");
    draft.push_str(&format!("From: {}\n", orig_from));
    draft.push_str(&format!("Date: {}\n", date));
    draft.push_str(&format!("Subject: {}\n", orig_subject));
    draft.push_str(&format!("To: {}\n", orig_to));
    if !orig_cc.is_empty() {
        draft.push_str(&format!("Cc: {}\n", orig_cc));
    }
    draft.push('\n');
    draft.push_str(&body_text);
    draft.push('\n');

    draft
}

/// Build a forward draft that carries the original message as a
/// `message/rfc822` attachment (preserving the HTML part and everything else).
///
/// `email` supplies header metadata (subject, attachment name); `raw` is the
/// full RFC822 bytes of the original message, written to a sidecar file and
/// referenced via an MML `<#part>` tag by [`write_compose_draft`].
pub fn build_forward_attachment_draft(
    email: Option<&crate::jmap::types::Email>,
    raw: Vec<u8>,
    from: &str,
) -> ComposeDraft {
    let orig_subject = email
        .and_then(|e| e.subject.as_deref())
        .unwrap_or("(no subject)");

    let subject = match email.and_then(|e| e.subject.as_deref()) {
        Some(s) if s.starts_with("Fwd: ") || s.starts_with("fwd: ") => s.to_string(),
        Some(s) => format!("Fwd: {}", s),
        _ => "Fwd: ".to_string(),
    };

    let mut body = format!("From: {}\nTo: \nSubject: {}\n", from, subject);
    body.push_str("--text follows this line--\n");
    body.push_str("\n(forwarded message attached)\n");

    let attachment = DraftAttachment {
        filename: forward_attachment_filename(orig_subject),
        content_type: "message/rfc822".to_string(),
        description: Some(format!("Forwarded message: {}", orig_subject)),
        data: raw,
    };

    ComposeDraft {
        body,
        attachments: vec![attachment],
    }
}

/// Derive a safe `.eml` filename from a subject for the forwarded attachment.
fn forward_attachment_filename(subject: &str) -> String {
    let mut name: String = subject
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    // Collapse to a reasonable length and trim filler underscores.
    if name.len() > 60 {
        // Walk back to a char boundary (alphanumerics here are ASCII, but the
        // subject may contain multi-byte letters that pass is_alphanumeric).
        let mut end = 60;
        while end > 0 && !name.is_char_boundary(end) {
            end -= 1;
        }
        name.truncate(end);
    }
    let name = name.trim_matches('_');
    if name.is_empty() {
        "forwarded.eml".to_string()
    } else {
        format!("{}.eml", name)
    }
}

fn format_address_list(addrs: &[crate::jmap::types::EmailAddress]) -> String {
    addrs
        .iter()
        .map(|a| a.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

fn extract_email_addr(from_header: &str) -> Option<String> {
    if let (Some(start), Some(end)) = (from_header.find('<'), from_header.rfind('>')) {
        if end > start + 1 {
            let addr = from_header[start + 1..end].trim();
            if !addr.is_empty() {
                return Some(addr.to_string());
            }
        }
    }
    let trimmed = from_header.trim();
    if trimmed.contains('@') {
        Some(trimmed.to_string())
    } else {
        None
    }
}

pub(crate) fn extract_body_text(email: &crate::jmap::types::Email) -> String {
    // Prefer textBody (plain text) — it preserves the author's formatting.
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
                    return html_to_plain(&value.value);
                }
                return value.value.clone();
            }
        }
    }
    // Fall back to htmlBody when no plain text is available
    if let Some(ref html_body) = email.html_body {
        for part in html_body {
            if let Some(value) = email.body_values.get(&part.part_id) {
                return html_to_plain(&value.value);
            }
        }
    }
    email.preview.as_deref().unwrap_or("(no body)").to_string()
}

/// Heuristic check: does this text look like HTML rather than plain text?
/// Checks for common HTML structural tags anywhere in the content.
fn looks_like_html(text: &str) -> bool {
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

/// Convert HTML to plain text (no ANSI formatting). Used for drafts and CLI.
///
/// Rendering cannot fail, so there is no fallback to the raw HTML any more.
fn html_to_plain(html: &str) -> String {
    crate::html::to_text(html.as_bytes(), 80)
}

/// Paths produced by [`write_compose_draft`] that the caller must remove once
/// the editor exits: the draft file plus an optional attachments directory.
pub struct PreparedDraft {
    pub draft_path: PathBuf,
    pub attachment_dir: Option<PathBuf>,
}

/// Write a [`ComposeDraft`] to disk: any attachments go into a private
/// per-draft subdirectory and are referenced from the body via MML `<#part>`
/// tags, then the (possibly augmented) body is written to the draft file.
pub fn write_compose_draft(draft: &ComposeDraft) -> io::Result<PreparedDraft> {
    let dir = draft_dir();
    fs::create_dir_all(&dir)?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;

    let stamp = format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );

    let mut body = draft.body.clone();
    let attachment_dir = if draft.attachments.is_empty() {
        None
    } else {
        let att_dir = dir.join(format!("td-mail-att-{}", stamp));
        fs::create_dir_all(&att_dir)?;
        fs::set_permissions(&att_dir, fs::Permissions::from_mode(0o700))?;
        for att in &draft.attachments {
            let path = att_dir.join(sanitize_filename(&att.filename));
            write_secure_file(&path, &att.data)?;
            body.push_str(&mml_part(
                &att.content_type,
                &path,
                att.description.as_deref(),
            ));
        }
        Some(att_dir)
    };

    let draft_path = dir.join(format!("td-mail-draft-{}.eml", stamp));
    write_secure_file(&draft_path, body.as_bytes())?;

    Ok(PreparedDraft {
        draft_path,
        attachment_dir,
    })
}

/// Write `bytes` to `path`, creating it fresh with 0600 permissions.
fn write_secure_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    io::Write::write_all(&mut file, bytes)
}

/// Render an MML part tag that tells message-mode to attach `path` on send.
fn mml_part(content_type: &str, path: &Path, description: Option<&str>) -> String {
    let mut tag = format!(
        "\n<#part type=\"{}\" filename=\"{}\" disposition=\"attachment\"",
        content_type,
        path.display()
    );
    if let Some(desc) = description {
        // Strip characters that would terminate the attribute / tag.
        let clean: String = desc
            .chars()
            .map(|c| match c {
                '"' => '\'',
                '\n' | '\r' => ' ',
                other => other,
            })
            .collect();
        tag.push_str(&format!(" description=\"{}\"", clean));
    }
    tag.push_str(">\n<#/part>\n");
    tag
}

/// Make a filename safe to use as a single path component.
fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | '\0' => '_',
            other => other,
        })
        .collect();
    let cleaned = cleaned.trim_matches(['.', ' ']);
    if cleaned.is_empty() {
        "forwarded.eml".to_string()
    } else {
        cleaned.to_string()
    }
}

fn draft_dir() -> PathBuf {
    draft_dir_from_env(
        std::env::var("XDG_RUNTIME_DIR").ok(),
        std::env::var("XDG_STATE_HOME").ok(),
        std::env::var("HOME").ok(),
    )
}

fn draft_dir_from_env(
    xdg_runtime_dir: Option<String>,
    xdg_state_home: Option<String>,
    home: Option<String>,
) -> PathBuf {
    if let Some(runtime_dir) = xdg_runtime_dir {
        let trimmed = runtime_dir.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed).join("td-mail").join("drafts");
        }
    }

    let state_dir = if let Some(xdg) = xdg_state_home {
        PathBuf::from(xdg)
    } else if let Some(home) = home {
        PathBuf::from(home).join(".local").join("state")
    } else {
        PathBuf::from(".")
    };

    state_dir.join("td-mail").join("drafts")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_compose_draft() {
        let draft = build_compose_draft("me@example.com");
        assert!(draft.contains("From: me@example.com"));
        assert!(draft.contains("To: \n"));
        assert!(draft.contains("Subject: \n"));
        assert!(draft.contains("--text follows this line--"));
    }

    #[test]
    fn test_build_reply_draft() {
        use crate::jmap::types::{Email, EmailAddress};
        use std::collections::HashMap;

        let email = Email {
            id: "test-id".to_string(),
            thread_id: None,
            from: Some(vec![EmailAddress {
                name: Some("Sender".to_string()),
                email: Some("sender@example.com".to_string()),
            }]),
            to: Some(vec![EmailAddress {
                name: None,
                email: Some("me@example.com".to_string()),
            }]),
            cc: None,
            reply_to: None,
            subject: Some("Hello".to_string()),
            received_at: Some("2024-01-01T00:00:00Z".to_string()),
            sent_at: Some("2024-01-01T00:00:00Z".to_string()),
            preview: Some("Preview text".to_string()),
            text_body: None,
            html_body: None,
            body_values: HashMap::new(),
            keywords: HashMap::new(),
            mailbox_ids: HashMap::new(),
            message_id: Some(vec!["abc@example.com".to_string()]),
            references: None,
            attachments: None,
            extra: HashMap::new(),
        };

        let draft = build_reply_draft(&email, false, "me@example.com");
        assert!(draft.contains("To: Sender <sender@example.com>"));
        assert!(draft.contains("Subject: Re: Hello"));
        assert!(draft.contains("In-Reply-To: <abc@example.com>"));
        assert!(draft.contains("> Preview text"));

        // Reply-all should include original To minus self
        let draft_all = build_reply_draft(&email, true, "me@example.com");
        assert!(!draft_all.contains("Cc:")); // self was the only To recipient
    }

    #[test]
    fn test_build_reply_draft_reply_all_skips_self_for_named_from_header() {
        use crate::jmap::types::{Email, EmailAddress};
        use std::collections::HashMap;

        let email = Email {
            id: "test-id".to_string(),
            thread_id: None,
            from: Some(vec![EmailAddress {
                name: Some("Sender".to_string()),
                email: Some("sender@example.com".to_string()),
            }]),
            to: Some(vec![
                EmailAddress {
                    name: Some("Example User".to_string()),
                    email: Some("user@example.com".to_string()),
                },
                EmailAddress {
                    name: Some("Other".to_string()),
                    email: Some("other@example.com".to_string()),
                },
            ]),
            cc: None,
            reply_to: None,
            subject: Some("Hello".to_string()),
            received_at: Some("2024-01-01T00:00:00Z".to_string()),
            sent_at: Some("2024-01-01T00:00:00Z".to_string()),
            preview: Some("Preview text".to_string()),
            text_body: None,
            html_body: None,
            body_values: HashMap::new(),
            keywords: HashMap::new(),
            mailbox_ids: HashMap::new(),
            message_id: Some(vec!["abc@example.com".to_string()]),
            references: None,
            attachments: None,
            extra: HashMap::new(),
        };

        let draft = build_reply_draft(&email, true, "Example User <user@example.com>");
        assert!(!draft.contains("Cc: Example User <user@example.com>"));
        assert!(draft.contains("Cc: Other <other@example.com>"));
    }

    #[test]
    fn test_build_forward_draft() {
        use crate::jmap::types::{Email, EmailAddress};
        use std::collections::HashMap;

        let email = Email {
            id: "test-id".to_string(),
            thread_id: None,
            from: Some(vec![EmailAddress {
                name: Some("Sender".to_string()),
                email: Some("sender@example.com".to_string()),
            }]),
            to: Some(vec![EmailAddress {
                name: None,
                email: Some("me@example.com".to_string()),
            }]),
            cc: Some(vec![EmailAddress {
                name: Some("Other".to_string()),
                email: Some("other@example.com".to_string()),
            }]),
            reply_to: None,
            subject: Some("Hello".to_string()),
            received_at: Some("2024-01-01T00:00:00Z".to_string()),
            sent_at: Some("2024-01-01T00:00:00Z".to_string()),
            preview: Some("Preview text".to_string()),
            text_body: None,
            html_body: None,
            body_values: HashMap::new(),
            keywords: HashMap::new(),
            mailbox_ids: HashMap::new(),
            message_id: None,
            references: None,
            attachments: None,
            extra: HashMap::new(),
        };

        let draft = build_forward_draft(&email, "me@example.com");
        assert!(draft.contains("From: me@example.com"));
        assert!(draft.contains("To: \n"));
        assert!(draft.contains("Subject: Fwd: Hello"));
        assert!(draft.contains("---------- Forwarded message ----------"));
        assert!(draft.contains("From: Sender <sender@example.com>"));
        assert!(draft.contains("Date: 2024-01-01T00:00:00Z"));
        assert!(draft.contains("Subject: Hello"));
        assert!(draft.contains("To: me@example.com"));
        assert!(draft.contains("Cc: Other <other@example.com>"));
        assert!(draft.contains("Preview text"));
    }

    #[test]
    fn test_build_forward_draft_already_prefixed() {
        use crate::jmap::types::{Email, EmailAddress};
        use std::collections::HashMap;

        let email = Email {
            id: "test-id".to_string(),
            thread_id: None,
            from: Some(vec![EmailAddress {
                name: None,
                email: Some("sender@example.com".to_string()),
            }]),
            to: None,
            cc: None,
            reply_to: None,
            subject: Some("Fwd: Already forwarded".to_string()),
            received_at: None,
            sent_at: None,
            preview: Some("body".to_string()),
            text_body: None,
            html_body: None,
            body_values: HashMap::new(),
            keywords: HashMap::new(),
            mailbox_ids: HashMap::new(),
            message_id: None,
            references: None,
            attachments: None,
            extra: HashMap::new(),
        };

        let draft = build_forward_draft(&email, "me@example.com");
        assert!(draft.contains("Subject: Fwd: Already forwarded\n"));
        // Should not double-prefix
        assert!(!draft.contains("Fwd: Fwd:"));
    }

    #[test]
    fn test_build_forward_attachment_draft() {
        use crate::jmap::types::{Email, EmailAddress};
        use std::collections::HashMap;

        let email = Email {
            id: "test-id".to_string(),
            thread_id: None,
            from: Some(vec![EmailAddress {
                name: Some("Sender".to_string()),
                email: Some("sender@example.com".to_string()),
            }]),
            to: None,
            cc: None,
            reply_to: None,
            subject: Some("Hello World".to_string()),
            received_at: None,
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
        };

        let raw = b"Subject: Hello World\r\nContent-Type: text/html\r\n\r\n<b>hi</b>";
        let draft = build_forward_attachment_draft(Some(&email), raw.to_vec(), "me@example.com");

        assert!(draft.body.contains("Subject: Fwd: Hello World"));
        assert!(draft.body.contains("--text follows this line--"));
        assert!(draft.body.contains("(forwarded message attached)"));
        // The raw bytes must travel as a single message/rfc822 attachment, not
        // be flattened into the editor body.
        assert_eq!(draft.attachments.len(), 1);
        let att = &draft.attachments[0];
        assert_eq!(att.content_type, "message/rfc822");
        assert_eq!(att.filename, "Hello_World.eml");
        assert_eq!(att.data, raw.to_vec());
        assert!(!draft.body.contains("<b>hi</b>"));
    }

    #[test]
    fn test_build_forward_attachment_draft_no_metadata() {
        // Falls back to a generic subject/filename when the email is unknown.
        let draft = build_forward_attachment_draft(None, b"raw".to_vec(), "me@example.com");
        assert!(draft.body.contains("Subject: Fwd: \n"));
        assert_eq!(draft.attachments[0].filename, "no_subject.eml");
    }

    #[test]
    fn test_forward_attachment_filename() {
        assert_eq!(
            forward_attachment_filename("Hello, World!"),
            "Hello__World.eml"
        );
        assert_eq!(forward_attachment_filename("   "), "forwarded.eml");
        assert_eq!(forward_attachment_filename(""), "forwarded.eml");
        // Path separators must never leak into the derived name.
        assert!(!forward_attachment_filename("a/b\\c").contains('/'));
        assert!(!forward_attachment_filename("a/b\\c").contains('\\'));
    }

    #[test]
    fn test_mml_part_escapes_description_and_targets_file() {
        let part = mml_part(
            "message/rfc822",
            Path::new("/tmp/fwd/orig.eml"),
            Some("Forwarded: say \"hi\"\nthere"),
        );
        assert!(part.contains("type=\"message/rfc822\""));
        assert!(part.contains("filename=\"/tmp/fwd/orig.eml\""));
        assert!(part.contains("disposition=\"attachment\""));
        assert!(part.trim_end().ends_with("<#/part>"));
        // A stray quote/newline in the description must not break the tag.
        assert!(!part.contains("say \"hi\""));
        assert!(part.contains("say 'hi' there"));
    }

    #[test]
    fn test_sanitize_filename() {
        assert_eq!(sanitize_filename("a/b"), "a_b");
        assert_eq!(sanitize_filename("..."), "forwarded.eml");
        assert_eq!(sanitize_filename("  spaced.eml  "), "spaced.eml");
    }

    #[test]
    fn test_draft_dir_uses_xdg_runtime_dir() {
        assert_eq!(
            draft_dir_from_env(
                Some("/tmp/runtime-test".to_string()),
                Some("/tmp/state-test".to_string()),
                Some("/home/example".to_string())
            ),
            PathBuf::from("/tmp/runtime-test")
                .join("td-mail")
                .join("drafts")
        );
    }

    #[test]
    fn test_draft_dir_falls_back_to_state_home() {
        assert_eq!(
            draft_dir_from_env(
                None,
                Some("/tmp/state-test".to_string()),
                Some("/home/example".to_string())
            ),
            PathBuf::from("/tmp/state-test").join("td-mail").join("drafts")
        );
    }

    #[test]
    fn test_draft_dir_falls_back_to_home_state() {
        assert_eq!(
            draft_dir_from_env(None, None, Some("/home/example".to_string())),
            PathBuf::from("/home/example")
                .join(".local")
                .join("state")
                .join("td-mail")
                .join("drafts")
        );
    }

    #[test]
    fn test_looks_like_html_handles_multibyte_at_sample_boundary() {
        // Regression: text with multi-byte UTF-8 (U+034F CGJ, as found in
        // newsletter preview "tracking pixel" runs) used to panic when the
        // 2000-byte sample boundary fell inside a code point.
        let mut s = String::from("AAA");
        for _ in 0..1000 {
            s.push_str("X\u{34f}");
        }
        assert!(!looks_like_html(&s));
    }
}

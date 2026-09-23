use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// A draft ready to retain and edit: the text plus any files that should
/// be attached via MML (Emacs message-mode) when the message is sent.
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
                        cc_addrs.push(addr.header_form());
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
                        cc_addrs.push(addr.header_form());
                    }
                }
            }
        }

        cc_addrs.join(", ")
    } else {
        String::new()
    };

    // Subject with Re: prefix
    let subject = match email.subject.as_deref().map(header_text) {
        Some(s) if s.starts_with("Re: ") || s.starts_with("re: ") => s,
        Some(s) => format!("Re: {}", s),
        None => "Re: ".to_string(),
    };

    // In-Reply-To header
    let in_reply_to = email
        .message_id
        .as_ref()
        .and_then(|ids| ids.first())
        .map(|id| message_id_form(id));

    // References header
    let references = {
        let mut refs = Vec::new();
        if let Some(ref orig_refs) = email.references {
            for r in orig_refs {
                refs.push(message_id_form(r));
            }
        }
        if let Some(ref msg_ids) = email.message_id {
            if let Some(id) = msg_ids.first() {
                let formatted = message_id_form(id);
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
    draft.push_str(&quote_mml(&format!(
        "\nOn {}, {} wrote:\n{}\n",
        date, sender_display, quoted
    )));

    draft
}

/// Build a forward draft from an existing email.
pub fn build_forward_draft(email: &crate::jmap::types::Email, from: &str) -> String {
    // Subject with Fwd: prefix
    let subject = match email.subject.as_deref().map(header_text) {
        Some(s) if s.starts_with("Fwd: ") || s.starts_with("fwd: ") => s,
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
    let mut body = String::from("\n---------- Forwarded message ----------\n");
    body.push_str(&format!("From: {}\n", orig_from));
    body.push_str(&format!("Date: {}\n", date));
    body.push_str(&format!("Subject: {}\n", orig_subject));
    body.push_str(&format!("To: {}\n", orig_to));
    if !orig_cc.is_empty() {
        body.push_str(&format!("Cc: {}\n", orig_cc));
    }
    body.push('\n');
    body.push_str(&body_text);
    body.push('\n');
    draft.push_str(&quote_mml(&body));

    draft
}

/// `text` as it is pasted into a draft's body: a line beginning `<#`,
/// an MML tag as the sender would read it, is quoted `<#!`, as Emacs
/// quotes yanked text, so a message that names a file to attach forwards
/// as the text it is (`submit::parse_draft` sends `<#!` as `<#`). The
/// whole block the message supplies goes through here, its decoded
/// header values included, since a newline in one begins a line too.
fn quote_mml(text: &str) -> String {
    let lines: Vec<String> = text
        .split('\n')
        .map(|line| match line.strip_prefix("<#") {
            Some(rest) => format!("<#!{rest}"),
            None => line.to_string(),
        })
        .collect();
    lines.join("\n")
}

/// A decoded header value on the one draft line that carries it: a
/// control character, the newline a decoded header may hold included,
/// becomes a space, so it cannot begin another header the draft would
/// then send.
fn header_text(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// A message id in angle brackets on one header line.
fn message_id_form(id: &str) -> String {
    format!(
        "<{}>",
        header_text(id.trim_matches(|c| c == '<' || c == '>')).trim()
    )
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

    let subject = match email.and_then(|e| e.subject.as_deref()).map(header_text) {
        Some(s) if s.starts_with("Fwd: ") || s.starts_with("fwd: ") => s,
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

/// The addresses as a draft header, names quoted where they must be so
/// the draft reads back as it was written (`submit::parse_draft`).
fn format_address_list(addrs: &[crate::jmap::types::EmailAddress]) -> String {
    addrs
        .iter()
        .map(|a| a.header_form())
        .filter(|a| !a.is_empty())
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

/// Retained draft and sidecar paths. Nothing here ever deletes them.
pub struct PreparedDraft {
    pub draft_path: PathBuf,
    pub attachment_dir: Option<PathBuf>,
}

/// Write a [`ComposeDraft`] to disk: any attachments go into a private
/// per-draft subdirectory and are referenced from the body via MML `<#part>`
/// tags, then the (possibly augmented) body is written to the draft file.
pub fn write_compose_draft(draft: &ComposeDraft) -> io::Result<PreparedDraft> {
    write_compose_draft_in(draft, &draft_dir()?)
}

pub(crate) fn write_compose_draft_in(
    draft: &ComposeDraft,
    dir: &Path,
) -> io::Result<PreparedDraft> {
    if !draft.attachments.is_empty() && dir.to_str().is_none_or(|s| !valid_mml_attribute(s)) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "attachment directory cannot be represented by the draft's MML path",
        ));
    }
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let metadata = fs::symlink_metadata(dir)?;
    if !metadata.is_dir() || metadata.permissions().mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "draft directory must be a private directory, not a symlink",
        ));
    }

    let stamp = format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );

    prepare_draft(draft, dir, &stamp)
}

fn prepare_draft(draft: &ComposeDraft, dir: &Path, stamp: &str) -> io::Result<PreparedDraft> {
    let att_dir = dir.join(format!("td-mail-att-{stamp}"));
    let draft_path = dir.join(format!("td-mail-draft-{stamp}.eml"));
    let mut body = draft.body.clone();
    let mut parts = Vec::with_capacity(draft.attachments.len());
    let mut names = std::collections::BTreeSet::new();
    for att in &draft.attachments {
        let name = sanitize_filename(&att.filename);
        if !names.insert(name.clone()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "duplicate sanitized attachment filename",
            ));
        }
        let path = att_dir.join(name);
        body.push_str(&mml_part(
            &att.content_type,
            &path,
            None,
            att.description.as_deref(),
        )?);
        parts.push((path, att.data.as_slice()));
    }
    let mut attachment_created = false;
    let mut draft_created = false;
    let result = (|| {
        if !parts.is_empty() {
            fs::DirBuilder::new().mode(0o700).create(&att_dir)?;
            attachment_created = true;
            for (path, bytes) in &parts {
                let mut file = create_secure_file(path)?;
                io::Write::write_all(&mut file, bytes)?;
            }
        }
        let mut file = create_secure_file(&draft_path)?;
        draft_created = true;
        io::Write::write_all(&mut file, body.as_bytes())
    })();
    if let Err(error) = result {
        let mut detail = error.to_string();
        for (created, path, directory) in [
            (draft_created, &draft_path, false),
            (attachment_created, &att_dir, true),
        ] {
            if created {
                let cleanup = if directory {
                    fs::remove_dir_all(path)
                } else {
                    fs::remove_file(path)
                };
                if let Err(cleanup) = cleanup {
                    detail.push_str(&format!(
                        "; incomplete preparation remains at {path:?}: {cleanup}"
                    ));
                }
            }
        }
        return Err(io::Error::new(error.kind(), detail));
    }
    Ok(PreparedDraft {
        draft_path,
        attachment_dir: attachment_created.then_some(att_dir),
    })
}

pub(crate) fn create_secure_file(path: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

pub(crate) fn valid_mml_attribute(text: &str) -> bool {
    !text
        .chars()
        .any(|c| c.is_control() || matches!(c, '"' | '\\' | '<' | '>'))
}

/// Render an MML part tag that tells message-mode to attach `path` on send,
/// under `name` when the recipient is to see another than the file's.
pub(crate) fn mml_part(
    content_type: &str,
    path: &Path,
    name: Option<&str>,
    description: Option<&str>,
) -> io::Result<String> {
    let path = path
        .to_str()
        .filter(|text| valid_mml_attribute(text))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "attachment path cannot be represented by MML",
            )
        })?;
    if !valid_mml_attribute(content_type) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "attachment content type cannot be represented by MML",
        ));
    }
    let mut tag = format!(
        "\n<#part type=\"{}\" filename=\"{}\" disposition=\"attachment\"",
        content_type, path
    );
    if let Some(name) = name {
        if !valid_mml_attribute(name) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "attachment name cannot be represented by MML",
            ));
        }
        tag.push_str(&format!(" name=\"{}\"", name));
    }
    if let Some(desc) = description {
        // Strip characters that would terminate the attribute / tag.
        let clean: String = desc
            .chars()
            .map(|c| match c {
                '"' => '\'',
                '\\' => '/',
                '<' => '(',
                '>' => ')',
                c if c.is_control() => ' ',
                other => other,
            })
            .collect();
        tag.push_str(&format!(" description=\"{}\"", clean));
    }
    tag.push_str(">\n<#/part>\n");
    Ok(tag)
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

/// Replaces the draft at `path` with `bytes`, whole or not at all: they
/// are written to a private sibling, synced, and renamed over the path,
/// so a write that fails leaves the draft as it was and a symlink put at
/// the path is replaced, not followed. A sibling left by a failure is
/// removed; the draft never is.
pub(crate) fn replace_draft(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "draft path has no directory")
        })?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "draft path has no name"))?;
    let mut sibling = std::ffi::OsString::from(".");
    sibling.push(name);
    sibling.push(format!(
        ".tmp-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let sibling = dir.join(sibling);
    let written = create_secure_file(&sibling).and_then(|mut file| {
        use std::io::Write;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&sibling, path)
    });
    if written.is_err() {
        let _ = fs::remove_file(&sibling);
    }
    written
}

/// Moves a sent draft, and its attachment sidecar when it has one, out
/// of the drafts directory into the `sent` directory beside it
/// (`.../td-mail/sent` for `.../td-mail/drafts`), made private as the
/// drafts directory is; answers the draft's new path. The sidecar must
/// be beside the draft. Both move or neither: a name already there, the
/// draft's or the sidecar's, is refused rather than overwritten before
/// anything moves, and a sidecar whose move fails has the draft moved
/// back, the error naming both paths when even that fails.
pub fn retire_draft(path: &Path, attachment_dir: Option<&Path>) -> io::Result<PathBuf> {
    let sent = crate::submit::sent_dir_for(path).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "the draft is not inside a drafts directory",
        )
    })?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&sent)?;
    let metadata = fs::symlink_metadata(&sent)?;
    if !metadata.is_dir() || metadata.permissions().mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the sent directory must be a private directory, not a symlink",
        ));
    }
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "the draft path has no file name",
        )
    })?;
    let target = sent.join(name);
    if fs::symlink_metadata(&target).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} is already there", target.display()),
        ));
    }
    let sidecar = match attachment_dir.filter(|dir| fs::symlink_metadata(dir).is_ok()) {
        Some(dir) => {
            // Only the draft's own sidecar follows it: a directory
            // elsewhere is not moved into sent on a draft's account.
            if dir.parent() != path.parent() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "the attachment directory {} is not beside the draft",
                        dir.display()
                    ),
                ));
            }
            let dir_name = dir.file_name().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "the attachment directory has no name",
                )
            })?;
            let dir_target = sent.join(dir_name);
            if fs::symlink_metadata(&dir_target).is_ok() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} is already there", dir_target.display()),
                ));
            }
            Some((dir, dir_target))
        }
        None => None,
    };
    fs::rename(path, &target)?;
    if let Some((dir, dir_target)) = sidecar {
        if let Err(e) = fs::rename(dir, &dir_target) {
            let detail = match fs::rename(&target, path) {
                Ok(()) => format!(
                    "the attachments at {} could not move to {}: {e}; the draft is back at {}",
                    dir.display(),
                    dir_target.display(),
                    path.display()
                ),
                Err(back) => format!(
                    "the attachments at {} could not move to {}: {e}; the draft is at {} and could not move back: {back}",
                    dir.display(),
                    dir_target.display(),
                    target.display()
                ),
            };
            return Err(io::Error::new(e.kind(), detail));
        }
    }
    Ok(target)
}

fn draft_dir() -> io::Result<PathBuf> {
    draft_dir_from_env(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME"))
}

fn draft_dir_from_env(
    xdg_state_home: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> io::Result<PathBuf> {
    let state_dir = xdg_state_home
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| {
            home.map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .map(|p| p.join(".local/state"))
        })
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "draft retention requires an absolute XDG_STATE_HOME or HOME",
            )
        })?;
    Ok(state_dir.join("td-mail/drafts"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sent draft and its sidecar move together into the private `sent`
    /// directory beside `drafts`; a draft already there by that name is
    /// refused, not overwritten, and a draft without a sidecar moves alone.
    #[test]
    fn retire_draft_moves_the_draft_and_its_sidecar_beside_drafts() -> io::Result<()> {
        let root = std::env::temp_dir().join(format!(
            "td-mail-retire-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let drafts = root.join("td-mail/drafts");
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&drafts)?;
        let draft = drafts.join("td-mail-draft-1-2.eml");
        fs::write(&draft, "From: me@example.com\n")?;
        let sidecar = drafts.join("td-mail-att-1-2");
        fs::DirBuilder::new().mode(0o700).create(&sidecar)?;
        fs::write(sidecar.join("a.txt"), "a")?;

        let retired = retire_draft(&draft, Some(&sidecar))?;
        let sent = root.join("td-mail/sent");
        assert_eq!(retired, sent.join("td-mail-draft-1-2.eml"));
        assert_eq!(fs::read_to_string(&retired)?, "From: me@example.com\n");
        assert_eq!(fs::read_to_string(sent.join("td-mail-att-1-2/a.txt"))?, "a");
        assert!(!draft.exists() && !sidecar.exists());
        assert_eq!(
            fs::symlink_metadata(&sent)?.permissions().mode() & 0o777,
            0o700
        );

        // The same name again is refused; the new draft stays where it is.
        fs::write(&draft, "second")?;
        let err = retire_draft(&draft, None).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(&draft)?, "second");
        assert_eq!(fs::read_to_string(&retired)?, "From: me@example.com\n");
        // A sidecar's name already there refuses before the draft moves.
        let other = drafts.join("td-mail-draft-5-6.eml");
        fs::write(&other, "other")?;
        let other_sidecar = drafts.join("td-mail-att-5-6");
        fs::DirBuilder::new().mode(0o700).create(&other_sidecar)?;
        fs::DirBuilder::new()
            .mode(0o700)
            .create(sent.join("td-mail-att-5-6"))?;
        let err = retire_draft(&other, Some(&other_sidecar)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert!(other.exists() && other_sidecar.exists(), "neither moved");
        assert!(!sent.join("td-mail-draft-5-6.eml").exists());

        // A directory that is not beside the draft is not its sidecar:
        // refused, nothing moves.
        let elsewhere = root.join("elsewhere");
        fs::DirBuilder::new().mode(0o700).create(&elsewhere)?;
        let err = retire_draft(&other, Some(&elsewhere)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("is not beside the draft"), "{err}");
        assert!(other.exists() && elsewhere.exists(), "neither moved");

        // Without a sidecar, or with one that is not there, the draft
        // moves alone.
        let lone = drafts.join("td-mail-draft-3-4.eml");
        fs::write(&lone, "lone")?;
        let retired = retire_draft(&lone, Some(&drafts.join("td-mail-att-3-4")))?;
        assert_eq!(fs::read_to_string(retired)?, "lone");
        assert!(!lone.exists());

        fs::remove_dir_all(&root)
    }

    /// A draft is replaced whole: the bytes land under the path, a
    /// symlink at the path is replaced rather than followed, and a
    /// failure leaves the draft as it was with no sibling behind.
    #[test]
    fn replace_draft_is_whole_or_not_at_all() -> io::Result<()> {
        let root = std::env::temp_dir().join(format!(
            "td-mail-replace-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::DirBuilder::new().mode(0o700).create(&root)?;
        let path = root.join("draft.eml");
        fs::write(&path, b"old")?;
        replace_draft(&path, b"new")?;
        assert_eq!(fs::read(&path)?, b"new");
        assert_eq!(fs::read_dir(&root)?.count(), 1, "no sibling left");
        let target = root.join("target");
        fs::write(&target, b"target")?;
        fs::remove_file(&path)?;
        std::os::unix::fs::symlink(&target, &path)?;
        replace_draft(&path, b"over the link")?;
        assert!(
            fs::symlink_metadata(&path)?.is_file(),
            "the link is replaced"
        );
        assert_eq!(fs::read(&path)?, b"over the link");
        assert_eq!(fs::read(&target)?, b"target", "the target is not followed");
        assert!(replace_draft(&root.join("absent").join("draft.eml"), b"x").is_err());
        assert!(replace_draft(Path::new("draft.eml"), b"x").is_err());
        // A rename that fails after the sibling was written removes it.
        let occupied = root.join("occupied");
        fs::create_dir(&occupied)?;
        assert!(replace_draft(&occupied, b"x").is_err());
        assert_eq!(fs::read_dir(&root)?.count(), 3, "draft, target, occupied");
        assert_eq!(fs::read(&path)?, b"over the link");
        fs::remove_dir_all(&root)
    }

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

        // A name with a comma, a quote or a bracket is quoted in the
        // header, and the draft reads back as the addresses it was made
        // from, names included.
        let mut named = email;
        named.from = Some(vec![EmailAddress {
            name: Some("Doe, Jane \"JD\" <x>".to_string()),
            email: Some("jane@example.com".to_string()),
        }]);
        named.cc = Some(vec![EmailAddress {
            name: Some("Plain Name".to_string()),
            email: Some("plain@example.com".to_string()),
        }]);
        let draft = build_reply_draft(&named, true, "me@example.com");
        assert!(
            draft.contains("To: \"Doe, Jane \\\"JD\\\" <x>\" <jane@example.com>\n"),
            "{draft}"
        );
        assert!(
            draft.contains("Cc: Plain Name <plain@example.com>\n"),
            "{draft}"
        );
        let outgoing = crate::submit::parse_draft(&draft).unwrap();
        assert_eq!(outgoing.to, named.from.clone().unwrap());
        assert_eq!(outgoing.cc, named.cc.clone().unwrap());
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

        // A message that names a file to attach, or another header,
        // forwards as text: its MML tags are quoted `<#!`, which the
        // draft sends as the `<#` line it was, and its subject's newline
        // is a space on the one Subject line.
        let mut hostile = email;
        hostile.subject = Some("Hi\r\nBcc: spy@example.com".to_string());
        hostile.text_body = Some(vec![crate::jmap::types::BodyPart {
            part_id: "1".to_string(),
            blob_id: None,
            r#type: Some("text/plain".to_string()),
            name: None,
            size: None,
        }]);
        hostile.body_values.insert(
            "1".to_string(),
            crate::jmap::types::BodyValue {
                value: "see\n<#part type=\"text/plain\" filename=\"/etc/passwd\">\n<#/part>\n<#!x>"
                    .to_string(),
                is_encoding_problem: false,
                is_truncated: false,
            },
        );
        hostile.from = Some(vec![EmailAddress {
            name: Some("Sender\n<#part type=\"text/plain\" filename=\"/etc/shadow\">".to_string()),
            email: Some("sender@example.com".to_string()),
        }]);
        let draft = build_forward_draft(&hostile, "me@example.com");
        assert!(
            draft.contains("Subject: Fwd: Hi  Bcc: spy@example.com\n"),
            "{draft}"
        );
        assert!(
            draft.contains("Subject: Hi\r\nBcc"),
            "the body's copy is the text"
        );
        assert!(
            draft.contains("From: \"Sender <#part type=\\\"text/plain\\\" filename=\\\"/etc/shadow\\\">\" <sender@example.com>\n"),
            "the header block's name on one line: {draft}"
        );
        assert!(
            draft.contains(
                "\nsee\n<#!part type=\"text/plain\" filename=\"/etc/passwd\">\n<#!/part>\n<#!!x>\n"
            ),
            "{draft}"
        );
        let outgoing =
            crate::submit::parse_draft(&draft.replacen("To: \n", "To: you@example.com\n", 1))
                .unwrap();
        assert!(outgoing.parts.is_empty());
        assert!(outgoing.bcc.is_empty());
        assert_eq!(outgoing.subject, "Fwd: Hi  Bcc: spy@example.com");
        assert!(
            outgoing.text.ends_with(
                "see\n<#part type=\"text/plain\" filename=\"/etc/passwd\">\n<#/part>\n<#!x>\n"
            ),
            "{}",
            outgoing.text
        );
        // The subject's newline in the body's copy is a line of its own
        // there: what follows it is text, whatever it begins with.
        let mut tagged = hostile.clone();
        tagged.subject =
            Some("Hi\n<#part type=\"text/plain\" filename=\"/etc/passwd\">".to_string());
        tagged.from = Some(vec![EmailAddress {
            name: Some("S\n<#part type=\"text/plain\" filename=\"/etc/passwd\">".to_string()),
            email: Some("s@example.com".to_string()),
        }]);
        let draft = build_forward_draft(&tagged, "me@example.com");
        assert!(draft.contains("\nSubject: Hi\n<#!part type="), "{draft}");
        let outgoing =
            crate::submit::parse_draft(&draft.replacen("To: \n", "To: you@example.com\n", 1))
                .unwrap();
        assert!(outgoing.parts.is_empty(), "{draft}");
        let draft = build_reply_draft(&tagged, false, "me@example.com");
        assert!(draft.contains(" wrote:\n"), "{draft}");
        let outgoing = crate::submit::parse_draft(&draft).unwrap();
        assert!(outgoing.parts.is_empty(), "the wrote line's name: {draft}");
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
            None,
            Some("Forwarded: say \"hi\"\nthere"),
        )
        .unwrap();
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
    fn test_draft_dir_uses_persistent_state() {
        assert_eq!(
            draft_dir_from_env(Some("/tmp/state-test".into()), Some("/home/example".into()))
                .unwrap(),
            PathBuf::from("/tmp/state-test")
                .join("td-mail")
                .join("drafts")
        );
    }

    #[test]
    fn test_invalid_state_home_falls_back_but_never_to_runtime_or_cwd() {
        for state in [None, Some("".into()), Some("relative".into())] {
            assert_eq!(
                draft_dir_from_env(state.clone(), Some("/home/example".into())).unwrap(),
                PathBuf::from("/home/example/.local/state/td-mail/drafts")
            );
            assert!(draft_dir_from_env(state, Some("relative".into())).is_err());
        }
    }

    #[test]
    fn test_draft_dir_falls_back_to_home_state() {
        assert_eq!(
            draft_dir_from_env(None, Some("/home/example".into())).unwrap(),
            PathBuf::from("/home/example")
                .join(".local")
                .join("state")
                .join("td-mail")
                .join("drafts")
        );
    }

    #[test]
    fn state_paths_preserve_os_bytes_and_attachment_paths_refuse_lossy_mml() {
        use std::os::unix::ffi::OsStringExt;
        let raw = std::ffi::OsString::from_vec(b"/state-\xff".to_vec());
        assert_eq!(
            draft_dir_from_env(Some(raw.clone()), None).unwrap(),
            PathBuf::from(raw).join("td-mail/drafts")
        );
        let draft = ComposeDraft {
            body: String::new(),
            attachments: vec![DraftAttachment {
                filename: "forward.eml".into(),
                content_type: "message/rfc822".into(),
                description: None,
                data: Vec::new(),
            }],
        };
        for bytes in [
            b"/state-\xff".as_slice(),
            b"/state-\"",
            b"/state-\\",
            b"/state-\n",
            b"/state-\t",
            b"/state->",
            b"/state-<",
        ] {
            let path = PathBuf::from(std::ffi::OsString::from_vec(bytes.to_vec()));
            assert_eq!(
                write_compose_draft_in(&draft, &path).err().unwrap().kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }

    #[test]
    fn retained_drafts_have_private_files_and_stable_sidecars() -> io::Result<()> {
        let root = std::env::temp_dir().join(format!(
            "td-mail-retained-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let draft = ComposeDraft {
            body: "body\n".into(),
            attachments: vec![DraftAttachment {
                filename: "forward.eml".into(),
                content_type: "message/rfc822".into(),
                description: None,
                data: b"original bytes".to_vec(),
            }],
        };
        let first = write_compose_draft_in(&draft, &root)?;
        let second = write_compose_draft_in(&draft, &root)?;
        assert_ne!(first.draft_path, second.draft_path);
        let sidecar = first.attachment_dir.as_ref().unwrap();
        let attachment = sidecar.join("forward.eml");
        let body = fs::read_to_string(&first.draft_path)?;
        assert!(body.contains(attachment.to_str().unwrap()));
        assert_eq!(fs::read(&attachment)?, b"original bytes");
        for path in [&root, sidecar] {
            assert_eq!(fs::metadata(path)?.permissions().mode() & 0o777, 0o700);
        }
        for path in [&first.draft_path, &attachment] {
            assert_eq!(fs::metadata(path)?.permissions().mode() & 0o777, 0o600);
        }
        drop(first);
        drop(second);
        assert_eq!(fs::read(&attachment)?, b"original bytes");
        fs::remove_dir_all(root)
    }

    #[test]
    fn mml_attributes_refuse_hostile_paths_types_and_clean_descriptions() {
        for value in [
            "bad\"name",
            "bad\nname",
            "bad\rname",
            "bad\\name",
            "bad\tname",
            "bad>name",
            "bad<name",
        ] {
            assert!(mml_part("text/plain", Path::new(value), None, None).is_err());
            assert!(mml_part(value, Path::new("/safe/file"), None, None).is_err());
            assert!(mml_part("text/plain", Path::new("/safe/file"), Some(value), None).is_err());
        }
        let part = mml_part(
            "text/plain",
            Path::new("/safe/file"),
            None,
            Some("bad\\\"\n\r\tname"),
        )
        .unwrap();
        assert!(part.contains("description=\"bad/'   name\""));
        assert!(!part.contains(" name="));
        let part = mml_part("text/plain", Path::new("/safe/file-2"), Some("file"), None).unwrap();
        assert!(
            part.contains("filename=\"/safe/file-2\" disposition=\"attachment\" name=\"file\">")
        );
    }

    #[test]
    fn preparation_failure_removes_only_attempt_owned_sidecars() -> io::Result<()> {
        let root = std::env::temp_dir().join(format!(
            "td-mail-prepare-failure-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::DirBuilder::new().mode(0o700).create(&root)?;
        let attachment = |filename: &str| DraftAttachment {
            filename: filename.into(),
            content_type: "text/plain".into(),
            description: None,
            data: b"private attachment".to_vec(),
        };
        let mut draft = ComposeDraft {
            body: "body".into(),
            attachments: vec![attachment("a/b"), attachment("a_b")],
        };
        assert!(prepare_draft(&draft, &root, "duplicate").is_err());
        assert_eq!(fs::read_dir(&root)?.count(), 0);
        draft.attachments = vec![attachment("first"), attachment(&"x".repeat(256))];
        assert!(prepare_draft(&draft, &root, "partial").is_err());
        assert_eq!(fs::read_dir(&root)?.count(), 0);
        draft.attachments = vec![attachment("first")];
        let existing = root.join("td-mail-draft-collision.eml");
        fs::write(&existing, b"do not remove or replace")?;
        assert!(prepare_draft(&draft, &root, "collision").is_err());
        assert_eq!(fs::read(&existing)?, b"do not remove or replace");
        assert!(!root.join("td-mail-att-collision").exists());
        let sidecar = root.join("td-mail-att-existing");
        fs::create_dir(&sidecar)?;
        fs::write(sidecar.join("keep"), b"owned earlier")?;
        assert!(prepare_draft(&draft, &root, "existing").is_err());
        assert_eq!(fs::read(sidecar.join("keep"))?, b"owned earlier");
        fs::remove_dir_all(root)
    }

    #[test]
    fn private_directory_policy_does_not_chmod_or_follow_a_final_symlink() -> io::Result<()> {
        let root = std::env::temp_dir().join(format!(
            "td-mail-dir-policy-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root)?;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755))?;
        assert!(write_compose_draft_in(&ComposeDraft::text("x".into()), &root).is_err());
        assert_eq!(fs::metadata(&root)?.permissions().mode() & 0o777, 0o755);
        let link = root.join("link");
        std::os::unix::fs::symlink(&root, &link)?;
        assert!(write_compose_draft_in(&ComposeDraft::text("x".into()), &link).is_err());
        fs::remove_dir_all(root)
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

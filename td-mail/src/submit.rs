//! A retained draft read back for submission: the message-mode text
//! `compose` writes (headers, `--text follows this line--`, the body with
//! its MML `<#part>` tags) parsed into what `Email/set` takes, so the
//! server builds the message and td-mail encodes no MIME. Every refusal
//! names the line or the header it is about, since the draft is the
//! person's own text.

use std::borrow::Cow;
use std::fmt;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::civil;
use crate::jmap::types::{EmailAddress, Identity};
use crate::json::{Json, ObjectBuilder};

/// message-mode's line between the headers and the body.
pub const SEPARATOR: &str = "--text follows this line--";

/// Why a draft cannot be sent as it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftError(pub String);

impl fmt::Display for DraftError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DraftError {}

fn refuse<T>(message: impl Into<String>) -> Result<T, DraftError> {
    Err(DraftError(message.into()))
}

/// A file the draft attaches, as its `<#part>` tag names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    pub path: PathBuf,
    pub content_type: String,
    /// The name the recipient sees: the tag's, else the file's.
    pub name: String,
    pub description: Option<String>,
}

/// The draft as a message to submit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outgoing {
    pub from: EmailAddress,
    pub to: Vec<EmailAddress>,
    pub cc: Vec<EmailAddress>,
    pub bcc: Vec<EmailAddress>,
    pub reply_to: Vec<EmailAddress>,
    pub subject: String,
    /// Message ids without their angle brackets, as JMAP carries them.
    pub in_reply_to: Vec<String>,
    pub references: Vec<String>,
    /// The text body, the lines between the tags in order.
    pub text: String,
    pub parts: Vec<Part>,
}

/// Parses the draft. The headers end at the separator line, or at the
/// first empty line when the draft has none (a plain message pasted in);
/// a header may fold over continuation lines. `From` is one address and
/// `To`, `Cc` and `Bcc` at least one between them; `Reply-To`, `Subject`,
/// `In-Reply-To` and `References` are read and any other header is
/// refused, named, since nothing else would be sent. In the body a line
/// that is an MML `<#part ...>` tag names a file to attach and `<#/part>`
/// closes it; any other MML tag is refused; and a line quoted as `<#!`,
/// the way forwarded text is written, is sent as text with one `!`
/// fewer, as Emacs sends it.
pub fn parse_draft(text: &str) -> Result<Outgoing, DraftError> {
    let lines: Vec<&str> = text
        .lines()
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();
    let separator = lines.iter().position(|line| line.trim_end() == SEPARATOR);
    let (header_lines, body_lines): (&[&str], &[&str]) = match separator {
        Some(at) => (
            lines.get(..at).unwrap_or(&[]),
            lines.get(at + 1..).unwrap_or(&[]),
        ),
        None => {
            let blank = lines
                .iter()
                .position(|line| line.trim().is_empty())
                .unwrap_or(lines.len());
            (
                lines.get(..blank).unwrap_or(&[]),
                lines.get(blank + 1..).unwrap_or(&[]),
            )
        }
    };

    let mut from = Vec::new();
    let mut to = Vec::new();
    let mut cc = Vec::new();
    let mut bcc = Vec::new();
    let mut reply_to = Vec::new();
    let mut subject = None;
    let mut in_reply_to = Vec::new();
    let mut references = Vec::new();
    for (name, value, line) in unfold_headers(header_lines)? {
        let key = name.to_ascii_lowercase();
        match key.as_str() {
            "from" => from.extend(parse_addresses(&name, &value)?),
            "to" => to.extend(parse_addresses(&name, &value)?),
            "cc" => cc.extend(parse_addresses(&name, &value)?),
            "bcc" => bcc.extend(parse_addresses(&name, &value)?),
            "reply-to" => reply_to.extend(parse_addresses(&name, &value)?),
            "subject" => {
                if subject.replace(value.trim().to_string()).is_some() {
                    return refuse("the draft has two Subject headers");
                }
            }
            "in-reply-to" => in_reply_to.extend(parse_message_ids(&value)),
            "references" => references.extend(parse_message_ids(&value)),
            _ => {
                return refuse(format!(
                    "line {line}: the header {name} is not one td-mail sends; \
                     From, To, Cc, Bcc, Reply-To, Subject, In-Reply-To and References are"
                ))
            }
        }
    }
    let from = match from.as_slice() {
        [one] => one.clone(),
        [] => return refuse("the draft has no From address"),
        _ => return refuse("the draft's From names more than one address"),
    };
    if to.is_empty() && cc.is_empty() && bcc.is_empty() {
        return refuse("the draft has no recipient: To, Cc and Bcc are all empty");
    }

    let mut text_lines: Vec<Cow<'_, str>> = Vec::with_capacity(body_lines.len());
    let mut parts = Vec::new();
    // The body begins after the separator, or after the blank line.
    let offset = header_lines.len() + 1;
    for (index, line) in body_lines.iter().enumerate() {
        let number = offset + index + 1;
        let trimmed = line.trim_end();
        if trimmed == "<#/part>" {
            continue;
        }
        if let Some(quoted) = line.strip_prefix("<#!") {
            text_lines.push(Cow::Owned(format!("<#{quoted}")));
            continue;
        }
        if let Some(rest) = trimmed
            .strip_prefix("<#part")
            .filter(|rest| rest.starts_with([' ', '\t', '>']))
        {
            parts.push(parse_part(number, rest)?);
            continue;
        }
        if trimmed.starts_with("<#") {
            return refuse(format!(
                "line {number}: only an MML <#part> tag can be sent, not {}",
                trimmed.split_whitespace().next().unwrap_or(trimmed)
            ));
        }
        text_lines.push(Cow::Borrowed(line));
    }
    let leading = text_lines
        .iter()
        .take_while(|line| line.trim().is_empty())
        .count();
    text_lines.drain(..leading);
    while text_lines.last().is_some_and(|line| line.trim().is_empty()) {
        text_lines.pop();
    }
    let mut text = text_lines.join("\n");
    if !text.is_empty() {
        text.push('\n');
    }

    Ok(Outgoing {
        from,
        to,
        cc,
        bcc,
        reply_to,
        subject: subject.unwrap_or_default(),
        in_reply_to,
        references,
        text,
        parts,
    })
}

/// The headers as (name, value, line number) with folded lines joined.
fn unfold_headers(lines: &[&str]) -> Result<Vec<(String, String, usize)>, DraftError> {
    let mut headers: Vec<(String, String, usize)> = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let number = index + 1;
        if line.trim().is_empty() {
            if headers.is_empty() {
                continue;
            }
            return refuse(format!(
                "line {number}: an empty line inside the headers; the body begins after `{SEPARATOR}`"
            ));
        }
        if line.starts_with([' ', '\t']) {
            match headers.last_mut() {
                Some((_, value, _)) => {
                    value.push(' ');
                    value.push_str(line.trim());
                }
                None => {
                    return refuse(format!(
                        "line {number}: a continuation line before any header"
                    ))
                }
            }
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            return refuse(format!("line {number}: not a header: {}", line.trim()));
        };
        let name = name.trim();
        if name.is_empty() || name.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return refuse(format!("line {number}: not a header: {}", line.trim()));
        }
        headers.push((name.to_string(), value.trim().to_string(), number));
    }
    Ok(headers)
}

/// A header's addresses: `Name <box@host>`, `"Quoted, Name" <box@host>`,
/// `<box@host>` or `box@host`, separated by commas outside quotes and
/// angle brackets; an empty value is no address.
fn parse_addresses(header: &str, value: &str) -> Result<Vec<EmailAddress>, DraftError> {
    let mut out = Vec::new();
    for item in split_addresses(value) {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let (name, email) = match (item.rfind('<'), item.ends_with('>')) {
            (Some(open), true) => {
                let email = item.get(open + 1..item.len() - 1).unwrap_or("").trim();
                let name = unquote(item.get(..open).unwrap_or("").trim());
                (if name.is_empty() { None } else { Some(name) }, email)
            }
            _ => (None, item),
        };
        if !plausible_address(email) {
            return refuse(format!("{header}: `{item}` is not an address"));
        }
        out.push(EmailAddress {
            name,
            email: Some(email.to_string()),
        });
    }
    Ok(out)
}

fn split_addresses(value: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut angled = false;
    let mut escaped = false;
    for c in value.chars() {
        if escaped {
            current.push(c);
            escaped = false;
            continue;
        }
        match c {
            '\\' if quoted => {
                current.push(c);
                escaped = true;
            }
            '"' => {
                quoted = !quoted;
                current.push(c);
            }
            '<' if !quoted => {
                angled = true;
                current.push(c);
            }
            '>' if !quoted => {
                angled = false;
                current.push(c);
            }
            ',' if !quoted && !angled => items.push(std::mem::take(&mut current)),
            _ => current.push(c),
        }
    }
    items.push(current);
    items
}

/// A quoted display name without its quotes and escapes.
fn unquote(name: &str) -> String {
    let Some(inner) = name
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
    else {
        return name.to_string();
    };
    let mut out = String::with_capacity(inner.len());
    let mut escaped = false;
    for c in inner.chars() {
        if escaped {
            out.push(c);
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else {
            out.push(c);
        }
    }
    out
}

/// One `@` with something on each side and nothing a header would
/// misread; the server judges the rest.
fn plausible_address(email: &str) -> bool {
    let Some((local, domain)) = email.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !domain.is_empty()
        && !domain.contains('@')
        && !email.chars().any(|c| {
            c.is_whitespace() || c.is_control() || matches!(c, '<' | '>' | '"' | ',' | '(' | ')')
        })
}

/// `<id@host>` tokens, brackets off, separated by whitespace or commas.
fn parse_message_ids(value: &str) -> Vec<String> {
    value
        .split(|c: char| c.is_whitespace() || c == ',')
        .map(|token| token.trim_matches(|c| c == '<' || c == '>'))
        .filter(|token| !token.is_empty())
        .map(str::to_string)
        .collect()
}

/// The attributes of a `<#part ...>` tag after its name: `key="value"`
/// or `key=value` pairs up to the closing `>`.
fn parse_part(number: usize, rest: &str) -> Result<Part, DraftError> {
    let Some(body) = rest.trim_end().strip_suffix('>') else {
        return refuse(format!("line {number}: the <#part tag does not end with >"));
    };
    let mut path = None;
    let mut content_type = None;
    let mut name = None;
    let mut description = None;
    for (key, value) in tag_attributes(number, body)? {
        match key.as_str() {
            "filename" => path = Some(PathBuf::from(value)),
            "type" => content_type = Some(value),
            "name" => name = Some(value),
            "description" => description = Some(value),
            "disposition" | "encoding" | "charset" => {}
            _ => {
                return refuse(format!(
                    "line {number}: the <#part tag's {key} is not one td-mail sends"
                ))
            }
        }
    }
    let Some(path) = path else {
        return refuse(format!("line {number}: the <#part tag names no filename"));
    };
    if !path.is_absolute() {
        return refuse(format!(
            "line {number}: the <#part tag's filename must be an absolute path: {}",
            path.display()
        ));
    }
    let Some(content_type) = content_type.filter(|t| !t.trim().is_empty()) else {
        return refuse(format!("line {number}: the <#part tag names no type"));
    };
    let is_token = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c))
    };
    if !content_type
        .split_once('/')
        .is_some_and(|(kind, subtype)| is_token(kind) && is_token(subtype))
    {
        return refuse(format!(
            "line {number}: the <#part tag's type is not a media type: {content_type}"
        ));
    }
    let name = name
        .filter(|n| !n.trim().is_empty())
        .or_else(|| path.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "attachment".to_string());
    Ok(Part {
        path,
        content_type,
        name,
        description: description.filter(|d| !d.trim().is_empty()),
    })
}

fn tag_attributes(number: usize, body: &str) -> Result<Vec<(String, String)>, DraftError> {
    let mut out = Vec::new();
    let mut rest = body.trim_start();
    while !rest.is_empty() {
        let Some(eq) = rest.find('=') else {
            return refuse(format!(
                "line {number}: the <#part tag has a word without =: {rest}"
            ));
        };
        let key = rest.get(..eq).unwrap_or("").trim().to_ascii_lowercase();
        if key.is_empty() || key.chars().any(|c| !c.is_ascii_alphanumeric() && c != '-') {
            return refuse(format!(
                "line {number}: the <#part tag has a malformed attribute: {rest}"
            ));
        }
        let after = rest.get(eq + 1..).unwrap_or("").trim_start();
        let (value, remainder) = if let Some(quoted) = after.strip_prefix('"') {
            let Some(close) = quoted.find('"') else {
                return refuse(format!(
                    "line {number}: the <#part tag's {key} is not closed"
                ));
            };
            (
                quoted.get(..close).unwrap_or("").to_string(),
                quoted.get(close + 1..).unwrap_or(""),
            )
        } else {
            let end = after.find(char::is_whitespace).unwrap_or(after.len());
            (
                after.get(..end).unwrap_or("").to_string(),
                after.get(end..).unwrap_or(""),
            )
        };
        out.push((key, value));
        rest = remainder.trim_start();
    }
    Ok(out)
}

/// Linux `O_NONBLOCK`: opening a pipe put where a file was does not wait
/// for a writer, and a regular file reads as ever.
const O_NONBLOCK: i32 = 0o4000;

/// The draft's attachment files read, each with its bytes, refusing one
/// that is not a regular file (a device or a pipe has no end to read to)
/// or past `ceiling` bytes, before any is uploaded. The file opened is
/// the one measured and read, and the read stops a byte past the
/// ceiling, so a file swapped or grown under the check is refused, not
/// read whole.
pub fn read_parts(parts: &[Part], ceiling: u64) -> Result<Vec<(usize, Vec<u8>)>, DraftError> {
    let mut out = Vec::with_capacity(parts.len());
    for (index, part) in parts.iter().enumerate() {
        let attachment =
            |e: std::io::Error| DraftError(format!("attachment {}: {e}", part.path.display()));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(O_NONBLOCK)
            .open(&part.path)
            .map_err(attachment)?;
        let metadata = file.metadata().map_err(attachment)?;
        if !metadata.is_file() {
            return refuse(format!(
                "attachment {} is not a regular file",
                part.path.display()
            ));
        }
        let size = metadata.len();
        if size > ceiling {
            return refuse(format!(
                "attachment {} is {size} bytes, past the {ceiling} the server or the fetch service takes",
                part.path.display()
            ));
        }
        let mut bytes = Vec::new();
        file.take(ceiling.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(attachment)?;
        if bytes.len() as u64 > ceiling {
            return refuse(format!(
                "attachment {} grew past the {ceiling} bytes the server or the fetch service takes",
                part.path.display()
            ));
        }
        out.push((index, bytes));
    }
    Ok(out)
}

/// Where a sent draft is kept: the `sent` directory beside the draft's
/// own, `.../td-mail/sent` for `.../td-mail/drafts`; none for a draft
/// whose directory is not named `drafts`.
pub fn sent_dir_for(draft: &Path) -> Option<PathBuf> {
    let drafts = draft.parent()?;
    if drafts.file_name()? != "drafts" {
        return None;
    }
    Some(drafts.parent()?.join("sent"))
}

/// A Unix time as JMAP's `UTCDate`, RFC 3339 in UTC to the second.
pub fn rfc3339_utc(unix: i64) -> String {
    let c = civil::unix_to_civil_utc(unix);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        c.year, c.month, c.day, c.hour, c.minute, c.second
    )
}

/// A Message-ID for one send, without its angle brackets as JMAP carries
/// it: the time in nanoseconds, the process and a count within it, so
/// two sends in one clock tick differ, at the From's domain. It is made
/// here so the sent copy and any reply can name it, and so a send whose
/// answer was lost can be asked about.
pub fn new_message_id(from: &EmailAddress) -> String {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let domain = from
        .email
        .as_deref()
        .and_then(|email| email.rsplit_once('@'))
        .map_or("td-mail.invalid", |(_, domain)| domain);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("td-mail.{nanos}.{}.{sequence}@{domain}", std::process::id())
}

/// The `Email/set` create object for `outgoing`, its attachments the
/// uploaded `blob_ids` in the parts' order: the server assembles the
/// message from these, so no MIME is written here. The From name is the
/// draft's, else the identity's; the Date is `now` and the Message-ID
/// `message_id`, the caller's, since it is what a lost answer is asked
/// about.
pub fn email_json(
    outgoing: &Outgoing,
    identity: &Identity,
    blob_ids: &[String],
    now: i64,
    message_id: &str,
) -> Json {
    let mut from = outgoing.from.clone();
    if from.name.as_deref().is_none_or(str::is_empty) && !identity.name.is_empty() {
        from.name = Some(identity.name.clone());
    }
    let sent_at = rfc3339_utc(now);
    let mut email = ObjectBuilder::new()
        .set("from", &vec![from])
        .set("subject", &outgoing.subject)
        .set("sentAt", &sent_at)
        .set("messageId", vec![message_id.to_string()]);
    for (key, addresses) in [
        ("to", &outgoing.to),
        ("cc", &outgoing.cc),
        ("bcc", &outgoing.bcc),
        ("replyTo", &outgoing.reply_to),
    ] {
        if !addresses.is_empty() {
            email = email.set(key, addresses);
        }
    }
    if !outgoing.in_reply_to.is_empty() {
        email = email.set("inReplyTo", &outgoing.in_reply_to);
    }
    if !outgoing.references.is_empty() {
        email = email.set("references", &outgoing.references);
    }
    email = email
        .set(
            "textBody",
            Json::Arr(vec![json!({ "partId": "text", "type": "text/plain" })]),
        )
        .set(
            "bodyValues",
            json!({
                "text": {
                    "value": outgoing.text,
                    "isEncodingProblem": false,
                    "isTruncated": false
                }
            }),
        );
    let attachments: Vec<Json> = outgoing
        .parts
        .iter()
        .zip(blob_ids)
        .map(|(part, blob_id)| {
            let mut attachment = ObjectBuilder::new()
                .set("blobId", blob_id)
                .set("type", &part.content_type)
                .set("name", &part.name)
                .set("disposition", "attachment");
            if let Some(description) = &part.description {
                attachment = attachment.set("header:Content-Description:asText", description);
            }
            attachment.build()
        })
        .collect();
    if !attachments.is_empty() {
        email = email.set("attachments", Json::Arr(attachments));
    }
    email.build()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(name: Option<&str>, email: &str) -> EmailAddress {
        EmailAddress {
            name: name.map(str::to_string),
            email: Some(email.to_string()),
        }
    }

    fn field<'a>(value: &'a Json, path: &[&str]) -> Option<&'a str> {
        value.get_path(path).and_then(|v| v.as_str())
    }

    /// The draft `compose` writes for a forward with the original attached
    /// parses whole: the headers, the text between the tags, and the tag
    /// as a part, its closing tag and the blank lines around it dropped.
    #[test]
    fn a_forward_draft_with_its_attachment_parses_whole() {
        let draft = "From: Me <me@example.com>\nTo: you@example.com, \"Doe, Jane\" <jane@example.com>\nCc: \nSubject: Fwd: Hello\n--text follows this line--\n\nSee attached.\n\n<#part type=\"message/rfc822\" filename=\"/state/td-mail/drafts/td-mail-att-1/forwarded.eml\" disposition=\"attachment\" description=\"Forwarded: Hello\">\n<#/part>\n";
        let outgoing = parse_draft(draft).unwrap();
        assert_eq!(outgoing.from, addr(Some("Me"), "me@example.com"));
        assert_eq!(
            outgoing.to,
            vec![
                addr(None, "you@example.com"),
                addr(Some("Doe, Jane"), "jane@example.com")
            ]
        );
        assert!(outgoing.cc.is_empty());
        assert_eq!(outgoing.subject, "Fwd: Hello");
        assert_eq!(outgoing.text, "See attached.\n");
        assert_eq!(
            outgoing.parts,
            vec![Part {
                path: PathBuf::from("/state/td-mail/drafts/td-mail-att-1/forwarded.eml"),
                content_type: "message/rfc822".to_string(),
                name: "forwarded.eml".to_string(),
                description: Some("Forwarded: Hello".to_string()),
            }]
        );
    }

    /// A reply's threading headers come through without their brackets,
    /// a folded header is one value, and a message pasted in without the
    /// separator ends its headers at the first empty line, its quoted
    /// lines kept as written.
    #[test]
    fn reply_headers_folding_and_a_plain_message_parse() {
        let draft = "From: me@example.com\nTo: a@example.com,\n b@example.com\nSubject: Re: Hello\nIn-Reply-To: <abc@example.com>\nReferences: <first@example.com> <abc@example.com>\n--text follows this line--\n\nOn Monday, A wrote:\n> hi\n\nhello\n\n\n";
        let outgoing = parse_draft(draft).unwrap();
        assert_eq!(
            outgoing.to,
            vec![addr(None, "a@example.com"), addr(None, "b@example.com")]
        );
        assert_eq!(outgoing.in_reply_to, vec!["abc@example.com"]);
        assert_eq!(
            outgoing.references,
            vec!["first@example.com", "abc@example.com"]
        );
        assert_eq!(outgoing.text, "On Monday, A wrote:\n> hi\n\nhello\n");

        let plain =
            "From: me@example.com\r\nBcc: <c@example.com>\r\nReply-To: Me <r@example.com>\r\nSubject: Plain\r\n\r\nBody line.\r\n";
        let outgoing = parse_draft(plain).unwrap();
        assert!(outgoing.to.is_empty());
        assert_eq!(outgoing.bcc, vec![addr(None, "c@example.com")]);
        assert_eq!(outgoing.reply_to, vec![addr(Some("Me"), "r@example.com")]);
        assert_eq!(outgoing.subject, "Plain");
        assert_eq!(outgoing.text, "Body line.\n");
        assert!(outgoing.parts.is_empty());
    }

    /// A quoted tag is text: `<#!part` is sent as `<#part`, one `!`
    /// fewer, so forwarded text that named a file attaches nothing.
    #[test]
    fn a_quoted_mml_tag_is_sent_as_text() {
        let draft = "From: me@example.com\nTo: you@example.com\n--text follows this line--\nsee:\n<#!part type=\"text/plain\" filename=\"/etc/passwd\">\n<#!!multipart>\n<#!/part>\n";
        let outgoing = parse_draft(draft).unwrap();
        assert!(outgoing.parts.is_empty());
        assert_eq!(
            outgoing.text,
            "see:\n<#part type=\"text/plain\" filename=\"/etc/passwd\">\n<#!multipart>\n<#/part>\n"
        );
    }

    /// Each refusal names what it is about: the header that is not sent,
    /// the missing From or recipient, the address that is not one, the
    /// MML tag that is not a part, and a part without its file or type.
    #[test]
    fn refusals_name_the_header_line_or_tag() {
        let refused = |draft: &str| parse_draft(draft).unwrap_err().0;
        let head = "From: me@example.com\nTo: you@example.com\n";
        let sep = "--text follows this line--\n";
        assert!(refused(&format!("{head}Fcc: ~/sent\n{sep}"))
            .starts_with("line 3: the header Fcc is not one td-mail sends"));
        assert_eq!(
            refused(&format!("To: you@example.com\n{sep}hi\n")),
            "the draft has no From address"
        );
        assert_eq!(
            refused(&format!("From: me@example.com\nTo: \nCc: \n{sep}hi\n")),
            "the draft has no recipient: To, Cc and Bcc are all empty"
        );
        assert_eq!(
            refused(&format!("From: me@example.com\nTo: you at example\n{sep}")),
            "To: `you at example` is not an address"
        );
        assert_eq!(
            refused(&format!("{head}From: other@example.com\n{sep}")),
            "the draft's From names more than one address"
        );
        assert_eq!(
            refused(&format!("{head}{sep}<#multipart type=\"mixed\">\n")),
            "line 4: only an MML <#part> tag can be sent, not <#multipart"
        );
        assert_eq!(
            refused(&format!("{head}{sep}<#party>\n")),
            "line 4: only an MML <#part> tag can be sent, not <#party>"
        );
        // Without the separator the body begins after the blank line.
        assert_eq!(
            refused(&format!("{head}\n<#multipart type=\"mixed\">\n")),
            "line 4: only an MML <#part> tag can be sent, not <#multipart"
        );
        assert_eq!(
            refused(&format!("{head}{sep}<#part type=\"text/plain\">\n")),
            "line 4: the <#part tag names no filename"
        );
        assert_eq!(
            refused(&format!("{head}{sep}<#part filename=\"/a/b\">\n")),
            "line 4: the <#part tag names no type"
        );
        // A media type is a token, a slash and a token: nothing less.
        for bad in [
            "/",
            "text/",
            "/plain",
            "text",
            "text/plain;x=y",
            "te xt/plain",
        ] {
            assert_eq!(
                refused(&format!(
                    "{head}{sep}<#part type=\"{bad}\" filename=\"/a/b\">\n"
                )),
                format!("line 4: the <#part tag's type is not a media type: {bad}")
            );
        }
        assert_eq!(
            refused(&format!(
                "{head}{sep}<#part type=\"text/plain\" filename=\"b\">\n"
            )),
            "line 4: the <#part tag's filename must be an absolute path: b"
        );
        assert_eq!(
            refused(&format!(
                "{head}{sep}<#part type=\"text/plain\" filename=\"/a/b\" raw=t>\n"
            )),
            "line 4: the <#part tag's raw is not one td-mail sends"
        );
        assert_eq!(
            refused(&format!("From: me@example.com\n\nTo: you@example.com\n{sep}")),
            "line 2: an empty line inside the headers; the body begins after `--text follows this line--`"
        );
        assert_eq!(
            refused(&format!("{head}not a header\n{sep}")),
            "line 3: not a header: not a header"
        );
    }

    /// `Email/set`'s object: the From name falls back to the identity's,
    /// empty recipient lists are left out, the text is one body part, the
    /// attachments carry their blob ids in the parts' order with a
    /// description as the part's Content-Description header, the date is
    /// `now` in RFC 3339 and the Message-ID is at the From's domain.
    #[test]
    fn the_email_object_is_what_the_server_assembles_from() {
        let mut outgoing = parse_draft(
            "From: me@example.com\nTo: you@example.com\nSubject: S\n--text follows this line--\nhi\n<#part type=\"text/plain\" filename=\"/tmp/a.txt\">\n<#part type=\"image/png\" filename=\"/tmp/b.png\" description=\"A picture\">\n",
        )
        .unwrap();
        let identity = Identity {
            id: "ident".to_string(),
            name: "Me Myself".to_string(),
            email: "me@example.com".to_string(),
        };
        let message_id_given = new_message_id(&outgoing.from);
        assert!(
            message_id_given.starts_with("td-mail.") && message_id_given.ends_with("@example.com"),
            "{message_id_given}"
        );
        assert!(!message_id_given.contains('<'));
        assert_ne!(
            message_id_given,
            new_message_id(&outgoing.from),
            "one per send"
        );
        let email = email_json(
            &outgoing,
            &identity,
            &["blob-a".to_string(), "blob-b".to_string()],
            1_735_689_600,
            &message_id_given,
        );
        let from = email.get("from").and_then(|f| f.as_array()).unwrap();
        assert_eq!(from.len(), 1);
        assert_eq!(field(&from[0], &["name"]), Some("Me Myself"));
        assert_eq!(field(&from[0], &["email"]), Some("me@example.com"));
        assert!(email.get("cc").is_none());
        assert!(email.get("bcc").is_none());
        assert!(email.get("replyTo").is_none());
        assert!(email.get("inReplyTo").is_none());
        assert_eq!(field(&email, &["subject"]), Some("S"));
        assert_eq!(field(&email, &["sentAt"]), Some("2025-01-01T00:00:00Z"));
        let message_id = email
            .get("messageId")
            .and_then(|m| m.as_array())
            .and_then(|m| m.first())
            .and_then(|m| m.as_str())
            .unwrap();
        assert_eq!(
            message_id, message_id_given,
            "the caller's, for a lost answer"
        );
        assert_eq!(
            field(&email, &["bodyValues", "text", "value"]),
            Some("hi\n")
        );
        let text_body = email.get("textBody").and_then(|t| t.as_array()).unwrap();
        assert_eq!(text_body.len(), 1);
        assert_eq!(field(&text_body[0], &["partId"]), Some("text"));
        assert_eq!(field(&text_body[0], &["type"]), Some("text/plain"));
        let attachments = email.get("attachments").and_then(|a| a.as_array()).unwrap();
        assert_eq!(attachments.len(), 2);
        assert_eq!(field(&attachments[0], &["blobId"]), Some("blob-a"));
        assert_eq!(field(&attachments[0], &["name"]), Some("a.txt"));
        assert!(attachments[0]
            .get("header:Content-Description:asText")
            .is_none());
        assert_eq!(field(&attachments[1], &["blobId"]), Some("blob-b"));
        assert_eq!(field(&attachments[1], &["type"]), Some("image/png"));
        assert_eq!(
            field(&attachments[1], &["header:Content-Description:asText"]),
            Some("A picture")
        );
        assert_eq!(field(&attachments[1], &["disposition"]), Some("attachment"));

        // A named From keeps its name; no parts, no attachments key.
        outgoing.from.name = Some("Named".to_string());
        outgoing.parts.clear();
        let email = email_json(&outgoing, &identity, &[], 0, "x@example.com");
        let from = email.get("from").and_then(|f| f.as_array()).unwrap();
        assert_eq!(field(&from[0], &["name"]), Some("Named"));
        assert!(email.get("attachments").is_none());
    }

    /// The parts are read before any is uploaded, and one past the ceiling
    /// or missing refuses the whole send by name.
    #[test]
    fn parts_are_read_whole_and_a_large_missing_or_special_one_refuses() {
        let dir = std::env::temp_dir().join(format!(
            "td-mail-parts-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let small = dir.join("small.txt");
        std::fs::write(&small, b"hello").unwrap();
        let part = |path: &Path| Part {
            path: path.to_path_buf(),
            content_type: "text/plain".to_string(),
            name: "x".to_string(),
            description: None,
        };
        let read = read_parts(&[part(&small)], 5).unwrap();
        assert_eq!(read, vec![(0, b"hello".to_vec())]);
        let err = read_parts(&[part(&small)], 4).unwrap_err().0;
        assert!(err.contains("is 5 bytes, past the 4"), "{err}");
        let err = read_parts(&[part(&dir.join("missing"))], 4).unwrap_err().0;
        assert!(
            err.starts_with("attachment ") && err.contains("missing"),
            "{err}"
        );
        // A directory or a device is not read: only a regular file is.
        let err = read_parts(&[part(&dir)], 4).unwrap_err().0;
        assert!(err.ends_with("is not a regular file"), "{err}");
        let err = read_parts(&[part(Path::new("/dev/null"))], 4)
            .unwrap_err()
            .0;
        assert_eq!(err, "attachment /dev/null is not a regular file");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_sent_directory_is_beside_the_drafts_directory() {
        assert_eq!(
            sent_dir_for(Path::new("/s/td-mail/drafts/td-mail-draft-1.eml")),
            Some(PathBuf::from("/s/td-mail/sent"))
        );
        assert_eq!(sent_dir_for(Path::new("draft.eml")), None);
        assert_eq!(sent_dir_for(Path::new("/tmp/x/draft.eml")), None);
        assert_eq!(
            sent_dir_for(Path::new("/drafts/draft.eml")),
            Some(PathBuf::from("/sent"))
        );
    }
}

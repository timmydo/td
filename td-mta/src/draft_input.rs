//! Borrowed decoding of td-mail's plain-text Email/set create object.
use crate::{
    admission::work::{Charge, Meter, Stop},
    compose_text::{Address, Message},
    ids::{BlobId, MailboxId},
    ports::{Clock, Deadline, Tick},
    wire,
};
use td_json::arena::Value;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Property(&'static str),
    Capacity,
    Invalid,
    Deadline,
    Clock,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "draft input: {self:?}")
    }
}
impl std::error::Error for Error {}

/// Text borrows its already parsed request; vectors hold only bounded address
/// and identifier descriptors. This is passive input, never write authority.
pub struct Draft<'a> {
    pub mailbox: MailboxId,
    pub attachments: Vec<Attachment<'a>>,
    from: Address<'a>,
    to: Vec<Address<'a>>,
    cc: Vec<Address<'a>>,
    bcc: Vec<Address<'a>>,
    reply_to: Vec<Address<'a>>,
    subject: &'a str,
    message_id: &'a str,
    in_reply_to: Vec<&'a str>,
    references: Vec<&'a str>,
    sent_at_ms: i64,
    text: &'a str,
}
pub struct Attachment<'a> {
    pub blob: BlobId,
    pub name: &'a str,
    pub content_type: &'a str,
    pub description: Option<&'a str>,
}
impl Draft<'_> {
    pub fn message(&self) -> Message<'_> {
        Message {
            from: self.from,
            to: &self.to,
            cc: &self.cc,
            bcc: &self.bcc,
            reply_to: &self.reply_to,
            subject: self.subject,
            message_id: self.message_id,
            in_reply_to: &self.in_reply_to,
            references: &self.references,
            sent_at_ms: self.sent_at_ms,
            text: self.text,
        }
    }
}
struct Work<'a> {
    clock: &'a dyn Clock,
    deadline: Deadline,
    last: Tick,
    meter: &'a mut Meter,
    text_left: usize,
}
impl Work<'_> {
    fn charge(&mut self, bytes: usize) -> Result<(), Error> {
        let now = self.clock.sample().map_err(|_| Error::Clock)?.monotonic;
        if now < self.last {
            return Err(Error::Clock);
        }
        self.last = now;
        if self.deadline.expired(now) {
            return Err(Error::Deadline);
        }
        self.meter
            .charge(
                now,
                Charge {
                    io_bytes: bytes as u64,
                    records: 1,
                    ..Charge::default()
                },
            )
            .map_err(|stop| {
                if stop == Stop::Deadline {
                    Error::Deadline
                } else {
                    Error::Capacity
                }
            })
    }
    fn string<'a>(&mut self, value: Value<'a>, field: &'static str) -> Result<&'a str, Error> {
        let bytes = value.as_string_bytes().ok_or(Error::Property(field))?;
        self.text_left = self
            .text_left
            .checked_sub(bytes.len())
            .ok_or(Error::Capacity)?;
        self.charge(bytes.len())?;
        let text = std::str::from_utf8(bytes).map_err(|_| Error::Property(field))?;
        self.charge(0)?;
        Ok(text)
    }
    fn attachments<'a>(&mut self, value: Value<'a>) -> Result<Vec<Attachment<'a>>, Error> {
        let parts = value
            .as_array()
            .filter(|v| v.len() <= crate::compose_text::MAX_ATTACHMENTS)
            .ok_or(Error::Property("attachments"))?;
        let mut result = Vec::new();
        result
            .try_reserve_exact(parts.len())
            .map_err(|_| Error::Capacity)?;
        for part in parts {
            self.charge(0)?;
            let mut fields = part.as_object().ok_or(Error::Property("attachments"))?;
            let (mut blob, mut name, mut kind, mut disposition, mut description) =
                (None, None, None, false, None);
            while let Some((key, value)) = fields.next_bytes() {
                let text = self.string(value, "attachments")?;
                match key {
                    b"blobId" => blob = wire::Id::parse(text).ok().and_then(|v| v.local()),
                    b"name" => name = Some(text),
                    b"type" => kind = Some(text),
                    b"disposition" => disposition = text == "attachment",
                    b"header:Content-Description:asText"
                        if text.len() <= 1024 && !text.chars().any(char::is_control) =>
                    {
                        description = Some(text)
                    }
                    _ => return Err(Error::Property("attachments")),
                }
            }
            let name = name
                .filter(|s| !s.is_empty() && s.len() <= 255 && !s.chars().any(char::is_control))
                .ok_or(Error::Property("attachments"))?;
            let content_type = kind
                .filter(|s| crate::compose_text::attachment_type(s))
                .ok_or(Error::Property("attachments"))?;
            if !disposition {
                return Err(Error::Property("attachments"));
            }
            result.push(Attachment {
                blob: blob.ok_or(Error::Property("attachments"))?,
                name,
                content_type,
                description,
            });
        }
        Ok(result)
    }
    fn addresses<'a>(
        &mut self,
        value: Value<'a>,
        field: &'static str,
    ) -> Result<Vec<Address<'a>>, Error> {
        let values = value.as_array().ok_or(Error::Property(field))?;
        if values.len() > 1000 {
            return Err(Error::Property(field));
        }
        let mut result = Vec::new();
        result
            .try_reserve_exact(values.len())
            .map_err(|_| Error::Capacity)?;
        for value in values {
            self.charge(0)?;
            let mut name = "";
            let mut email = None;
            let mut fields = value.as_object().ok_or(Error::Property(field))?;
            while let Some((key, value)) = fields.next_bytes() {
                self.charge(0)?;
                match key {
                    b"name" if value.is_null() => (),
                    b"name" => name = self.string(value, field)?,
                    b"email" => email = Some(self.string(value, field)?),
                    _ => return Err(Error::Property(field)),
                }
            }
            result.push(Address {
                name,
                email: email.ok_or(Error::Property(field))?,
            });
        }
        Ok(result)
    }
    fn ids<'a>(&mut self, value: Value<'a>, field: &'static str) -> Result<Vec<&'a str>, Error> {
        let values = value.as_array().ok_or(Error::Property(field))?;
        if values.len() > 1000 {
            return Err(Error::Property(field));
        }
        let mut result = Vec::new();
        result
            .try_reserve_exact(values.len())
            .map_err(|_| Error::Capacity)?;
        for value in values {
            self.charge(0)?;
            result.push(self.string(value, field)?);
        }
        Ok(result)
    }
    fn mailbox(&mut self, value: Value<'_>) -> Result<MailboxId, Error> {
        let mut fields = value
            .as_object()
            .filter(|v| v.len() == 1)
            .ok_or(Error::Property("mailboxIds"))?;
        self.charge(0)?;
        let (key, value) = fields.next_bytes().ok_or(Error::Property("mailboxIds"))?;
        if key.len() != wire::IdKind::WIRE_BYTES || value.as_bool() != Some(true) {
            return Err(Error::Property("mailboxIds"));
        }
        self.charge(key.len())?;
        let key = std::str::from_utf8(key).map_err(|_| Error::Property("mailboxIds"))?;
        wire::Id::parse(key)
            .ok()
            .and_then(|id| id.local())
            .ok_or(Error::Property("mailboxIds"))
    }
    fn keywords(&mut self, value: Value<'_>) -> Result<(), Error> {
        let mut fields = value
            .as_object()
            .filter(|v| v.len() == 2)
            .ok_or(Error::Property("keywords"))?;
        let (mut draft, mut seen) = (false, false);
        while let Some((key, value)) = fields.next_bytes() {
            self.charge(0)?;
            if value.as_bool() != Some(true) {
                return Err(Error::Property("keywords"));
            }
            match key {
                b"$draft" => draft = true,
                b"$seen" => seen = true,
                _ => return Err(Error::Property("keywords")),
            }
        }
        if draft && seen {
            Ok(())
        } else {
            Err(Error::Property("keywords"))
        }
    }
    fn text_part(&mut self, value: Value<'_>) -> Result<(), Error> {
        let mut parts = value
            .as_array()
            .filter(|v| v.len() == 1)
            .ok_or(Error::Property("textBody"))?;
        let mut fields = parts
            .next()
            .and_then(|v| v.as_object())
            .ok_or(Error::Property("textBody"))?;
        let (mut id, mut kind) = (false, false);
        while let Some((key, value)) = fields.next_bytes() {
            self.charge(0)?;
            match key {
                b"partId" => id = self.string(value, "textBody")? == "text",
                b"type" => kind = self.string(value, "textBody")? == "text/plain",
                _ => return Err(Error::Property("textBody")),
            }
        }
        if id && kind {
            Ok(())
        } else {
            Err(Error::Property("textBody"))
        }
    }
    fn text<'a>(&mut self, value: Value<'a>) -> Result<&'a str, Error> {
        let mut parts = value
            .as_object()
            .filter(|v| v.len() == 1)
            .ok_or(Error::Property("bodyValues"))?;
        let (key, value) = parts.next_bytes().ok_or(Error::Property("bodyValues"))?;
        if key != b"text" {
            return Err(Error::Property("bodyValues"));
        }
        let mut fields = value.as_object().ok_or(Error::Property("bodyValues"))?;
        let (mut text, mut encoding, mut truncated) = (None, false, false);
        while let Some((key, value)) = fields.next_bytes() {
            self.charge(0)?;
            match key {
                b"value" => text = Some(self.string(value, "bodyValues")?),
                b"isEncodingProblem" => encoding = value.as_bool() == Some(false),
                b"isTruncated" => truncated = value.as_bool() == Some(false),
                _ => return Err(Error::Property("bodyValues")),
            }
        }
        if !encoding || !truncated {
            return Err(Error::Property("bodyValues"));
        }
        text.ok_or(Error::Property("bodyValues"))
    }
}
/// Decode only td-mail's emitted plain-text shape. `maximum_text` is the
/// caller's admitted JSON-byte limit (at most the configuration's 4 MiB cap).
/// MIME field syntax remains the composer's responsibility; all bytes remain
/// borrowed until native creation returns. No account or identity is authorized.
pub fn parse<'a>(
    value: Value<'a>,
    maximum_text: usize,
    clock: &dyn Clock,
    deadline: Deadline,
    meter: &mut Meter,
) -> Result<Draft<'a>, Error> {
    if meter.deadline() < deadline.into() {
        return Err(Error::Invalid);
    }
    if maximum_text > 4 * crate::limits::MIB {
        return Err(Error::Capacity);
    }
    let mut work = Work {
        clock,
        deadline,
        last: clock.sample().map_err(|_| Error::Clock)?.monotonic,
        meter,
        text_left: maximum_text,
    };
    work.charge(0)?;
    let mut fields = value.as_object().ok_or(Error::Property("create"))?;
    let (mut mailbox, mut from, mut subject, mut message_id, mut date, mut text) =
        (None, None, None, None, None, None);
    let (mut keywords, mut text_part) = (false, false);
    let mut attachments = Vec::new();
    let (mut to, mut cc, mut bcc, mut reply_to, mut in_reply_to, mut references) = (
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );
    while let Some((key, value)) = fields.next_bytes() {
        work.charge(0)?;
        match key {
            b"mailboxIds" => mailbox = Some(work.mailbox(value)?),
            b"keywords" => {
                work.keywords(value)?;
                keywords = true;
            }
            b"from" => {
                if !value.as_array().is_some_and(|values| values.len() == 1) {
                    return Err(Error::Property("from"));
                }
                from = work.addresses(value, "from")?.first().copied();
            }
            b"to" => to = work.addresses(value, "to")?,
            b"cc" => cc = work.addresses(value, "cc")?,
            b"bcc" => bcc = work.addresses(value, "bcc")?,
            b"replyTo" => reply_to = work.addresses(value, "replyTo")?,
            b"subject" => subject = Some(work.string(value, "subject")?),
            b"messageId" => {
                if !value.as_array().is_some_and(|values| values.len() == 1) {
                    return Err(Error::Property("messageId"));
                }
                message_id = work.ids(value, "messageId")?.first().copied();
            }
            b"inReplyTo" => in_reply_to = work.ids(value, "inReplyTo")?,
            b"references" => references = work.ids(value, "references")?,
            b"sentAt" => {
                let text = work.string(value, "sentAt")?;
                let bytes = text.as_bytes();
                if bytes.len() != 20
                    || bytes.get(4) != Some(&b'-')
                    || bytes.get(7) != Some(&b'-')
                    || bytes.get(10) != Some(&b'T')
                    || bytes.get(13) != Some(&b':')
                    || bytes.get(16) != Some(&b':')
                    || bytes.get(19) != Some(&b'Z')
                {
                    return Err(Error::Property("sentAt"));
                }
                date = Some(
                    text.get(..19)
                        .and_then(td_civil::parse_ymd_hms)
                        .filter(|civil| (1900..=9999).contains(&civil.year))
                        .and_then(|civil| td_civil::civil_to_unix_utc(&civil))
                        .and_then(|seconds| seconds.checked_mul(1000))
                        .ok_or(Error::Property("sentAt"))?,
                );
            }
            b"textBody" => {
                work.text_part(value)?;
                text_part = true;
            }
            b"bodyValues" => text = Some(work.text(value)?),
            b"attachments" => attachments = work.attachments(value)?,
            b"htmlBody" => return Err(Error::Property("htmlBody")),
            b"preview" => return Err(Error::Property("preview")),
            _ => return Err(Error::Property("create")),
        }
    }
    if !keywords {
        return Err(Error::Property("keywords"));
    }
    if !text_part {
        return Err(Error::Property("textBody"));
    }
    work.charge(0)?;
    Ok(Draft {
        attachments,
        mailbox: mailbox.ok_or(Error::Property("mailboxIds"))?,
        from: from.ok_or(Error::Property("from"))?,
        to,
        cc,
        bcc,
        reply_to,
        subject: subject.ok_or(Error::Property("subject"))?,
        message_id: message_id.ok_or(Error::Property("messageId"))?,
        in_reply_to,
        references,
        sent_at_ms: date.ok_or(Error::Property("sentAt"))?,
        text: text.ok_or(Error::Property("bodyValues"))?,
    })
}

#[cfg(test)]
#[path = "draft_input_tests.rs"]
mod tests;

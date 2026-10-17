//! Stored Email fields, independent of MIME interpretation.
use crate::{
    bounded::TextBuffer,
    email_property::Field,
    ids::{BlobId, EmailId, MailboxId, ThreadId},
    ports::{Error, ViewIdentity},
    wire,
};
pub const MAX_EMAIL_VALUES: usize = 256;

/// Passive snapshot values, not body-read or publication authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EmailMetadata {
    pub identity: ViewIdentity,
    pub email: EmailId,
    pub blob: BlobId,
    pub thread: ThreadId,
    pub size: u64,
    pub received_at: i64,
}

/// Ascending, exclusive-after window from the same admitted snapshot.
#[derive(Debug)]
pub struct EmailValues<T> {
    pub identity: ViewIdentity,
    pub email: EmailId,
    pub values: Vec<T>,
    pub has_more: bool,
}

/// Trusted storage adapter retaining one admitted snapshot and its original
/// budget. Reads must validate values and refuse after terminal failure.
pub trait Source {
    type Error: From<Error>;
    fn check_snapshot(&mut self) -> Result<(), Self::Error>;
    fn email_metadata(
        &mut self,
        email: EmailId,
        scratch: &mut [u8],
    ) -> Result<Option<EmailMetadata>, Self::Error>;
    fn email_mailboxes(
        &mut self,
        email: EmailId,
        after: Option<MailboxId>,
        maximum: usize,
    ) -> Result<EmailValues<MailboxId>, Self::Error>;
    fn email_keywords(
        &mut self,
        email: EmailId,
        after: Option<&str>,
        maximum: usize,
    ) -> Result<EmailValues<String>, Self::Error>;
}

/// Maximum bytes in one provisional sink callback.
pub const MAX_CHUNK_BYTES: usize = 512;
const MAX_UNSIGNED: u64 = 9_007_199_254_740_991;
const BLOB: u8 = 1;
const THREAD: u8 = 2;
const MAILBOXES: u8 = 4;
const KEYWORDS: u8 = 8;
const SIZE: u8 = 16;
const RECEIVED: u8 = 32;

/// Only the seven stored fields; id is always emitted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Stored(u8);
impl Stored {
    pub const fn all() -> Self {
        Self(BLOB | THREAD | MAILBOXES | KEYWORDS | SIZE | RECEIVED)
    }
    pub const fn id_only() -> Self {
        Self(0)
    }
    pub fn include(&mut self, field: Field) -> Result<(), Error> {
        self.0 |= match field {
            Field::Id => 0,
            Field::BlobId => BLOB,
            Field::ThreadId => THREAD,
            Field::MailboxIds => MAILBOXES,
            Field::Keywords => KEYWORDS,
            Field::Size => SIZE,
            Field::ReceivedAt => RECEIVED,
            Field::Headers
            | Field::BodyStructure
            | Field::BodyValues
            | Field::TextBody
            | Field::HtmlBody
            | Field::Attachments
            | Field::HasAttachment
            | Field::Preview => return Err(Error::Invalid),
        };
        Ok(())
    }
    fn has(self, field: u8) -> bool {
        self.0 & field != 0
    }
}

/// Emit a provisional complete stored-field object, or false for an absent
/// Email (no bytes emitted). The sink must retain all bytes until success and
/// charge its original request budget. Every emission is at most 512 bytes;
/// set reads retain at most one 256-value window. No callback may publish a
/// prefix. Original view checks surround callbacks and completion; the caller
/// additionally checks request authorization before final publication.
pub fn write_stored<S: Source + ?Sized>(
    view: &mut S,
    email: EmailId,
    selected: Stored,
    scratch: &mut [u8],
    mut emit: impl FnMut(&[u8]) -> Result<(), Error>,
) -> Result<bool, S::Error> {
    write_members(view, email, selected, scratch, &mut emit, true)
}

/// Internal composition keeps the object open for interpreted properties.
pub(crate) fn write_members<S: Source + ?Sized>(
    view: &mut S,
    email: EmailId,
    selected: Stored,
    scratch: &mut [u8],
    mut emit: impl FnMut(&[u8]) -> Result<(), Error>,
    close: bool,
) -> Result<bool, S::Error> {
    view.check_snapshot()?;
    let Some(metadata) = view.email_metadata(email, scratch)? else {
        view.check_snapshot()?;
        return Ok(false);
    };
    if metadata.email != email {
        return Err(Error::Corrupt.into());
    }
    let mut bytes = [0; MAX_CHUNK_BYTES];
    let mut out = TextBuffer::new(&mut bytes);
    let mut id_bytes = [0; wire::IdKind::WIRE_BYTES];
    let id = wire::encode_id(email, &mut id_bytes).map_err(|_| Error::Corrupt)?;
    format(&mut out, format_args!("{{\"id\":\"{id}\""))?;
    if selected.has(BLOB) {
        let id = wire::encode_id(metadata.blob, &mut id_bytes).map_err(|_| Error::Corrupt)?;
        format(&mut out, format_args!(",\"blobId\":\"{id}\""))?;
    }
    if selected.has(THREAD) {
        let id = wire::encode_id(metadata.thread, &mut id_bytes).map_err(|_| Error::Corrupt)?;
        format(&mut out, format_args!(",\"threadId\":\"{id}\""))?;
    }
    if selected.has(SIZE) {
        if metadata.size > MAX_UNSIGNED {
            return Err(Error::Corrupt.into());
        }
        format(&mut out, format_args!(",\"size\":{}", metadata.size))?;
    }
    if selected.has(RECEIVED) {
        append(&mut out, ",\"receivedAt\":\"")?;
        crate::jmap_date::write_utc_ms(metadata.received_at, &mut out).map_err(
            |error| match error {
                crate::jmap_date::Error::OutOfRange => Error::Corrupt,
                crate::jmap_date::Error::Output(_) => Error::Capacity,
            },
        )?;
        append(&mut out, "\"")?;
    }
    flush(view, &mut out, &mut emit)?;
    if selected.has(MAILBOXES) {
        append(&mut out, ",\"mailboxIds\":{")?;
        let mut after = None;
        let mut first = true;
        loop {
            let page = view.email_mailboxes(email, after, MAX_EMAIL_VALUES)?;
            if page.identity != metadata.identity || page.email != email {
                return Err(Error::Corrupt.into());
            }
            for mailbox in &page.values {
                let id = wire::encode_id(*mailbox, &mut id_bytes).map_err(|_| Error::Corrupt)?;
                buffered(
                    view,
                    &mut out,
                    &mut emit,
                    format_args!("{}\"{id}\":true", if first { "" } else { "," }),
                )?;
                first = false;
            }
            if !page.has_more {
                break;
            }
            after = Some(*page.values.last().ok_or(Error::Corrupt)?);
        }
        if first {
            return Err(Error::Corrupt.into());
        }
        buffered(view, &mut out, &mut emit, format_args!("}}"))?;
        flush(view, &mut out, &mut emit)?;
    }
    if selected.has(KEYWORDS) {
        append(&mut out, ",\"keywords\":{")?;
        let mut after = None::<String>;
        let mut first = true;
        loop {
            let mut page = view.email_keywords(email, after.as_deref(), MAX_EMAIL_VALUES)?;
            if page.identity != metadata.identity || page.email != email {
                return Err(Error::Corrupt.into());
            }
            for keyword in &page.values {
                // email_keywords revalidates lowercase canonical atoms on read,
                // excluding JSON quote/backslash and control/non-ASCII bytes.
                buffered(
                    view,
                    &mut out,
                    &mut emit,
                    format_args!("{}\"{keyword}\":true", if first { "" } else { "," }),
                )?;
                first = false;
            }
            if !page.has_more {
                break;
            }
            after = Some(page.values.pop().ok_or(Error::Corrupt)?);
        }
        buffered(view, &mut out, &mut emit, format_args!("}}"))?;
        flush(view, &mut out, &mut emit)?;
    }
    if close {
        append(&mut out, "}")?;
        flush(view, &mut out, &mut emit)?;
    }
    Ok(true)
}
fn append(out: &mut TextBuffer<'_>, text: &str) -> Result<(), Error> {
    out.append(text).map_err(|_| Error::Capacity)
}
fn format(out: &mut TextBuffer<'_>, args: std::fmt::Arguments<'_>) -> Result<(), Error> {
    out.format(args).map_err(|_| Error::Capacity)
}
fn buffered<S: Source + ?Sized>(
    view: &mut S,
    out: &mut TextBuffer<'_>,
    emit: &mut impl FnMut(&[u8]) -> Result<(), Error>,
    arguments: std::fmt::Arguments<'_>,
) -> Result<(), S::Error> {
    match out.format(arguments) {
        Ok(()) => Ok(()),
        Err(crate::bounded::Error::Capacity) => {
            // TextBuffer rolls back visible length on a failed format.
            flush(view, out, emit)?;
            format(out, arguments).map_err(Into::into)
        }
        Err(_) => Err(Error::Corrupt.into()),
    }
}
fn flush<S: Source + ?Sized>(
    view: &mut S,
    out: &mut TextBuffer<'_>,
    emit: &mut impl FnMut(&[u8]) -> Result<(), Error>,
) -> Result<(), S::Error> {
    view.check_snapshot()?;
    emit(out.as_bytes().map_err(|_| Error::Corrupt)?)?;
    view.check_snapshot()?;
    out.clear();
    Ok(())
}
#[cfg(test)]
#[path = "email_response_tests.rs"]
mod tests;

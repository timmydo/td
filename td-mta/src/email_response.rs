//! Stored Email fields, independent of MIME interpretation.
use crate::{
    bounded::TextBuffer,
    email_property::Field,
    ids::EmailId,
    ports::Error,
    store_fs::{IndexReadView, MAX_EMAIL_VALUES},
    wire,
};
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
pub fn write_stored(
    view: &mut IndexReadView<'_, '_>,
    email: EmailId,
    selected: Stored,
    scratch: &mut [u8],
    mut emit: impl FnMut(&[u8]) -> Result<(), Error>,
) -> Result<bool, Error> {
    view.check_snapshot()?;
    let Some(metadata) = view.email_metadata(email, scratch)? else {
        return Ok(false);
    };
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
            return Err(Error::Corrupt);
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
            return Err(Error::Corrupt);
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
    append(&mut out, "}")?;
    flush(view, &mut out, &mut emit)?;
    Ok(true)
}
fn append(out: &mut TextBuffer<'_>, text: &str) -> Result<(), Error> {
    out.append(text).map_err(|_| Error::Capacity)
}
fn format(out: &mut TextBuffer<'_>, args: std::fmt::Arguments<'_>) -> Result<(), Error> {
    out.format(args).map_err(|_| Error::Capacity)
}
fn buffered(
    view: &mut IndexReadView<'_, '_>,
    out: &mut TextBuffer<'_>,
    emit: &mut impl FnMut(&[u8]) -> Result<(), Error>,
    arguments: std::fmt::Arguments<'_>,
) -> Result<(), Error> {
    match out.format(arguments) {
        Ok(()) => Ok(()),
        Err(crate::bounded::Error::Capacity) => {
            // TextBuffer rolls back visible length on a failed format.
            flush(view, out, emit)?;
            format(out, arguments)
        }
        Err(_) => Err(Error::Corrupt),
    }
}
fn flush(
    view: &mut IndexReadView<'_, '_>,
    out: &mut TextBuffer<'_>,
    emit: &mut impl FnMut(&[u8]) -> Result<(), Error>,
) -> Result<(), Error> {
    view.check_snapshot()?;
    emit(out.as_bytes().map_err(|_| Error::Corrupt)?)?;
    view.check_snapshot()?;
    out.clear();
    Ok(())
}
#[cfg(test)]
#[path = "email_response_tests.rs"]
mod tests;

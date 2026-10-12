//! Bounded recognition of one decoded Email property; no value projection.
use crate::{
    admission::work::{Charge, Meter},
    header_property,
    ports::Tick,
};
pub use header_property::Error;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Field {
    Id,
    BlobId,
    ThreadId,
    MailboxIds,
    Keywords,
    Size,
    ReceivedAt,
    Headers,
    BodyStructure,
    BodyValues,
    TextBody,
    HtmlBody,
    Attachments,
    HasAttachment,
    Preview,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Property<'a> {
    Field(Field),
    Header(header_property::Property<'a>),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status<'a> {
    Yield,
    Complete(Option<Property<'a>>),
}
enum Phase<'a> {
    Header(header_property::Cursor<'a>),
    Fields,
    Complete(Option<Property<'a>>),
}
/// Borrows the immutable decoded request key. Completed results are passive;
/// the caller must independently check its original job before using them.
pub struct Cursor<'a> {
    source: &'a str,
    phase: Phase<'a>,
    failure: Option<Error>,
}
impl<'a> Cursor<'a> {
    pub const fn new(source: &'a str) -> Self {
        Self {
            source,
            phase: Phase::Header(header_property::Cursor::new(
                source,
                header_property::Context::Email,
            )),
            failure: None,
        }
    }
    pub fn poll(&mut self, now: Tick, work: &mut Meter) -> Result<Status<'a>, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self.step(now, work);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    fn step(&mut self, now: Tick, work: &mut Meter) -> Result<Status<'a>, Error> {
        match &mut self.phase {
            Phase::Header(cursor) => match cursor.poll(now, work)? {
                header_property::Status::Yield => Ok(Status::Yield),
                header_property::Status::Complete(Some(property)) => {
                    Ok(self.finish(Some(Property::Header(property))))
                }
                header_property::Status::Complete(None) => {
                    self.phase = Phase::Fields;
                    Ok(Status::Yield)
                }
            },
            Phase::Fields => {
                // Charge the whole fixed table before touching candidate bytes.
                work.charge(
                    now,
                    Charge {
                        io_bytes: FIELD_BYTES,
                        records: FIELDS.len() as u64,
                        ..Charge::default()
                    },
                )?;
                // The explicit length gate bounds compared source bytes by
                // the fixed candidate length even for a huge unknown key.
                let field = FIELDS.iter().find_map(|(name, field)| {
                    (name.len() == self.source.len() && *name == self.source).then_some(*field)
                });
                Ok(self.finish(field.map(Property::Field)))
            }
            Phase::Complete(value) => Ok(Status::Complete(*value)),
        }
    }
    fn finish(&mut self, value: Option<Property<'a>>) -> Status<'a> {
        self.phase = Phase::Complete(value);
        Status::Complete(value)
    }
}
const FIELDS: &[(&str, Field)] = &[
    ("id", Field::Id),
    ("blobId", Field::BlobId),
    ("threadId", Field::ThreadId),
    ("mailboxIds", Field::MailboxIds),
    ("keywords", Field::Keywords),
    ("size", Field::Size),
    ("receivedAt", Field::ReceivedAt),
    ("headers", Field::Headers),
    ("bodyStructure", Field::BodyStructure),
    ("bodyValues", Field::BodyValues),
    ("textBody", Field::TextBody),
    ("htmlBody", Field::HtmlBody),
    ("attachments", Field::Attachments),
    ("hasAttachment", Field::HasAttachment),
    ("preview", Field::Preview),
];
const FIELD_BYTES: u64 = {
    let mut sum = 0;
    let mut rest = FIELDS;
    while let Some(((name, _), tail)) = rest.split_first() {
        sum += name.len() as u64;
        rest = tail;
    }
    sum
};
const _: () = assert!(FIELD_BYTES <= 256 && FIELDS.len() <= 32);
const _: () = assert!(std::mem::size_of::<Cursor<'_>>() <= 192);
#[cfg(test)]
#[path = "email_property_tests.rs"]
mod tests;

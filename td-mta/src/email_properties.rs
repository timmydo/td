//! Complete passive Email property selection in fixed caller-owned cells.
pub use crate::property_selection::{Cell, Error, Status};
use crate::{
    admission::work::Meter,
    email_property, header_property,
    ports::Tick,
    property_selection::{Domain, Property, Recognition},
};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Fields(u16);
impl Fields {
    pub const fn id_only() -> Self {
        Self(bit(email_property::Field::Id))
    }
    pub const fn contains(self, field: email_property::Field) -> bool {
        self.0 & bit(field) != 0
    }
}
const fn bit(field: email_property::Field) -> u16 {
    match field {
        email_property::Field::Id => 1,
        email_property::Field::BlobId => 2,
        email_property::Field::ThreadId => 4,
        email_property::Field::MailboxIds => 8,
        email_property::Field::Keywords => 16,
        email_property::Field::Size => 32,
        email_property::Field::ReceivedAt => 64,
        email_property::Field::Headers => 128,
        email_property::Field::BodyStructure => 256,
        email_property::Field::BodyValues => 512,
        email_property::Field::TextBody => 1024,
        email_property::Field::HtmlBody => 2048,
        email_property::Field::Attachments => 4096,
        email_property::Field::HasAttachment => 8192,
        email_property::Field::Preview => 16384,
    }
}
pub type View<'v, 'k> = crate::property_selection::View<'v, 'k, Fields>;
pub type Cursor<'i, 'k, 'c> = crate::property_selection::Cursor<'i, 'k, 'c, Email>;
pub struct Email;
impl crate::property_selection::Sealed for Email {}
impl<'k> Domain<'k> for Email {
    type Parser = email_property::Cursor<'k>;
    type Field = email_property::Field;
    type Fields = Fields;
    fn new(key: &'k str) -> Self::Parser {
        email_property::Cursor::new(key)
    }
    fn poll(
        parser: &mut Self::Parser,
        now: Tick,
        work: &mut Meter,
    ) -> Result<Recognition<'k, Self::Field>, header_property::Error> {
        Ok(match parser.poll(now, work)? {
            email_property::Status::Yield => Recognition::Yield,
            email_property::Status::Complete(value) => {
                Recognition::Complete(value.map(|p| match p {
                    email_property::Property::Field(field) => Property::Field(field),
                    email_property::Property::Header(header) => Property::Header(header),
                }))
            }
        })
    }
    fn empty() -> Fields {
        Fields::id_only()
    }
    fn select(fields: &mut Fields, field: Self::Field) {
        fields.0 |= bit(field);
    }
}
/// Header cells required by the omitted/null default property selection.
pub const DEFAULT_HEADER_CELLS: usize = 11;
/// RFC 8621 section 4.2 defaults for omitted/null Email/get properties.
pub const DEFAULT: &[&str] = &[
    "id",
    "blobId",
    "threadId",
    "mailboxIds",
    "keywords",
    "size",
    "receivedAt",
    "messageId",
    "inReplyTo",
    "references",
    "sender",
    "from",
    "to",
    "cc",
    "bcc",
    "replyTo",
    "subject",
    "sentAt",
    "hasAttachment",
    "preview",
    "bodyValues",
    "textBody",
    "htmlBody",
    "attachments",
];
const _: () = assert!(std::mem::size_of::<Cursor<'_, '_, '_>>() <= 384);
#[cfg(test)]
#[path = "email_properties_tests.rs"]
mod tests;

#[path = "email_properties/request.rs"]
pub mod request;

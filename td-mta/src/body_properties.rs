//! Aggregate decoded body-part request keys in fixed caller-owned cells.
use crate::{
    admission::work::Meter,
    body_property, header_property,
    ports::Tick,
    property_selection::{Domain, Property, Recognition},
};

pub use crate::mime_response::selected_metadata::Properties as Metadata;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Fields {
    pub metadata: Metadata,
    pub headers: bool,
    pub sub_parts: bool,
}
impl Fields {
    pub const NONE: Self = Self {
        metadata: Metadata::NONE,
        headers: false,
        sub_parts: false,
    };
    /// RFC 8621's omitted bodyProperties argument; explicit empty stays NONE.
    pub const DEFAULT: Self = Self {
        metadata: Metadata::ALL,
        headers: false,
        sub_parts: false,
    };
    fn select(&mut self, field: body_property::Field) {
        use body_property::Field;
        match field {
            Field::PartId => self.metadata.part_id = true,
            Field::BlobId => self.metadata.blob_id = true,
            Field::Size => self.metadata.size = true,
            Field::Headers => self.headers = true,
            Field::Name => self.metadata.name = true,
            Field::MediaType => self.metadata.media_type = true,
            Field::Charset => self.metadata.charset = true,
            Field::Disposition => self.metadata.disposition = true,
            Field::Cid => self.metadata.cid = true,
            Field::Language => self.metadata.language = true,
            Field::Location => self.metadata.location = true,
            Field::SubParts => self.sub_parts = true,
        }
    }
}
pub use crate::property_selection::{Cell, Error, Status};
pub type View<'v, 'k> = crate::property_selection::View<'v, 'k, Fields>;
pub type Cursor<'i, 'k, 'c> = crate::property_selection::Cursor<'i, 'k, 'c, Body>;
pub struct Body;
impl crate::property_selection::Sealed for Body {}
impl<'k> Domain<'k> for Body {
    type Parser = body_property::Cursor<'k>;
    type Field = body_property::Field;
    type Fields = Fields;
    fn new(key: &'k str) -> Self::Parser {
        body_property::Cursor::new(key)
    }
    fn poll(
        parser: &mut Self::Parser,
        now: Tick,
        work: &mut Meter,
    ) -> Result<Recognition<'k, Self::Field>, header_property::Error> {
        Ok(match parser.poll(now, work)? {
            body_property::Status::Yield => Recognition::Yield,
            body_property::Status::Complete(value) => {
                Recognition::Complete(value.map(|p| match p {
                    body_property::Property::Field(field) => Property::Field(field),
                    body_property::Property::Header(header) => Property::Header(header),
                }))
            }
        })
    }
    fn empty() -> Fields {
        Fields::NONE
    }
    fn select(fields: &mut Fields, field: Self::Field) {
        fields.select(field);
    }
}
#[path = "body_properties/json.rs"]
pub mod json;
#[path = "body_properties/request.rs"]
pub mod request;
const _: () = assert!(std::mem::size_of::<Cursor<'_, '_, '_>>() <= 384);
const _: () = assert!(std::mem::size_of::<Cell<'_>>() <= 64);
#[cfg(test)]
pub use crate::property_selection::body_tests::probe as probe_allocations;

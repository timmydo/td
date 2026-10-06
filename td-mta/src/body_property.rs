//! Bounded recognition of one decoded EmailBodyPart property key.
use crate::{
    admission::work::{Charge, Meter},
    header_property,
    ports::Tick,
};
pub use header_property::Error;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Field {
    PartId,
    BlobId,
    Size,
    Headers,
    Name,
    MediaType,
    Charset,
    Disposition,
    Cid,
    Language,
    Location,
    SubParts,
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
    Fields(usize),
    Complete(Option<Property<'a>>),
}
/// Borrow one immutable decoded request key; parsing grants no publication authority.
/// ```compile_fail,E0277
/// fn required<T: Copy>() {} required::<td_mta::body_property::Cursor<'_>>();
/// ```
/// ```compile_fail,E0277
/// fn required<T: Clone>() {} required::<td_mta::body_property::Cursor<'_>>();
/// ```
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
                header_property::Context::BodyPart,
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
                    self.phase = Phase::Fields(0);
                    Ok(Status::Yield)
                }
            },
            Phase::Fields(row) => {
                let Some(&(key, field)) = FIELDS.get(*row) else {
                    charge(now, work, 0)?;
                    return Ok(self.finish(None));
                };
                let same_length = key.len() == self.source.len();
                charge(now, work, if same_length { key.len() } else { 0 })?;
                if same_length && key == self.source {
                    return Ok(self.finish(Some(Property::Field(field))));
                }
                *row = row.checked_add(1).ok_or(Error::InvalidState)?;
                Ok(Status::Yield)
            }
            Phase::Complete(value) => Ok(Status::Complete(*value)),
        }
    }
    fn finish(&mut self, value: Option<Property<'a>>) -> Status<'a> {
        self.phase = Phase::Complete(value);
        Status::Complete(value)
    }
}
fn charge(now: Tick, work: &mut Meter, bytes: usize) -> Result<(), Error> {
    work.charge(
        now,
        Charge {
            io_bytes: u64::try_from(bytes).map_err(|_| Error::InvalidState)?,
            records: 1,
            ..Charge::default()
        },
    )?;
    Ok(())
}
const FIELDS: &[(&str, Field)] = &[
    ("partId", Field::PartId),
    ("blobId", Field::BlobId),
    ("size", Field::Size),
    ("headers", Field::Headers),
    ("name", Field::Name),
    ("type", Field::MediaType),
    ("charset", Field::Charset),
    ("disposition", Field::Disposition),
    ("cid", Field::Cid),
    ("language", Field::Language),
    ("location", Field::Location),
    ("subParts", Field::SubParts),
];
const _: () = assert!(std::mem::size_of::<Cursor<'_>>() <= 192);
const _: () = assert!(std::mem::size_of::<Property<'_>>() <= 64);
#[cfg(test)]
pub use tests::probe as probe_allocations;
#[cfg(test)]
#[path = "body_property/tests.rs"]
mod tests;

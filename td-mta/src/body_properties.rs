//! Aggregate decoded body-part request keys in fixed caller-owned cells.
use crate::{
    admission::work::{Charge, Meter, Stop},
    body_property, header_property,
    ports::Tick,
};

pub use crate::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::selected::Properties as Metadata;
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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Recognition(body_property::Error),
    Capacity,
    InvalidProperty,
    InvalidState,
}
impl From<body_property::Error> for Error {
    fn from(error: body_property::Error) -> Self {
        Self::Recognition(error)
    }
}
impl From<Stop> for Error {
    fn from(error: Stop) -> Self {
        Self::Recognition(body_property::Error::Work(error))
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Recognition(error) => write!(f, "body property selection: {error}"),
            Self::Capacity => f.write_str("body property header cells exhausted"),
            Self::InvalidProperty => f.write_str("unknown body property"),
            Self::InvalidState => f.write_str("invalid body property selection state"),
        }
    }
}
impl std::error::Error for Error {}
/// Reusable caller storage; only the complete selected prefix is a whole result.
pub struct Cell<'k> {
    property: Option<header_property::Property<'k>>,
}
impl<'k> Cell<'k> {
    pub const fn new() -> Self {
        Self { property: None }
    }
    pub fn property(&self) -> Option<header_property::Property<'k>> {
        self.property
    }
}
impl Default for Cell<'_> {
    fn default() -> Self {
        Self::new()
    }
}
/// Complete passive selection; construction is confined to the cursor.
/// ```compile_fail,E0451
/// let cells = [td_mta::body_properties::Cell::new()];
/// let _ = td_mta::body_properties::View {
///     fields: td_mta::body_properties::Fields::NONE, headers: &cells,
/// };
/// ```
#[derive(Clone, Copy)]
pub struct View<'v, 'k> {
    fields: Fields,
    headers: &'v [Cell<'k>],
}
impl<'v, 'k> View<'v, 'k> {
    pub fn fields(&self) -> Fields {
        self.fields
    }
    pub fn headers(&self) -> &'v [Cell<'k>] {
        self.headers
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Yield,
    Complete,
}
enum Phase<'k> {
    Parse(body_property::Cursor<'k>),
    Header {
        candidate: header_property::Property<'k>,
        slot: usize,
        offset: usize,
    },
    Complete,
}
/// Pure selection owns no source custody or publication authority.
/// ```compile_fail,E0277
/// fn required<T: Copy>() {} required::<td_mta::body_properties::Cursor<'_, '_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn required<T: Clone>() {} required::<td_mta::body_properties::Cursor<'_, '_, '_>>();
/// ```
pub struct Cursor<'i, 'k, 'c> {
    keys: &'i [&'k str],
    cells: &'c mut [Cell<'k>],
    next: usize,
    used: usize,
    fields: Fields,
    phase: Phase<'k>,
    failure: Option<Error>,
}
impl<'i, 'k, 'c> Cursor<'i, 'k, 'c> {
    pub fn new(keys: &'i [&'k str], cells: &'c mut [Cell<'k>]) -> Self {
        let phase = match keys.first() {
            Some(key) => Phase::Parse(body_property::Cursor::new(key)),
            None => Phase::Complete,
        };
        Self {
            keys,
            cells,
            next: 0,
            used: 0,
            fields: Fields::NONE,
            phase,
            failure: None,
        }
    }
    pub fn value(&self) -> Option<View<'_, 'k>> {
        if self.failure.is_some() || !matches!(self.phase, Phase::Complete) {
            return None;
        }
        Some(View {
            fields: self.fields,
            headers: self.cells.get(..self.used)?,
        })
    }
    pub fn finish(self) -> Result<View<'c, 'k>, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if !matches!(self.phase, Phase::Complete) {
            return Err(Error::InvalidState);
        }
        Ok(View {
            fields: self.fields,
            headers: self.cells.get(..self.used).ok_or(Error::InvalidState)?,
        })
    }
    pub fn poll(&mut self, now: Tick, work: &mut Meter) -> Result<Status, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self.step(now, work);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    fn advance(&mut self) -> Result<Status, Error> {
        self.next = self.next.checked_add(1).ok_or(Error::InvalidState)?;
        self.phase = match self.keys.get(self.next) {
            Some(key) => Phase::Parse(body_property::Cursor::new(key)),
            None => Phase::Complete,
        };
        Ok(if matches!(self.phase, Phase::Complete) {
            Status::Complete
        } else {
            Status::Yield
        })
    }
    fn step(&mut self, now: Tick, work: &mut Meter) -> Result<Status, Error> {
        match &mut self.phase {
            Phase::Complete => Ok(Status::Complete),
            Phase::Parse(cursor) => match cursor.poll(now, work)? {
                body_property::Status::Yield => Ok(Status::Yield),
                body_property::Status::Complete(None) => Err(Error::InvalidProperty),
                body_property::Status::Complete(Some(body_property::Property::Field(field))) => {
                    self.fields.select(field);
                    self.advance()
                }
                body_property::Status::Complete(Some(body_property::Property::Header(
                    candidate,
                ))) => {
                    self.phase = Phase::Header {
                        candidate,
                        slot: 0,
                        offset: 0,
                    };
                    Ok(Status::Yield)
                }
            },
            Phase::Header {
                candidate,
                slot,
                offset,
            } => {
                if *slot >= self.used {
                    charge(now, work, 0)?;
                    let used = self.used.checked_add(1).ok_or(Error::InvalidState)?;
                    self.cells
                        .get_mut(self.used)
                        .ok_or(Error::Capacity)?
                        .property = Some(*candidate);
                    self.used = used;
                    return self.advance();
                }
                let existing = self
                    .cells
                    .get(*slot)
                    .and_then(Cell::property)
                    .ok_or(Error::InvalidState)?;
                let left = existing.requested().as_bytes();
                let right = candidate.requested().as_bytes();
                let next_slot = slot.checked_add(1).ok_or(Error::InvalidState)?;
                if left.len() != right.len() {
                    charge(now, work, 0)?;
                    *slot = next_slot;
                    *offset = 0;
                    return Ok(Status::Yield);
                }
                let width = left
                    .len()
                    .checked_sub(*offset)
                    .ok_or(Error::InvalidState)?
                    .min(64);
                let end = offset.checked_add(width).ok_or(Error::InvalidState)?;
                charge(now, work, width.checked_mul(2).ok_or(Error::InvalidState)?)?;
                let same = left.get(*offset..end).ok_or(Error::InvalidState)?
                    == right.get(*offset..end).ok_or(Error::InvalidState)?;
                if !same {
                    *slot = next_slot;
                    *offset = 0;
                    return Ok(Status::Yield);
                }
                if end == left.len() {
                    return self.advance();
                }
                *offset = end;
                Ok(Status::Yield)
            }
        }
    }
}
fn charge(now: Tick, work: &mut Meter, visits: usize) -> Result<(), Error> {
    work.charge(
        now,
        Charge {
            io_bytes: u64::try_from(visits).map_err(|_| Error::InvalidState)?,
            records: 1,
            ..Charge::default()
        },
    )?;
    Ok(())
}
const _: () = assert!(std::mem::size_of::<Cursor<'_, '_, '_>>() <= 384);
const _: () = assert!(std::mem::size_of::<Cell<'_>>() <= 64);
#[cfg(test)]
pub use tests::probe as probe_allocations;
#[cfg(test)]
#[path = "body_properties/tests.rs"]
mod tests;

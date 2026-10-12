//! Shared bounded aggregation of decoded Email and body-part property keys.
use crate::{
    admission::work::{Charge, Meter, Stop},
    header_property,
    ports::Tick,
};
#[derive(Clone, Copy)]
pub enum Property<'k, F> {
    Field(F),
    Header(header_property::Property<'k>),
}
pub enum Recognition<'k, F> {
    Yield,
    Complete(Option<Property<'k, F>>),
}
mod sealed {
    pub trait Sealed {}
}
pub(crate) use sealed::Sealed;
/// Built-in bounded classifiers; external implementations are forbidden.
/// ```compile_fail,E0277
/// use td_mta::{email_properties::Fields, email_property::Field,
///     header_property, property_selection::{Domain, Recognition},
///     admission::work::Meter, ports::Tick};
/// struct External;
/// impl<'k> Domain<'k> for External {
///     type Parser = (); type Field = Field; type Fields = Fields;
///     fn new(_: &'k str) {}
///     fn poll(_: &mut (), _: Tick, _: &mut Meter)
///         -> Result<Recognition<'k, Field>, header_property::Error> {
///         Ok(Recognition::Complete(None))
///     }
///     fn empty() -> Fields { Fields::id_only() }
///     fn select(_: &mut Fields, _: Field) {}
/// }
/// ```
pub trait Domain<'k>: sealed::Sealed {
    type Parser;
    type Field: Copy;
    type Fields: Copy;
    fn new(key: &'k str) -> Self::Parser;
    fn poll(
        parser: &mut Self::Parser,
        now: Tick,
        work: &mut Meter,
    ) -> Result<Recognition<'k, Self::Field>, header_property::Error>;
    fn empty() -> Self::Fields;
    fn select(fields: &mut Self::Fields, field: Self::Field);
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Recognition(header_property::Error),
    Capacity,
    InvalidProperty,
    InvalidState,
}
impl From<header_property::Error> for Error {
    fn from(error: header_property::Error) -> Self {
        Self::Recognition(error)
    }
}
impl From<Stop> for Error {
    fn from(error: Stop) -> Self {
        Self::Recognition(header_property::Error::Work(error))
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Recognition(error) => write!(f, "property selection: {error}"),
            Self::Capacity => f.write_str("property header cells exhausted"),
            Self::InvalidProperty => f.write_str("unknown property"),
            Self::InvalidState => f.write_str("invalid property selection state"),
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
/// Complete passive selection; only body defaults have an internal shortcut.
/// ```compile_fail,E0624
/// let _ = td_mta::body_properties::View::body_default(&[]);
/// ```
/// ```compile_fail,E0451
/// let cells = [td_mta::body_properties::Cell::new()];
/// let _ = td_mta::body_properties::View {
///     fields: td_mta::body_properties::Fields::NONE, headers: &cells,
/// };
/// ```
#[derive(Clone, Copy)]
pub struct View<'v, 'k, F: Copy> {
    fields: F,
    headers: &'v [Cell<'k>],
}
impl<'v, 'k, F: Copy> View<'v, 'k, F> {
    pub fn fields(&self) -> F {
        self.fields
    }
    pub fn headers(&self) -> &'v [Cell<'k>] {
        self.headers
    }
}
// Only body defaults omit every header property; preserve the original loan.
impl<'v, 'k> View<'v, 'k, crate::body_properties::Fields> {
    pub(crate) fn body_default(headers: &'v [Cell<'k>]) -> Option<Self> {
        Some(Self {
            fields: crate::body_properties::Fields::DEFAULT,
            headers: headers.get(..0)?,
        })
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Yield,
    Complete,
}
enum Phase<'k, P> {
    Parse(P),
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
pub struct Cursor<'i, 'k, 'c, D: Domain<'k>> {
    keys: &'i [&'k str],
    cells: &'c mut [Cell<'k>],
    next: usize,
    used: usize,
    fields: D::Fields,
    phase: Phase<'k, D::Parser>,
    failure: Option<Error>,
}
impl<'i, 'k, 'c, D: Domain<'k>> Cursor<'i, 'k, 'c, D> {
    pub fn new(keys: &'i [&'k str], cells: &'c mut [Cell<'k>]) -> Self {
        let phase = match keys.first() {
            Some(key) => Phase::Parse(D::new(key)),
            None => Phase::Complete,
        };
        Self {
            keys,
            cells,
            next: 0,
            used: 0,
            fields: D::empty(),
            phase,
            failure: None,
        }
    }
    pub fn value(&self) -> Option<View<'_, 'k, D::Fields>> {
        if self.failure.is_some() || !matches!(self.phase, Phase::Complete) {
            return None;
        }
        Some(View {
            fields: self.fields,
            headers: self.cells.get(..self.used)?,
        })
    }
    pub fn finish(self) -> Result<View<'c, 'k, D::Fields>, Error> {
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
            Some(key) => Phase::Parse(D::new(key)),
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
            Phase::Parse(cursor) => match D::poll(cursor, now, work)? {
                Recognition::Yield => Ok(Status::Yield),
                Recognition::Complete(None) => Err(Error::InvalidProperty),
                Recognition::Complete(Some(Property::Field(field))) => {
                    D::select(&mut self.fields, field);
                    self.advance()
                }
                Recognition::Complete(Some(Property::Header(candidate))) => {
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

#[cfg(test)]
#[path = "body_properties/tests.rs"]
pub(crate) mod body_tests;

//! Collect every original source-bound member while retaining the actual pin.
pub mod composed;
use super::super::super::super::Error as OriginalError;
use super::{Bound, Cursor, Error, RetainStatus, View as MemberView};
use crate::{admission::work::Charge, nfc::HeaderBudget, ports::Tick};

pub struct Cell<'m> {
    output: Option<&'m mut [u8]>,
    member: Option<MemberView<'m>>,
}
impl<'m> Cell<'m> {
    pub const fn new(output: &'m mut [u8]) -> Self {
        Self {
            output: Some(output),
            member: None,
        }
    }
    pub fn value(&self) -> Option<MemberView<'_>> {
        self.member
    }
}
#[derive(Clone, Copy)]
pub struct View<'v, 'm, 'o> {
    pub original: super::super::super::View<'v, 'o>,
    pub members: &'v [Cell<'m>],
}
/// Exclusive original source-bound collection custody.
/// ```compile_fail,E0277
/// fn required<T: Copy>() {} required::<td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::retained::collected::Collecting<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn required<T: Clone>() {} required::<td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::retained::collected::Collecting<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0308
/// use td_mta::{ports::Tick, mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::retained::collected::{Collecting, Cell, View}};
/// fn passive(view: View<'_, '_, '_>, cells: &mut [Cell<'_>]) { let _ = Collecting::new(view, cells, Tick(1)); }
/// ```
/// ```compile_fail,E0308
/// use td_mta::{ports::Tick, mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::retained::{Retained, collected::{Collecting, Cell}}};
/// fn member(retained: Retained<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_>, cells: &mut [Cell<'_>]) { let _ = Collecting::new(retained, cells, Tick(1)); }
/// ```
pub struct Collecting<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm> {
    source: Option<Bound<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k>>,
    cells: &'s mut [Cell<'m>],
    next: usize,
    failure: Option<Error>,
}
impl<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>
    Collecting<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>
{
    pub fn new(
        mut source: Bound<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k>,
        cells: &'s mut [Cell<'m>],
        now: Tick,
    ) -> Result<Self, Error> {
        source.check_deadline(now)?;
        let total = source.value().ok_or(Error::InvalidState)?.candidates.len();
        if cells.len() < total {
            return Err(Error::Original(OriginalError::ResponseCapacity));
        }
        if cells.len() != total {
            return Err(Error::InvalidState);
        }
        Ok(Self {
            source: Some(source),
            cells,
            next: 0,
            failure: None,
        })
    }
    fn outcome<T>(&mut self, result: Result<T, Error>) -> Result<T, Error> {
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self
            .source
            .as_mut()
            .ok_or(Error::InvalidState)?
            .check_deadline(now);
        self.outcome(result)
    }
    pub fn completed(&self) -> Result<usize, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        self.source
            .as_ref()
            .and_then(|source| source.value())
            .ok_or(Error::InvalidState)?;
        Ok(self.next)
    }
    pub fn total(&self) -> Result<usize, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        self.source
            .as_ref()
            .and_then(|source| source.value())
            .ok_or(Error::InvalidState)?;
        Ok(self.cells.len())
    }
    pub fn next<'t>(
        &'t mut self,
        now: Tick,
    ) -> Result<Child<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 't, 'm>, Error> {
        self.check_deadline(now)?;
        self.failure = Some(Error::InvalidState);
        let ordinal = self
            .next
            .checked_add(1)
            .and_then(|index| u16::try_from(index).ok())
            .ok_or(Error::InvalidState)?;
        let cell = self.cells.get_mut(self.next).ok_or(Error::InvalidState)?;
        if cell.member.is_some() {
            return Err(Error::InvalidState);
        }
        let output = cell.output.take().ok_or(Error::InvalidState)?;
        let source = self.source.take().ok_or(Error::InvalidState)?;
        let cursor = match Cursor::new(source, ordinal, output, now) {
            Ok(cursor) => cursor,
            Err(error) => {
                self.failure = Some(error);
                return Err(error);
            }
        };
        self.failure = Some(Error::Original(OriginalError::Abandoned));
        Ok(Child {
            cursor: Some(cursor),
            source: &mut self.source,
            next: &mut self.next,
            slot: &mut cell.member,
            parent_failure: &mut self.failure,
            failure: None,
        })
    }
    pub fn finish(
        mut self,
        now: Tick,
    ) -> Result<Serialized<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>, Error> {
        self.check_deadline(now)?;
        if self.next != self.cells.len() {
            return Err(Error::InvalidState);
        }
        Ok(Serialized {
            source: self.source.take().ok_or(Error::InvalidState)?,
            cells: self.cells,
        })
    }
}
/// One original ordinal handoff; abandonment poisons the borrowed parent.
/// ```compile_fail,E0277
/// fn required<T: Copy>() {} required::<td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::retained::collected::Child<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn required<T: Clone>() {} required::<td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::retained::collected::Child<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
pub struct Child<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 't, 'm> {
    cursor: Option<Cursor<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 'm>>,
    source: &'t mut Option<Bound<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k>>,
    next: &'t mut usize,
    slot: &'t mut Option<MemberView<'m>>,
    parent_failure: &'t mut Option<Error>,
    failure: Option<Error>,
}
impl<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 't, 'm> Child<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 't, 'm> {
    fn outcome<T>(&mut self, result: Result<T, Error>) -> Result<T, Error> {
        if let Err(error) = result {
            self.failure = Some(error);
            *self.parent_failure = Some(error);
        }
        result
    }
    pub fn value(&self) -> Option<MemberView<'_>> {
        if self.failure.is_some() {
            return None;
        }
        self.cursor.as_ref()?.value()
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self
            .cursor
            .as_mut()
            .ok_or(Error::InvalidState)?
            .check_deadline(now);
        self.outcome(result)
    }
    pub fn poll(&mut self, now: Tick) -> Result<RetainStatus, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self.cursor.as_mut().ok_or(Error::InvalidState)?.poll(now);
        self.outcome(result)
    }
    pub fn finish(mut self, now: Tick) -> Result<(), Error> {
        self.check_deadline(now)?;
        let result = self
            .cursor
            .take()
            .ok_or(Error::InvalidState)?
            .finish(now)
            .and_then(|retained| retained.finish(now));
        let (mut source, ordinal, members) = self.outcome(result)?;
        let result = self.next.checked_add(1).ok_or(Error::InvalidState);
        let next = self.outcome(result)?;
        if usize::from(ordinal) != next {
            return self.outcome(Err(Error::InvalidState));
        }
        let structure = &mut source.original.source.original.source.projected.structure;
        let result = structure
            .budget
            .charge(structure.work, now, 0, 1, &mut 0)
            .map_err(|error| Error::Original(OriginalError::Admission(error)))
            .and_then(|()| {
                structure
                    .work
                    .charge(
                        now,
                        Charge {
                            records: 1,
                            ..Charge::default()
                        },
                    )
                    .map_err(|error| {
                        Error::Original(OriginalError::Admission(crate::nfc::Error::Work(error)))
                    })
            });
        let result = source
            .parent
            .check_deadline()
            .map_err(Error::Parent)
            .and(result);
        self.outcome(result)?;
        *self.slot = Some(MemberView { ordinal, members });
        *self.source = Some(source);
        *self.next = next;
        *self.parent_failure = None;
        Ok(())
    }
}
/// Every original ordinal retained while keeping original source and pin.
/// ```compile_fail,E0277
/// fn required<T: Copy>() {} required::<td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::retained::collected::Serialized<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn required<T: Clone>() {} required::<td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::retained::collected::Serialized<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
pub struct Serialized<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm> {
    source: Bound<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k>,
    cells: &'s [Cell<'m>],
}
pub type Release<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm> =
    (Bound<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k>, &'s [Cell<'m>]);
impl<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>
    Serialized<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>
{
    pub fn value(&self) -> Option<View<'_, 'm, 'o>> {
        Some(View {
            original: self.source.value()?,
            members: self.cells,
        })
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        self.source.check_deadline(now)
    }
    pub fn finish(
        mut self,
        now: Tick,
    ) -> Result<Release<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>, Error> {
        self.check_deadline(now)?;
        Ok((self.source, self.cells))
    }
}
const _: () = assert!(std::mem::size_of::<Cell<'_>>() <= 64);
const _: () = assert!(
    std::mem::size_of::<Collecting<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>()
        + std::mem::size_of::<Child<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>()
        + std::mem::size_of::<HeaderBudget>()
        <= 2048
);
const _: () =
    assert!(std::mem::size_of::<Serialized<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>() <= 768);

#[cfg(test)]
pub use tests::probe as probe_allocations;
#[cfg(test)]
mod tests;

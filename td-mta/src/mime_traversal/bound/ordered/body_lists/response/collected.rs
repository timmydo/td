//! Retain every original fragment before granting whole serialization ownership.
use super::{json::retained, Error, Part, Projected, Projecting, Selected, View as ListsView};
pub use crate::mime_traversal::Status;
use crate::{
    admission::work::{Charge, Meter},
    mime_part_headers::label_json,
    nfc::{HeaderBudget, Scratch},
    ports::Tick,
};

/// One separately admitted part window. Reused or passive cells grant no authority.
pub struct Cell<'o> {
    output: Option<&'o mut [u8]>,
    retained: Option<retained::Retained<'o>>,
}
impl<'o> Cell<'o> {
    pub const fn new(output: &'o mut [u8]) -> Self {
        Self {
            output: Some(output),
            retained: None,
        }
    }
    /// Passive bytes remain provisional through fresh whole-job publication.
    pub fn value(&self) -> Option<retained::Retained<'_>> {
        self.retained
    }
}
#[derive(Clone, Copy)]
pub struct View<'w, 'n, 'c, 'o> {
    pub selected: ListsView<'w, 'n>,
    pub fragments: &'c [Cell<'o>],
}
/// Only the original selected owner enters; passive or completed visitation cannot.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mta::mime_traversal::bound::ordered::body_lists::response::collected::Collecting<'_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mta::mime_traversal::bound::ordered::body_lists::response::collected::Collecting<'_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0308
/// use td_mta::{mime_traversal::bound::ordered::body_lists::response::{Projected, collected::{Collecting, Cell}}, ports::Tick};
/// fn substitute<'c, 'o>(projected: Projected<'_, '_, '_>, cells: &'c mut [Cell<'o>]) { let _ = Collecting::new(projected, cells, Tick(1)); }
/// ```
pub struct Collecting<'a, 'w, 'n, 'c, 'o> {
    projecting: Projecting<'a, 'w, 'n>,
    cells: &'c mut [Cell<'o>],
    failure: Option<Error>,
}
impl<'a, 'w, 'n, 'c, 'o> Collecting<'a, 'w, 'n, 'c, 'o> {
    pub fn new(
        selected: Selected<'a, 'w, 'n>,
        cells: &'c mut [Cell<'o>],
        now: Tick,
    ) -> Result<Self, Error> {
        let projecting = Projecting::new(selected, now)?;
        let total = projecting.total()?;
        if cells.len() < total {
            return Err(Error::ResponseCapacity);
        }
        if cells.len() > total {
            return Err(Error::InvalidState);
        }
        Ok(Self {
            projecting,
            cells,
            failure: None,
        })
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self.projecting.check_deadline(now);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    pub fn completed(&self) -> Result<usize, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        self.projecting.completed()
    }
    pub fn total(&self) -> Result<usize, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        self.projecting.total()
    }
    pub fn next<'m>(
        &'m mut self,
        backing: label_json::Backing<'m>,
        scratch: &'m mut Scratch,
        now: Tick,
    ) -> Result<Child<'a, 'm, 'o>, Error> {
        self.check_deadline(now)?;
        let ordinal = match self.projecting.completed() {
            Ok(ordinal) => ordinal,
            Err(error) => {
                self.failure = Some(error);
                return Err(error);
            }
        };
        let structure = &mut self.projecting.structure;
        let result = structure
            .budget
            .charge(structure.work, now, 0, 1, &mut 0)
            .map_err(Error::Admission);
        if let Err(error) = result {
            self.failure = Some(error);
            return Err(error);
        }
        self.failure = Some(Error::Abandoned);
        let Some(cell) = self.cells.get_mut(ordinal) else {
            self.failure = Some(Error::InvalidState);
            return Err(Error::InvalidState);
        };
        let Some(output) = cell.output.take() else {
            self.failure = Some(Error::InvalidState);
            return Err(Error::InvalidState);
        };
        let child = match self.projecting.next(backing, scratch, now) {
            Ok(child) => child,
            Err(error) => {
                self.failure = Some(error);
                return Err(error);
            }
        };
        Ok(Child {
            phase: Some(Phase::Metadata(child)),
            output: Some(output),
            slot: &mut cell.retained,
            parent_failure: &mut self.failure,
            failure: None,
        })
    }
    pub fn finish(mut self, now: Tick) -> Result<Serialized<'a, 'w, 'n, 'c, 'o>, Error> {
        self.check_deadline(now)?;
        let projected = self.projecting.finish(now)?;
        Ok(Serialized {
            projected,
            cells: self.cells,
        })
    }
}
// Serial inline owners share the compiled 8 KiB parser reservation.
#[allow(clippy::large_enum_variant)]
enum Phase<'a, 'm, 'o> {
    Metadata(Part<'a, 'm>),
    Framing(retained::Cursor<'m, 'o>),
}
/// The metadata and retention phases cannot escape or skip their original slot.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mta::mime_traversal::bound::ordered::body_lists::response::collected::Child<'_, '_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mta::mime_traversal::bound::ordered::body_lists::response::collected::Child<'_, '_, '_>>();
/// ```
pub struct Child<'a, 'm, 'o> {
    phase: Option<Phase<'a, 'm, 'o>>,
    output: Option<&'o mut [u8]>,
    slot: &'m mut Option<retained::Retained<'o>>,
    parent_failure: &'m mut Option<Error>,
    failure: Option<Error>,
}
impl<'a, 'm, 'o> Child<'a, 'm, 'o> {
    fn outcome<T>(&mut self, result: Result<T, Error>) -> Result<T, Error> {
        if let Err(error) = result {
            self.failure = Some(error);
            *self.parent_failure = Some(error);
        }
        result
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = match self.phase.as_mut() {
            Some(Phase::Metadata(child)) => child.check_deadline(now),
            Some(Phase::Framing(child)) => child.check_deadline(now),
            None => Err(Error::InvalidState),
        };
        self.outcome(result)
    }
    pub fn value(&self) -> Option<retained::Retained<'_>> {
        if self.failure.is_some() {
            return None;
        }
        match self.phase.as_ref()? {
            Phase::Framing(child) => child.value(),
            Phase::Metadata(_) => None,
        }
    }
    pub fn poll(&mut self, now: Tick) -> Result<Status, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self.step(now);
        self.outcome(result)
    }
    fn step(&mut self, now: Tick) -> Result<Status, Error> {
        match self.phase.as_mut().ok_or(Error::InvalidState)? {
            Phase::Framing(child) => return child.poll(now),
            Phase::Metadata(child) => {
                if child.poll(now)? != Status::Complete {
                    return Ok(Status::Yield);
                }
            }
        }
        let Some(Phase::Metadata(child)) = self.phase.take() else {
            return Err(Error::InvalidState);
        };
        let output = self.output.take().ok_or(Error::InvalidState)?;
        self.phase = Some(Phase::Framing(retained::Cursor::new(child, output, now)?));
        Ok(Status::Yield)
    }
    /// Store only a fresh whole retained fragment; no detached passive evidence enters.
    pub fn finish(mut self, now: Tick) -> Result<(), Error> {
        self.check_deadline(now)?;
        let result = match self.phase.take() {
            Some(Phase::Framing(child)) => child.finish(now),
            _ => return self.outcome(Err(Error::InvalidState)),
        };
        let (retained, work, budget, _) = self.outcome(result)?;
        let result = budget
            .charge(work, now, 0, 1, &mut 0)
            .map_err(Error::Admission);
        self.outcome(result)?;
        let result = work
            .charge(
                now,
                Charge {
                    records: 1,
                    ..Charge::default()
                },
            )
            .map_err(|e| Error::Admission(crate::nfc::Error::Work(e)));
        self.outcome(result)?;
        *self.slot = Some(retained);
        *self.parent_failure = None;
        Ok(())
    }
}
/// Exclusive proof that every original ordinal was retained, still unpublished.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mta::mime_traversal::bound::ordered::body_lists::response::collected::Serialized<'_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mta::mime_traversal::bound::ordered::body_lists::response::collected::Serialized<'_, '_, '_, '_, '_>>();
/// ```
pub struct Serialized<'a, 'w, 'n, 'c, 'o> {
    projected: Projected<'a, 'w, 'n>,
    cells: &'c [Cell<'o>],
}
impl<'w, 'n, 'c, 'o> Serialized<'_, 'w, 'n, 'c, 'o> {
    pub fn value(&self) -> Option<View<'_, '_, '_, 'o>> {
        Some(View {
            selected: self.projected.value()?,
            fragments: self.cells,
        })
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        self.projected.check_deadline(now)
    }
    pub fn finish(
        self,
        now: Tick,
    ) -> Result<(View<'w, 'n, 'c, 'o>, &'w mut Meter, &'w mut HeaderBudget), Error> {
        let (selected, work, budget) = self.projected.finish(now)?;
        Ok((
            View {
                selected,
                fragments: self.cells,
            },
            work,
            budget,
        ))
    }
}
const _: () = assert!(
    std::mem::size_of::<Collecting<'_, '_, '_, '_, '_>>()
        + std::mem::size_of::<Child<'_, '_, '_>>()
        + std::mem::size_of::<HeaderBudget>()
        <= 8 * 1024
);
const _: () = assert!(std::mem::size_of::<Cell<'_>>() <= 128);
const _: () = assert!(std::mem::size_of::<Serialized<'_, '_, '_, '_, '_>>() <= 256);
#[cfg(test)]
mod tests;

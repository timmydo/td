//! Source-bound first-valid selection and retained optional location JSON.
use super::{Cursor as Selector, Input, Selection};
use crate::{
    admission::work::Meter,
    mime_location_field::retained,
    nfc::{self, HeaderBudget},
    ports::Tick,
};
pub struct Retained<'w> {
    pub selection: Selection,
    pub value: Option<&'w [u8]>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Selection(super::Error),
    Retention(retained::Error),
    Admission(nfc::Error),
    InvalidState,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Selection(error) => write!(f, "source-bound location selection: {error}"),
            Self::Retention(error) => write!(f, "source-bound location retention: {error}"),
            Self::Admission(error) => write!(f, "source-bound location admission: {error}"),
            Self::InvalidState => f.write_str("invalid source-bound location state"),
        }
    }
}
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Selection(error) => Some(error),
            Self::Retention(error) => Some(error),
            Self::Admission(error) => Some(error),
            Self::InvalidState => None,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Yield,
    Complete,
}
// Only one inline child or original pair exists at a time.
#[allow(clippy::large_enum_variant)]
enum Owner<'a, 'w> {
    Selection(Selector<'a, 'w>),
    Retention(retained::Cursor<'a, 'w>),
    Complete(&'w mut Meter, &'w mut HeaderBudget),
    Retired,
}
/// Bind one immutable authorized resident input and distinct reserved backing.
/// Only that source's selected extent is replayed; callers cannot substitute a
/// slice between phases. Discovery and projection retain their original costs.
/// Views remain provisional through whole-job fresh publication admission.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mta::mime_location_fields::json::Cursor<'_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mta::mime_location_fields::json::Cursor<'_, '_>>();
/// ```
pub struct Cursor<'a, 'w> {
    input: Input<'a>,
    backing: Option<&'w mut [u8]>,
    owner: Owner<'a, 'w>,
    selection: Option<Selection>,
    value: Option<&'w [u8]>,
    failure: Option<Error>,
}
impl<'a, 'w> Cursor<'a, 'w> {
    pub fn new(
        input: Input<'a>,
        backing: &'w mut [u8],
        work: &'w mut Meter,
        budget: &'w mut HeaderBudget,
    ) -> Result<Self, Error> {
        let owner = Owner::Selection(Selector::new(input, work, budget).map_err(Error::Selection)?);
        Ok(Self {
            input,
            backing: Some(backing),
            owner,
            selection: None,
            value: None,
            failure: None,
        })
    }
    pub fn is_complete(&self) -> bool {
        self.failure.is_none()
            && matches!(self.owner, Owner::Complete(..))
            && self
                .selection
                .is_some_and(|selected| selected.content_location.is_some() == self.value.is_some())
    }
    /// Whole passive fragments; source/publication authority stays external.
    pub fn view(&self) -> Option<Retained<'_>> {
        if !self.is_complete() {
            return None;
        }
        Some(Retained {
            selection: self.selection?,
            value: self.value,
        })
    }
    fn outcome<T>(&mut self, result: Result<T, Error>) -> Result<T, Error> {
        if let Err(error) = result {
            self.failure = Some(error);
            self.selection = None;
            self.value = None;
            self.owner = Owner::Retired;
            self.backing = None;
        }
        result
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = match &mut self.owner {
            Owner::Selection(cursor) => cursor.check_deadline(now).map_err(Error::Selection),
            Owner::Retention(cursor) => cursor.check_deadline(now).map_err(Error::Retention),
            Owner::Complete(work, budget) => budget
                .charge(work, now, 0, 0, &mut 0)
                .map_err(Error::Admission),
            Owner::Retired => Err(Error::InvalidState),
        };
        self.outcome(result)
    }
    pub fn poll(&mut self, now: Tick) -> Result<Status, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if self.is_complete() {
            return Ok(Status::Complete);
        }
        self.check_deadline(now)?;
        let result = self.step(now);
        self.outcome(result)
    }
    fn step(&mut self, now: Tick) -> Result<Status, Error> {
        match &mut self.owner {
            Owner::Selection(cursor) => {
                if cursor.poll(now).map_err(Error::Selection)? == super::Status::Complete {
                    let Owner::Selection(cursor) =
                        std::mem::replace(&mut self.owner, Owner::Retired)
                    else {
                        return Err(Error::InvalidState);
                    };
                    let (work, budget, selected) = cursor.finish(now).map_err(Error::Selection)?;
                    if selected.content_location.is_some() != selected.location_end.is_some() {
                        return Err(Error::InvalidState);
                    }
                    let source = match selected.content_location {
                        Some(field) => Some(
                            td_header::resident::slice(
                                self.input.source,
                                self.input.base,
                                field.value_start..field.value_end,
                            )
                            .ok_or(Error::InvalidState)?,
                        ),
                        None => None,
                    };
                    let backing = self.backing.take().ok_or(Error::InvalidState)?;
                    self.selection = Some(selected);
                    self.owner =
                        Owner::Retention(retained::Cursor::new(source, backing, work, budget));
                }
                Ok(Status::Yield)
            }
            Owner::Retention(cursor) => {
                if cursor.poll(now).map_err(Error::Retention)? == retained::Status::Complete {
                    let Owner::Retention(cursor) =
                        std::mem::replace(&mut self.owner, Owner::Retired)
                    else {
                        return Err(Error::InvalidState);
                    };
                    let (retained, work, budget) = cursor.finish(now).map_err(Error::Retention)?;
                    let selected = self.selection.ok_or(Error::InvalidState)?;
                    if retained.end != selected.location_end
                        || retained.value.is_some() != selected.content_location.is_some()
                    {
                        return Err(Error::InvalidState);
                    }
                    self.value = retained.value;
                    self.owner = Owner::Complete(work, budget);
                    Ok(Status::Complete)
                } else {
                    Ok(Status::Yield)
                }
            }
            Owner::Complete(..) | Owner::Retired => Err(Error::InvalidState),
        }
    }
    pub fn finish(
        mut self,
        now: Tick,
    ) -> Result<(Retained<'w>, &'w mut Meter, &'w mut HeaderBudget), Error> {
        self.check_deadline(now)?;
        if !self.is_complete() {
            return Err(Error::InvalidState);
        }
        let Owner::Complete(work, budget) = self.owner else {
            return Err(Error::InvalidState);
        };
        Ok((
            Retained {
                selection: self.selection.ok_or(Error::InvalidState)?,
                value: self.value,
            },
            work,
            budget,
        ))
    }
}
const _: () =
    assert!(std::mem::size_of::<Cursor<'_, '_>>() + std::mem::size_of::<HeaderBudget>() <= 1536);

#[cfg(test)]
mod tests;

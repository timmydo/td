//! Retain one optional authorized location JSON value in caller-reserved backing.
use super::json;
use crate::{
    admission::work::Meter,
    nfc::{self, HeaderBudget},
    ports::Tick,
};
pub use json::End;
/// Conservative retained JSON capacity from complete raw field length.
/// Literal URI bytes are ASCII and JSON escaping needs at most six bytes each.
/// Q/B decoding produces no more octets than its original wire spelling. Every
/// supported charset maps a decoded octet to at most three UTF-8 bytes (including
/// repairs); UTF-8 copies or repairs under the same bound. Encoded controls drop,
/// while quoted/backslash ASCII doubles and other scalars copy as UTF-8. Thus
/// projected wire length is at most six times complete raw length, plus quotes.
/// Sizing grants no validity, source, work or reserved output allowance.
pub const fn capacity_bound(raw_bytes: usize) -> Option<usize> {
    td_json::string::capacity_bound(raw_bytes)
}
pub struct Retained<'w> {
    pub value: Option<&'w [u8]>,
    pub end: Option<End>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Projection(json::Error),
    Admission(nfc::Error),
    OutputCapacity,
    InvalidState,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Projection(error) => write!(f, "retained location projection: {error}"),
            Self::Admission(error) => write!(f, "retained location admission: {error}"),
            Self::OutputCapacity => f.write_str("retained location JSON capacity"),
            Self::InvalidState => f.write_str("invalid retained location JSON state"),
        }
    }
}
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Projection(error) => Some(error),
            Self::Admission(error) => Some(error),
            _ => None,
        }
    }
}
fn window_error(error: td_json::retain::Error) -> Error {
    match error {
        td_json::retain::Error::Capacity => Error::OutputCapacity,
        td_json::retain::Error::InvalidState => Error::InvalidState,
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Yield,
    Complete,
}
#[derive(Clone, Copy)]
enum Phase {
    Start,
    Read,
    Complete,
}
// Inline exclusive children keep original owners and allocation-free progress.
#[allow(clippy::large_enum_variant)]
enum Owner<'a, 'w> {
    Budgets(&'w mut Meter, &'w mut HeaderBudget),
    Field(json::Cursor<'a, 'w>),
    Retired,
}
/// Optional caller-selected complete field and a distinct reserved output window.
/// Missing values remain absent; a present empty reference produces an empty
/// JSON string. No discovery, null mapping, label matching or publication grant.
/// Every byte/view stays provisional through whole completion and fresh finish.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mta::mime_location_field::retained::Cursor<'_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mta::mime_location_field::retained::Cursor<'_, '_>>();
/// ```
/// The shared backing owner is also exclusive (tested here because td-json's
/// standalone build disables doctests).
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_json::retain::Window<'_>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_json::retain::Window<'_>>();
/// ```
pub struct Cursor<'a, 'w> {
    source: Option<&'a [u8]>,
    window: td_json::retain::Window<'w>,
    owner: Owner<'a, 'w>,
    phase: Phase,
    end: Option<End>,
    failure: Option<Error>,
}
impl<'a, 'w> Cursor<'a, 'w> {
    pub const fn new(
        source: Option<&'a [u8]>,
        backing: &'w mut [u8],
        work: &'w mut Meter,
        budget: &'w mut HeaderBudget,
    ) -> Self {
        Self {
            source,
            window: td_json::retain::Window::new(backing),
            owner: Owner::Budgets(work, budget),
            phase: Phase::Start,
            end: None,
            failure: None,
        }
    }
    pub fn is_complete(&self) -> bool {
        self.failure.is_none()
            && matches!(self.phase, Phase::Complete)
            && matches!(self.owner, Owner::Budgets(..))
            && (self.source.is_none() || self.end.is_some())
    }
    /// Passive view; whole-job final publication admission remains external.
    pub fn view(&self) -> Option<Retained<'_>> {
        if !self.is_complete() {
            return None;
        }
        Some(Retained {
            value: if self.source.is_some() {
                Some(self.window.provisional()?)
            } else {
                None
            },
            end: self.end,
        })
    }
    fn outcome<T>(&mut self, result: Result<T, Error>) -> Result<T, Error> {
        if let Err(error) = result {
            self.failure = Some(error);
            self.end = None;
            self.owner = Owner::Retired;
        }
        result
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = match &mut self.owner {
            Owner::Budgets(work, budget) => budget
                .charge(work, now, 0, 0, &mut 0)
                .map_err(Error::Admission),
            Owner::Field(cursor) => cursor.check_deadline(now).map_err(Error::Projection),
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
        match self.phase {
            Phase::Start => {
                if let Some(source) = self.source {
                    let Owner::Budgets(work, budget) =
                        std::mem::replace(&mut self.owner, Owner::Retired)
                    else {
                        return Err(Error::InvalidState);
                    };
                    self.owner = Owner::Field(json::Cursor::new(source, work, budget));
                    self.phase = Phase::Read;
                    Ok(Status::Yield)
                } else {
                    self.phase = Phase::Complete;
                    Ok(Status::Complete)
                }
            }
            Phase::Read => {
                let Owner::Field(cursor) = &mut self.owner else {
                    return Err(Error::InvalidState);
                };
                let progress = cursor
                    .poll(now, self.window.tail().map_err(window_error)?)
                    .map_err(Error::Projection)?;
                self.window
                    .advance(progress.written)
                    .map_err(window_error)?;
                if progress.status == json::Status::NeedOutput {
                    return Err(Error::InvalidState);
                }
                if progress.status == json::Status::Complete {
                    let Owner::Field(cursor) = std::mem::replace(&mut self.owner, Owner::Retired)
                    else {
                        return Err(Error::InvalidState);
                    };
                    let (work, budget, end) = cursor.finish(now).map_err(Error::Projection)?;
                    self.owner = Owner::Budgets(work, budget);
                    self.end = Some(end);
                    self.phase = Phase::Complete;
                    Ok(Status::Complete)
                } else {
                    Ok(Status::Yield)
                }
            }
            Phase::Complete => Err(Error::InvalidState),
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
        let Owner::Budgets(work, budget) = self.owner else {
            return Err(Error::InvalidState);
        };
        let value = if self.source.is_some() {
            Some(self.window.into_slice().map_err(window_error)?)
        } else {
            None
        };
        Ok((
            Retained {
                value,
                end: self.end,
            },
            work,
            budget,
        ))
    }
}
const _: () =
    assert!(std::mem::size_of::<Cursor<'_, '_>>() + std::mem::size_of::<HeaderBudget>() <= 1024);
#[cfg(test)]
mod tests;

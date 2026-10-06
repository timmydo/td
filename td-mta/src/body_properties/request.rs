//! Decode and validate one admitted bodyProperties argument without copying keys.
use super::{json, Cell, Fields, View};
use crate::{admission::work::Meter, ports::Tick};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Json(json::Error),
    Selection(super::Error),
    InvalidState,
}
impl From<json::Error> for Error {
    fn from(error: json::Error) -> Self {
        Self::Json(error)
    }
}
impl From<super::Error> for Error {
    fn from(error: super::Error) -> Self {
        Self::Selection(error)
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Json(error) => write!(f, "body properties request: {error}"),
            Self::Selection(error) => write!(f, "body properties request: {error}"),
            Self::InvalidState => f.write_str("invalid body properties request state"),
        }
    }
}
impl std::error::Error for Error {}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Yield,
    Complete,
}
enum Phase<'i, 'c, 'k, 'm, 'h> {
    Default,
    Json(json::Cursor<'i, 'c, 'k, 'm>),
    Selection(super::Cursor<'k, 'm, 'h>),
}
/// Caller storage is prepared/admitted outside this passive request cursor.
/// ```compile_fail,E0277
/// fn required<T: Copy>() {} required::<td_mta::body_properties::request::Cursor<'_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn required<T: Clone>() {} required::<td_mta::body_properties::request::Cursor<'_, '_, '_, '_, '_>>();
/// ```
pub struct Cursor<'i, 'c, 'k, 'm, 'h> {
    phase: Option<Phase<'i, 'c, 'k, 'm, 'h>>,
    headers: Option<&'h mut [Cell<'m>]>,
    failure: Option<Error>,
}
impl<'i, 'c, 'k, 'm, 'h> Cursor<'i, 'c, 'k, 'm, 'h> {
    pub fn new(
        source: Option<&'i str>,
        text: &'c mut [json::Cell<'m>],
        keys: &'k mut [&'m str],
        headers: &'h mut [Cell<'m>],
    ) -> Self {
        Self {
            phase: Some(if source.is_none() {
                Phase::Default
            } else {
                Phase::Json(json::Cursor::new(source, text, keys))
            }),
            headers: Some(headers),
            failure: None,
        }
    }
    pub fn value(&self) -> Option<View<'_, 'm>> {
        if self.failure.is_some() {
            return None;
        }
        match self.phase.as_ref()? {
            Phase::Default => Some(View {
                fields: Fields::DEFAULT,
                headers: self.headers.as_ref()?.get(..0)?,
            }),
            Phase::Selection(cursor) => cursor.value(),
            Phase::Json(_) => None,
        }
    }
    pub fn finish(self) -> Result<View<'h, 'm>, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        match self.phase.ok_or(Error::InvalidState)? {
            Phase::Default => Ok(View {
                fields: Fields::DEFAULT,
                headers: self
                    .headers
                    .ok_or(Error::InvalidState)?
                    .get(..0)
                    .ok_or(Error::InvalidState)?,
            }),
            Phase::Selection(cursor) => match cursor.finish() {
                Ok(view) => Ok(view),
                Err(super::Error::InvalidState) => Err(Error::InvalidState),
                Err(error) => Err(Error::Selection(error)),
            },
            Phase::Json(_) => Err(Error::InvalidState),
        }
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
    fn step(&mut self, now: Tick, work: &mut Meter) -> Result<Status, Error> {
        match self.phase.as_mut().ok_or(Error::InvalidState)? {
            Phase::Default => Ok(Status::Complete),
            Phase::Selection(cursor) => Ok(match cursor.poll(now, work)? {
                super::Status::Yield => Status::Yield,
                super::Status::Complete => Status::Complete,
            }),
            Phase::Json(cursor) => match cursor.poll(now, work)? {
                json::Status::Yield => Ok(Status::Yield),
                json::Status::Complete => {
                    let Some(Phase::Json(cursor)) = self.phase.take() else {
                        return Err(Error::InvalidState);
                    };
                    let argument = cursor.finish()?;
                    match argument {
                        json::Argument::Default => Err(Error::InvalidState),
                        json::Argument::Explicit(keys) => {
                            let headers = self.headers.take().ok_or(Error::InvalidState)?;
                            let cursor = super::Cursor::new(keys, headers);
                            let status = if cursor.value().is_some() {
                                Status::Complete
                            } else {
                                Status::Yield
                            };
                            self.phase = Some(Phase::Selection(cursor));
                            Ok(status)
                        }
                    }
                }
            },
        }
    }
}
const _: () = assert!(std::mem::size_of::<Cursor<'_, '_, '_, '_, '_>>() <= 512);
#[cfg(test)]
pub use tests::probe as probe_allocations;
#[cfg(test)]
#[path = "request/tests.rs"]
mod tests;

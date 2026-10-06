//! Selected display-name validation and NFC under the original email budgets.
use crate::{
    decode_work::{Error as DecodeError, Parsing},
    header_comment, header_phrase,
    nfc::{self, HeaderBudget, Scratch},
    time::Tick,
    work::{Charge, Meter},
};
pub use crate::{header_message_ids::Extent, nfc::Status};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    Phrase,
    Comment,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Phrase(header_phrase::Error),
    Comment(header_comment::Error),
    Normalize(nfc::Error),
    InvalidState,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Phrase(error) => write!(f, "display-name phrase: {error}"),
            Self::Comment(error) => write!(f, "display-name comment: {error}"),
            Self::Normalize(error) => write!(f, "display-name normalization: {error}"),
            Self::InvalidState => f.write_str("invalid display-name state"),
        }
    }
}
impl std::error::Error for Error {}
enum Grammar<'a> {
    Phrase(header_phrase::Cursor<'a>),
    Comment(header_comment::Cursor<'a>),
}
impl Grammar<'_> {
    fn error(&self, error: DecodeError) -> Error {
        match self {
            Self::Phrase(_) => Error::Phrase(error.into()),
            Self::Comment(_) => Error::Comment(error.into()),
        }
    }
}
// Inline state keeps validation-to-normalization handoff allocation-free.
#[allow(clippy::large_enum_variant)]
enum Owner<'a, 'w> {
    Validate {
        grammar: Grammar<'a>,
        work: &'w mut Meter,
        budget: &'w mut HeaderBudget,
        scratch: &'w mut Scratch,
        credit: u8,
    },
    Normalize(nfc::Cursor<'a, 'w>),
    Retired,
}
/// The caller selects a display-name extent within one admitted field value.
/// All scalars and diagnostics remain provisional through Complete.
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {}
/// cloned::<td_mime::header_name::Cursor<'_, '_>>();
/// ```
pub struct Cursor<'a, 'w> {
    field: &'a [u8],
    extent: Extent,
    owner: Owner<'a, 'w>,
    complete: bool,
    failure: Option<Error>,
}
impl<'a, 'w> Cursor<'a, 'w> {
    #[cfg(test)]
    pub(crate) fn remaining(&self) -> Option<(Charge, u64)> {
        match &self.owner {
            Owner::Validate { work, budget, .. } => {
                Some((work.remaining(), budget.steps_remaining()))
            }
            Owner::Normalize(cursor) => Some(cursor.remaining()),
            Owner::Retired => None,
        }
    }

    pub fn new(
        field: &'a [u8],
        extent: Extent,
        kind: Kind,
        work: &'w mut Meter,
        budget: &'w mut HeaderBudget,
        scratch: &'w mut Scratch,
    ) -> Result<Self, Error> {
        let source = field
            .get(extent.start..extent.end)
            .ok_or(Error::InvalidState)?;
        let grammar = match kind {
            Kind::Phrase => Grammar::Phrase(header_phrase::Cursor::new(source)),
            Kind::Comment => Grammar::Comment(header_comment::Cursor::new(source)),
        };
        Ok(Self {
            field,
            extent,
            owner: Owner::Validate {
                grammar,
                work,
                budget,
                scratch,
                credit: 0,
            },
            complete: false,
            failure: None,
        })
    }
    pub const fn is_encoding_problem(&self) -> bool {
        match &self.owner {
            Owner::Normalize(cursor) => cursor.is_encoding_problem(),
            _ => false,
        }
    }
    pub(crate) fn finish(
        self,
    ) -> Result<(&'w mut Meter, &'w mut HeaderBudget, &'w mut Scratch), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if !self.complete {
            return Err(Error::InvalidState);
        }
        let Owner::Normalize(cursor) = self.owner else {
            return Err(Error::InvalidState);
        };
        cursor.finish().map_err(Error::Normalize)
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        self.charge_output(now, 0)
    }
    pub(crate) fn charge_output(&mut self, now: Tick, bytes: u64) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = match &mut self.owner {
            Owner::Validate {
                grammar,
                work,
                budget,
                credit,
                ..
            } => budget
                .charge_local(work, now, 0, 0, credit)
                .and_then(|()| {
                    work.charge(
                        now,
                        Charge {
                            output_bytes: bytes,
                            ..Charge::default()
                        },
                    )
                    .map_err(nfc::Error::Work)
                })
                .map_err(DecodeError::from)
                .map_err(|error| grammar.error(error)),
            Owner::Normalize(cursor) => cursor.charge_output(now, bytes).map_err(Error::Normalize),
            Owner::Retired => Err(Error::InvalidState),
        };
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    pub fn poll(&mut self, now: Tick) -> Result<Status, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if self.complete {
            return Ok(Status::Complete);
        }
        self.check_deadline(now)?;
        let result = self.step(now);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    fn step(&mut self, now: Tick) -> Result<Status, Error> {
        match &mut self.owner {
            Owner::Validate {
                grammar,
                work,
                budget,
                credit,
                ..
            } => {
                let mut charged = Parsing::new(work, budget, credit);
                let complete = match grammar {
                    Grammar::Phrase(cursor) => matches!(
                        cursor
                            .poll_with_work(now, &mut charged)
                            .map_err(Error::Phrase)?,
                        header_phrase::Status::Complete(_)
                    ),
                    Grammar::Comment(cursor) => {
                        cursor
                            .poll_with_work(now, &mut charged)
                            .map_err(Error::Comment)?
                            == header_comment::Status::Complete
                    }
                };
                if complete {
                    self.normalize()?;
                }
                Ok(Status::Yield)
            }
            Owner::Normalize(cursor) => {
                let status = cursor.poll(now).map_err(Error::Normalize)?;
                self.complete = status == Status::Complete;
                Ok(status)
            }
            Owner::Retired => Err(Error::InvalidState),
        }
    }
    fn normalize(&mut self) -> Result<(), Error> {
        let Owner::Validate {
            grammar,
            work,
            budget,
            scratch,
            ..
        } = std::mem::replace(&mut self.owner, Owner::Retired)
        else {
            return Err(Error::InvalidState);
        };
        let cursor = match grammar {
            Grammar::Phrase(cursor) => nfc::Cursor::from_phrase(
                cursor.into_validated().map_err(Error::Phrase)?,
                self.field,
                self.extent,
                scratch,
                work,
                budget,
            )
            .map_err(Error::Normalize)?,
            Grammar::Comment(cursor) => nfc::Cursor::from_comment(
                cursor.into_validated().map_err(Error::Comment)?,
                scratch,
                work,
                budget,
            ),
        };
        self.owner = Owner::Normalize(cursor);
        Ok(())
    }
}

#[cfg(test)]
#[path = "header_name/tests.rs"]
mod tests;

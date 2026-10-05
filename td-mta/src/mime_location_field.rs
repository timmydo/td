//! Complete authorized field spelling with whole-word or literal projection.
//! Field discovery, presence, label matching and publication remain external.
use crate::{
    admission::work::{Charge, Meter},
    mime_location_literal as literal, mime_location_selection as selection,
    mime_location_word as word,
    nfc::{self, HeaderBudget},
    ports::Tick,
};
pub use selection::Spelling;
pub mod json;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Selection(selection::Error),
    Words(word::Error),
    Literal(literal::Error),
    Admission(nfc::Error),
    InvalidState,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Selection(error) => write!(f, "location selection: {error}"),
            Self::Words(error) => write!(f, "location words: {error}"),
            Self::Literal(error) => write!(f, "location literal: {error}"),
            Self::Admission(error) => write!(f, "location admission: {error}"),
            Self::InvalidState => f.write_str("invalid location field state"),
        }
    }
}
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Selection(error) => Some(error),
            Self::Words(error) => Some(error),
            Self::Literal(error) => Some(error),
            Self::Admission(error) => Some(error),
            Self::InvalidState => None,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Yield,
    /// Provisional through whole completion and fresh original admission.
    Scalar(char),
    Complete,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct End {
    pub spelling: Spelling,
    pub encoded_words: bool,
    pub encoding_problem: bool,
}
enum Owner<'a, 'w> {
    Selection(selection::Cursor<'a, 'w>),
    Words(word::Cursor<'a, 'w>),
    Literal(literal::Cursor<'a, 'w>),
    Complete(&'w mut Meter, &'w mut HeaderBudget),
    Retired,
}
/// Caller authorizes surrounding CFWS and whole-spelling encoded-word placement.
/// Only a complete recognized word run decodes; other spellings replay literally
/// after complete URI-reference validation. Empty references remain permitted.
/// Source excludes the final header ending. Decoded labels receive no URI check,
/// percent decoding, normalization, resolution or retained-output authority.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mta::mime_location_field::Cursor<'_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mta::mime_location_field::Cursor<'_, '_>>();
/// ```
pub struct Cursor<'a, 'w> {
    source: &'a [u8],
    owner: Owner<'a, 'w>,
    spelling: Option<Spelling>,
    end: Option<End>,
    failure: Option<Error>,
}
impl<'a, 'w> Cursor<'a, 'w> {
    pub const fn new(source: &'a [u8], work: &'w mut Meter, budget: &'w mut HeaderBudget) -> Self {
        Self {
            source,
            owner: Owner::Selection(selection::Cursor::new(source, work, budget)),
            spelling: None,
            end: None,
            failure: None,
        }
    }
    pub fn is_complete(&self) -> bool {
        self.failure.is_none() && self.end.is_some() && matches!(self.owner, Owner::Complete(..))
    }
    pub fn end(&self) -> Option<End> {
        if self.is_complete() {
            self.end
        } else {
            None
        }
    }
    fn outcome<T>(&mut self, result: Result<T, Error>) -> Result<T, Error> {
        if let Err(error) = result {
            self.failure = Some(error);
            self.end = None;
            self.spelling = None;
            self.owner = Owner::Retired;
        }
        result
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = match &mut self.owner {
            Owner::Selection(cursor) => cursor.check_deadline(now).map_err(Error::Selection),
            Owner::Words(cursor) => cursor.check_deadline(now).map_err(Error::Words),
            Owner::Literal(cursor) => cursor.check_deadline(now).map_err(Error::Literal),
            Owner::Complete(work, budget) => budget
                .charge(work, now, 0, 0, &mut 0)
                .map_err(Error::Admission),
            Owner::Retired => Err(Error::InvalidState),
        };
        self.outcome(result)
    }
    #[cfg(test)]
    fn remaining(&self) -> Option<(Charge, u64, u64)> {
        match &self.owner {
            Owner::Selection(cursor) => Some(cursor.remaining()),
            Owner::Words(cursor) => Some(cursor.remaining()),
            Owner::Literal(cursor) => Some(cursor.remaining()),
            Owner::Complete(work, budget) => Some((
                work.remaining(),
                budget.source_bytes_remaining(),
                budget.steps_remaining(),
            )),
            Owner::Retired => None,
        }
    }
    fn charge_output(&mut self, now: Tick, bytes: u64) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = match &mut self.owner {
            Owner::Selection(cursor) => cursor.charge_output(now, bytes).map_err(Error::Selection),
            Owner::Words(cursor) => cursor.charge_output(now, bytes).map_err(Error::Words),
            Owner::Literal(cursor) => cursor.charge_output(now, bytes).map_err(Error::Literal),
            Owner::Complete(work, budget) => budget
                .charge(work, now, 0, 0, &mut 0)
                .map_err(Error::Admission)
                .and_then(|()| {
                    work.charge(
                        now,
                        Charge {
                            output_bytes: bytes,
                            ..Charge::default()
                        },
                    )
                    .map_err(|error| Error::Admission(nfc::Error::Work(error)))
                }),
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
        let result = self.step(now);
        self.outcome(result)
    }
    fn selected(&self) -> Result<&'a [u8], Error> {
        let spelling = self.spelling.ok_or(Error::InvalidState)?;
        self.source
            .get(spelling.start..spelling.end)
            .ok_or(Error::InvalidState)
    }
    fn step(&mut self, now: Tick) -> Result<Status, Error> {
        match &mut self.owner {
            Owner::Selection(cursor) => match cursor.poll(now).map_err(Error::Selection)? {
                selection::Status::Yield => Ok(Status::Yield),
                selection::Status::Complete(_) => {
                    let Owner::Selection(cursor) =
                        std::mem::replace(&mut self.owner, Owner::Retired)
                    else {
                        return Err(Error::InvalidState);
                    };
                    let (work, budget, spelling) = cursor.finish(now).map_err(Error::Selection)?;
                    self.spelling = Some(spelling);
                    self.owner =
                        Owner::Words(word::Cursor::new_run(self.selected()?, work, budget));
                    Ok(Status::Yield)
                }
            },
            Owner::Words(cursor) => match cursor.poll(now).map_err(Error::Words)? {
                word::Status::Yield => Ok(Status::Yield),
                word::Status::Scalar(value) => Ok(Status::Scalar(value)),
                word::Status::Complete => {
                    let Owner::Words(cursor) = std::mem::replace(&mut self.owner, Owner::Retired)
                    else {
                        return Err(Error::InvalidState);
                    };
                    let (work, budget, end) = cursor.finish(now).map_err(Error::Words)?;
                    if end.recognized {
                        self.end = Some(End {
                            spelling: self.spelling.ok_or(Error::InvalidState)?,
                            encoded_words: true,
                            encoding_problem: end.encoding_problem,
                        });
                        self.owner = Owner::Complete(work, budget);
                        Ok(Status::Complete)
                    } else {
                        self.owner =
                            Owner::Literal(literal::Cursor::new(self.selected()?, work, budget));
                        Ok(Status::Yield)
                    }
                }
            },
            Owner::Literal(cursor) => match cursor.poll(now).map_err(Error::Literal)? {
                literal::Status::Yield => Ok(Status::Yield),
                literal::Status::Octet { byte, .. } => Ok(Status::Scalar(char::from(byte))),
                literal::Status::Complete => {
                    let Owner::Literal(cursor) = std::mem::replace(&mut self.owner, Owner::Retired)
                    else {
                        return Err(Error::InvalidState);
                    };
                    let (work, budget) = cursor.finish(now).map_err(Error::Literal)?;
                    self.end = Some(End {
                        spelling: self.spelling.ok_or(Error::InvalidState)?,
                        encoded_words: false,
                        encoding_problem: false,
                    });
                    self.owner = Owner::Complete(work, budget);
                    Ok(Status::Complete)
                }
            },
            Owner::Complete(..) | Owner::Retired => Err(Error::InvalidState),
        }
    }
    pub fn finish(
        mut self,
        now: Tick,
    ) -> Result<(&'w mut Meter, &'w mut HeaderBudget, End), Error> {
        self.check_deadline(now)?;
        let end = self.end().ok_or(Error::InvalidState)?;
        let Owner::Complete(work, budget) = self.owner else {
            return Err(Error::InvalidState);
        };
        Ok((work, budget, end))
    }
}
const _: () =
    assert!(std::mem::size_of::<Cursor<'_, '_>>() + std::mem::size_of::<HeaderBudget>() <= 640);

#[cfg(test)]
mod tests;

//! Complete field-value boundary selection and literal URI-reference projection.
//! Encoded-word placement/path choice, field discovery and publication are external.
use crate::{
    location_literal as literal, location_selection as selection,
    nfc::HeaderBudget,
    time::Tick,
    work::{Meter, Stop},
};
pub use selection::Spelling;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    MalformedCfws,
    NestingLimit,
    MalformedUri,
    MalformedFold,
    Work(Stop),
    InterpretationLimit,
    InvalidState,
}
impl From<selection::Error> for Error {
    fn from(error: selection::Error) -> Self {
        match error {
            selection::Error::Malformed => Self::MalformedCfws,
            selection::Error::NestingLimit => Self::NestingLimit,
            selection::Error::Work(stop) => Self::Work(stop),
            selection::Error::InterpretationLimit => Self::InterpretationLimit,
            selection::Error::InvalidState => Self::InvalidState,
        }
    }
}
impl From<literal::Error> for Error {
    fn from(error: literal::Error) -> Self {
        match error {
            literal::Error::MalformedUri => Self::MalformedUri,
            literal::Error::MalformedFold => Self::MalformedFold,
            literal::Error::Work(stop) => Self::Work(stop),
            literal::Error::InterpretationLimit => Self::InterpretationLimit,
            literal::Error::InvalidState => Self::InvalidState,
        }
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MalformedCfws => f.write_str("malformed literal location CFWS"),
            Self::NestingLimit => f.write_str("literal location comment nesting limit"),
            Self::MalformedUri => f.write_str("malformed literal location URI"),
            Self::MalformedFold => f.write_str("malformed literal location fold"),
            Self::Work(stop) => write!(f, "literal location work: {stop}"),
            Self::InterpretationLimit => f.write_str("literal location interpretation limit"),
            Self::InvalidState => f.write_str("invalid literal location field state"),
        }
    }
}
impl std::error::Error for Error {}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Yield,
    /// Literal ASCII and position in the original supplied field-value slice.
    /// Provisional through healthy completion and fresh original admission.
    Octet {
        byte: u8,
        position: usize,
    },
    Complete,
}
enum Phase<'a, 'w> {
    Select(selection::Cursor<'a, 'w>),
    Literal(literal::Cursor<'a, 'w>),
    Empty,
}
/// Caller authorizes surrounding CFWS and chooses literal interpretation.
/// Word-looking text stays literal; encoded-word placement/path choice is external.
/// Source excludes the final header ending. Empty URI references remain permitted.
/// No percent decoding, normalization, resolution or retained metadata authority.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mime::location_literal_field::Cursor<'_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mime::location_literal_field::Cursor<'_, '_>>();
/// ```
pub struct Cursor<'a, 'w> {
    source: &'a [u8],
    phase: Phase<'a, 'w>,
    spelling: Option<Spelling>,
    failure: Option<Error>,
}
impl<'a, 'w> Cursor<'a, 'w> {
    pub const fn new(source: &'a [u8], work: &'w mut Meter, budget: &'w mut HeaderBudget) -> Self {
        Self {
            source,
            phase: Phase::Select(selection::Cursor::new(source, work, budget)),
            spelling: None,
            failure: None,
        }
    }
    pub fn is_complete(&self) -> bool {
        self.failure.is_none()
            && matches!(&self.phase, Phase::Literal(cursor) if cursor.is_complete())
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
        let result = match &mut self.phase {
            Phase::Select(cursor) => cursor.check_deadline(now).map_err(Error::from),
            Phase::Literal(cursor) => cursor.check_deadline(now).map_err(Error::from),
            Phase::Empty => Err(Error::InvalidState),
        };
        self.outcome(result)
    }
    pub fn poll(&mut self, now: Tick) -> Result<Status, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self.step(now);
        self.outcome(result)
    }
    fn step(&mut self, now: Tick) -> Result<Status, Error> {
        match &mut self.phase {
            Phase::Select(cursor) => match cursor.poll(now)? {
                selection::Status::Yield => Ok(Status::Yield),
                selection::Status::Complete(_) => {
                    let Phase::Select(cursor) = std::mem::replace(&mut self.phase, Phase::Empty)
                    else {
                        return Err(Error::InvalidState);
                    };
                    let (work, budget, spelling) = cursor.finish(now)?;
                    let source = self
                        .source
                        .get(spelling.start..spelling.end)
                        .ok_or(Error::InvalidState)?;
                    self.spelling = Some(spelling);
                    self.phase = Phase::Literal(literal::Cursor::new(source, work, budget));
                    Ok(Status::Yield)
                }
            },
            Phase::Literal(cursor) => match cursor.poll(now)? {
                literal::Status::Yield => Ok(Status::Yield),
                literal::Status::Complete => Ok(Status::Complete),
                literal::Status::Octet { byte, position } => {
                    let spelling = self.spelling.ok_or(Error::InvalidState)?;
                    let position = spelling
                        .start
                        .checked_add(position)
                        .ok_or(Error::InvalidState)?;
                    if position >= spelling.end {
                        return Err(Error::InvalidState);
                    }
                    Ok(Status::Octet { byte, position })
                }
            },
            Phase::Empty => Err(Error::InvalidState),
        }
    }
    pub fn finish(
        mut self,
        now: Tick,
    ) -> Result<(&'w mut Meter, &'w mut HeaderBudget, Spelling), Error> {
        self.check_deadline(now)?;
        let spelling = self.spelling.ok_or(Error::InvalidState)?;
        let Phase::Literal(cursor) = self.phase else {
            return Err(Error::InvalidState);
        };
        let (work, budget) = cursor.finish(now)?;
        Ok((work, budget, spelling))
    }
}
const _: () =
    assert!(std::mem::size_of::<Cursor<'_, '_>>() + std::mem::size_of::<HeaderBudget>() <= 384);

#[cfg(test)]
#[path = "location_literal_field/tests.rs"]
mod tests;

//! First-valid resident Content-Location selection; no retained output grant.
pub mod json;
pub use crate::mime_label_fields::Input;
use crate::{
    admission::work::Meter,
    decode_work,
    header_select::SourceEnd,
    header_work::{self, Aggregate},
    mime_headers::{self, End, Field, Scanner},
    mime_location_field as location,
    nfc::{self, HeaderBudget},
    ports::Tick,
};
/// Passive completed selection; no source authorization or retention grant.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Selection {
    pub content_location: Option<Field>,
    pub location_end: Option<location::End>,
    pub end: End,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Yield,
    Complete,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidRange,
    Truncated,
    Headers(mime_headers::Error),
    Location(location::Error),
    Admission(nfc::Error),
    InvalidState,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRange => f.write_str("invalid resident location range"),
            Self::Truncated => f.write_str("incomplete resident location headers"),
            Self::Headers(error) => write!(f, "location headers: {error}"),
            Self::Location(error) => write!(f, "location field: {error}"),
            Self::Admission(error) => write!(f, "location selection admission: {error}"),
            Self::InvalidState => f.write_str("invalid location selection state"),
        }
    }
}
impl std::error::Error for Error {}
impl From<mime_headers::Error> for Error {
    fn from(error: mime_headers::Error) -> Self {
        Self::Headers(error)
    }
}
impl From<decode_work::Error> for Error {
    fn from(error: decode_work::Error) -> Self {
        Self::Admission(match error {
            decode_work::Error::Work(stop) => nfc::Error::Work(stop),
            decode_work::Error::InterpretationLimit => nfc::Error::InterpretationLimit,
            decode_work::Error::InvalidState => nfc::Error::InvalidState,
        })
    }
}
#[derive(Clone, Copy)]
enum Phase {
    Scan,
    Match,
    Parse,
    Finish,
    Complete,
}
// One inline child holds the original owners without allocation.
#[allow(clippy::large_enum_variant)]
enum Owner<'a, 'w> {
    Budgets(&'w mut Meter, &'w mut HeaderBudget),
    Field(location::Cursor<'a, 'w>),
    Retired,
}
/// Select the first completely valid field, including an empty reference.
/// Only malformed syntax can be skipped; resource/nesting refusals are fatal.
/// Later duplicates are scanned without interpreting their values. The entire
/// raw header boundary must validate before passive selection becomes visible.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mta::mime_location_fields::Cursor<'_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mta::mime_location_fields::Cursor<'_, '_>>();
/// ```
pub struct Cursor<'a, 'w> {
    input: Input<'a>,
    scanner: Scanner,
    consumed: usize,
    candidate: Option<Field>,
    selected: Option<Field>,
    location_end: Option<location::End>,
    end: Option<End>,
    phase: Phase,
    owner: Owner<'a, 'w>,
    credit: u8,
    failure: Option<Error>,
}
impl<'a, 'w> Cursor<'a, 'w> {
    pub fn new(
        input: Input<'a>,
        work: &'w mut Meter,
        budget: &'w mut HeaderBudget,
    ) -> Result<Self, Error> {
        input
            .base
            .checked_add(u64::try_from(input.source.len()).map_err(|_| Error::InvalidRange)?)
            .ok_or(Error::InvalidRange)?;
        Ok(Self {
            input,
            scanner: Scanner::new(input.base, input.header_limit),
            consumed: 0,
            candidate: None,
            selected: None,
            location_end: None,
            end: None,
            phase: Phase::Scan,
            owner: Owner::Budgets(work, budget),
            credit: 0,
            failure: None,
        })
    }
    fn outcome<T>(&mut self, result: Result<T, Error>) -> Result<T, Error> {
        if let Err(error) = result {
            self.failure = Some(error);
            self.selected = None;
            self.location_end = None;
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
                .charge(work, now, 0, 0, &mut self.credit)
                .map_err(Error::Admission),
            Owner::Field(cursor) => cursor.check_deadline(now).map_err(Error::Location),
            Owner::Retired => Err(Error::InvalidState),
        };
        self.outcome(result)
    }
    #[cfg(test)]
    fn remaining(&self) -> Option<(crate::admission::work::Charge, u64, u64)> {
        match &self.owner {
            Owner::Budgets(work, budget) => Some((
                work.remaining(),
                budget.source_bytes_remaining(),
                budget.steps_remaining(),
            )),
            Owner::Field(cursor) => cursor.remaining(),
            Owner::Retired => None,
        }
    }
    /// Cached selection is provisional until fresh original admission.
    pub fn selection(&self) -> Option<Selection> {
        if self.failure.is_some() || !matches!(self.phase, Phase::Complete) {
            return None;
        }
        Some(Selection {
            content_location: self.selected,
            location_end: self.location_end,
            end: self.end?,
        })
    }
    pub fn poll(&mut self, now: Tick) -> Result<Status, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if matches!(self.phase, Phase::Complete) {
            return Ok(Status::Complete);
        }
        self.check_deadline(now)?;
        let result = self.step(now);
        self.outcome(result)
    }
    fn range(&self, start: u64, end: u64) -> Result<&'a [u8], Error> {
        td_header::resident::slice(self.input.source, self.input.base, start..end)
            .ok_or(Error::InvalidState)
    }
    fn reset_candidate(&mut self) {
        self.candidate = None;
        self.phase = Phase::Scan;
    }
    fn step(&mut self, now: Tick) -> Result<Status, Error> {
        match self.phase {
            Phase::Scan => {
                let Owner::Budgets(work, budget) = &mut self.owner else {
                    return Err(Error::InvalidState);
                };
                let input = self
                    .input
                    .source
                    .get(self.consumed..)
                    .ok_or(Error::InvalidState)?;
                let progress = self.scanner.poll_with_work(
                    input,
                    self.input.source_end == SourceEnd::Eof,
                    now,
                    &mut Aggregate::new(work, budget, &mut self.credit),
                )?;
                self.consumed = self
                    .consumed
                    .checked_add(progress.consumed)
                    .ok_or(Error::InvalidState)?;
                match progress.status {
                    mime_headers::Status::Field(field) => {
                        self.candidate = Some(field);
                        self.phase = Phase::Match;
                    }
                    mime_headers::Status::Complete(end) => {
                        self.end = Some(end);
                        self.phase = Phase::Finish;
                    }
                    mime_headers::Status::NeedInput => return Err(Error::Truncated),
                    mime_headers::Status::Yield => {}
                }
            }
            Phase::Match => {
                let field = self.candidate.ok_or(Error::InvalidState)?;
                let name = self.range(field.name_start, field.name_end)?;
                let Owner::Budgets(work, budget) = &mut self.owner else {
                    return Err(Error::InvalidState);
                };
                let wanted = b"Content-Location";
                let same_length = name.len() == wanted.len();
                header_work::Work::charge(
                    &mut Aggregate::new(work, budget, &mut self.credit),
                    now,
                    header_work::Charge {
                        visits: if same_length {
                            wanted.len() as u64 * 2
                        } else {
                            0
                        },
                        steps: if same_length { wanted.len() as u64 } else { 1 },
                        records: 1,
                    },
                )?;
                if !same_length || !name.eq_ignore_ascii_case(wanted) || self.selected.is_some() {
                    self.reset_candidate();
                } else {
                    let source = self.range(field.value_start, field.value_end)?;
                    let Owner::Budgets(work, budget) =
                        std::mem::replace(&mut self.owner, Owner::Retired)
                    else {
                        return Err(Error::InvalidState);
                    };
                    self.owner = Owner::Field(location::Cursor::new(source, work, budget));
                    self.phase = Phase::Parse;
                }
            }
            Phase::Parse => {
                let Owner::Field(cursor) = &mut self.owner else {
                    return Err(Error::InvalidState);
                };
                match cursor.poll(now) {
                    Ok(location::Status::Yield | location::Status::Scalar(_)) => {}
                    Ok(location::Status::Complete) => {
                        let Owner::Field(cursor) =
                            std::mem::replace(&mut self.owner, Owner::Retired)
                        else {
                            return Err(Error::InvalidState);
                        };
                        let (work, budget, end) = cursor.finish(now).map_err(Error::Location)?;
                        self.owner = Owner::Budgets(work, budget);
                        self.selected = Some(self.candidate.ok_or(Error::InvalidState)?);
                        self.location_end = Some(end);
                        self.reset_candidate();
                    }
                    Err(_) => {
                        let Owner::Field(cursor) =
                            std::mem::replace(&mut self.owner, Owner::Retired)
                        else {
                            return Err(Error::InvalidState);
                        };
                        let (work, budget) =
                            cursor.discard_malformed(now).map_err(Error::Location)?;
                        self.owner = Owner::Budgets(work, budget);
                        self.reset_candidate();
                    }
                }
            }
            Phase::Finish => {
                let Owner::Budgets(work, budget) = &mut self.owner else {
                    return Err(Error::InvalidState);
                };
                header_work::Work::charge(
                    &mut Aggregate::new(work, budget, &mut self.credit),
                    now,
                    header_work::Charge {
                        steps: 1,
                        records: 1,
                        ..header_work::Charge::default()
                    },
                )?;
                if self.end.is_none() {
                    return Err(Error::InvalidState);
                }
                self.phase = Phase::Complete;
                return Ok(Status::Complete);
            }
            Phase::Complete => return Ok(Status::Complete),
        }
        Ok(Status::Yield)
    }
    pub fn finish(
        mut self,
        now: Tick,
    ) -> Result<(&'w mut Meter, &'w mut HeaderBudget, Selection), Error> {
        self.check_deadline(now)?;
        let selection = self.selection().ok_or(Error::InvalidState)?;
        let Owner::Budgets(work, budget) = self.owner else {
            return Err(Error::InvalidState);
        };
        Ok((work, budget, selection))
    }
}
const _: () =
    assert!(std::mem::size_of::<Cursor<'_, '_>>() + std::mem::size_of::<HeaderBudget>() <= 1024);

#[cfg(test)]
mod tests;

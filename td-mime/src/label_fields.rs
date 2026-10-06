//! First-valid resident Content-ID and Content-Language field extents.
#[path = "label_fields/json.rs"]
pub mod json;
use crate::{
    decode_work::{self, Lexical, Parsing},
    header_message_ids,
    header_select::SourceEnd,
    header_work::{self, Aggregate},
    headers::{self, End, Field, Scanner},
    nfc::HeaderBudget,
    time::Tick,
    work::{Meter, Stop},
};

/// Caller-authorized resident entity input; offsets remain absolute.
#[derive(Clone, Copy)]
pub struct Input<'a> {
    pub source: &'a [u8],
    pub base: u64,
    pub header_limit: u64,
    pub source_end: SourceEnd,
}
/// Passive completed selection; no retained text or source authority follows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Selection {
    pub content_id: Option<Field>,
    pub content_language: Option<Field>,
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
    Headers(headers::Error),
    NestingLimit,
    Work(Stop),
    InterpretationLimit,
    InvalidState,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRange => f.write_str("invalid MIME label header range"),
            Self::Truncated => f.write_str("incomplete resident MIME label headers"),
            Self::Headers(error) => write!(f, "MIME label headers: {error}"),
            Self::NestingLimit => f.write_str("MIME label nesting limit"),
            Self::Work(stop) => write!(f, "MIME label work: {stop}"),
            Self::InterpretationLimit => f.write_str("MIME label interpretation limit"),
            Self::InvalidState => f.write_str("invalid MIME label selection state"),
        }
    }
}
impl std::error::Error for Error {}
impl From<decode_work::Error> for Error {
    fn from(error: decode_work::Error) -> Self {
        match error {
            decode_work::Error::Work(stop) => Self::Work(stop),
            decode_work::Error::InterpretationLimit => Self::InterpretationLimit,
            decode_work::Error::InvalidState => Self::InvalidState,
        }
    }
}
impl From<headers::Error> for Error {
    fn from(error: headers::Error) -> Self {
        match error {
            headers::Error::Work(stop) => Self::Work(stop),
            headers::Error::InterpretationLimit => Self::InterpretationLimit,
            headers::Error::InvalidState => Self::InvalidState,
            other => Self::Headers(other),
        }
    }
}
#[derive(Clone, Copy)]
enum Kind {
    Id,
    Language,
}
enum Syntax<'a> {
    None,
    Id(header_message_ids::Cursor<'a>),
    Language(td_header::language_list::Cursor<'a, decode_work::Error>),
}
#[derive(Clone, Copy)]
enum Phase {
    Scan,
    Match,
    Parse,
    Finish,
    Complete,
}
/// The complete raw header section must validate before selection is visible.
/// Malformed needed occurrences are skipped; nesting and work refusals retire
/// the whole selection. Later duplicates are scanned but not interpreted.
/// No projected CID/language strings, normalization or publication is provided.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mime::label_fields::Cursor<'_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mime::label_fields::Cursor<'_, '_>>();
/// ```
pub struct Cursor<'a, 'w> {
    input: Input<'a>,
    scanner: Scanner,
    consumed: usize,
    candidate: Option<Field>,
    kind: Option<Kind>,
    syntax: Syntax<'a>,
    content_id: Option<Field>,
    content_language: Option<Field>,
    end: Option<End>,
    phase: Phase,
    work: &'w mut Meter,
    budget: &'w mut HeaderBudget,
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
            scanner: Scanner::new(input.base, input.header_limit),
            input,
            consumed: 0,
            candidate: None,
            kind: None,
            syntax: Syntax::None,
            content_id: None,
            content_language: None,
            end: None,
            phase: Phase::Scan,
            work,
            budget,
            credit: 0,
            failure: None,
        })
    }
    fn outcome<T>(&mut self, result: Result<T, Error>) -> Result<T, Error> {
        if let Err(error) = result {
            self.failure = Some(error);
            self.syntax = Syntax::None;
            self.content_id = None;
            self.content_language = None;
            self.end = None;
        }
        result
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self
            .budget
            .charge_local(self.work, now, 0, 0, &mut self.credit)
            .map_err(decode_work::Error::from)
            .map_err(Error::from);
        self.outcome(result)
    }
    /// Cached selection is provisional until fresh original admission.
    pub fn selection(&self) -> Option<Selection> {
        if self.failure.is_some() || !matches!(self.phase, Phase::Complete) {
            return None;
        }
        Some(Selection {
            content_id: self.content_id,
            content_language: self.content_language,
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
    fn reset(&mut self) {
        self.candidate = None;
        self.kind = None;
        self.syntax = Syntax::None;
        self.phase = Phase::Scan;
    }
    fn step(&mut self, now: Tick) -> Result<Status, Error> {
        match self.phase {
            Phase::Scan => {
                let input = self
                    .input
                    .source
                    .get(self.consumed..)
                    .ok_or(Error::InvalidState)?;
                let progress = self.scanner.poll_with_work(
                    input,
                    self.input.source_end == SourceEnd::Eof,
                    now,
                    &mut Aggregate::new(self.work, self.budget, &mut self.credit),
                )?;
                self.consumed = self
                    .consumed
                    .checked_add(progress.consumed)
                    .ok_or(Error::InvalidState)?;
                match progress.status {
                    headers::Status::Field(field) => {
                        self.candidate = Some(field);
                        self.phase = Phase::Match;
                    }
                    headers::Status::Complete(end) => {
                        self.end = Some(end);
                        self.phase = Phase::Finish;
                    }
                    headers::Status::Yield => {}
                    headers::Status::NeedInput => return Err(Error::Truncated),
                }
            }
            Phase::Match => {
                let field = self.candidate.ok_or(Error::InvalidState)?;
                let name = self.range(field.name_start, field.name_end)?;
                let (wanted, kind): (&[u8], Kind) = match name.len() {
                    10 => (b"Content-ID", Kind::Id),
                    16 => (b"Content-Language", Kind::Language),
                    _ => {
                        header_work::Work::charge(
                            &mut Aggregate::new(self.work, self.budget, &mut self.credit),
                            now,
                            header_work::Charge {
                                steps: 1,
                                records: 1,
                                ..header_work::Charge::default()
                            },
                        )?;
                        self.reset();
                        return Ok(Status::Yield);
                    }
                };
                header_work::Work::charge(
                    &mut Aggregate::new(self.work, self.budget, &mut self.credit),
                    now,
                    header_work::Charge {
                        visits: wanted.len() as u64 * 2,
                        steps: wanted.len() as u64,
                        records: 1,
                    },
                )?;
                let occupied = match kind {
                    Kind::Id => self.content_id.is_some(),
                    Kind::Language => self.content_language.is_some(),
                };
                if !name.eq_ignore_ascii_case(wanted) || occupied {
                    self.reset();
                } else {
                    let value = self.range(field.value_start, field.value_end)?;
                    self.kind = Some(kind);
                    self.syntax = match kind {
                        Kind::Id => Syntax::Id(header_message_ids::Cursor::content_id(value)),
                        Kind::Language => {
                            Syntax::Language(td_header::language_list::Cursor::new(value))
                        }
                    };
                    self.phase = Phase::Parse;
                }
            }
            Phase::Parse => {
                let result = match &mut self.syntax {
                    Syntax::Id(cursor) => cursor
                        .poll_with_work(
                            now,
                            &mut Parsing::new(self.work, self.budget, &mut self.credit),
                        )
                        .map(|status| status == header_message_ids::Status::Complete)
                        .map_err(|error| match error {
                            header_message_ids::Error::Malformed => None,
                            header_message_ids::Error::NestingLimit => Some(Error::NestingLimit),
                            header_message_ids::Error::Work(stop) => Some(Error::Work(stop)),
                            header_message_ids::Error::InterpretationLimit => {
                                Some(Error::InterpretationLimit)
                            }
                            header_message_ids::Error::InvalidState => Some(Error::InvalidState),
                        }),
                    Syntax::Language(cursor) => cursor
                        .poll(&mut Lexical::new(
                            &mut Parsing::new(self.work, self.budget, &mut self.credit),
                            now,
                        ))
                        .map(|status| status == td_header::language_list::Status::Complete)
                        .map_err(|error| match error {
                            td_header::language_list::Error::Malformed => None,
                            td_header::language_list::Error::NestingLimit => {
                                Some(Error::NestingLimit)
                            }
                            td_header::language_list::Error::Work(error) => {
                                Some(Error::from(error))
                            }
                            td_header::language_list::Error::InvalidState => {
                                Some(Error::InvalidState)
                            }
                        }),
                    Syntax::None => return Err(Error::InvalidState),
                };
                match result {
                    Ok(false) => {}
                    Ok(true) => {
                        let field = self.candidate.ok_or(Error::InvalidState)?;
                        let selected = match self.kind.ok_or(Error::InvalidState)? {
                            Kind::Id => &mut self.content_id,
                            Kind::Language => &mut self.content_language,
                        };
                        if selected.replace(field).is_some() {
                            return Err(Error::InvalidState);
                        }
                        self.reset();
                    }
                    Err(None) => self.reset(),
                    Err(Some(error)) => return Err(error),
                }
            }
            Phase::Finish => {
                header_work::Work::charge(
                    &mut Aggregate::new(self.work, self.budget, &mut self.credit),
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
        Ok((self.work, self.budget, selection))
    }
}
const _: () =
    assert!(std::mem::size_of::<Cursor<'_, '_>>() + std::mem::size_of::<HeaderBudget>() <= 768);

#[cfg(test)]
#[path = "label_fields/tests.rs"]
mod tests;

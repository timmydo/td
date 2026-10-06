//! Source-ordered EmailHeader objects from one resident authorized header section.
pub mod retained;
use super::{End, Field, Scanner};
pub use crate::json_string::{Progress, Status};
use crate::{
    admission::work::{Charge, Meter},
    header_raw,
    header_select::SourceEnd,
    header_work::Aggregate,
    json_string::{self, Frame, Source},
    nfc::{self, HeaderBudget},
    ports::Tick,
};

/// Bounds and EOF knowledge come from the caller's original authorized entity.
#[derive(Clone, Copy)]
pub struct Input<'a> {
    pub source: &'a [u8],
    pub base: u64,
    pub source_end: SourceEnd,
    pub header_limit: u64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Headers(super::Error),
    Raw(header_raw::Error),
    Json(json_string::Error),
    Admission(nfc::Error),
    Truncated,
    ResponseCapacity,
    InvalidState,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Headers(e) => write!(f, "header array scanning: {e}"),
            Self::Raw(e) => write!(f, "header array Raw value: {e}"),
            Self::Json(e) => write!(f, "header array JSON string: {e}"),
            Self::Admission(e) => write!(f, "header array admission: {e}"),
            Self::Truncated => f.write_str("incomplete resident header array"),
            Self::ResponseCapacity => f.write_str("retained header array capacity"),
            Self::InvalidState => f.write_str("invalid header array state"),
        }
    }
}
impl std::error::Error for Error {}
enum Owner<'a, 'w> {
    Budgets(&'w mut Meter, &'w mut HeaderBudget),
    Raw(header_raw::Budgeted<'a, 'w>),
    Retired,
}
#[derive(Clone, Copy, Eq, PartialEq)]
enum Phase {
    Open,
    Scan,
    Member,
    NameStart,
    Name,
    Between,
    ValueStart,
    Value,
    EndMember,
    Close,
    Done,
}
/// Every byte remains provisional until whole healthy completion.
/// Caller output is transient; this owner cannot recover already emitted bytes.
/// ```compile_fail,E0277
/// fn bound<T: Copy>() {} bound::<td_mta::mime_headers::json::Cursor<'_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn bound<T: Clone>() {} bound::<td_mta::mime_headers::json::Cursor<'_, '_>>();
/// ```
pub struct Cursor<'a, 'w> {
    input: Input<'a>,
    scanner: Scanner,
    consumed: usize,
    field: Option<Field>,
    end: Option<End>,
    owner: Owner<'a, 'w>,
    frame: Frame,
    phase: Phase,
    offset: usize,
    first: bool,
    encoding_problem: bool,
    credit: u8,
    failure: Option<Error>,
}
/// Original budget handoff after complete emission and fresh admission.
/// This is not retained output or source/publication authority.
/// ```compile_fail,E0277
/// fn bound<T: Copy>() {} bound::<td_mta::mime_headers::json::Completion<'_>>();
/// ```
/// ```compile_fail,E0277
/// fn bound<T: Clone>() {} bound::<td_mta::mime_headers::json::Completion<'_>>();
/// ```
pub struct Completion<'w> {
    pub end: End,
    pub is_encoding_problem: bool,
    pub work: &'w mut Meter,
    pub budget: &'w mut HeaderBudget,
}
impl<'a, 'w> Cursor<'a, 'w> {
    pub fn new(
        input: Input<'a>,
        work: &'w mut Meter,
        budget: &'w mut HeaderBudget,
        now: Tick,
    ) -> Result<Self, Error> {
        budget
            .charge(work, now, 0, 0, &mut 0)
            .map_err(Error::Admission)?;
        input
            .base
            .checked_add(input.source.len() as u64)
            .ok_or(Error::Headers(super::Error::Offset))?;
        Ok(Self {
            input,
            scanner: Scanner::new(input.base, input.header_limit),
            consumed: 0,
            field: None,
            end: None,
            owner: Owner::Budgets(work, budget),
            frame: Frame::new(),
            phase: Phase::Open,
            offset: 0,
            first: true,
            encoding_problem: false,
            credit: 0,
            failure: None,
        })
    }
    pub fn value(&self) -> Option<(End, bool)> {
        if self.failure.is_some() || self.phase != Phase::Done {
            return None;
        }
        Some((self.end?, self.encoding_problem))
    }
    fn outcome<T>(&mut self, result: Result<T, Error>) -> Result<T, Error> {
        if let Err(error) = result {
            self.failure = Some(error);
            self.end = None;
            self.field = None;
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
            Owner::Raw(raw) => raw.charge_output(now, 0).map_err(Error::Raw),
            Owner::Retired => Err(Error::InvalidState),
        };
        self.outcome(result)
    }
    pub fn finish(mut self, now: Tick) -> Result<Completion<'w>, Error> {
        self.check_deadline(now)?;
        let (end, is_encoding_problem) = self.value().ok_or(Error::InvalidState)?;
        let Owner::Budgets(work, budget) = self.owner else {
            return Err(Error::InvalidState);
        };
        Ok(Completion {
            end,
            is_encoding_problem,
            work,
            budget,
        })
    }
    pub fn poll(&mut self, now: Tick, output: &mut [u8]) -> Result<Progress, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if self.phase == Phase::Done {
            return Ok(Progress {
                written: 0,
                status: Status::Complete,
            });
        }
        self.check_deadline(now)?;
        if output.is_empty() {
            return Ok(Progress {
                written: 0,
                status: Status::NeedOutput,
            });
        }
        let result = self.step(now, output);
        let progress = self.outcome(result)?;
        self.check_deadline(now)?;
        Ok(progress)
    }
    fn transition(&mut self, phase: Phase) {
        self.phase = phase;
        self.offset = 0;
    }
    fn begin_raw(&mut self, now: Tick, name: bool) -> Result<(), Error> {
        let field = self.field.ok_or(Error::InvalidState)?;
        let range = if name {
            field.name_start..field.name_end
        } else {
            field.value_start..field.value_end
        };
        let bytes = td_header::resident::slice(self.input.source, self.input.base, range)
            .ok_or(Error::InvalidState)?;
        let Owner::Budgets(work, budget) = std::mem::replace(&mut self.owner, Owner::Retired)
        else {
            return Err(Error::InvalidState);
        };
        budget
            .charge(work, now, 0, 1, &mut self.credit)
            .map_err(Error::Admission)?;
        self.owner = Owner::Raw(header_raw::Budgeted::new(bytes, work, budget));
        self.frame = Frame::new();
        self.transition(if name { Phase::Name } else { Phase::Value });
        Ok(())
    }
    fn literal(
        &mut self,
        now: Tick,
        output: &mut [u8],
        bytes: &[u8],
        next: Phase,
    ) -> Result<Progress, Error> {
        let tail = bytes.get(self.offset..).ok_or(Error::InvalidState)?;
        let written = tail.len().min(output.len()).min(64);
        let Owner::Budgets(work, budget) = &mut self.owner else {
            return Err(Error::InvalidState);
        };
        budget
            .charge(work, now, 0, 1, &mut self.credit)
            .map_err(Error::Admission)?;
        work.charge(
            now,
            Charge {
                output_bytes: written as u64,
                ..Charge::default()
            },
        )
        .map_err(|e| Error::Admission(nfc::Error::Work(e)))?;
        output
            .get_mut(..written)
            .ok_or(Error::InvalidState)?
            .copy_from_slice(tail.get(..written).ok_or(Error::InvalidState)?);
        self.offset = self
            .offset
            .checked_add(written)
            .ok_or(Error::InvalidState)?;
        if self.offset == bytes.len() {
            self.transition(next);
        }
        Ok(Progress {
            written,
            status: if self.phase == Phase::Done {
                Status::Complete
            } else {
                Status::Yield
            },
        })
    }
    fn scan(&mut self, now: Tick) -> Result<(), Error> {
        let bytes = self
            .input
            .source
            .get(self.consumed..)
            .ok_or(Error::InvalidState)?;
        let Owner::Budgets(work, budget) = &mut self.owner else {
            return Err(Error::InvalidState);
        };
        let progress = self
            .scanner
            .poll_with_work(
                bytes,
                self.input.source_end == SourceEnd::Eof,
                now,
                &mut Aggregate::new(work, budget, &mut self.credit),
            )
            .map_err(Error::Headers)?;
        if progress.consumed > bytes.len() {
            return Err(Error::InvalidState);
        }
        self.consumed = self
            .consumed
            .checked_add(progress.consumed)
            .ok_or(Error::InvalidState)?;
        match progress.status {
            super::Status::Field(field) => {
                let endpoint = self
                    .input
                    .base
                    .checked_add(self.consumed as u64)
                    .ok_or(Error::InvalidState)?;
                if field.name_start < self.input.base
                    || field.name_start >= field.name_end
                    || field.name_end > field.value_start
                    || field.value_start > field.value_end
                    || field.value_end > endpoint
                {
                    return Err(Error::InvalidState);
                }
                self.field = Some(field);
                self.transition(Phase::Member);
            }
            super::Status::Complete(end) => {
                self.end = Some(end);
                self.transition(Phase::Close);
            }
            super::Status::NeedInput => return Err(Error::Truncated),
            super::Status::Yield => {}
        }
        Ok(())
    }
    fn string(&mut self, now: Tick, output: &mut [u8], name: bool) -> Result<Progress, Error> {
        let Owner::Raw(raw) = &mut self.owner else {
            return Err(Error::InvalidState);
        };
        let progress = self
            .frame
            .poll(&mut Source::BudgetedRaw(raw), now, output)
            .map_err(Error::Json)?;
        if progress.status == Status::Complete {
            let Owner::Raw(raw) = std::mem::replace(&mut self.owner, Owner::Retired) else {
                return Err(Error::InvalidState);
            };
            self.encoding_problem |= raw.is_encoding_problem();
            let (work, budget) = raw.finish().map_err(Error::Raw)?;
            self.owner = Owner::Budgets(work, budget);
            self.transition(if name {
                Phase::Between
            } else {
                Phase::EndMember
            });
        }
        Ok(Progress {
            written: progress.written,
            status: Status::Yield,
        })
    }
    fn step(&mut self, now: Tick, output: &mut [u8]) -> Result<Progress, Error> {
        match self.phase {
            Phase::Open => return self.literal(now, output, b"[", Phase::Scan),
            Phase::Scan => self.scan(now)?,
            Phase::Member => {
                let bytes: &[u8] = if self.first {
                    b"{\"name\":"
                } else {
                    b",{\"name\":"
                };
                let progress = self.literal(now, output, bytes, Phase::NameStart)?;
                if self.phase == Phase::NameStart {
                    self.first = false;
                }
                return Ok(progress);
            }
            Phase::NameStart => self.begin_raw(now, true)?,
            Phase::Name => return self.string(now, output, true),
            Phase::Between => return self.literal(now, output, b",\"value\":", Phase::ValueStart),
            Phase::ValueStart => self.begin_raw(now, false)?,
            Phase::Value => return self.string(now, output, false),
            Phase::EndMember => return self.literal(now, output, b"}", Phase::Scan),
            Phase::Close => return self.literal(now, output, b"]", Phase::Done),
            Phase::Done => return Err(Error::InvalidState),
        }
        Ok(Progress {
            written: 0,
            status: Status::Yield,
        })
    }
}
const _: () =
    assert!(std::mem::size_of::<Cursor<'_, '_>>() + std::mem::size_of::<HeaderBudget>() <= 512);
const _: () = assert!(std::mem::size_of::<Completion<'_>>() <= 64);
#[cfg(test)]
pub use tests::probe as probe_allocations;
#[cfg(test)]
mod tests;

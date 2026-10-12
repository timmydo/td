//! Interpret clipped entity headers without releasing the original native job.
use crate::{
    admission::work::{Charge, Meter, Stop},
    mime_metadata as metadata,
    mime_structure::{self, entity},
    nfc::{self, Credit, HeaderBudget},
    ports::{Error as PolicyError, Tick},
    root_headers,
    store_fs::PinnedBlob,
};

pub struct Settings {
    pub extent: std::ops::Range<u64>,
    pub header_limit: u64,
    pub context: metadata::Context,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Headers(root_headers::Error),
    Interpretation(mime_structure::Error),
    Policy(PolicyError),
    Work(Stop),
    State,
}
impl From<root_headers::Error> for Error {
    fn from(error: root_headers::Error) -> Self {
        match error {
            root_headers::Error::Policy(error) => Self::Policy(error),
            root_headers::Error::Work(error)
            | root_headers::Error::Headers(crate::mime_headers::Error::Work(error)) => {
                Self::Work(error)
            }
            root_headers::Error::State
            | root_headers::Error::Headers(crate::mime_headers::Error::InvalidState) => Self::State,
            error => Self::Headers(error),
        }
    }
}
impl From<mime_structure::Error> for Error {
    fn from(error: mime_structure::Error) -> Self {
        match error {
            mime_structure::Error::Work(error) => Self::Work(error),
            mime_structure::Error::InvalidState => Self::State,
            error => Self::Interpretation(error),
        }
    }
}
impl From<nfc::Error> for Error {
    fn from(error: nfc::Error) -> Self {
        match error {
            nfc::Error::Work(error) => Self::Work(error),
            nfc::Error::InterpretationLimit => {
                Self::Interpretation(mime_structure::Error::InterpretationLimit)
            }
            nfc::Error::InvalidState | nfc::Error::InvalidTable => Self::State,
        }
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Headers(error) => write!(f, "native entity headers: {error}"),
            Self::Interpretation(error) => write!(f, "native entity interpretation: {error}"),
            Self::Policy(error) => write!(f, "native entity source: {error}"),
            Self::Work(error) => write!(f, "native entity work: {error}"),
            Self::State => f.write_str("invalid native entity state"),
        }
    }
}
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Headers(error) => Some(error),
            Self::Interpretation(error) => Some(error),
            Self::Policy(error) => Some(error),
            Self::Work(error) => Some(error),
            Self::State => None,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Yield,
    Complete,
}
/// Passive header evidence and disjoint backing beside the original live owners.
/// This handoff is not a complete-tree proof or publication authority.
pub struct Retained<'p, 'v, 'c, 'b, 'w> {
    pub source: &'p mut PinnedBlob<'v, 'c>,
    pub work: &'w mut Meter,
    pub budget: &'w mut HeaderBudget,
    pub headers: root_headers::View<'b>,
    pub remaining: &'b mut [u8],
    pub value: entity::Value,
}
/// Each poll performs one bounded collection or interpretation turn,
/// including its consuming handoff when that stage completes.
/// The same native pin and job/header budgets survive every transition.
pub struct Cursor<'p, 'v, 'c, 'b, 'w> {
    reader: Option<root_headers::Cursor<'p, 'v, 'c, 'b, 'w>>,
    interpreter: Option<entity::Cursor<'b, 'w>>,
    source: Option<&'p mut PinnedBlob<'v, 'c>>,
    work: Option<&'w mut Meter>,
    budget: Option<&'w mut HeaderBudget>,
    headers: Option<root_headers::View<'b>>,
    remaining: Option<&'b mut [u8]>,
    value: Option<entity::Value>,
    context: metadata::Context,
    header_limit: u64,
    credit: Credit,
    failure: Option<Error>,
}
impl<'p, 'v, 'c, 'b, 'w> Cursor<'p, 'v, 'c, 'b, 'w> {
    pub fn new(
        source: &'p mut PinnedBlob<'v, 'c>,
        settings: Settings,
        buffer: &'b mut [u8],
        work: &'w mut Meter,
        budget: &'w mut HeaderBudget,
        now: Tick,
    ) -> Result<Self, Error> {
        source.check_deadline().map_err(Error::Policy)?;
        let mut credit = Credit::new();
        budget
            .charge(work, now, 0, 0, &mut credit)
            .map_err(Error::from)?;
        let reader = root_headers::Cursor::new_extent(
            source,
            settings.extent,
            settings.header_limit,
            buffer,
            work,
            now,
        )?;
        let mut cursor = Self {
            reader: Some(reader),
            interpreter: None,
            source: None,
            work: None,
            budget: Some(budget),
            headers: None,
            remaining: None,
            value: None,
            context: settings.context,
            header_limit: settings.header_limit,
            credit,
            failure: None,
        };
        cursor.check(now)?;
        Ok(cursor)
    }
    fn check(&mut self, now: Tick) -> Result<(), Error> {
        if let Some(reader) = self.reader.as_mut() {
            return reader.check(now).map_err(Error::from);
        }
        self.source
            .as_deref_mut()
            .ok_or(Error::State)?
            .check_deadline()
            .map_err(Error::Policy)?;
        if let Some(interpreter) = self.interpreter.as_mut() {
            return interpreter.check_deadline(now).map_err(Error::from);
        }
        let work = self.work.as_deref_mut().ok_or(Error::State)?;
        work.charge(now, Charge::default()).map_err(Error::Work)?;
        self.budget
            .as_deref_mut()
            .ok_or(Error::State)?
            .charge(work, now, 0, 0, &mut self.credit)
            .map_err(Error::from)
    }
    fn outcome<T>(&mut self, result: Result<T, Error>) -> Result<T, Error> {
        if let Err(error) = result {
            self.failure = Some(error);
            self.value = None;
        }
        result
    }
    pub fn poll(&mut self, now: Tick) -> Result<Status, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let before = self.check(now);
        self.outcome(before)?;
        let result = self.step(now);
        let after = if result.is_ok() {
            self.check(now)
        } else if let Some(source) = self.source.as_deref_mut() {
            source.check_deadline().map_err(Error::Policy)
        } else if let Some(reader) = self.reader.as_mut() {
            reader.check_native().map_err(Error::Policy)
        } else {
            // Preserve the consuming handoff refusal after it released its handles.
            Ok(())
        };
        self.outcome(after.and(result))
    }
    fn step(&mut self, now: Tick) -> Result<Status, Error> {
        if self.value.is_some() {
            return Ok(Status::Complete);
        }
        if let Some(reader) = self.reader.as_mut() {
            if reader.poll(now)? == root_headers::Status::Yield {
                return Ok(Status::Yield);
            }
            let reader = self.reader.take().ok_or(Error::State)?;
            let (source, work, headers, remaining) = reader.into_split_parts(now)?;
            self.source = Some(source);
            self.headers = Some(headers);
            self.remaining = Some(remaining);
            self.work = Some(work);
            return Ok(Status::Yield);
        }
        if self.interpreter.is_none() {
            let headers = self.headers.ok_or(Error::State)?;
            let interpreter = entity::Cursor::new(
                entity::Input {
                    source: headers.bytes,
                    base: headers.base,
                    header_limit: self.header_limit,
                    context: self.context,
                    source_end: headers.source_end,
                },
                self.work.take().ok_or(Error::State)?,
                self.budget.take().ok_or(Error::State)?,
            )
            .map_err(Error::from)?;
            self.interpreter = Some(interpreter);
            return Ok(Status::Yield);
        }
        if self
            .interpreter
            .as_mut()
            .ok_or(Error::State)?
            .poll(now)
            .map_err(Error::from)?
            == mime_structure::Status::Yield
        {
            return Ok(Status::Yield);
        }
        let interpreter = self.interpreter.take().ok_or(Error::State)?;
        let (value, work, budget) = interpreter.finish(now).map_err(Error::from)?;
        self.work = Some(work);
        self.budget = Some(budget);
        if value.selection.end != self.headers.ok_or(Error::State)?.end {
            return Err(Error::State);
        }
        self.value = Some(value);
        Ok(Status::Complete)
    }
    /// Passive observation; publication still requires original native/job checks.
    pub fn value(&self) -> Option<entity::Value> {
        if self.failure.is_some() {
            None
        } else {
            self.value
        }
    }
    pub fn finish(mut self, now: Tick) -> Result<Retained<'p, 'v, 'c, 'b, 'w>, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        self.check(now)?;
        Ok(Retained {
            source: self.source.take().ok_or(Error::State)?,
            work: self.work.take().ok_or(Error::State)?,
            budget: self.budget.take().ok_or(Error::State)?,
            headers: self.headers.ok_or(Error::State)?,
            remaining: self.remaining.take().ok_or(Error::State)?,
            value: self.value.ok_or(Error::State)?,
        })
    }
}
const _: () = assert!(std::mem::size_of::<Cursor<'_, '_, '_, '_, '_>>() <= 4096);

#[cfg(test)]
#[path = "blob_entity_tests.rs"]
mod tests;

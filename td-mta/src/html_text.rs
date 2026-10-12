//! Charged HTML text extraction over already charset-decoded scalars.
use crate::{
    admission::work::{Charge, Meter, Stop},
    mime_text::CheckedClock,
    ports::{Clock, Error as PolicyError, Tick},
};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Work(Stop),
    Policy(PolicyError),
    State,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HTML text extraction: {self:?}")
    }
}
impl std::error::Error for Error {}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Yield,
    NeedInput,
    Complete,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Progress {
    pub consumed: bool,
    pub scalar: Option<char>,
    pub status: Status,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Diagnostics {
    pub malformed_markup: bool,
}
/// Retain unconsumed input/last pairs and drain EOF until Complete, handling
/// any scalar accompanying completion. Output remains provisional until the
/// original source and enclosing response finish under their own custody.
pub struct Cursor<'w, 'c> {
    work: &'w mut Meter,
    clock: CheckedClock<'c>,
    core: td_html::extract::Extractor,
    complete: bool,
    failure: Option<Error>,
}
impl<'w, 'c> Cursor<'w, 'c> {
    pub fn new(work: &'w mut Meter, clock: &'c dyn Clock, last: Tick) -> Result<Self, Error> {
        let mut cursor = Self {
            work,
            clock: CheckedClock::new(clock, last),
            core: td_html::extract::Extractor::new(),
            complete: false,
            failure: None,
        };
        cursor.check()?;
        Ok(cursor)
    }
    fn charge(&mut self, records: u64, output_bytes: u64) -> Result<(), Error> {
        let now = self.clock.sample().map_err(Error::Policy)?.monotonic;
        self.work
            .charge(
                now,
                Charge {
                    records,
                    output_bytes,
                    ..Charge::default()
                },
            )
            .map_err(Error::Work)
    }
    fn outcome<T>(&mut self, result: Result<T, Error>) -> Result<T, Error> {
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    pub fn check(&mut self) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self.charge(0, 0);
        self.outcome(result)
    }
    /// Passive completed diagnostics; check/finish provides fresh admission.
    pub fn value(&self) -> Option<Diagnostics> {
        (self.failure.is_none() && self.complete).then(|| Diagnostics {
            malformed_markup: self.core.diagnostics().malformed_markup,
        })
    }
    pub fn poll(&mut self, input: Option<char>, last: bool) -> Result<Progress, Error> {
        self.check()?;
        let result = self.step(input, last);
        let after = self.charge(0, 0);
        self.outcome(after.and(result))
    }
    fn step(&mut self, input: Option<char>, last: bool) -> Result<Progress, Error> {
        if self.complete {
            return Ok(Progress {
                consumed: false,
                scalar: None,
                status: Status::Complete,
            });
        }
        let progress = poll_shared(&mut self.core, input, last, self.work, &self.clock)?;
        let status = progress.status;
        self.complete = status == Status::Complete;
        Ok(progress)
    }

    pub fn finish(mut self) -> Result<(&'w mut Meter, Diagnostics), Error> {
        self.check()?;
        let diagnostics = self.value().ok_or(Error::State)?;
        Ok((self.work, diagnostics))
    }
}
// The owning adapter supplies pre/post freshness and sticky retirement.
pub(crate) fn poll_shared(
    core: &mut td_html::extract::Extractor,
    input: Option<char>,
    last: bool,
    work: &mut Meter,
    clock: &CheckedClock<'_>,
) -> Result<Progress, Error> {
    // A gap poll can flush a pending reference scalar, so every turn is funded.
    let now = clock.sample().map_err(Error::Policy)?.monotonic;
    work.charge(
        now,
        Charge {
            records: 1,
            ..Charge::default()
        },
    )
    .map_err(Error::Work)?;
    let result = core.poll(input, last).map_err(|_| Error::State)?;
    if let Some(value) = result.scalar {
        let now = clock.sample().map_err(Error::Policy)?.monotonic;
        let bytes = u64::try_from(value.len_utf8()).map_err(|_| Error::State)?;
        work.charge(
            now,
            Charge {
                output_bytes: bytes,
                ..Charge::default()
            },
        )
        .map_err(Error::Work)?;
    }
    Ok(Progress {
        consumed: result.consumed,
        scalar: result.scalar,
        status: match result.status {
            td_html::extract::Status::Yield => Status::Yield,
            td_html::extract::Status::NeedInput => Status::NeedInput,
            td_html::extract::Status::Complete => Status::Complete,
        },
    })
}
const _: () = assert!(std::mem::size_of::<Cursor<'_, '_>>() <= 384);

#[cfg(test)]
#[path = "html_text_tests.rs"]
mod tests;

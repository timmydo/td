//! First-pass HTML cutoff planning; replay and native source custody are external.
use super::{Plain, Status as PlainStatus};
use crate::{
    admission::work::{Charge, Meter, Stop},
    mime_text::CheckedClock,
    ports::{Clock, Error as PolicyError, Tick},
};
use std::num::NonZeroU64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Work(Stop),
    Policy(PolicyError),
    Filter(super::Error),
    State,
    SizeOverflow,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HTML body-value cutoff: {self:?}")
    }
}
impl std::error::Error for Error {}
impl From<super::Error> for Error {
    fn from(error: super::Error) -> Self {
        match error {
            super::Error::Work(stop) => Self::Work(stop),
            other => Self::Filter(other),
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Yield,
    NeedInput,
    Complete,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Progress {
    pub consumed: bool,
    pub status: Status,
}

/// An exact prefix, with no zero-means-unlimited interpretation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Prefix {
    Empty,
    Bytes(NonZeroU64),
}
impl Prefix {
    fn new(bytes: u64) -> Self {
        NonZeroU64::new(bytes).map_or(Self::Empty, Self::Bytes)
    }
}

/// Passive metadata, not a source binding or publication authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Cutoff {
    /// Exact projected UTF-8 prefix; Empty must be handled explicitly on replay.
    pub prefix: Prefix,
    pub is_truncated: bool,
    /// Only the Plain filter diagnostic; OR with the original text reader's.
    pub is_encoding_problem: bool,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Tag {
    Outside,
    Inside,
    Single,
    Double,
}

/// Consumes decoded scalars under the original work/clock until actual EOF.
/// Retain an unconsumed input/last pair, as for Plain. Caller retains the
/// immutable source, its authorization, decoder diagnostics and replay state.
/// No whole-tag buffer is needed: replay the same source after planning.
/// ```compile_fail,E0277
/// fn required<T: Copy>() {} required::<td_mta::body_value::html::Cursor<'_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn required<T: Clone>() {} required::<td_mta::body_value::html::Cursor<'_, '_>>();
/// ```
pub struct Cursor<'w, 'c> {
    work: &'w mut Meter,
    clock: CheckedClock<'c>,
    core: Core,
    failure: Option<Error>,
}
impl<'w, 'c> Cursor<'w, 'c> {
    pub fn new(max_bytes: u64, work: &'w mut Meter, clock: &'c dyn Clock) -> Result<Self, Error> {
        let mut cursor = Self {
            work,
            clock: CheckedClock::new(clock, Tick(0)),
            core: Core::new(max_bytes),
            failure: None,
        };
        cursor.check()?;
        Ok(cursor)
    }
    /// Passive cached metadata; call check or finish for fresh admission.
    pub fn value(&self) -> Option<Cutoff> {
        if self.failure.is_some() {
            None
        } else {
            self.core.value()
        }
    }

    pub fn check(&mut self) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self.admit(0).map(|_| ());
        self.record(result)
    }
    pub fn finish(mut self) -> Result<Cutoff, Error> {
        self.check()?;
        self.value().ok_or(Error::State)
    }
    /// One Plain operation or one tag/scalar operation per turn. Completion
    /// stays freshly admitted; errors hide the plan permanently. A post-turn
    /// admission failure takes precedence over a step failure.
    pub fn poll(&mut self, input: Option<char>, last: bool) -> Result<Progress, Error> {
        self.check()?;
        let result = self.step(input, last);
        let after = self.admit(0);
        self.record(after.and(result))
    }
    fn admit(&mut self, records: u64) -> Result<Tick, Error> {
        let now = self.clock.sample().map_err(Error::Policy)?.monotonic;
        self.work
            .charge(
                now,
                Charge {
                    records,
                    ..Charge::default()
                },
            )
            .map_err(Error::Work)?;
        Ok(now)
    }
    fn record<T>(&mut self, result: Result<T, Error>) -> Result<T, Error> {
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    fn step(&mut self, input: Option<char>, last: bool) -> Result<Progress, Error> {
        if self.core.complete {
            return Ok(Progress {
                consumed: false,
                status: Status::Complete,
            });
        }
        let now = self.admit(0)?;
        self.core.step(input, last, now, self.work)
    }
}

// Detached progress only; every composing owner retains admission and source.
pub(super) struct Core {
    filter: Plain,
    cap: u64,
    total: u64,
    safe: u64,
    tag: Tag,
    pending: Option<char>,
    complete: bool,
}
impl Core {
    pub(super) fn new(max_bytes: u64) -> Self {
        Self {
            filter: Plain::new(0),
            cap: max_bytes,
            total: 0,
            safe: 0,
            tag: Tag::Outside,
            pending: None,
            complete: false,
        }
    }
    pub(super) fn value(&self) -> Option<Cutoff> {
        if !self.complete {
            return None;
        }
        let truncated = self.cap != 0 && self.total > self.cap;
        Some(Cutoff {
            prefix: Prefix::new(if truncated { self.safe } else { self.total }),
            is_truncated: truncated,
            is_encoding_problem: self.filter.is_encoding_problem(),
        })
    }
    pub(super) fn step(
        &mut self,
        input: Option<char>,
        last: bool,
        now: Tick,
        work: &mut Meter,
    ) -> Result<Progress, Error> {
        if self.complete {
            return Ok(Progress {
                consumed: false,
                status: Status::Complete,
            });
        }
        if let Some(value) = self.pending {
            work.charge(
                now,
                Charge {
                    records: 1,
                    ..Charge::default()
                },
            )
            .map_err(Error::Work)?;
            self.total = self
                .total
                .checked_add(value.len_utf8() as u64)
                .ok_or(Error::SizeOverflow)?;
            self.tag = match (self.tag, value) {
                (Tag::Outside, '<') => Tag::Inside,
                (Tag::Inside, '\'') => Tag::Single,
                (Tag::Inside, '"') => Tag::Double,
                (Tag::Single, '\'') | (Tag::Double, '"') => Tag::Inside,
                (Tag::Inside, '>') => Tag::Outside,
                (tag, _) => tag,
            };
            if self.tag == Tag::Outside && (self.cap == 0 || self.total <= self.cap) {
                self.safe = self.total;
            }
            self.pending = None;
            return Ok(Progress {
                consumed: false,
                status: Status::Yield,
            });
        }
        let progress = self.filter.poll(input, last, now, work)?;
        let status = match progress.status {
            PlainStatus::Scalar(value) => {
                self.pending = Some(value);
                Status::Yield
            }
            PlainStatus::Yield => Status::Yield,
            PlainStatus::NeedInput => Status::NeedInput,
            PlainStatus::Complete => {
                self.complete = true;
                Status::Complete
            }
        };
        Ok(Progress {
            consumed: progress.consumed,
            status,
        })
    }
}
const _: () = assert!(std::mem::size_of::<Cursor<'_, '_>>() <= 256);

#[cfg(test)]
#[path = "html/tests.rs"]
mod tests;

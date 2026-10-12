//! One provisional non-HTML EmailBodyValue from an admitted immutable extent.
use super::Plain;
use crate::{
    admission::work::{Charge, Meter, Stop},
    mime_input::Checkpoints,
    mime_text::{self, CheckedClock, Input, Reader},
    ports::{Clock, Error as PolicyError, Tick},
};
use td_json::string::{self, Source};

/// Maximum provisional bytes returned by one poll.
pub const MAX_CHUNK_BYTES: usize = 47;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Work(Stop),
    Policy(PolicyError),
    Text(mime_text::Error),
    Filter(super::Error),
    State,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "plain body-value JSON: {self:?}")
    }
}
impl std::error::Error for Error {}
impl From<mime_text::Error> for Error {
    fn from(error: mime_text::Error) -> Self {
        match error {
            mime_text::Error::Work(stop) => Self::Work(stop),
            mime_text::Error::Policy(error) => Self::Policy(error),
            _ => Self::Text(error),
        }
    }
}
impl From<super::Error> for Error {
    fn from(error: super::Error) -> Self {
        match error {
            super::Error::Work(stop) => Self::Work(stop),
            _ => Self::Filter(error),
        }
    }
}
impl From<string::Error<Error>> for Error {
    fn from(error: string::Error<Error>) -> Self {
        match error {
            string::Error::Source(error) => error,
            string::Error::InvalidState => Self::State,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Diagnostics {
    pub is_encoding_problem: bool,
    pub is_truncated: bool,
}
pub use td_json::string::{Progress, Status};

struct Text<'r, 'b, 'w, 'c> {
    reader: Reader<'r, 'b>,
    filter: Plain,
    work: &'w mut Meter,
    clock: CheckedClock<'c>,
    pending: Option<char>,
    ended: bool,
}
impl Text<'_, '_, '_, '_> {
    fn diagnostics(&self) -> Diagnostics {
        Diagnostics {
            is_encoding_problem: self.reader.is_encoding_problem()
                || self.filter.is_encoding_problem(),
            is_truncated: self.filter.is_truncated(),
        }
    }
}
impl Source for Text<'_, '_, '_, '_> {
    type Context = ();
    type Error = Error;
    fn charge_output(&mut self, (): (), bytes: u64) -> Result<(), Error> {
        let now = self.clock.sample().map_err(Error::Policy)?.monotonic;
        self.work
            .charge(
                now,
                Charge {
                    output_bytes: bytes,
                    ..Charge::default()
                },
            )
            .map_err(Error::Work)
    }
    fn poll(&mut self, (): ()) -> Result<string::Scalar, Error> {
        if self.pending.is_none() && !self.ended {
            match self.reader.poll(&self.clock, self.work)? {
                mime_text::Status::Scalar(value) => self.pending = Some(value),
                mime_text::Status::Complete => self.ended = true,
                mime_text::Status::Yield => {}
            }
            return Ok(string::Scalar::Yield);
        }
        let now = self.clock.sample().map_err(Error::Policy)?.monotonic;
        let progress = self.filter.poll(self.pending, self.ended, now, self.work)?;
        if progress.consumed {
            self.pending = None;
        }
        Ok(match progress.status {
            super::Status::Scalar(value) => string::Scalar::Value(value),
            super::Status::Complete => string::Scalar::Complete,
            super::Status::Yield | super::Status::NeedInput => string::Scalar::Yield,
        })
    }
}
#[derive(Clone, Copy, Eq, PartialEq)]
enum Phase {
    Head,
    Value,
    Tail,
    Complete,
}

/// Retains one original text source, work budget and clock until consuming finish.
/// Caller admission authorizes a non-HTML part and retains its native snapshot;
/// this adapter grants no part, account, or publication authority. Retain every
/// emitted byte in an unpublished tail until the enclosing method succeeds.
/// ```compile_fail,E0277
/// fn required<T: Copy>() {} required::<td_mta::body_value::json::Cursor<'_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn required<T: Clone>() {} required::<td_mta::body_value::json::Cursor<'_, '_, '_, '_>>();
/// ```
pub struct Cursor<'r, 'b, 'w, 'c> {
    source: Text<'r, 'b, 'w, 'c>,
    frame: string::Frame<Error>,
    phase: Phase,
    position: usize,
    failure: Option<Error>,
}
impl<'r, 'b, 'w, 'c> Cursor<'r, 'b, 'w, 'c> {
    pub fn new(
        input: Input<'r>,
        buffer: &'b mut [u8],
        checkpoints: &'b mut Checkpoints,
        max_bytes: u64,
        work: &'w mut Meter,
        clock: &'c dyn Clock,
    ) -> Result<Self, Error> {
        let mut cursor = Self {
            source: Text {
                reader: Reader::new(input, buffer, checkpoints)?,
                filter: Plain::new(max_bytes),
                work,
                clock: CheckedClock::new(clock, Tick(0)),
                pending: None,
                ended: false,
            },
            frame: string::Frame::new(),
            phase: Phase::Head,
            position: 0,
            failure: None,
        };
        cursor.check()?;
        Ok(cursor)
    }
    /// Passive diagnostics, hidden until the complete object has been emitted.
    pub fn value(&self) -> Option<Diagnostics> {
        (self.failure.is_none() && self.phase == Phase::Complete).then(|| self.source.diagnostics())
    }
    pub fn check(&mut self) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self.source.charge_output((), 0);
        self.record(result)
    }
    pub fn finish(mut self) -> Result<Diagnostics, Error> {
        self.check()?;
        self.value().ok_or(Error::State)
    }
    pub fn poll(&mut self, output: &mut [u8]) -> Result<Progress, Error> {
        self.check()?;
        let result = self.step(output);
        let after = self.source.charge_output((), 0);
        self.record(after.and(result))
    }
    fn record<T>(&mut self, result: Result<T, Error>) -> Result<T, Error> {
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    fn step(&mut self, output: &mut [u8]) -> Result<Progress, Error> {
        if self.phase == Phase::Complete {
            return Ok(Progress {
                written: 0,
                status: Status::Complete,
            });
        }
        if output.is_empty() {
            return Ok(Progress {
                written: 0,
                status: Status::NeedOutput,
            });
        }
        match self.phase {
            Phase::Head => self.literal(b"{\"value\":", Phase::Value, output),
            Phase::Value => {
                let progress = self.frame.poll(&mut self.source, (), output)?;
                if progress.status == Status::Complete {
                    self.phase = Phase::Tail;
                }
                Ok(Progress {
                    written: progress.written,
                    status: if progress.status == Status::Complete {
                        Status::Yield
                    } else {
                        progress.status
                    },
                })
            }
            Phase::Tail => {
                let diagnostics = self.source.diagnostics();
                let tail: &[u8] = match (diagnostics.is_encoding_problem, diagnostics.is_truncated)
                {
                    (false, false) => b",\"isEncodingProblem\":false,\"isTruncated\":false}",
                    (false, true) => b",\"isEncodingProblem\":false,\"isTruncated\":true}",
                    (true, false) => b",\"isEncodingProblem\":true,\"isTruncated\":false}",
                    (true, true) => b",\"isEncodingProblem\":true,\"isTruncated\":true}",
                };
                self.literal(tail, Phase::Complete, output)
            }
            Phase::Complete => Err(Error::State),
        }
    }
    fn literal(&mut self, bytes: &[u8], next: Phase, output: &mut [u8]) -> Result<Progress, Error> {
        let remaining = bytes.get(self.position..).ok_or(Error::State)?;
        let count = remaining.len().min(output.len()).min(MAX_CHUNK_BYTES);
        let bytes = remaining.get(..count).ok_or(Error::State)?;
        self.source.charge_output((), count as u64)?;
        output
            .get_mut(..count)
            .ok_or(Error::State)?
            .copy_from_slice(bytes);
        self.position = self.position.checked_add(count).ok_or(Error::State)?;
        if count == remaining.len() {
            self.position = 0;
            self.phase = next;
        }
        Ok(Progress {
            written: count,
            status: if self.phase == Phase::Complete {
                Status::Complete
            } else {
                Status::Yield
            },
        })
    }
}
const _: () = assert!(std::mem::size_of::<Cursor<'_, '_, '_, '_>>() <= 1024);

#[cfg(test)]
#[path = "json/tests.rs"]
mod tests;

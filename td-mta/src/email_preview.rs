//! Bounded Email preview projection over already-decoded plaintext scalars.
use crate::{
    admission::work::{Charge, Meter, Stop},
    mime_text::CheckedClock,
    ports::{Clock, Error as PolicyError, Tick},
};
pub const MAX_SCALARS: u16 = 256;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Work(Stop),
    Policy(PolicyError),
    State,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Email preview: {self:?}")
    }
}
impl std::error::Error for Error {}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Scalar(char),
    Yield,
    NeedInput,
    Complete,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Progress {
    pub consumed: bool,
    pub status: Status,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Summary {
    pub scalars: u16,
    pub utf8_bytes: u16,
}
/// Supply plaintext, after charset decoding and any HTML extraction. Retain an
/// unconsumed input/last pair. Output stays provisional until fresh completion
/// of the original source and enclosing response, not merely this filter.
pub struct Cursor<'w, 'c> {
    work: &'w mut Meter,
    clock: CheckedClock<'c>,
    summary: Summary,
    space: bool,
    capped: bool,
    ended: bool,
    failure: Option<Error>,
}
impl<'w, 'c> Cursor<'w, 'c> {
    pub fn new(work: &'w mut Meter, clock: &'c dyn Clock, last: Tick) -> Result<Self, Error> {
        let mut cursor = Self {
            work,
            clock: CheckedClock::new(clock, last),
            summary: Summary {
                scalars: 0,
                utf8_bytes: 0,
            },
            space: false,
            capped: false,
            ended: false,
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
    /// Passive counts only; use check or finish for fresh admission.
    pub fn value(&self) -> Option<Summary> {
        (self.failure.is_none() && self.ended).then_some(self.summary)
    }
    pub fn poll(&mut self, input: Option<char>, last: bool) -> Result<Progress, Error> {
        self.check()?;
        let result = self.step(input, last);
        let after = self.charge(0, 0);
        self.outcome(after.and(result))
    }
    fn step(&mut self, input: Option<char>, last: bool) -> Result<Progress, Error> {
        if self.ended {
            return Ok(Progress {
                consumed: false,
                status: Status::Complete,
            });
        }
        if input.is_none() && !last {
            return Ok(Progress {
                consumed: false,
                status: Status::NeedInput,
            });
        }
        self.charge(1, 0)?;
        let Some(value) = input else {
            self.space = false;
            self.ended = true;
            return Ok(Progress {
                consumed: false,
                status: Status::Complete,
            });
        };
        if self.capped || self.summary.scalars == MAX_SCALARS {
            self.ended = last;
            return Ok(Progress {
                consumed: true,
                status: Status::Yield,
            });
        }
        if matches!(
            value,
            ' ' | '\t'
                | '\r'
                | '\n'
                | '\u{b}'
                | '\u{c}'
                | '\u{a0}'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        ) {
            self.space = self.summary.scalars != 0;
            self.ended = last;
            return Ok(Progress {
                consumed: true,
                status: Status::Yield,
            });
        }
        if self.space {
            self.space = false;
            // Do not end a capped preview with a generated separator alone.
            if self.summary.scalars == MAX_SCALARS - 1 {
                self.capped = true;
                self.ended = last;
                return Ok(Progress {
                    consumed: true,
                    status: Status::Yield,
                });
            }
            return self.emit(' ', false);
        }
        self.ended = last;
        self.emit(
            if crate::unicode::is_noncharacter(value)
                || matches!(value, '\0'..='\u{1f}' | '\u{7f}'..='\u{9f}')
            {
                '\u{fffd}'
            } else {
                value
            },
            true,
        )
    }
    fn emit(&mut self, value: char, consumed: bool) -> Result<Progress, Error> {
        let scalars = self.summary.scalars.checked_add(1).ok_or(Error::State)?;
        let bytes = u16::try_from(value.len_utf8()).map_err(|_| Error::State)?;
        let utf8_bytes = self
            .summary
            .utf8_bytes
            .checked_add(bytes)
            .ok_or(Error::State)?;
        self.charge(0, u64::from(bytes))?;
        self.summary = Summary {
            scalars,
            utf8_bytes,
        };
        Ok(Progress {
            consumed,
            status: Status::Scalar(value),
        })
    }
    pub fn finish(mut self) -> Result<(&'w mut Meter, Summary), Error> {
        self.check()?;
        let summary = self.value().ok_or(Error::State)?;
        Ok((self.work, summary))
    }
}
const _: () = assert!(std::mem::size_of::<Cursor<'_, '_>>() <= 128);

#[cfg(test)]
#[path = "email_preview_tests.rs"]
mod tests;

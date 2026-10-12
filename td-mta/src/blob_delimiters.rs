//! Bounded delimiter reads from one retained, verified native blob.
use crate::{
    admission::work::{Charge, Meter, Stop},
    mime_delimiter::{self, Scanner, StreamStatus},
    ports::{BlobReader, Error as PolicyError, Tick},
    store_fs::PinnedBlob,
};
pub use mime_delimiter::Delimiter;
const READ_BYTES: usize = 4096;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Policy(PolicyError),
    Work(Stop),
    Delimiter(mime_delimiter::Error),
    Range,
    Capacity,
    State,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "native delimiter scan: {self:?}")
    }
}
impl std::error::Error for Error {}
impl From<mime_delimiter::Error> for Error {
    fn from(error: mime_delimiter::Error) -> Self {
        match error {
            mime_delimiter::Error::Work(stop) => Self::Work(stop),
            error => Self::Delimiter(error),
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Yield,
    Delimiter(Delimiter),
    Complete,
}
/// Retains the native snapshot and original meter through all source reads.
/// An extent must already be authorized; events grant no part authority.
/// Retain events provisionally until the entire enclosing traversal succeeds.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mta::blob_delimiters::Cursor<'_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mta::blob_delimiters::Cursor<'_, '_, '_, '_, '_>>();
/// ```
pub struct Cursor<'p, 'v, 'c, 'b, 'w> {
    source: &'p mut PinnedBlob<'v, 'c>,
    work: &'w mut Meter,
    scanner: Scanner,
    buffer: &'b mut [u8],
    position: u64,
    end: u64,
    filled: usize,
    consumed: usize,
    read: bool,
    complete: bool,
    failure: Option<Error>,
}
impl<'p, 'v, 'c, 'b, 'w> Cursor<'p, 'v, 'c, 'b, 'w> {
    pub fn new(
        source: &'p mut PinnedBlob<'v, 'c>,
        extent: std::ops::Range<u64>,
        boundary: &[u8],
        buffer: &'b mut [u8],
        work: &'w mut Meter,
        now: Tick,
    ) -> Result<Self, Error> {
        source.check_deadline().map_err(Error::Policy)?;
        work.charge(now, Charge::default()).map_err(Error::Work)?;
        let length = extent.end.checked_sub(extent.start).ok_or(Error::Range)?;
        if extent.end > source.len() {
            return Err(Error::Range);
        }
        if length != 0 && buffer.is_empty() {
            return Err(Error::Capacity);
        }
        let scanner = Scanner::new(extent.start, length, boundary, now, work)?;
        let mut cursor = Self {
            source,
            work,
            scanner,
            buffer,
            position: extent.start,
            end: extent.end,
            filled: 0,
            consumed: 0,
            read: false,
            complete: false,
            failure: None,
        };
        cursor.check(now)?;
        Ok(cursor)
    }
    fn check(&mut self, now: Tick) -> Result<(), Error> {
        self.source.check_deadline().map_err(Error::Policy)?;
        self.scanner
            .check_deadline(now, self.work)
            .map_err(Error::from)
    }
    pub fn poll(&mut self, now: Tick) -> Result<Status, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self.check(now).and_then(|()| self.step(now));
        let after = self.check(now);
        let result = after.and(result);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    fn step(&mut self, now: Tick) -> Result<Status, Error> {
        if self.complete {
            return Ok(Status::Complete);
        }
        if self.read {
            let remaining = self.end.checked_sub(self.position).ok_or(Error::State)?;
            let count = usize::try_from(remaining.min(READ_BYTES as u64))
                .map_err(|_| Error::State)?
                .min(self.buffer.len());
            if count == 0 {
                return Err(Error::State);
            }
            self.work
                .charge(
                    now,
                    Charge {
                        io_bytes: count as u64,
                        records: 1,
                        ..Charge::default()
                    },
                )
                .map_err(Error::Work)?;
            let output = self.buffer.get_mut(..count).ok_or(Error::State)?;
            let read = self
                .source
                .read_at(self.position, output)
                .map_err(Error::Policy)?;
            if read == 0 || read > count {
                return Err(Error::Policy(PolicyError::Corrupt));
            }
            self.position = self.position.checked_add(read as u64).ok_or(Error::State)?;
            self.filled = read;
            self.consumed = 0;
            self.read = false;
            return Ok(Status::Yield);
        }
        let input = self
            .buffer
            .get(self.consumed..self.filled)
            .ok_or(Error::State)?;
        let step = self.scanner.poll(input, now, self.work)?;
        if step.consumed > input.len() {
            return Err(Error::State);
        }
        self.consumed = self
            .consumed
            .checked_add(step.consumed)
            .ok_or(Error::State)?;
        match step.status {
            StreamStatus::Yield => Ok(Status::Yield),
            StreamStatus::Delimiter(value) => Ok(Status::Delimiter(value)),
            StreamStatus::NeedInput => {
                if self.consumed != self.filled || self.position == self.end {
                    return Err(Error::State);
                }
                self.read = true;
                Ok(Status::Yield)
            }
            StreamStatus::Complete => {
                if self.position != self.end || self.consumed != self.filled {
                    return Err(Error::State);
                }
                self.complete = true;
                Ok(Status::Complete)
            }
        }
    }
    pub fn finish(mut self, now: Tick) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        self.check(now)?;
        if !self.complete {
            return Err(Error::State);
        }
        self.scanner.finish(now, self.work).map_err(Error::from)
    }
}
const _: () = assert!(std::mem::size_of::<Cursor<'_, '_, '_, '_, '_>>() <= 512);

#[cfg(test)]
#[path = "blob_delimiters_tests.rs"]
mod tests;

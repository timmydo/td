//! Retain bounded root-header input from one verified native blob.
use crate::{
    admission::work::{Charge, Meter, Stop},
    header_select::SourceEnd,
    mime_headers::{self, Scanner},
    ports::{BlobReader, Error as PolicyError, Tick},
    store_fs::PinnedBlob,
};

const READ_BYTES: usize = 4096;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Policy(PolicyError),
    Work(Stop),
    Headers(mime_headers::Error),
    Capacity,
    State,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Policy(error) => write!(f, "root header source: {error}"),
            Self::Work(error) => write!(f, "root header work: {error}"),
            Self::Headers(error) => write!(f, "root headers: {error}"),
            Self::Capacity => f.write_str("root header storage too small"),
            Self::State => f.write_str("invalid root header read state"),
        }
    }
}
impl std::error::Error for Error {}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Yield,
    Complete,
}
/// Passive immutable input, not source custody or publication permission.
#[derive(Clone, Copy)]
pub struct View<'b> {
    pub bytes: &'b [u8],
    pub source_end: SourceEnd,
    pub end: mime_headers::End,
}
/// Owns the original source/meter borrows while collecting header input.
/// Each turn reads at most 4096 bytes or takes one shared scanner turn.
pub struct Cursor<'p, 'v, 'c, 'b, 'w> {
    source: &'p mut PinnedBlob<'v, 'c>,
    work: &'w mut Meter,
    buffer: &'b mut [u8],
    scanner: Scanner,
    total: u64,
    ceiling: usize,
    filled: usize,
    consumed: usize,
    read: bool,
    end: Option<mime_headers::End>,
    failure: Option<Error>,
}
impl<'p, 'v, 'c, 'b, 'w> Cursor<'p, 'v, 'c, 'b, 'w> {
    pub fn new(
        source: &'p mut PinnedBlob<'v, 'c>,
        header_limit: u64,
        buffer: &'b mut [u8],
        work: &'w mut Meter,
        now: Tick,
    ) -> Result<Self, Error> {
        source.check_deadline().map_err(Error::Policy)?;
        work.charge(now, Charge::default()).map_err(Error::Work)?;
        let total = source.len();
        let required = usize::try_from(total.min(header_limit.saturating_add(2)))
            .map_err(|_| Error::Capacity)?;
        if buffer.len() < required {
            return Err(Error::Capacity);
        }
        let ceiling =
            usize::try_from(total.min(buffer.len() as u64)).map_err(|_| Error::Capacity)?;
        Ok(Self {
            source,
            work,
            buffer,
            scanner: Scanner::new(0, header_limit),
            total,
            ceiling,
            filled: 0,
            consumed: 0,
            read: total != 0,
            end: None,
            failure: None,
        })
    }
    fn check(&mut self, now: Tick) -> Result<(), Error> {
        // The native pin checks its retained snapshot as well as its clock.
        self.source.check_deadline().map_err(Error::Policy)?;
        self.work
            .charge(now, Charge::default())
            .map_err(Error::Work)
    }
    pub fn poll(&mut self, now: Tick) -> Result<Status, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self.step(now);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    fn step(&mut self, now: Tick) -> Result<Status, Error> {
        self.check(now)?;
        if self.end.is_some() {
            return Ok(Status::Complete);
        }
        if self.read {
            let count = self
                .ceiling
                .checked_sub(self.filled)
                .ok_or(Error::State)?
                .min(READ_BYTES);
            if count == 0 {
                return Err(Error::Capacity);
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
            let end = self.filled.checked_add(count).ok_or(Error::State)?;
            let output = self.buffer.get_mut(self.filled..end).ok_or(Error::State)?;
            let read = self
                .source
                .read_at(self.filled as u64, output)
                .map_err(Error::Policy)?;
            if read == 0 || read > count {
                return Err(Error::Policy(PolicyError::Corrupt));
            }
            self.filled = self.filled.checked_add(read).ok_or(Error::State)?;
            self.read = false;
        } else {
            let input = self
                .buffer
                .get(self.consumed..self.filled)
                .ok_or(Error::State)?;
            let progress = self
                .scanner
                .poll(input, self.filled as u64 == self.total, now, self.work)
                .map_err(Error::Headers)?;
            if progress.consumed > input.len() {
                return Err(Error::State);
            }
            self.consumed = self
                .consumed
                .checked_add(progress.consumed)
                .ok_or(Error::State)?;
            match progress.status {
                mime_headers::Status::Complete(end) => self.end = Some(end),
                mime_headers::Status::NeedInput => {
                    if self.consumed != self.filled || self.filled as u64 == self.total {
                        return Err(Error::State);
                    }
                    self.read = true;
                }
                mime_headers::Status::Field(_) | mime_headers::Status::Yield => {}
            }
        }
        self.check(now)?;
        Ok(if self.end.is_some() {
            Status::Complete
        } else {
            Status::Yield
        })
    }
    /// Passive observation only; this does not perform fresh admission.
    pub fn value(&self) -> Option<View<'_>> {
        if self.failure.is_some() {
            return None;
        }
        Some(View {
            bytes: self.buffer.get(..self.filled)?,
            source_end: self.source_end(),
            end: self.end?,
        })
    }
    fn source_end(&self) -> SourceEnd {
        if self.filled as u64 == self.total {
            SourceEnd::Eof
        } else {
            SourceEnd::Prefix
        }
    }
    /// Fresh completion returns only passive bytes; retain the original blob
    /// and request for header projection and final publication checks.
    pub fn finish(mut self, now: Tick) -> Result<View<'b>, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        self.check(now)?;
        let end = self.end.ok_or(Error::State)?;
        let source_end = self.source_end();
        Ok(View {
            bytes: self.buffer.get(..self.filled).ok_or(Error::State)?,
            source_end,
            end,
        })
    }
}
const _: () = assert!(std::mem::size_of::<Cursor<'_, '_, '_, '_, '_>>() <= 512);
#[cfg(test)]
#[path = "root_headers_tests.rs"]
mod tests;

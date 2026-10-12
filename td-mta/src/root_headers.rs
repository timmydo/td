//! Retain bounded entity-header input from one verified native blob.
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
    Range,
    State,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Policy(error) => write!(f, "entity header source: {error}"),
            Self::Work(error) => write!(f, "entity header work: {error}"),
            Self::Headers(error) => write!(f, "entity headers: {error}"),
            Self::Capacity => f.write_str("entity header storage too small"),
            Self::Range => f.write_str("invalid entity header extent"),
            Self::State => f.write_str("invalid entity header read state"),
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
    pub base: u64,
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
    base: u64,
    total: u64,
    header_limit: u64,
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
        let total = source.len();
        Self::new_extent(source, 0..total, header_limit, buffer, work, now)
    }
    /// Collect an already-clipped, authorized entity within the same native blob.
    /// Bounds and header bytes are passive evidence, not MIME-part authority.
    pub fn new_extent(
        source: &'p mut PinnedBlob<'v, 'c>,
        extent: std::ops::Range<u64>,
        header_limit: u64,
        buffer: &'b mut [u8],
        work: &'w mut Meter,
        now: Tick,
    ) -> Result<Self, Error> {
        source.check_deadline().map_err(Error::Policy)?;
        work.charge(now, Charge::default()).map_err(Error::Work)?;
        let total = extent.end.checked_sub(extent.start).ok_or(Error::Range)?;
        if extent.end > source.len() {
            return Err(Error::Range);
        }
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
            scanner: Scanner::new(extent.start, header_limit),
            base: extent.start,
            total,
            header_limit,
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
                .read_at(
                    self.base
                        .checked_add(self.filled as u64)
                        .ok_or(Error::State)?,
                    output,
                )
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
            base: self.base,
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
            base: self.base,
            source_end,
            end,
        })
    }
    /// Freeze only the complete header witness and return unused arena capacity.
    /// A nonconsumed lookahead byte may prove liberal end-of-headers; retain it.
    /// The returned view is passive and the caller retains the original pin/job.
    pub fn finish_split(mut self, now: Tick) -> Result<(View<'b>, &'b mut [u8]), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        self.check(now)?;
        let end = self.end.ok_or(Error::State)?;
        let retained = self
            .consumed
            .checked_add(usize::from(self.consumed < self.filled))
            .ok_or(Error::State)?;
        let source_end = if retained as u64 == self.total {
            SourceEnd::Eof
        } else {
            SourceEnd::Prefix
        };
        let (bytes, remaining) = self
            .buffer
            .split_at_mut_checked(retained)
            .ok_or(Error::State)?;
        Ok((
            View {
                bytes,
                base: self.base,
                source_end,
                end,
            },
            remaining,
        ))
    }
    pub fn into_json<'h>(
        self,
        budget: &'h mut crate::nfc::HeaderBudget,
        scratch: &'h mut crate::nfc::Scratch,
        now: Tick,
    ) -> Result<
        crate::mime_response::root_header_json::Input<'p, 'v, 'c, 'b, 'w, 'h>,
        crate::mime_response::root_header_json::Error,
    > {
        crate::mime_response::root_header_json::Input::new(self, budget, scratch, now)
    }
    pub(crate) fn into_json_parts(
        mut self,
        now: Tick,
    ) -> Result<JsonParts<'p, 'v, 'c, 'b, 'w>, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        self.check(now)?;
        let end = self.end.ok_or(Error::State)?;
        let source_end = self.source_end();
        let bytes = self.buffer.get(..self.filled).ok_or(Error::State)?;
        Ok((
            self.source,
            self.work,
            View {
                bytes,
                base: self.base,
                source_end,
                end,
            },
            self.header_limit,
        ))
    }
}
const _: () = assert!(std::mem::size_of::<Cursor<'_, '_, '_, '_, '_>>() <= 512);
pub(crate) type JsonParts<'p, 'v, 'c, 'b, 'w> =
    (&'p mut PinnedBlob<'v, 'c>, &'w mut Meter, View<'b>, u64);

#[cfg(test)]
#[path = "root_headers_tests.rs"]
mod tests;

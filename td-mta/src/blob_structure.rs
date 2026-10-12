//! Complete MIME traversal while retaining the original verified native snapshot.
pub use crate::blob_entity::Error;
use crate::{
    admission::work::{Charge, Meter},
    mime_structure::{self, stream, Limits, Part, Status},
    nfc::HeaderBudget,
    ports::{BlobReader, Error as PolicyError, Tick},
    root_headers,
    store_fs::PinnedBlob,
};
const READ_BYTES: usize = 4096;
pub struct Settings {
    pub extent: std::ops::Range<u64>,
    pub limits: Limits,
}
/// All backing is reserved by the enclosing job; no body-sized allocation occurs.
pub struct Buffers<'b, 'r, 'o> {
    pub header_bytes: &'b mut [u8],
    pub body: &'r mut [u8],
    pub parts: &'o mut [Part],
    pub headers: &'o mut [Option<root_headers::View<'b>>],
}
/// Passive descriptors and corresponding preorder header views beside live custody.
/// No MIME-part ID or response publication is authorized by these values alone.
pub struct Retained<'p, 'v, 'c, 'b, 'o, 'w> {
    pub source: &'p mut PinnedBlob<'v, 'c>,
    pub work: &'w mut Meter,
    pub budget: &'w mut HeaderBudget,
    pub parts: &'o [Part],
    pub headers: &'o [Option<root_headers::View<'b>>],
    pub remaining: &'b mut [u8],
    pub header_bytes: u64,
}
/// Native reads and shared parser turns keep the same pin and admission owners.
/// Caller-backed headers remain immutable while body input can be overwritten.
pub struct Cursor<'p, 'v, 'c, 'b, 'r, 'o, 'w> {
    core: stream::Cursor<'b, 'o>,
    reader: Option<root_headers::Cursor<'p, 'v, 'c, 'b, 'w>>,
    source: Option<&'p mut PinnedBlob<'v, 'c>>,
    work: Option<&'w mut Meter>,
    budget: &'w mut HeaderBudget,
    arena: Option<&'b mut [u8]>,
    headers: &'o mut [Option<root_headers::View<'b>>],
    used: usize,
    pending: Option<root_headers::View<'b>>,
    buffer: &'r mut [u8],
    buffer_base: u64,
    filled: usize,
    parsed: bool,
    checked: usize,
    checked_headers: u64,
    failure: Option<Error>,
}
impl<'p, 'v, 'c, 'b, 'r, 'o, 'w> Cursor<'p, 'v, 'c, 'b, 'r, 'o, 'w> {
    pub fn new(
        source: &'p mut PinnedBlob<'v, 'c>,
        settings: Settings,
        buffers: Buffers<'b, 'r, 'o>,
        work: &'w mut Meter,
        budget: &'w mut HeaderBudget,
        now: Tick,
    ) -> Result<Self, Error> {
        source.check_deadline().map_err(Error::Policy)?;
        if settings.extent.end > source.len() {
            return Err(Error::Headers(root_headers::Error::Range));
        }
        if buffers.body.is_empty() {
            return Err(Error::Interpretation(mime_structure::Error::OutputCapacity));
        }
        let mut core = stream::Cursor::new(settings.extent, settings.limits, buffers.parts)?;
        core.check_deadline(now, work, budget)?;
        source.check_deadline().map_err(Error::Policy)?;
        Ok(Self {
            core,
            reader: None,
            source: Some(source),
            work: Some(work),
            budget,
            arena: Some(buffers.header_bytes),
            headers: buffers.headers,
            used: 0,
            pending: None,
            buffer: buffers.body,
            buffer_base: 0,
            filled: 0,
            parsed: false,
            checked: 0,
            checked_headers: 0,
            failure: None,
        })
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
        self.core
            .check_deadline(
                now,
                self.work.as_deref_mut().ok_or(Error::State)?,
                self.budget,
            )
            .map_err(Error::from)
    }
    fn outcome<T>(&mut self, result: Result<T, Error>) -> Result<T, Error> {
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    pub fn poll(&mut self, now: Tick) -> Result<Status, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let before = self.check(now);
        self.outcome(before)?;
        let mut result = self.step(now);
        if result == Ok(Status::Complete) && !self.parsed {
            self.parsed = true;
            result = Ok(Status::Yield);
        }
        let after = if result.is_ok() {
            self.check(now)
        } else if let Some(source) = self.source.as_deref_mut() {
            source.check_deadline().map_err(Error::Policy)
        } else if let Some(reader) = self.reader.as_mut() {
            reader.check_native().map_err(Error::Policy)
        } else {
            // Consuming collector refusal already fenced and retired its owners.
            Ok(())
        };
        self.outcome(after.and(result))
    }
    fn step(&mut self, now: Tick) -> Result<Status, Error> {
        if self.parsed {
            let parts = self.core.parts()?.ok_or(Error::State)?;
            if parts.len() != self.used {
                return Err(Error::State);
            }
            if self.checked == self.used {
                if self.core.header_bytes() != Some(self.checked_headers) {
                    return Err(Error::State);
                }
                return Ok(Status::Complete);
            }
            self.work
                .as_deref_mut()
                .ok_or(Error::State)?
                .charge(
                    now,
                    Charge {
                        records: 1,
                        ..Charge::default()
                    },
                )
                .map_err(Error::Work)?;
            let part = parts.get(self.checked).ok_or(Error::State)?;
            let view = self
                .headers
                .get(self.checked)
                .and_then(Option::as_ref)
                .ok_or(Error::State)?;
            if part.entity_start != view.base
                || part.body_start != view.end.body_start
                || usize::from(part.ordinal) != self.checked.checked_add(1).ok_or(Error::State)?
            {
                return Err(Error::State);
            }
            self.checked_headers = self
                .checked_headers
                .checked_add(view.end.header_bytes)
                .ok_or(Error::State)?;
            self.checked += 1;
            return Ok(Status::Yield);
        }
        if let Some(reader) = self.reader.as_mut() {
            if reader.poll(now)? == root_headers::Status::Yield {
                return Ok(Status::Yield);
            }
            let reader = self.reader.take().ok_or(Error::State)?;
            let (source, work, view, remaining) = reader.into_split_parts(now)?;
            self.source = Some(source);
            self.work = Some(work);
            self.arena = Some(remaining);
            let target = self
                .headers
                .get_mut(self.used)
                .ok_or(Error::Interpretation(mime_structure::Error::OutputCapacity))?;
            self.work
                .as_deref_mut()
                .ok_or(Error::State)?
                .charge(
                    now,
                    Charge {
                        output_bytes: std::mem::size_of::<Option<root_headers::View<'_>>>() as u64,
                        records: 1,
                        ..Charge::default()
                    },
                )
                .map_err(Error::Work)?;
            *target = Some(view);
            self.used = self.used.checked_add(1).ok_or(Error::State)?;
            self.pending = Some(view);
            return Ok(Status::Yield);
        }
        if let Some(view) = self.pending.take() {
            return self
                .core
                .poll(
                    stream::Input::Headers {
                        bytes: view.bytes,
                        source_end: view.source_end,
                    },
                    now,
                    self.work.as_deref_mut().ok_or(Error::State)?,
                    self.budget,
                )
                .map_err(Error::from);
        }
        match self.core.request()? {
            stream::Request::Headers {
                start, end, limit, ..
            } => {
                if self.headers.get(self.used).is_none() {
                    return Err(Error::Interpretation(mime_structure::Error::OutputCapacity));
                }
                self.reader = Some(root_headers::Cursor::new_extent(
                    self.source.take().ok_or(Error::State)?,
                    start..end,
                    limit,
                    self.arena.take().ok_or(Error::State)?,
                    self.work.take().ok_or(Error::State)?,
                    now,
                )?);
                Ok(Status::Yield)
            }
            stream::Request::None => self
                .core
                .poll(
                    stream::Input::None,
                    now,
                    self.work.as_deref_mut().ok_or(Error::State)?,
                    self.budget,
                )
                .map_err(Error::from),
            stream::Request::Body { start, end } => {
                let buffered_end = self
                    .buffer_base
                    .checked_add(self.filled as u64)
                    .ok_or(Error::State)?;
                if start < end && (start < self.buffer_base || start >= buffered_end) {
                    let count = usize::try_from((end - start).min(READ_BYTES as u64))
                        .map_err(|_| Error::State)?
                        .min(self.buffer.len());
                    if count == 0 {
                        return Err(Error::Interpretation(mime_structure::Error::OutputCapacity));
                    }
                    self.work
                        .as_deref_mut()
                        .ok_or(Error::State)?
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
                        .as_deref_mut()
                        .ok_or(Error::State)?
                        .read_at(start, output)
                        .map_err(Error::Policy)?;
                    if read == 0 || read > count {
                        return Err(Error::Policy(PolicyError::Corrupt));
                    }
                    self.buffer_base = start;
                    self.filled = read;
                    return Ok(Status::Yield);
                }
                let bytes = if start == end {
                    &[][..]
                } else {
                    let from =
                        usize::try_from(start.checked_sub(self.buffer_base).ok_or(Error::State)?)
                            .map_err(|_| Error::State)?;
                    let to = usize::try_from(
                        buffered_end
                            .min(end)
                            .checked_sub(self.buffer_base)
                            .ok_or(Error::State)?,
                    )
                    .map_err(|_| Error::State)?;
                    self.buffer.get(from..to).ok_or(Error::State)?
                };
                self.core
                    .poll(
                        stream::Input::Body {
                            offset: start,
                            bytes,
                        },
                        now,
                        self.work.as_deref_mut().ok_or(Error::State)?,
                        self.budget,
                    )
                    .map_err(Error::from)
            }
        }
    }
    pub fn finish(mut self, now: Tick) -> Result<Retained<'p, 'v, 'c, 'b, 'o, 'w>, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        self.check(now)?;
        let header_bytes = self.core.header_bytes().ok_or(Error::State)?;
        if !self.parsed || self.checked != self.used || self.checked_headers != header_bytes {
            return Err(Error::State);
        }
        let source = self.source.take().ok_or(Error::State)?;
        let work = self.work.take().ok_or(Error::State)?;
        let parts = self.core.finish(now, work, self.budget)?;
        if parts.len() != self.used {
            return Err(Error::State);
        }
        source.check_deadline().map_err(Error::Policy)?;
        Ok(Retained {
            source,
            work,
            budget: self.budget,
            parts,
            headers: self.headers.get(..self.used).ok_or(Error::State)?,
            remaining: self.arena.take().ok_or(Error::State)?,
            header_bytes,
        })
    }
}
const _: () = assert!(
    std::mem::size_of::<Cursor<'_, '_, '_, '_, '_, '_, '_>>() + std::mem::size_of::<HeaderBudget>()
        <= 16 * 1024
);
#[cfg(test)]
#[path = "blob_structure_tests.rs"]
mod tests;

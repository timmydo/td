//! One explicit-frame MIME policy for resident and fragmented source adapters.
use super::*;
/// Exact next input extent. Headers may retain a definitive prefix; body bytes
/// begin at the requested absolute offset, which can rewind for QP replay.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Request {
    None,
    Headers {
        start: u64,
        end: u64,
        limit: u64,
        context: metadata::Context,
    },
    Body {
        start: u64,
        end: u64,
    },
}
/// Header backing remains immutable for the cursor lifetime. Body fragments are
/// borrowed only for this turn; empty fragments never fabricate source EOF.
pub enum Input<'h, 'b> {
    None,
    Headers {
        bytes: &'h [u8],
        source_end: SourceEnd,
    },
    Body {
        offset: u64,
        bytes: &'b [u8],
    },
}
fn delimiter_error(error: delimiter::Error) -> Error {
    match error {
        delimiter::Error::Work(stop) => Error::Work(stop),
        error => Error::Delimiter(error),
    }
}
struct Frame {
    scanner: delimiter::Scanner,
    position: u64,
    end: u64,
    ordinal: u16,
    next_start: Option<u64>,
    closed: bool,
    digest: bool,
}
enum Phase {
    Entity,
    FirstOpening,
    NextDelimiter,
    Leaf,
    Advance,
    Drain,
    Complete,
}
enum Decode {
    Base64 {
        decoder: base64::Decoder,
        position: u64,
    },
    QuotedPrintable(quoted_printable::Decoder),
}
/// Complete MIME policy over retained headers and caller-supplied body fragments.
/// The caller pins immutable source identity and supplies each requested extent.
/// Passive descriptors never authenticate source bytes or authorize publication.
/// Original job/header owners bind on the first poll and cannot be replaced.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mime::structure::stream::Cursor<'_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mime::structure::stream::Cursor<'_, '_>>();
/// ```
pub struct Cursor<'a, 'w> {
    binding: crate::nfc::Credit,
    credit: u8,
    parts: &'w mut [Part],
    used: usize,
    max_parts: usize,
    max_depth: usize,
    max_headers: u64,
    headers: u64,
    frames: [Option<Frame>; MAX_DEPTH],
    depth: usize,
    current: Part,
    phase: Phase,
    entity: Option<entity::Core<'a>>,
    decoder: Option<Decode>,
    decoded: [u8; 32],
    failure: Option<Error>,
}
impl<'a, 'w> Cursor<'a, 'w> {
    pub fn new(
        extent: std::ops::Range<u64>,
        limits: impl Into<Limits>,
        parts: &'w mut [Part],
    ) -> Result<Self, Error> {
        let limits = limits.into();
        if extent.start > extent.end {
            return Err(Error::InvalidRange);
        }
        if !(1..=MAX_DEPTH).contains(&limits.mime_depth)
            || !(1..=4096).contains(&limits.mime_parts)
            || !(1..=1024 * 1024).contains(&limits.header_bytes)
            || limits.mime_depth > limits.mime_parts
        {
            return Err(Error::InvalidLimits);
        }
        Ok(Self {
            binding: crate::nfc::Credit::new(),
            credit: 0,
            parts,
            used: 0,
            max_parts: limits.mime_parts,
            max_depth: limits.mime_depth,
            max_headers: u64::try_from(limits.header_bytes).map_err(|_| Error::InvalidLimits)?,
            headers: 0,
            frames: std::array::from_fn(|_| None),
            depth: 0,
            current: Part {
                entity_start: extent.start,
                entity_end: extent.end,
                depth: 1,
                ..Part::default()
            },
            phase: Phase::Entity,
            entity: None,
            decoder: None,
            decoded: [0; 32],
            failure: None,
        })
    }
    pub(super) fn outcome<T>(&mut self, result: Result<T, Error>) -> Result<T, Error> {
        if let Err(error) = result {
            self.failure = Some(error);
            self.used = 0;
        }
        result
    }
    pub fn check_deadline(
        &mut self,
        now: Tick,
        work: &mut Meter,
        budget: &mut HeaderBudget,
    ) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = budget
            .charge(work, now, 0, 0, &mut self.binding)
            .map_err(decode_work::Error::from)
            .map_err(Error::from);
        self.outcome(result)
    }
    /// Cached passive descriptors appear only after the complete tree succeeds.
    pub fn parts(&self) -> Result<Option<&[Part]>, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if matches!(self.phase, Phase::Complete) {
            return self
                .parts
                .get(..self.used)
                .map(Some)
                .ok_or(Error::InvalidState);
        }
        Ok(None)
    }
    pub fn header_bytes(&self) -> Option<u64> {
        if self.failure.is_none() && matches!(self.phase, Phase::Complete) {
            Some(self.headers)
        } else {
            None
        }
    }
    /// Passive request labels; the caller retains source custody and all backing.
    pub fn request(&self) -> Result<Request, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        match self.phase {
            Phase::Entity if self.entity.is_none() => Ok(Request::Headers {
                start: self.current.entity_start,
                end: self.current.entity_end,
                limit: self
                    .max_headers
                    .checked_sub(self.headers)
                    .ok_or(Error::HeaderLimit)?,
                context: self.current.context(),
            }),
            Phase::FirstOpening | Phase::NextDelimiter | Phase::Drain => {
                let frame = self
                    .frames
                    .get(self.frame_index()?)
                    .and_then(Option::as_ref)
                    .ok_or(Error::InvalidState)?;
                Ok(Request::Body {
                    start: frame.position,
                    end: frame.end,
                })
            }
            Phase::Leaf => {
                let position = match self.decoder.as_ref() {
                    None => return Ok(Request::None),
                    Some(Decode::Base64 { position, .. }) => *position,
                    Some(Decode::QuotedPrintable(decoder)) => decoder.position(),
                };
                Ok(Request::Body {
                    start: self
                        .current
                        .body_start
                        .checked_add(position)
                        .ok_or(Error::InvalidRange)?,
                    end: self.current.entity_end,
                })
            }
            _ => Ok(Request::None),
        }
    }
    pub fn finish(
        mut self,
        now: Tick,
        work: &mut Meter,
        budget: &mut HeaderBudget,
    ) -> Result<&'w [Part], Error> {
        self.check_deadline(now, work, budget)?;
        if !matches!(self.phase, Phase::Complete) {
            return Err(Error::InvalidState);
        }
        self.parts.get(..self.used).ok_or(Error::InvalidState)
    }
    pub fn poll(
        &mut self,
        input: Input<'a, '_>,
        now: Tick,
        work: &mut Meter,
        budget: &mut HeaderBudget,
    ) -> Result<Status, Error> {
        self.check_deadline(now, work, budget)?;
        let result = self.step(input, now, work, budget);
        self.outcome(result)
    }
    fn append(&mut self, now: Tick, work: &mut Meter) -> Result<u16, Error> {
        if self.used >= self.max_parts {
            return Err(Error::PartLimit);
        }
        let ordinal = u16::try_from(self.used.checked_add(1).ok_or(Error::PartLimit)?)
            .map_err(|_| Error::PartLimit)?;
        let target = self.parts.get_mut(self.used).ok_or(Error::OutputCapacity)?;
        work.charge(
            now,
            Charge {
                output_bytes: u64::try_from(std::mem::size_of::<Part>())
                    .map_err(|_| Error::InvalidState)?,
                ..Charge::default()
            },
        )
        .map_err(Error::Work)?;
        self.current.ordinal = ordinal;
        *target = self.current;
        self.used += 1;
        Ok(ordinal)
    }
    fn finish_entity(
        &mut self,
        now: Tick,
        value: entity::Value,
        work: &mut Meter,
    ) -> Result<(), Error> {
        self.headers = self
            .headers
            .checked_add(value.selection.end.header_bytes)
            .ok_or(Error::HeaderLimit)?;
        if self.headers > self.max_headers {
            return Err(Error::HeaderLimit);
        }
        self.current.body_start = value.selection.end.body_start;
        self.current.size = self
            .current
            .entity_end
            .checked_sub(self.current.body_start)
            .ok_or(Error::InvalidRange)?;
        let classified = value.classified;
        self.current.media = classified.media;
        self.current.encoding = classified.encoding;
        self.current.type_start = classified.type_start;
        self.current.type_end = classified.type_end;
        self.current.diagnostics |= classified.diagnostics;
        if self.current.media == Media::Multipart {
            let boundary = value.boundary().ok_or(Error::InvalidState)?;
            let scanner = delimiter::Scanner::new(
                self.current.body_start,
                self.current.size,
                boundary,
                now,
                work,
            )
            .map_err(delimiter_error)?;
            let ordinal = self.append(now, work)?;
            let frame = Frame {
                scanner,
                position: self.current.body_start,
                end: self.current.entity_end,
                ordinal,
                next_start: None,
                closed: false,
                digest: classified.digest,
            };
            *self.frames.get_mut(self.depth).ok_or(Error::DepthLimit)? = Some(frame);
            self.depth += 1;
            self.phase = Phase::FirstOpening;
        } else {
            self.decoder = match self.current.encoding {
                Encoding::Identity => None,
                Encoding::Base64 => Some(Decode::Base64 {
                    decoder: base64::Decoder::default(),
                    position: 0,
                }),
                Encoding::QuotedPrintable => Some(Decode::QuotedPrintable(
                    quoted_printable::Decoder::new(self.current.size),
                )),
            };
            if self.decoder.is_some() {
                self.current.size = 0;
            }
            self.phase = Phase::Leaf;
        }
        Ok(())
    }
    fn frame_index(&self) -> Result<usize, Error> {
        self.depth.checked_sub(1).ok_or(Error::InvalidState)
    }
    fn frame(&mut self) -> Result<&mut Frame, Error> {
        let index = self.frame_index()?;
        self.frames
            .get_mut(index)
            .and_then(Option::as_mut)
            .ok_or(Error::InvalidState)
    }
    fn diagnostic(&mut self, ordinal: u16, flag: u8) -> Result<(), Error> {
        let index = usize::from(ordinal)
            .checked_sub(1)
            .ok_or(Error::InvalidState)?;
        let part = self.parts.get_mut(index).ok_or(Error::InvalidState)?;
        part.diagnostics |= flag;
        Ok(())
    }
    fn start_child(
        &mut self,
        start: u64,
        end: u64,
        parent: u16,
        digest: bool,
    ) -> Result<(), Error> {
        if start > end {
            return Err(Error::InvalidRange);
        }
        let depth = self.depth.checked_add(1).ok_or(Error::DepthLimit)?;
        if depth > self.max_depth {
            return Err(Error::DepthLimit);
        }
        if self.used >= self.max_parts {
            return Err(Error::PartLimit);
        }
        if self.parts.get(self.used).is_none() {
            return Err(Error::OutputCapacity);
        }
        self.current = Part {
            entity_start: start,
            entity_end: end,
            parent,
            diagnostics: if digest { DIGEST_CHILD_CONTEXT } else { 0 },
            depth: u8::try_from(depth).map_err(|_| Error::DepthLimit)?,
            ..Part::default()
        };
        self.entity = None;
        self.phase = Phase::Entity;
        Ok(())
    }
    fn delimiter(
        &mut self,
        input: &[u8],
        now: Tick,
        work: &mut Meter,
    ) -> Result<delimiter::Status, Error> {
        let frame = self.frame()?;
        let progress = frame
            .scanner
            .poll(input, now, work)
            .map_err(delimiter_error)?;
        frame.position = frame
            .position
            .checked_add(u64::try_from(progress.consumed).map_err(|_| Error::InvalidRange)?)
            .ok_or(Error::InvalidRange)?;
        Ok(match progress.status {
            delimiter::StreamStatus::Yield | delimiter::StreamStatus::NeedInput => {
                delimiter::Status::Yield
            }
            delimiter::StreamStatus::Delimiter(value) => delimiter::Status::Delimiter(value),
            delimiter::StreamStatus::Complete => delimiter::Status::Complete,
        })
    }
    fn step(
        &mut self,
        input: Input<'a, '_>,
        now: Tick,
        work: &mut Meter,
        budget: &mut HeaderBudget,
    ) -> Result<Status, Error> {
        let body = match (self.request()?, input) {
            (Request::None, Input::None) => &[][..],
            (
                Request::Headers {
                    start,
                    end,
                    limit,
                    context,
                },
                Input::Headers { bytes, source_end },
            ) => {
                let covered = start
                    .checked_add(u64::try_from(bytes.len()).map_err(|_| Error::InvalidRange)?)
                    .ok_or(Error::InvalidRange)?;
                if covered > end {
                    return Err(Error::InvalidRange);
                }
                if (source_end == SourceEnd::Eof) != (covered == end) {
                    return Err(Error::IncompleteSource);
                }
                self.entity = Some(entity::Core::new(entity::Input {
                    source: bytes,
                    base: start,
                    header_limit: limit,
                    context,
                    source_end,
                })?);
                &[][..]
            }
            (Request::Body { start, end }, Input::Body { offset, bytes }) => {
                if offset != start
                    || u64::try_from(bytes.len()).map_err(|_| Error::InvalidRange)?
                        > end.checked_sub(start).ok_or(Error::InvalidRange)?
                {
                    return Err(Error::InvalidRange);
                }
                bytes
            }
            _ => return Err(Error::InvalidState),
        };
        if matches!(self.phase, Phase::Complete) {
            return Ok(Status::Complete);
        }
        work.charge(
            now,
            Charge {
                records: 1,
                ..Charge::default()
            },
        )
        .map_err(Error::Work)?;
        match &mut self.phase {
            Phase::Entity => {
                let entity = self.entity.as_mut().ok_or(Error::InvalidState)?;
                if entity.poll(now, work, budget, &mut self.credit)? == Status::Complete {
                    let value = entity.value().ok_or(Error::InvalidState)?;
                    self.finish_entity(now, value, work)?;
                    self.entity = None;
                }
            }
            Phase::FirstOpening => match self.delimiter(body, now, work)? {
                delimiter::Status::Yield => {}
                delimiter::Status::Complete => return Err(Error::NotParsable),
                delimiter::Status::Delimiter(d) => {
                    if d.closing {
                        return Err(Error::NotParsable);
                    }
                    let ordinal = self.frame()?.ordinal;
                    if d.ignored_suffix {
                        self.diagnostic(ordinal, IGNORED_SUFFIX)?;
                    }
                    self.frame()?.next_start = Some(d.after_line);
                    self.phase = Phase::NextDelimiter;
                }
            },
            Phase::NextDelimiter => {
                let event = self.delimiter(body, now, work)?;
                if event == delimiter::Status::Yield {
                    return Ok(Status::Yield);
                }
                let frame = self.frame()?;
                let start = frame.next_start.ok_or(Error::InvalidState)?;
                let parent = frame.ordinal;
                let digest = frame.digest;
                let (end, next, closed, flag) = match event {
                    delimiter::Status::Delimiter(d) => (
                        d.preceding_end.max(start),
                        Some(d.after_line),
                        d.closing,
                        if d.ignored_suffix { IGNORED_SUFFIX } else { 0 },
                    ),
                    delimiter::Status::Complete => {
                        let index = usize::from(parent)
                            .checked_sub(1)
                            .ok_or(Error::InvalidState)?;
                        (
                            self.parts.get(index).ok_or(Error::InvalidState)?.entity_end,
                            None,
                            true,
                            MISSING_CLOSE,
                        )
                    }
                    delimiter::Status::Yield => return Err(Error::InvalidState),
                };
                let frame = self.frame()?;
                frame.next_start = next;
                frame.closed = closed;
                self.diagnostic(parent, flag)?;
                self.start_child(start, end, parent, digest)?;
            }
            Phase::Leaf => {
                let (written, complete, problem) = match self.decoder.as_mut() {
                    None => (0, true, false),
                    Some(Decode::Base64 { decoder, position }) => {
                        let body_length =
                            u64::try_from(body.len()).map_err(|_| Error::InvalidRange)?;
                        let last = self
                            .current
                            .body_start
                            .checked_add(*position)
                            .and_then(|start| start.checked_add(body_length))
                            .ok_or(Error::InvalidRange)?
                            == self.current.entity_end;
                        let p = decoder
                            .poll(body, &mut self.decoded, last, now, work)
                            .map_err(Error::Work)?;
                        *position = position
                            .checked_add(
                                u64::try_from(p.consumed).map_err(|_| Error::InvalidState)?,
                            )
                            .ok_or(Error::InvalidState)?;
                        (
                            p.written,
                            p.status == base64::Status::Complete,
                            decoder.is_encoding_problem(),
                        )
                    }
                    Some(Decode::QuotedPrintable(decoder)) => {
                        let p = decoder
                            .poll(body, &mut self.decoded, now, work)
                            .map_err(Error::Work)?;
                        (
                            p.written,
                            p.status == quoted_printable::Status::Complete,
                            decoder.is_encoding_problem(),
                        )
                    }
                };
                self.current.size = self
                    .current
                    .size
                    .checked_add(u64::try_from(written).map_err(|_| Error::InvalidState)?)
                    .ok_or(Error::InvalidState)?;
                if complete {
                    if problem {
                        self.current.diagnostics |= ENCODING_PROBLEM;
                    }
                    self.append(now, work)?;
                    self.decoder = None;
                    self.phase = Phase::Advance;
                }
            }
            Phase::Advance => {
                if self.depth == 0 {
                    self.phase = Phase::Complete;
                    return Ok(Status::Complete);
                }
                if self.frame()?.closed {
                    self.phase = Phase::Drain;
                } else {
                    self.phase = Phase::NextDelimiter;
                }
            }
            Phase::Drain => {
                if self.delimiter(body, now, work)? == delimiter::Status::Complete {
                    self.depth = self.depth.checked_sub(1).ok_or(Error::InvalidState)?;
                    let frame = self
                        .frames
                        .get_mut(self.depth)
                        .ok_or(Error::InvalidState)?
                        .take()
                        .ok_or(Error::InvalidState)?;
                    frame.scanner.finish(now, work).map_err(delimiter_error)?;
                    self.phase = Phase::Advance;
                }
            }
            Phase::Complete => return Ok(Status::Complete),
        }
        Ok(Status::Yield)
    }
}

const _: () = assert!(
    std::mem::size_of::<Cursor<'_, '_>>() + std::mem::size_of::<HeaderBudget>() <= 16 * 1024
);

#[cfg(test)]
#[path = "stream_tests.rs"]
mod tests;

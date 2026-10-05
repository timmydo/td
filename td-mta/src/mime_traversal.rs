//! Complete resident MIME structure; source authorization is external.
pub mod bound;
use crate::{
    admission::work::{Charge, Meter, Stop},
    decode_work,
    header_select::SourceEnd,
    limits::Limits,
    mime_base64, mime_delimiter, mime_metadata,
    mime_parameter::protocol,
    mime_qp,
    nfc::HeaderBudget,
    ports::Tick,
};
const MAX_DEPTH: usize = 64;
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Media {
    #[default]
    Other,
    TextPlain,
    MessageRfc822,
    Multipart,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Encoding {
    #[default]
    Identity,
    Base64,
    QuotedPrintable,
}
/// Passive structure evidence, never a blob ID or source authorization.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Part {
    pub entity_start: u64,
    pub entity_end: u64,
    pub body_start: u64,
    pub size: u64,
    pub type_start: u64,
    pub type_end: u64,
    pub ordinal: u16,
    /// Zero for the root; otherwise the parent's preorder ordinal.
    pub parent: u16,
    pub depth: u8,
    pub media: Media,
    pub encoding: Encoding,
    /// Parse-problem flags plus DIGEST_CHILD_CONTEXT; use problems() for errors.
    pub diagnostics: u8,
}
pub const UNKNOWN_ENCODING: u8 = 1;
pub const ENCODING_PROBLEM: u8 = 2;
pub const MISSING_CLOSE: u8 = 4;
pub const IGNORED_SUFFIX: u8 = 8;
pub const PARAMETER_PROBLEM: u8 = 16;
/// Context evidence, not a parse problem; independent of the selected MIME type.
pub const DIGEST_CHILD_CONTEXT: u8 = 32;
pub const PROBLEM_FLAGS: u8 =
    UNKNOWN_ENCODING | ENCODING_PROBLEM | MISSING_CLOSE | IGNORED_SUFFIX | PARAMETER_PROBLEM;
impl Part {
    /// Only parse-problem bits; context evidence does not imply a malformed part.
    pub const fn problems(&self) -> u8 {
        self.diagnostics & PROBLEM_FLAGS
    }
    /// Original header default context; passive evidence grants no source authority.
    pub const fn context(&self) -> mime_metadata::Context {
        if self.diagnostics & DIGEST_CHILD_CONTEXT != 0 {
            mime_metadata::Context::DigestChild
        } else {
            mime_metadata::Context::Normal
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidRange,
    IncompleteSource,
    InvalidLimits,
    InvalidState,
    DepthLimit,
    PartLimit,
    HeaderLimit,
    OutputCapacity,
    NotParsable,
    Work(Stop),
    InterpretationLimit,
    Metadata(mime_metadata::Error),
    Parameter(protocol::Error),
    Delimiter(mime_delimiter::Error),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidRange => "invalid MIME entity range",
            Self::IncompleteSource => "incomplete MIME entity source",
            Self::InvalidLimits => "invalid MIME traversal limits",
            Self::InvalidState => "invalid MIME traversal state",
            Self::DepthLimit => "MIME depth limit",
            Self::PartLimit => "MIME part limit",
            Self::HeaderLimit => "MIME aggregate header limit",
            Self::OutputCapacity => "MIME descriptor capacity",
            Self::NotParsable => "MIME structure is not parsable",
            Self::Work(_) => "MIME traversal work refusal",
            Self::InterpretationLimit => "MIME header interpretation limit",
            Self::Metadata(_) => "MIME metadata refusal",
            Self::Parameter(_) => "MIME boundary parameter refusal",
            Self::Delimiter(_) => "MIME delimiter refusal",
        })
    }
}
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Work(error) => Some(error),
            Self::Metadata(error) => Some(error),
            Self::Parameter(error) => Some(error),
            Self::Delimiter(error) => Some(error),
            _ => None,
        }
    }
}
impl From<decode_work::Error> for Error {
    fn from(error: decode_work::Error) -> Self {
        match error {
            decode_work::Error::Work(stop) => Self::Work(stop),
            decode_work::Error::InterpretationLimit => Self::InterpretationLimit,
            decode_work::Error::InvalidState => Self::InvalidState,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Yield,
    Complete,
}
struct Frame<'a> {
    scanner: mime_delimiter::Core<'a>,
    boundary: [u8; 70],
    length: u8,
    ordinal: u16,
    next_start: Option<u64>,
    closed: bool,
    digest: bool,
}
enum Phase<'a> {
    Headers(mime_metadata::Cursor<'a>),
    Boundary(protocol::Reader<'a>),
    FirstOpening,
    NextDelimiter,
    Leaf,
    Advance,
    Complete,
}
enum Decode {
    Base64 {
        decoder: mime_base64::Decoder,
        position: usize,
    },
    QuotedPrintable(mime_qp::Decoder),
}
/// Original job and email admission stay exclusively borrowed through completion.
/// Input must be the complete authorized entity, never a captured prefix.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mta::mime_traversal::Cursor<'_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mta::mime_traversal::Cursor<'_, '_>>();
/// ```
pub struct Cursor<'a, 'w> {
    source: &'a [u8],
    base: u64,
    work: &'w mut Meter,
    budget: &'w mut HeaderBudget,
    credit: u8,
    parts: &'w mut [Part],
    used: usize,
    max_parts: usize,
    max_depth: usize,
    max_headers: u64,
    headers: u64,
    frames: [Option<Frame<'a>>; MAX_DEPTH],
    depth: usize,
    current: Part,
    phase: Phase<'a>,
    boundary: [u8; 70],
    boundary_len: usize,
    digest: bool,
    decoder: Option<Decode>,
    decoded: [u8; 32],
    failure: Option<Error>,
}
impl<'a, 'w> Cursor<'a, 'w> {
    pub fn new(
        source: &'a [u8],
        base: u64,
        source_end: SourceEnd,
        limits: &Limits,
        parts: &'w mut [Part],
        work: &'w mut Meter,
        budget: &'w mut HeaderBudget,
    ) -> Result<Self, Error> {
        if source_end != SourceEnd::Eof {
            return Err(Error::IncompleteSource);
        }
        if !(1..=MAX_DEPTH).contains(&limits.mime_depth)
            || !(1..=4096).contains(&limits.mime_parts)
            || !(1..=1024 * 1024).contains(&limits.header_bytes)
            || limits.mime_depth > limits.mime_parts
        {
            return Err(Error::InvalidLimits);
        }
        let end = base
            .checked_add(u64::try_from(source.len()).map_err(|_| Error::InvalidRange)?)
            .ok_or(Error::InvalidRange)?;
        let max_headers = u64::try_from(limits.header_bytes).map_err(|_| Error::InvalidLimits)?;
        let metadata = mime_metadata::Cursor::new(
            source,
            base,
            max_headers,
            mime_metadata::Context::Normal,
            SourceEnd::Eof,
        )
        .map_err(Self::metadata_error)?;
        Ok(Self {
            source,
            base,
            work,
            budget,
            credit: 0,
            parts,
            used: 0,
            max_parts: limits.mime_parts,
            max_depth: limits.mime_depth,
            max_headers,
            headers: 0,
            frames: std::array::from_fn(|_| None),
            depth: 0,
            current: Part {
                entity_start: base,
                entity_end: end,
                depth: 1,
                ..Part::default()
            },
            phase: Phase::Headers(metadata),
            boundary: [0; 70],
            boundary_len: 0,
            digest: false,
            decoder: None,
            decoded: [0; 32],
            failure: None,
        })
    }
    fn metadata_error(error: mime_metadata::Error) -> Error {
        match error {
            mime_metadata::Error::Work(stop) => Error::Work(stop),
            mime_metadata::Error::InterpretationLimit => Error::InterpretationLimit,
            mime_metadata::Error::Headers(crate::mime_headers::Error::HeaderLimit) => {
                Error::HeaderLimit
            }
            error => Error::Metadata(error),
        }
    }
    fn parameter_error(error: protocol::Error) -> Error {
        match error {
            protocol::Error::Parameter(crate::mime_parameter::Error::Work(stop)) => {
                Error::Work(stop)
            }
            protocol::Error::Parameter(crate::mime_parameter::Error::InterpretationLimit) => {
                Error::InterpretationLimit
            }
            error => Error::Parameter(error),
        }
    }
    fn outcome<T>(&mut self, result: Result<T, Error>) -> Result<T, Error> {
        if let Err(error) = result {
            self.failure = Some(error);
            self.used = 0;
        }
        result
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self
            .budget
            .charge(self.work, now, 0, 0, &mut self.credit)
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
    pub fn finish(
        mut self,
        now: Tick,
    ) -> Result<(&'w [Part], &'w mut Meter, &'w mut HeaderBudget), Error> {
        self.check_deadline(now)?;
        if !matches!(self.phase, Phase::Complete) {
            return Err(Error::InvalidState);
        }
        let parts: &'w [Part] = self.parts;
        Ok((
            parts.get(..self.used).ok_or(Error::InvalidState)?,
            self.work,
            self.budget,
        ))
    }
    pub fn poll(&mut self, now: Tick) -> Result<Status, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if matches!(self.phase, Phase::Complete) {
            return Ok(Status::Complete);
        }
        self.check_deadline(now)?;
        let result = self.step(now);
        self.outcome(result)
    }
    fn range(&self, start: u64, end: u64) -> Result<&'a [u8], Error> {
        td_header::resident::slice(self.source, self.base, start..end).ok_or(Error::InvalidRange)
    }
    fn head(&self, selected: mime_metadata::Selected, second: bool) -> Result<&'a [u8], Error> {
        let raw = self.range(selected.field.value_start, selected.field.value_end)?;
        let extent = if second {
            selected.head.second.ok_or(Error::InvalidState)?
        } else {
            selected.head.first
        };
        raw.get(extent.start..extent.end).ok_or(Error::InvalidState)
    }
    fn equal(&mut self, now: Tick, source: &[u8], wanted: &[u8]) -> Result<bool, Error> {
        if source.len() != wanted.len() {
            return Ok(false);
        }
        let count = u64::try_from(source.len()).map_err(|_| Error::InvalidState)?;
        self.budget
            .charge(self.work, now, count, count, &mut self.credit)
            .map_err(decode_work::Error::from)?;
        Ok(source.eq_ignore_ascii_case(wanted))
    }
    fn append(&mut self, now: Tick) -> Result<u16, Error> {
        if self.used >= self.max_parts {
            return Err(Error::PartLimit);
        }
        let ordinal = u16::try_from(self.used.checked_add(1).ok_or(Error::PartLimit)?)
            .map_err(|_| Error::PartLimit)?;
        let target = self.parts.get_mut(self.used).ok_or(Error::OutputCapacity)?;
        self.work
            .charge(
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
    fn finish_headers(
        &mut self,
        now: Tick,
        selected: mime_metadata::Selection,
    ) -> Result<(), Error> {
        self.headers = self
            .headers
            .checked_add(selected.end.header_bytes)
            .ok_or(Error::HeaderLimit)?;
        if self.headers > self.max_headers {
            return Err(Error::HeaderLimit);
        }
        self.current.body_start = selected.end.body_start;
        self.current.size = self
            .current
            .entity_end
            .checked_sub(self.current.body_start)
            .ok_or(Error::InvalidRange)?;
        self.digest = false;
        self.current.media = match selected.content_type {
            mime_metadata::ContentType::Default(mime_metadata::DefaultType::TextPlain) => {
                Media::TextPlain
            }
            mime_metadata::ContentType::Default(mime_metadata::DefaultType::MessageRfc822) => {
                Media::MessageRfc822
            }
            mime_metadata::ContentType::Field(value) => {
                self.current.type_start = value.field.value_start;
                self.current.type_end = value.field.value_end;
                let first = self.head(value, false)?;
                let second = self.head(value, true)?;
                if self.equal(now, first, b"multipart")? {
                    self.digest = self.equal(now, second, b"digest")?;
                    Media::Multipart
                } else if self.equal(now, first, b"text")? && self.equal(now, second, b"plain")? {
                    Media::TextPlain
                } else if self.equal(now, first, b"message")?
                    && self.equal(now, second, b"rfc822")?
                {
                    Media::MessageRfc822
                } else {
                    Media::Other
                }
            }
        };
        self.current.encoding = if let Some(value) = selected.transfer_encoding {
            let name = self.head(value, false)?;
            if self.equal(now, name, b"base64")? {
                Encoding::Base64
            } else if self.equal(now, name, b"quoted-printable")? {
                Encoding::QuotedPrintable
            } else {
                if !(self.equal(now, name, b"7bit")?
                    || self.equal(now, name, b"8bit")?
                    || self.equal(now, name, b"binary")?)
                {
                    self.current.diagnostics |= UNKNOWN_ENCODING;
                }
                Encoding::Identity
            }
        } else {
            Encoding::Identity
        };
        if self.current.media == Media::Multipart {
            if self.current.encoding != Encoding::Identity {
                return Err(Error::NotParsable);
            }
            let raw = self.range(self.current.type_start, self.current.type_end)?;
            self.boundary_len = 0;
            self.phase = Phase::Boundary(protocol::Reader::new(raw, protocol::Purpose::Boundary));
        } else {
            self.decoder = match self.current.encoding {
                Encoding::Identity => None,
                Encoding::Base64 => Some(Decode::Base64 {
                    decoder: mime_base64::Decoder::default(),
                    position: 0,
                }),
                Encoding::QuotedPrintable => Some(Decode::QuotedPrintable(mime_qp::Decoder::new(
                    self.current.size,
                ))),
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
    fn frame(&mut self) -> Result<&mut Frame<'a>, Error> {
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
        let source = self.range(start, end)?;
        let remaining = self
            .max_headers
            .checked_sub(self.headers)
            .ok_or(Error::HeaderLimit)?;
        let context = if digest {
            mime_metadata::Context::DigestChild
        } else {
            mime_metadata::Context::Normal
        };
        let metadata =
            mime_metadata::Cursor::new(source, start, remaining, context, SourceEnd::Eof)
                .map_err(Self::metadata_error)?;
        self.current = Part {
            entity_start: start,
            entity_end: end,
            parent,
            diagnostics: if digest { DIGEST_CHILD_CONTEXT } else { 0 },
            depth: u8::try_from(depth).map_err(|_| Error::DepthLimit)?,
            ..Part::default()
        };
        self.phase = Phase::Headers(metadata);
        Ok(())
    }
    fn delimiter(&mut self, now: Tick) -> Result<mime_delimiter::Status, Error> {
        let index = self.frame_index()?;
        let frame = self
            .frames
            .get_mut(index)
            .and_then(Option::as_mut)
            .ok_or(Error::InvalidState)?;
        let boundary = frame
            .boundary
            .get(..usize::from(frame.length))
            .ok_or(Error::InvalidState)?;
        frame
            .scanner
            .poll(now, self.work, boundary)
            .map_err(|e| match e {
                mime_delimiter::Error::Work(stop) => Error::Work(stop),
                e => Error::Delimiter(e),
            })
    }
    fn step(&mut self, now: Tick) -> Result<Status, Error> {
        self.work
            .charge(
                now,
                Charge {
                    records: 1,
                    ..Charge::default()
                },
            )
            .map_err(Error::Work)?;
        match &mut self.phase {
            Phase::Headers(metadata) => {
                if metadata
                    .poll_in_context(now, self.work, self.budget, &mut self.credit)
                    .map_err(Self::metadata_error)?
                    == mime_metadata::Status::Complete
                {
                    let selected = metadata
                        .selection()
                        .map_err(Self::metadata_error)?
                        .ok_or(Error::InvalidState)?;
                    self.finish_headers(now, selected)?;
                }
            }
            Phase::Boundary(reader) => {
                match reader
                    .poll(
                        now,
                        &mut decode_work::Parsing::new(self.work, self.budget, &mut self.credit),
                    )
                    .map_err(Self::parameter_error)?
                {
                    protocol::Read::Yield => {}
                    protocol::Read::Octet(byte) => {
                        let target = self
                            .boundary
                            .get_mut(self.boundary_len)
                            .ok_or(Error::InvalidState)?;
                        self.work
                            .charge(
                                now,
                                Charge {
                                    output_bytes: 1,
                                    ..Charge::default()
                                },
                            )
                            .map_err(Error::Work)?;
                        *target = byte;
                        self.boundary_len += 1;
                    }
                    protocol::Read::Complete(end) => {
                        if end.value != protocol::Value::Present {
                            return Err(Error::NotParsable);
                        }
                        if end.bytes != self.boundary_len {
                            return Err(Error::InvalidState);
                        }
                        if end.unsupported_qualifier || end.selection.invalid_extended {
                            self.current.diagnostics |= PARAMETER_PROBLEM;
                        }
                        let body = self.range(self.current.body_start, self.current.entity_end)?;
                        let scanner = mime_delimiter::Core::new(
                            body,
                            self.current.body_start,
                            self.boundary_len,
                        )
                        .map_err(Error::Delimiter)?;
                        let ordinal = self.append(now)?;
                        let frame = Frame {
                            scanner,
                            boundary: self.boundary,
                            length: u8::try_from(self.boundary_len)
                                .map_err(|_| Error::InvalidState)?,
                            ordinal,
                            next_start: None,
                            closed: false,
                            digest: self.digest,
                        };
                        *self.frames.get_mut(self.depth).ok_or(Error::DepthLimit)? = Some(frame);
                        self.depth += 1;
                        self.phase = Phase::FirstOpening;
                    }
                }
            }
            Phase::FirstOpening => match self.delimiter(now)? {
                mime_delimiter::Status::Yield => {}
                mime_delimiter::Status::Complete => return Err(Error::NotParsable),
                mime_delimiter::Status::Delimiter(d) => {
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
                let event = self.delimiter(now)?;
                if event == mime_delimiter::Status::Yield {
                    return Ok(Status::Yield);
                }
                let frame = self.frame()?;
                let start = frame.next_start.ok_or(Error::InvalidState)?;
                let parent = frame.ordinal;
                let digest = frame.digest;
                let (end, next, closed, flag) = match event {
                    mime_delimiter::Status::Delimiter(d) => (
                        d.preceding_end.max(start),
                        Some(d.after_line),
                        d.closing,
                        if d.ignored_suffix { IGNORED_SUFFIX } else { 0 },
                    ),
                    mime_delimiter::Status::Complete => {
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
                    mime_delimiter::Status::Yield => return Err(Error::InvalidState),
                };
                let frame = self.frame()?;
                frame.next_start = next;
                frame.closed = closed;
                self.diagnostic(parent, flag)?;
                self.start_child(start, end, parent, digest)?;
            }
            Phase::Leaf => {
                let body = self.range(self.current.body_start, self.current.entity_end)?;
                let (written, complete, problem) = match self.decoder.as_mut() {
                    None => (0, true, false),
                    Some(Decode::Base64 { decoder, position }) => {
                        let input = body.get(*position..).ok_or(Error::InvalidState)?;
                        let p = decoder
                            .poll(input, &mut self.decoded, true, now, self.work)
                            .map_err(Error::Work)?;
                        *position = position
                            .checked_add(p.consumed)
                            .ok_or(Error::InvalidState)?;
                        (
                            p.written,
                            p.status == mime_base64::Status::Complete,
                            decoder.is_encoding_problem(),
                        )
                    }
                    Some(Decode::QuotedPrintable(decoder)) => {
                        let position =
                            usize::try_from(decoder.position()).map_err(|_| Error::InvalidState)?;
                        let input = body.get(position..).ok_or(Error::InvalidState)?;
                        let p = decoder
                            .poll(input, &mut self.decoded, now, self.work)
                            .map_err(Error::Work)?;
                        (
                            p.written,
                            p.status == mime_qp::Status::Complete,
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
                    self.append(now)?;
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
                    self.depth = self.depth.checked_sub(1).ok_or(Error::InvalidState)?;
                    *self.frames.get_mut(self.depth).ok_or(Error::InvalidState)? = None;
                } else {
                    self.phase = Phase::NextDelimiter;
                }
            }
            Phase::Complete => return Ok(Status::Complete),
        }
        Ok(Status::Yield)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::ports::Deadline;
    fn meter() -> Meter {
        Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 100_000_000,
                records: 10_000_000,
                output_bytes: 10_000_000,
                ..Charge::default()
            },
        )
    }
    fn drain(cursor: &mut Cursor<'_, '_>) -> Result<(), Error> {
        for _ in 0..1_000_000 {
            let before = (
                cursor.work.remaining(),
                cursor.budget.source_bytes_remaining(),
                cursor.budget.steps_remaining(),
            );
            let result = cursor.poll(Tick(1));
            let after = (
                cursor.work.remaining(),
                cursor.budget.source_bytes_remaining(),
                cursor.budget.steps_remaining(),
            );
            assert!(before.0.io_bytes - after.0.io_bytes <= 352);
            assert!(before.0.records - after.0.records <= 32);
            assert!(before.0.output_bytes - after.0.output_bytes <= 96);
            assert!(before.1 - after.1 <= 352);
            assert!(before.2 - after.2 <= 512);
            if result? == Status::Complete {
                return Ok(());
            }
            assert!(cursor.parts().unwrap().is_none());
        }
        panic!("traversal did not finish")
    }
    fn parse(source: &[u8]) -> Result<(Vec<Part>, u64), Error> {
        let mut backing = [Part::default(); 64];
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(
            source,
            17,
            SourceEnd::Eof,
            &Limits::default(),
            &mut backing,
            &mut work,
            &mut budget,
        )?;
        drain(&mut cursor)?;
        Ok((
            cursor.parts()?.unwrap().to_vec(),
            cursor.header_bytes().unwrap(),
        ))
    }
    #[test]
    fn nested_preorder_exact_extents_sizes_and_digest_defaults() {
        const {
            assert!(std::mem::size_of::<Part>() <= 64);
        }
        assert!(
            std::mem::size_of::<Cursor<'_, '_>>() + std::mem::size_of::<HeaderBudget>()
                <= 16 * 1024
        );
        let source = concat!(
            "Content-Type: multipart/mixed; boundary=a\r\n\r\npre\r\n--a\r\n",
            "Content-Type: multipart/digest; boundary=b\r\n\r\n--b\r\n",
            "\r\nFrom: nested\r\n\r\nbody\r\n--b--\r\nepi\r\n--a\r\n",
            "Content-Transfer-Encoding: base64\r\n\r\nYWJj\r\n--a\r\n",
            "Content-Transfer-Encoding: quoted-printable\r\n\r\nx=20y=\r\n--a--\r\nepi"
        )
        .as_bytes();
        let (parts, headers) = parse(source).unwrap();
        assert_eq!(parts.len(), 5);
        assert_eq!(
            parts.iter().map(|part| part.context()).collect::<Vec<_>>(),
            [
                mime_metadata::Context::Normal,
                mime_metadata::Context::Normal,
                mime_metadata::Context::DigestChild,
                mime_metadata::Context::Normal,
                mime_metadata::Context::Normal,
            ]
        );
        assert_eq!(
            parts
                .iter()
                .map(|p| (p.ordinal, p.parent, p.depth, p.media, p.size))
                .collect::<Vec<_>>(),
            [
                (
                    1,
                    0,
                    1,
                    Media::Multipart,
                    (source.len() - source.windows(4).position(|w| w == b"\r\n\r\n").unwrap() - 4)
                        as u64
                ),
                (
                    2,
                    1,
                    2,
                    Media::Multipart,
                    b"--b\r\n\r\nFrom: nested\r\n\r\nbody\r\n--b--\r\nepi".len() as u64,
                ),
                (3, 2, 3, Media::MessageRfc822, 20),
                (4, 1, 2, Media::TextPlain, 3),
                (5, 1, 2, Media::TextPlain, 3),
            ]
        );
        // Independent offsets selected from literal separator/delimiter positions.
        let first_a = source.windows(5).position(|w| w == b"--a\r\n").unwrap();
        let second_a = source
            .windows(5)
            .enumerate()
            .skip(first_a + 5)
            .find(|(_, w)| *w == b"--a\r\n")
            .unwrap()
            .0;
        assert_eq!(parts[1].entity_start, 17 + first_a as u64 + 5);
        assert_eq!(parts[1].entity_end, 17 + second_a as u64 - 2);
        assert_eq!(parts[1].size, parts[1].entity_end - parts[1].body_start);
        assert_eq!(parts[2].size, parts[2].entity_end - parts[2].body_start);
        assert_eq!(headers, 43 + 44 + 35 + 45);
        assert_eq!(parts[4].diagnostics & ENCODING_PROBLEM, ENCODING_PROBLEM);
    }
    #[test]
    fn retained_metadata_replays_original_digest_context_and_absolute_ranges() {
        use crate::{mime_part_headers, nfc::Scratch};
        use mime_part_headers::label_json;
        let source = concat!(
            "Content-Type: multipart/mixed; boundary=a\r\n\r\n--a\r\n",
            "Content-Type: multipart/digest; boundary=b\r\n\r\n--b\r\n",
            "Content-ID: <id@a>\r\nContent-Language: fr\r\nContent-Location: \r\n",
            "\r\nFrom: inner\r\n\r\nbody\r\n--b\r\n",
            "Content-Type: text/plain\r\nContent-Location: ../a\r\n",
            "Content-Transfer-Encoding: base64\r\n\r\nYQ!\r\n--b\r\n",
            "Content-Type: bad\r\nContent-Location: a%\r\n\r\nbody\r\n--b--\r\n--a\r\n",
            "\r\nContent-Type: image/png\r\nContent-Location: ../body\r\n--a--\r\n"
        )
        .as_bytes();
        let expected_types: [&[u8]; 6] = [
            b"multipart/mixed",
            b"multipart/digest",
            b"message/rfc822",
            b"text/plain",
            b"message/rfc822",
            b"text/plain",
        ];
        let expected_locations: [Option<&[u8]>; 6] =
            [None, None, Some(b"\"\""), Some(b"\"../a\""), None, None];
        for base in [0, 17, u64::MAX - source.len() as u64] {
            let mut parts = [Part::default(); 64];
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let work_ptr = std::ptr::from_mut(&mut work);
            let budget_ptr = std::ptr::from_mut(&mut budget);
            let mut cursor = Cursor::new(
                source,
                base,
                SourceEnd::Eof,
                &Limits::default(),
                &mut parts,
                &mut work,
                &mut budget,
            )
            .unwrap();
            drain(&mut cursor).unwrap();
            let (parts, work, budget) = cursor.finish(Tick(1)).unwrap();
            assert_eq!(parts.len(), expected_types.len());
            for (index, part) in parts.iter().enumerate() {
                let mut heads = [0; 256];
                let mut charset = [0; 256];
                let mut filename = [0; 256];
                let mut id = [0; 256];
                let mut language = [0; 256];
                let mut location = [0; 256];
                let mut scratch = Scratch::new();
                let scratch_ptr = std::ptr::from_mut(&mut scratch);
                let entity = mime_part_headers::Entity {
                    source: td_header::resident::slice(
                        source,
                        base,
                        part.entity_start..part.entity_end,
                    )
                    .unwrap(),
                    base: part.entity_start,
                    source_end: SourceEnd::Eof,
                    header_limit: Limits::default().header_bytes as u64,
                    context: part.context(),
                };
                let mut cursor = label_json::Cursor::new(
                    entity,
                    label_json::Backing {
                        headers: mime_part_headers::Backing {
                            heads: &mut heads,
                            charset: &mut charset,
                            filename: &mut filename,
                        },
                        labels: crate::mime_label_fields::json::Backing {
                            content_id: &mut id,
                            content_language: &mut language,
                        },
                        content_location: &mut location,
                    },
                    work,
                    budget,
                    &mut scratch,
                )
                .unwrap();
                let mut complete = false;
                for _ in 0..100_000 {
                    if cursor.poll(Tick(1)).unwrap() == mime_part_headers::Status::Complete {
                        complete = true;
                        break;
                    }
                    assert!(cursor.value().is_none());
                }
                assert!(complete);
                let (view, returned_work, returned_budget, returned_scratch) =
                    cursor.finish(Tick(1)).unwrap();
                assert_eq!(std::ptr::from_mut(returned_work), work_ptr);
                assert_eq!(std::ptr::from_mut(returned_budget), budget_ptr);
                assert_eq!(std::ptr::from_mut(returned_scratch), scratch_ptr);
                assert_eq!(view.headers.content_type, expected_types[index]);
                assert_eq!(view.headers.body_start, part.body_start);
                assert_eq!(view.location.selection.end.body_start, part.body_start);
                assert_eq!(view.location.value, expected_locations[index]);
                assert_eq!(
                    part.context(),
                    if (2..=4).contains(&index) {
                        mime_metadata::Context::DigestChild
                    } else {
                        mime_metadata::Context::Normal
                    }
                );
                if index == 3 {
                    assert_eq!(part.diagnostics, ENCODING_PROBLEM | DIGEST_CHILD_CONTEXT);
                }
                if index == 2 {
                    assert_eq!(view.labels.content_id, Some(b"\"id@a\"".as_slice()));
                    assert_eq!(view.labels.content_language, Some(b"[\"fr\"]".as_slice()));
                }
            }
        }
    }
    #[test]
    fn nested_digest_context_and_post_append_problems_remain_independent() {
        let source = concat!(
            "Content-Type: multipart/digest;boundary=a\n\n--a\n",
            "Content-Type: multipart/mixed;boundary=b\n\n--b\n\ninside\n",
            "--a\nContent-Type: multipart/digest;boundary=c\n\n",
            "--c-tail\n\ninner\n--c--\n--a--"
        )
        .as_bytes();
        let (parts, _) = parse(source).unwrap();
        assert_eq!(parts.len(), 5);
        let expected = [
            (mime_metadata::Context::Normal, Media::Multipart, 0),
            (
                mime_metadata::Context::DigestChild,
                Media::Multipart,
                MISSING_CLOSE,
            ),
            (mime_metadata::Context::Normal, Media::TextPlain, 0),
            (
                mime_metadata::Context::DigestChild,
                Media::Multipart,
                IGNORED_SUFFIX,
            ),
            (mime_metadata::Context::DigestChild, Media::MessageRfc822, 0),
        ];
        for (part, (context, media, problems)) in parts.iter().zip(expected) {
            assert_eq!(part.context(), context);
            assert_eq!(part.media, media);
            assert_eq!(part.problems(), problems);
            assert_eq!(part.diagnostics & PROBLEM_FLAGS, problems);
        }
        assert_eq!(parts[1].diagnostics, DIGEST_CHILD_CONTEXT | MISSING_CLOSE);
        assert_eq!(parts[3].diagnostics, DIGEST_CHILD_CONTEXT | IGNORED_SUFFIX);
        let context_only = Part {
            diagnostics: DIGEST_CHILD_CONTEXT | 64 | 128,
            ..Part::default()
        };
        assert_eq!(context_only.problems(), 0);
        assert_eq!(context_only.context(), mime_metadata::Context::DigestChild);
    }
    #[test]
    fn recovery_outer_precedence_empty_children_and_no_implicit_recursion() {
        for (source, count, media, diagnostics) in [
            (b"".as_slice(), 1, Media::TextPlain, 0),
            (
                concat!(
                    "Content-Type: message/global\n\n",
                    "Content-Type: multipart/mixed;boundary=x\n\n--x\n"
                )
                .as_bytes(),
                1,
                Media::Other,
                0,
            ),
            (
                b"Content-Type: multipart/mixed;boundary=x\n\n--x\n--x--",
                2,
                Media::Multipart,
                0,
            ),
            (
                b"Content-Type: multipart/mixed;boundary=x\n\n--x\n\nbody",
                2,
                Media::Multipart,
                MISSING_CLOSE,
            ),
            (
                b"Content-Type: multipart/mixed;boundary=x\n\n--x-tail\n\nbody\n--x--tail",
                2,
                Media::Multipart,
                IGNORED_SUFFIX,
            ),
        ] {
            let (parts, _) = parse(source).unwrap();
            assert_eq!(parts.len(), count);
            assert_eq!(parts[0].media, media);
            assert_eq!(parts[0].diagnostics, diagnostics);
        }
        for source in [
            b"Content-Type: multipart/mixed\n\nraw".as_slice(),
            b"Content-Type: multipart/mixed;boundary=x\n\n--x--",
            b"Content-Type: multipart/mixed;boundary=x\nContent-Transfer-Encoding: base64\n\nLS14",
            b"Content-Type: multipart/mixed;boundary=ok;boundary*=utf-8''bad%3B\n\n--ok\n",
            // Outer 'a' wins over 'ab', leaving the inner container no opening.
            concat!(
                "Content-Type: multipart/mixed;boundary=a\n\n--a\n",
                "Content-Type: multipart/mixed;boundary=ab\n\n",
                "--ab\n\nbody\n--ab--\n--a--"
            )
            .as_bytes(),
        ] {
            assert_eq!(parse(source), Err(Error::NotParsable));
        }
        let source = b"Content-Type: multipart/mixed;boundary=a\n\n--a\nContent-Type: multipart/mixed;boundary=b\n\n--b\n\ninside\n--a\n\nafter\n--a--";
        let (parts, _) = parse(source).unwrap();
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[1].diagnostics, MISSING_CLOSE);
        assert_eq!(parts[2].size, 6);
        assert_eq!(parts[3].size, 5);
        // Same-length patterns remain distinct across suspended frame state.
        assert_eq!(parts[2].parent, 2);
        assert_eq!(parts[3].parent, 1);
        let (parts, headers) =
            parse(b"Content-Type: multipart/mixed;boundary=x\r\n\r\n--x\r\nX: y\r\n--x--").unwrap();
        assert_eq!(headers, 42 + 4); // Delimiter-leading CRLF belongs to the delimiter.
        assert_eq!(parts[1].body_start, parts[1].entity_end);
        assert_eq!(parts[1].size, 0);
    }
    #[test]
    fn exact_transfer_sizes_ignore_charset_and_diagnose_unknown_or_malformed() {
        for (source, size, flag) in [
            (
                b"Content-Transfer-Encoding: base64\n\nYQ==".as_slice(),
                1,
                0,
            ),
            (
                b"Content-Transfer-Encoding: base64\n\nYQ",
                1,
                ENCODING_PROBLEM,
            ),
            (
                b"Content-Transfer-Encoding: quoted-printable\n\na=20b  \r\n",
                5,
                0,
            ),
            (
                b"Content-Transfer-Encoding: quoted-printable\n\nx=",
                1,
                ENCODING_PROBLEM,
            ),
            (
                b"Content-Transfer-Encoding: custom\n\n\xff\0\r\n",
                4,
                UNKNOWN_ENCODING,
            ),
            (b"Content-Type: text/plain; charset=unknown\n\n\xff", 1, 0),
            (
                b"Content-Type: invalid;\nContent-Type: text/plain\n\nabc",
                3,
                0,
            ),
        ] {
            let (parts, _) = parse(source).unwrap();
            assert_eq!(parts[0].size, size);
            assert_eq!(parts[0].diagnostics, flag);
        }
        let run = format!(
            "Content-Transfer-Encoding: quoted-printable\n\na{}b{}\n",
            " ".repeat(5000),
            " ".repeat(5000)
        );
        let (parts, _) = parse(run.as_bytes()).unwrap();
        assert_eq!(parts[0].size, 5003);
        assert_eq!(parts[0].diagnostics, ENCODING_PROBLEM);
    }
    #[test]
    fn boundary_length_and_complete_head_invariants_refuse_input_cleanly() {
        for length in [0, 1, 69, 70, 71, 200] {
            let boundary = "x".repeat(length);
            let source=format!("Content-Type: multipart/mixed;boundary=\"{boundary}\"\n\n--{boundary}\n\na\n--{boundary}--");
            let result = parse(source.as_bytes());
            if (1..=70).contains(&length) {
                assert_eq!(result.unwrap().0.len(), 2);
            } else {
                assert_eq!(result, Err(Error::NotParsable));
            }
        }
        for source in [
            b"Content-Type: multipart\n\nraw".as_slice(),
            b"Content-Type: multipart/\n\nraw",
        ] {
            let (parts, _) = parse(source).unwrap();
            assert_eq!(parts.len(), 1);
            assert_eq!(parts[0].media, Media::TextPlain);
            assert_eq!(parts[0].size, 3);
        }
    }
    #[test]
    fn diagnostics_case_spelling_type_spans_and_incomplete_handoff() {
        for parameter in ["boundary=x;boundary*=utf-8''%zz", "boundary*=''x"] {
            let source = format!("Content-Type: multipart/mixed;{parameter}\n\n--x\n\na\n--x--");
            let (parts, _) = parse(source.as_bytes()).unwrap();
            assert_eq!(parts[0].diagnostics, PARAMETER_PROBLEM);
        }
        for label in ["7bit", "8bit", "binary"] {
            let source = format!("Content-Transfer-Encoding: {label}\n\nabc");
            let (parts, _) = parse(source.as_bytes()).unwrap();
            assert_eq!(parts[0].diagnostics, 0);
            assert_eq!(parts[0].size, 3);
            assert_eq!((parts[0].type_start, parts[0].type_end), (0, 0));
        }
        let source = concat!(
            "Content-Type: MULTIPART/Mixed;boundary=x\n",
            "Content-Transfer-Encoding: X-CUSTOM\n\n--x\n",
            "Content-Transfer-Encoding: BASE64\n\nYQ==\n--x--"
        )
        .as_bytes();
        let (parts, _) = parse(source).unwrap();
        assert_eq!(parts[0].media, Media::Multipart);
        assert_eq!(parts[0].diagnostics, UNKNOWN_ENCODING);
        assert_eq!(parts[1].size, 1);
        assert_eq!(parts[1].encoding, Encoding::Base64);
        let start = (parts[0].type_start - 17) as usize;
        let end = (parts[0].type_end - 17) as usize;
        assert_eq!(&source[start..end], b" MULTIPART/Mixed;boundary=x");
        let mut parts = [Part::default(); 8];
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let cursor = Cursor::new(
            source,
            0,
            SourceEnd::Eof,
            &Limits::default(),
            &mut parts,
            &mut work,
            &mut budget,
        )
        .unwrap();
        assert!(matches!(cursor.finish(Tick(1)), Err(Error::InvalidState)));
        // This isolates the largest header/parameter and maximal validation phases.
        let source = format!(
            concat!(
                "X: {}\nContent-Type: multipart/mixed;",
                " boundary*0*=utf-8''{}; boundary*1={}\n\n",
                "--{}\n\na\n--{}--"
            ),
            "a".repeat(8192),
            "x".repeat(35),
            "x".repeat(35),
            "x".repeat(70),
            "x".repeat(70)
        );
        assert_eq!(parse(source.as_bytes()).unwrap().0.len(), 2);
        // 32 counted octets followed by EOF can copy a cell in the same turn.
        let source = format!(
            "Content-Transfer-Encoding: base64\n\n{}YQ==",
            "YWJj".repeat(21)
        );
        assert_eq!(parse(source.as_bytes()).unwrap().0[0].size, 64);
    }
    #[test]
    fn original_work_and_live_deadline_cuts_hide_all_provisional_parts() {
        let source = b"Content-Type: multipart/mixed;boundary=x\n\n--x\n\na\n--x--";
        let mut parts = [Part::default(); 8];
        let mut work = meter();
        let initial = work.remaining();
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(
            source,
            0,
            SourceEnd::Eof,
            &Limits::default(),
            &mut parts,
            &mut work,
            &mut budget,
        )
        .unwrap();
        let mut turns = 0;
        loop {
            let before = (
                cursor.work.remaining(),
                cursor.budget.source_bytes_remaining(),
                cursor.budget.steps_remaining(),
            );
            turns += 1;
            let status = cursor.poll(Tick(1)).unwrap();
            let after = (
                cursor.work.remaining(),
                cursor.budget.source_bytes_remaining(),
                cursor.budget.steps_remaining(),
            );
            assert!(before.0.io_bytes - after.0.io_bytes <= 352);
            assert!(before.0.records - after.0.records <= 32);
            assert!(before.0.output_bytes - after.0.output_bytes <= 96);
            assert!(before.1 - after.1 <= 352);
            assert!(before.2 - after.2 <= 512);
            if status == Status::Complete {
                break;
            }
        }
        let left = cursor.work.remaining();
        let used = Charge {
            io_bytes: initial.io_bytes - left.io_bytes,
            records: initial.records - left.records,
            output_bytes: initial.output_bytes - left.output_bytes,
            ..Charge::default()
        };
        let identity = (
            std::ptr::from_ref(cursor.work),
            std::ptr::from_ref(cursor.budget),
        );
        assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
        let (retained, work, budget) = cursor.finish(Tick(1)).unwrap();
        assert_eq!(retained.len(), 2);
        assert_eq!(
            identity,
            (std::ptr::from_ref(work), std::ptr::from_ref(budget))
        );
        for kind in 0..3 {
            let amount = match kind {
                0 => used.io_bytes,
                1 => used.records,
                _ => used.output_bytes,
            };
            for limit in 0..amount {
                let mut caps = used;
                let stop = match kind {
                    0 => {
                        caps.io_bytes = limit;
                        Stop::IoBytes
                    }
                    1 => {
                        caps.records = limit;
                        Stop::Records
                    }
                    _ => {
                        caps.output_bytes = limit;
                        Stop::OutputBytes
                    }
                };
                let mut work = Meter::new(Deadline::after(Tick(0), 100).unwrap(), caps);
                let mut budget = HeaderBudget::new();
                let mut cursor = Cursor::new(
                    source,
                    0,
                    SourceEnd::Eof,
                    &Limits::default(),
                    &mut parts,
                    &mut work,
                    &mut budget,
                )
                .unwrap();
                let error = drain(&mut cursor).unwrap_err();
                assert_eq!(error, Error::Work(stop));
                assert_eq!(cursor.parts(), Err(error));
                assert_eq!(cursor.header_bytes(), None);
                let before = (
                    cursor.work.remaining(),
                    cursor.budget.source_bytes_remaining(),
                    cursor.budget.steps_remaining(),
                );
                assert_eq!(cursor.poll(Tick(1)), Err(error));
                assert_eq!(cursor.check_deadline(Tick(1)), Err(error));
                assert_eq!(
                    before,
                    (
                        cursor.work.remaining(),
                        cursor.budget.source_bytes_remaining(),
                        cursor.budget.steps_remaining()
                    )
                );
                assert!(cursor.finish(Tick(1)).is_err());
            }
        }
        for cut in 0..=turns {
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let mut cursor = Cursor::new(
                source,
                0,
                SourceEnd::Eof,
                &Limits::default(),
                &mut parts,
                &mut work,
                &mut budget,
            )
            .unwrap();
            for _ in 0..cut {
                let _ = cursor.poll(Tick(1)).unwrap();
            }
            assert_eq!(
                cursor.check_deadline(Tick(100)),
                Err(Error::Work(Stop::Deadline))
            );
            assert_eq!(cursor.parts(), Err(Error::Work(Stop::Deadline)));
            assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(Stop::Deadline)));
        }
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(
            source,
            0,
            SourceEnd::Eof,
            &Limits::default(),
            &mut parts,
            &mut work,
            &mut budget,
        )
        .unwrap();
        drain(&mut cursor).unwrap();
        assert!(matches!(
            cursor.finish(Tick(100)),
            Err(Error::Work(Stop::Deadline))
        ));
    }
    #[test]
    fn structural_limits_ranges_and_caller_capacity_never_publish_prefixes() {
        let source = b"Content-Type: multipart/mixed;boundary=x\n\n--x\nX: y\n\nbody\n--x--";
        let mut parts = [Part::default(); 8];
        let header_total = b"Content-Type: multipart/mixed;boundary=x\n".len() + b"X: y\n".len();
        for (limits, capacity, expected) in [
            (
                Limits {
                    mime_depth: 1,
                    ..Limits::default()
                },
                8,
                Error::DepthLimit,
            ),
            (
                Limits {
                    mime_depth: 1,
                    mime_parts: 1,
                    ..Limits::default()
                },
                8,
                Error::DepthLimit,
            ),
            (
                Limits {
                    mime_depth: 2,
                    mime_parts: 2,
                    ..Limits::default()
                },
                1,
                Error::OutputCapacity,
            ),
            (Limits::default(), 0, Error::OutputCapacity),
            (
                Limits {
                    header_bytes: header_total - 1,
                    ..Limits::default()
                },
                8,
                Error::HeaderLimit,
            ),
        ] {
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let mut cursor = Cursor::new(
                source,
                0,
                SourceEnd::Eof,
                &limits,
                &mut parts[..capacity],
                &mut work,
                &mut budget,
            )
            .unwrap();
            assert_eq!(drain(&mut cursor), Err(expected));
            assert_eq!(cursor.parts(), Err(expected));
        }
        {
            let source_end = SourceEnd::Prefix;
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            assert!(matches!(
                Cursor::new(
                    source,
                    0,
                    source_end,
                    &Limits::default(),
                    &mut parts,
                    &mut work,
                    &mut budget
                ),
                Err(Error::IncompleteSource)
            ));
        }
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        assert!(matches!(
            Cursor::new(
                source,
                u64::MAX,
                SourceEnd::Eof,
                &Limits::default(),
                &mut parts,
                &mut work,
                &mut budget
            ),
            Err(Error::InvalidRange)
        ));
        for limit in [0, 65] {
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let limits = Limits {
                mime_depth: limit,
                ..Limits::default()
            };
            assert!(matches!(
                Cursor::new(
                    source,
                    0,
                    SourceEnd::Eof,
                    &limits,
                    &mut parts,
                    &mut work,
                    &mut budget
                ),
                Err(Error::InvalidLimits)
            ));
        }
    }
    #[test]
    fn aggregate_interpretation_and_entity_headers_never_reset() {
        let source = b"Content-Type: multipart/mixed;boundary=x\n\n--x\nX: y\n\na\n--x--";
        let mut parts = [Part::default(); 8];
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let initial = (budget.source_bytes_remaining(), budget.steps_remaining());
        let mut cursor = Cursor::new(
            source,
            0,
            SourceEnd::Eof,
            &Limits::default(),
            &mut parts,
            &mut work,
            &mut budget,
        )
        .unwrap();
        drain(&mut cursor).unwrap();
        let used = (
            initial.0 - cursor.budget.source_bytes_remaining(),
            initial.1 - cursor.budget.steps_remaining(),
        );
        let total = cursor.header_bytes().unwrap();
        let _ = cursor.finish(Tick(1)).unwrap();
        for bytes in [true, false] {
            let amount = if bytes { used.0 } else { used.1 };
            for limit in 0..amount {
                let mut work = meter();
                let mut budget = HeaderBudget::new();
                let mut credit = 0;
                let (visits, steps) = if bytes {
                    (budget.source_bytes_remaining() - limit, 0)
                } else {
                    (0, budget.steps_remaining() - limit)
                };
                budget
                    .charge(&mut work, Tick(1), visits, steps, &mut credit)
                    .unwrap();
                let mut cursor = Cursor::new(
                    source,
                    0,
                    SourceEnd::Eof,
                    &Limits::default(),
                    &mut parts,
                    &mut work,
                    &mut budget,
                )
                .unwrap();
                assert_eq!(drain(&mut cursor), Err(Error::InterpretationLimit));
                assert_eq!(cursor.parts(), Err(Error::InterpretationLimit));
                assert_eq!(cursor.poll(Tick(1)), Err(Error::InterpretationLimit));
            }
        }
        for headers in 1..=total {
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let limits = Limits {
                header_bytes: headers as usize,
                ..Limits::default()
            };
            let mut cursor = Cursor::new(
                source,
                0,
                SourceEnd::Eof,
                &limits,
                &mut parts,
                &mut work,
                &mut budget,
            )
            .unwrap();
            if headers < total {
                assert_eq!(drain(&mut cursor), Err(Error::HeaderLimit));
                assert_eq!(cursor.parts(), Err(Error::HeaderLimit));
            } else {
                drain(&mut cursor).unwrap();
                assert_eq!(cursor.header_bytes(), Some(total));
            }
        }
        let source = b"Content-Type: multipart/mixed;boundary=x\n\n--x\n\na\n--x\n\nb\n--x--";
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let limits = Limits {
            mime_depth: 2,
            mime_parts: 2,
            ..Limits::default()
        };
        let mut cursor = Cursor::new(
            source,
            0,
            SourceEnd::Eof,
            &limits,
            &mut parts,
            &mut work,
            &mut budget,
        )
        .unwrap();
        assert_eq!(drain(&mut cursor), Err(Error::PartLimit));
        assert_eq!(cursor.parts(), Err(Error::PartLimit));
    }
    #[test]
    fn maximum_explicit_depth_and_descriptor_count_stay_bounded() {
        let mut source = String::new();
        for depth in 0..63 {
            source.push_str(&format!(
                "Content-Type: multipart/mixed;boundary=b{depth:02}\n\n--b{depth:02}\n"
            ));
        }
        source.push_str("\nleaf");
        for depth in (0..63).rev() {
            source.push_str(&format!("\n--b{depth:02}--"));
        }
        let limits = Limits {
            mime_depth: 64,
            mime_parts: 4096,
            ..Limits::default()
        };
        let mut parts = vec![Part::default(); 4096];
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(
            source.as_bytes(),
            0,
            SourceEnd::Eof,
            &limits,
            &mut parts,
            &mut work,
            &mut budget,
        )
        .unwrap();
        drain(&mut cursor).unwrap();
        let selected = cursor.parts().unwrap().unwrap();
        assert_eq!(selected.len(), 64);
        assert_eq!(selected[63].depth, 64);
        assert_eq!(selected[63].size, 4);
        let _ = cursor.finish(Tick(1)).unwrap();
        let mut wide = String::from("Content-Type: multipart/mixed;boundary=x\n\n");
        for _ in 0..4095 {
            wide.push_str("--x\n\na\n");
        }
        wide.push_str("--x--");
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(
            wide.as_bytes(),
            0,
            SourceEnd::Eof,
            &limits,
            &mut parts,
            &mut work,
            &mut budget,
        )
        .unwrap();
        drain(&mut cursor).unwrap();
        let selected = cursor.parts().unwrap().unwrap();
        assert_eq!(selected.len(), 4096);
        assert_eq!(selected[4095].ordinal, 4096);
        assert_eq!(selected[4095].size, 1);
    }
}

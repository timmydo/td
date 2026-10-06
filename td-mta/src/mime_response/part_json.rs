//! Fixed metadata member fragments; braces, locators and tree composition are external.
pub use string::{Progress, Status};
use td_json::string::{self, Frame, Source};
use {
    crate::admission::work::Charge, crate::admission::work::Meter, crate::bounded::TextBuffer,
    crate::mime_body_lists::Node, crate::mime_part_headers::label_json,
    crate::mime_structure::Media, crate::mime_structure::Part as Descriptor, crate::nfc,
    crate::nfc::HeaderBudget, crate::nfc::Scratch, crate::ports::Tick,
};
use {crate::mime_response::bound::Error, crate::mime_response::response::Part};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct End {
    pub part: Descriptor,
    pub node: Node,
}
#[derive(Clone, Copy)]
enum Text<'w> {
    Borrowed(&'w [u8]),
    Static(&'static [u8]),
    Digits,
}
pub(in crate::mime_response) struct Scalars<'w> {
    pub(in crate::mime_response) work: &'w mut Meter,
    pub(in crate::mime_response) budget: &'w mut HeaderBudget,
    text: Text<'w>,
    digits: [u8; 20],
    used: usize,
    position: usize,
    credit: crate::nfc::Credit,
}
impl Scalars<'_> {
    fn bytes(&self) -> Result<&[u8], nfc::Error> {
        match self.text {
            Text::Borrowed(bytes) | Text::Static(bytes) => Ok(bytes),
            Text::Digits => self.digits.get(..self.used).ok_or(nfc::Error::InvalidState),
        }
    }
    fn charge(&mut self, now: Tick, bytes: u64, steps: u64) -> Result<(), nfc::Error> {
        self.budget
            .charge(self.work, now, bytes, steps, &mut self.credit)
    }
    fn number(&mut self, now: Tick, number: u64) -> Result<(), Error> {
        // Only primitive u64 formatting runs here, under its 20-digit bound.
        self.charge(now, 0, 20).map_err(Error::Admission)?;
        let mut writer = TextBuffer::new(&mut self.digits);
        writer
            .format(format_args!("{number}"))
            .map_err(|_| Error::InvalidState)?;
        self.used = writer.len();
        self.text = Text::Digits;
        Ok(())
    }
}
impl Source for Scalars<'_> {
    type Context = Tick;
    type Error = nfc::Error;
    fn charge_output(&mut self, now: Tick, bytes: u64) -> Result<(), nfc::Error> {
        self.charge(now, 0, 0)?;
        self.work
            .charge(
                now,
                Charge {
                    output_bytes: bytes,
                    ..Charge::default()
                },
            )
            .map_err(nfc::Error::Work)
    }
    fn poll(&mut self, now: Tick) -> Result<string::Scalar, nfc::Error> {
        if self.position == self.bytes()?.len() {
            self.charge(now, 0, 1)?;
            return Ok(string::Scalar::Complete);
        }
        let source = matches!(self.text, Text::Borrowed(_));
        self.charge(now, u64::from(source), 1)?;
        let first = self.bytes()?.get(self.position).copied();
        let width = match first {
            None => return Err(nfc::Error::InvalidState),
            Some(0..=0x7f) => 1,
            Some(0xc2..=0xdf) => 2,
            Some(0xe0..=0xef) => 3,
            Some(0xf0..=0xf4) => 4,
            _ => return Err(nfc::Error::InvalidState),
        };
        self.charge(now, if source { (width - 1) as u64 } else { 0 }, 0)?;
        let end = self
            .position
            .checked_add(width)
            .ok_or(nfc::Error::InvalidState)?;
        let bytes = self
            .bytes()?
            .get(self.position..end)
            .ok_or(nfc::Error::InvalidState)?;
        let text = std::str::from_utf8(bytes).map_err(|_| nfc::Error::InvalidState)?;
        let scalar = text.chars().next().ok_or(nfc::Error::InvalidState)?;
        self.position = end;
        Ok(string::Scalar::Value(scalar))
    }
}
#[derive(Clone, Copy)]
enum Phase {
    Prefix,
    Prepare,
    String,
    Raw,
    Complete,
}
#[derive(Clone, Copy, Eq, PartialEq)]
enum Field {
    PartId,
    Size,
    Type,
    Charset,
    Name,
    Disposition,
    Cid,
    Language,
    Location,
}
impl Field {
    fn prefix(self) -> &'static [u8] {
        match self {
            Self::PartId => b"\"partId\":",
            Self::Size => b",\"size\":",
            Self::Type => b",\"type\":",
            Self::Charset => b",\"charset\":",
            Self::Name => b",\"name\":",
            Self::Disposition => b",\"disposition\":",
            Self::Cid => b",\"cid\":",
            Self::Language => b",\"language\":",
            Self::Location => b",\"location\":",
        }
    }
    fn next(self) -> Option<Self> {
        match self {
            Self::PartId => Some(Self::Size),
            Self::Size => Some(Self::Type),
            Self::Type => Some(Self::Charset),
            Self::Charset => Some(Self::Name),
            Self::Name => Some(Self::Disposition),
            Self::Disposition => Some(Self::Cid),
            Self::Cid => Some(Self::Language),
            Self::Language => Some(Self::Location),
            Self::Location => None,
        }
    }
}

/// Consumes one complete original replay child before its window is reused.
/// Only fresh whole-fragment finish advances its parent. Bytes remain provisional.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mta::mime_response::part_json::Cursor<'_>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mta::mime_response::part_json::Cursor<'_>>();
/// ```
/// ```compile_fail,E0308
/// use {td_mta::mime_response::bound::ClassifiedView, td_mta::mime_response::part_json::Cursor, td_mta::ports::Tick};
/// fn substitute(view: ClassifiedView<'_>) { let _ = Cursor::new(view, Tick(1)); }
/// ```
pub struct Cursor<'w> {
    metadata: label_json::View<'w>,
    end: End,
    pub(in crate::mime_response) scalars: Scalars<'w>,
    pub(in crate::mime_response) scratch: &'w mut Scratch,
    parent_failure: &'w mut Option<Error>,
    next: &'w mut usize,
    following: u16,
    field: Field,
    position: usize,
    phase: Phase,
    frame: Frame<nfc::Error>,
    pub(in crate::mime_response) failure: Option<Error>,
}
impl<'w> Cursor<'w> {
    pub fn new(mut part: Part<'_, 'w>, now: Tick) -> Result<Self, Error> {
        let end = End {
            part: part.child.part,
            node: part.node,
        };
        // Keep the parent's Abandoned latch through serialization, not just metadata.
        let (metadata, work, budget, scratch) = part.child.finish_metadata(now)?;
        Ok(Self {
            metadata,
            end,
            scalars: Scalars {
                work,
                budget,
                text: Text::Borrowed(b""),
                digits: [0; 20],
                used: 0,
                position: 0,
                credit: crate::nfc::Credit::new(),
            },
            scratch,
            parent_failure: part.child.parent_failure,
            next: part.next,
            following: part.following,
            field: Field::PartId,
            position: 0,
            phase: Phase::Prefix,
            frame: Frame::new(),
            failure: None,
        })
    }
    pub(in crate::mime_response) fn outcome<T>(
        &mut self,
        result: Result<T, Error>,
    ) -> Result<T, Error> {
        if let Err(error) = result {
            self.failure = Some(error);
            *self.parent_failure = Some(error);
        }
        result
    }
    pub fn value(&self) -> Option<End> {
        (self.failure.is_none() && matches!(self.phase, Phase::Complete)).then_some(self.end)
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self.scalars.charge(now, 0, 0).map_err(Error::Admission);
        self.outcome(result)
    }
    pub fn poll(&mut self, now: Tick, output: &mut [u8]) -> Result<Progress, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if matches!(self.phase, Phase::Complete) {
            return Ok(Progress {
                written: 0,
                status: Status::Complete,
            });
        }
        let result = self.step(now, output);
        self.outcome(result)
    }
    fn advance(&mut self) {
        self.position = 0;
        if let Some(next) = self.field.next() {
            self.field = next;
            self.phase = Phase::Prefix;
        } else {
            self.phase = Phase::Complete;
        }
    }
    fn charset(&mut self, now: Tick) -> Result<(), Error> {
        let headers = self.metadata.headers;
        self.scalars.text = if headers.charset_end.is_none() {
            Text::Static(b"us-ascii")
        } else if let Some(label) = headers.charset {
            Text::Borrowed(label)
        } else {
            self.scalars
                .charge(now, headers.content_type.len().min(5) as u64, 1)
                .map_err(Error::Admission)?;
            if headers.content_type.starts_with(b"text/") {
                Text::Static(b"us-ascii")
            } else {
                self.scalars.text = Text::Static(b"null");
                self.phase = Phase::Raw;
                return Ok(());
            }
        };
        self.frame = Frame::new();
        self.phase = Phase::String;
        Ok(())
    }
    fn prepare(&mut self, now: Tick) -> Result<(), Error> {
        self.scalars.charge(now, 0, 1).map_err(Error::Admission)?;
        self.scalars.position = 0;
        let selected = match self.field {
            Field::PartId if self.end.part.media != Media::Multipart => {
                self.scalars.number(now, u64::from(self.end.part.ordinal))?;
                self.frame = Frame::new();
                self.phase = Phase::String;
                return Ok(());
            }
            Field::Size => {
                if self.end.part.size > 9_007_199_254_740_991 {
                    return Err(Error::InvalidState);
                }
                self.scalars.number(now, self.end.part.size)?;
                self.phase = Phase::Raw;
                return Ok(());
            }
            Field::Type => Some(self.metadata.headers.content_type),
            Field::Charset => return self.charset(now),
            Field::Name => self.metadata.headers.filename,
            Field::Disposition => self.metadata.headers.disposition,
            Field::Cid => self.metadata.labels.content_id,
            Field::Language => self.metadata.labels.content_language,
            Field::Location => self.metadata.location.value,
            Field::PartId => None,
        };
        self.scalars.text = selected.map_or(Text::Static(b"null"), Text::Borrowed);
        self.phase = if selected.is_some()
            && matches!(self.field, Field::Type | Field::Name | Field::Disposition)
        {
            self.frame = Frame::new();
            Phase::String
        } else {
            Phase::Raw
        };
        Ok(())
    }
    fn step(&mut self, now: Tick, output: &mut [u8]) -> Result<Progress, Error> {
        self.check_deadline(now)?;
        if output.is_empty() {
            return Ok(Progress {
                written: 0,
                status: Status::NeedOutput,
            });
        }
        let mut written = 0;
        match self.phase {
            Phase::Prepare => self.prepare(now)?,
            Phase::String => {
                let progress = self.frame.poll(&mut self.scalars, now, output).map_err(
                    |error| match error {
                        string::Error::Source(error) => Error::Admission(error),
                        string::Error::InvalidState => Error::Serialization(error),
                    },
                )?;
                written = progress.written;
                if progress.status == Status::Complete {
                    self.advance();
                }
            }
            Phase::Prefix | Phase::Raw => {
                self.scalars.charge(now, 0, 1).map_err(Error::Admission)?;
                let bytes = if matches!(self.phase, Phase::Prefix) {
                    self.field.prefix()
                } else {
                    self.scalars.bytes().map_err(Error::Admission)?
                };
                let count = bytes
                    .len()
                    .checked_sub(self.position)
                    .ok_or(Error::InvalidState)?
                    .min(output.len())
                    .min(64);
                let end = self
                    .position
                    .checked_add(count)
                    .ok_or(Error::InvalidState)?;
                // This new fragment pays every wire byte, including copied metadata JSON.
                self.scalars
                    .charge_output(now, count as u64)
                    .map_err(Error::Admission)?;
                // Reborrow the same immutable selection after admission, without a copy.

                let bytes = if matches!(self.phase, Phase::Prefix) {
                    self.field.prefix()
                } else {
                    self.scalars.bytes().map_err(Error::Admission)?
                };
                output
                    .get_mut(..count)
                    .ok_or(Error::InvalidState)?
                    .copy_from_slice(bytes.get(self.position..end).ok_or(Error::InvalidState)?);
                let complete = end == bytes.len();
                self.position = end;
                written = count;
                if complete {
                    if matches!(self.phase, Phase::Prefix) {
                        self.position = 0;
                        self.phase = Phase::Prepare;
                    } else {
                        self.advance();
                    }
                }
            }
            Phase::Complete => return Err(Error::InvalidState),
        }
        Ok(Progress {
            written,
            status: if matches!(self.phase, Phase::Complete) {
                Status::Complete
            } else {
                Status::Yield
            },
        })
    }
    pub fn finish(
        mut self,
        now: Tick,
    ) -> Result<(End, &'w mut Meter, &'w mut HeaderBudget, &'w mut Scratch), Error> {
        self.check_deadline(now)?;
        if !matches!(self.phase, Phase::Complete) {
            return self.outcome(Err(Error::InvalidState));
        }
        *self.parent_failure = None;
        *self.next = usize::from(self.following);
        Ok((
            self.end,
            self.scalars.work,
            self.scalars.budget,
            self.scratch,
        ))
    }
}
const _: () =
    assert!(std::mem::size_of::<Cursor<'_>>() + std::mem::size_of::<HeaderBudget>() <= 1024);
#[cfg(test)]
#[path = "part_json/tests.rs"]
pub(in crate::mime_response) mod tests;

//! Stream a preview JSON string from one admitted immutable text extent.
use crate::{
    admission::work::{Charge, Meter, Stop},
    email_preview::{self, Core, Summary},
    html_text,
    mime_input::Checkpoints,
    mime_text::{self, CheckedClock, InputSource, Reader},
    ports::{BlobReader, Clock, Error as PolicyError, Tick},
};
pub use string::{Progress, Status};
use td_json::string::{self, Source};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Format {
    Plain,
    Html,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Work(Stop),
    Policy(PolicyError),
    Text(mime_text::Error),
    State,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "preview JSON: {self:?}")
    }
}
impl std::error::Error for Error {}
impl From<mime_text::Error> for Error {
    fn from(error: mime_text::Error) -> Self {
        match error {
            mime_text::Error::Work(e) => Self::Work(e),
            mime_text::Error::Policy(e) => Self::Policy(e),
            e => Self::Text(e),
        }
    }
}
impl From<email_preview::Error> for Error {
    fn from(error: email_preview::Error) -> Self {
        match error {
            email_preview::Error::Work(e) => Self::Work(e),
            email_preview::Error::Policy(e) => Self::Policy(e),
            email_preview::Error::State => Self::State,
        }
    }
}
impl From<html_text::Error> for Error {
    fn from(error: html_text::Error) -> Self {
        match error {
            html_text::Error::Work(e) => Self::Work(e),
            html_text::Error::Policy(e) => Self::Policy(e),
            html_text::Error::State => Self::State,
        }
    }
}
impl From<string::Error<Error>> for Error {
    fn from(error: string::Error<Error>) -> Self {
        match error {
            string::Error::Source(e) => e,
            string::Error::InvalidState => Self::State,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Diagnostics {
    pub summary: Summary,
    pub is_encoding_problem: bool,
    pub malformed_markup: bool,
}
struct Text<'r, 'b, 'w, 'c, S: BlobReader + ?Sized> {
    reader: Reader<'r, 'b, S>,
    preview: Core,
    html: Option<td_html::extract::Extractor>,
    work: &'w mut Meter,
    clock: CheckedClock<'c>,
    decoded: Option<char>,
    extracted: Option<char>,
    source_end: bool,
    html_end: bool,
}
impl<S: BlobReader + ?Sized> Text<'_, '_, '_, '_, S> {
    fn diagnostics(&self) -> Option<Diagnostics> {
        if !self.source_end || (self.html.is_some() && !self.html_end) {
            return None;
        }
        Some(Diagnostics {
            summary: self.preview.value()?,
            is_encoding_problem: self.reader.is_encoding_problem(),
            malformed_markup: self
                .html
                .as_ref()
                .is_some_and(|html| html.diagnostics().malformed_markup),
        })
    }
}
impl<S: BlobReader + ?Sized> Source for Text<'_, '_, '_, '_, S> {
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
        let (input, last) = if let Some(html) = self.html.as_mut() {
            if self.extracted.is_none() && !self.html_end {
                if self.decoded.is_none() && !self.source_end {
                    match self.reader.poll(&self.clock, self.work)? {
                        mime_text::Status::Scalar(c) => self.decoded = Some(c),
                        mime_text::Status::Complete => self.source_end = true,
                        mime_text::Status::Yield => {}
                    }
                } else {
                    let progress = html_text::poll_shared(
                        html,
                        self.decoded,
                        self.source_end,
                        self.work,
                        &self.clock,
                    )?;
                    if progress.consumed {
                        self.decoded = None;
                    }
                    self.extracted = progress.scalar;
                    self.html_end = progress.status == html_text::Status::Complete;
                }
                return Ok(string::Scalar::Yield);
            }
            (self.extracted, self.html_end)
        } else {
            if self.decoded.is_none() && !self.source_end {
                match self.reader.poll(&self.clock, self.work)? {
                    mime_text::Status::Scalar(c) => self.decoded = Some(c),
                    mime_text::Status::Complete => self.source_end = true,
                    mime_text::Status::Yield => {}
                }
                return Ok(string::Scalar::Yield);
            }
            (self.decoded, self.source_end)
        };
        let progress = self.preview.poll(input, last, self.work, &self.clock)?;
        if progress.consumed {
            if self.html.is_some() {
                self.extracted = None;
            } else {
                self.decoded = None;
            }
        }
        Ok(match progress.status {
            email_preview::Status::Scalar(c) => string::Scalar::Value(c),
            email_preview::Status::Complete => string::Scalar::Complete,
            email_preview::Status::Yield => string::Scalar::Yield,
            email_preview::Status::NeedInput => return Err(Error::State),
        })
    }
}
/// Caller admission binds the chosen source/format and native snapshot. This
/// cursor retains the source through transfer/charset decoding, optional HTML
/// extraction, full-source preview projection and JSON framing. All wire bytes
/// remain provisional until source and enclosing response admission succeeds.
pub struct Cursor<'r, 'b, 'w, 'c, S: BlobReader + ?Sized = dyn BlobReader + 'r> {
    source: Text<'r, 'b, 'w, 'c, S>,
    frame: string::Frame<Error>,
    complete: bool,
    failure: Option<Error>,
}
impl<'r, 'b, 'w, 'c, S: BlobReader + ?Sized> Cursor<'r, 'b, 'w, 'c, S> {
    pub fn new(
        input: InputSource<'r, S>,
        buffer: &'b mut [u8],
        checkpoints: &'b mut Checkpoints,
        format: Format,
        work: &'w mut Meter,
        clock: &'c dyn Clock,
        last: Tick,
    ) -> Result<Self, Error> {
        let mut cursor = Self::prepare(input, buffer, checkpoints, format, work, clock, last)?;
        cursor.check()?;
        Ok(cursor)
    }
    pub(crate) fn source_mut(&mut self) -> &mut S {
        self.source.reader.source_mut()
    }
    // No I/O or clock work; native callers must check before exposure.
    pub(crate) fn prepare(
        input: InputSource<'r, S>,
        buffer: &'b mut [u8],
        checkpoints: &'b mut Checkpoints,
        format: Format,
        work: &'w mut Meter,
        clock: &'c dyn Clock,
        last: Tick,
    ) -> Result<Self, Error> {
        Ok(Self {
            source: Text {
                reader: Reader::new_typed(input, buffer, checkpoints)?,
                preview: Core::new(),
                html: (format == Format::Html).then(td_html::extract::Extractor::new),
                work,
                clock: CheckedClock::new(clock, last),
                decoded: None,
                extracted: None,
                source_end: false,
                html_end: false,
            },
            frame: string::Frame::new(),
            complete: false,
            failure: None,
        })
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
        let result = self.source.charge_output((), 0);
        self.outcome(result)
    }
    /// Passive completed diagnostics; check/finish provides fresh admission.
    pub fn value(&self) -> Option<Diagnostics> {
        if self.failure.is_none() && self.complete {
            self.source.diagnostics()
        } else {
            None
        }
    }
    pub fn poll(&mut self, output: &mut [u8]) -> Result<Progress, Error> {
        self.check()?;
        let result = self
            .frame
            .poll(&mut self.source, (), output)
            .map_err(Error::from);
        let after = self.source.charge_output((), 0);
        let progress = self.outcome(after.and(result))?;
        self.complete = progress.status == Status::Complete;
        Ok(progress)
    }
    pub fn finish(mut self) -> Result<Retained<'r, 'b, 'w, S>, Error> {
        self.check()?;
        self.finish_retained()
    }
    // Native callers fence the final check before this passive handoff.
    pub(crate) fn finish_retained(self) -> Result<Retained<'r, 'b, 'w, S>, Error> {
        let diagnostics = self.value().ok_or(Error::State)?;
        let input = self.source.reader.release()?;
        Ok(Retained {
            source: input.source,
            buffer: input.buffer,
            checkpoints: input.checkpoints.ok_or(Error::State)?,
            work: self.source.work,
            diagnostics,
        })
    }
}
/// Healthy handoff of the exact source, buffers and original job budget.
pub struct Retained<'r, 'b, 'w, S: BlobReader + ?Sized> {
    pub source: &'r mut S,
    pub buffer: &'b mut [u8],
    pub checkpoints: &'b mut Checkpoints,
    pub work: &'w mut Meter,
    pub diagnostics: Diagnostics,
}
const _: () = assert!(std::mem::size_of::<Cursor<'_, '_, '_, '_>>() <= 2048);

#[cfg(test)]
#[path = "preview_json_tests.rs"]
mod tests;

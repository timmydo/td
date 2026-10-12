//! Preview one original native text part without replacing its pin or work owner.
use crate::{
    blob_part, blob_structure,
    mime_input::Checkpoints,
    mime_response::bound::{self, ClassifiedView},
    mime_structure::Part,
    mime_text::InputSource as Input,
    nfc::{Credit, HeaderBudget, Scratch},
    ports::{Clock, Error as PolicyError, Tick},
    preview_json as json, root_headers,
    store_fs::PinnedBlob,
};
pub use json::{Diagnostics, Progress, Status};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Metadata(bound::Error),
    Json(json::Error),
    Policy(PolicyError),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "native preview: {self:?}")
    }
}
impl std::error::Error for Error {}
/// Healthy handoff of the original interpreted part and reusable input storage.
pub struct Retained<'p, 'v, 'c, 'b, 'o, 'w, 'i> {
    pub original: blob_part::Retained<'p, 'v, 'c, 'b, 'o, 'w>,
    pub buffer: &'i mut [u8],
    pub checkpoints: &'i mut Checkpoints,
    pub diagnostics: Diagnostics,
}
/// Stream provisional preview JSON under the original snapshot and job owners.
/// Failure is terminal; passive diagnostics never authorize publication.
pub struct Cursor<'p, 'v, 'c, 'b, 'o, 'w, 'i, 't> {
    child: json::Cursor<'p, 'i, 'w, 't, PinnedBlob<'v, 'c>>,
    budget: &'w mut HeaderBudget,
    parts: &'o [Part],
    headers: &'o [Option<root_headers::View<'b>>],
    remaining: &'b mut [u8],
    header_bytes: u64,
    metadata: ClassifiedView<'w>,
    scratch: &'w mut Scratch,
    failure: Option<Error>,
}
impl<'p, 'v, 'c, 'b, 'o, 'w, 'i, 't> Cursor<'p, 'v, 'c, 'b, 'o, 'w, 'i, 't> {
    /// Admit one retained text extent; callers select the body-list entry.
    pub fn new(
        original: blob_part::Retained<'p, 'v, 'c, 'b, 'o, 'w>,
        buffer: &'i mut [u8],
        checkpoints: &'i mut Checkpoints,
        clock: &'t dyn Clock,
        now: Tick,
    ) -> Result<Self, Error> {
        original
            .structure
            .source
            .check_deadline()
            .map_err(Error::Policy)?;
        let prepared = (|| {
            original
                .structure
                .budget
                .charge(original.structure.work, now, 0, 0, &mut Credit::new())
                .map_err(|e| Error::Metadata(bound::Error::Admission(e)))?;
            if buffer.is_empty() || buffer.len() > crate::mime_input::INPUT_BYTES {
                return Err(Error::Json(json::Error::Text(
                    crate::mime_text::Error::Input(crate::mime_input::Error::InvalidBacking),
                )));
            }
            original.text_spec().map_err(Error::Metadata)
        })();
        original
            .structure
            .source
            .check_deadline()
            .map_err(Error::Policy)?;
        let spec = prepared?;
        // The checks above cover every fallible prepare input: backing and
        // extent. Preparation performs no I/O; check brackets its success.
        let child = json::Cursor::prepare(
            Input {
                source: original.structure.source,
                offset: spec.offset,
                length: spec.length,
                encoding: spec.encoding,
                charset: spec.charset,
            },
            buffer,
            checkpoints,
            if spec.html {
                json::Format::Html
            } else {
                json::Format::Plain
            },
            original.structure.work,
            clock,
            now,
        )
        .map_err(Error::Json)?;
        let mut cursor = Self {
            child,
            budget: original.structure.budget,
            parts: original.structure.parts,
            headers: original.structure.headers,
            remaining: original.structure.remaining,
            header_bytes: original.structure.header_bytes,
            metadata: original.metadata,
            scratch: original.scratch,
            failure: None,
        };
        cursor.check()?;
        Ok(cursor)
    }
    fn outcome<T>(&mut self, result: Result<T, Error>) -> Result<T, Error> {
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    /// Fence the job-clock check with fresh native snapshot checks.
    pub fn check(&mut self) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = (|| {
            self.child
                .source_mut()
                .check_deadline()
                .map_err(Error::Policy)?;
            let result = self.child.check().map_err(Error::Json);
            self.child
                .source_mut()
                .check_deadline()
                .map_err(Error::Policy)
                .and(result)
        })();
        self.outcome(result)
    }
    /// Passive diagnostics, hidden after any admission failure.
    pub fn value(&self) -> Option<Diagnostics> {
        if self.failure.is_none() {
            self.child.value()
        } else {
            None
        }
    }
    /// Advance one bounded turn; copied bytes remain provisional.
    pub fn poll(&mut self, output: &mut [u8]) -> Result<Progress, Error> {
        self.check()?;
        let result = self.child.poll(output).map_err(Error::Json);
        let result = self
            .child
            .source_mut()
            .check_deadline()
            .map_err(Error::Policy)
            .and(result);
        self.outcome(result)
    }
    /// Freshly admit completion and return every original owner.
    pub fn finish(mut self) -> Result<Retained<'p, 'v, 'c, 'b, 'o, 'w, 'i>, Error> {
        self.check()?;
        let complete = self.value().is_some();
        // Fence the completion decision independently of passive release.
        self.child
            .source_mut()
            .check_deadline()
            .map_err(Error::Policy)?;
        if !complete {
            return Err(Error::Metadata(bound::Error::InvalidState));
        }
        let child = self.child.finish_retained().map_err(Error::Json)?;
        child.source.check_deadline().map_err(Error::Policy)?;
        Ok(Retained {
            original: blob_part::Retained {
                structure: blob_structure::Retained {
                    source: child.source,
                    work: child.work,
                    budget: self.budget,
                    parts: self.parts,
                    headers: self.headers,
                    remaining: self.remaining,
                    header_bytes: self.header_bytes,
                },
                metadata: self.metadata,
                scratch: self.scratch,
            },
            buffer: child.buffer,
            checkpoints: child.checkpoints,
            diagnostics: child.diagnostics,
        })
    }
}
const _: () = assert!(std::mem::size_of::<Cursor<'_, '_, '_, '_, '_, '_, '_, '_>>() <= 2048);

#[cfg(test)]
#[path = "blob_preview_tests.rs"]
mod tests;

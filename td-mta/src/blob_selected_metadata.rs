//! Select original generated part members under native traversal custody.
use crate::{
    blob_part_json,
    mime_response::{
        bound,
        locators::Candidate,
        metadata_core::{segments, Core},
        part_json::End,
        pinned::Error,
        selected_metadata::Properties,
    },
    mime_structure::{Encoding, Media},
    nfc::{Credit, HeaderBudget},
    ports::{BlobReader, Tick},
    wire::TransferEncoding,
};
pub use td_json::string::{Progress, Status};
/// The caller must preserve the complete nine-member output of this handoff.
/// Fragment identity and completeness are caller preconditions, not checked here.
pub struct Cursor<'p, 'v, 'c, 'b, 'o, 'w, 'f> {
    original: blob_part_json::Retained<'p, 'v, 'c, 'b, 'o, 'w>,
    fragment: &'f [u8],
    candidate: &'f Candidate,
    properties: Properties,
    core: Core,
    failure: Option<Error>,
}
pub struct Retained<'p, 'v, 'c, 'b, 'o, 'w, 'f> {
    pub original: blob_part_json::Retained<'p, 'v, 'c, 'b, 'o, 'w>,
    pub fragment: &'f [u8],
    pub candidate: &'f Candidate,
    pub properties: Properties,
}
impl<'p, 'v, 'c, 'b, 'o, 'w, 'f> Cursor<'p, 'v, 'c, 'b, 'o, 'w, 'f> {
    pub fn new(
        original: blob_part_json::Retained<'p, 'v, 'c, 'b, 'o, 'w>,
        fragment: &'f [u8],
        candidate: &'f Candidate,
        properties: Properties,
        now: Tick,
    ) -> Result<Self, Error> {
        original
            .structure
            .source
            .check_deadline()
            .map_err(Error::Parent)?;
        let prepared = (|| {
            original
                .structure
                .budget
                .charge(original.structure.work, now, 0, 0, &mut Credit::new())
                .map_err(|error| Error::Original(bound::Error::Admission(error)))?;
            let part = original.end.part;
            let index = usize::from(part.ordinal)
                .checked_sub(1)
                .ok_or(Error::InvalidState)?;
            if original.structure.parts.get(index) != Some(&part)
                || candidate.ordinal() != part.ordinal
            {
                return Err(Error::InvalidState);
            }
            if part.media != Media::Multipart {
                let locator = candidate.locator().ok_or(Error::InvalidState)?;
                let encoding = match part.encoding {
                    Encoding::Identity => TransferEncoding::Identity,
                    Encoding::Base64 => TransferEncoding::Base64,
                    Encoding::QuotedPrintable => TransferEncoding::QuotedPrintable,
                };
                if locator.parent != original.structure.source.id()
                    || locator.offset != part.body_start
                    || locator.length
                        != part
                            .entity_end
                            .checked_sub(part.body_start)
                            .ok_or(Error::InvalidState)?
                    || locator.encoding != encoding
                    || locator
                        .checked_end(original.structure.source.len())
                        .is_err()
                {
                    return Err(Error::SourceMismatch);
                }
            }
            Core::new(
                segments(fragment, candidate, part.media == Media::Multipart)?,
                properties,
            )
        })();
        original
            .structure
            .source
            .check_deadline()
            .map_err(Error::Parent)?;
        Ok(Self {
            original,
            fragment,
            candidate,
            properties,
            core: prepared?,
            failure: None,
        })
    }
    fn outcome<T>(&mut self, result: Result<T, Error>) -> Result<T, Error> {
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = (|| {
            let structure = &mut self.original.structure;
            structure.source.check_deadline().map_err(Error::Parent)?;
            let result = structure
                .budget
                .charge(structure.work, now, 0, 0, &mut Credit::new())
                .map_err(|error| Error::Original(bound::Error::Admission(error)));
            structure
                .source
                .check_deadline()
                .map_err(Error::Parent)
                .and(result)
        })();
        self.outcome(result)
    }
    /// Passive completion does not authorize response publication.
    pub fn value(&self) -> Option<(End, Properties)> {
        (self.failure.is_none() && self.core.complete())
            .then_some((self.original.end, self.properties))
    }
    pub fn poll(&mut self, now: Tick, output: &mut [u8]) -> Result<Progress, Error> {
        self.check_deadline(now)?;
        let result = (|| {
            if self.core.complete() {
                return Ok(Progress {
                    written: 0,
                    status: Status::Complete,
                });
            }
            if output.is_empty() {
                return Ok(Progress {
                    written: 0,
                    status: Status::NeedOutput,
                });
            }
            let segments = segments(
                self.fragment,
                self.candidate,
                self.original.end.part.media == Media::Multipart,
            )?;
            let turn = self.core.plan(segments, output.len())?;
            let structure = &mut self.original.structure;
            let charged = self
                .core
                .charge(turn, structure.work, structure.budget, now)?;
            self.core.emit(charged, segments, output)
        })();
        let result = self
            .original
            .structure
            .source
            .check_deadline()
            .map_err(Error::Parent)
            .and(result);
        self.outcome(result)
    }
    pub fn finish(mut self, now: Tick) -> Result<Retained<'p, 'v, 'c, 'b, 'o, 'w, 'f>, Error> {
        self.check_deadline(now)?;
        let result = self.value().ok_or(Error::InvalidState);
        self.original
            .structure
            .source
            .check_deadline()
            .map_err(Error::Parent)?;
        result?;
        Ok(Retained {
            original: self.original,
            fragment: self.fragment,
            candidate: self.candidate,
            properties: self.properties,
        })
    }
}
const _: () = assert!(
    std::mem::size_of::<Cursor<'_, '_, '_, '_, '_, '_, '_>>()
        + std::mem::size_of::<HeaderBudget>()
        + 64
        <= 1024
);

#[cfg(test)]
#[path = "blob_selected_metadata_tests.rs"]
pub(crate) mod tests;

//! Source-bound part members; current access authorization and publication follow.
use crate::mime_response::metadata_core::Core;
use crate::mime_response::selected_metadata as selected;
use crate::{nfc::HeaderBudget, ports::Tick};
pub use td_json::string::{Progress, Status};
use {crate::mime_response::pinned::Bound, crate::mime_response::pinned::Error};

/// Only the original source-matched owner can supply these provisional bytes.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mta::mime_response::metadata::Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mta::mime_response::metadata::Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0308
/// use {td_mta::ports::Tick, td_mta::mime_response::locators::View, td_mta::mime_response::metadata::Cursor};
/// fn passive(view: View<'_, '_>) { let _ = Cursor::new(view, 1, Tick(1)); }
/// ```
pub struct Cursor<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k> {
    pub(in crate::mime_response) source: Bound<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k>,
    ordinal: u16,
    pub(in crate::mime_response) core: Core,
    pub(in crate::mime_response) failure: Option<Error>,
}
impl<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k> Cursor<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k> {
    pub fn new(
        source: Bound<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k>,
        ordinal: u16,
        now: Tick,
    ) -> Result<Self, Error> {
        Self::with_properties(source, ordinal, selected::Properties::ALL, now)
    }
    pub(in crate::mime_response) fn with_properties(
        mut source: Bound<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k>,
        ordinal: u16,
        properties: selected::Properties,
        now: Tick,
    ) -> Result<Self, Error> {
        source.check_deadline(now)?;
        let core = Core::new(Self::segments_from(&source, ordinal)?, properties)?;
        Ok(Self {
            source,
            ordinal,
            core,
            failure: None,
        })
    }
    pub(in crate::mime_response) fn segments(&self) -> Result<[&[u8]; 5], Error> {
        Self::segments_from(&self.source, self.ordinal)
    }
    fn segments_from<'s>(
        source: &'s Bound<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k>,
        ordinal: u16,
    ) -> Result<[&'s [u8]; 5], Error> {
        let index = usize::from(ordinal.checked_sub(1).ok_or(Error::InvalidState)?);
        let original = source.value().ok_or(Error::InvalidState)?;
        let fragment = original
            .original
            .original
            .fragments
            .get(index)
            .and_then(|cell| cell.value())
            .ok_or(Error::InvalidState)?;
        let part = original
            .original
            .original
            .selected
            .structure
            .parts
            .get(index)
            .copied()
            .ok_or(Error::InvalidState)?;
        let candidate = original.candidates.get(index).ok_or(Error::InvalidState)?;
        if candidate.ordinal() != ordinal || part.ordinal != ordinal || fragment.end.part != part {
            return Err(Error::InvalidState);
        }
        crate::mime_response::metadata_core::segments(
            fragment.fragment,
            candidate,
            part.media == crate::mime_structure::Media::Multipart,
        )
    }

    pub(in crate::mime_response) fn outcome<T>(
        &mut self,
        result: Result<T, Error>,
    ) -> Result<T, Error> {
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    pub fn value(&self) -> Option<u16> {
        if self.failure.is_some() || !self.core.complete() {
            return None;
        }
        self.source.value()?;
        Some(self.ordinal)
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self.source.check_deadline(now);
        self.outcome(result)
    }
    pub fn poll(&mut self, now: Tick, output: &mut [u8]) -> Result<Progress, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if self.value().is_some() {
            return Ok(Progress {
                written: 0,
                status: Status::Complete,
            });
        }
        self.check_deadline(now)?;
        if output.is_empty() {
            return Ok(Progress {
                written: 0,
                status: Status::NeedOutput,
            });
        }
        let result = self.step(now, output);
        let result = self
            .source
            .parent
            .check_deadline()
            .map_err(Error::Parent)
            .and(result);
        self.outcome(result)
    }
    fn step(&mut self, now: Tick, output: &mut [u8]) -> Result<Progress, Error> {
        let turn = self.core.plan(self.segments()?, output.len())?;
        let structure = &mut self
            .source
            .original
            .source
            .original
            .source
            .projected
            .structure;
        let charged = self
            .core
            .charge(turn, structure.work, structure.budget, now)?;
        let segments = Self::segments_from(&self.source, self.ordinal)?;
        self.core.emit(charged, segments, output)
    }
    pub fn finish(
        mut self,
        now: Tick,
    ) -> Result<Member<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k>, Error> {
        self.check_deadline(now)?;
        if self.value().is_none() {
            return Err(Error::InvalidState);
        }
        Ok(Member {
            source: self.source,
            ordinal: self.ordinal,
            failure: None,
        })
    }
}
/// Completed emission retains original source matching and the actual descriptor.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mta::mime_response::metadata::Member<'_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mta::mime_response::metadata::Member<'_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
pub struct Member<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k> {
    source: Bound<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k>,
    ordinal: u16,
    failure: Option<Error>,
}
pub type Release<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k> =
    (Bound<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k>, u16);
impl<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k> Member<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k> {
    pub fn value(&self) -> Option<u16> {
        if self.failure.is_some() {
            return None;
        }
        self.source.value()?;
        Some(self.ordinal)
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self.source.check_deadline(now);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    pub fn finish(
        mut self,
        now: Tick,
    ) -> Result<Release<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k>, Error> {
        self.check_deadline(now)?;
        Ok((self.source, self.ordinal))
    }
}
const _: () = assert!(
    std::mem::size_of::<Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_>>()
        + std::mem::size_of::<HeaderBudget>()
        + 64
        + std::mem::size_of::<[&[u8]; 5]>()
        <= 1024
);
const _: () = assert!(std::mem::size_of::<Member<'_, '_, '_, '_, '_, '_, '_, '_, '_>>() <= 768);

#[cfg(test)]
pub use tests::probe as probe_allocations;
#[cfg(test)]
#[path = "metadata/tests.rs"]
pub(in crate::mime_response) mod tests;

//! Whole source-bound part-member bytes in a separately admitted fixed window.
pub use crate::mime_structure::Status as RetainStatus;
use crate::{nfc::HeaderBudget, ports::Tick};
use td_json::retain::Window;
use {
    crate::mime_response::metadata::Member, crate::mime_response::metadata::Status,
    crate::mime_response::pinned::Bound, crate::mime_response::pinned::Error,
};

fn window_error(error: td_json::retain::Error) -> Error {
    Error::Original(match error {
        td_json::retain::Error::Capacity => crate::mime_response::bound::Error::ResponseCapacity,
        td_json::retain::Error::InvalidState => crate::mime_response::bound::Error::InvalidState,
    })
}
/// Conservative checked reservation; it supplies no completion or permission.
pub fn window_bound(fragment_bytes: usize) -> Result<usize, Error> {
    fragment_bytes
        .checked_add(crate::wire::PartLocator::WIRE_BYTES)
        .and_then(|bytes| bytes.checked_add(12))
        .ok_or(Error::InvalidState)
}
#[derive(Clone, Copy)]
pub struct View<'v> {
    pub ordinal: u16,
    pub members: &'v [u8],
}
/// Construct a fresh emitter from original source matching, never advanced output.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mta::mime_response::metadata_window::Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mta::mime_response::metadata_window::Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0308
/// use {td_mta::ports::Tick, td_mta::mime_response::metadata::Cursor as Advanced, td_mta::mime_response::metadata_window::Cursor};
/// fn prefix_lost(advanced: Advanced<'_, '_, '_, '_, '_, '_, '_, '_, '_>, output: &mut [u8]) { let _ = Cursor::new(advanced, 1, output, Tick(1)); }
/// ```
/// ```compile_fail,E0308
/// use {td_mta::ports::Tick, td_mta::mime_response::metadata::Member, td_mta::mime_response::metadata_window::Cursor};
/// fn emission_only(member: Member<'_, '_, '_, '_, '_, '_, '_, '_, '_>, output: &mut [u8]) { let _ = Cursor::new(member, 1, output, Tick(1)); }
/// ```
pub struct Cursor<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 'm> {
    pub(in crate::mime_response) emitter:
        crate::mime_response::metadata::Cursor<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k>,
    window: Window<'m>,
}
impl<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 'm> Cursor<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 'm> {
    pub fn new(
        source: Bound<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k>,
        ordinal: u16,
        output: &'m mut [u8],
        now: Tick,
    ) -> Result<Self, Error> {
        Self::with_properties(
            source,
            ordinal,
            crate::mime_response::selected_metadata::Properties::ALL,
            output,
            now,
        )
    }
    pub(in crate::mime_response) fn with_properties(
        source: Bound<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k>,
        ordinal: u16,
        properties: crate::mime_response::selected_metadata::Properties,
        output: &'m mut [u8],
        now: Tick,
    ) -> Result<Self, Error> {
        Ok(Self {
            emitter: crate::mime_response::metadata::Cursor::with_properties(
                source, ordinal, properties, now,
            )?,
            window: Window::new(output),
        })
    }
    #[cfg(test)]
    pub(in crate::mime_response) fn costs(&self) -> [u64; 5] {
        let structure = &self
            .emitter
            .source
            .original
            .source
            .original
            .source
            .projected
            .structure;
        let left = structure.work.remaining();
        [
            structure.budget.source_bytes_remaining(),
            structure.budget.steps_remaining(),
            left.io_bytes,
            left.records,
            left.output_bytes,
        ]
    }
    pub fn value(&self) -> Option<View<'_>> {
        Some(View {
            ordinal: self.emitter.value()?,
            members: self.window.provisional()?,
        })
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        self.emitter.check_deadline(now)
    }
    pub fn poll(&mut self, now: Tick) -> Result<RetainStatus, Error> {
        if let Some(error) = self.emitter.failure {
            return Err(error);
        }
        if self.emitter.value().is_some() {
            return Ok(RetainStatus::Complete);
        }
        self.check_deadline(now)?;
        let result = self.step(now);
        self.emitter.outcome(result)
    }
    fn step(&mut self, now: Tick) -> Result<RetainStatus, Error> {
        let tail = self.window.tail().map_err(window_error)?;
        let progress = self.emitter.poll(now, tail)?;
        self.window
            .advance(progress.written)
            .map_err(window_error)?;
        match progress.status {
            Status::Complete => Ok(RetainStatus::Complete),
            Status::Yield => Ok(RetainStatus::Yield),
            Status::NeedOutput => Err(Error::InvalidState),
        }
    }
    pub fn finish(
        mut self,
        now: Tick,
    ) -> Result<Retained<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 'm>, Error> {
        self.check_deadline(now)?;
        if self.emitter.value().is_none() {
            return self.emitter.outcome(Err(Error::InvalidState));
        }
        let members = self.window.into_slice().map_err(window_error)?;
        Ok(Retained {
            original: self.emitter.finish(now)?,
            members,
        })
    }
}
/// Exclusive original emission completion and whole retained member bytes.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mta::mime_response::metadata_window::Retained<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mta::mime_response::metadata_window::Retained<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
pub struct Retained<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 'm> {
    original: Member<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k>,
    members: &'m [u8],
}
pub type Release<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 'm> =
    (Bound<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k>, u16, &'m [u8]);
impl<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 'm> Retained<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 'm> {
    pub fn value(&self) -> Option<View<'_>> {
        Some(View {
            ordinal: self.original.value()?,
            members: self.members,
        })
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        self.original.check_deadline(now)
    }
    pub fn finish(
        self,
        now: Tick,
    ) -> Result<Release<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 'm>, Error> {
        let (source, ordinal) = self.original.finish(now)?;
        Ok((source, ordinal, self.members))
    }
}
const _: () = assert!(
    std::mem::size_of::<Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>()
        + std::mem::size_of::<HeaderBudget>()
        + std::mem::size_of::<[&[u8]; 5]>()
        + 64
        <= 1024
);
const _: () =
    assert!(std::mem::size_of::<Retained<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>() <= 768);

#[cfg(test)]
pub use tests::probe as probe_allocations;
#[cfg(test)]
#[path = "metadata_window/tests.rs"]
pub(in crate::mime_response) mod tests;

//! Whole selected part members through the shared original-bound fixed window.
use crate::mime_response::metadata_window as shared;
pub use crate::mime_structure::Status;
use crate::{nfc::HeaderBudget, ports::Tick};
use {
    crate::mime_response::pinned::Bound, crate::mime_response::pinned::Error,
    crate::mime_response::selected_metadata::Properties,
};
#[derive(Clone, Copy)]
pub struct View<'v> {
    pub properties: Properties,
    pub ordinal: u16,
    pub members: &'v [u8],
}
/// Exclusive original custody; passive selected bytes convey no publication authority.
/// ```compile_fail,E0277
/// fn bound<T: Copy>() {} bound::<td_mta::mime_response::selected_metadata_window::Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn bound<T: Clone>() {} bound::<td_mta::mime_response::selected_metadata_window::Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0308
/// use {td_mta::ports::Tick, td_mta::mime_response::selected_metadata::Cursor as Advanced, td_mta::mime_response::selected_metadata::Properties, td_mta::mime_response::selected_metadata_window::Cursor};
/// fn prefix_lost(source: Advanced<'_, '_, '_, '_, '_, '_, '_, '_, '_>, output: &mut [u8]) { let _ = Cursor::new(source, 1, Properties::ALL, output, Tick(1)); }
/// ```
/// ```compile_fail,E0308
/// use {td_mta::ports::Tick, td_mta::mime_response::selected_metadata::Member as Advanced, td_mta::mime_response::selected_metadata::Properties, td_mta::mime_response::selected_metadata_window::Cursor};
/// fn prefix_lost(source: Advanced<'_, '_, '_, '_, '_, '_, '_, '_, '_>, output: &mut [u8]) { let _ = Cursor::new(source, 1, Properties::ALL, output, Tick(1)); }
/// ```
pub struct Cursor<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 'm> {
    inner: shared::Cursor<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 'm>,
    properties: Properties,
}
impl<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 'm> Cursor<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 'm> {
    pub fn new(
        source: Bound<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k>,
        ordinal: u16,
        properties: Properties,
        output: &'m mut [u8],
        now: Tick,
    ) -> Result<Self, Error> {
        Ok(Self {
            inner: shared::Cursor::with_properties(source, ordinal, properties, output, now)?,
            properties,
        })
    }
    pub fn value(&self) -> Option<View<'_>> {
        let view = self.inner.value()?;
        Some(View {
            properties: self.properties,
            ordinal: view.ordinal,
            members: view.members,
        })
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        self.inner.check_deadline(now)
    }
    pub fn poll(&mut self, now: Tick) -> Result<Status, Error> {
        self.inner.poll(now)
    }
    pub fn finish(
        self,
        now: Tick,
    ) -> Result<Retained<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 'm>, Error> {
        Ok(Retained {
            inner: self.inner.finish(now)?,
            properties: self.properties,
        })
    }
}
/// Exclusive original custody; passive selected bytes convey no publication authority.
/// ```compile_fail,E0277
/// fn bound<T: Copy>() {} bound::<td_mta::mime_response::selected_metadata_window::Retained<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn bound<T: Clone>() {} bound::<td_mta::mime_response::selected_metadata_window::Retained<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
pub struct Retained<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 'm> {
    inner: shared::Retained<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 'm>,
    properties: Properties,
}
impl<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 'm> Retained<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 'm> {
    pub fn value(&self) -> Option<View<'_>> {
        let view = self.inner.value()?;
        Some(View {
            properties: self.properties,
            ordinal: view.ordinal,
            members: view.members,
        })
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        self.inner.check_deadline(now)
    }
    pub fn finish(
        self,
        now: Tick,
    ) -> Result<Release<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 'm>, Error> {
        let (source, ordinal, members) = self.inner.finish(now)?;
        Ok((source, ordinal, self.properties, members))
    }
}
pub type Release<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 'm> = (
    Bound<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k>,
    u16,
    Properties,
    &'m [u8],
);
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
#[path = "selected_metadata_window/tests.rs"]
pub(in crate::mime_response) mod tests;

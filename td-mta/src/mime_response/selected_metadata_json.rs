//! Selected per-part fields under the original bounded tree/list framing.
use crate::mime_response::metadata_json as shared;
use crate::{nfc::HeaderBudget, ports::Tick};
pub use shared::{Mode, Progress, Status};
use {
    crate::mime_response::pinned::Error, crate::mime_response::selected_metadata::Properties,
    crate::mime_response::selected_metadata_collection::Serialized,
    crate::mime_response::selected_metadata_collection::View,
};
/// Exclusive original collection custody; bytes remain provisional.
/// ```compile_fail,E0277
/// fn required<T: Copy>() {} required::<td_mta::mime_response::selected_metadata_json::Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn required<T: Clone>() {} required::<td_mta::mime_response::selected_metadata_json::Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0308
/// use {td_mta::mime_response::selected_metadata_collection::View};
/// use {td_mta::mime_response::selected_metadata_json::Cursor, td_mta::mime_response::selected_metadata_json::Mode};
/// use td_mta::ports::Tick;
/// fn invalid(source: View<'_, '_, '_>) { let _ = Cursor::new(source, Mode::Structure, Tick(1)); }
/// ```
/// ```compile_fail,E0308
/// use {td_mta::mime_response::metadata_json::Cursor as Advanced};
/// use {td_mta::mime_response::selected_metadata_json::Cursor, td_mta::mime_response::selected_metadata_json::Mode};
/// use td_mta::ports::Tick;
/// fn invalid(source: Advanced<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>) { let _ = Cursor::new(source, Mode::Structure, Tick(1)); }
/// ```
pub struct Cursor<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm> {
    inner: shared::Cursor<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>,
    properties: Properties,
}
impl<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>
    Cursor<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>
{
    pub fn new(
        source: Serialized<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>,
        mode: Mode,
        now: Tick,
    ) -> Result<Self, Error> {
        Ok(Self {
            properties: source.properties,
            inner: shared::Cursor::new(source.inner, mode, now)?,
        })
    }
    pub fn poll(&mut self, now: Tick, output: &mut [u8]) -> Result<Progress, Error> {
        self.inner.poll(now, output)
    }
    pub fn finish(
        self,
        now: Tick,
    ) -> Result<Composed<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>, Error> {
        Ok(Composed {
            inner: self.inner.finish(now)?,
            properties: self.properties,
        })
    }
    pub fn value(&self) -> Option<(Mode, View<'_, 'm, 'o>)> {
        let (mode, original) = self.inner.value()?;
        Some((
            mode,
            View {
                properties: self.properties,
                original,
            },
        ))
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        self.inner.check_deadline(now)
    }
}
/// Exclusive original collection custody; bytes remain provisional.
/// ```compile_fail,E0277
/// fn required<T: Copy>() {} required::<td_mta::mime_response::selected_metadata_json::Composed<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn required<T: Clone>() {} required::<td_mta::mime_response::selected_metadata_json::Composed<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
pub struct Composed<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm> {
    inner: shared::Composed<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>,
    properties: Properties,
}
impl<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>
    Composed<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>
{
    pub fn finish(
        self,
        now: Tick,
    ) -> Result<(Serialized<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>, Mode), Error> {
        let (inner, mode) = self.inner.finish(now)?;
        Ok((
            Serialized {
                inner,
                properties: self.properties,
            },
            mode,
        ))
    }
    pub fn value(&self) -> Option<(Mode, View<'_, 'm, 'o>)> {
        let (mode, original) = self.inner.value()?;
        Some((
            mode,
            View {
                properties: self.properties,
                original,
            },
        ))
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        self.inner.check_deadline(now)
    }
}
const _: () = assert!(
    std::mem::size_of::<Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>()
        + std::mem::size_of::<HeaderBudget>()
        + 64
        <= 1024
);
const _: () =
    assert!(std::mem::size_of::<Composed<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>() <= 768);
#[cfg(test)]
pub use tests::probe as probe_allocations;
#[cfg(test)]
#[path = "selected_metadata_json/tests.rs"]
pub(in crate::mime_response) mod tests;

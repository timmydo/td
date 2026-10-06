//! Requested outer properties preserve selected metadata and original custody.
use crate::mime_response::pinned::Error;
use crate::mime_response::requested_body_json as shared;
use crate::mime_response::subparts_json as subparts;
use crate::{nfc::HeaderBudget, ports::Tick};
pub use shared::Properties;
use {
    crate::mime_response::selected_metadata_collection::Serialized,
    crate::mime_response::selected_metadata_collection::View,
};
pub use {
    crate::mime_response::selected_metadata_json::Progress,
    crate::mime_response::selected_metadata_json::Status,
};
/// Exclusive original collection custody; bytes remain provisional.
/// ```compile_fail,E0277
/// fn required<T: Copy>() {} required::<td_mta::mime_response::requested_selected_json::Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn required<T: Clone>() {} required::<td_mta::mime_response::requested_selected_json::Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0308
/// use {td_mta::mime_response::selected_metadata_collection::View as Source};
/// use {td_mta::mime_response::requested_selected_json::Cursor, td_mta::mime_response::requested_selected_json::Properties};
/// use td_mta::ports::Tick;
/// fn invalid(source: Source<'_, '_, '_>) { let _ = Cursor::new(source, Properties::ALL, Tick(1)); }
/// ```
/// ```compile_fail,E0308
/// use {td_mta::mime_response::requested_body_json::Cursor as Source};
/// use {td_mta::mime_response::requested_selected_json::Cursor, td_mta::mime_response::requested_selected_json::Properties};
/// use td_mta::ports::Tick;
/// fn invalid(source: Source<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>) { let _ = Cursor::new(source, Properties::ALL, Tick(1)); }
/// ```
pub struct Cursor<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm> {
    pub(in crate::mime_response) inner:
        subparts::Cursor<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>,
}
impl<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>
    Cursor<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>
{
    pub fn new(
        source: Serialized<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>,
        properties: Properties,
        now: Tick,
    ) -> Result<Self, Error> {
        Ok(Self {
            inner: subparts::Cursor::new(
                source,
                subparts::Selection {
                    properties,
                    sub_parts: true,
                },
                now,
            )?,
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
        })
    }
    pub fn value(&self) -> Option<(Properties, View<'_, 'm, 'o>)> {
        let (selection, view) = self.inner.value()?;
        Some((selection.properties, view))
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        self.inner.check_deadline(now)
    }
}
/// Exclusive original collection custody; bytes remain provisional.
/// ```compile_fail,E0277
/// fn required<T: Copy>() {} required::<td_mta::mime_response::requested_selected_json::Composed<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn required<T: Clone>() {} required::<td_mta::mime_response::requested_selected_json::Composed<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
pub struct Composed<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm> {
    inner: subparts::Composed<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>,
}
impl<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>
    Composed<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>
{
    pub fn finish(
        self,
        now: Tick,
    ) -> Result<
        (
            Serialized<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>,
            Properties,
        ),
        Error,
    > {
        let (serialized, selection) = self.inner.finish(now)?;
        Ok((serialized, selection.properties))
    }
    pub fn value(&self) -> Option<(Properties, View<'_, 'm, 'o>)> {
        let (selection, view) = self.inner.value()?;
        Some((selection.properties, view))
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
#[path = "requested_selected_json/tests.rs"]
pub(in crate::mime_response) mod tests;

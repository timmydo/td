//! Whole selected subParts output preserves all labels and original custody.
use crate::mime_response::requested_body_window as shared;
use crate::mime_response::selected_metadata::Properties as PartProperties;
pub use crate::mime_structure::Status;
use crate::{nfc::HeaderBudget, ports::Tick};
use {
    crate::mime_response::pinned::Error,
    crate::mime_response::selected_metadata_collection::Serialized,
    crate::mime_response::selected_metadata_collection::View,
    crate::mime_response::subparts_json::Selection,
};

/// Complete retained bytes, still provisional through authenticated publication.
#[derive(Clone, Copy)]
pub struct ViewBytes<'v, 'm, 'o> {
    pub selection: Selection,
    pub members: &'v [u8],
    pub original: View<'v, 'm, 'o>,
}
/// Only an original whole collection starts fresh composition into this window.
/// ```compile_fail,E0277
/// fn bound<T: Copy>() {} bound::<td_mta::mime_response::subparts_window::Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn bound<T: Clone>() {} bound::<td_mta::mime_response::subparts_window::Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0308
/// use {td_mta::mime_response::subparts_json::Cursor as Advanced, td_mta::mime_response::subparts_json::Selection, td_mta::mime_response::subparts_window::Cursor};
/// use td_mta::ports::Tick;
/// fn prefix_lost(cursor: Advanced<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>, output: &mut [u8]) { let _ = Cursor::new(cursor, Selection { properties: td_mta::mime_response::requested_selected_json::Properties::ALL, sub_parts: false }, output, Tick(1)); }
/// ```
/// ```compile_fail,E0308
/// use {td_mta::mime_response::subparts_json::Composed, td_mta::mime_response::subparts_json::Selection, td_mta::mime_response::subparts_window::Cursor};
/// use td_mta::ports::Tick;
/// fn emitted_only(composed: Composed<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>, output: &mut [u8]) { let _ = Cursor::new(composed, Selection { properties: td_mta::mime_response::requested_selected_json::Properties::ALL, sub_parts: false }, output, Tick(1)); }
/// ```
/// ```compile_fail,E0308
/// use {td_mta::mime_response::selected_metadata_collection::View, td_mta::mime_response::subparts_json::Selection, td_mta::mime_response::subparts_window::Cursor};
/// use td_mta::ports::Tick;
/// fn passive(view: View<'_, '_, '_>, output: &mut [u8]) { let _ = Cursor::new(view, Selection { properties: td_mta::mime_response::requested_selected_json::Properties::ALL, sub_parts: false }, output, Tick(1)); }
/// ```
/// ```compile_fail,E0308
/// use {td_mta::mime_response::metadata_collection::Serialized};
/// use {td_mta::mime_response::subparts_json::Selection, td_mta::mime_response::subparts_window::Cursor};
/// use td_mta::ports::Tick;
/// fn unselected(source: Serialized<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>, output: &mut [u8]) { let _ = Cursor::new(source, Selection { properties: td_mta::mime_response::requested_selected_json::Properties::ALL, sub_parts: false }, output, Tick(1)); }
/// ```
pub struct Cursor<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm, 'z> {
    inner: shared::Cursor<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm, 'z>,
    part_properties: PartProperties,
    selection: Selection,
}
impl<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm, 'z>
    Cursor<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm, 'z>
{
    pub fn new(
        source: Serialized<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>,
        selection: Selection,
        output: &'z mut [u8],
        now: Tick,
    ) -> Result<Self, Error> {
        Ok(Self {
            part_properties: source.properties,
            inner: shared::Cursor::with_sub_parts(
                source.inner,
                selection.properties,
                selection.sub_parts,
                output,
                now,
            )?,
            selection,
        })
    }
    #[cfg(test)]
    fn costs(&self) -> [u64; 5] {
        self.inner.costs()
    }

    pub fn value(&self) -> Option<ViewBytes<'_, 'm, 'o>> {
        let view = self.inner.value()?;
        Some(ViewBytes {
            selection: self.selection,
            members: view.members,
            original: View {
                properties: self.part_properties,
                original: view.original,
            },
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
    ) -> Result<Retained<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm, 'z>, Error> {
        Ok(Retained {
            inner: self.inner.finish(now)?,
            part_properties: self.part_properties,
            selection: self.selection,
        })
    }
}
/// Exclusive original completion and complete retained composition bytes.
/// ```compile_fail,E0277
/// fn bound<T: Copy>() {} bound::<td_mta::mime_response::subparts_window::Retained<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn bound<T: Clone>() {} bound::<td_mta::mime_response::subparts_window::Retained<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
pub struct Retained<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm, 'z> {
    inner: shared::Retained<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm, 'z>,
    part_properties: PartProperties,
    selection: Selection,
}
impl<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm, 'z>
    Retained<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm, 'z>
{
    pub fn value(&self) -> Option<ViewBytes<'_, 'm, 'o>> {
        let view = self.inner.value()?;
        Some(ViewBytes {
            selection: self.selection,
            members: view.members,
            original: View {
                properties: self.part_properties,
                original: view.original,
            },
        })
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        self.inner.check_deadline(now)
    }
    pub fn finish(
        self,
        now: Tick,
    ) -> Result<Release<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm, 'z>, Error> {
        let (inner, _, members) = self.inner.finish(now)?;
        Ok((
            Serialized {
                inner,
                properties: self.part_properties,
            },
            self.selection,
            members,
        ))
    }
}

const _: () = assert!(
    std::mem::size_of::<Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>()
        + std::mem::size_of::<HeaderBudget>()
        <= 1024
);
const _: () =
    assert!(std::mem::size_of::<Retained<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>() <= 768);

pub type Release<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm, 'z> = (
    Serialized<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>,
    Selection,
    &'z [u8],
);

#[cfg(test)]
pub use tests::probe as probe_allocations;
#[cfg(test)]
#[path = "subparts_window/tests.rs"]
pub(in crate::mime_response) mod tests;

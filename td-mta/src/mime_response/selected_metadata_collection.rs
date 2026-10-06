//! Collect every original ordinal with one immutable ten-field selection.
use crate::mime_response::metadata_collection as shared;
pub use crate::mime_response::selected_metadata_window::Status;
use crate::{nfc::HeaderBudget, ports::Tick};
pub use shared::Cell;
use {
    crate::mime_response::pinned::Bound, crate::mime_response::pinned::Error,
    crate::mime_response::selected_metadata::Properties,
    crate::mime_response::selected_metadata_window::View as MemberView,
};
#[derive(Clone, Copy)]
pub struct View<'v, 'm, 'o> {
    pub properties: Properties,
    pub original: shared::View<'v, 'm, 'o>,
}
/// Exclusive original collection custody; passive bytes confer no publication authority.
/// ```compile_fail,E0277
/// fn required<T: Copy>() {} required::<td_mta::mime_response::selected_metadata_collection::Collecting<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_> >();
/// ```
/// ```compile_fail,E0277
/// fn required<T: Clone>() {} required::<td_mta::mime_response::selected_metadata_collection::Collecting<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_> >();
/// ```
/// ```compile_fail,E0308
/// use {td_mta::ports::Tick, td_mta::mime_response::selected_metadata_collection::Collecting, td_mta::mime_response::selected_metadata_collection::Cell, td_mta::mime_response::selected_metadata_collection::View};
/// use {td_mta::mime_response::selected_metadata::Properties};
/// fn passive(source: View<'_, '_, '_>, cells: &mut [Cell<'_>]) { let _ = Collecting::new(source, Properties::ALL, cells, Tick(1)); }
/// ```
/// ```compile_fail,E0308
/// use {td_mta::ports::Tick, td_mta::mime_response::selected_metadata_window::Retained, td_mta::mime_response::selected_metadata_collection::Collecting, td_mta::mime_response::selected_metadata_collection::Cell};
/// use {td_mta::mime_response::selected_metadata::Properties};
/// fn passive(source: Retained<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_>, cells: &mut [Cell<'_>]) { let _ = Collecting::new(source, Properties::ALL, cells, Tick(1)); }
/// ```
/// ```compile_fail,E0624
/// use {td_mta::mime_response::metadata_collection::Collecting};
/// let _ = Collecting::with_properties;
/// ```
/// ```compile_fail,E0308
/// use {td_mta::mime_response::selected_metadata_collection::Serialized, td_mta::mime_response::metadata_json::Cursor, td_mta::mime_response::metadata_json::Mode};
/// use td_mta::ports::Tick;
/// fn invalid(source: Serialized<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>) { let _ = Cursor::new(source, Mode::Lists, Tick(1)); }
/// ```
pub struct Collecting<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm> {
    inner: shared::Collecting<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>,
    properties: Properties,
}
impl<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>
    Collecting<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>
{
    pub fn new(
        source: Bound<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k>,
        properties: Properties,
        cells: &'s mut [Cell<'m>],
        now: Tick,
    ) -> Result<Self, Error> {
        Ok(Self {
            inner: shared::Collecting::with_properties(source, properties, cells, now)?,
            properties,
        })
    }
    pub fn properties(&self) -> Properties {
        self.properties
    }
    pub fn completed(&self) -> Result<usize, Error> {
        self.inner.completed()
    }
    pub fn total(&self) -> Result<usize, Error> {
        self.inner.total()
    }
    pub fn next<'t>(
        &'t mut self,
        now: Tick,
    ) -> Result<Child<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 't, 'm>, Error> {
        Ok(Child {
            inner: self.inner.next(now)?,
            properties: self.properties,
        })
    }
    pub fn finish(
        self,
        now: Tick,
    ) -> Result<Serialized<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>, Error> {
        Ok(Serialized {
            inner: self.inner.finish(now)?,
            properties: self.properties,
        })
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        self.inner.check_deadline(now)
    }
}
/// One original ordinal handoff; abandonment poisons the borrowed parent.
/// ```compile_fail,E0277
/// fn required<T: Copy>() {} required::<td_mta::mime_response::selected_metadata_collection::Child<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_> >();
/// ```
/// ```compile_fail,E0277
/// fn required<T: Clone>() {} required::<td_mta::mime_response::selected_metadata_collection::Child<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_> >();
/// ```
pub struct Child<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 't, 'm> {
    inner: shared::Child<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 't, 'm>,
    properties: Properties,
}
impl<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 't, 'm> Child<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 't, 'm> {
    pub fn value(&self) -> Option<MemberView<'_>> {
        let view = self.inner.value()?;
        Some(MemberView {
            properties: self.properties,
            ordinal: view.ordinal,
            members: view.members,
        })
    }
    pub fn poll(&mut self, now: Tick) -> Result<Status, Error> {
        self.inner.poll(now)
    }
    pub fn finish(self, now: Tick) -> Result<(), Error> {
        self.inner.finish(now)
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        self.inner.check_deadline(now)
    }
}
/// Exclusive original collection custody; passive bytes confer no publication authority.
/// ```compile_fail,E0277
/// fn required<T: Copy>() {} required::<td_mta::mime_response::selected_metadata_collection::Serialized<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_> >();
/// ```
/// ```compile_fail,E0277
/// fn required<T: Clone>() {} required::<td_mta::mime_response::selected_metadata_collection::Serialized<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_> >();
/// ```
pub struct Serialized<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm> {
    pub(in crate::mime_response) inner:
        shared::Serialized<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>,
    pub(in crate::mime_response) properties: Properties,
}
impl<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>
    Serialized<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>
{
    pub fn value(&self) -> Option<View<'_, 'm, 'o>> {
        Some(View {
            properties: self.properties,
            original: self.inner.value()?,
        })
    }
    pub fn finish(
        self,
        now: Tick,
    ) -> Result<Release<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>, Error> {
        let (source, cells) = self.inner.finish(now)?;
        Ok((source, self.properties, cells))
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        self.inner.check_deadline(now)
    }
}
pub type Release<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm> = (
    Bound<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k>,
    Properties,
    &'s [Cell<'m>],
);
const _: () = assert!(std::mem::size_of::<Cell<'_>>() <= 64);
const _: () = assert!(
    std::mem::size_of::<Collecting<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>()
        + std::mem::size_of::<Child<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>()
        + std::mem::size_of::<HeaderBudget>()
        <= 2048
);
const _: () =
    assert!(std::mem::size_of::<Serialized<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>() <= 768);

#[cfg(test)]
pub use tests::probe as probe_allocations;
#[cfg(test)]
#[path = "selected_metadata_collection/tests.rs"]
pub(in crate::mime_response) mod tests;

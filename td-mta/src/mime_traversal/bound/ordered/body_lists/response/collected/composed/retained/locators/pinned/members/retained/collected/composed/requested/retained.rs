//! Whole requested members through the shared source-bound retention core.
use super::super::retained as shared;
use super::{Error, Properties, Serialized, View};
pub use crate::mime_traversal::Status;
use crate::{nfc::HeaderBudget, ports::Tick};

/// Complete retained bytes, still provisional through authenticated publication.
#[derive(Clone, Copy)]
pub struct ViewBytes<'v, 'm, 'o> {
    pub properties: Properties,
    pub members: &'v [u8],
    pub original: View<'v, 'm, 'o>,
}
/// Only an original whole collection starts fresh composition into this window.
/// ```compile_fail,E0277
/// fn bound<T: Copy>() {} bound::<td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::retained::collected::composed::requested::retained::Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn bound<T: Clone>() {} bound::<td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::retained::collected::composed::requested::retained::Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0308
/// use td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::retained::collected::composed::requested::{Cursor as Advanced, Properties, retained::Cursor};
/// use td_mta::ports::Tick;
/// fn prefix_lost(cursor: Advanced<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>, output: &mut [u8]) { let _ = Cursor::new(cursor, Properties::ALL, output, Tick(1)); }
/// ```
/// ```compile_fail,E0308
/// use td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::retained::collected::composed::requested::{Composed, Properties, retained::Cursor};
/// use td_mta::ports::Tick;
/// fn emitted_only(composed: Composed<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>, output: &mut [u8]) { let _ = Cursor::new(composed, Properties::ALL, output, Tick(1)); }
/// ```
pub struct Cursor<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm, 'z> {
    inner: shared::Cursor<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm, 'z>,
    properties: Properties,
}
impl<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm, 'z>
    Cursor<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm, 'z>
{
    pub fn new(
        source: Serialized<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>,
        properties: Properties,
        output: &'z mut [u8],
        now: Tick,
    ) -> Result<Self, Error> {
        Self::with_sub_parts(source, properties, true, output, now)
    }
    pub(in super::super::super::super::super) fn with_sub_parts(
        source: Serialized<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>,
        properties: Properties,
        sub_parts: bool,
        output: &'z mut [u8],
        now: Tick,
    ) -> Result<Self, Error> {
        Ok(Self {
            inner: shared::Cursor::with_frame(
                source,
                super::framing::Frame::requested(properties.body_structure, properties.bits())
                    .with_sub_parts(sub_parts),
                output,
                now,
            )?,
            properties,
        })
    }
    #[cfg(test)]
    pub(in super::super::super::super::super) fn costs(&self) -> [u64; 5] {
        self.inner.costs()
    }
    pub fn value(&self) -> Option<ViewBytes<'_, 'm, 'o>> {
        let view = self.inner.value()?;
        Some(ViewBytes {
            properties: self.properties,
            members: view.members,
            original: view.original,
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
            properties: self.properties,
        })
    }
}
/// Exclusive original completion and complete retained composition bytes.
/// ```compile_fail,E0277
/// fn bound<T: Copy>() {} bound::<td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::retained::collected::composed::requested::retained::Retained<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn bound<T: Clone>() {} bound::<td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::retained::collected::composed::requested::retained::Retained<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
pub struct Retained<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm, 'z> {
    inner: shared::Retained<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm, 'z>,
    properties: Properties,
}
impl<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm, 'z>
    Retained<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm, 'z>
{
    pub fn value(&self) -> Option<ViewBytes<'_, 'm, 'o>> {
        let view = self.inner.value()?;
        Some(ViewBytes {
            properties: self.properties,
            members: view.members,
            original: view.original,
        })
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        self.inner.check_deadline(now)
    }
    pub fn finish(
        self,
        now: Tick,
    ) -> Result<Release<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm, 'z>, Error> {
        let (source, _, members) = self.inner.finish(now)?;
        Ok((source, self.properties, members))
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
    Properties,
    &'z [u8],
);

#[cfg(test)]
pub use tests::probe as probe_allocations;
#[cfg(test)]
#[path = "retained/tests.rs"]
mod tests;

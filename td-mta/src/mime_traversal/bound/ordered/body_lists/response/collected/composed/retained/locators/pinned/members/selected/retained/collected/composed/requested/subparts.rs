//! Explicit subParts selection shares bounded framing and original custody.
pub mod retained;

use super::super::super::{Serialized, View};
use super::super::Error;
use super::super::Properties as PartProperties;
pub use super::super::{Progress, Status};
use super::shared;
use crate::{nfc::HeaderBudget, ports::Tick};
/// Pure outer-property and structural choices; no ownership or permission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Selection {
    pub properties: super::Properties,
    pub sub_parts: bool,
}
/// Exclusive original collection custody; bytes remain provisional.
/// ```compile_fail,E0277
/// fn required<T: Copy>() {} required::<td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::selected::retained::collected::composed::requested::subparts::Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn required<T: Clone>() {} required::<td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::selected::retained::collected::composed::requested::subparts::Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0308
/// use td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::selected::retained::collected::View as Source;
/// use td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::selected::retained::collected::composed::requested::subparts::{Cursor, Selection};
/// use td_mta::ports::Tick;
/// fn invalid(source: Source<'_, '_, '_>) { let _ = Cursor::new(source, Selection { properties: td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::selected::retained::collected::composed::requested::Properties::ALL, sub_parts: false }, Tick(1)); }
/// ```
/// ```compile_fail,E0308
/// use td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::retained::collected::composed::requested::Cursor as Source;
/// use td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::selected::retained::collected::composed::requested::subparts::{Cursor, Selection};
/// use td_mta::ports::Tick;
/// fn invalid(source: Source<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>) { let _ = Cursor::new(source, Selection { properties: td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::selected::retained::collected::composed::requested::Properties::ALL, sub_parts: false }, Tick(1)); }
/// ```
/// ```compile_fail,E0308
/// use td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::selected::retained::collected::composed::requested::{Cursor as Advanced, Properties, subparts::{Cursor, Selection}};
/// use td_mta::ports::Tick;
/// fn invalid(source: Advanced<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>) { let _ = Cursor::new(source, Selection { properties: Properties::ALL, sub_parts: false }, Tick(1)); }
/// ```
pub struct Cursor<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm> {
    inner: shared::Cursor<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>,
    part_properties: PartProperties,
    selection: Selection,
}
impl<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>
    Cursor<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>
{
    pub fn new(
        source: Serialized<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>,
        selection: Selection,
        now: Tick,
    ) -> Result<Self, Error> {
        Ok(Self {
            part_properties: source.properties,
            inner: shared::Cursor::with_sub_parts(
                source.inner,
                selection.properties,
                selection.sub_parts,
                now,
            )?,
            selection,
        })
    }
    #[cfg(test)]
    pub(super) fn costs(&self) -> [u64; 5] {
        self.inner.costs()
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
            part_properties: self.part_properties,
            selection: self.selection,
        })
    }
    pub fn value(&self) -> Option<(Selection, View<'_, 'm, 'o>)> {
        let (_, original) = self.inner.value()?;
        Some((
            self.selection,
            View {
                properties: self.part_properties,
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
/// fn required<T: Copy>() {} required::<td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::selected::retained::collected::composed::requested::subparts::Composed<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn required<T: Clone>() {} required::<td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::selected::retained::collected::composed::requested::subparts::Composed<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
pub struct Composed<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm> {
    inner: shared::Composed<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>,
    part_properties: PartProperties,
    selection: Selection,
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
            Selection,
        ),
        Error,
    > {
        let (inner, _) = self.inner.finish(now)?;
        Ok((
            Serialized {
                inner,
                properties: self.part_properties,
            },
            self.selection,
        ))
    }
    pub fn value(&self) -> Option<(Selection, View<'_, 'm, 'o>)> {
        let (_, original) = self.inner.value()?;
        Some((
            self.selection,
            View {
                properties: self.part_properties,
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
mod tests;

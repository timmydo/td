//! Collect every original ordinal with one immutable ten-field selection.
use super::super::super::retained::collected as shared;
pub use super::Status;
use super::{Bound, Error, Properties, View as MemberView};
use crate::{nfc::HeaderBudget, ports::Tick};
pub use shared::Cell;
#[derive(Clone, Copy)]
pub struct View<'v, 'm, 'o> {
    pub properties: Properties,
    pub original: shared::View<'v, 'm, 'o>,
}
/// Exclusive original collection custody; passive bytes confer no publication authority.
/// ```compile_fail,E0277
/// fn required<T: Copy>() {} required::<td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::selected::retained::collected::Collecting<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_> >();
/// ```
/// ```compile_fail,E0277
/// fn required<T: Clone>() {} required::<td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::selected::retained::collected::Collecting<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_> >();
/// ```
/// ```compile_fail,E0308
/// use td_mta::{ports::Tick, mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::selected::retained::collected::{Collecting, Cell, View}};
/// use td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::selected::Properties;
/// fn passive(source: View<'_, '_, '_>, cells: &mut [Cell<'_>]) { let _ = Collecting::new(source, Properties::ALL, cells, Tick(1)); }
/// ```
/// ```compile_fail,E0308
/// use td_mta::{ports::Tick, mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::selected::retained::{Retained, collected::{Collecting, Cell}}};
/// use td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::selected::Properties;
/// fn passive(source: Retained<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_>, cells: &mut [Cell<'_>]) { let _ = Collecting::new(source, Properties::ALL, cells, Tick(1)); }
/// ```
/// ```compile_fail,E0624
/// use td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::retained::collected::Collecting;
/// let _ = Collecting::with_properties;
/// ```
/// ```compile_fail,E0308
/// use td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::{selected::retained::collected::Serialized, retained::collected::composed::{Cursor, Mode}};
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
/// fn required<T: Copy>() {} required::<td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::selected::retained::collected::Child<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_> >();
/// ```
/// ```compile_fail,E0277
/// fn required<T: Clone>() {} required::<td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::selected::retained::collected::Child<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_> >();
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
/// fn required<T: Copy>() {} required::<td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::selected::retained::collected::Serialized<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_> >();
/// ```
/// ```compile_fail,E0277
/// fn required<T: Clone>() {} required::<td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::selected::retained::collected::Serialized<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_> >();
/// ```
pub struct Serialized<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm> {
    inner: shared::Serialized<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>,
    properties: Properties,
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
mod tests;

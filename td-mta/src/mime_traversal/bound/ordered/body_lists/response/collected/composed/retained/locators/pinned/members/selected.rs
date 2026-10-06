//! Pure ten-field selection over original generated metadata and bound locators.
mod index;
pub mod retained;
pub use super::Status;
use super::{Bound, Error, Progress};
use crate::{nfc::HeaderBudget, ports::Tick};
pub(super) use index::Index;
/// Caller selection only; request defaults and authorization are separate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Properties {
    pub part_id: bool,
    pub size: bool,
    pub media_type: bool,
    pub charset: bool,
    pub name: bool,
    pub disposition: bool,
    pub cid: bool,
    pub language: bool,
    pub location: bool,
    pub blob_id: bool,
}
impl Properties {
    pub const NONE: Self = Self {
        part_id: false,
        size: false,
        media_type: false,
        charset: false,
        name: false,
        disposition: false,
        cid: false,
        language: false,
        location: false,
        blob_id: false,
    };
    pub const ALL: Self = Self {
        part_id: true,
        size: true,
        media_type: true,
        charset: true,
        name: true,
        disposition: true,
        cid: true,
        language: true,
        location: true,
        blob_id: true,
    };
    pub(super) fn bits(self) -> u16 {
        self.part_id as u16
            | ((self.size as u16) << 1)
            | ((self.media_type as u16) << 2)
            | ((self.charset as u16) << 3)
            | ((self.name as u16) << 4)
            | ((self.disposition as u16) << 5)
            | ((self.cid as u16) << 6)
            | ((self.language as u16) << 7)
            | ((self.location as u16) << 8)
            | ((self.blob_id as u16) << 9)
    }
}
/// Exclusive original source-matched custody through selected part emission.
/// ```compile_fail,E0277
/// fn bound<T: Copy>() {} bound::<td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::selected::Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn bound<T: Clone>() {} bound::<td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::selected::Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0308
/// use td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::selected::{Cursor, Properties};
/// use td_mta::{ports::Tick, mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::View};
/// fn passive(view: View<'_, '_>) { let _ = Cursor::new(view, 1, Properties::ALL, Tick(1)); }
/// ```
/// ```compile_fail,E0308
/// use td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::selected::{Cursor, Properties};
/// use td_mta::ports::Tick;
/// fn advanced(cursor: td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_>) { let _ = Cursor::new(cursor, 1, Properties::ALL, Tick(1)); }
/// ```
pub struct Cursor<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k> {
    inner: super::Cursor<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k>,
    properties: Properties,
}
impl<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k> Cursor<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k> {
    pub fn new(
        source: Bound<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k>,
        ordinal: u16,
        properties: Properties,
        now: Tick,
    ) -> Result<Self, Error> {
        let inner = super::Cursor::with_properties(source, ordinal, properties, now)?;
        Ok(Self { inner, properties })
    }
    pub fn value(&self) -> Option<(Properties, u16)> {
        Some((self.properties, self.inner.value()?))
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        self.inner.check_deadline(now)
    }
    pub fn poll(&mut self, now: Tick, output: &mut [u8]) -> Result<Progress, Error> {
        self.inner.poll(now, output)
    }
    pub fn finish(self, now: Tick) -> Result<Member<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k>, Error> {
        Ok(Member {
            inner: self.inner.finish(now)?,
            properties: self.properties,
        })
    }
}
/// Completed emission retains actual source matching and immutable selection.
/// ```compile_fail,E0277
/// fn bound<T: Copy>() {} bound::<td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::selected::Member<'_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn bound<T: Clone>() {} bound::<td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::selected::Member<'_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
pub struct Member<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k> {
    inner: super::Member<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k>,
    properties: Properties,
}
pub type Release<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k> =
    (Bound<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k>, u16, Properties);
impl<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k> Member<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k> {
    pub fn value(&self) -> Option<(Properties, u16)> {
        Some((self.properties, self.inner.value()?))
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        self.inner.check_deadline(now)
    }
    pub fn finish(self, now: Tick) -> Result<Release<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k>, Error> {
        let (source, ordinal) = self.inner.finish(now)?;
        Ok((source, ordinal, self.properties))
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
mod tests;

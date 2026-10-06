//! Requested tree and list-property members under original collection and pin custody.
pub mod retained;
use super::{Error, Progress, Serialized, View};
use crate::mime_traversal::bound::ordered::body_lists::response::collected::composed::framing;
use crate::{nfc::HeaderBudget, ports::Tick};
/// Pure caller selection; neither source ownership nor permission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Properties {
    pub body_structure: bool,
    pub text_body: bool,
    pub html_body: bool,
    pub attachments: bool,
    pub has_attachment: bool,
}
impl Properties {
    pub const NONE: Self = Self {
        body_structure: false,
        text_body: false,
        html_body: false,
        attachments: false,
        has_attachment: false,
    };
    pub const ALL: Self = Self {
        body_structure: true,
        text_body: true,
        html_body: true,
        attachments: true,
        has_attachment: true,
    };
    const fn bits(self) -> u8 {
        self.text_body as u8
            | ((self.html_body as u8) << 1)
            | ((self.attachments as u8) << 2)
            | ((self.has_attachment as u8) << 3)
    }
}
/// Exclusive original custody through requested provisional emission.
/// ```compile_fail,E0277
/// fn required<T: Copy>() {} required::<td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::retained::collected::composed::requested::Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn required<T: Clone>() {} required::<td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::retained::collected::composed::requested::Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0308
/// use td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::retained::collected::{View, composed::requested::{Cursor, Properties}};
/// use td_mta::ports::Tick;
/// fn passive(view: View<'_, '_, '_>) { let _ = Cursor::new(view, Properties::ALL, Tick(1)); }
/// ```
/// ```compile_fail,E0308
/// use td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::retained::collected::composed::{Cursor as Advanced, requested::{Cursor, Properties}};
/// use td_mta::ports::Tick;
/// fn advanced(cursor: Advanced<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>) { let _ = Cursor::new(cursor, Properties::ALL, Tick(1)); }
/// ```
pub struct Cursor<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm> {
    original: super::Cursor<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>,
    properties: Properties,
}
impl<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>
    Cursor<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>
{
    pub fn new(
        source: Serialized<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>,
        properties: Properties,
        now: Tick,
    ) -> Result<Self, Error> {
        let original = super::Cursor::with_frame(
            source,
            framing::Frame::requested(properties.body_structure, properties.bits()),
            now,
        )?;
        Ok(Self {
            original,
            properties,
        })
    }
    pub fn value(&self) -> Option<(Properties, View<'_, 'm, 'o>)> {
        Some((self.properties, self.original.value()?.1))
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        self.original.check_deadline(now)
    }
    pub fn poll(&mut self, now: Tick, output: &mut [u8]) -> Result<Progress, Error> {
        self.original.poll(now, output)
    }
    pub fn finish(
        self,
        now: Tick,
    ) -> Result<Composed<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>, Error> {
        Ok(Composed {
            original: self.original.finish(now)?,
            properties: self.properties,
        })
    }
}
/// Exclusive original custody after complete requested emission.
/// ```compile_fail,E0277
/// fn required<T: Copy>() {} required::<td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::retained::collected::composed::requested::Composed<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn required<T: Clone>() {} required::<td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::retained::collected::composed::requested::Composed<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
pub struct Composed<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm> {
    original: super::Composed<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>,
    properties: Properties,
}
impl<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>
    Composed<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>
{
    pub fn value(&self) -> Option<(Properties, View<'_, 'm, 'o>)> {
        Some((self.properties, self.original.value()?.1))
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        self.original.check_deadline(now)
    }
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
        let (source, _) = self.original.finish(now)?;
        Ok((source, self.properties))
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

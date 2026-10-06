//! Fixed whole-member retention bound to a fresh original composer.
use super::{Composed, Error, Mode, Serialized, View};
pub use crate::mime_traversal::Status;
use crate::{nfc::HeaderBudget, ports::Tick};
use td_json::{retain::Window, string::Status as FrameStatus};

fn window_error(error: td_json::retain::Error) -> Error {
    match error {
        td_json::retain::Error::Capacity => Error::Original(super::OriginalError::ResponseCapacity),
        td_json::retain::Error::InvalidState => Error::InvalidState,
    }
}
/// Complete retained bytes, still provisional through authenticated publication.
#[derive(Clone, Copy)]
pub struct ViewBytes<'v, 'm, 'o> {
    pub mode: Mode,
    pub members: &'v [u8],
    pub original: View<'v, 'm, 'o>,
}
/// Only an original whole collection starts fresh composition into this window.
/// ```compile_fail,E0277
/// fn bound<T: Copy>() {} bound::<td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::retained::collected::composed::retained::Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn bound<T: Clone>() {} bound::<td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::retained::collected::composed::retained::Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0308
/// use td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::retained::collected::composed::{Cursor as Advanced, Mode, retained::Cursor};
/// use td_mta::ports::Tick;
/// fn prefix_lost(cursor: Advanced<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>, output: &mut [u8]) { let _ = Cursor::new(cursor, Mode::Structure, output, Tick(1)); }
/// ```
/// ```compile_fail,E0308
/// use td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::retained::collected::composed::{Composed, Mode, retained::Cursor};
/// use td_mta::ports::Tick;
/// fn emitted_only(composed: Composed<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>, output: &mut [u8]) { let _ = Cursor::new(composed, Mode::Structure, output, Tick(1)); }
/// ```
pub struct Cursor<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm, 'z> {
    composer: super::Cursor<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>,
    window: Window<'z>,
}
impl<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm, 'z>
    Cursor<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm, 'z>
{
    pub fn new(
        source: Serialized<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>,
        mode: Mode,
        output: &'z mut [u8],
        now: Tick,
    ) -> Result<Self, Error> {
        Self::with_frame(source, super::framing::Frame::new(mode), output, now)
    }
    pub(super) fn with_frame(
        source: Serialized<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>,
        frame: super::framing::Frame,
        output: &'z mut [u8],
        now: Tick,
    ) -> Result<Self, Error> {
        Ok(Self {
            composer: super::Cursor::with_frame(source, frame, now)?,
            window: Window::new(output),
        })
    }
    #[cfg(test)]
    pub(super) fn costs(&self) -> [u64; 5] {
        super::tests::costs(&self.composer)
    }
    pub fn value(&self) -> Option<ViewBytes<'_, 'm, 'o>> {
        let (mode, original) = self.composer.value()?;
        Some(ViewBytes {
            mode,
            members: self.window.provisional()?,
            original,
        })
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        self.composer.check_deadline(now)
    }
    pub fn poll(&mut self, now: Tick) -> Result<Status, Error> {
        if let Some(error) = self.composer.failure {
            return Err(error);
        }
        if self.composer.frame.complete() {
            return Ok(Status::Complete);
        }
        self.check_deadline(now)?;
        let result = self.step(now);
        self.composer.outcome(result)
    }
    fn step(&mut self, now: Tick) -> Result<Status, Error> {
        let tail = self.window.tail().map_err(window_error)?;
        let progress = self.composer.poll(now, tail)?;
        self.window
            .advance(progress.written)
            .map_err(window_error)?;
        match progress.status {
            FrameStatus::Complete => Ok(Status::Complete),
            FrameStatus::Yield => Ok(Status::Yield),
            FrameStatus::NeedOutput => Err(Error::InvalidState),
        }
    }
    pub fn finish(
        mut self,
        now: Tick,
    ) -> Result<Retained<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm, 'z>, Error> {
        self.check_deadline(now)?;
        if self.composer.value().is_none() {
            return self.composer.outcome(Err(Error::InvalidState));
        }
        let members = match self.window.into_slice() {
            Ok(bytes) => bytes,
            Err(error) => return self.composer.outcome(Err(window_error(error))),
        };
        Ok(Retained {
            original: self.composer.finish(now)?,
            members,
        })
    }
}
/// Exclusive original completion and complete retained composition bytes.
/// ```compile_fail,E0277
/// fn bound<T: Copy>() {} bound::<td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::retained::collected::composed::retained::Retained<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn bound<T: Clone>() {} bound::<td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::retained::collected::composed::retained::Retained<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
pub struct Retained<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm, 'z> {
    original: Composed<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>,
    members: &'z [u8],
}
impl<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm, 'z>
    Retained<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm, 'z>
{
    pub fn value(&self) -> Option<ViewBytes<'_, 'm, 'o>> {
        let (mode, original) = self.original.value()?;
        Some(ViewBytes {
            mode,
            members: self.members,
            original,
        })
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        self.original.check_deadline(now)
    }
    pub fn finish(
        self,
        now: Tick,
    ) -> Result<Release<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm, 'z>, Error> {
        let (source, mode) = self.original.finish(now)?;
        Ok((source, mode, self.members))
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
    Mode,
    &'z [u8],
);

#[cfg(test)]
pub use tests::probe as probe_allocations;
#[cfg(test)]
#[path = "retained/tests.rs"]
mod tests;

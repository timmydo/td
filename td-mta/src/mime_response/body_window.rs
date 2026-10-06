//! Fixed whole-member retention bound to a fresh original composer.
pub use crate::mime_structure::Status;
use crate::{admission::work::Meter, nfc::HeaderBudget, ports::Tick};
use td_json::{retain::Window, string::Status as FrameStatus};
use {
    crate::mime_response::body_json::Composed, crate::mime_response::body_json::Mode,
    crate::mime_response::bound::Error, crate::mime_response::part_collection::Serialized,
    crate::mime_response::part_collection::View,
};

fn window_error(error: td_json::retain::Error) -> Error {
    match error {
        td_json::retain::Error::Capacity => Error::ResponseCapacity,
        td_json::retain::Error::InvalidState => Error::InvalidState,
    }
}
/// Original passive handoff plus complete retained bytes.
pub type Release<'w, 'n, 'c, 'o, 'r> = (
    (Mode, &'r [u8], View<'w, 'n, 'c, 'o>),
    &'w mut Meter,
    &'w mut HeaderBudget,
);
/// Complete retained bytes, still provisional through authenticated publication.
#[derive(Clone, Copy)]
pub struct ViewBytes<'v, 'o> {
    pub mode: Mode,
    pub members: &'v [u8],
    pub original: View<'v, 'v, 'v, 'o>,
}
/// Only an original whole collection starts fresh composition into this window.
/// ```compile_fail,E0277
/// fn bound<T: Copy>() {} bound::<td_mta::mime_response::body_window::Cursor<'_, '_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn bound<T: Clone>() {} bound::<td_mta::mime_response::body_window::Cursor<'_, '_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0308
/// use {td_mta::mime_response::body_json::Cursor as Advanced, td_mta::mime_response::body_json::Mode, td_mta::mime_response::body_window::Cursor};
/// use td_mta::ports::Tick;
/// fn prefix_lost(cursor: Advanced<'_, '_, '_, '_, '_>, output: &mut [u8]) { let _ = Cursor::new(cursor, Mode::Structure, output, Tick(1)); }
/// ```
/// ```compile_fail,E0308
/// use {td_mta::mime_response::body_json::Composed, td_mta::mime_response::body_json::Mode, td_mta::mime_response::body_window::Cursor};
/// use td_mta::ports::Tick;
/// fn emitted_only(composed: Composed<'_, '_, '_, '_, '_>, output: &mut [u8]) { let _ = Cursor::new(composed, Mode::Structure, output, Tick(1)); }
/// ```
pub struct Cursor<'a, 'w, 'n, 'c, 'o, 'r> {
    composer: crate::mime_response::body_json::Cursor<'a, 'w, 'n, 'c, 'o>,
    window: Window<'r>,
}
impl<'a, 'w, 'n, 'c, 'o, 'r> Cursor<'a, 'w, 'n, 'c, 'o, 'r> {
    pub fn new(
        source: Serialized<'a, 'w, 'n, 'c, 'o>,
        mode: Mode,
        output: &'r mut [u8],
        now: Tick,
    ) -> Result<Self, Error> {
        Ok(Self {
            composer: crate::mime_response::body_json::Cursor::new(source, mode, now)?,
            window: Window::new(output),
        })
    }
    pub fn value(&self) -> Option<ViewBytes<'_, 'o>> {
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
        if self.composer.value().is_some() {
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
    pub fn finish(mut self, now: Tick) -> Result<Retained<'a, 'w, 'n, 'c, 'o, 'r>, Error> {
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
/// fn bound<T: Copy>() {} bound::<td_mta::mime_response::body_window::Retained<'_, '_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn bound<T: Clone>() {} bound::<td_mta::mime_response::body_window::Retained<'_, '_, '_, '_, '_, '_>>();
/// ```
pub struct Retained<'a, 'w, 'n, 'c, 'o, 'r> {
    pub(in crate::mime_response) original: Composed<'a, 'w, 'n, 'c, 'o>,
    members: &'r [u8],
}
impl<'w, 'n, 'c, 'o, 'r> Retained<'_, 'w, 'n, 'c, 'o, 'r> {
    pub fn value(&self) -> Option<ViewBytes<'_, 'o>> {
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
    pub fn finish(self, now: Tick) -> Result<Release<'w, 'n, 'c, 'o, 'r>, Error> {
        let ((mode, view), work, budget) = self.original.finish(now)?;
        Ok(((mode, self.members, view), work, budget))
    }
}
const _: () = assert!(
    std::mem::size_of::<Cursor<'_, '_, '_, '_, '_, '_>>() + std::mem::size_of::<HeaderBudget>()
        <= 1024
);
const _: () = assert!(std::mem::size_of::<Retained<'_, '_, '_, '_, '_, '_>>() <= 256);

#[cfg(test)]
#[path = "body_window/tests.rs"]
pub(in crate::mime_response) mod tests;

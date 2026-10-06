//! Fixed retention of one original replay fragment before visitation can advance.
pub use crate::mime_structure::Status;
use crate::{
    admission::work::Meter,
    nfc::{HeaderBudget, Scratch},
    ports::Tick,
};
use td_json::{retain::Window, string::Status as FrameStatus};
use {
    crate::mime_response::bound::Error, crate::mime_response::part_json::End,
    crate::mime_response::response::Part,
};

/// Passive complete member bytes, still provisional through whole-job publication.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Retained<'o> {
    pub fragment: &'o [u8],
    pub end: End,
}
fn window_error(error: td_json::retain::Error) -> Error {
    match error {
        td_json::retain::Error::Capacity => Error::ResponseCapacity,
        td_json::retain::Error::InvalidState => Error::InvalidState,
    }
}
/// One distinct caller-reserved window, bound to a fresh original replay child.
/// Safe forgetting and any retention refusal leave the parent retired.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mta::mime_response::part_window::Cursor<'_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mta::mime_response::part_window::Cursor<'_, '_>>();
/// ```
/// ```compile_fail,E0308
/// use {td_mta::mime_response::part_json::Cursor as Framing, td_mta::mime_response::part_window::Cursor, td_mta::ports::Tick};
/// fn advanced(framing: Framing<'_>, output: &mut [u8]) { let _ = Cursor::new(framing, output, Tick(1)); }
/// ```
pub struct Cursor<'w, 'o> {
    framing: crate::mime_response::part_json::Cursor<'w>,
    window: Window<'o>,
}
impl<'w, 'o> Cursor<'w, 'o> {
    pub fn new(part: Part<'_, 'w>, output: &'o mut [u8], now: Tick) -> Result<Self, Error> {
        Ok(Self {
            framing: crate::mime_response::part_json::Cursor::new(part, now)?,
            window: Window::new(output),
        })
    }
    pub fn value(&self) -> Option<Retained<'_>> {
        Some(Retained {
            end: self.framing.value()?,
            fragment: self.window.provisional()?,
        })
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        self.framing.check_deadline(now)
    }
    pub fn poll(&mut self, now: Tick) -> Result<Status, Error> {
        if let Some(error) = self.framing.failure {
            return Err(error);
        }
        if self.framing.value().is_some() {
            return Ok(Status::Complete);
        }
        self.check_deadline(now)?;
        let result = self.step(now);
        self.framing.outcome(result)
    }
    fn step(&mut self, now: Tick) -> Result<Status, Error> {
        let tail = self.window.tail().map_err(window_error)?;
        let progress = self.framing.poll(now, tail)?;
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
    ) -> Result<
        (
            Retained<'o>,
            &'w mut Meter,
            &'w mut HeaderBudget,
            &'w mut Scratch,
        ),
        Error,
    > {
        self.check_deadline(now)?;
        if self.framing.value().is_none() {
            return self.framing.outcome(Err(Error::InvalidState));
        }
        let fragment = match self.window.into_slice() {
            Ok(fragment) => fragment,
            Err(error) => return self.framing.outcome(Err(window_error(error))),
        };
        let (end, work, budget, scratch) = self.framing.finish(now)?;
        Ok((Retained { fragment, end }, work, budget, scratch))
    }
}
const _: () =
    assert!(std::mem::size_of::<Cursor<'_, '_>>() + std::mem::size_of::<HeaderBudget>() <= 1024);
#[cfg(test)]
#[path = "part_window/tests.rs"]
pub(in crate::mime_response) mod tests;

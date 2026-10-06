//! Whole header arrays in one caller-reserved fixed window.
use super::{Completion, Error, Input, Progress, Status};
use crate::{admission::work::Meter, nfc::HeaderBudget, ports::Tick};
use td_json::retain::Window;

/// Passive complete bytes; original pin and publication admission stay external.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Retained<'o> {
    pub fragment: &'o [u8],
    pub end: super::End,
    pub is_encoding_problem: bool,
}
fn window_error(error: td_json::retain::Error) -> Error {
    match error {
        td_json::retain::Error::Capacity => Error::ResponseCapacity,
        td_json::retain::Error::InvalidState => Error::InvalidState,
    }
}
/// Constructs its own fresh cursor; emitted prefixes cannot start retention.
/// ```compile_fail,E0277
/// fn bound<T: Copy>() {} bound::<td_mta::mime_headers::json::retained::Cursor<'_, '_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn bound<T: Clone>() {} bound::<td_mta::mime_headers::json::retained::Cursor<'_, '_, '_>>();
/// ```
/// ```compile_fail,E0308
/// use td_mta::{mime_headers::json::{Cursor as Stream, retained::Cursor}, admission::work::Meter, nfc::HeaderBudget, ports::Tick};
/// fn advanced(stream: Stream<'_, '_>, work: &mut Meter, budget: &mut HeaderBudget, output: &mut [u8]) { let _ = Cursor::new(stream, work, budget, output, Tick(1)); }
/// ```
/// ```compile_fail,E0308
/// use td_mta::{mime_headers::json::{Completion, retained::Cursor}, admission::work::Meter, nfc::HeaderBudget, ports::Tick};
/// fn emitted(done: Completion<'_>, work: &mut Meter, budget: &mut HeaderBudget, output: &mut [u8]) { let _ = Cursor::new(done, work, budget, output, Tick(1)); }
/// ```
pub struct Cursor<'a, 'w, 'o> {
    framing: super::Cursor<'a, 'w>,
    window: Window<'o>,
}
impl<'a, 'w, 'o> Cursor<'a, 'w, 'o> {
    pub fn new(
        input: Input<'a>,
        work: &'w mut Meter,
        budget: &'w mut HeaderBudget,
        output: &'o mut [u8],
        now: Tick,
    ) -> Result<Self, Error> {
        Ok(Self {
            framing: super::Cursor::new(input, work, budget, now)?,
            window: Window::new(output),
        })
    }
    pub fn value(&self) -> Option<Retained<'_>> {
        let (end, is_encoding_problem) = self.framing.value()?;
        Some(Retained {
            fragment: self.window.provisional()?,
            end,
            is_encoding_problem,
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
        let Progress { written, status } = self.framing.poll(now, tail)?;
        self.window.advance(written).map_err(window_error)?;
        match status {
            Status::Complete => Ok(Status::Complete),
            Status::Yield => Ok(Status::Yield),
            Status::NeedOutput => Err(Error::InvalidState),
        }
    }
    pub fn finish(mut self, now: Tick) -> Result<(Retained<'o>, Completion<'w>), Error> {
        self.check_deadline(now)?;
        if self.framing.value().is_none() {
            return self.framing.outcome(Err(Error::InvalidState));
        }
        let fragment = match self.window.into_slice() {
            Ok(fragment) => fragment,
            Err(error) => return self.framing.outcome(Err(window_error(error))),
        };
        let completion = self.framing.finish(now)?;
        Ok((
            Retained {
                fragment,
                end: completion.end,
                is_encoding_problem: completion.is_encoding_problem,
            },
            completion,
        ))
    }
}
const _: () =
    assert!(std::mem::size_of::<Cursor<'_, '_, '_>>() + std::mem::size_of::<HeaderBudget>() <= 640);
#[cfg(test)]
pub use tests::probe as probe_allocations;
#[cfg(test)]
mod tests;

//! Original source-bound whole members share the bounded tree/list frame.
#[path = "composed/requested.rs"]
pub mod requested;
#[path = "composed/retained.rs"]
pub mod retained;
#[path = "composed/selected.rs"]
pub mod selected;
use super::super::super::super::super::Error as OriginalError;
use super::{Error, Serialized, View};
use crate::mime_traversal::bound::ordered::body_lists::response::collected::composed::framing::{
    self, Source,
};
pub use crate::mime_traversal::bound::ordered::body_lists::response::collected::composed::Mode;
use crate::{admission::work::Charge, nfc::HeaderBudget, ports::Tick};
pub use td_json::string::{Progress, Status};

/// Original complete collection custody; emitted bytes stay provisional.
/// ```compile_fail,E0277
/// fn required<T: Copy>() {} required::<td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::retained::collected::composed::Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn required<T: Clone>() {} required::<td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::retained::collected::composed::Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0308
/// use td_mta::{ports::Tick, mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::retained::collected::{View, composed::{Cursor, Mode}}};
/// fn passive(view: View<'_, '_, '_>) { let _ = Cursor::new(view, Mode::Structure, Tick(1)); }
/// ```
pub struct Cursor<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm> {
    source: Serialized<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>,
    frame: framing::Frame,
    failure: Option<Error>,
}
impl<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>
    Cursor<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>
{
    pub fn new(
        source: Serialized<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>,
        mode: Mode,
        now: Tick,
    ) -> Result<Self, Error> {
        Self::with_frame(source, framing::Frame::new(mode), now)
    }
    fn with_frame(
        mut source: Serialized<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>,
        frame: framing::Frame,
        now: Tick,
    ) -> Result<Self, Error> {
        source.check_deadline(now)?;
        Ok(Self {
            source,
            frame,
            failure: None,
        })
    }
    #[cfg(test)]
    pub(in super::super::super) fn costs(&self) -> [u64; 5] {
        let structure = &self
            .source
            .source
            .original
            .source
            .original
            .source
            .projected
            .structure;
        let left = structure.work.remaining();
        [
            structure.budget.source_bytes_remaining(),
            structure.budget.steps_remaining(),
            left.io_bytes,
            left.records,
            left.output_bytes,
        ]
    }
    fn outcome<T>(&mut self, result: Result<T, Error>) -> Result<T, Error> {
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self.source.check_deadline(now);
        self.outcome(result)
    }
    pub fn value(&self) -> Option<(Mode, View<'_, 'm, 'o>)> {
        if self.failure.is_some() || !self.frame.complete() {
            return None;
        }
        Some((self.frame.mode(), self.source.value()?))
    }
    pub fn poll(&mut self, now: Tick, output: &mut [u8]) -> Result<Progress, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if self.frame.complete() {
            return Ok(Progress {
                written: 0,
                status: Status::Complete,
            });
        }
        self.check_deadline(now)?;
        if output.is_empty() {
            return Ok(Progress {
                written: 0,
                status: Status::NeedOutput,
            });
        }
        let result = self
            .frame
            .step(&mut self.source, now, output)
            .map_err(Error::Original);
        let result = self
            .source
            .source
            .parent
            .check_deadline()
            .map_err(Error::Parent)
            .and(result);
        self.outcome(result)
    }
    pub fn finish(
        mut self,
        now: Tick,
    ) -> Result<Composed<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>, Error> {
        self.check_deadline(now)?;
        if !self.frame.complete() {
            return self.outcome(Err(Error::InvalidState));
        }
        Ok(Composed {
            source: self.source,
            mode: self.frame.mode(),
        })
    }
}
impl Source for Serialized<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_> {
    fn charge(
        &mut self,
        now: Tick,
        steps: u64,
        output: u64,
        credit: &mut crate::nfc::Credit,
    ) -> Result<(), OriginalError> {
        let structure = &mut self
            .source
            .original
            .source
            .original
            .source
            .projected
            .structure;
        structure
            .budget
            .charge(structure.work, now, 0, steps, credit)
            .map_err(OriginalError::Admission)?;
        structure
            .work
            .charge(
                now,
                Charge {
                    output_bytes: output,
                    ..Charge::default()
                },
            )
            .map_err(|error| OriginalError::Admission(crate::nfc::Error::Work(error)))
    }
    fn parts(&self) -> Result<&[crate::mime_traversal::Part], OriginalError> {
        self.source
            .original
            .source
            .original
            .source
            .projected
            .structure
            .parts()
    }
    fn node(&self, index: usize) -> Result<crate::mime_traversal::Part, OriginalError> {
        let part = self
            .parts()?
            .get(index)
            .copied()
            .ok_or(OriginalError::InvalidState)?;
        let original = self.source.value().ok_or(OriginalError::InvalidState)?;
        let fragment = original
            .original
            .original
            .fragments
            .get(index)
            .and_then(|cell| cell.value())
            .ok_or(OriginalError::InvalidState)?;
        let candidate = original
            .candidates
            .get(index)
            .ok_or(OriginalError::InvalidState)?;
        let member = self
            .cells
            .get(index)
            .and_then(|cell| cell.value())
            .ok_or(OriginalError::InvalidState)?;
        let ordinal = index
            .checked_add(1)
            .and_then(|index| u16::try_from(index).ok())
            .ok_or(OriginalError::InvalidState)?;
        if part.ordinal != ordinal
            || fragment.end.part != part
            || candidate.ordinal() != ordinal
            || member.ordinal != ordinal
        {
            return Err(OriginalError::InvalidState);
        }
        Ok(part)
    }
    fn fragment(&self, index: usize) -> Result<&[u8], OriginalError> {
        Ok(self
            .cells
            .get(index)
            .and_then(|cell| cell.value())
            .ok_or(OriginalError::InvalidState)?
            .members)
    }
    fn list(&self, list: framing::List) -> &[u16] {
        let lists = &self.source.original.source.original.source.projected.lists;
        match list {
            framing::List::Text => lists.text,
            framing::List::Html => lists.html,
            framing::List::Attachments => lists.attachments,
        }
    }
    fn has_attachment(&self) -> bool {
        self.source
            .original
            .source
            .original
            .source
            .projected
            .lists
            .has_attachment
    }
}
/// Original complete collection custody; emitted bytes stay provisional.
/// ```compile_fail,E0277
/// fn required<T: Copy>() {} required::<td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::retained::collected::composed::Composed<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn required<T: Clone>() {} required::<td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::retained::collected::composed::Composed<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
pub struct Composed<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm> {
    source: Serialized<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>,
    mode: Mode,
}
impl<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>
    Composed<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>
{
    pub fn value(&self) -> Option<(Mode, View<'_, 'm, 'o>)> {
        Some((self.mode, self.source.value()?))
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        self.source.check_deadline(now)
    }
    pub fn finish(
        mut self,
        now: Tick,
    ) -> Result<(Serialized<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, 's, 'm>, Mode), Error> {
        self.check_deadline(now)?;
        Ok((self.source, self.mode))
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
#[path = "composed/tests.rs"]
mod tests;

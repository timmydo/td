//! Private body metadata members; authenticated blob locators and publication follow.
use crate::mime_response::framing;
use crate::{admission::work::Charge, ports::Tick};
pub use td_json::string::{Progress, Status};
use {
    crate::mime_response::bound::Error, crate::mime_response::part_collection::Serialized,
    crate::mime_response::part_collection::View,
};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    Structure,
    Lists,
}
/// Only complete original collection ownership enters the bounded composition.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mta::mime_response::body_json::Cursor<'_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mta::mime_response::body_json::Cursor<'_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0308
/// use {td_mta::mime_response::part_collection::View, td_mta::mime_response::body_json::Cursor, td_mta::mime_response::body_json::Mode, td_mta::ports::Tick};
/// fn substitute(view: View<'_, '_, '_, '_>) { let _ = Cursor::new(view, Mode::Lists, Tick(1)); }
/// ```
pub struct Cursor<'a, 'w, 'n, 'c, 'o> {
    pub(in crate::mime_response) source: Serialized<'a, 'w, 'n, 'c, 'o>,
    frame: framing::Frame,
    pub(in crate::mime_response) failure: Option<Error>,
}
impl<'a, 'w, 'n, 'c, 'o> Cursor<'a, 'w, 'n, 'c, 'o> {
    pub fn new(
        mut source: Serialized<'a, 'w, 'n, 'c, 'o>,
        mode: Mode,
        now: Tick,
    ) -> Result<Self, Error> {
        source.check_deadline(now)?;
        Ok(Self {
            source,
            frame: framing::Frame::new(mode),
            failure: None,
        })
    }
    pub(in crate::mime_response) fn outcome<T>(
        &mut self,
        result: Result<T, Error>,
    ) -> Result<T, Error> {
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self.frame.check_deadline(&mut self.source, now);
        self.outcome(result)
    }
    pub fn value(&self) -> Option<(Mode, View<'_, '_, '_, 'o>)> {
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
        let result = self.frame.step(&mut self.source, now, output);
        self.outcome(result)
    }
    pub fn finish(mut self, now: Tick) -> Result<Composed<'a, 'w, 'n, 'c, 'o>, Error> {
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
impl framing::Source for Serialized<'_, '_, '_, '_, '_> {
    fn charge(
        &mut self,
        now: Tick,
        steps: u64,
        output: u64,
        credit: &mut crate::nfc::Credit,
    ) -> Result<(), Error> {
        let structure = &mut self.projected.structure;
        structure
            .budget
            .charge(structure.work, now, 0, steps, credit)
            .map_err(Error::Admission)?;
        structure
            .work
            .charge(
                now,
                Charge {
                    output_bytes: output,
                    ..Charge::default()
                },
            )
            .map_err(|error| Error::Admission(crate::nfc::Error::Work(error)))
    }
    fn node(&self, index: usize) -> Result<crate::mime_structure::Part, Error> {
        let part = self
            .projected
            .structure
            .parts()?
            .get(index)
            .copied()
            .ok_or(Error::InvalidState)?;
        let retained = self
            .cells
            .get(index)
            .and_then(|cell| cell.retained)
            .ok_or(Error::InvalidState)?;
        let ordinal = index
            .checked_add(1)
            .and_then(|i| u16::try_from(i).ok())
            .ok_or(Error::InvalidState)?;
        if part.ordinal != ordinal || retained.end.part != part {
            return Err(Error::InvalidState);
        }
        Ok(part)
    }
    fn list(&self, list: framing::List) -> &[u16] {
        match list {
            framing::List::Text => self.projected.lists.text,
            framing::List::Html => self.projected.lists.html,
            framing::List::Attachments => self.projected.lists.attachments,
        }
    }
    fn parts(&self) -> Result<&[crate::mime_structure::Part], Error> {
        self.projected.structure.parts()
    }
    fn fragment(&self, index: usize) -> Result<&[u8], Error> {
        Ok(self
            .cells
            .get(index)
            .and_then(|cell| cell.retained)
            .ok_or(Error::InvalidState)?
            .fragment)
    }
    fn has_attachment(&self) -> bool {
        self.projected.lists.has_attachment
    }
}
/// Complete emitted member composition, still provisional and not retained wire proof.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mta::mime_response::body_json::Composed<'_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mta::mime_response::body_json::Composed<'_, '_, '_, '_, '_>>();
/// ```
pub struct Composed<'a, 'w, 'n, 'c, 'o> {
    pub(in crate::mime_response) source: Serialized<'a, 'w, 'n, 'c, 'o>,
    mode: Mode,
}
impl<'w, 'n, 'c, 'o> Composed<'_, 'w, 'n, 'c, 'o> {
    pub fn value(&self) -> Option<(Mode, View<'_, '_, '_, 'o>)> {
        Some((self.mode, self.source.value()?))
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        self.source.check_deadline(now)
    }
    pub fn finish(
        self,
        now: Tick,
    ) -> Result<
        (
            (Mode, View<'w, 'n, 'c, 'o>),
            &'w mut crate::admission::work::Meter,
            &'w mut crate::nfc::HeaderBudget,
        ),
        Error,
    > {
        let (view, work, budget) = self.source.finish(now)?;
        Ok(((self.mode, view), work, budget))
    }
}
const _: () = assert!(
    std::mem::size_of::<Cursor<'_, '_, '_, '_, '_>>()
        + std::mem::size_of::<crate::nfc::HeaderBudget>()
        <= 1024
);
const _: () = assert!(std::mem::size_of::<Composed<'_, '_, '_, '_, '_>>() <= 256);

#[cfg(test)]
#[path = "body_json/tests.rs"]
pub(in crate::mime_response) mod tests;

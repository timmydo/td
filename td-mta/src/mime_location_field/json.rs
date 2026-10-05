//! One complete caller-authorized location field as a provisional JSON string.
pub use super::End;
use crate::{admission::work::Meter, nfc::HeaderBudget, ports::Tick};
pub use td_json::string::{Progress, Status};
pub type Error = td_json::string::Error<super::Error>;
struct Source<'a, 'w> {
    cursor: super::Cursor<'a, 'w>,
}
impl td_json::string::Source for Source<'_, '_> {
    type Context = Tick;
    type Error = super::Error;
    fn charge_output(&mut self, now: Tick, bytes: u64) -> Result<(), Self::Error> {
        self.cursor.charge_output(now, bytes)
    }
    fn poll(&mut self, now: Tick) -> Result<td_json::string::Scalar, Self::Error> {
        self.cursor.poll(now).map(|status| match status {
            super::Status::Yield => td_json::string::Scalar::Yield,
            super::Status::Scalar(value) => td_json::string::Scalar::Value(value),
            super::Status::Complete => td_json::string::Scalar::Complete,
        })
    }
}
/// Binds shared JSON framing to the complete authorized field and original owners.
/// Quotes and escaped/UTF-8 wire bytes are charged in addition to field scalars.
/// Output and metadata stay provisional through whole completion and fresh finish.
/// Field presence/null selection, retained output and publication stay external.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mta::mime_location_field::json::Cursor<'_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mta::mime_location_field::json::Cursor<'_, '_>>();
/// ```
pub struct Cursor<'a, 'w> {
    source: Source<'a, 'w>,
    frame: td_json::string::Frame<super::Error>,
    complete: bool,
}
impl<'a, 'w> Cursor<'a, 'w> {
    pub fn new(source: &'a [u8], work: &'w mut Meter, budget: &'w mut HeaderBudget) -> Self {
        Self {
            source: Source {
                cursor: super::Cursor::new(source, work, budget),
            },
            frame: td_json::string::Frame::new(),
            complete: false,
        }
    }
    pub fn end(&self) -> Option<End> {
        if self.complete {
            self.source.cursor.end()
        } else {
            None
        }
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        let result = self.frame.check_admission(&mut self.source, now);
        if result.is_err() {
            self.complete = false;
        }
        result
    }
    pub fn poll(&mut self, now: Tick, output: &mut [u8]) -> Result<Progress, Error> {
        let result = self.frame.poll(&mut self.source, now, output);
        self.complete = matches!(
            result,
            Ok(Progress {
                status: Status::Complete,
                ..
            })
        );
        result
    }
    pub fn finish(
        mut self,
        now: Tick,
    ) -> Result<(&'w mut Meter, &'w mut HeaderBudget, End), Error> {
        self.check_deadline(now)?;
        let end = self.end().ok_or(Error::InvalidState)?;
        let (work, budget, _) = self.source.cursor.finish(now).map_err(Error::Source)?;
        Ok((work, budget, end))
    }
}
const _: () =
    assert!(std::mem::size_of::<Cursor<'_, '_>>() + std::mem::size_of::<HeaderBudget>() <= 768);
#[cfg(test)]
mod tests;

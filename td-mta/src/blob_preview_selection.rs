//! Select one preview source while retaining the original native body-list owner.
use crate::{
    admission::work::Charge,
    blob_body_lists,
    blob_part::Error,
    mime_body_lists::{Media, Node},
    mime_response::bound,
    mime_structure,
    nfc::Credit,
    ports::Tick,
    preview_json::Format,
};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Choice {
    pub ordinal: u16,
    /// Expected classification; confirm the selected part's actual type before decoding.
    pub format: Format,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Yield,
    Complete,
}
#[derive(Clone, Copy, Eq, PartialEq)]
enum Phase {
    Check,
    Text,
    Html,
    Complete,
}
/// The choice is passive metadata; finish supplies fresh native admission.
pub struct Cursor<'p, 'v, 'c, 'b, 'o, 'n, 'w> {
    original: blob_body_lists::Retained<'p, 'v, 'c, 'b, 'o, 'n, 'w>,
    phase: Phase,
    position: usize,
    previous: u16,
    choice: Option<Choice>,
    credit: Credit,
    failure: Option<Error>,
}
pub struct Retained<'p, 'v, 'c, 'b, 'o, 'n, 'w> {
    pub original: blob_body_lists::Retained<'p, 'v, 'c, 'b, 'o, 'n, 'w>,
    pub choice: Option<Choice>,
}
impl<'p, 'v, 'c, 'b, 'o, 'n, 'w> Cursor<'p, 'v, 'c, 'b, 'o, 'n, 'w> {
    pub fn new(
        original: blob_body_lists::Retained<'p, 'v, 'c, 'b, 'o, 'n, 'w>,
        now: Tick,
    ) -> Result<Self, Error> {
        let mut cursor = Self {
            original,
            phase: Phase::Check,
            position: 0,
            previous: 0,
            choice: None,
            credit: Credit::new(),
            failure: None,
        };
        cursor.check_deadline(now)?;
        let n = cursor.original.structure.parts.len();
        let valid = n != 0 && n <= usize::from(u16::MAX) && cursor.original.nodes.len() == n;
        cursor.check_deadline(now)?;
        if !valid {
            return Err(bound::Error::InvalidState.into());
        }
        Ok(cursor)
    }
    fn charge(&mut self, now: Tick, records: u64) -> Result<(), Error> {
        let original = &mut self.original.structure;
        original
            .budget
            .charge(original.work, now, 0, records, &mut self.credit)
            .map_err(bound::Error::Admission)?;
        original
            .work
            .charge(
                now,
                Charge {
                    records,
                    ..Charge::default()
                },
            )
            .map_err(|e| bound::Error::Admission(crate::nfc::Error::Work(e)))?;
        Ok(())
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
        let result = (|| {
            self.original
                .structure
                .source
                .check_deadline()
                .map_err(Error::Policy)?;
            let result = self.charge(now, 0);
            self.original
                .structure
                .source
                .check_deadline()
                .map_err(Error::Policy)
                .and(result)
        })();
        self.outcome(result)
    }
    /// None means incomplete/failed; Some(None) means no textual body source.
    pub fn value(&self) -> Option<Option<Choice>> {
        (self.failure.is_none() && self.phase == Phase::Complete).then_some(self.choice)
    }
    pub fn poll(&mut self, now: Tick) -> Result<Status, Error> {
        self.check_deadline(now)?;
        let result = self.step(now);
        let result = self
            .original
            .structure
            .source
            .check_deadline()
            .map_err(Error::Policy)
            .and(result);
        self.outcome(result)
    }
    fn node(&self, ordinal: u16) -> Result<&Node, Error> {
        let index = usize::from(ordinal)
            .checked_sub(1)
            .ok_or(bound::Error::InvalidState)?;
        self.original
            .nodes
            .get(index)
            .ok_or(bound::Error::InvalidState.into())
    }
    fn step(&mut self, now: Tick) -> Result<Status, Error> {
        if self.phase == Phase::Complete {
            return Ok(Status::Complete);
        }
        self.charge(now, 1)?;
        if self.phase == Phase::Check {
            if let Some(part) = self.original.structure.parts.get(self.position) {
                let ordinal =
                    u16::try_from(self.position + 1).map_err(|_| bound::Error::InvalidState)?;
                let node = self.node(ordinal)?;
                if part.ordinal != ordinal
                    || part.parent != node.parent
                    || part.depth != node.depth
                    || (matches!(
                        node.class.media,
                        Media::Plain | Media::Html | Media::InlineMedia
                    ) && matches!(
                        part.media,
                        mime_structure::Media::Multipart | mime_structure::Media::MessageRfc822
                    ))
                {
                    return Err(bound::Error::InvalidState.into());
                }
                self.position += 1;
            } else {
                self.phase = Phase::Text;
                self.position = 0;
            }
            return Ok(Status::Yield);
        }
        let list = if self.phase == Phase::Text {
            self.original.lists.text
        } else {
            self.original.lists.html
        };
        if let Some(&ordinal) = list.get(self.position) {
            if ordinal <= self.previous {
                return Err(bound::Error::InvalidState.into());
            }
            let format = match self.node(ordinal)?.class.media {
                Media::Plain => Some(Format::Plain),
                Media::Html => Some(Format::Html),
                Media::InlineMedia => None,
                _ => return Err(bound::Error::InvalidState.into()),
            };
            if self.choice.is_none() {
                self.choice = format.map(|format| Choice { ordinal, format });
            }
            self.previous = ordinal;
            self.position += 1;
            return Ok(Status::Yield);
        }
        self.position = 0;
        self.previous = 0;
        if self.phase == Phase::Text {
            self.phase = Phase::Html;
            Ok(Status::Yield)
        } else {
            self.phase = Phase::Complete;
            Ok(Status::Complete)
        }
    }
    pub fn finish(mut self, now: Tick) -> Result<Retained<'p, 'v, 'c, 'b, 'o, 'n, 'w>, Error> {
        self.check_deadline(now)?;
        let choice = self.value().ok_or(bound::Error::InvalidState)?;
        self.original
            .structure
            .source
            .check_deadline()
            .map_err(Error::Policy)?;
        Ok(Retained {
            original: self.original,
            choice,
        })
    }
}
const _: () = assert!(std::mem::size_of::<Cursor<'_, '_, '_, '_, '_, '_, '_>>() <= 512);
#[cfg(test)]
#[path = "blob_preview_selection_tests.rs"]
mod tests;

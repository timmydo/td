//! Select text body-value parts while retaining the original native list owners.
use crate::{
    admission::work::Charge,
    blob_body_lists,
    blob_part::Error,
    body_value_options::Options,
    mime_part_headers,
    mime_response::{bound, part_json::End},
    mime_structure::Media,
    nfc::{Credit, HeaderBudget},
    ports::Tick,
};
/// Derived only from the original interpreted Content-Type, not body-list class.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    Other,
    Text,
    Html,
}
impl Kind {
    /// Metadata interpretation already admitted the original normalized label.
    pub fn from_headers(headers: &mime_part_headers::View<'_>) -> Self {
        let value = headers.content_type;
        if value.eq_ignore_ascii_case(b"text/html") {
            Self::Html
        } else if value.len() > 5
            && value
                .get(..5)
                .is_some_and(|p| p.eq_ignore_ascii_case(b"text/"))
        {
            Self::Text
        } else {
            Self::Other
        }
    }
}
/// Original metadata facts; correspondence does not authenticate fabricated facts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Fact {
    pub end: End,
    pub kind: Kind,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Yield,
    Complete,
}
#[derive(Clone, Copy)]
enum Phase {
    Check,
    Text,
    Html,
    Emit,
    Complete,
}
pub struct Retained<'p, 'v, 'c, 'b, 'o, 'n, 'w, 'f, 's> {
    pub original: blob_body_lists::Retained<'p, 'v, 'c, 'b, 'o, 'n, 'w>,
    pub facts: &'f [Fact],
    pub ordinals: &'s mut [u16],
    pub membership: &'s mut [u8],
    pub options: Options,
}
pub struct Cursor<'p, 'v, 'c, 'b, 'o, 'n, 'w, 'f, 's> {
    retained: Retained<'p, 'v, 'c, 'b, 'o, 'n, 'w, 'f, 's>,
    phase: Phase,
    position: usize,
    used: usize,
    credit: Credit,
    failure: Option<Error>,
}
impl<'p, 'v, 'c, 'b, 'o, 'n, 'w, 'f, 's> Cursor<'p, 'v, 'c, 'b, 'o, 'n, 'w, 'f, 's> {
    pub fn new(
        original: blob_body_lists::Retained<'p, 'v, 'c, 'b, 'o, 'n, 'w>,
        facts: &'f [Fact],
        ordinals: &'s mut [u16],
        membership: &'s mut [u8],
        options: Options,
        now: Tick,
    ) -> Result<Self, Error> {
        let mut cursor = Self {
            retained: Retained {
                original,
                facts,
                ordinals,
                membership,
                options,
            },
            phase: Phase::Check,
            position: 0,
            used: 0,
            credit: Credit::new(),
            failure: None,
        };
        cursor.check_deadline(now)?;
        let n = cursor.retained.original.structure.parts.len();
        let valid = n != 0
            && n <= usize::from(u16::MAX)
            && cursor.retained.facts.len() == n
            && cursor.retained.original.nodes.len() == n;
        cursor.check_deadline(now)?;
        if !valid {
            return Err(bound::Error::InvalidState.into());
        }
        if cursor.retained.membership.len() < n {
            return Err(bound::Error::ResponseCapacity.into());
        }
        Ok(cursor)
    }
    fn outcome<T>(&mut self, result: Result<T, Error>) -> Result<T, Error> {
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    fn charge(&mut self, now: Tick, steps: u64, output: u64) -> Result<(), Error> {
        let structure = &mut self.retained.original.structure;
        structure
            .budget
            .charge(structure.work, now, 0, steps, &mut self.credit)
            .map_err(bound::Error::Admission)?;
        structure
            .work
            .charge(
                now,
                Charge {
                    records: steps,
                    output_bytes: output,
                    ..Charge::default()
                },
            )
            .map_err(|e| bound::Error::Admission(crate::nfc::Error::Work(e)))?;
        Ok(())
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = (|| {
            self.retained
                .original
                .structure
                .source
                .check_deadline()
                .map_err(Error::Policy)?;
            let result = self.charge(now, 0, 0);
            self.retained
                .original
                .structure
                .source
                .check_deadline()
                .map_err(Error::Policy)
                .and(result)
        })();
        self.outcome(result)
    }
    /// Passive provisional ordinals, available only after the whole selection.
    pub fn value(&self) -> Option<&[u16]> {
        if self.failure.is_none() && matches!(self.phase, Phase::Complete) {
            self.retained.ordinals.get(..self.used)
        } else {
            None
        }
    }
    pub fn poll(&mut self, now: Tick) -> Result<Status, Error> {
        self.check_deadline(now)?;
        let result = self.step(now);
        let result = self
            .retained
            .original
            .structure
            .source
            .check_deadline()
            .map_err(Error::Policy)
            .and(result);
        self.outcome(result)
    }
    fn advance(&mut self, phase: Phase) {
        self.phase = phase;
        self.position = 0;
    }
    fn step(&mut self, now: Tick) -> Result<Status, Error> {
        if matches!(self.phase, Phase::Complete) {
            return Ok(Status::Complete);
        }
        self.charge(now, 1, 0)?;
        match self.phase {
            Phase::Check => {
                let part = self
                    .retained
                    .original
                    .structure
                    .parts
                    .get(self.position)
                    .ok_or(bound::Error::InvalidState)?;
                let fact = self
                    .retained
                    .facts
                    .get(self.position)
                    .ok_or(bound::Error::InvalidState)?;
                if usize::from(part.ordinal) != self.position + 1
                    || fact.end.part != *part
                    || self.retained.original.nodes.get(self.position) != Some(&fact.end.node)
                    || (matches!(part.media, Media::Multipart | Media::MessageRfc822)
                        && fact.kind != Kind::Other)
                {
                    return Err(bound::Error::InvalidState.into());
                }
                *self
                    .retained
                    .membership
                    .get_mut(self.position)
                    .ok_or(bound::Error::InvalidState)? =
                    u8::from(self.retained.options.fetch_all && fact.kind != Kind::Other);
                self.position += 1;
                if self.position == self.retained.facts.len() {
                    self.advance(Phase::Text);
                }
            }
            Phase::Text | Phase::Html => {
                let text = matches!(self.phase, Phase::Text);
                let enabled = if text {
                    self.retained.options.fetch_text
                } else {
                    self.retained.options.fetch_html
                };
                let list = if text {
                    self.retained.original.lists.text
                } else {
                    self.retained.original.lists.html
                };
                if !enabled || self.position == list.len() {
                    self.advance(if text { Phase::Html } else { Phase::Emit });
                } else {
                    let ordinal = *list.get(self.position).ok_or(bound::Error::InvalidState)?;
                    let index = usize::from(ordinal)
                        .checked_sub(1)
                        .ok_or(bound::Error::InvalidState)?;
                    let fact = self
                        .retained
                        .facts
                        .get(index)
                        .ok_or(bound::Error::InvalidState)?;
                    if fact.kind != Kind::Other {
                        *self
                            .retained
                            .membership
                            .get_mut(index)
                            .ok_or(bound::Error::InvalidState)? = 1;
                    }
                    self.position += 1;
                }
            }
            Phase::Emit => {
                if *self
                    .retained
                    .membership
                    .get(self.position)
                    .ok_or(bound::Error::InvalidState)?
                    != 0
                {
                    let ordinal = self
                        .retained
                        .facts
                        .get(self.position)
                        .ok_or(bound::Error::InvalidState)?
                        .end
                        .part
                        .ordinal;
                    if self.used == self.retained.ordinals.len() {
                        return Err(bound::Error::ResponseCapacity.into());
                    }
                    self.charge(now, 0, 2)?;
                    *self
                        .retained
                        .ordinals
                        .get_mut(self.used)
                        .ok_or(bound::Error::InvalidState)? = ordinal;
                    self.used += 1;
                }
                self.position += 1;
                if self.position == self.retained.facts.len() {
                    self.advance(Phase::Complete);
                    return Ok(Status::Complete);
                }
            }
            Phase::Complete => return Ok(Status::Complete),
        }
        Ok(Status::Yield)
    }
    pub fn finish(
        mut self,
        now: Tick,
    ) -> Result<Retained<'p, 'v, 'c, 'b, 'o, 'n, 'w, 'f, 's>, Error> {
        self.check_deadline(now)?;
        let complete = self.value().is_some();
        self.retained
            .original
            .structure
            .source
            .check_deadline()
            .map_err(Error::Policy)?;
        if !complete {
            return Err(bound::Error::InvalidState.into());
        }
        let (ordinals, _) = self
            .retained
            .ordinals
            .split_at_mut_checked(self.used)
            .ok_or(bound::Error::InvalidState)?;
        self.retained.ordinals = ordinals;
        Ok(self.retained)
    }
}
const _: () = assert!(
    std::mem::size_of::<Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_>>()
        + std::mem::size_of::<HeaderBudget>()
        <= 1024
);

#[cfg(test)]
#[path = "blob_value_parts_tests.rs"]
pub(crate) mod tests;

//! Direct-leaf locator candidates beside the original native traversal owner.
pub use crate::mime_response::body_window::Status;
use crate::{
    blob_part::Error,
    blob_structure,
    mime_response::{
        bound,
        locators::{self, Candidate, Parent},
    },
    nfc::{Credit, HeaderBudget},
    ports::{BlobReader, Tick},
};
/// Candidate bytes are passive; callers still authorize every published locator.
pub struct Cursor<'p, 'v, 'c, 'b, 'o, 'w, 'l> {
    structure: blob_structure::Retained<'p, 'v, 'c, 'b, 'o, 'w>,
    candidates: &'l mut [Candidate],
    next: usize,
    credit: Credit,
    failure: Option<Error>,
}
pub struct Retained<'p, 'v, 'c, 'b, 'o, 'w, 'l> {
    pub structure: blob_structure::Retained<'p, 'v, 'c, 'b, 'o, 'w>,
    pub candidates: &'l [Candidate],
}
impl<'p, 'v, 'c, 'b, 'o, 'w, 'l> Cursor<'p, 'v, 'c, 'b, 'o, 'w, 'l> {
    pub fn new(
        structure: blob_structure::Retained<'p, 'v, 'c, 'b, 'o, 'w>,
        candidates: &'l mut [Candidate],
        now: Tick,
    ) -> Result<Self, Error> {
        let mut cursor = Self {
            structure,
            candidates,
            next: 0,
            credit: Credit::new(),
            failure: None,
        };
        cursor.check_deadline(now)?;
        let result: Result<(), Error> = if cursor.candidates.len() < cursor.structure.parts.len() {
            Err(bound::Error::ResponseCapacity.into())
        } else if cursor.candidates.len() != cursor.structure.parts.len()
            || cursor.structure.parts.is_empty()
        {
            Err(bound::Error::InvalidState.into())
        } else {
            Ok(())
        };
        cursor
            .structure
            .source
            .check_deadline()
            .map_err(Error::Policy)?;
        result?;
        Ok(cursor)
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
            self.structure
                .source
                .check_deadline()
                .map_err(Error::Policy)?;
            let result = self
                .structure
                .budget
                .charge(self.structure.work, now, 0, 0, &mut self.credit)
                .map_err(bound::Error::Admission)
                .map_err(Error::from);
            self.structure
                .source
                .check_deadline()
                .map_err(Error::Policy)
                .and(result)
        })();
        self.outcome(result)
    }
    /// No clock sampling or authority is supplied by passive inspection.
    pub fn value(&self) -> Option<&[Candidate]> {
        (self.failure.is_none() && self.next == self.candidates.len()).then_some(self.candidates)
    }
    pub fn poll(&mut self, now: Tick) -> Result<Status, Error> {
        self.check_deadline(now)?;
        let result = self.step(now);
        let result = self
            .structure
            .source
            .check_deadline()
            .map_err(Error::Policy)
            .and(result);
        self.outcome(result)
    }
    fn step(&mut self, now: Tick) -> Result<Status, Error> {
        if self.next == self.candidates.len() {
            return Ok(Status::Complete);
        }
        let part = self
            .structure
            .parts
            .get(self.next)
            .copied()
            .ok_or(bound::Error::InvalidState)?;
        let ordinal = self
            .next
            .checked_add(1)
            .and_then(|n| u16::try_from(n).ok())
            .ok_or(bound::Error::InvalidState)?;
        let candidate = locators::candidate(
            part,
            ordinal,
            Parent {
                id: self.structure.source.id(),
                base: 0,
                length: self.structure.source.len(),
            },
            self.structure.work,
            self.structure.budget,
            &mut self.credit,
            now,
        )?;
        let root = self
            .structure
            .parts
            .first()
            .ok_or(bound::Error::InvalidState)?;
        if part.entity_start < root.entity_start
            || part.body_start < part.entity_start
            || part.entity_end > root.entity_end
            || root.entity_end > self.structure.source.len()
        {
            return Err(bound::Error::InvalidRange.into());
        }
        *self
            .candidates
            .get_mut(self.next)
            .ok_or(bound::Error::InvalidState)? = candidate;
        self.next = self.next.checked_add(1).ok_or(bound::Error::InvalidState)?;
        Ok(if self.next == self.candidates.len() {
            Status::Complete
        } else {
            Status::Yield
        })
    }
    pub fn finish(mut self, now: Tick) -> Result<Retained<'p, 'v, 'c, 'b, 'o, 'w, 'l>, Error> {
        self.check_deadline(now)?;
        let complete = self.value().is_some();
        self.structure
            .source
            .check_deadline()
            .map_err(Error::Policy)?;
        if !complete {
            return Err(bound::Error::InvalidState.into());
        }
        Ok(Retained {
            structure: self.structure,
            candidates: self.candidates,
        })
    }
}
const _: () = assert!(
    std::mem::size_of::<Cursor<'_, '_, '_, '_, '_, '_, '_>>()
        + std::mem::size_of::<HeaderBudget>()
        + std::mem::size_of::<Candidate>()
        <= 1024
);

#[cfg(test)]
#[path = "blob_part_locators_tests.rs"]
pub(crate) mod tests;

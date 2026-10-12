//! Compose requested body properties from original native lists and part members.
pub use crate::mime_response::subparts_json::Selection;
use crate::{
    admission::work::Charge,
    blob_body_lists,
    blob_part::Error,
    mime_response::{
        bound,
        framing::{self, Source},
        part_json::End,
    },
    mime_structure::Part,
    nfc::{Credit, HeaderBudget},
    ports::Tick,
};
pub use td_json::string::{Progress, Status};
/// Original generated members after applying the requested per-part selection.
#[derive(Clone, Copy)]
pub struct Fragment<'m> {
    pub end: End,
    pub members: &'m [u8],
}
pub struct Retained<'p, 'v, 'c, 'b, 'o, 'n, 'w, 'f, 'm> {
    pub original: blob_body_lists::Retained<'p, 'v, 'c, 'b, 'o, 'n, 'w>,
    pub fragments: &'f [Fragment<'m>],
    pub selection: Selection,
}
pub struct Cursor<'p, 'v, 'c, 'b, 'o, 'n, 'w, 'f, 'm> {
    retained: Retained<'p, 'v, 'c, 'b, 'o, 'n, 'w, 'f, 'm>,
    frame: framing::Frame,
    checked: usize,
    failure: Option<Error>,
}
impl<'p, 'v, 'c, 'b, 'o, 'n, 'w, 'f, 'm> Cursor<'p, 'v, 'c, 'b, 'o, 'n, 'w, 'f, 'm> {
    pub fn new(
        original: blob_body_lists::Retained<'p, 'v, 'c, 'b, 'o, 'n, 'w>,
        fragments: &'f [Fragment<'m>],
        selection: Selection,
        now: Tick,
    ) -> Result<Self, Error> {
        original
            .structure
            .source
            .check_deadline()
            .map_err(Error::Policy)?;
        let mut retained = Retained {
            original,
            fragments,
            selection,
        };
        let frame = framing::Frame::requested(
            selection.properties.body_structure,
            selection.properties.bits(),
        )
        .with_sub_parts(selection.sub_parts);
        let result = (|| {
            retained.charge(now, 0, 0, &mut Credit::new())?;
            let count = retained.original.structure.parts.len();
            if count == 0 || fragments.len() != count || retained.original.nodes.len() != count {
                return Err(bound::Error::InvalidState);
            }
            Ok(())
        })();
        retained
            .original
            .structure
            .source
            .check_deadline()
            .map_err(Error::Policy)?;
        result?;
        Ok(Self {
            retained,
            frame,
            checked: 0,
            failure: None,
        })
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
            self.retained
                .original
                .structure
                .source
                .check_deadline()
                .map_err(Error::Policy)?;
            let result = self
                .frame
                .check_deadline(&mut self.retained, now)
                .map_err(Error::from);
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
    pub fn value(&self) -> Option<Selection> {
        (self.failure.is_none()
            && self.checked == self.retained.fragments.len()
            && self.frame.complete())
        .then_some(self.retained.selection)
    }
    pub fn poll(&mut self, now: Tick, output: &mut [u8]) -> Result<Progress, Error> {
        self.check_deadline(now)?;
        let result = self.step(now, output);
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
    fn step(&mut self, now: Tick, output: &mut [u8]) -> Result<Progress, Error> {
        if self.value().is_some() {
            return Ok(Progress {
                written: 0,
                status: Status::Complete,
            });
        }
        if output.is_empty() {
            return Ok(Progress {
                written: 0,
                status: Status::NeedOutput,
            });
        }
        if self.checked < self.retained.fragments.len() {
            self.retained
                .original
                .structure
                .work
                .charge(
                    now,
                    Charge {
                        records: 1,
                        ..Charge::default()
                    },
                )
                .map_err(|error| bound::Error::Admission(crate::nfc::Error::Work(error)))?;
            self.retained.node(self.checked)?;
            self.checked = self
                .checked
                .checked_add(1)
                .ok_or(bound::Error::InvalidState)?;
            return Ok(Progress {
                written: 0,
                status: if self.value().is_some() {
                    Status::Complete
                } else {
                    Status::Yield
                },
            });
        }
        self.frame
            .step(&mut self.retained, now, output)
            .map_err(Error::from)
    }
    pub fn finish(
        mut self,
        now: Tick,
    ) -> Result<Retained<'p, 'v, 'c, 'b, 'o, 'n, 'w, 'f, 'm>, Error> {
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
        Ok(self.retained)
    }
}
impl Source for Retained<'_, '_, '_, '_, '_, '_, '_, '_, '_> {
    fn charge(
        &mut self,
        now: Tick,
        steps: u64,
        output: u64,
        credit: &mut Credit,
    ) -> Result<(), bound::Error> {
        let structure = &mut self.original.structure;
        structure
            .budget
            .charge(structure.work, now, 0, steps, credit)
            .map_err(bound::Error::Admission)?;
        structure
            .work
            .charge(
                now,
                Charge {
                    output_bytes: output,
                    ..Charge::default()
                },
            )
            .map_err(|error| bound::Error::Admission(crate::nfc::Error::Work(error)))
    }
    fn parts(&self) -> Result<&[Part], bound::Error> {
        Ok(self.original.structure.parts)
    }
    fn node(&self, index: usize) -> Result<Part, bound::Error> {
        let part = *self
            .original
            .structure
            .parts
            .get(index)
            .ok_or(bound::Error::InvalidState)?;
        let fragment = self
            .fragments
            .get(index)
            .ok_or(bound::Error::InvalidState)?;
        let ordinal = index
            .checked_add(1)
            .and_then(|n| u16::try_from(n).ok())
            .ok_or(bound::Error::InvalidState)?;
        if part.ordinal != ordinal
            || fragment.end.part != part
            || self.original.nodes.get(index) != Some(&fragment.end.node)
        {
            return Err(bound::Error::InvalidState);
        }
        Ok(part)
    }
    fn fragment(&self, index: usize) -> Result<&[u8], bound::Error> {
        Ok(self
            .fragments
            .get(index)
            .ok_or(bound::Error::InvalidState)?
            .members)
    }
    fn list(&self, list: framing::List) -> &[u16] {
        match list {
            framing::List::Text => self.original.lists.text,
            framing::List::Html => self.original.lists.html,
            framing::List::Attachments => self.original.lists.attachments,
        }
    }
    fn has_attachment(&self) -> bool {
        self.original.lists.has_attachment
    }
}
const _: () = assert!(
    std::mem::size_of::<Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_>>()
        + std::mem::size_of::<HeaderBudget>()
        + std::mem::size_of::<Fragment<'_>>()
        + 64
        <= 1024
);
const _: () = assert!(std::mem::size_of::<Fragment<'_>>() <= 128);

#[cfg(test)]
#[path = "blob_body_json_tests.rs"]
pub(crate) mod tests;

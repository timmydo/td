//! Select body lists from native traversal and its caller-retained classifications.
use crate::{
    admission::work::{Charge, Meter},
    blob_part::Error,
    blob_structure,
    mime_body_lists::{self, Backing, Node, View},
    mime_response::bound,
    mime_structure::{Limits, Part},
    nfc::{Credit, HeaderBudget},
    ports::Tick,
    root_headers,
    store_fs::PinnedBlob,
};
pub use mime_body_lists::Status;
struct Binding<'w> {
    work: &'w mut Meter,
    budget: &'w mut HeaderBudget,
    output: Backing<'w>,
    checked: usize,
}
/// Classifications are passive evidence and must come from these original parts.
pub struct Cursor<'p, 'v, 'c, 'b, 'o, 'n, 'w> {
    source: &'p mut PinnedBlob<'v, 'c>,
    parts: &'o [Part],
    headers: &'o [Option<root_headers::View<'b>>],
    remaining: &'b mut [u8],
    header_bytes: u64,
    nodes: &'n [Node],
    limits: Limits,
    binding: Option<Binding<'w>>,
    child: Option<mime_body_lists::Cursor<'n, 'w>>,
    failure: Option<Error>,
}
pub struct Retained<'p, 'v, 'c, 'b, 'o, 'n, 'w> {
    pub structure: blob_structure::Retained<'p, 'v, 'c, 'b, 'o, 'w>,
    pub nodes: &'n [Node],
    pub lists: View<'w>,
}
impl<'p, 'v, 'c, 'b, 'o, 'n, 'w> Cursor<'p, 'v, 'c, 'b, 'o, 'n, 'w> {
    pub fn new(
        structure: blob_structure::Retained<'p, 'v, 'c, 'b, 'o, 'w>,
        nodes: &'n [Node],
        limits: Limits,
        output: Backing<'w>,
        now: Tick,
    ) -> Result<Self, Error> {
        structure.source.check_deadline().map_err(Error::Policy)?;
        let result = (|| -> Result<(), Error> {
            structure
                .budget
                .charge(structure.work, now, 0, 0, &mut Credit::new())
                .map_err(bound::Error::Admission)?;
            if nodes.len() != structure.parts.len() || nodes.is_empty() {
                return Err(bound::Error::InvalidState.into());
            }
            Ok(())
        })();
        structure.source.check_deadline().map_err(Error::Policy)?;
        result?;
        Ok(Self {
            source: structure.source,
            parts: structure.parts,
            headers: structure.headers,
            remaining: structure.remaining,
            header_bytes: structure.header_bytes,
            nodes,
            limits,
            binding: Some(Binding {
                work: structure.work,
                budget: structure.budget,
                output,
                checked: 0,
            }),
            child: None,
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
            self.source.check_deadline().map_err(Error::Policy)?;
            let result = if let Some(binding) = self.binding.as_mut() {
                binding
                    .budget
                    .charge(binding.work, now, 0, 0, &mut Credit::new())
                    .map_err(bound::Error::Admission)
                    .map_err(Error::from)
            } else if let Some(child) = self.child.as_mut() {
                child
                    .check_deadline(now)
                    .map_err(bound::Error::BodyLists)
                    .map_err(Error::from)
            } else {
                Err(bound::Error::InvalidState.into())
            };
            self.source
                .check_deadline()
                .map_err(Error::Policy)
                .and(result)
        })();
        self.outcome(result)
    }
    /// Passive completion never samples a clock or authorizes a response.
    pub fn value(&self) -> Option<View<'_>> {
        if self.failure.is_some() {
            return None;
        }
        self.child.as_ref()?.value()
    }

    pub fn poll(&mut self, now: Tick) -> Result<Status, Error> {
        self.check_deadline(now)?;
        let result = self.step(now);
        let result = self
            .source
            .check_deadline()
            .map_err(Error::Policy)
            .and(result);
        self.outcome(result)
    }
    fn step(&mut self, now: Tick) -> Result<Status, Error> {
        if let Some(child) = self.child.as_mut() {
            return child
                .poll(now)
                .map_err(bound::Error::BodyLists)
                .map_err(Error::from);
        }
        let binding = self.binding.as_mut().ok_or(bound::Error::InvalidState)?;
        if binding.checked < self.nodes.len() {
            binding
                .work
                .charge(
                    now,
                    Charge {
                        records: 1,
                        ..Charge::default()
                    },
                )
                .map_err(|error| bound::Error::Admission(crate::nfc::Error::Work(error)))?;
            let node = self
                .nodes
                .get(binding.checked)
                .ok_or(bound::Error::InvalidState)?;
            let part = self
                .parts
                .get(binding.checked)
                .ok_or(bound::Error::InvalidState)?;
            let ordinal = binding
                .checked
                .checked_add(1)
                .and_then(|n| u16::try_from(n).ok())
                .ok_or(bound::Error::InvalidState)?;
            if part.ordinal != ordinal || part.parent != node.parent || part.depth != node.depth {
                return Err(bound::Error::InvalidState.into());
            }
            binding.checked = binding
                .checked
                .checked_add(1)
                .ok_or(bound::Error::InvalidState)?;
            return Ok(Status::Yield);
        }
        let binding = self.binding.take().ok_or(bound::Error::InvalidState)?;
        self.child = Some(
            mime_body_lists::Cursor::new(
                self.nodes,
                self.limits,
                binding.output,
                binding.work,
                binding.budget,
            )
            .map_err(bound::Error::BodyLists)?,
        );
        Ok(Status::Yield)
    }
    pub fn finish(mut self, now: Tick) -> Result<Retained<'p, 'v, 'c, 'b, 'o, 'n, 'w>, Error> {
        self.check_deadline(now)?;
        let result = match self.child.take() {
            Some(child) => child
                .finish(now)
                .map_err(bound::Error::BodyLists)
                .map_err(Error::from),
            None => Err(bound::Error::InvalidState.into()),
        };
        self.source.check_deadline().map_err(Error::Policy)?;
        let (lists, work, budget) = result?;
        Ok(Retained {
            structure: blob_structure::Retained {
                source: self.source,
                work,
                budget,
                parts: self.parts,
                headers: self.headers,
                remaining: self.remaining,
                header_bytes: self.header_bytes,
            },
            nodes: self.nodes,
            lists,
        })
    }
}
const _: () = assert!(
    std::mem::size_of::<Cursor<'_, '_, '_, '_, '_, '_, '_>>() + std::mem::size_of::<HeaderBudget>()
        <= 3 * 1024
);

const _: () = assert!(
    std::mem::size_of::<Cursor<'_, '_, '_, '_, '_, '_, '_>>()
        <= std::mem::size_of::<mime_body_lists::Cursor<'_, '_>>() + 256
);

#[cfg(test)]
#[path = "blob_body_lists_tests.rs"]
mod tests;

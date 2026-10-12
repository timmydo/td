//! Stream original native part metadata through the shared member encoder.
use crate::{
    blob_part::{self, Error},
    blob_structure,
    mime_response::part_json::{self, Core, End},
    mime_structure::Part,
    nfc::{HeaderBudget, Scratch},
    ports::Tick,
    root_headers,
    store_fs::PinnedBlob,
};
pub use part_json::{Progress, Status};
/// Braces, blob locators, requested-property filtering and publication are external.
pub struct Cursor<'p, 'v, 'c, 'b, 'o, 'w> {
    core: Core<'w>,
    source: &'p mut PinnedBlob<'v, 'c>,
    parts: &'o [Part],
    headers: &'o [Option<root_headers::View<'b>>],
    remaining: &'b mut [u8],
    header_bytes: u64,
    failure: Option<Error>,
}
pub struct Retained<'p, 'v, 'c, 'b, 'o, 'w> {
    pub structure: blob_structure::Retained<'p, 'v, 'c, 'b, 'o, 'w>,
    pub end: End,
    pub scratch: &'w mut Scratch,
}
impl<'p, 'v, 'c, 'b, 'o, 'w> Cursor<'p, 'v, 'c, 'b, 'o, 'w> {
    pub fn new(
        retained: blob_part::Retained<'p, 'v, 'c, 'b, 'o, 'w>,
        now: Tick,
    ) -> Result<Self, Error> {
        let structure = retained.structure;
        structure.source.check_deadline().map_err(Error::Policy)?;
        let mut core = Core::new(
            retained.metadata.metadata,
            End {
                part: retained.metadata.part,
                node: retained.metadata.node,
            },
            structure.work,
            structure.budget,
            retained.scratch,
        );
        let result = core.check_deadline(now).map_err(Error::from);
        structure.source.check_deadline().map_err(Error::Policy)?;
        result?;
        Ok(Self {
            core,
            source: structure.source,
            parts: structure.parts,
            headers: structure.headers,
            remaining: structure.remaining,
            header_bytes: structure.header_bytes,
            failure: None,
        })
    }
    fn outcome<T>(&mut self, result: Result<T, Error>) -> Result<T, Error> {
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    pub fn value(&self) -> Option<End> {
        if self.failure.is_some() {
            None
        } else {
            self.core.value()
        }
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = (|| {
            self.source.check_deadline().map_err(Error::Policy)?;
            let result = self.core.check_deadline(now).map_err(Error::from);
            self.source
                .check_deadline()
                .map_err(Error::Policy)
                .and(result)
        })();
        self.outcome(result)
    }
    /// Emitted bytes are provisional until whole response publication is admitted.
    pub fn poll(&mut self, now: Tick, output: &mut [u8]) -> Result<Progress, Error> {
        self.check_deadline(now)?;
        let result = self.core.poll(now, output).map_err(Error::from);
        let result = self
            .source
            .check_deadline()
            .map_err(Error::Policy)
            .and(result);
        self.outcome(result)
    }
    pub fn finish(mut self, now: Tick) -> Result<Retained<'p, 'v, 'c, 'b, 'o, 'w>, Error> {
        self.check_deadline(now)?;
        let result = self.core.finish(now).map_err(Error::from);
        self.source.check_deadline().map_err(Error::Policy)?;
        let (part, work, budget, scratch) = result?;
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
            end: part,
            scratch,
        })
    }
}
const _: () = assert!(
    std::mem::size_of::<Cursor<'_, '_, '_, '_, '_, '_>>()
        <= std::mem::size_of::<part_json::Cursor<'_>>() + 128
);
const _: () = assert!(
    std::mem::size_of::<Cursor<'_, '_, '_, '_, '_, '_>>() + std::mem::size_of::<HeaderBudget>()
        <= 2 * 1024
);

#[cfg(test)]
#[path = "blob_part_json_tests.rs"]
pub(crate) mod tests;

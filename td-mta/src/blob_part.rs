//! Interpret one retained entity header while preserving native traversal custody.
use crate::{
    blob_structure,
    mime_part_headers::{self, label_json},
    mime_response::bound::{self, ClassifiedView, PartView},
    mime_structure::Part,
    nfc::{HeaderBudget, Scratch},
    ports::{Error as PolicyError, Tick},
    root_headers,
    store_fs::PinnedBlob,
};
pub use mime_part_headers::Status;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Interpretation(bound::Error),
    Policy(PolicyError),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Interpretation(error) => write!(f, "native part metadata: {error}"),
            Self::Policy(error) => write!(f, "native part snapshot: {error}"),
        }
    }
}
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Interpretation(error) => Some(error),
            Self::Policy(error) => Some(error),
        }
    }
}
impl From<bound::Error> for Error {
    fn from(error: bound::Error) -> Self {
        Self::Interpretation(error)
    }
}
impl From<label_json::Error> for Error {
    fn from(error: label_json::Error) -> Self {
        Self::Interpretation(bound::Error::Metadata(error))
    }
}
/// Complete passive metadata beside the same source, work and header owners.
pub struct Retained<'p, 'v, 'c, 'b, 'o, 'w> {
    pub structure: blob_structure::Retained<'p, 'v, 'c, 'b, 'o, 'w>,
    pub metadata: ClassifiedView<'w>,
    pub scratch: &'w mut Scratch,
}
/// This cursor reads only retained headers, never the native body or another pin.
/// Each fresh poll fences metadata progress with the original native deadline.
pub struct Cursor<'p, 'v, 'c, 'b, 'o, 'w> {
    source: &'p mut PinnedBlob<'v, 'c>,
    parts: &'o [Part],
    headers: &'o [Option<root_headers::View<'b>>],
    remaining: &'b mut [u8],
    header_bytes: u64,
    part: Part,
    child: label_json::Cursor<'b, 'w>,
    failure: Option<Error>,
}
impl<'p, 'v, 'c, 'b, 'o, 'w> Cursor<'p, 'v, 'c, 'b, 'o, 'w> {
    pub fn new(
        structure: blob_structure::Retained<'p, 'v, 'c, 'b, 'o, 'w>,
        ordinal: u16,
        backing: label_json::Backing<'w>,
        scratch: &'w mut Scratch,
        now: Tick,
    ) -> Result<Self, Error> {
        structure.source.check_deadline().map_err(Error::Policy)?;
        let prepared = (|| -> Result<_, Error> {
            structure
                .budget
                .charge(structure.work, now, 0, 0, &mut crate::nfc::Credit::new())
                .map_err(bound::Error::Admission)?;
            let index = usize::from(ordinal)
                .checked_sub(1)
                .ok_or(bound::Error::PartOrdinal)?;
            let part = *structure
                .parts
                .get(index)
                .ok_or(bound::Error::PartOrdinal)?;
            let view = structure
                .headers
                .get(index)
                .and_then(Option::as_ref)
                .ok_or(bound::Error::InvalidState)?;
            if part.ordinal != ordinal
                || part.entity_start != view.base
                || part.body_start != view.end.body_start
            {
                return Err(bound::Error::InvalidState.into());
            }
            let child = label_json::Cursor::new(
                mime_part_headers::Entity {
                    source: view.bytes,
                    base: view.base,
                    source_end: view.source_end,
                    header_limit: view.end.header_bytes,
                    context: part.context(),
                },
                backing,
                structure.work,
                structure.budget,
                scratch,
            )?;
            Ok((part, child))
        })();
        structure.source.check_deadline().map_err(Error::Policy)?;
        let (part, child) = prepared?;
        Ok(Self {
            source: structure.source,
            parts: structure.parts,
            headers: structure.headers,
            remaining: structure.remaining,
            header_bytes: structure.header_bytes,
            part,
            child,
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
        let result = (|| -> Result<_, Error> {
            self.source.check_deadline().map_err(Error::Policy)?;
            let result = self.child.check_deadline(now).map_err(Error::from);
            self.source
                .check_deadline()
                .map_err(Error::Policy)
                .and(result)
        })();
        self.outcome(result)
    }
    /// Passive evidence only; successful finish still requires fresh admission.
    pub fn value(&self) -> Option<PartView<'_>> {
        if self.failure.is_some() {
            return None;
        }
        let metadata = self.child.value()?;
        if metadata.headers.body_start != self.part.body_start {
            return None;
        }
        Some(PartView {
            part: self.part,
            metadata,
        })
    }
    pub fn poll(&mut self, now: Tick) -> Result<Status, Error> {
        self.check_deadline(now)?;
        let result = self
            .child
            .poll(now)
            .map_err(Error::from)
            .and_then(|status| {
                if status == Status::Complete && self.value().is_none() {
                    Err(bound::Error::InvalidState.into())
                } else {
                    Ok(status)
                }
            });
        let result = self
            .source
            .check_deadline()
            .map_err(Error::Policy)
            .and(result);
        self.outcome(result)
    }
    pub fn finish(mut self, now: Tick) -> Result<Retained<'p, 'v, 'c, 'b, 'o, 'w>, Error> {
        self.check_deadline(now)?;
        let result = (|| -> Result<_, Error> {
            let (metadata, work, budget, scratch) = self.child.finish(now)?;
            if metadata.headers.body_start != self.part.body_start {
                return Err(bound::Error::InvalidState.into());
            }
            let node = bound::classify(self.part, metadata.headers, now, work, budget)?;
            Ok((metadata, work, budget, scratch, node))
        })();
        self.source.check_deadline().map_err(Error::Policy)?;
        let (metadata, work, budget, scratch, node) = result?;
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
            metadata: ClassifiedView {
                part: self.part,
                metadata,
                node,
            },
            scratch,
        })
    }
}
const _: () = assert!(
    std::mem::size_of::<Cursor<'_, '_, '_, '_, '_, '_>>() + std::mem::size_of::<HeaderBudget>()
        <= 8 * 1024
);

#[cfg(test)]
#[path = "blob_part_tests.rs"]
mod tests;

//! Bind completed structure and part metadata to one authorized resident source.
pub mod ordered;
use super::{Part, Status};
const NODE_EXTRA_OUTPUT: u64 = (std::mem::size_of::<Node>() - std::mem::size_of::<Class>()) as u64;
const _: () = assert!(std::mem::size_of::<Node>() >= std::mem::size_of::<Class>());
use crate::{
    admission::work::Meter,
    header_select::SourceEnd,
    limits::Limits,
    mime_body_lists::{self, Class, Node},
    mime_part_headers::{self, label_json},
    nfc::{self, HeaderBudget, Scratch},
    ports::Tick,
};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Traversal(super::Error),
    Metadata(label_json::Error),
    Classification(mime_body_lists::Error),
    BodyLists(mime_body_lists::Error),
    Admission(nfc::Error),
    NodeCapacity,
    PartOrdinal,
    InvalidRange,
    InvalidState,
    Abandoned,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Traversal(error) => write!(f, "bound MIME traversal: {error}"),
            Self::Metadata(error) => write!(f, "bound MIME part metadata: {error}"),
            Self::Classification(error) => write!(f, "bound MIME classification: {error}"),
            Self::BodyLists(error) => write!(f, "bound MIME body lists: {error}"),
            Self::Admission(error) => write!(f, "bound MIME structure admission: {error}"),
            Self::NodeCapacity => f.write_str("bound MIME classification node capacity"),
            Self::PartOrdinal => f.write_str("invalid bound MIME part ordinal"),
            Self::InvalidRange => f.write_str("invalid bound MIME part range"),
            Self::Abandoned => f.write_str("unconsumed bound MIME part cursor"),
            Self::InvalidState => f.write_str("invalid bound MIME part state"),
        }
    }
}
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Traversal(error) => Some(error),
            Self::Metadata(error) => Some(error),
            Self::Classification(error) => Some(error),
            Self::BodyLists(error) => Some(error),
            Self::Admission(error) => Some(error),
            _ => None,
        }
    }
}
/// Only a healthy fresh traversal finish creates this immutable source binding.
/// It confers no source/blob authorization or response publication authority.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mta::mime_traversal::bound::Structure<'_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mta::mime_traversal::bound::Structure<'_, '_>>();
/// ```
pub struct Structure<'a, 'w> {
    source: &'a [u8],
    base: u64,
    header_limit: u64,
    parts: &'w [Part],
    work: &'w mut Meter,
    budget: &'w mut HeaderBudget,
    failure: Option<Error>,
}
impl<'a, 'w> Structure<'a, 'w> {
    /// Complete passive cells; copied offsets cannot construct another binding.
    pub fn parts(&self) -> Result<&[Part], Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        Ok(self.parts)
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self
            .budget
            .charge(self.work, now, 0, 0, &mut 0)
            .map_err(Error::Admission);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    fn selected(&self, ordinal: u16) -> Result<(Part, &'a [u8]), Error> {
        let index = usize::from(ordinal)
            .checked_sub(1)
            .ok_or(Error::PartOrdinal)?;
        let part = *self.parts.get(index).ok_or(Error::PartOrdinal)?;
        if part.ordinal != ordinal {
            return Err(Error::PartOrdinal);
        }
        if part.body_start < part.entity_start || part.body_start > part.entity_end {
            return Err(Error::InvalidRange);
        }
        let source =
            td_header::resident::slice(self.source, self.base, part.entity_start..part.entity_end)
                .ok_or(Error::InvalidRange)?;
        Ok((part, source))
    }
    /// Reborrow only this binding's original job/header owners and source.
    /// Caller scratch and independent windows must already be admitted.
    pub fn metadata<'m>(
        &'m mut self,
        ordinal: u16,
        backing: label_json::Backing<'m>,
        scratch: &'m mut Scratch,
    ) -> Result<PartCursor<'a, 'm>, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let (part, source) = match self.selected(ordinal) {
            Ok(value) => value,
            Err(error) => {
                self.failure = Some(error);
                return Err(error);
            }
        };
        let entity = mime_part_headers::Entity {
            source,
            base: part.entity_start,
            source_end: SourceEnd::Eof,
            header_limit: self.header_limit,
            context: part.context(),
        };
        let child = match label_json::Cursor::new(entity, backing, self.work, self.budget, scratch)
        {
            Ok(child) => child,
            Err(error) => {
                let error = Error::Metadata(error);
                self.failure = Some(error);
                return Err(error);
            }
        };
        self.failure = Some(Error::Abandoned);
        Ok(PartCursor {
            part,
            child: Some(child),
            failure: None,
            parent_failure: &mut self.failure,
        })
    }
    /// Fresh whole-binding admission before releasing the original owners.
    /// Previously copied passive evidence cannot authorize publication by itself.
    pub fn finish(
        mut self,
        now: Tick,
    ) -> Result<(&'w [Part], &'w mut Meter, &'w mut HeaderBudget), Error> {
        self.check_deadline(now)?;
        Ok((self.parts, self.work, self.budget))
    }
}
/// Exclusive original traversal owners; the complete binding appears on finish.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mta::mime_traversal::bound::Cursor<'_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mta::mime_traversal::bound::Cursor<'_, '_>>();
/// ```
pub struct Cursor<'a, 'w> {
    child: super::Cursor<'a, 'w>,
}
impl<'a, 'w> Cursor<'a, 'w> {
    pub fn new(
        source: &'a [u8],
        base: u64,
        source_end: SourceEnd,
        limits: &Limits,
        parts: &'w mut [Part],
        work: &'w mut Meter,
        budget: &'w mut HeaderBudget,
    ) -> Result<Self, Error> {
        let child = super::Cursor::new(source, base, source_end, limits, parts, work, budget)
            .map_err(Error::Traversal)?;
        Ok(Self { child })
    }
    pub fn poll(&mut self, now: Tick) -> Result<Status, Error> {
        self.child.poll(now).map_err(Error::Traversal)
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        self.child.check_deadline(now).map_err(Error::Traversal)
    }
    pub fn finish(self, now: Tick) -> Result<Structure<'a, 'w>, Error> {
        let source = self.child.source;
        let base = self.child.base;
        let header_limit = self.child.max_headers;
        let (parts, work, budget) = self.child.finish(now).map_err(Error::Traversal)?;
        Ok(Structure {
            source,
            base,
            header_limit,
            parts,
            work,
            budget,
            failure: None,
        })
    }
}
/// Complete passive part evidence and metadata, provisional through publication.
pub struct PartView<'w> {
    pub part: Part,
    pub metadata: label_json::View<'w>,
}
/// Complete original retained metadata and its descriptor's passive body node.
pub struct ClassifiedView<'w> {
    pub part: Part,
    pub metadata: label_json::View<'w>,
    pub node: Node,
}
/// Metadata child uses only the completed structure's immutable source binding.
/// Failure or abandonment retires the binding; finish freshly admits before release.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mta::mime_traversal::bound::PartCursor<'_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mta::mime_traversal::bound::PartCursor<'_, '_>>();
/// ```
pub struct PartCursor<'a, 'w> {
    part: Part,
    child: Option<label_json::Cursor<'a, 'w>>,
    failure: Option<Error>,
    parent_failure: &'w mut Option<Error>,
}
impl<'w> PartCursor<'_, 'w> {
    fn correlate(&self, metadata: &label_json::View<'_>) -> Result<(), Error> {
        if metadata.headers.body_start != self.part.body_start {
            return Err(Error::InvalidState);
        }
        Ok(())
    }
    fn outcome<T>(&mut self, result: Result<T, Error>) -> Result<T, Error> {
        if let Err(error) = result {
            self.failure = Some(error);
            *self.parent_failure = Some(error);
            self.child = None;
        }
        result
    }
    pub fn value(&self) -> Option<PartView<'_>> {
        if self.failure.is_some() {
            return None;
        }
        let metadata = self.child.as_ref()?.value()?;
        self.correlate(&metadata).ok()?;
        Some(PartView {
            part: self.part,
            metadata,
        })
    }
    pub fn poll(&mut self, now: Tick) -> Result<Status, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self
            .child
            .as_mut()
            .ok_or(Error::InvalidState)
            .and_then(|child| child.poll(now).map_err(Error::Metadata))
            .and_then(|status| {
                if status == mime_part_headers::Status::Complete {
                    let metadata = self
                        .child
                        .as_ref()
                        .and_then(label_json::Cursor::value)
                        .ok_or(Error::InvalidState)?;
                    self.correlate(&metadata)?;
                    Ok(Status::Complete)
                } else {
                    Ok(Status::Yield)
                }
            });
        self.outcome(result)
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self
            .child
            .as_mut()
            .ok_or(Error::InvalidState)
            .and_then(|child| child.check_deadline(now).map_err(Error::Metadata));
        self.outcome(result)
    }
    fn finish_metadata(
        &mut self,
        now: Tick,
    ) -> Result<
        (
            label_json::View<'w>,
            &'w mut Meter,
            &'w mut HeaderBudget,
            &'w mut Scratch,
        ),
        Error,
    > {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let child = match self.child.take() {
            Some(child) => child,
            None => {
                *self.parent_failure = Some(Error::InvalidState);
                return Err(Error::InvalidState);
            }
        };
        let (metadata, work, budget, scratch) = match child.finish(now) {
            Ok(value) => value,
            Err(error) => {
                let error = Error::Metadata(error);
                self.failure = Some(error);
                *self.parent_failure = Some(error);
                return Err(error);
            }
        };
        if let Err(error) = self.correlate(&metadata) {
            *self.parent_failure = Some(error);
            return Err(error);
        }
        Ok((metadata, work, budget, scratch))
    }
    pub fn finish(
        mut self,
        now: Tick,
    ) -> Result<
        (
            PartView<'w>,
            &'w mut Meter,
            &'w mut HeaderBudget,
            &'w mut Scratch,
        ),
        Error,
    > {
        let (metadata, work, budget, scratch) = self.finish_metadata(now)?;
        *self.parent_failure = None;
        Ok((
            PartView {
                part: self.part,
                metadata,
            },
            work,
            budget,
            scratch,
        ))
    }
    /// Consume original metadata into its descriptor's compact body-list node.
    /// Fixed comparisons and retained node bytes spend the original allowances.
    pub fn finish_classified(
        mut self,
        now: Tick,
    ) -> Result<
        (
            ClassifiedView<'w>,
            &'w mut Meter,
            &'w mut HeaderBudget,
            &'w mut Scratch,
        ),
        Error,
    > {
        let (metadata, work, budget, scratch) = self.finish_metadata(now)?;
        let result = (|| {
            let class = Class::from_headers(metadata.headers, now, work, budget)
                .map_err(Error::Classification)?;
            work.charge(
                now,
                crate::admission::work::Charge {
                    output_bytes: NODE_EXTRA_OUTPUT,
                    ..crate::admission::work::Charge::default()
                },
            )
            .map_err(|error| Error::Classification(mime_body_lists::Error::Work(error)))?;
            Ok(Node {
                parent: self.part.parent,
                depth: self.part.depth,
                class,
            })
        })();
        let node = self.outcome(result)?;
        *self.parent_failure = None;
        Ok((
            ClassifiedView {
                part: self.part,
                metadata,
                node,
            },
            work,
            budget,
            scratch,
        ))
    }
}
const _: () = assert!(
    std::mem::size_of::<Cursor<'_, '_>>() + std::mem::size_of::<HeaderBudget>() <= 16 * 1024
);
const _: () = assert!(
    std::mem::size_of::<Structure<'_, '_>>()
        + std::mem::size_of::<PartCursor<'_, '_>>()
        + std::mem::size_of::<HeaderBudget>()
        <= 8 * 1024
);

#[cfg(test)]
mod tests;

//! Source-bound part members; current access authorization and publication follow.
#[path = "members/retained.rs"]
pub mod retained;
#[path = "members/selected.rs"]
pub mod selected;
use super::{Bound, Error};
use crate::{admission::work::Charge, nfc::HeaderBudget, ports::Tick};
pub use td_json::string::{Progress, Status};

/// Only the original source-matched owner can supply these provisional bytes.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0308
/// use td_mta::{ports::Tick, mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::{View, pinned::members::Cursor}};
/// fn passive(view: View<'_, '_>) { let _ = Cursor::new(view, 1, Tick(1)); }
/// ```
pub struct Cursor<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k> {
    source: Bound<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k>,
    ordinal: u16,
    position: usize,
    total: usize,
    credit: crate::nfc::Credit,
    selection: Option<selected::Index>,
    failure: Option<Error>,
}
impl<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k> Cursor<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k> {
    pub fn new(
        source: Bound<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k>,
        ordinal: u16,
        now: Tick,
    ) -> Result<Self, Error> {
        Self::with_properties(source, ordinal, selected::Properties::ALL, now)
    }
    fn with_properties(
        mut source: Bound<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k>,
        ordinal: u16,
        properties: selected::Properties,
        now: Tick,
    ) -> Result<Self, Error> {
        source.check_deadline(now)?;
        let mut cursor = Self {
            source,
            ordinal,
            position: 0,
            total: 0,
            credit: crate::nfc::Credit::new(),
            selection: None,
            failure: None,
        };
        let total = cursor
            .segments()?
            .into_iter()
            .try_fold(0usize, |total, bytes| {
                total.checked_add(bytes.len()).ok_or(Error::InvalidState)
            })?;
        cursor.total = total;
        let bits = properties.bits();
        if bits == 0 {
            cursor.position = cursor.total;
        } else if bits != 1023 {
            let fragment = cursor
                .segments()?
                .into_iter()
                .next()
                .ok_or(Error::InvalidState)?;
            cursor.selection = Some(selected::Index::new(bits, fragment)?);
            cursor.total = 0;
        }
        Ok(cursor)
    }
    fn segments(&self) -> Result<[&[u8]; 5], Error> {
        let index = usize::from(self.ordinal.checked_sub(1).ok_or(Error::InvalidState)?);
        let original = self.source.value().ok_or(Error::InvalidState)?;
        let fragment = original
            .original
            .original
            .fragments
            .get(index)
            .and_then(|cell| cell.value())
            .ok_or(Error::InvalidState)?;
        let part = original
            .original
            .original
            .selected
            .structure
            .parts
            .get(index)
            .copied()
            .ok_or(Error::InvalidState)?;
        let candidate = original.candidates.get(index).ok_or(Error::InvalidState)?;
        if candidate.ordinal() != self.ordinal
            || part.ordinal != self.ordinal
            || fragment.end.part != part
        {
            return Err(Error::InvalidState);
        }
        if part.media == crate::mime_traversal::Media::Multipart {
            if candidate.locator().is_some() {
                return Err(Error::InvalidState);
            }
            return Ok([fragment.fragment, b",\"blobId\":", b"", b"null", b""]);
        }
        candidate.locator().ok_or(Error::InvalidState)?;
        let wire = candidate.wire().ok_or(Error::InvalidState)?;
        Ok([
            fragment.fragment,
            b",\"blobId\":",
            b"\"",
            wire.as_bytes(),
            b"\"",
        ])
    }

    fn outcome<T>(&mut self, result: Result<T, Error>) -> Result<T, Error> {
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    pub fn value(&self) -> Option<u16> {
        if self.failure.is_some()
            || self.position != self.total
            || self.selection.as_ref().is_some_and(|index| !index.ready())
        {
            return None;
        }
        self.source.value()?;
        Some(self.ordinal)
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self.source.check_deadline(now);
        self.outcome(result)
    }
    pub fn poll(&mut self, now: Tick, output: &mut [u8]) -> Result<Progress, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if self.value().is_some() {
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
        let result = self.step(now, output);
        let result = self
            .source
            .parent
            .check_deadline()
            .map_err(Error::Parent)
            .and(result);
        self.outcome(result)
    }
    fn step(&mut self, now: Tick, output: &mut [u8]) -> Result<Progress, Error> {
        if self.selection.as_ref().is_some_and(|index| !index.ready()) {
            return self.index_step(now);
        }
        let written = self
            .total
            .checked_sub(self.position)
            .ok_or(Error::InvalidState)?
            .min(output.len())
            .min(64);
        self.charge(now, 1, written)?;
        let segments = self.segments()?;
        let mut position = self.position;
        if let Some(index) = self.selection.as_ref() {
            for destination in output.get_mut(..written).ok_or(Error::InvalidState)? {
                *destination = index.byte_at(position, segments)?;
                position = position.checked_add(1).ok_or(Error::InvalidState)?;
            }
        } else {
            for destination in output.get_mut(..written).ok_or(Error::InvalidState)? {
                let mut offset = position;
                let mut byte = None;
                for segment in segments {
                    if let Some(found) = segment.get(offset) {
                        byte = Some(*found);
                        break;
                    }
                    offset = offset
                        .checked_sub(segment.len())
                        .ok_or(Error::InvalidState)?;
                }
                *destination = byte.ok_or(Error::InvalidState)?;
                position = position.checked_add(1).ok_or(Error::InvalidState)?;
            }
        }
        self.position = position;
        Ok(Progress {
            written,
            status: if self.position == self.total {
                Status::Complete
            } else {
                Status::Yield
            },
        })
    }
    fn charge(&mut self, now: Tick, steps: u64, written: usize) -> Result<(), Error> {
        let structure = &mut self
            .source
            .original
            .source
            .original
            .source
            .projected
            .structure;
        structure
            .budget
            .charge(structure.work, now, 0, steps, &mut self.credit)
            .map_err(|error| Error::Original(super::super::Error::Admission(error)))?;
        structure
            .work
            .charge(
                now,
                Charge {
                    records: 1,
                    output_bytes: u64::try_from(written).map_err(|_| Error::InvalidState)?,
                    ..Charge::default()
                },
            )
            .map_err(|error| {
                Error::Original(super::super::Error::Admission(crate::nfc::Error::Work(
                    error,
                )))
            })?;
        Ok(())
    }
    fn index_step(&mut self, now: Tick) -> Result<Progress, Error> {
        let index = self.selection.as_ref().ok_or(Error::InvalidState)?;
        let fragment = self
            .segments()?
            .into_iter()
            .next()
            .ok_or(Error::InvalidState)?;
        let count = index.remaining(fragment)?.min(64);
        self.charge(
            now,
            u64::try_from(count).map_err(|_| Error::InvalidState)?,
            0,
        )?;
        let mut index = self.selection.take().ok_or(Error::InvalidState)?;
        let segments = self.segments()?;
        let result = index.scan(segments, count).and_then(|()| {
            if index.ready() {
                Ok(Some(index.total(segments)?))
            } else {
                Ok(None)
            }
        });
        self.selection = Some(index);
        if let Some(total) = result? {
            self.total = total;
        }
        Ok(Progress {
            written: 0,
            status: Status::Yield,
        })
    }
    pub fn finish(
        mut self,
        now: Tick,
    ) -> Result<Member<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k>, Error> {
        self.check_deadline(now)?;
        if self.value().is_none() {
            return Err(Error::InvalidState);
        }
        Ok(Member {
            source: self.source,
            ordinal: self.ordinal,
            failure: None,
        })
    }
}
/// Completed emission retains original source matching and the actual descriptor.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::Member<'_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::Member<'_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
pub struct Member<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k> {
    source: Bound<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k>,
    ordinal: u16,
    failure: Option<Error>,
}
pub type Release<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k> =
    (Bound<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k>, u16);
impl<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k> Member<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k> {
    pub fn value(&self) -> Option<u16> {
        if self.failure.is_some() {
            return None;
        }
        self.source.value()?;
        Some(self.ordinal)
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self.source.check_deadline(now);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    pub fn finish(
        mut self,
        now: Tick,
    ) -> Result<Release<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k>, Error> {
        self.check_deadline(now)?;
        Ok((self.source, self.ordinal))
    }
}
const _: () = assert!(
    std::mem::size_of::<Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_>>()
        + std::mem::size_of::<HeaderBudget>()
        + 64
        + std::mem::size_of::<[&[u8]; 5]>()
        <= 1024
);
const _: () = assert!(std::mem::size_of::<Member<'_, '_, '_, '_, '_, '_, '_, '_, '_>>() <= 768);

#[cfg(test)]
pub use tests::probe as probe_allocations;
#[cfg(test)]
#[path = "members/tests.rs"]
mod tests;

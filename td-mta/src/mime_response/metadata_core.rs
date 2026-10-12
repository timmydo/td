//! Fixed-member selection over td-generated part fragments and locator segments.
use crate::{
    admission::work::{Charge, Meter},
    mime_response::{
        bound, locators::Candidate, pinned::Error, selected_metadata::Properties,
        selection_index::Index,
    },
    nfc::{Credit, HeaderBudget},
    ports::Tick,
};
use td_json::string::{Progress, Status};
pub(crate) struct Core {
    pub(in crate::mime_response) position: usize,
    total: usize,
    finalized: bool,
    pub(in crate::mime_response) credit: Credit,
    pub(in crate::mime_response) selection: Option<Index>,
}
pub(crate) struct Turn {
    indexing: bool,
    count: usize,
}
pub(crate) struct Charged(Turn);
impl Core {
    pub(crate) fn new(segments: [&[u8]; 5], properties: Properties) -> Result<Self, Error> {
        let total = segments.into_iter().try_fold(0usize, |n, b| {
            n.checked_add(b.len()).ok_or(Error::InvalidState)
        })?;
        let bits = properties.bits();
        let selection = if bits != 0 && bits != 1023 {
            Some(Index::new(
                bits,
                segments.into_iter().next().ok_or(Error::InvalidState)?,
            )?)
        } else {
            None
        };
        Ok(Self {
            position: if bits == 0 { total } else { 0 },
            total: if selection.is_some() { 0 } else { total },
            finalized: selection.is_none(),
            credit: Credit::new(),
            selection,
        })
    }
    pub(crate) fn complete(&self) -> bool {
        self.finalized && self.position == self.total
    }
    pub(crate) fn plan(&self, segments: [&[u8]; 5], output: usize) -> Result<Turn, Error> {
        if let Some(index) = self.selection.as_ref().filter(|index| !index.ready()) {
            let fragment = segments.into_iter().next().ok_or(Error::InvalidState)?;
            Ok(Turn {
                indexing: true,
                count: index.remaining(fragment)?.min(64),
            })
        } else {
            Ok(Turn {
                indexing: false,
                count: self
                    .total
                    .checked_sub(self.position)
                    .ok_or(Error::InvalidState)?
                    .min(output)
                    .min(64),
            })
        }
    }
    pub(crate) fn charge(
        &mut self,
        turn: Turn,
        work: &mut Meter,
        budget: &mut HeaderBudget,
        now: Tick,
    ) -> Result<Charged, Error> {
        let steps = if turn.indexing {
            u64::try_from(turn.count).map_err(|_| Error::InvalidState)?
        } else {
            1
        };
        let written = if turn.indexing {
            0
        } else {
            u64::try_from(turn.count).map_err(|_| Error::InvalidState)?
        };
        budget
            .charge(work, now, 0, steps, &mut self.credit)
            .map_err(|error| Error::Original(bound::Error::Admission(error)))?;
        work.charge(
            now,
            Charge {
                records: 1,
                output_bytes: written,
                ..Charge::default()
            },
        )
        .map_err(|error| {
            Error::Original(bound::Error::Admission(crate::nfc::Error::Work(error)))
        })?;
        Ok(Charged(turn))
    }
    pub(crate) fn emit(
        &mut self,
        charged: Charged,
        segments: [&[u8]; 5],
        output: &mut [u8],
    ) -> Result<Progress, Error> {
        let turn = charged.0;
        if turn.indexing {
            let index = self.selection.as_mut().ok_or(Error::InvalidState)?;
            index.scan(segments, turn.count)?;
            if index.ready() {
                self.total = index.total(segments)?;
                self.finalized = true;
            }
            return Ok(Progress {
                written: 0,
                status: Status::Yield,
            });
        }
        let mut position = self.position;
        for destination in output.get_mut(..turn.count).ok_or(Error::InvalidState)? {
            *destination = if let Some(index) = self.selection.as_ref() {
                index.byte_at(position, segments)?
            } else {
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
                byte.ok_or(Error::InvalidState)?
            };
            position = position.checked_add(1).ok_or(Error::InvalidState)?;
        }
        self.position = position;
        Ok(Progress {
            written: turn.count,
            status: if self.position == self.total {
                Status::Complete
            } else {
                Status::Yield
            },
        })
    }
}

pub(crate) fn segments<'f>(
    fragment: &'f [u8],
    candidate: &'f Candidate,
    multipart: bool,
) -> Result<[&'f [u8]; 5], Error> {
    if multipart {
        if candidate.locator().is_some() {
            return Err(Error::InvalidState);
        }
        Ok([fragment, b",\"blobId\":", b"", b"null", b""])
    } else {
        candidate.locator().ok_or(Error::InvalidState)?;
        let wire = candidate.wire().ok_or(Error::InvalidState)?;
        Ok([fragment, b",\"blobId\":", b"\"", wire.as_bytes(), b"\""])
    }
}

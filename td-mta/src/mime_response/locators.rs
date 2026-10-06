//! Original direct-leaf locator candidates; parent authorization stays external.
use {
    crate::admission::work::Charge, crate::ids::BlobId, crate::mime_structure::Encoding,
    crate::mime_structure::Media, crate::ports::Tick, crate::wire::PartLocator,
    crate::wire::TransferEncoding,
};
use {
    crate::mime_response::body_window::Retained, crate::mime_response::body_window::ViewBytes,
    crate::mime_response::bound::Error,
};
/// Passive candidate bytes; neither a parent pin nor permission to publish/download.
#[derive(Clone, Copy)]
pub struct Candidate {
    pub(in crate::mime_response) ordinal: u16,
    locator: Option<PartLocator>,
    wire: [u8; PartLocator::WIRE_BYTES],
}
impl Default for Candidate {
    fn default() -> Self {
        Self {
            ordinal: 0,
            locator: None,
            wire: [0; PartLocator::WIRE_BYTES],
        }
    }
}
impl Candidate {
    pub fn ordinal(&self) -> u16 {
        self.ordinal
    }
    pub fn locator(&self) -> Option<PartLocator> {
        self.locator
    }
    pub fn wire(&self) -> Option<&str> {
        self.locator?;
        std::str::from_utf8(&self.wire).ok()
    }
}
#[derive(Clone, Copy)]
pub struct View<'v, 'o> {
    pub parent: BlobId,
    pub original: ViewBytes<'v, 'o>,
    pub candidates: &'v [Candidate],
}
/// The original complete retention is consumed; passive bytes cannot substitute.
/// ```compile_fail,E0277
/// fn bound<T: Copy>() {} bound::<td_mta::mime_response::locators::Cursor<'_, '_, '_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn bound<T: Clone>() {} bound::<td_mta::mime_response::locators::Cursor<'_, '_, '_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0308
/// use {td_mta::ids::BlobId, td_mta::ports::Tick, td_mta::mime_response::body_window::ViewBytes, td_mta::mime_response::locators::Cursor, td_mta::mime_response::locators::Candidate};
/// fn passive(view: ViewBytes<'_, '_>, cells: &mut [Candidate]) {let _=Cursor::new(view,BlobId::from_bytes([0;16]),cells,Tick(1));}
/// ```
pub struct Cursor<'a, 'w, 'n, 'c, 'o, 'r, 'l> {
    source: Retained<'a, 'w, 'n, 'c, 'o, 'r>,
    parent: BlobId,
    candidates: &'l mut [Candidate],
    next: usize,
    credit: crate::nfc::Credit,
    failure: Option<Error>,
}
impl<'a, 'w, 'n, 'c, 'o, 'r, 'l> Cursor<'a, 'w, 'n, 'c, 'o, 'r, 'l> {
    pub fn new(
        mut source: Retained<'a, 'w, 'n, 'c, 'o, 'r>,
        parent: BlobId,
        candidates: &'l mut [Candidate],
        now: Tick,
    ) -> Result<Self, Error> {
        source.check_deadline(now)?;
        let count = source.original.source.projected.structure.parts()?.len();
        if candidates.len() < count {
            return Err(Error::ResponseCapacity);
        }
        if candidates.len() > count {
            return Err(Error::InvalidState);
        }
        Ok(Self {
            source,
            parent,
            candidates,
            next: 0,
            credit: crate::nfc::Credit::new(),
            failure: None,
        })
    }
    pub(in crate::mime_response) fn outcome<T>(
        &mut self,
        result: Result<T, Error>,
    ) -> Result<T, Error> {
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self.source.check_deadline(now);
        self.outcome(result)
    }
    pub fn value(&self) -> Option<View<'_, 'o>> {
        if self.failure.is_some() || self.next != self.candidates.len() {
            return None;
        }
        Some(View {
            parent: self.parent,
            original: self.source.value()?,
            candidates: self.candidates,
        })
    }
    pub fn poll(&mut self, now: Tick) -> Result<crate::mime_response::body_window::Status, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if self.value().is_some() {
            return Ok(crate::mime_response::body_window::Status::Complete);
        }
        self.check_deadline(now)?;
        let result = self.step(now);
        self.outcome(result)
    }
    fn step(&mut self, now: Tick) -> Result<crate::mime_response::body_window::Status, Error> {
        let structure = &mut self.source.original.source.projected.structure;
        let part = structure
            .parts()?
            .get(self.next)
            .copied()
            .ok_or(Error::InvalidState)?;
        let ordinal = self
            .next
            .checked_add(1)
            .and_then(|n| u16::try_from(n).ok())
            .ok_or(Error::InvalidState)?;
        if part.ordinal != ordinal {
            return Err(Error::InvalidState);
        }
        let leaf = part.media != Media::Multipart;
        structure
            .budget
            .charge(
                structure.work,
                now,
                0,
                if leaf {
                    PartLocator::WIRE_BYTES as u64
                } else {
                    1
                },
                &mut self.credit,
            )
            .map_err(Error::Admission)?;
        structure
            .work
            .charge(
                now,
                Charge {
                    records: 1,
                    output_bytes: if leaf {
                        PartLocator::WIRE_BYTES as u64
                    } else {
                        0
                    },
                    ..Charge::default()
                },
            )
            .map_err(|error| Error::Admission(crate::nfc::Error::Work(error)))?;
        let mut candidate = Candidate {
            ordinal,
            ..Candidate::default()
        };
        if leaf {
            let offset = part
                .body_start
                .checked_sub(structure.base)
                .ok_or(Error::InvalidRange)?;
            let length = part
                .entity_end
                .checked_sub(part.body_start)
                .ok_or(Error::InvalidRange)?;
            let parent_length =
                u64::try_from(structure.source.len()).map_err(|_| Error::InvalidRange)?;
            let locator = PartLocator {
                parent: self.parent,
                offset,
                length,
                encoding: match part.encoding {
                    Encoding::Identity => TransferEncoding::Identity,
                    Encoding::Base64 => TransferEncoding::Base64,
                    Encoding::QuotedPrintable => TransferEncoding::QuotedPrintable,
                },
            };
            locator
                .checked_end(parent_length)
                .map_err(|_| Error::InvalidRange)?;
            locator
                .encode(&mut candidate.wire)
                .map_err(|_| Error::InvalidState)?;
            candidate.locator = Some(locator);
        }
        *self
            .candidates
            .get_mut(self.next)
            .ok_or(Error::InvalidState)? = candidate;
        self.next = self.next.checked_add(1).ok_or(Error::InvalidState)?;
        Ok(if self.next == self.candidates.len() {
            crate::mime_response::body_window::Status::Complete
        } else {
            crate::mime_response::body_window::Status::Yield
        })
    }
    pub fn finish(mut self, now: Tick) -> Result<Mapped<'a, 'w, 'n, 'c, 'o, 'r, 'l>, Error> {
        self.check_deadline(now)?;
        if self.value().is_none() {
            return self.outcome(Err(Error::InvalidState));
        }
        Ok(Mapped {
            source: self.source,
            parent: self.parent,
            candidates: self.candidates,
        })
    }
}
/// Complete original candidate mapping; still not parent authorization.
/// ```compile_fail,E0277
/// fn bound<T: Copy>() {} bound::<td_mta::mime_response::locators::Mapped<'_, '_, '_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn bound<T: Clone>() {} bound::<td_mta::mime_response::locators::Mapped<'_, '_, '_, '_, '_, '_, '_>>();
/// ```
pub struct Mapped<'a, 'w, 'n, 'c, 'o, 'r, 'l> {
    pub(in crate::mime_response) source: Retained<'a, 'w, 'n, 'c, 'o, 'r>,
    pub(in crate::mime_response) parent: BlobId,
    pub(in crate::mime_response) candidates: &'l [Candidate],
}
pub type Release<'w, 'n, 'c, 'o, 'r, 'l> = (
    (BlobId, &'l [Candidate]),
    crate::mime_response::body_window::Release<'w, 'n, 'c, 'o, 'r>,
);
impl<'w, 'n, 'c, 'o, 'r, 'l> Mapped<'_, 'w, 'n, 'c, 'o, 'r, 'l> {
    pub fn value(&self) -> Option<View<'_, 'o>> {
        Some(View {
            parent: self.parent,
            original: self.source.value()?,
            candidates: self.candidates,
        })
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        self.source.check_deadline(now)
    }
    pub fn finish(self, now: Tick) -> Result<Release<'w, 'n, 'c, 'o, 'r, 'l>, Error> {
        Ok(((self.parent, self.candidates), self.source.finish(now)?))
    }
}
const _: () = assert!(std::mem::size_of::<Candidate>() <= 128);
const _: () = assert!(
    std::mem::size_of::<Cursor<'_, '_, '_, '_, '_, '_, '_>>()
        + std::mem::size_of::<crate::nfc::HeaderBudget>()
        + std::mem::size_of::<Candidate>()
        <= 1024
);
const _: () = assert!(std::mem::size_of::<Mapped<'_, '_, '_, '_, '_, '_, '_>>() <= 256);

#[cfg(test)]
#[path = "locators/tests.rs"]
pub(in crate::mime_response) mod tests;

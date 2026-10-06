//! Match original resident source identity while retaining its actual file pin.
use crate::{
    admission::work::Charge,
    ports::{BlobReader, Crypto, CryptoError, Digest, Error as PolicyError, Tick},
    store_fs::PinnedBlob,
};
use {crate::mime_response::locators::Mapped, crate::mime_response::locators::View};
const CHUNK: usize = 4096;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Original(crate::mime_response::bound::Error),
    Parent(PolicyError),
    Crypto(CryptoError),
    SourceMismatch,
    InvalidState,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Original(error) => write!(f, "pinned MIME original: {error}"),
            Self::Parent(error) => write!(f, "pinned MIME parent: {error}"),
            Self::Crypto(error) => write!(f, "pinned MIME digest: {error}"),
            Self::SourceMismatch => f.write_str("pinned MIME source identity mismatch"),
            Self::InvalidState => f.write_str("invalid pinned MIME state"),
        }
    }
}
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Original(error) => Some(error),
            Self::Parent(error) => Some(error),
            Self::Crypto(error) => Some(error),
            _ => None,
        }
    }
}
/// Root authorization is the caller's obligation before opening PinnedBlob.
/// This owner binds source identity and keeps that pin, not email visibility.
/// ```compile_fail,E0277
/// fn bound<T: Copy>() {} bound::<td_mta::mime_response::pinned::Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_, td_crypto::Provider>>();
/// ```
/// ```compile_fail,E0277
/// fn bound<T: Clone>() {} bound::<td_mta::mime_response::pinned::Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_, td_crypto::Provider>>();
/// ```
/// ```compile_fail,E0308
/// use {td_mta::ports::Tick, td_mta::store_fs::PinnedBlob, td_mta::mime_response::locators::View, td_mta::mime_response::pinned::Cursor};
/// fn passive(view: View<'_, '_>, parent: PinnedBlob<'_, '_>) { let _=Cursor::new(view,parent,&td_crypto::Provider,Tick(1)); }
/// ```
pub struct Cursor<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, C: Crypto> {
    original: Mapped<'a, 'w, 'n, 'c, 'o, 'r, 'l>,
    parent: PinnedBlob<'p, 'k>,
    crypto: &'k C,
    digest: Option<C::Sha256>,
    position: usize,
    complete: bool,
    credit: crate::nfc::Credit,
    failure: Option<Error>,
}
impl<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, C: Crypto> Cursor<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k, C> {
    pub fn new(
        mut original: Mapped<'a, 'w, 'n, 'c, 'o, 'r, 'l>,
        mut parent: PinnedBlob<'p, 'k>,
        crypto: &'k C,
        now: Tick,
    ) -> Result<Self, Error> {
        original.check_deadline(now).map_err(Error::Original)?;
        parent.check_deadline().map_err(Error::Parent)?;
        let source = original.source.original.source.projected.structure.source;
        let length = u64::try_from(source.len()).map_err(|_| Error::InvalidState)?;
        if parent.id() != original.parent || parent.len() != length {
            return Err(Error::SourceMismatch);
        }
        if std::mem::size_of::<Self>()
            .checked_add(std::mem::size_of::<crate::nfc::HeaderBudget>())
            .and_then(|bytes| bytes.checked_add(32))
            .is_none_or(|bytes| bytes > 1024)
        {
            return Err(Error::InvalidState);
        }
        let digest = crypto.sha256();
        parent.check_deadline().map_err(Error::Parent)?;
        let digest = digest.map_err(Error::Crypto)?;
        Ok(Self {
            original,
            parent,
            crypto,
            digest: Some(digest),
            position: 0,
            complete: false,
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
            self.digest = None;
        }
        result
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self
            .original
            .check_deadline(now)
            .map_err(Error::Original)
            .and_then(|()| self.parent.check_deadline().map_err(Error::Parent));
        self.outcome(result)
    }
    pub fn value(&self) -> Option<View<'_, 'o>> {
        if self.failure.is_some() || !self.complete {
            return None;
        }
        self.original.value()
    }
    pub fn poll(&mut self, now: Tick) -> Result<crate::mime_response::body_window::Status, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if self.complete {
            return Ok(crate::mime_response::body_window::Status::Complete);
        }
        self.check_deadline(now)?;
        let result = self.step(now);
        let fenced = self
            .parent
            .check_deadline()
            .map_err(Error::Parent)
            .and(result);
        self.outcome(fenced)
    }
    fn step(&mut self, now: Tick) -> Result<crate::mime_response::body_window::Status, Error> {
        let structure = &mut self.original.source.original.source.projected.structure;
        let end = self
            .position
            .checked_add(CHUNK)
            .ok_or(Error::InvalidState)?
            .min(structure.source.len());
        let bytes = structure
            .source
            .get(self.position..end)
            .ok_or(Error::InvalidState)?;
        structure
            .budget
            .charge(structure.work, now, 0, 1, &mut self.credit)
            .map_err(|e| Error::Original(crate::mime_response::bound::Error::Admission(e)))?;
        structure
            .work
            .charge(
                now,
                Charge {
                    io_bytes: u64::try_from(bytes.len()).map_err(|_| Error::InvalidState)?,
                    records: 1,
                    ..Charge::default()
                },
            )
            .map_err(|e| {
                Error::Original(crate::mime_response::bound::Error::Admission(
                    crate::nfc::Error::Work(e),
                ))
            })?;
        self.digest
            .as_mut()
            .ok_or(Error::InvalidState)?
            .update(bytes)
            .map_err(Error::Crypto)?;
        self.position = end;
        if end == structure.source.len() {
            let digest = self
                .digest
                .take()
                .ok_or(Error::InvalidState)?
                .finish()
                .map_err(Error::Crypto)?;
            if !self.crypto.equal_digest(&digest, self.parent.digest()) {
                return Err(Error::SourceMismatch);
            }
            self.complete = true;
            Ok(crate::mime_response::body_window::Status::Complete)
        } else {
            Ok(crate::mime_response::body_window::Status::Yield)
        }
    }
    pub fn finish(mut self, now: Tick) -> Result<Bound<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k>, Error> {
        self.check_deadline(now)?;
        if self.value().is_none() {
            return Err(Error::InvalidState);
        }
        Ok(Bound {
            original: self.original,
            parent: self.parent,
            failure: None,
        })
    }
}
/// Matching original bytes and retained pin; authorization is still external.
/// ```compile_fail,E0277
/// fn bound<T: Copy>() {} bound::<td_mta::mime_response::pinned::Bound<'_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn bound<T: Clone>() {} bound::<td_mta::mime_response::pinned::Bound<'_, '_, '_, '_, '_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0451
/// use td_mta::{mime_response::{locators::Mapped, pinned::Bound}, store_fs::PinnedBlob};
/// fn fabricate(original: Mapped<'_, '_, '_, '_, '_, '_, '_>, parent: PinnedBlob<'_, '_>) {
///     let _ = Bound { original, parent, failure: None };
/// }
/// ```
pub struct Bound<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k> {
    pub(in crate::mime_response) original: Mapped<'a, 'w, 'n, 'c, 'o, 'r, 'l>,
    pub(in crate::mime_response) parent: PinnedBlob<'p, 'k>,
    pub(in crate::mime_response) failure: Option<Error>,
}
pub type Release<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k> =
    (Mapped<'a, 'w, 'n, 'c, 'o, 'r, 'l>, PinnedBlob<'p, 'k>);
impl<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k> Bound<'a, 'w, 'n, 'c, 'o, 'r, 'l, 'p, 'k> {
    pub fn value(&self) -> Option<View<'_, 'o>> {
        if self.failure.is_some() {
            return None;
        }
        self.original.value()
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self
            .original
            .check_deadline(now)
            .map_err(Error::Original)
            .and_then(|()| self.parent.check_deadline().map_err(Error::Parent));
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
        Ok((self.original, self.parent))
    }
}

const _: () = assert!(
    std::mem::size_of::<Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_, td_crypto::Provider>>()
        + std::mem::size_of::<crate::nfc::HeaderBudget>()
        + 32
        <= 1024
);
const _: () = assert!(std::mem::size_of::<Bound<'_, '_, '_, '_, '_, '_, '_, '_, '_>>() <= 768);

#[cfg(test)]
pub use tests::probe as probe_allocations;
#[cfg(test)]
#[path = "pinned/tests.rs"]
pub(in crate::mime_response) mod tests;

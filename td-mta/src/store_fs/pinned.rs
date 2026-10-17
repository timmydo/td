//! Snapshot-bound bounded reads of a verified PostgreSQL body.
use crate::{
    format::row::BlobRow,
    ids::BlobId,
    ports::{BlobReader, Error as PolicyError},
};
/// Internal adapters grant this boundary only after whole-body verification.
/// The source retains the original immutable snapshot and finite deadline;
/// checks and reads must preserve sticky connection/snapshot failures.
pub(crate) trait VerifiedSource: BlobReader {
    fn check(&mut self) -> Result<(), PolicyError>;
}
/// One verified immutable body retaining its original snapshot custody.
/// Construction is private to the verified storage adapters.
pub struct PinnedBlob<'a, 'c> {
    source: &'a mut (dyn VerifiedSource + 'c),
    id: BlobId,
    row: BlobRow,
    failed: Option<PolicyError>,
}
// Drop checking keeps the snapshot loan until this owner (or its MIME owner)
// is destroyed, even after its last read. The view owns the native connection.
impl Drop for PinnedBlob<'_, '_> {
    fn drop(&mut self) {}
}
impl<'a, 'c> PinnedBlob<'a, 'c> {
    pub(crate) fn from_verified(
        source: &'a mut (dyn VerifiedSource + 'c),
        id: BlobId,
        row: BlobRow,
    ) -> Result<Self, PolicyError> {
        source.check()?;
        if source.len() != row.length {
            return Err(PolicyError::Corrupt);
        }
        Ok(Self {
            source,
            id,
            row,
            failed: None,
        })
    }
    pub fn id(&self) -> BlobId {
        self.id
    }
    pub fn digest(&self) -> &[u8; 32] {
        &self.row.digest
    }
    pub fn is_failed(&self) -> bool {
        self.failed.is_some()
    }
    pub fn check_deadline(&mut self) -> Result<(), PolicyError> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = self.source.check();
        if let Err(error) = result {
            self.failed = Some(error);
        }
        result
    }
}
impl BlobReader for PinnedBlob<'_, '_> {
    fn len(&self) -> u64 {
        self.row.length
    }
    fn read_at(&mut self, offset: u64, output: &mut [u8]) -> Result<usize, PolicyError> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = (|| {
            self.source.check()?;
            let result = self.source.read_at(offset, output);
            let after = self.source.check();
            result.and_then(|count| after.map(|()| count))
        })();
        if let Err(error) = result {
            self.failed = Some(error);
        }
        result
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
pub fn with_pinned_fixture<'c>(
    bytes: &'c [u8],
    clock: &'c dyn crate::ports::Clock,
    run: impl FnOnce(PinnedBlob<'_, 'c>),
) {
    use crate::ports::{Crypto, Digest};
    struct Source<'a> {
        bytes: &'a [u8],
        clock: &'a dyn crate::ports::Clock,
        last: u64,
    }
    impl VerifiedSource for Source<'_> {
        fn check(&mut self) -> Result<(), PolicyError> {
            for _ in 0..2 {
                let now = self.clock.sample()?.monotonic.0;
                if now < self.last {
                    return Err(PolicyError::Invalid);
                }
                self.last = now;
                if now >= 100 {
                    return Err(PolicyError::Deadline);
                }
            }
            Ok(())
        }
    }
    impl BlobReader for Source<'_> {
        fn len(&self) -> u64 {
            self.bytes.len() as u64
        }
        fn read_at(&mut self, offset: u64, output: &mut [u8]) -> Result<usize, PolicyError> {
            let offset = usize::try_from(offset).map_err(|_| PolicyError::Invalid)?;
            let remaining = self.bytes.get(offset..).ok_or(PolicyError::Invalid)?;
            let count = remaining.len().min(output.len());
            output
                .get_mut(..count)
                .ok_or(PolicyError::Invalid)?
                .copy_from_slice(remaining.get(..count).ok_or(PolicyError::Invalid)?);
            Ok(count)
        }
    }
    let mut digest = td_crypto::Provider.sha256().unwrap();
    digest.update(bytes).unwrap();
    let row = BlobRow {
        length: bytes.len() as u64,
        digest: digest.finish().unwrap(),
        created_at: 0,
    };
    let mut source = Source {
        bytes,
        clock,
        last: 1,
    };
    run(PinnedBlob {
        source: &mut source,
        id: BlobId::from_bytes([0x44; 16]),
        row,
        failed: None,
    });
}

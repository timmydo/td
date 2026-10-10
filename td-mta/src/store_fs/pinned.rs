//! Snapshot-bound verification and bounded random reads of immutable SQLite bodies.
use super::{index::BodyRead, IndexReadView};
use crate::{
    format::{
        key::Key,
        row::{BlobRow, Row},
    },
    ids::BlobId,
    ports::{BlobReader, Clock, Crypto, Deadline, Digest, Error as PolicyError, ReadView, Time},
};
use std::sync::atomic::{AtomicU64, Ordering};
struct VerifyClock<'a> {
    source: &'a dyn Clock,
    deadline: Deadline,
    last: AtomicU64,
}
impl Clock for VerifyClock<'_> {
    fn sample(&self) -> Result<Time, PolicyError> {
        let now = self.source.sample()?;
        let prior = self.last.fetch_max(now.monotonic.0, Ordering::Relaxed);
        if now.monotonic.0 < prior {
            return Err(PolicyError::Invalid);
        }
        if self.deadline.expired(now.monotonic) {
            return Err(PolicyError::Deadline);
        }
        Ok(now)
    }
}
fn checked<T>(
    clock: &VerifyClock<'_>,
    body: &BodyRead<'_>,
    run: impl FnOnce() -> Result<T, PolicyError>,
) -> Result<T, PolicyError> {
    body.read_snapshot(|| {
        clock.sample()?;
        let result = run();
        clock.sample()?;
        result
    })
}
impl IndexReadView<'_, '_> {
    /// The original snapshot and deadline cover verification and subsequent reads.
    pub fn open_blob_input<'a, 'c, C: Crypto>(
        &'a mut self,
        crypto: &'c C,
        id: BlobId,
        max_bytes: u64,
    ) -> Result<PinnedBlobInput<'a, 'c, C>, PolicyError>
    where
        'a: 'c,
    {
        let mut value = [0; 64];
        let row = match self.get(Key::Blob(id), &mut value)? {
            Some((Row::Blob(row), _)) => row,
            Some(_) => return Err(PolicyError::Corrupt),
            None => return Err(PolicyError::NotFound),
        };
        if row.length > max_bytes {
            return Err(PolicyError::Capacity);
        }
        let body = self.open_body(id, row.length)?;
        let deadline = body.deadline();
        let clock = VerifyClock {
            source: body.clock(),
            deadline,
            last: AtomicU64::new(0),
        };
        let digest = checked(&clock, &body, || crypto.sha256().map_err(PolicyError::from))?;
        Ok(PinnedBlobInput {
            body,
            crypto,
            digest,
            id,
            row,
            position: 0,
            clock,
            failed: None,
        })
    }
}
/// Retains the pooled view borrow; an error retires input without further I/O.
pub struct PinnedBlobInput<'a, 'c, C: Crypto> {
    body: BodyRead<'a>,
    crypto: &'c C,
    digest: C::Sha256,
    id: BlobId,
    row: BlobRow,
    position: u64,
    clock: VerifyClock<'c>,
    failed: Option<PolicyError>,
}
impl<'a, 'c, C: Crypto> PinnedBlobInput<'a, 'c, C> {
    pub fn len(&self) -> u64 {
        self.row.length
    }
    pub fn is_empty(&self) -> bool {
        self.row.length == 0
    }
    pub fn position(&self) -> u64 {
        self.position
    }
    pub fn is_failed(&self) -> bool {
        self.failed.is_some()
    }
    /// Output remains provisional until complete digest verification succeeds.
    pub fn read(&mut self, output: &mut [u8]) -> Result<usize, PolicyError> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = checked(&self.clock, &self.body, || {
            let count = self.body.read_body(self.position, output)?;
            self.digest
                .update(output.get(..count).ok_or(PolicyError::Corrupt)?)?;
            self.position = self
                .position
                .checked_add(count as u64)
                .ok_or(PolicyError::Corrupt)?;
            Ok(count)
        });
        if let Err(error) = result {
            self.failed = Some(error);
        }
        result
    }
    /// Whole-body integrity grants random reads under the same live snapshot.
    pub fn finish(self) -> Result<PinnedBlob<'a, 'c>, PolicyError> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        checked(&self.clock, &self.body, || {
            if self.position != self.row.length {
                return Err(PolicyError::Invalid);
            }
            if !self
                .crypto
                .equal_digest(&self.digest.finish()?, &self.row.digest)
            {
                return Err(PolicyError::Corrupt);
            }
            Ok(())
        })?;
        Ok(PinnedBlob {
            body: self.body,
            id: self.id,
            row: self.row,
            clock: self.clock,
            failed: None,
        })
    }
}
/// One verified immutable body. Its borrow retains the pooled SQLite snapshot.
pub struct PinnedBlob<'a, 'c> {
    body: BodyRead<'a>,
    id: BlobId,
    row: BlobRow,
    clock: VerifyClock<'c>,
    failed: Option<PolicyError>,
}
// Drop checking keeps the snapshot loan until this owner (or its MIME owner)
// is destroyed, even after its last read. The view owns the native connection.
impl Drop for PinnedBlob<'_, '_> {
    fn drop(&mut self) {}
}
impl PinnedBlob<'_, '_> {
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
        let result = checked(&self.clock, &self.body, || Ok(()));
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
        let result = checked(&self.clock, &self.body, || {
            self.body.read_body(offset, output)
        });
        if let Err(error) = result {
            self.failed = Some(error);
        }
        result
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
pub fn with_pinned_fixture(bytes: &[u8], clock: &dyn Clock, run: impl FnOnce(PinnedBlob<'_, '_>)) {
    use super::tests::Fixture;
    use crate::{
        format::{
            operation::Operation,
            row::{BlobRow, Row},
        },
        ids::{AccountId, BlobId, StoreEpoch},
        ports::{Digest, Tick},
    };
    struct Fixed;
    impl Clock for Fixed {
        fn sample(&self) -> Result<Time, PolicyError> {
            Ok(Time {
                utc_ms: 0,
                monotonic: Tick(1),
            })
        }
    }
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let account = AccountId::from_bytes([1; 16]);
    let id = BlobId::from_bytes([0x44; 16]);
    let deadline = Deadline::after(Tick(0), 100).unwrap();
    let store = super::IndexStore::create(
        &mut root,
        StoreEpoch::from_bytes([3; 16]),
        std::sync::Arc::new(Fixed),
        1,
        deadline,
    )
    .unwrap();
    store.create_account(account, deadline).unwrap();
    let mut digest = td_crypto::Provider.sha256().unwrap();
    digest.update(bytes).unwrap();
    let row = Row::Blob(BlobRow {
        length: bytes.len() as u64,
        digest: digest.finish().unwrap(),
        created_at: 0,
    });
    let mut key = [0; 16];
    let len = Key::Blob(id).encode(&mut key).unwrap();
    let mut value = [0; 64];
    let length = row.encode(&mut value).unwrap();
    let op = Operation::put(crate::format::Table::Blobs, &key[..len], &value[..length]).unwrap();
    let mut source = bytes;
    store
        .commit(
            &td_crypto::Provider,
            super::CommitRequest {
                account,
                epoch: store.epoch(),
                expected: crate::format::Sequence::default(),
                utc_ms: 0,
                deadline,
            },
            &[op],
            &mut [super::BlobSource {
                id,
                source: &mut source,
            }],
        )
        .unwrap();
    let mut view = store.view(account, deadline).unwrap();
    let mut input = view
        .open_blob_input(&td_crypto::Provider, id, bytes.len() as u64)
        .unwrap();
    let mut scratch = [0; 4096];
    while input.position() != input.len() {
        assert_ne!(input.read(&mut scratch).unwrap(), 0);
    }
    let mut pin = input.finish().unwrap();
    pin.clock = VerifyClock {
        source: clock,
        deadline,
        last: AtomicU64::new(1),
    };
    run(pin);
}

#[cfg(test)]
mod owner_size_tests {
    use super::*;
    #[test]
    fn retained_body_owners_fit_the_four_kib_state_partition() {
        assert!(
            std::mem::size_of::<PinnedBlobInput<'static, 'static, td_crypto::Provider>>() <= 4096
        );
        assert!(std::mem::size_of::<PinnedBlob<'static, 'static>>() <= 4096);
    }
}

#[cfg(test)]
pub(in crate::store_fs) mod snapshot_tests {
    use super::*;
    pub(in crate::store_fs) fn input_body<'b, 'a, C: Crypto>(
        input: &'b PinnedBlobInput<'a, '_, C>,
    ) -> &'b BodyRead<'a> {
        &input.body
    }
    pub(in crate::store_fs) fn pin_body<'b, 'a>(pin: &'b PinnedBlob<'a, '_>) -> &'b BodyRead<'a> {
        &pin.body
    }
}

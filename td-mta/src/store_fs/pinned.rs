//! Snapshot-bound integrity checking and immutable message file reads.
use super::{BlobInput, BlobInputError, CompleteBlob, IndexReadView};
use crate::{
    format::{
        key::Key,
        row::{BlobKind, Row},
    },
    ids::BlobId,
    ports::{BlobReader, Clock, Crypto, Deadline, Error as PolicyError, ReadView, Time},
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
fn blob_io_error(error: std::io::Error) -> PolicyError {
    match error.kind() {
        std::io::ErrorKind::NotFound
        | std::io::ErrorKind::UnexpectedEof
        | std::io::ErrorKind::InvalidData => PolicyError::Corrupt,
        _ => error.into(),
    }
}
pub(in crate::store_fs) fn blob_error(error: BlobInputError) -> PolicyError {
    match error {
        BlobInputError::Io(error) => blob_io_error(error),
        BlobInputError::Crypto(error) => error.into(),
        BlobInputError::Checksum => PolicyError::Corrupt,
    }
}
fn checked<T>(
    clock: &VerifyClock<'_>,
    run: impl FnOnce() -> Result<T, PolicyError>,
) -> Result<T, PolicyError> {
    clock.sample()?;
    let result = run();
    clock.sample()?;
    result
}
impl IndexReadView<'_, '_> {
    /// The original snapshot clock and deadline cover verification and reads.
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
        let account = self.identity().account;
        let (root, source, deadline) = self.blob_scope()?;
        let clock = VerifyClock {
            source,
            deadline,
            last: AtomicU64::new(0),
        };
        let input = checked(&clock, || {
            root.open_blob_input(crypto, account, id, row, max_bytes)
                .map_err(blob_error)
        })?;
        Ok(PinnedBlobInput {
            input,
            clock,
            failed: None,
        })
    }
}
/// Retains the pooled view borrow; an error retires this input without more I/O.
pub struct PinnedBlobInput<'a, 'c, C: Crypto> {
    input: BlobInput<'a, 'c, C>,
    clock: VerifyClock<'c>,
    failed: Option<PolicyError>,
}
impl<'a, 'c, C: Crypto> PinnedBlobInput<'a, 'c, C> {
    pub fn len(&self) -> u64 {
        self.input.len()
    }
    pub fn is_empty(&self) -> bool {
        self.input.is_empty()
    }
    pub fn position(&self) -> u64 {
        self.input.position()
    }
    pub fn is_failed(&self) -> bool {
        self.failed.is_some()
    }
    /// Errors may overwrite output; no returned byte is verified until finish.
    pub fn read(&mut self, output: &mut [u8]) -> Result<usize, PolicyError> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = checked(&self.clock, || self.input.read(output).map_err(blob_error));
        if let Err(error) = result {
            self.failed = Some(error);
        }
        result
    }
    /// Observe exact EOF and digest before lending random reads of the same file.
    pub fn finish(self) -> Result<PinnedBlob<'a, 'c>, PolicyError> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let complete = checked(&self.clock, || self.input.finish().map_err(blob_error))?;
        Ok(PinnedBlob {
            complete,
            clock: self.clock,
            failed: None,
        })
    }
}
/// One checked immutable descriptor. Its lifetime retains the pooled view borrow.
pub struct PinnedBlob<'a, 'c> {
    complete: CompleteBlob<'a>,
    clock: VerifyClock<'c>,
    failed: Option<PolicyError>,
}
impl PinnedBlob<'_, '_> {
    pub fn id(&self) -> BlobId {
        self.complete.id()
    }
    pub fn kind(&self) -> BlobKind {
        self.complete.kind()
    }
    pub fn digest(&self) -> &[u8; 32] {
        self.complete.digest()
    }
    pub fn is_failed(&self) -> bool {
        self.failed.is_some()
    }
    /// Freshly fence this pin without reading bytes; failures remain terminal.
    pub fn check_deadline(&mut self) -> Result<(), PolicyError> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = checked(&self.clock, || Ok(()));
        if let Err(error) = result {
            self.failed = Some(error);
        }
        result
    }
}
impl BlobReader for PinnedBlob<'_, '_> {
    fn len(&self) -> u64 {
        self.complete.file().len()
    }
    fn read_at(&mut self, offset: u64, output: &mut [u8]) -> Result<usize, PolicyError> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = checked(&self.clock, || {
            self.complete
                .file()
                .read_at(offset, output)
                .map_err(blob_io_error)
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
        store_paths::{AccountEntry, Number},
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
    root.create_accounts_directory().unwrap();
    for entry in [
        AccountEntry::Root,
        AccountEntry::Messages,
        AccountEntry::Temporary,
        AccountEntry::Shard(BlobKind::Message, 0x44),
    ] {
        root.create_account_directory(account, entry).unwrap();
    }
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
    let mut temp = store
        .root()
        .create_temporary(account, Number::new(1).unwrap(), bytes.len() as u64)
        .unwrap();
    for chunk in bytes.chunks(super::MAX_FILE_STEP_BYTES) {
        temp.write(chunk).unwrap();
    }
    let published = temp
        .sync()
        .unwrap()
        .publish_blob(BlobKind::Message, id)
        .unwrap();
    let mut digest = td_crypto::Provider.sha256().unwrap();
    digest.update(bytes).unwrap();
    let row = Row::Blob(BlobRow {
        kind: BlobKind::Message,
        length: bytes.len() as u64,
        digest: digest.finish().unwrap(),
        created_at: 0,
    });
    let mut key = [0; 16];
    let len = Key::Blob(id).encode(&mut key).unwrap();
    let mut value = [0; 64];
    let length = row.encode(&mut value).unwrap();
    let op = Operation::put(crate::format::Table::Blobs, &key[..len], &value[..length]).unwrap();
    store
        .commit(
            &td_crypto::Provider,
            super::CommitRequest {
                account,
                expected: crate::format::Sequence::default(),
                utc_ms: 0,
                deadline,
            },
            &[op],
            &[published],
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

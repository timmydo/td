//! Stream every supplied final blob row through the private-file verifier.
use super::{BlobInput, BlobInputError, LockedRoot};
use crate::{
    format::{
        self,
        key::Key,
        row::{BlobRow, Row},
        Table,
    },
    ids::BlobId,
    ports::{self, Crypto, ReadView, ViewIdentity},
};

#[derive(Debug)]
pub enum BlobSweepError {
    View(ports::Error),
    Format(format::Error),
    Input(BlobInputError),
    ChangedView,
    RowLimit,
    ByteLimit,
    Failed,
    Incomplete,
}
impl std::fmt::Display for BlobSweepError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "blob sweep: {self:?}")
    }
}
impl std::error::Error for BlobSweepError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::View(e) => Some(e),
            Self::Format(e) => Some(e),
            Self::Input(e) => Some(e),
            _ => None,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlobSweepStep {
    Blob { id: BlobId, length: u64 },
    Opened,
    Read { bytes: usize },
    Verified { id: BlobId, length: u64 },
    Complete,
}
pub struct BlobSweep<'r, 'c, C: Crypto> {
    root: &'r LockedRoot,
    crypto: &'c C,
    identity: ViewIdentity,
    prior: Option<BlobId>,
    pending: Option<(BlobId, BlobRow)>,
    input: Option<BlobInput<'r, 'c, C>>,
    max_rows: u64,
    max_bytes: u64,
    blobs: u64,
    bytes: u64,
    complete: bool,
    failed: bool,
}
impl<'r, 'c, C: Crypto> BlobSweep<'r, 'c, C> {
    /// Caller holds authorization and a real pinned view or stopped-store barrier.
    pub const fn new(
        root: &'r LockedRoot,
        crypto: &'c C,
        identity: ViewIdentity,
        max_rows: u64,
        max_bytes: u64,
    ) -> Self {
        Self {
            root,
            crypto,
            identity,
            prior: None,
            pending: None,
            input: None,
            max_rows,
            max_bytes,
            blobs: 0,
            bytes: 0,
            complete: false,
            failed: false,
        }
    }
    pub const fn is_failed(&self) -> bool {
        self.failed
    }
    pub const fn is_complete(&self) -> bool {
        self.complete && !self.failed
    }
    /// One next, open, chunk read or selected-blob completion per call.
    /// Value scratch becomes the chunk buffer once its source row is detached.
    pub fn advance<V: ReadView + ?Sized>(
        &mut self,
        view: &mut V,
        key: &mut [u8],
        value: &mut [u8],
    ) -> Result<BlobSweepStep, BlobSweepError> {
        if self.failed {
            return Err(BlobSweepError::Failed);
        }
        self.failed = true;
        let step = self.work(view, key, value)?;
        self.failed = false;
        Ok(step)
    }
    fn work<V: ReadView + ?Sized>(
        &mut self,
        view: &mut V,
        key: &mut [u8],
        value: &mut [u8],
    ) -> Result<BlobSweepStep, BlobSweepError> {
        if view.identity() != self.identity {
            return Err(BlobSweepError::ChangedView);
        }
        if self.complete {
            return Ok(BlobSweepStep::Complete);
        }
        if let Some(input) = self.input.as_mut() {
            if input.position() < input.len() {
                if value.is_empty() {
                    return Err(BlobSweepError::Format(format::Error::OutputFull));
                }
                let read = input.read(value);
                if view.identity() != self.identity {
                    return Err(BlobSweepError::ChangedView);
                }
                let bytes = read.map_err(BlobSweepError::Input)?;
                if bytes == 0 {
                    return Err(BlobSweepError::Input(
                        std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into(),
                    ));
                }
                return Ok(BlobSweepStep::Read { bytes });
            }
            let result = self
                .input
                .take()
                .ok_or(BlobSweepError::Incomplete)?
                .finish();
            if view.identity() != self.identity {
                return Err(BlobSweepError::ChangedView);
            }
            let complete = result.map_err(BlobSweepError::Input)?;
            let id = complete.id();
            let length = complete.file().len();
            self.blobs = self
                .blobs
                .checked_add(1)
                .ok_or(BlobSweepError::Format(format::Error::Overflow))?;
            self.bytes = self
                .bytes
                .checked_add(length)
                .ok_or(BlobSweepError::Format(format::Error::Overflow))?;
            return Ok(BlobSweepStep::Verified { id, length });
        }
        if let Some((id, row)) = self.pending.take() {
            let result =
                self.root
                    .open_blob_input(self.crypto, self.identity.account, id, row, row.length);
            if view.identity() != self.identity {
                return Err(BlobSweepError::ChangedView);
            }
            self.input = Some(result.map_err(BlobSweepError::Input)?);
            return Ok(BlobSweepStep::Opened);
        }
        let after = self.prior.as_ref().map(|id| id.as_bytes().as_slice());
        let found = view.next(Table::Blobs, after, key, value);
        if view.identity() != self.identity {
            return Err(BlobSweepError::ChangedView);
        }
        let Some(record) = found.map_err(BlobSweepError::View)? else {
            self.complete = true;
            return Ok(BlobSweepStep::Complete);
        };
        let (Key::Blob(id), Row::Blob(row)) = (record.key, record.row) else {
            return Err(BlobSweepError::Format(format::Error::InvalidValue));
        };
        if self.prior.is_some_and(|prior| prior >= id)
            || record.last_change > self.identity.committed_sequence
        {
            return Err(BlobSweepError::Format(format::Error::InvalidValue));
        }
        record
            .row
            .validate_key(record.key)
            .map_err(BlobSweepError::Format)?;
        if self.blobs >= self.max_rows {
            return Err(BlobSweepError::RowLimit);
        }
        let remaining = self
            .max_bytes
            .checked_sub(self.bytes)
            .ok_or(BlobSweepError::ByteLimit)?;
        if row.length > remaining {
            return Err(BlobSweepError::ByteLimit);
        }
        self.prior = Some(id);
        self.pending = Some((id, row));
        Ok(BlobSweepStep::Blob {
            id,
            length: row.length,
        })
    }
    pub fn finish(self) -> Result<CompleteBlobSweep, BlobSweepError> {
        if self.failed {
            return Err(BlobSweepError::Failed);
        }
        if !self.complete {
            return Err(BlobSweepError::Incomplete);
        }
        Ok(CompleteBlobSweep {
            identity: self.identity,
            blobs: self.blobs,
            bytes: self.bytes,
        })
    }
}
/// Checks supplied rows/files only; physical view completeness and pins stay external.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompleteBlobSweep {
    identity: ViewIdentity,
    blobs: u64,
    bytes: u64,
}
impl CompleteBlobSweep {
    pub const fn identity(self) -> ViewIdentity {
        self.identity
    }
    pub const fn blobs(self) -> u64 {
        self.blobs
    }
    pub const fn bytes(self) -> u64 {
        self.bytes
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
pub(super) fn prepare_probe(root: &LockedRoot) {
    use crate::{
        format::row::BlobKind,
        store_paths::{AccountEntry, Number},
    };
    for (byte, contents) in [(0xec, b"".as_slice()), (0xed, b"abc".as_slice())] {
        root.create_account_directory(tests::ACCOUNT, AccountEntry::Shard(BlobKind::Message, byte))
            .unwrap();
        let mut file = root
            .create_temporary(
                tests::ACCOUNT,
                Number::new(500 + u64::from(byte)).unwrap(),
                contents.len() as u64,
            )
            .unwrap();
        if !contents.is_empty() {
            file.write(contents).unwrap();
        }
        file.sync()
            .unwrap()
            .publish_blob(BlobKind::Message, tests::id(byte))
            .unwrap();
    }
}
#[cfg(test)]
pub(super) use tests::probe;
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::super::tests::Fixture;
    use super::*;
    use crate::{
        format::{row::BlobKind, ObjectType, Sequence},
        ids::{AccountId, StoreEpoch},
        ports::{ChangeCursor, ChangeStep, Digest, Record},
        store_paths::AccountEntry,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use td_crypto::Provider;
    pub(super) const ACCOUNT: AccountId = AccountId::from_bytes([0x42; 16]);
    pub(super) fn id(byte: u8) -> BlobId {
        BlobId::from_bytes([byte; 16])
    }
    fn identity() -> ViewIdentity {
        ViewIdentity {
            account: ACCOUNT,
            epoch: StoreEpoch::from_bytes([2; 16]),
            generation: 1,
            checkpoint: Sequence::from_u64(1),
            segment: 2,
            committed_offset: 256,
            committed_sequence: Sequence::from_u64(2),
            history_floor: Sequence::default(),
        }
    }
    fn row(bytes: &[u8]) -> BlobRow {
        let mut hash = Provider.sha256().unwrap();
        hash.update(bytes).unwrap();
        BlobRow {
            kind: BlobKind::Message,
            length: bytes.len() as u64,
            digest: hash.finish().unwrap(),
            created_at: 0,
        }
    }
    fn rows() -> [(u8, BlobRow); 3] {
        [(0xec, row(b"")), (0xed, row(b"abc")), (0xee, row(b"abc"))]
    }
    struct View<'a> {
        identity: ViewIdentity,
        rows: &'a [(u8, BlobRow)],
        nexts: usize,
        identity_calls: AtomicUsize,
        flip_at: usize,
        error: bool,
        absent: bool,
        bad_key: bool,
        bad_row: bool,
        future: bool,
        forced: Option<usize>,
    }
    impl<'a> View<'a> {
        fn new(rows: &'a [(u8, BlobRow)]) -> Self {
            Self {
                identity: identity(),
                rows,
                nexts: 0,
                identity_calls: AtomicUsize::new(0),
                flip_at: usize::MAX,
                error: false,
                absent: false,
                bad_key: false,
                bad_row: false,
                future: false,
                forced: None,
            }
        }
        fn flip_during_step(&mut self) {
            self.flip_at = self.identity_calls.load(Ordering::Relaxed) + 1;
        }
    }
    impl ReadView for View<'_> {
        fn identity(&self) -> ViewIdentity {
            let mut result = self.identity;
            if self.identity_calls.fetch_add(1, Ordering::Relaxed) >= self.flip_at {
                result.generation += 1;
            }
            result
        }
        fn next_change(
            &mut self,
            _: ChangeCursor,
            _: ObjectType,
        ) -> Result<ChangeStep, ports::Error> {
            Err(ports::Error::Invalid)
        }
        fn get<'a>(
            &mut self,
            _: Key<'_>,
            _: &'a mut [u8],
        ) -> Result<Option<(Row<'a>, Sequence)>, ports::Error> {
            Err(ports::Error::Invalid)
        }
        fn next<'a>(
            &mut self,
            table: Table,
            after: Option<&[u8]>,
            key: &'a mut [u8],
            value: &'a mut [u8],
        ) -> Result<Option<Record<'a>>, ports::Error> {
            assert_eq!(table, Table::Blobs);
            self.nexts += 1;
            if self.error {
                return Err(ports::Error::Capacity);
            }
            if self.absent {
                return Ok(None);
            }
            for (index, &(byte, blob)) in self.rows.iter().enumerate() {
                let identifier = id(byte);
                if let Some(forced) = self.forced {
                    if forced != index {
                        continue;
                    }
                } else if after.is_some_and(|prior| prior >= identifier.as_bytes()) {
                    continue;
                }
                let len = Key::Blob(identifier)
                    .encode(key)
                    .map_err(|_| ports::Error::Capacity)?;
                let source = if self.bad_key {
                    Key::Thread(crate::ids::ThreadId::from_bytes([1; 16]))
                } else {
                    Key::decode(table, &key[..len]).unwrap()
                };
                let row = if self.bad_row {
                    Row::Thread
                } else {
                    Row::Blob(blob)
                };
                let len = row.encode(value).map_err(|_| ports::Error::Capacity)?;
                return Ok(Some(Record {
                    key: source,
                    row: Row::decode(row.table(), &value[..len]).unwrap(),
                    last_change: Sequence::from_u64(if self.future { 3 } else { 2 }),
                }));
            }
            Ok(None)
        }
    }
    fn setup(fixture: &Fixture) -> LockedRoot {
        let root = fixture.locked();
        root.create_accounts_directory().unwrap();
        for entry in [
            AccountEntry::Root,
            AccountEntry::Temporary,
            AccountEntry::Messages,
        ] {
            root.create_account_directory(ACCOUNT, entry).unwrap();
        }
        super::super::blob::prepare_probe(&root);
        prepare_probe(&root);
        root
    }
    pub(crate) fn probe(root: &LockedRoot) {
        let rows = rows();
        for supplied in [&rows[..], &[][..]] {
            let mut view = View::new(supplied);
            let mut sweep = BlobSweep::new(root, &Provider, identity(), supplied.len() as u64, 6);
            let mut verified = 0;
            for _ in 0..16 {
                match sweep
                    .advance(&mut view, &mut [0; 16], &mut [0; 128])
                    .unwrap()
                {
                    BlobSweepStep::Verified { .. } => verified += 1,
                    BlobSweepStep::Complete => break,
                    _ => {}
                }
            }
            assert!(sweep.is_complete());
            assert_eq!(verified, supplied.len());
            assert_eq!(view.nexts, supplied.len() + 1);
            assert_eq!(
                sweep.advance(&mut view, &mut [], &mut []).unwrap(),
                BlobSweepStep::Complete
            );
            assert_eq!(view.nexts, supplied.len() + 1);
            let done = sweep.finish().unwrap();
            assert_eq!(done.identity(), identity());
            assert_eq!(done.blobs(), supplied.len() as u64);
            assert_eq!(done.bytes(), if supplied.is_empty() { 0 } else { 6 });
        }
        for (max_rows, max_bytes, row_error) in [(2, 6, true), (3, 5, false)] {
            let mut view = View::new(&rows);
            let mut sweep = BlobSweep::new(root, &Provider, identity(), max_rows, max_bytes);
            let mut failed = false;
            for _ in 0..16 {
                if let Err(e) = sweep.advance(&mut view, &mut [0; 16], &mut [0; 128]) {
                    assert!(if row_error {
                        matches!(e, BlobSweepError::RowLimit)
                    } else {
                        matches!(e, BlobSweepError::ByteLimit)
                    });
                    failed = true;
                    break;
                }
            }
            assert!(failed);
            assert!(sweep.is_failed());
            assert!(matches!(sweep.finish(), Err(BlobSweepError::Failed)));
        }
    }
    #[test]
    fn complete_files_empty_blobs_and_total_admission() {
        let fixture = Fixture::new();
        let root = setup(&fixture);
        probe(&root);
        assert!(std::mem::size_of::<BlobSweep<'_, '_, Provider>>() <= 1024);
        assert!(matches!(
            BlobSweep::new(&root, &Provider, identity(), 0, 0).finish(),
            Err(BlobSweepError::Incomplete)
        ));
    }
    #[test]
    fn chunks_and_digest_completion_cannot_be_skipped() {
        let fixture = Fixture::new();
        let root = setup(&fixture);
        let mut rows = rows();
        rows[2].1.digest[0] ^= 1;
        let mut view = View::new(&rows[2..]);
        let mut sweep = BlobSweep::new(&root, &Provider, identity(), 1, 3);
        assert!(matches!(
            sweep
                .advance(&mut view, &mut [0; 16], &mut [0; 128])
                .unwrap(),
            BlobSweepStep::Blob { .. }
        ));
        assert_eq!(
            sweep.advance(&mut view, &mut [], &mut []).unwrap(),
            BlobSweepStep::Opened
        );
        for _ in 0..3 {
            assert_eq!(
                sweep.advance(&mut view, &mut [], &mut [0; 1]).unwrap(),
                BlobSweepStep::Read { bytes: 1 }
            );
        }
        assert!(!sweep.is_complete());
        assert!(matches!(
            sweep.advance(&mut view, &mut [], &mut []),
            Err(BlobSweepError::Input(BlobInputError::Checksum))
        ));
        assert!(matches!(
            sweep.advance(&mut view, &mut [], &mut []),
            Err(BlobSweepError::Failed)
        ));
        assert!(matches!(sweep.finish(), Err(BlobSweepError::Failed)));
        let mut view = View::new(&rows[1..2]);
        let mut sweep = BlobSweep::new(&root, &Provider, identity(), 1, 3);
        sweep
            .advance(&mut view, &mut [0; 16], &mut [0; 128])
            .unwrap();
        sweep.advance(&mut view, &mut [], &mut []).unwrap();
        assert!(matches!(
            sweep.advance(&mut view, &mut [], &mut []),
            Err(BlobSweepError::Format(format::Error::OutputFull))
        ));
        assert!(matches!(sweep.finish(), Err(BlobSweepError::Failed)));
    }
    #[test]
    fn large_value_scratch_still_reads_only_one_bounded_chunk() {
        use crate::store_paths::Number;
        let fixture = Fixture::new();
        let root = setup(&fixture);
        let data = vec![0x5a; super::super::MAX_FILE_STEP_BYTES + 1];
        root.create_account_directory(ACCOUNT, AccountEntry::Shard(BlobKind::Message, 0xef))
            .unwrap();
        let mut file = root
            .create_temporary(ACCOUNT, Number::new(999).unwrap(), data.len() as u64)
            .unwrap();
        for chunk in data.chunks(super::super::MAX_FILE_STEP_BYTES) {
            file.write(chunk).unwrap();
        }
        file.sync()
            .unwrap()
            .publish_blob(BlobKind::Message, id(0xef))
            .unwrap();
        let rows = [(0xef, row(&data))];
        let mut view = View::new(&rows);
        let mut sweep = BlobSweep::new(&root, &Provider, identity(), 1, data.len() as u64);
        let mut value = vec![0; data.len() * 2];
        assert!(matches!(
            sweep.advance(&mut view, &mut [0; 16], &mut value).unwrap(),
            BlobSweepStep::Blob { .. }
        ));
        assert_eq!(
            sweep.advance(&mut view, &mut [], &mut []).unwrap(),
            BlobSweepStep::Opened
        );
        assert_eq!(
            sweep.advance(&mut view, &mut [], &mut value).unwrap(),
            BlobSweepStep::Read { bytes: 65536 }
        );
        assert_eq!(
            sweep.advance(&mut view, &mut [], &mut value).unwrap(),
            BlobSweepStep::Read { bytes: 1 }
        );
        assert_eq!(view.nexts, 1);
        assert_eq!(
            sweep.advance(&mut view, &mut [], &mut []).unwrap(),
            BlobSweepStep::Verified {
                id: id(0xef),
                length: 65537
            }
        );
        assert!(!sweep.is_complete());
        assert_eq!(
            sweep.advance(&mut view, &mut [0; 16], &mut value).unwrap(),
            BlobSweepStep::Complete
        );
        assert_eq!(sweep.finish().unwrap().bytes(), 65537);
    }
    #[test]
    fn movement_precedes_every_phase_and_lookup_outcome() {
        let fixture = Fixture::new();
        let root = setup(&fixture);
        let rows = rows();
        for phase in 0..4 {
            let mut view = View::new(&rows[1..2]);
            let mut sweep = BlobSweep::new(&root, &Provider, identity(), 1, 3);
            for _ in 0..phase {
                sweep
                    .advance(&mut view, &mut [0; 16], &mut [0; 128])
                    .unwrap();
            }
            view.flip_during_step();
            assert!(matches!(
                sweep.advance(&mut view, &mut [0; 16], &mut [0; 128]),
                Err(BlobSweepError::ChangedView)
            ));
            assert!(matches!(sweep.finish(), Err(BlobSweepError::Failed)));
        }
        for absent in [false, true] {
            let mut view = View::new(&rows);
            view.absent = absent;
            view.error = !absent;
            view.flip_during_step();
            let mut sweep = BlobSweep::new(&root, &Provider, identity(), 3, 6);
            assert!(matches!(
                sweep.advance(&mut view, &mut [0; 16], &mut [0; 128]),
                Err(BlobSweepError::ChangedView)
            ));
        }
        let mut view = View::new(&[]);
        let mut sweep = BlobSweep::new(&root, &Provider, identity(), 0, 0);
        assert_eq!(
            sweep.advance(&mut view, &mut [], &mut []).unwrap(),
            BlobSweepStep::Complete
        );
        view.identity.generation += 1;
        assert!(matches!(
            sweep.advance(&mut view, &mut [], &mut []),
            Err(BlobSweepError::ChangedView)
        ));
        assert_eq!(view.nexts, 1);
    }
    #[test]
    fn file_errors_lose_to_movement_during_open_read_and_finish() {
        use crate::store_paths::Name;
        for phase in 0..3 {
            let fixture = Fixture::new();
            let root = setup(&fixture);
            let mut descriptor = row(b"abc");
            if phase == 2 {
                descriptor.digest[0] ^= 1;
            }
            let identifier = if phase == 0 { 0xef } else { 0xed };
            let rows = [(identifier, descriptor)];
            let mut view = View::new(&rows);
            let mut sweep = BlobSweep::new(&root, &Provider, identity(), 1, 3);
            for _ in 0..=phase {
                sweep
                    .advance(&mut view, &mut [0; 16], &mut [0; 128])
                    .unwrap();
            }
            if phase == 1 {
                let name = Name::account(
                    ACCOUNT,
                    AccountEntry::Blob(BlobKind::Message, id(identifier)),
                )
                .unwrap();
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(fixture.path.join(name.as_path().unwrap()))
                    .unwrap()
                    .set_len(0)
                    .unwrap();
            }
            view.flip_during_step();
            assert!(matches!(
                sweep.advance(&mut view, &mut [0; 16], &mut [0; 128]),
                Err(BlobSweepError::ChangedView)
            ));
            assert!(matches!(
                sweep.advance(&mut view, &mut [], &mut []),
                Err(BlobSweepError::Failed)
            ));
            assert!(matches!(sweep.finish(), Err(BlobSweepError::Failed)));
        }
    }
    #[test]
    fn malformed_sources_order_missing_files_and_view_errors_retire() {
        let fixture = Fixture::new();
        let root = setup(&fixture);
        let rows = rows();
        for mode in 0..4 {
            let mut view = View::new(&rows);
            view.bad_key = mode == 0;
            view.bad_row = mode == 1;
            view.future = mode == 2;
            view.error = mode == 3;
            let mut sweep = BlobSweep::new(&root, &Provider, identity(), 0, 0);
            let error = sweep
                .advance(&mut view, &mut [0; 16], &mut [0; 128])
                .unwrap_err();
            assert!(if mode == 3 {
                matches!(error, BlobSweepError::View(ports::Error::Capacity))
            } else {
                matches!(error, BlobSweepError::Format(format::Error::InvalidValue))
            });
            assert!(matches!(sweep.finish(), Err(BlobSweepError::Failed)));
        }
        for lower in [false, true] {
            let mut view = View::new(&rows);
            view.forced = Some(1);
            let mut sweep = BlobSweep::new(&root, &Provider, identity(), 1, 3);
            for _ in 0..4 {
                sweep
                    .advance(&mut view, &mut [0; 16], &mut [0; 128])
                    .unwrap();
            }
            if lower {
                view.forced = Some(0);
            }
            assert!(matches!(
                sweep.advance(&mut view, &mut [0; 16], &mut [0; 128]),
                Err(BlobSweepError::Format(format::Error::InvalidValue))
            ));
        }
        let missing = [(0xef, row(b"abc"))];
        let mut view = View::new(&missing);
        let mut sweep = BlobSweep::new(&root, &Provider, identity(), 1, 3);
        sweep
            .advance(&mut view, &mut [0; 16], &mut [0; 128])
            .unwrap();
        assert!(matches!(
            sweep.advance(&mut view, &mut [], &mut []),
            Err(BlobSweepError::Input(BlobInputError::Io(_)))
        ));
        assert!(matches!(sweep.finish(), Err(BlobSweepError::Failed)));
    }
}

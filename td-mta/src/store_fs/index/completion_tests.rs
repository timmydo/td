#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{
    format::row::BlobRow,
    ports::{Crypto, Tick, Time},
    store_fs::tests::Fixture,
};
use std::{
    collections::VecDeque,
    io::Cursor,
    sync::atomic::{AtomicU64, Ordering},
};
const ACCOUNT: AccountId = AccountId::from_bytes([1; 16]);
const BODY: BlobId = BlobId::from_bytes([2; 16]);
const EPOCH: StoreEpoch = StoreEpoch::from_bytes([9; 16]);
const RAW: &[u8] = b"prepared upload";
struct Timer {
    now: AtomicU64,
    handoff: Mutex<VecDeque<u64>>,
}
impl Timer {
    fn new() -> Self {
        Self {
            now: AtomicU64::new(1),
            handoff: Mutex::new(VecDeque::new()),
        }
    }
}
impl Clock for Timer {
    fn sample(&self) -> Result<Time, ports::Error> {
        if let Some(next) = lock(&self.handoff)?.pop_front() {
            self.now.store(next, Ordering::Relaxed);
        }
        Ok(Time {
            utc_ms: 0,
            monotonic: Tick(self.now.load(Ordering::Relaxed)),
        })
    }
}
fn deadline() -> Deadline {
    Deadline::after(Tick(0), 100).unwrap()
}
fn request(expected: u64) -> CommitRequest {
    CommitRequest {
        account: ACCOUNT,
        epoch: EPOCH,
        expected: Sequence::from_u64(expected),
        utc_ms: 0,
        deadline: deadline(),
    }
}
fn row(bytes: &[u8]) -> Vec<u8> {
    let mut digest = td_crypto::Provider.sha256().unwrap();
    digest.update(bytes).unwrap();
    let row = Row::Blob(BlobRow {
        length: bytes.len() as u64,
        digest: digest.finish().unwrap(),
        created_at: 0,
    });
    let mut output = vec![0; 64];
    let length = row.encode(&mut output).unwrap();
    output.truncate(length);
    output
}
fn run(
    store: &IndexStore<'_>,
    request: CommitRequest,
    operations: &[Operation<'_>],
    source: &mut Cursor<&[u8]>,
    encoded: bool,
) -> CommitCompletion {
    let mut sources = [BlobSource { id: BODY, source }];
    if !encoded {
        return store.commit_with_files(&td_crypto::Provider, request, operations, &mut sources);
    }
    let mut bytes = vec![0; 1024];
    let mut length = 0;
    for operation in operations {
        length += operation.encode(&mut bytes[length..]).unwrap();
    }
    bytes.truncate(length);
    let mut slots = vec![None; operations.len()];
    let batch = crate::format::batch::Batch::decode(
        ports::TransactionInput {
            bytes: &bytes,
            count: operations.len(),
        },
        &mut slots,
    )
    .unwrap();
    store.commit_batch_with_files(&td_crypto::Provider, request, &batch, &mut sources)
}
fn actual_files(store: &IndexStore<'_>) -> StoreFileUsage {
    StoreFileUsage {
        database_bytes: fs::metadata(db_path(store.root, RootEntry::Database).unwrap())
            .unwrap()
            .len(),
        wal_bytes: match fs::metadata(db_path(store.root, RootEntry::Wal).unwrap()) {
            Ok(metadata) => metadata.len(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => panic!("independent WAL metadata: {error}"),
        },
    }
}
fn inspect(store: &IndexStore<'_>, expected: u64, bytes: Option<&[u8]>) {
    let mut view = store
        .view(ACCOUNT, Deadline::after(Tick(0), 200).unwrap())
        .unwrap();
    assert_eq!(
        view.identity().committed_sequence,
        Sequence::from_u64(expected)
    );
    let mut metadata_bytes = [0; 64];
    let metadata = view.get(Key::Blob(BODY), &mut metadata_bytes).unwrap();
    if let Some(bytes) = bytes {
        assert!(metadata.is_some());
        let mut input = view
            .open_blob_input(&td_crypto::Provider, BODY, bytes.len() as u64)
            .unwrap();
        let mut output = vec![0; bytes.len()];
        assert_eq!(input.read(&mut output).unwrap(), bytes.len());
        assert_eq!(output, bytes);
        input.finish().unwrap();
    } else {
        assert!(metadata.is_none());
    }
}

#[test]
fn both_commit_forms_retain_outcome_and_exact_file_extents_through_reopen() {
    for encoded in [false, true] {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let timer = Arc::new(Timer::new());
        let store = IndexStore::create(&mut root, EPOCH, timer.clone(), 1, deadline()).unwrap();
        store.create_account(ACCOUNT, deadline()).unwrap();
        let value = row(RAW);
        let operations = [Operation::put(Table::Blobs, BODY.as_bytes(), &value).unwrap()];
        let complete = run(
            &store,
            request(0),
            &operations,
            &mut Cursor::new(RAW),
            encoded,
        );
        assert_eq!(complete.outcome(), Ok(Sequence::from_u64(1)));
        assert_eq!(
            complete.files(),
            CommitFileUsage::Measured(actual_files(&store))
        );
        assert!(!complete.writer_stopped());
        assert!(!complete.is_sequence_conflict());
        inspect(&store, 1, Some(RAW));
        store.checkpoint(deadline()).unwrap();
        drop(store);
        let store = IndexStore::open(&mut root, timer, 1, deadline()).unwrap();
        store.validate_integrity(deadline()).unwrap();
        inspect(&store, 1, Some(RAW));
    }
}

#[test]
fn completion_observer_runs_before_the_writer_fence_is_released() {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let store =
        IndexStore::create(&mut root, EPOCH, Arc::new(Timer::new()), 1, deadline()).unwrap();
    store.create_account(ACCOUNT, deadline()).unwrap();
    let value = row(RAW);
    let operations = [Operation::put(Table::Blobs, BODY.as_bytes(), &value).unwrap()];
    let mut source = Cursor::new(RAW);
    let complete = store.commit_operations_observed(
        &td_crypto::Provider,
        request(0),
        Operations::Typed(&operations),
        &mut [BlobSource {
            id: BODY,
            source: &mut source,
        }],
        |result, phase, writer| {
            assert!(matches!(
                store.writer.try_lock(),
                Err(TryLockError::WouldBlock)
            ));
            assert!(matches!(
                store.usage_fence(deadline()),
                Err(ports::Error::Busy)
            ));
            CommitCompletion::capture(result, phase, writer, store.root)
        },
    );
    assert_eq!(complete.outcome(), Ok(Sequence::from_u64(1)));
    assert_eq!(
        complete.files(),
        CommitFileUsage::Measured(actual_files(&store))
    );
    assert!(store.writer.try_lock().is_ok());
}

#[test]
fn pre_sql_epoch_and_deadline_refusals_prove_unchanged_without_body_reads() {
    for encoded in [false, true] {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let timer = Arc::new(Timer::new());
        let store = IndexStore::create(&mut root, EPOCH, timer.clone(), 1, deadline()).unwrap();
        store.create_account(ACCOUNT, deadline()).unwrap();
        let before = actual_files(&store);
        let value = row(RAW);
        let operations = [Operation::put(Table::Blobs, BODY.as_bytes(), &value).unwrap()];
        let mut source = Cursor::new(RAW);
        let complete = run(
            &store,
            CommitRequest {
                epoch: StoreEpoch::from_bytes([10; 16]),
                ..request(0)
            },
            &operations,
            &mut source,
            encoded,
        );
        assert_eq!(
            complete.outcome(),
            Err(CommitError::Rejected(ports::Error::Conflict))
        );
        assert_eq!(complete.files(), CommitFileUsage::Unchanged);
        assert!(!complete.is_sequence_conflict());
        *timer.handoff.lock().unwrap() = [1, 100].into();
        let complete = run(&store, request(0), &operations, &mut source, encoded);
        assert_eq!(
            complete.outcome(),
            Err(CommitError::Rejected(ports::Error::Deadline))
        );
        assert_eq!(complete.files(), CommitFileUsage::Unchanged);
        assert!(!complete.writer_stopped());
        assert_eq!(source.position(), 0);
        assert_eq!(actual_files(&store), before);
        inspect(&store, 0, None);
        let complete = run(
            &store,
            CommitRequest {
                deadline: Deadline::after(Tick(0), 200).unwrap(),
                ..request(0)
            },
            &operations,
            &mut source,
            encoded,
        );
        assert_eq!(complete.outcome(), Ok(Sequence::from_u64(1)));
        assert!(matches!(complete.files(), CommitFileUsage::Measured(_)));
    }
}

#[test]
fn only_an_accounted_early_endpoint_conflict_allows_replanning() {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let store =
        IndexStore::create(&mut root, EPOCH, Arc::new(Timer::new()), 1, deadline()).unwrap();
    store.create_account(ACCOUNT, deadline()).unwrap();
    let value = row(RAW);
    let operations = [Operation::put(Table::Blobs, BODY.as_bytes(), &value).unwrap()];
    let mut source = Cursor::new(RAW);
    let complete = run(&store, request(1), &operations, &mut source, false);
    assert_eq!(
        complete.outcome(),
        Err(CommitError::Rejected(ports::Error::Conflict))
    );
    assert!(complete.is_sequence_conflict());
    assert_eq!(
        complete.files(),
        CommitFileUsage::Measured(actual_files(&store))
    );
    assert_eq!(source.position(), 0);
    assert_eq!(
        run(&store, request(0), &operations, &mut source, false).outcome(),
        Ok(Sequence::from_u64(1))
    );
    store
        .commit(
            &td_crypto::Provider,
            request(1),
            &[Operation::delete(Table::Blobs, BODY.as_bytes()).unwrap()],
            &mut [],
        )
        .unwrap();
    let mut source = Cursor::new(RAW);
    let complete = run(&store, request(2), &operations, &mut source, false);
    assert_eq!(
        complete.outcome(),
        Err(CommitError::Rejected(ports::Error::Conflict))
    );
    assert!(!complete.is_sequence_conflict());
    assert!(matches!(complete.files(), CommitFileUsage::Measured(_)));
    assert_eq!(source.position(), 0);
    inspect(&store, 2, None);
}

#[test]
fn rolled_back_body_writes_retain_real_wal_growth_in_the_completion() {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let store =
        IndexStore::create(&mut root, EPOCH, Arc::new(Timer::new()), 1, deadline()).unwrap();
    store.create_account(ACCOUNT, deadline()).unwrap();
    store.checkpoint(deadline()).unwrap();
    let before = actual_files(&store);
    let body = vec![0x53; 2 * 1024 * 1024];
    let mut value = row(&body);
    // Corrupt only the declared digest, retaining valid length and row framing.
    let Row::Blob(mut blob) = Row::decode(Table::Blobs, &value).unwrap() else {
        panic!("blob")
    };
    blob.digest[0] ^= 1;
    let length = Row::Blob(blob).encode(&mut value).unwrap();
    value.truncate(length);
    let operations = [Operation::put(Table::Blobs, BODY.as_bytes(), &value).unwrap()];
    let complete = run(
        &store,
        request(0),
        &operations,
        &mut Cursor::new(body.as_slice()),
        false,
    );
    assert_eq!(
        complete.outcome(),
        Err(CommitError::Rejected(ports::Error::Corrupt))
    );
    let after = actual_files(&store);
    assert!(
        after.wal_bytes > before.wal_bytes,
        "{before:?} -> {after:?}"
    );
    assert_eq!(complete.files(), CommitFileUsage::Measured(after));
    assert!(!complete.writer_stopped());
    assert!(!complete.is_sequence_conflict());
    inspect(&store, 0, None);
}

#[test]
fn late_success_and_indeterminate_commit_keep_their_distinct_outcomes() {
    for deny in [false, true] {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let timer = Arc::new(Timer::new());
        let store = IndexStore::create(&mut root, EPOCH, timer.clone(), 1, deadline()).unwrap();
        store.create_account(ACCOUNT, deadline()).unwrap();
        let tick = timer.clone();
        lock(&lock(&store.writer).unwrap().native.connection)
            .unwrap()
            .authorizer(Some(move |context: AuthContext<'_>| {
                if matches!(
                    context.action,
                    AuthAction::Transaction { operation }
                    if !matches!(operation, TransactionOperation::Begin | TransactionOperation::Rollback)
                ) {
                    if deny {
                        return Authorization::Deny;
                    }
                    tick.now.store(100, Ordering::Relaxed);
                }
                Authorization::Allow
            }))
            .unwrap();
        let value = row(RAW);
        let operations = [Operation::put(Table::Blobs, BODY.as_bytes(), &value).unwrap()];
        let complete = run(
            &store,
            request(0),
            &operations,
            &mut Cursor::new(RAW),
            false,
        );
        if deny {
            assert!(matches!(
                complete.outcome(),
                Err(CommitError::Indeterminate(_))
            ));
            assert!(complete.writer_stopped());
            assert_eq!(
                complete.files(),
                CommitFileUsage::Measured(actual_files(&store))
            );
        } else {
            assert_eq!(complete.outcome(), Ok(Sequence::from_u64(1)));
            assert_eq!(
                complete.files(),
                CommitFileUsage::Unavailable(ports::Error::Deadline)
            );
            assert!(!complete.writer_stopped());
        }
        assert!(!complete.is_sequence_conflict());
        drop(store);
        let store =
            IndexStore::open(&mut root, timer, 1, Deadline::after(Tick(0), 200).unwrap()).unwrap();
        store
            .validate_integrity(Deadline::after(Tick(0), 200).unwrap())
            .unwrap();
        inspect(&store, u64::from(!deny), (!deny).then_some(RAW));
    }
}

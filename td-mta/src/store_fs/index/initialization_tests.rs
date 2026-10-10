#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{
    admission::{
        logical::{Cell, Error as LeaseError},
        quota::Kind,
        DiskLimits, Plan, ViewMode, WorkLimits,
    },
    format::row::{BlobRow, LeaseRow, LeaseUse, NotificationState, SubmissionRow},
    ids::{DeviceId, EmailId, IdentityId, SubmissionId, ThreadId},
    ownership::SlotState,
    ports::{Crypto, Tick, Time},
    store_fs::{index::recipient_tests::queued, tests::Fixture},
};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
const ACCOUNT: AccountId = AccountId::from_bytes([1; 16]);
const BODY: BlobId = BlobId::from_bytes([2; 16]);
const UPLOAD: BlobId = BlobId::from_bytes([3; 16]);
const SUBMISSION: SubmissionId = SubmissionId::from_bytes([4; 16]);
struct Timer(AtomicU64, AtomicBool);
impl Timer {
    fn new() -> Self {
        Self(AtomicU64::new(1), AtomicBool::new(false))
    }
}
impl Clock for Timer {
    fn sample(&self) -> Result<Time, ports::Error> {
        Ok(Time {
            utc_ms: 0,
            monotonic: Tick(if self.1.load(Ordering::Relaxed) {
                self.0.fetch_add(1, Ordering::Relaxed)
            } else {
                self.0.load(Ordering::Relaxed)
            }),
        })
    }
}
fn deadline() -> Deadline {
    Deadline::after(Tick(0), 100).unwrap()
}
fn plan(blobs: u64) -> Plan {
    DiskLimits {
        blob_count: blobs,
        ..DiskLimits::default()
    }
    .plan(
        &crate::limits::Limits::default().plan().unwrap(),
        WorkLimits::default(),
        ViewMode::OnlineBackground,
    )
    .unwrap()
}
fn auxiliary() -> AuxiliaryUsage {
    AuxiliaryUsage {
        sort_bytes: 17,
        response_bytes: 19,
        cache_bytes: 23,
        log_bytes: 29,
        cold_bytes: 31,
    }
}
fn blob(bytes: &[u8]) -> Row<'static> {
    let mut hash = td_crypto::Provider.sha256().unwrap();
    hash.update(bytes).unwrap();
    Row::Blob(BlobRow {
        length: bytes.len() as u64,
        digest: hash.finish().unwrap(),
        created_at: 0,
    })
}
fn open(root: &mut LockedRoot, clock: Arc<Timer>) -> IndexStore<'_> {
    let store =
        IndexStore::create(root, StoreEpoch::from_bytes([5; 16]), clock, 2, deadline()).unwrap();
    store.create_account(ACCOUNT, deadline()).unwrap();
    let mut message = b"queue".as_slice();
    let mut upload = b"uploaded".as_slice();
    let rows = [
        (Key::Blob(BODY), blob(message)),
        (Key::Blob(UPLOAD), blob(upload)),
        (
            Key::Lease(UPLOAD),
            Row::Lease(LeaseRow {
                account: ACCOUNT,
                device: DeviceId::from_bytes([6; 16]),
                expires_at: -1,
                uses: LeaseUse::Both,
            }),
        ),
        (
            Key::Submission(SUBMISSION),
            Row::Submission(SubmissionRow {
                email: EmailId::from_bytes([7; 16]),
                thread: ThreadId::from_bytes([8; 16]),
                identity: IdentityId::from_bytes([9; 16]),
                transmitted_blob: BODY,
                reverse_path: "",
                send_at: 0,
                expires_at: 432_000_000,
                recipient_count: 1,
                completed_at: None,
                notification: NotificationState::None,
                notification_email: None,
            }),
        ),
        (Key::Recipient(SUBMISSION, 0), Row::Recipient(queued())),
    ];
    let values: Vec<_> = rows
        .iter()
        .map(|(key, row)| {
            let mut k = vec![0; 1024];
            let n = key.encode(&mut k).unwrap();
            k.truncate(n);
            let mut v = vec![0; 65536];
            let n = row.encode(&mut v).unwrap();
            v.truncate(n);
            (key.table(), k, v)
        })
        .collect();
    let operations: Vec<_> = values
        .iter()
        .map(|(table, key, value)| Operation::put(*table, key, value).unwrap())
        .collect();
    store
        .commit(
            &td_crypto::Provider,
            CommitRequest {
                account: ACCOUNT,
                epoch: store.epoch(),
                expected: Sequence::default(),
                utc_ms: 0,
                deadline: deadline(),
            },
            &operations,
            &mut [
                BlobSource {
                    id: BODY,
                    source: &mut message,
                },
                BlobSource {
                    id: UPLOAD,
                    source: &mut upload,
                },
            ],
        )
        .unwrap();
    store
}
#[test]
fn captured_logical_files_and_auxiliary_usage_initialize_every_ledger_bucket() {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let store = open(&mut root, Arc::new(Timer::new()));
    let fence = store.usage_fence(deadline()).unwrap();
    let files = fence.file_usage();
    assert_eq!(
        files.database_bytes,
        fs::symlink_metadata(db_path(store.root(), RootEntry::Database).unwrap())
            .unwrap()
            .len()
    );
    assert_eq!(
        files.wal_bytes,
        fs::symlink_metadata(db_path(store.root(), RootEntry::Wal).unwrap())
            .unwrap()
            .len()
    );
    assert!(files.database_bytes > 0);
    assert!(files.wal_bytes > 0);
    let mut states = [const { SlotState::EMPTY }; 2];
    let mut cells = [const { Cell::EMPTY }; 2];
    let ledger = fence
        .initialize_leases(&plan(100), auxiliary(), &mut states, &mut cells)
        .unwrap();
    for (kind, expected) in [
        (Kind::BodyBytes, 13),
        (Kind::BlobCount, 2),
        (Kind::UploadBytes, 8),
        (Kind::QueueBytes, 5),
        (Kind::QueueSubmissions, 1),
        (Kind::DatabaseBytes, files.database_bytes),
        (Kind::WalBytes, files.wal_bytes),
        (Kind::SortBytes, 17),
        (Kind::ResponseBytes, 19),
        (Kind::CacheBytes, 23),
        (Kind::LogBytes, 29),
        (Kind::ColdBytes, 31),
    ] {
        assert_eq!(ledger.used(kind).unwrap(), expected, "{kind:?}");
        assert_eq!(ledger.pending(kind).unwrap(), 0, "{kind:?}");
    }
    assert_eq!(ledger.available_cells(), 2);
    drop(store.usage_fence(deadline()).unwrap());
    drop(store.view(ACCOUNT, deadline()).unwrap());
}
#[test]
fn quota_and_expiry_refusal_release_the_fence_without_occupying_cells() {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let clock = Arc::new(Timer::new());
    let store = open(&mut root, clock.clone());
    let mut states = [const { SlotState::EMPTY }; 2];
    let mut cells = [const { Cell::EMPTY }; 2];
    assert_eq!(
        store
            .usage_fence(deadline())
            .unwrap()
            .initialize_leases(&plan(1), auxiliary(), &mut states, &mut cells)
            .err(),
        Some(LedgerInitError::Ledger(LeaseError::Quota(Kind::BlobCount)))
    );
    let excess = AuxiliaryUsage {
        sort_bytes: u64::MAX,
        ..auxiliary()
    };
    assert_eq!(
        store
            .usage_fence(deadline())
            .unwrap()
            .initialize_leases(&plan(100), excess, &mut states, &mut cells)
            .err(),
        Some(LedgerInitError::Ledger(LeaseError::Quota(Kind::SortBytes)))
    );
    let fence = store.usage_fence(deadline()).unwrap();
    clock.0.store(100, Ordering::Relaxed);
    assert_eq!(
        fence
            .initialize_leases(&plan(100), auxiliary(), &mut states, &mut cells)
            .err(),
        Some(LedgerInitError::Store(ports::Error::Deadline))
    );
    clock.0.store(1, Ordering::Relaxed);
    let ledger = store
        .usage_fence(deadline())
        .unwrap()
        .initialize_leases(&plan(100), auxiliary(), &mut states, &mut cells)
        .unwrap();
    assert_eq!(ledger.available_cells(), 2);
    assert_eq!(ledger.used(Kind::BodyBytes).unwrap(), 13);
    drop(store.view(ACCOUNT, deadline()).unwrap());
}
#[test]
fn file_capture_refuses_invalid_metadata_without_retiring_a_healthy_writer() {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let store = open(&mut root, Arc::new(Timer::new()));
    let path = db_path(store.root(), RootEntry::Database).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(
        store.usage_fence(deadline()).err(),
        Some(ports::Error::Invalid)
    );
    let writer = lock(&store.writer).unwrap();
    assert!(!writer.stopped);
    assert!(lock(&writer.native.connection).unwrap().is_autocommit());
    drop(writer);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    drop(store.usage_fence(deadline()).unwrap());
}

#[test]
fn expiration_after_ledger_construction_releases_empty_backing_and_writer() {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let clock = Arc::new(Timer::new());
    let store = open(&mut root, clock.clone());
    let mut states = [const { SlotState::EMPTY }; 2];
    let mut cells = [const { Cell::EMPTY }; 2];
    let fence = store.usage_fence(deadline()).unwrap();
    clock.0.store(99, Ordering::Relaxed);
    clock.1.store(true, Ordering::Relaxed);
    assert_eq!(
        fence
            .initialize_leases(&plan(100), auxiliary(), &mut states, &mut cells)
            .err(),
        Some(LedgerInitError::Store(ports::Error::Deadline))
    );
    assert_eq!(clock.0.load(Ordering::Relaxed), 101);
    clock.1.store(false, Ordering::Relaxed);
    clock.0.store(1, Ordering::Relaxed);
    let ledger = store
        .usage_fence(deadline())
        .unwrap()
        .initialize_leases(&plan(100), auxiliary(), &mut states, &mut cells)
        .unwrap();
    assert_eq!(ledger.available_cells(), 2);
    drop(store.usage_fence(deadline()).unwrap());
}
#[test]
fn file_extent_reader_checks_absent_wal_and_each_file_ceiling() {
    let live = Fixture::new();
    let mut live_root = live.locked();
    let store = open(&mut live_root, Arc::new(Timer::new()));
    let fence = store.usage_fence(deadline()).unwrap();
    // Exercise metadata cases in an inert namespace, never unlink a live WAL.
    let fixture = Fixture::new();
    let root = fixture.locked();
    let database = db_path(&root, RootEntry::Database).unwrap();
    let wal = db_path(&root, RootEntry::Wal).unwrap();
    let make_file = |path: &Path, length| {
        let file = fs::File::create(path).unwrap();
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .unwrap();
        file.set_len(length).unwrap();
        file
    };
    let database_file = make_file(&database, 4096);
    let capture = || StoreFileUsage::capture(&root, &fence.writer.native);
    assert_eq!(
        capture().unwrap(),
        StoreFileUsage {
            database_bytes: 4096,
            wal_bytes: 0
        }
    );
    let wal_file = make_file(&wal, 123);
    assert_eq!(capture().unwrap().wal_bytes, 123);
    wal_file
        .set_permissions(fs::Permissions::from_mode(0o644))
        .unwrap();
    assert_eq!(capture(), Err(ports::Error::Invalid));
    wal_file
        .set_permissions(fs::Permissions::from_mode(0o600))
        .unwrap();
    wal_file.set_len(MAX_WAL_BYTES + 1).unwrap();
    assert_eq!(capture(), Err(ports::Error::Invalid));
    wal_file.set_len(123).unwrap();
    database_file.set_len(MAX_PAGES * PAGE_BYTES + 1).unwrap();
    assert_eq!(capture(), Err(ports::Error::Invalid));
    database_file.set_len(4096).unwrap();
    drop(wal_file);
    fs::remove_file(&wal).unwrap();
    std::os::unix::fs::symlink(&database, &wal).unwrap();
    assert_eq!(capture(), Err(ports::Error::Invalid));
    fs::remove_file(&wal).unwrap();
    let target = wal.with_extension("target");
    let target_file = make_file(&target, 123);
    fs::hard_link(&target, &wal).unwrap();
    assert_eq!(capture(), Err(ports::Error::Invalid));
    fs::remove_file(&wal).unwrap();
    drop(target_file);
    assert_eq!(capture().unwrap().wal_bytes, 0);
    fs::remove_file(&database).unwrap();
    assert!(matches!(
        capture(),
        Err(ports::Error::Io {
            kind: std::io::ErrorKind::NotFound,
            ..
        })
    ));
}

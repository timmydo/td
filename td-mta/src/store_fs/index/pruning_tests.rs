#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{
    format::row::{BlobKind, BlobRow, MailboxRow},
    ids::MailboxId,
    ports::{BlobReader, Tick, Time},
    store_fs::tests::Fixture,
};
use std::sync::atomic::{AtomicU64, Ordering};
use td_crypto::Crypto;
const ACCOUNT: AccountId = AccountId::from_bytes([1; 16]);
const OTHER: AccountId = AccountId::from_bytes([2; 16]);
const EPOCH: StoreEpoch = StoreEpoch::from_bytes([9; 16]);
struct Timer(AtomicU64);
impl Clock for Timer {
    fn sample(&self) -> Result<Time, ports::Error> {
        Ok(Time {
            utc_ms: 0,
            monotonic: Tick(self.0.load(Ordering::Relaxed)),
        })
    }
}
fn deadline() -> Deadline {
    Deadline::after(Tick(0), 100).unwrap()
}
fn request(expected: u64, through: u64, max_rows: u32) -> HistoryPruneRequest {
    HistoryPruneRequest {
        account: ACCOUNT,
        expected: Sequence::from_u64(expected),
        through: Sequence::from_u64(through),
        max_rows,
        deadline: deadline(),
    }
}
fn cursor(sequence: u64, operation: u32) -> ChangeCursor {
    ChangeCursor {
        sequence: Sequence::from_u64(sequence),
        operation,
    }
}
fn count(store: &IndexStore<'_>, account: AccountId) -> i64 {
    let writer = lock(&store.writer).unwrap();
    assert!(lock(&writer.native.connection).unwrap().is_autocommit());
    writer.native.begin_work(deadline()).unwrap();
    writer
        .native
        .run(|db| {
            db.query_row(
                "SELECT count(*) FROM changes WHERE account=?1",
                [account.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .map_err(sql)
        })
        .unwrap()
}
fn seed(store: &IndexStore<'_>, account: AccountId, total: u32) {
    let writer = lock(&store.writer).unwrap();
    writer.native.begin_work(deadline()).unwrap();
    writer
        .native
        .run(|db| {
            db.execute_batch("BEGIN IMMEDIATE").map_err(sql)?;
            for n in 0..total {
                let seq = u64::from(n / 4096 + 1).to_be_bytes();
                let mut id = [0u8; 16];
                id[..4].copy_from_slice(&n.to_be_bytes());
                db.execute(
                    "INSERT INTO changes VALUES(?1,?2,?3,?4,1,?5)",
                    params![
                        account.as_bytes().as_slice(),
                        seq.as_slice(),
                        i64::from(n % 4096),
                        if n % 2 == 0 { 1 } else { 2 },
                        id.as_slice()
                    ],
                )
                .map_err(sql)?;
            }
            let end = u64::from(total.saturating_sub(1) / 4096 + 1).to_be_bytes();
            db.execute(
                "UPDATE accounts SET sequence=?2 WHERE id=?1",
                params![account.as_bytes().as_slice(), end.as_slice()],
            )
            .map_err(sql)?;
            db.execute_batch("COMMIT").map_err(sql)
        })
        .unwrap();
}
fn mailbox_commit(store: &IndexStore<'_>, expected: u64, fresh: bool) {
    let id = MailboxId::from_bytes([3; 16]);
    let row = Row::Mailbox(MailboxRow {
        name: if fresh { "first" } else { "updated" },
        parent: None,
        role: None,
        sort_order: 0,
        subscribed: true,
    });
    let mut value = [0; 128];
    let n = row.encode(&mut value).unwrap();
    let ops = [
        Operation::put(Table::Mailboxes, id.as_bytes(), &value[..n]).unwrap(),
        Operation::change(
            ObjectType::Mailbox,
            if fresh {
                ChangeAction::Created
            } else {
                ChangeAction::Updated
            },
            id.as_bytes(),
        ),
    ];
    assert_eq!(
        store.commit(
            &td_crypto::Provider,
            CommitRequest {
                account: ACCOUNT,
                expected: Sequence::from_u64(expected),
                utc_ms: 0,
                deadline: deadline()
            },
            &ops,
            &mut []
        ),
        Ok(Sequence::from_u64(expected + 1))
    );
}

#[test]
fn retirement_is_atomic_while_bounded_cleanup_preserves_old_snapshots() {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let clock = Arc::new(Timer(AtomicU64::new(1)));
    let store = IndexStore::create(&mut root, EPOCH, clock.clone(), 3, deadline()).unwrap();
    store.create_account(ACCOUNT, deadline()).unwrap();
    store.create_account(OTHER, deadline()).unwrap();
    seed(&store, ACCOUNT, 3);
    seed(&store, OTHER, 2);
    let mut old = store.view(ACCOUNT, deadline()).unwrap();
    let first = old
        .next_change(cursor(0, u32::MAX), ObjectType::Mailbox)
        .unwrap();
    assert!(matches!(first, ChangeStep::Record(_)));
    let receipt = store.prune_history(request(1, 1, 1)).unwrap();
    assert_eq!(
        receipt,
        HistoryPruned {
            identity: ViewIdentity {
                account: ACCOUNT,
                epoch: EPOCH,
                committed_sequence: Sequence::from_u64(1),
                history_floor: Sequence::from_u64(1)
            },
            removed: 1,
            more: true
        }
    );
    assert_eq!(count(&store, ACCOUNT), 2);
    assert_eq!(count(&store, OTHER), 2);
    let mut new = store.view(ACCOUNT, deadline()).unwrap();
    assert_eq!(new.identity(), receipt.identity);
    assert_eq!(
        new.next_change(cursor(0, u32::MAX), ObjectType::Mailbox),
        Err(ports::Error::HistoryLost)
    );
    for kind in [ObjectType::Mailbox, ObjectType::Thread] {
        for operation in [0, 1, 4095] {
            assert_eq!(
                new.next_change(cursor(1, operation), kind),
                Err(ports::Error::HistoryLost)
            );
        }
        assert_eq!(
            new.next_change(cursor(1, u32::MAX), kind),
            Ok(ChangeStep::Complete)
        );
    }
    assert_eq!(
        old.next_change(cursor(0, u32::MAX), ObjectType::Mailbox),
        Ok(first)
    );
    let receipt = store.prune_history(request(1, 1, 1)).unwrap();
    assert_eq!((receipt.removed, receipt.more), (1, true));
    let receipt = store.prune_history(request(1, 1, 1)).unwrap();
    assert_eq!((receipt.removed, receipt.more), (1, false));
    let receipt = store.prune_history(request(1, 1, 1)).unwrap();
    assert_eq!((receipt.removed, receipt.more), (0, false));
    assert_eq!(
        old.next_change(cursor(0, u32::MAX), ObjectType::Mailbox),
        Ok(first)
    );
    assert_eq!(store.checkpoint(deadline()), Err(ports::Error::Busy));
    drop(new);
    drop(old);
    mailbox_commit(&store, 1, true);
    mailbox_commit(&store, 2, false);
    let mut view = store.view(ACCOUNT, deadline()).unwrap();
    assert_eq!(view.identity().history_floor, Sequence::from_u64(1));
    assert_eq!(
        view.next_change(cursor(1, u32::MAX), ObjectType::Mailbox),
        Ok(ChangeStep::Record(ChangeRecord {
            cursor: cursor(2, 1),
            change: Change {
                kind: ObjectType::Mailbox,
                action: ChangeAction::Created,
                id: [3; 16],
            },
        }))
    );
    drop(view);
    store.checkpoint(deadline()).unwrap();
    drop(store);
    let store = IndexStore::open(&mut root, clock, 2, deadline()).unwrap();
    assert_eq!(
        store
            .view(ACCOUNT, deadline())
            .unwrap()
            .identity()
            .history_floor,
        Sequence::from_u64(1)
    );
    assert_eq!(count(&store, ACCOUNT), 2);
    assert_eq!(count(&store, OTHER), 2);
    let receipt = store.prune_history(request(3, 3, 4096)).unwrap();
    assert_eq!((receipt.removed, receipt.more), (2, false));
    let mut view = store.view(ACCOUNT, deadline()).unwrap();
    assert_eq!(
        view.next_change(cursor(3, u32::MAX), ObjectType::Mailbox),
        Ok(ChangeStep::Complete)
    );
}

#[test]
fn maximum_batch_uses_a_primary_key_prefix_and_one_lookahead() {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let store = IndexStore::create(
        &mut root,
        EPOCH,
        Arc::new(Timer(AtomicU64::new(1))),
        1,
        deadline(),
    )
    .unwrap();
    store.create_account(ACCOUNT, deadline()).unwrap();
    seed(&store, ACCOUNT, 4097);
    {
        let writer = lock(&store.writer).unwrap();
        let db = lock(&writer.native.connection).unwrap();
        for query in [PREFIX, DELETE_PREFIX] {
            let mut statement = db.prepare(&format!("EXPLAIN QUERY PLAN {query}")).unwrap();
            let details: Vec<String> = statement
                .query_map(
                    params![
                        ACCOUNT.as_bytes().as_slice(),
                        2u64.to_be_bytes().as_slice(),
                        4097i64
                    ],
                    |row| row.get(3),
                )
                .unwrap()
                .map(Result::unwrap)
                .collect();
            assert!(
                details
                    .iter()
                    .any(|line| line.contains("SEARCH changes USING PRIMARY KEY")),
                "{details:?}"
            );
            assert!(
                details
                    .iter()
                    .all(|line| !line.contains("SCAN ") && !line.contains("TEMP B-TREE")),
                "{details:?}"
            );
        }
    }
    let receipt = store.prune_history(request(2, 2, 4096)).unwrap();
    assert_eq!((receipt.removed, receipt.more), (4096, true));
    assert_eq!(count(&store, ACCOUNT), 1);
    let receipt = store.prune_history(request(2, 2, 4096)).unwrap();
    assert_eq!((receipt.removed, receipt.more), (1, false));
}

#[test]
fn invalid_or_stale_requests_leave_history_and_counters_unchanged() {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let store = IndexStore::create(
        &mut root,
        EPOCH,
        Arc::new(Timer(AtomicU64::new(1))),
        1,
        deadline(),
    )
    .unwrap();
    store.create_account(ACCOUNT, deadline()).unwrap();
    seed(&store, ACCOUNT, 3);
    for (request, error) in [
        (request(1, 1, 0), ports::Error::Invalid),
        (request(1, 1, 4097), ports::Error::Invalid),
        (request(0, 1, 1), ports::Error::Conflict),
        (request(1, 2, 1), ports::Error::Invalid),
    ] {
        assert_eq!(
            store.prune_history(request),
            Err(CommitError::Rejected(error))
        );
        assert_eq!(count(&store, ACCOUNT), 3);
        assert_eq!(
            store
                .view(ACCOUNT, deadline())
                .unwrap()
                .identity()
                .history_floor,
            Sequence::default()
        );
    }
    store.prune_history(request(1, 1, 1)).unwrap();
    assert_eq!(
        store.prune_history(request(1, 0, 1)),
        Err(CommitError::Rejected(ports::Error::Conflict))
    );
    assert_eq!(count(&store, ACCOUNT), 2);
    let fence = store.usage_fence(deadline()).unwrap();
    assert_eq!(
        store.prune_history(request(1, 1, 1)),
        Err(CommitError::Rejected(ports::Error::Busy))
    );
    drop(fence);
    lock(&store.writer).unwrap().stopped = true;
    assert_eq!(
        store.prune_history(request(1, 1, 1)),
        Err(CommitError::Rejected(ports::Error::WriterStopped))
    );
}

#[test]
fn selected_prefix_and_lookahead_require_valid_cursor_metadata() {
    for (bytes, operation, max_rows) in [
        (vec![0; 7], 0, 1),
        (vec![0; 9], 0, 1),
        (1u64.to_be_bytes().to_vec(), -1, 1),
        (1u64.to_be_bytes().to_vec(), 4096, 2),
    ] {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let store = IndexStore::create(
            &mut root,
            EPOCH,
            Arc::new(Timer(AtomicU64::new(1))),
            1,
            deadline(),
        )
        .unwrap();
        store.create_account(ACCOUNT, deadline()).unwrap();
        seed(&store, ACCOUNT, 3);
        {
            let writer = lock(&store.writer).unwrap();
            let db = lock(&writer.native.connection).unwrap();
            db.execute_batch("PRAGMA ignore_check_constraints=ON")
                .unwrap();
            db.execute(
                "UPDATE changes SET sequence=?1,operation=?2 WHERE operation=0",
                params![bytes, operation],
            )
            .unwrap();
            db.execute_batch("PRAGMA ignore_check_constraints=OFF")
                .unwrap();
        }
        assert_eq!(
            store.prune_history(request(1, 1, max_rows)),
            Err(CommitError::Rejected(ports::Error::Corrupt))
        );
        assert_eq!(count(&store, ACCOUNT), 3);
        assert_eq!(
            store
                .view(ACCOUNT, deadline())
                .unwrap()
                .identity()
                .history_floor,
            Sequence::default()
        );
    }
}

#[test]
fn native_failures_roll_back_deletion_and_floor_together() {
    for stage in 0..3 {
        for error in [
            ports::Error::Deadline,
            ports::Error::Capacity,
            ports::Error::Corrupt,
            ports::Error::Io {
                kind: std::io::ErrorKind::Other,
                os_code: None,
            },
        ] {
            let fixture = Fixture::new();
            let mut root = fixture.locked();
            let store = IndexStore::create(
                &mut root,
                EPOCH,
                Arc::new(Timer(AtomicU64::new(1))),
                1,
                deadline(),
            )
            .unwrap();
            store.create_account(ACCOUNT, deadline()).unwrap();
            seed(&store, ACCOUNT, 3);
            let hit = Arc::new(AtomicU64::new(0));
            {
                let writer = lock(&store.writer).unwrap();
                let budget = Arc::clone(&writer.native.budget);
                let seen = Arc::clone(&hit);
                lock(&writer.native.connection)
                    .unwrap()
                    .authorizer(Some(move |context: AuthContext<'_>| {
                        let target = match stage {
                            0 => matches!(
                                context.action,
                                AuthAction::Read {
                                    table_name: "changes",
                                    ..
                                }
                            ),
                            1 => matches!(
                                context.action,
                                AuthAction::Delete {
                                    table_name: "changes"
                                }
                            ),
                            _ => matches!(
                                context.action,
                                AuthAction::Update {
                                    table_name: "accounts",
                                    column_name: "floor"
                                }
                            ),
                        };
                        if target {
                            seen.fetch_add(1, Ordering::Relaxed);
                            lock(&budget).unwrap().failure = Some(error);
                        }
                        Authorization::Allow
                    }))
                    .unwrap();
            }
            assert_eq!(
                store.prune_history(request(1, 1, 2)),
                Err(CommitError::Rejected(error))
            );
            assert!(hit.load(Ordering::Relaxed) > 0);
            lock(&lock(&store.writer).unwrap().native.connection)
                .unwrap()
                .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
                .unwrap();
            assert_eq!(count(&store, ACCOUNT), 3);
            assert_eq!(
                store
                    .view(ACCOUNT, deadline())
                    .unwrap()
                    .identity()
                    .history_floor,
                Sequence::default()
            );
            assert!(!lock(&store.writer).unwrap().stopped);
            assert_eq!(store.prune_history(request(1, 1, 2)).unwrap().removed, 2);
        }
    }
}

#[test]
fn pruning_uses_original_deadline_and_common_commit_outcomes() {
    for indeterminate in [false, true] {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let clock = Arc::new(Timer(AtomicU64::new(1)));
        let store = IndexStore::create(&mut root, EPOCH, clock.clone(), 1, deadline()).unwrap();
        store.create_account(ACCOUNT, deadline()).unwrap();
        seed(&store, ACCOUNT, 3);
        clock.0.store(100, Ordering::Relaxed);
        assert_eq!(
            store.prune_history(request(1, 1, 1)),
            Err(CommitError::Rejected(ports::Error::Deadline))
        );
        clock.0.store(1, Ordering::Relaxed);
        assert_eq!(count(&store, ACCOUNT), 3);
        if indeterminate {
            lock(&lock(&store.writer).unwrap().native.connection)
                .unwrap()
                .authorizer(Some(|context: AuthContext<'_>| {
                    let commit = matches!(context.action,
                        AuthAction::Transaction { operation }
                        if !matches!(operation,
                            TransactionOperation::Begin | TransactionOperation::Rollback));
                    if commit {
                        Authorization::Deny
                    } else {
                        Authorization::Allow
                    }
                }))
                .unwrap();
            assert!(matches!(
                store.prune_history(request(1, 1, 1)),
                Err(CommitError::Indeterminate(_))
            ));
            lock(&lock(&store.writer).unwrap().native.connection)
                .unwrap()
                .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
                .unwrap();
            assert!(lock(&store.writer).unwrap().stopped);
            assert_eq!(count(&store, ACCOUNT), 3);
            assert_eq!(
                store
                    .view(ACCOUNT, deadline())
                    .unwrap()
                    .identity()
                    .history_floor,
                Sequence::default()
            );
            assert_eq!(
                store.prune_history(request(1, 1, 1)),
                Err(CommitError::Rejected(ports::Error::WriterStopped))
            );
        } else {
            let timer = clock.clone();
            lock(&lock(&store.writer).unwrap().native.connection)
                .unwrap()
                .commit_hook(Some(move || {
                    timer.0.store(100, Ordering::Relaxed);
                    false
                }))
                .unwrap();
            let receipt = store.prune_history(request(1, 1, 1)).unwrap();
            assert_eq!((receipt.removed, receipt.more), (1, true));
            assert_eq!(clock.0.load(Ordering::Relaxed), 100);
            clock.0.store(1, Ordering::Relaxed);
            assert_eq!(count(&store, ACCOUNT), 2);
            assert_eq!(
                store
                    .view(ACCOUNT, deadline())
                    .unwrap()
                    .identity()
                    .history_floor,
                Sequence::from_u64(1)
            );
        }
    }
}

#[test]
fn partway_cursor_cannot_resume_in_a_retired_floor_sequence() {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let store = IndexStore::create(
        &mut root,
        EPOCH,
        Arc::new(Timer(AtomicU64::new(1))),
        2,
        deadline(),
    )
    .unwrap();
    store.create_account(ACCOUNT, deadline()).unwrap();
    seed(&store, ACCOUNT, 3);
    let mut old = store.view(ACCOUNT, deadline()).unwrap();
    let first = old
        .next_change(cursor(0, u32::MAX), ObjectType::Mailbox)
        .unwrap();
    let ChangeStep::Record(first) = first else {
        panic!("missing initial record")
    };
    assert_eq!(first.cursor, cursor(1, 0));
    store.prune_history(request(1, 1, 1)).unwrap();
    let mut new = store.view(ACCOUNT, deadline()).unwrap();
    let resumed = new.next_change(first.cursor, ObjectType::Mailbox);
    let later = old.next_change(first.cursor, ObjectType::Mailbox).unwrap();
    assert!(matches!(later, ChangeStep::Record(record) if record.cursor == cursor(1, 2)));
    assert_eq!(
        new.next_change(cursor(1, u32::MAX), ObjectType::Mailbox),
        Ok(ChangeStep::Complete)
    );
    assert_eq!(resumed, Err(ports::Error::HistoryLost));
}

#[test]
fn advancing_floor_reclaims_old_leftovers_before_newly_retired_rows() {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let store = IndexStore::create(
        &mut root,
        EPOCH,
        Arc::new(Timer(AtomicU64::new(1))),
        2,
        deadline(),
    )
    .unwrap();
    store.create_account(ACCOUNT, deadline()).unwrap();
    seed(&store, ACCOUNT, 3);
    let receipt = store.prune_history(request(1, 1, 1)).unwrap();
    assert_eq!((receipt.removed, receipt.more), (1, true));
    mailbox_commit(&store, 1, true);
    mailbox_commit(&store, 2, false);
    let mut retained = store.view(ACCOUNT, deadline()).unwrap();
    let before = retained
        .next_change(cursor(1, u32::MAX), ObjectType::Mailbox)
        .unwrap();
    assert!(matches!(before, ChangeStep::Record(record) if record.cursor == cursor(2, 1)));
    let receipt = store.prune_history(request(3, 3, 2)).unwrap();
    assert_eq!((receipt.removed, receipt.more), (2, true));
    assert_eq!(receipt.identity.history_floor, Sequence::from_u64(3));
    assert_eq!(receipt.identity.committed_sequence, Sequence::from_u64(3));
    assert_eq!(count(&store, ACCOUNT), 2);
    {
        let writer = lock(&store.writer).unwrap();
        let db = lock(&writer.native.connection).unwrap();
        let mut statement = db
            .prepare("SELECT sequence FROM changes WHERE account=?1 ORDER BY sequence,operation")
            .unwrap();
        let sequences: Vec<Vec<u8>> = statement
            .query_map([ACCOUNT.as_bytes().as_slice()], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            sequences,
            vec![2u64.to_be_bytes().to_vec(), 3u64.to_be_bytes().to_vec()]
        );
    }
    let receipt = store.prune_history(request(3, 3, 2)).unwrap();
    assert_eq!((receipt.removed, receipt.more), (2, false));
    assert_eq!(count(&store, ACCOUNT), 0);
    assert_eq!(
        retained.next_change(cursor(1, u32::MAX), ObjectType::Mailbox),
        Ok(before)
    );
    let mut current = store.view(ACCOUNT, deadline()).unwrap();
    let mut bytes = [0; 128];
    let (row, changed) = current
        .get(Key::Mailbox(MailboxId::from_bytes([3; 16])), &mut bytes)
        .unwrap()
        .unwrap();
    assert!(matches!(row, Row::Mailbox(mailbox) if mailbox.name=="updated"));
    assert_eq!(changed, Sequence::from_u64(3));
}

#[test]
fn pruning_requires_an_existing_account_and_wal_headroom() {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let store = IndexStore::create(
        &mut root,
        EPOCH,
        Arc::new(Timer(AtomicU64::new(1))),
        1,
        deadline(),
    )
    .unwrap();
    assert_eq!(
        store.prune_history(request(0, 0, 1)),
        Err(CommitError::Rejected(ports::Error::NotFound))
    );
    assert_eq!(count(&store, ACCOUNT), 0);
    store.create_account(ACCOUNT, deadline()).unwrap();
    seed(&store, ACCOUNT, 3);
    let path = db_path(store.root, RootEntry::Wal).unwrap();
    let wal = OpenOptions::new().write(true).open(path).unwrap();
    let original = wal.metadata().unwrap().len();
    wal.set_len(MAX_WAL_BYTES - TRANSACTION_WAL_BYTES + 1)
        .unwrap();
    let refused = store.prune_history(request(1, 1, 1));
    wal.set_len(original).unwrap();
    assert_eq!(refused, Err(CommitError::Rejected(ports::Error::Busy)));
    assert_eq!(count(&store, ACCOUNT), 3);
    assert_eq!(
        store
            .view(ACCOUNT, deadline())
            .unwrap()
            .identity()
            .history_floor,
        Sequence::default()
    );
    assert_eq!(store.prune_history(request(1, 1, 1)).unwrap().removed, 1);
}

#[test]
fn full_reader_pool_retains_bodies_and_history_during_bounded_pruning() {
    pruned_reader_scenario(PrunedScenario::Cleaned);
}

#[test]
fn backup_preserves_retired_history_pending_bounded_cleanup() {
    pruned_reader_scenario(PrunedScenario::PendingBackup);
}

#[test]
fn epoch_renewal_preserves_backup_history_pending_cleanup() {
    pruned_reader_scenario(PrunedScenario::RenewedBackup);
}

// SQLite's heap cap is process-wide; isolate these full-pool fixtures.
static FULL_READER_FIXTURE: Mutex<()> = Mutex::new(());

enum PrunedScenario {
    Cleaned,
    PendingBackup,
    RenewedBackup,
}

fn pruned_reader_scenario(scenario: PrunedScenario) {
    let _fixture_guard = FULL_READER_FIXTURE.lock().unwrap();
    let copy_pending = !matches!(scenario, PrunedScenario::Cleaned);
    let renew_pending = matches!(scenario, PrunedScenario::RenewedBackup);
    const READERS: usize = 8;
    const BYTES: usize = 2 * 1024 * 1024;
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let clock = Arc::new(Timer(AtomicU64::new(1)));
    let store = IndexStore::create(&mut root, EPOCH, clock.clone(), READERS, deadline()).unwrap();
    store.create_account(ACCOUNT, deadline()).unwrap();
    let blob = BlobId::from_bytes([4; 16]);
    let parent = MailboxId::from_bytes([3; 16]);
    let body: Vec<u8> = (0..BYTES).map(|n| (n % 251) as u8).collect();
    let mut digest = td_crypto::Provider.sha256().unwrap();
    digest.update(&body).unwrap();
    let blob_row = Row::Blob(BlobRow {
        kind: BlobKind::Message,
        length: BYTES as u64,
        digest: digest.finish().unwrap(),
        created_at: 0,
    });
    let mut blob_bytes = [0; 128];
    let blob_len = blob_row.encode(&mut blob_bytes).unwrap();
    let parent_row = Row::Mailbox(MailboxRow {
        name: "first",
        parent: None,
        role: None,
        sort_order: 0,
        subscribed: true,
    });
    let mut parent_bytes = [0; 128];
    let parent_len = parent_row.encode(&mut parent_bytes).unwrap();
    let mut source = body.as_slice();
    assert_eq!(
        store.commit(
            &td_crypto::Provider,
            CommitRequest {
                account: ACCOUNT,
                expected: Sequence::default(),
                utc_ms: 0,
                deadline: deadline()
            },
            &[
                Operation::put(Table::Blobs, blob.as_bytes(), &blob_bytes[..blob_len]).unwrap(),
                Operation::put(
                    Table::Mailboxes,
                    parent.as_bytes(),
                    &parent_bytes[..parent_len]
                )
                .unwrap(),
                Operation::change(
                    ObjectType::Mailbox,
                    ChangeAction::Created,
                    parent.as_bytes()
                ),
            ],
            &mut [BlobSource {
                id: blob,
                source: &mut source
            }],
        ),
        Ok(Sequence::from_u64(1))
    );
    assert!(source.is_empty());
    mailbox_commit(&store, 1, false);
    let updated_parent = Row::Mailbox(MailboxRow {
        name: "updated",
        parent: None,
        role: None,
        sort_order: 0,
        subscribed: true,
    });
    let old_identity = ViewIdentity {
        account: ACCOUNT,
        epoch: EPOCH,
        committed_sequence: Sequence::from_u64(2),
        history_floor: Sequence::default(),
    };
    let current_identity = ViewIdentity {
        history_floor: Sequence::from_u64(2),
        ..old_identity
    };
    let created = ChangeStep::Record(ChangeRecord {
        cursor: cursor(1, 2),
        change: Change {
            kind: ObjectType::Mailbox,
            action: ChangeAction::Created,
            id: *parent.as_bytes(),
        },
    });
    let updated = ChangeStep::Record(ChangeRecord {
        cursor: cursor(2, 1),
        change: Change {
            kind: ObjectType::Mailbox,
            action: ChangeAction::Updated,
            id: *parent.as_bytes(),
        },
    });
    let mut scratch = [0; 65536];
    {
        let mut views: [_; READERS] =
            std::array::from_fn(|_| store.view(ACCOUNT, deadline()).unwrap());
        assert!(views.iter().all(|v| v.identity() == old_identity));
        assert!(matches!(
            store.view(ACCOUNT, deadline()),
            Err(ports::Error::Busy)
        ));
        let mut inputs = views.each_mut().map(|view| {
            view.open_blob_input(&td_crypto::Provider, blob, BYTES as u64)
                .unwrap()
        });
        for input in &mut inputs {
            assert_eq!(input.read(&mut scratch).unwrap(), scratch.len());
            assert_eq!(scratch.as_slice(), &body[..65536]);
        }
        let first = store.prune_history(request(2, 2, 1)).unwrap();
        assert_eq!(
            first,
            HistoryPruned {
                identity: current_identity,
                removed: 1,
                more: true
            }
        );
        if !copy_pending {
            let second = store.prune_history(request(2, 2, 1)).unwrap();
            assert_eq!(
                second,
                HistoryPruned {
                    identity: current_identity,
                    removed: 1,
                    more: false
                }
            );
            assert_eq!(
                store.prune_history(request(2, 2, 1)).unwrap(),
                HistoryPruned {
                    identity: current_identity,
                    removed: 0,
                    more: false
                }
            );
        }
        assert_eq!(store.checkpoint(deadline()), Err(ports::Error::Busy));
        for offset in (65536..BYTES).step_by(65536) {
            for input in &mut inputs {
                assert_eq!(input.read(&mut scratch).unwrap(), scratch.len());
                assert_eq!(scratch.as_slice(), &body[offset..offset + 65536]);
            }
        }
        let mut pins = inputs.map(|input| input.finish().unwrap());
        for pin in &mut pins {
            assert_eq!(pin.len(), BYTES as u64);
            assert_eq!(pin.read_at(65535, &mut scratch).unwrap(), scratch.len());
            assert_eq!(scratch.as_slice(), &body[65535..65535 + 65536]);
            assert_eq!(pin.read_at(BYTES as u64 - 1, &mut scratch).unwrap(), 1);
            assert_eq!(scratch.first(), body.last());
        }
        assert!(matches!(
            store.view(ACCOUNT, deadline()),
            Err(ports::Error::Busy)
        ));
        drop(pins);
        for view in &mut views {
            assert_eq!(view.identity(), old_identity);
            assert_eq!(
                view.get(Key::Blob(blob), &mut scratch).unwrap(),
                Some((blob_row, Sequence::from_u64(1)))
            );
            assert_eq!(
                view.get(Key::Mailbox(parent), &mut scratch).unwrap(),
                Some((updated_parent, Sequence::from_u64(2)))
            );
            assert_eq!(
                view.next_change(cursor(0, u32::MAX), ObjectType::Mailbox)
                    .unwrap(),
                created
            );
            assert_eq!(
                view.next_change(cursor(1, 2), ObjectType::Mailbox).unwrap(),
                updated
            );
            assert_eq!(
                view.next_change(cursor(2, 1), ObjectType::Mailbox).unwrap(),
                ChangeStep::Complete
            );
        }
        let [released, remaining @ ..] = views;
        drop(released);
        let mut current = store.view(ACCOUNT, deadline()).unwrap();
        assert_eq!(current.identity(), current_identity);
        assert_eq!(
            current.next_change(cursor(0, u32::MAX), ObjectType::Mailbox),
            Err(ports::Error::HistoryLost)
        );
        assert_eq!(
            current
                .next_change(cursor(2, u32::MAX), ObjectType::Mailbox)
                .unwrap(),
            ChangeStep::Complete
        );
        assert!(matches!(
            store.view(ACCOUNT, deadline()),
            Err(ports::Error::Busy)
        ));
        assert_eq!(store.checkpoint(deadline()), Err(ports::Error::Busy));
        for mut old in remaining {
            assert_eq!(old.identity(), old_identity);
            assert_eq!(
                old.next_change(cursor(0, u32::MAX), ObjectType::Mailbox)
                    .unwrap(),
                created
            );
            assert_eq!(
                old.next_change(cursor(1, 2), ObjectType::Mailbox).unwrap(),
                updated
            );
        }
        drop(current);
        let mut fresh: [_; READERS] =
            std::array::from_fn(|_| store.view(ACCOUNT, deadline()).unwrap());
        for view in &mut fresh {
            assert_eq!(view.identity(), current_identity);
            assert_eq!(
                view.get(Key::Blob(blob), &mut scratch).unwrap(),
                Some((blob_row, Sequence::from_u64(1)))
            );
            assert_eq!(
                view.get(Key::Mailbox(parent), &mut scratch).unwrap(),
                Some((updated_parent, Sequence::from_u64(2)))
            );
            assert_eq!(
                view.next_change(cursor(0, u32::MAX), ObjectType::Mailbox),
                Err(ports::Error::HistoryLost)
            );
            assert_eq!(
                view.next_change(cursor(2, u32::MAX), ObjectType::Mailbox)
                    .unwrap(),
                ChangeStep::Complete
            );
        }
        drop(fresh);
    }
    store.checkpoint(deadline()).unwrap();
    drop(store);
    let check_current =
        |view: &mut IndexReadView<'_, '_>, epoch: StoreEpoch, scratch: &mut [u8]| {
            assert_eq!(
                view.identity(),
                ViewIdentity {
                    epoch,
                    ..current_identity
                }
            );
            assert_eq!(
                view.get(Key::Blob(blob), scratch).unwrap(),
                Some((blob_row, Sequence::from_u64(1)))
            );
            assert_eq!(
                view.get(Key::Mailbox(parent), scratch).unwrap(),
                Some((updated_parent, Sequence::from_u64(2)))
            );
            assert_eq!(
                view.next_change(cursor(0, u32::MAX), ObjectType::Mailbox),
                Err(ports::Error::HistoryLost)
            );
            assert_eq!(
                view.next_change(cursor(2, u32::MAX), ObjectType::Mailbox)
                    .unwrap(),
                ChangeStep::Complete
            );
            let mut input = view
                .open_blob_input(&td_crypto::Provider, blob, BYTES as u64)
                .unwrap();
            for bytes in body.chunks(65536) {
                assert_eq!(input.read(scratch).unwrap(), bytes.len());
                assert_eq!(&*scratch, bytes);
            }
            drop(input.finish().unwrap());
        };
    let store = IndexStore::open(&mut root, clock.clone(), READERS, deadline()).unwrap();
    store.validate_integrity(deadline()).unwrap();
    {
        let mut reopened: [_; READERS] =
            std::array::from_fn(|_| store.view(ACCOUNT, deadline()).unwrap());
        for view in &mut reopened {
            check_current(view, EPOCH, &mut scratch);
        }
    }
    if copy_pending {
        let destination = Fixture::new();
        let mut copied_root = destination.locked();
        let receipt = store
            .backup(&mut copied_root, deadline(), &mut scratch)
            .unwrap();
        assert_eq!(receipt.epoch, EPOCH);
        assert!(
            receipt.bytes > BYTES as u64 && receipt.bytes <= crate::limits::SQLITE_DATABASE_BYTES
        );
        assert_eq!(receipt.bytes % PAGE_BYTES, 0);
        assert_eq!(
            fs::symlink_metadata(db_path(&copied_root, RootEntry::Database).unwrap())
                .unwrap()
                .len(),
            receipt.bytes
        );
        assert!(matches!(
            fixture.lock(),
            Err(crate::store_fs::LockError::Busy)
        ));
        assert!(matches!(
            destination.lock(),
            Err(crate::store_fs::LockError::Busy)
        ));
        let source = IndexStore::open(&mut root, clock.clone(), READERS, deadline()).unwrap();
        let copied =
            IndexStore::open(&mut copied_root, clock.clone(), READERS, deadline()).unwrap();
        let copied_epoch = if renew_pending {
            StoreEpoch::from_bytes([0xa5; 16])
        } else {
            EPOCH
        };
        let copied = if renew_pending {
            struct FreshEntropy(usize);
            impl ports::Entropy for FreshEntropy {
                fn fill(&mut self, output: &mut [u8]) -> Result<(), ports::CryptoError> {
                    assert_eq!(output.len(), 16);
                    self.0 += 1;
                    output.fill(0xa5);
                    Ok(())
                }
            }
            let mut entropy = FreshEntropy(0);
            let copied = copied.renew_epoch(&mut entropy, deadline()).unwrap();
            assert_eq!(entropy.0, 1);
            assert_eq!(copied.epoch(), copied_epoch);
            drop(copied);
            IndexStore::open(&mut copied_root, clock, READERS, deadline()).unwrap()
        } else {
            copied
        };
        assert_eq!(source.epoch(), EPOCH);
        assert_eq!(copied.epoch(), copied_epoch);
        source.validate_integrity(deadline()).unwrap();
        copied.validate_integrity(deadline()).unwrap();
        {
            let mut source_views: [_; READERS] =
                std::array::from_fn(|_| source.view(ACCOUNT, deadline()).unwrap());
            let mut copied_views: [_; READERS] =
                std::array::from_fn(|_| copied.view(ACCOUNT, deadline()).unwrap());
            for (views, epoch) in [
                (&mut source_views, EPOCH),
                (&mut copied_views, copied_epoch),
            ] {
                for view in views {
                    check_current(view, epoch, &mut scratch);
                }
            }
            for (store, epoch) in [(&copied, copied_epoch), (&source, EPOCH)] {
                assert!(matches!(
                    store.view(ACCOUNT, deadline()),
                    Err(ports::Error::Busy)
                ));
                assert_eq!(store.checkpoint(deadline()), Err(ports::Error::Busy));
                assert_eq!(
                    store.prune_history(request(2, 2, 1)).unwrap(),
                    HistoryPruned {
                        identity: ViewIdentity {
                            epoch,
                            ..current_identity
                        },
                        removed: 1,
                        more: false
                    }
                );
                assert_eq!(
                    store.prune_history(request(2, 2, 1)).unwrap(),
                    HistoryPruned {
                        identity: ViewIdentity {
                            epoch,
                            ..current_identity
                        },
                        removed: 0,
                        more: false
                    }
                );
            }
            for (views, epoch) in [
                (&mut source_views, EPOCH),
                (&mut copied_views, copied_epoch),
            ] {
                for view in views {
                    check_current(view, epoch, &mut scratch);
                }
            }
            for store in [&copied, &source] {
                assert!(matches!(
                    store.view(ACCOUNT, deadline()),
                    Err(ports::Error::Busy)
                ));
                assert_eq!(store.checkpoint(deadline()), Err(ports::Error::Busy));
            }
        }
        source.checkpoint(deadline()).unwrap();
        copied.checkpoint(deadline()).unwrap();
        source.validate_integrity(deadline()).unwrap();
        copied.validate_integrity(deadline()).unwrap();
    }
}

#[test]
fn renewed_backup_cleanup_preserves_distinct_accounts_with_shared_object_ids() {
    let _fixture_guard = FULL_READER_FIXTURE.lock().unwrap();
    const READERS: usize = 8;
    const BYTES: usize = 2 * 1024 * 1024;
    const FRESH: StoreEpoch = StoreEpoch::from_bytes([0xa6; 16]);
    let fixture = Fixture::new();
    let destination = Fixture::new();
    let mut root = fixture.locked();
    let mut copied_root = destination.locked();
    let clock = Arc::new(Timer(AtomicU64::new(1)));
    let store = IndexStore::create(&mut root, EPOCH, clock.clone(), READERS, deadline()).unwrap();
    let blob = BlobId::from_bytes([4; 16]);
    let parent = MailboxId::from_bytes([3; 16]);
    let bodies: [_; 2] = std::array::from_fn(|index| {
        (0..BYTES)
            .map(|n| ((n + index * 17) % 251) as u8)
            .collect::<Vec<_>>()
    });
    let blob_rows: [_; 2] = std::array::from_fn(|index| {
        let mut digest = td_crypto::Provider.sha256().unwrap();
        digest.update(&bodies[index]).unwrap();
        Row::Blob(BlobRow {
            kind: BlobKind::Message,
            length: BYTES as u64,
            digest: digest.finish().unwrap(),
            created_at: 0,
        })
    });
    let mailbox = |name: &'static str| {
        Row::Mailbox(MailboxRow {
            name,
            parent: None,
            role: None,
            sort_order: 0,
            subscribed: true,
        })
    };
    for (index, account) in [ACCOUNT, OTHER].into_iter().enumerate() {
        store.create_account(account, deadline()).unwrap();
        let mut body_bytes = [0; 128];
        let body_len = blob_rows[index].encode(&mut body_bytes).unwrap();
        let mut parent_bytes = [0; 128];
        let parent_len = mailbox(if index == 0 { "first" } else { "other" })
            .encode(&mut parent_bytes)
            .unwrap();
        let mut input = bodies[index].as_slice();
        assert_eq!(
            store.commit(
                &td_crypto::Provider,
                CommitRequest {
                    account,
                    expected: Sequence::default(),
                    utc_ms: 0,
                    deadline: deadline()
                },
                &[
                    Operation::put(Table::Blobs, blob.as_bytes(), &body_bytes[..body_len]).unwrap(),
                    Operation::put(
                        Table::Mailboxes,
                        parent.as_bytes(),
                        &parent_bytes[..parent_len]
                    )
                    .unwrap(),
                    Operation::change(
                        ObjectType::Mailbox,
                        ChangeAction::Created,
                        parent.as_bytes()
                    ),
                ],
                &mut [BlobSource {
                    id: blob,
                    source: &mut input
                }],
            ),
            Ok(Sequence::from_u64(1))
        );
        assert!(input.is_empty());
    }
    mailbox_commit(&store, 1, false);
    let identity = |account, epoch| ViewIdentity {
        account,
        epoch,
        committed_sequence: Sequence::from_u64(if account == ACCOUNT { 2 } else { 1 }),
        history_floor: Sequence::from_u64(if account == ACCOUNT { 2 } else { 0 }),
    };
    assert_eq!(
        store.prune_history(request(2, 2, 1)).unwrap(),
        HistoryPruned {
            identity: identity(ACCOUNT, EPOCH),
            removed: 1,
            more: true,
        }
    );
    let mut scratch = [0; 65536];
    let receipt = store
        .backup(&mut copied_root, deadline(), &mut scratch)
        .unwrap();
    assert_eq!(receipt.epoch, EPOCH);
    assert!(
        receipt.bytes > (2 * BYTES) as u64 && receipt.bytes <= crate::limits::SQLITE_DATABASE_BYTES
    );
    assert_eq!(receipt.bytes % PAGE_BYTES, 0);
    assert_eq!(
        fs::symlink_metadata(db_path(&copied_root, RootEntry::Database).unwrap())
            .unwrap()
            .len(),
        receipt.bytes
    );
    for fixture in [&fixture, &destination] {
        assert!(matches!(
            fixture.lock(),
            Err(crate::store_fs::LockError::Busy)
        ));
    }
    let source = IndexStore::open(&mut root, clock.clone(), READERS, deadline()).unwrap();
    let copied = IndexStore::open(&mut copied_root, clock.clone(), READERS, deadline()).unwrap();
    struct FreshEntropy(usize);
    impl ports::Entropy for FreshEntropy {
        fn fill(&mut self, output: &mut [u8]) -> Result<(), ports::CryptoError> {
            assert_eq!(output.len(), 16);
            self.0 += 1;
            output.copy_from_slice(FRESH.as_bytes());
            Ok(())
        }
    }
    let mut entropy = FreshEntropy(0);
    let copied = copied.renew_epoch(&mut entropy, deadline()).unwrap();
    assert_eq!(entropy.0, 1);
    assert_eq!(copied.epoch(), FRESH);
    drop(copied);
    let copied = IndexStore::open(&mut copied_root, clock.clone(), READERS, deadline()).unwrap();
    let check = |view: &mut IndexReadView<'_, '_>, account, epoch, scratch: &mut [u8]| {
        assert_eq!(view.identity(), identity(account, epoch));
        let index = if account == ACCOUNT { 0 } else { 1 };
        assert_eq!(
            view.get(Key::Blob(blob), scratch).unwrap(),
            Some((blob_rows[index], Sequence::from_u64(1)))
        );
        assert_eq!(
            view.get(Key::Mailbox(parent), scratch).unwrap(),
            Some((
                mailbox(if index == 0 { "updated" } else { "other" }),
                Sequence::from_u64(if index == 0 { 2 } else { 1 })
            ))
        );
        if account == ACCOUNT {
            assert_eq!(
                view.next_change(cursor(0, u32::MAX), ObjectType::Mailbox),
                Err(ports::Error::HistoryLost)
            );
            assert_eq!(
                view.next_change(cursor(2, u32::MAX), ObjectType::Mailbox)
                    .unwrap(),
                ChangeStep::Complete
            );
        } else {
            assert_eq!(account, OTHER);
            assert_eq!(
                view.next_change(cursor(0, u32::MAX), ObjectType::Mailbox)
                    .unwrap(),
                ChangeStep::Record(ChangeRecord {
                    cursor: cursor(1, 2),
                    change: Change {
                        kind: ObjectType::Mailbox,
                        action: ChangeAction::Created,
                        id: *parent.as_bytes()
                    },
                })
            );
            assert_eq!(
                view.next_change(cursor(1, 2), ObjectType::Mailbox).unwrap(),
                ChangeStep::Complete
            );
        }
        let mut input = view
            .open_blob_input(&td_crypto::Provider, blob, BYTES as u64)
            .unwrap();
        for bytes in bodies[index].chunks(65536) {
            assert_eq!(input.read(scratch).unwrap(), bytes.len());
            assert_eq!(&*scratch, bytes);
        }
        drop(input.finish().unwrap());
    };
    assert_eq!(source.epoch(), EPOCH);
    assert_eq!(copied.epoch(), FRESH);
    for store in [&source, &copied] {
        store.validate_integrity(deadline()).unwrap();
    }
    {
        let mut source_views: [_; READERS] = std::array::from_fn(|index| {
            source
                .view(if index % 2 == 0 { ACCOUNT } else { OTHER }, deadline())
                .unwrap()
        });
        let mut copied_views: [_; READERS] = std::array::from_fn(|index| {
            copied
                .view(if index % 2 == 0 { ACCOUNT } else { OTHER }, deadline())
                .unwrap()
        });
        for (views, epoch) in [(&mut source_views, EPOCH), (&mut copied_views, FRESH)] {
            for (index, view) in views.iter_mut().enumerate() {
                check(
                    view,
                    if index % 2 == 0 { ACCOUNT } else { OTHER },
                    epoch,
                    &mut scratch,
                );
            }
        }
        for (store, epoch) in [(&copied, FRESH), (&source, EPOCH)] {
            assert!(matches!(
                store.view(ACCOUNT, deadline()),
                Err(ports::Error::Busy)
            ));
            assert_eq!(store.checkpoint(deadline()), Err(ports::Error::Busy));
            for removed in [1, 0] {
                assert_eq!(
                    store.prune_history(request(2, 2, 1)).unwrap(),
                    HistoryPruned {
                        identity: identity(ACCOUNT, epoch),
                        removed,
                        more: false
                    }
                );
            }
        }
        for (views, epoch) in [(&mut source_views, EPOCH), (&mut copied_views, FRESH)] {
            for (index, view) in views.iter_mut().enumerate() {
                check(
                    view,
                    if index % 2 == 0 { ACCOUNT } else { OTHER },
                    epoch,
                    &mut scratch,
                );
            }
        }
        for store in [&source, &copied] {
            assert!(matches!(
                store.view(OTHER, deadline()),
                Err(ports::Error::Busy)
            ));
            assert_eq!(store.checkpoint(deadline()), Err(ports::Error::Busy));
        }
    }
    for store in [&source, &copied] {
        store.checkpoint(deadline()).unwrap();
        store.validate_integrity(deadline()).unwrap();
    }
    drop(copied);
    drop(source);
    let source = IndexStore::open(&mut root, clock.clone(), READERS, deadline()).unwrap();
    let copied = IndexStore::open(&mut copied_root, clock, READERS, deadline()).unwrap();
    for (store, epoch) in [(&source, EPOCH), (&copied, FRESH)] {
        store.validate_integrity(deadline()).unwrap();
        for account in [ACCOUNT, OTHER] {
            let mut view = store.view(account, deadline()).unwrap();
            check(&mut view, account, epoch, &mut scratch);
        }
    }
}

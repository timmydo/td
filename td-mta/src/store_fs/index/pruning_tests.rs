#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{
    format::row::MailboxRow,
    ids::MailboxId,
    ports::{Tick, Time},
    store_fs::tests::Fixture,
};
use std::sync::atomic::{AtomicU64, Ordering};
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

#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{
    format::row::{
        BlobKind, BlobRow, FailureReason, LeaseRow, LeaseUse, NotificationState, RecipientState,
        SubmissionRow,
    },
    ids::{DeviceId, EmailId, IdentityId, SubmissionId, ThreadId},
    ports::{Crypto, Digest, Tick, Time},
    store_fs::{index::recipient_tests::queued, tests::Fixture},
};
use std::sync::atomic::{AtomicU64, Ordering};
const ACCOUNT: AccountId = AccountId::from_bytes([1; 16]);
const OTHER: AccountId = AccountId::from_bytes([2; 16]);
const BODY: BlobId = BlobId::from_bytes([3; 16]);
const UPLOAD: BlobId = BlobId::from_bytes([4; 16]);
const EMPTY: BlobId = BlobId::from_bytes([5; 16]);
const FIRST: SubmissionId = SubmissionId::from_bytes([6; 16]);
const SECOND: SubmissionId = SubmissionId::from_bytes([7; 16]);
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
fn encoded(row: Row<'_>) -> Vec<u8> {
    let mut bytes = vec![0; 65536];
    let n = row.encode(&mut bytes).unwrap();
    bytes.truncate(n);
    bytes
}
fn key_bytes(key: Key<'_>) -> Vec<u8> {
    let mut bytes = vec![0; 1024];
    let n = key.encode(&mut bytes).unwrap();
    bytes.truncate(n);
    bytes
}
fn body(kind: BlobKind, bytes: &[u8]) -> Row<'static> {
    let mut hash = td_crypto::Provider.sha256().unwrap();
    hash.update(bytes).unwrap();
    Row::Blob(BlobRow {
        kind,
        length: bytes.len() as u64,
        digest: hash.finish().unwrap(),
        created_at: 0,
    })
}
fn submission() -> Row<'static> {
    Row::Submission(SubmissionRow {
        email: EmailId::from_bytes([8; 16]),
        thread: ThreadId::from_bytes([9; 16]),
        identity: IdentityId::from_bytes([10; 16]),
        transmitted_blob: BODY,
        reverse_path: "",
        send_at: 0,
        expires_at: 432_000_000,
        recipient_count: 1,
        completed_at: None,
        notification: NotificationState::None,
        notification_email: None,
    })
}
fn commit(
    store: &IndexStore<'_>,
    account: AccountId,
    expected: u64,
    puts: &[(Key<'_>, Row<'_>)],
    deletes: &[Key<'_>],
    sources: &mut [BlobSource<'_>],
) {
    let values: Vec<_> = puts
        .iter()
        .map(|(key, row)| (key.table(), key_bytes(*key), encoded(*row)))
        .collect();
    let removed: Vec<_> = deletes
        .iter()
        .map(|key| (key.table(), key_bytes(*key)))
        .collect();
    let mut operations: Vec<_> = values
        .iter()
        .map(|(table, key, row)| Operation::put(*table, key, row).unwrap())
        .collect();
    operations.extend(
        removed
            .iter()
            .map(|(table, key)| Operation::delete(*table, key).unwrap()),
    );
    assert_eq!(
        store.commit(
            &td_crypto::Provider,
            CommitRequest {
                account,
                epoch: store.epoch(),
                expected: Sequence::from_u64(expected),
                utc_ms: 0,
                deadline: deadline()
            },
            &operations,
            sources
        ),
        Ok(Sequence::from_u64(expected + 1))
    );
}
fn amounts(usage: LogicalUsage) -> [u64; 5] {
    [
        usage.body_bytes,
        usage.blob_count,
        usage.upload_bytes,
        usage.queue_bytes,
        usage.queue_submissions,
    ]
}

#[test]
fn categories_deduplicate_references_and_follow_retained_snapshot_rows() {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let clock = Arc::new(Timer(AtomicU64::new(1)));
    let store = IndexStore::create(
        &mut root,
        StoreEpoch::from_bytes([20; 16]),
        clock,
        3,
        deadline(),
    )
    .unwrap();
    store.create_account(ACCOUNT, deadline()).unwrap();
    store.create_account(OTHER, deadline()).unwrap();
    assert_eq!(
        amounts(
            store
                .view(ACCOUNT, deadline())
                .unwrap()
                .logical_usage()
                .unwrap()
        ),
        [0; 5]
    );
    let mut bytes = b"abc".as_slice();
    let mut upload = b"12345".as_slice();
    let mut empty = b"".as_slice();
    let Row::Submission(mut completed) = submission() else {
        panic!("submission fixture");
    };
    completed.completed_at = Some(1);
    let mut canceled = queued();
    canceled.state = RecipientState::Canceled;
    canceled.next_attempt_at = None;
    canceled.reason = FailureReason::Canceled;
    commit(
        &store,
        ACCOUNT,
        0,
        &[
            (Key::Blob(BODY), body(BlobKind::Message, bytes)),
            (Key::Blob(UPLOAD), body(BlobKind::Upload, upload)),
            (Key::Blob(EMPTY), body(BlobKind::Message, empty)),
            (
                Key::Lease(UPLOAD),
                Row::Lease(LeaseRow {
                    account: ACCOUNT,
                    device: DeviceId::from_bytes([11; 16]),
                    expires_at: -1,
                    uses: LeaseUse::Both,
                }),
            ),
            (Key::Submission(FIRST), submission()),
            (Key::Submission(SECOND), Row::Submission(completed)),
            (Key::Recipient(FIRST, 0), Row::Recipient(queued())),
            (Key::Recipient(SECOND, 0), Row::Recipient(canceled)),
        ],
        &[],
        &mut [
            BlobSource {
                id: BODY,
                source: &mut bytes,
            },
            BlobSource {
                id: UPLOAD,
                source: &mut upload,
            },
            BlobSource {
                id: EMPTY,
                source: &mut empty,
            },
        ],
    );
    let mut foreign = b"foreign".as_slice();
    let mut foreign_upload = b"up".as_slice();
    commit(
        &store,
        OTHER,
        0,
        &[
            (Key::Blob(BODY), body(BlobKind::Message, foreign)),
            (Key::Blob(UPLOAD), body(BlobKind::Upload, foreign_upload)),
        ],
        &[],
        &mut [
            BlobSource {
                id: BODY,
                source: &mut foreign,
            },
            BlobSource {
                id: UPLOAD,
                source: &mut foreign_upload,
            },
        ],
    );
    let mut old = store.view(ACCOUNT, deadline()).unwrap();
    let first = old.logical_usage().unwrap();
    assert_eq!(amounts(first), [8, 3, 5, 3, 2]);
    assert_eq!(first.identity, old.identity());
    assert_eq!(
        amounts(
            store
                .view(OTHER, deadline())
                .unwrap()
                .logical_usage()
                .unwrap()
        ),
        [9, 2, 0, 0, 0]
    );
    commit(
        &store,
        ACCOUNT,
        1,
        &[
            (Key::Submission(FIRST), Row::Submission(completed)),
            (Key::Recipient(FIRST, 0), Row::Recipient(canceled)),
        ],
        &[],
        &mut [],
    );
    assert_eq!(
        amounts(
            store
                .view(ACCOUNT, deadline())
                .unwrap()
                .logical_usage()
                .unwrap()
        ),
        [8, 3, 5, 3, 2]
    );
    commit(
        &store,
        ACCOUNT,
        2,
        &[],
        &[
            Key::Recipient(FIRST, 0),
            Key::Submission(FIRST),
            Key::Lease(UPLOAD),
        ],
        &mut [],
    );
    assert_eq!(
        amounts(
            store
                .view(ACCOUNT, deadline())
                .unwrap()
                .logical_usage()
                .unwrap()
        ),
        [8, 3, 0, 3, 1]
    );
    commit(
        &store,
        ACCOUNT,
        3,
        &[],
        &[Key::Blob(UPLOAD), Key::Blob(EMPTY)],
        &mut [],
    );
    let current = store
        .view(ACCOUNT, deadline())
        .unwrap()
        .logical_usage()
        .unwrap();
    assert_eq!(amounts(current), [3, 1, 0, 3, 1]);
    assert_eq!(current.identity.committed_sequence, Sequence::from_u64(4));
    commit(
        &store,
        ACCOUNT,
        4,
        &[],
        &[Key::Recipient(SECOND, 0), Key::Submission(SECOND)],
        &mut [],
    );
    assert_eq!(
        amounts(
            store
                .view(ACCOUNT, deadline())
                .unwrap()
                .logical_usage()
                .unwrap()
        ),
        [3, 1, 0, 0, 0]
    );
    assert_eq!(old.logical_usage().unwrap(), first);
    drop(old);
    store.validate_integrity(deadline()).unwrap();
}
#[test]
fn usage_preserves_original_deadline_and_vm_failure_without_partial_totals() {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let clock = Arc::new(Timer(AtomicU64::new(1)));
    let store = IndexStore::create(
        &mut root,
        StoreEpoch::from_bytes([21; 16]),
        clock.clone(),
        2,
        deadline(),
    )
    .unwrap();
    store.create_account(ACCOUNT, deadline()).unwrap();
    let mut expired = store.view(ACCOUNT, deadline()).unwrap();
    clock.0.store(100, Ordering::Relaxed);
    assert_eq!(expired.logical_usage(), Err(ports::Error::Deadline));
    clock.0.store(1, Ordering::Relaxed);
    assert_eq!(expired.logical_usage(), Err(ports::Error::Deadline));
    let mut exhausted = store.view(ACCOUNT, deadline()).unwrap();
    lock(&exhausted.native().unwrap().scope.budget)
        .unwrap()
        .remaining = 0;
    assert_eq!(exhausted.logical_usage(), Err(ports::Error::Capacity));
    assert_eq!(exhausted.logical_usage(), Err(ports::Error::Capacity));
}

#[test]
fn usage_queries_keep_indexed_account_and_category_probes() {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let store = IndexStore::create(
        &mut root,
        StoreEpoch::from_bytes([22; 16]),
        Arc::new(Timer(AtomicU64::new(1))),
        1,
        deadline(),
    )
    .unwrap();
    let writer = lock(&store.writer).unwrap();
    let db = lock(&writer.native.connection).unwrap();
    for query in [BODIES, SUBMISSIONS] {
        let mut statement = db.prepare(&format!("EXPLAIN QUERY PLAN {query}")).unwrap();
        let details: Vec<String> = statement
            .query_map(params![ACCOUNT.as_bytes().as_slice()], |row| row.get(3))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert!(
            details.iter().all(|detail| !detail.contains("SCAN ")
                && !detail.contains("AUTOMATIC")
                && !detail.contains("TEMP B-TREE")),
            "{details:?}"
        );
        if query == BODIES {
            for required in ["SEARCH b ", "SEARCH l USING PRIMARY KEY (account=? AND blob_id=?)", "SEARCH s USING COVERING INDEX submissions_blob (account=? AND transmitted_blob_id=?)"] {
                assert!(details.iter().any(|detail| detail.contains(required)), "missing {required}: {details:?}");
            }
        } else {
            assert!(
                details
                    .iter()
                    .any(|detail| detail.starts_with("SEARCH submissions ")
                        && detail.contains("(account=?)")),
                "{details:?}"
            );
        }
    }
}

#[test]
fn populated_scan_and_second_aggregate_consume_the_original_vm_allowance() {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let store = IndexStore::create(
        &mut root,
        StoreEpoch::from_bytes([23; 16]),
        Arc::new(Timer(AtomicU64::new(1))),
        1,
        deadline(),
    )
    .unwrap();
    store.create_account(ACCOUNT, deadline()).unwrap();
    let mut empty = store.view(ACCOUNT, deadline()).unwrap();
    let before = lock(&empty.native().unwrap().scope.budget)
        .unwrap()
        .remaining;
    empty.logical_usage().unwrap();
    let empty_steps = before
        - lock(&empty.native().unwrap().scope.budget)
            .unwrap()
            .remaining;
    drop(empty);
    let ids: Vec<_> = (0u128..100)
        .map(|id| BlobId::from_bytes(id.to_be_bytes()))
        .collect();
    let mut puts = Vec::new();
    for (ordinal, id) in ids.iter().copied().enumerate() {
        let kind = if ordinal % 2 == 0 {
            BlobKind::Upload
        } else {
            BlobKind::Message
        };
        puts.push((Key::Blob(id), body(kind, b"")));
        if kind == BlobKind::Upload {
            puts.push((
                Key::Lease(id),
                Row::Lease(LeaseRow {
                    account: ACCOUNT,
                    device: DeviceId::from_bytes([11; 16]),
                    expires_at: -1,
                    uses: LeaseUse::Both,
                }),
            ));
        } else {
            let Row::Submission(mut row) = submission() else {
                panic!("submission fixture");
            };
            row.transmitted_blob = id;
            let submission_id = SubmissionId::from_bytes(*id.as_bytes());
            puts.push((Key::Submission(submission_id), Row::Submission(row)));
            puts.push((Key::Recipient(submission_id, 0), Row::Recipient(queued())));
        }
    }
    let mut streams = vec![b"".as_slice(); ids.len()];
    let mut sources: Vec<_> = ids
        .iter()
        .zip(streams.iter_mut())
        .map(|(id, source)| BlobSource { id: *id, source })
        .collect();
    commit(&store, ACCOUNT, 0, &puts, &[], &mut sources);
    let mut limited = store.view(ACCOUNT, deadline()).unwrap();
    lock(&limited.native().unwrap().scope.budget)
        .unwrap()
        .remaining = empty_steps + 32;
    assert_eq!(limited.logical_usage(), Err(ports::Error::Capacity));
    assert_eq!(limited.logical_usage(), Err(ports::Error::Capacity));
    drop(limited);
    // Calibrate the exact first aggregate's VM cost on the same populated snapshot.
    let mut measured = store.view(ACCOUNT, deadline()).unwrap();
    let before = lock(&measured.native().unwrap().scope.budget)
        .unwrap()
        .remaining;
    measured
        .read_snapshot(|native| {
            native.run(|db| {
                db.query_row(BODIES, params![ACCOUNT.as_bytes().as_slice()], |row| {
                    row.get::<_, i64>(0)
                })
                .map_err(sql)
            })
        })
        .unwrap();
    let body_steps = before
        - lock(&measured.native().unwrap().scope.budget)
            .unwrap()
            .remaining;
    drop(measured);
    let mut second = store.view(ACCOUNT, deadline()).unwrap();
    lock(&second.native().unwrap().scope.budget)
        .unwrap()
        .remaining = body_steps;
    assert_eq!(second.logical_usage(), Err(ports::Error::Capacity));
    assert_eq!(second.logical_usage(), Err(ports::Error::Capacity));
    drop(second);
    assert_eq!(
        amounts(
            store
                .view(ACCOUNT, deadline())
                .unwrap()
                .logical_usage()
                .unwrap()
        ),
        [0, 100, 0, 0, 50]
    );
}

#[test]
fn whole_store_usage_fence_keeps_writes_busy_but_old_views_readable() {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let epoch = StoreEpoch::from_bytes([24; 16]);
    let store = IndexStore::create(
        &mut root,
        epoch,
        Arc::new(Timer(AtomicU64::new(1))),
        2,
        deadline(),
    )
    .unwrap();
    let empty = store.usage_fence(deadline()).unwrap();
    assert_eq!(empty.usage().epoch, epoch);
    assert_eq!(empty.usage().accounts, 0);
    assert_eq!(empty.usage().blob_count, 0);
    drop(empty);
    for account in [ACCOUNT, OTHER] {
        store.create_account(account, deadline()).unwrap();
        let mut bytes = b"same ID, distinct account".as_slice();
        commit(
            &store,
            account,
            0,
            &[(Key::Blob(BODY), body(BlobKind::Message, bytes))],
            &[],
            &mut [BlobSource {
                id: BODY,
                source: &mut bytes,
            }],
        );
    }
    let mut old = store.view(ACCOUNT, deadline()).unwrap();
    let original = old.logical_usage().unwrap();
    let fence = store.usage_fence(deadline()).unwrap();
    let total = fence.usage();
    assert_eq!(total.epoch, epoch);
    assert_eq!(total.accounts, 2);
    assert_eq!(total.body_bytes, 2 * original.body_bytes);
    assert_eq!(total.blob_count, 2);
    assert_eq!(
        (
            total.upload_bytes,
            total.queue_bytes,
            total.queue_submissions
        ),
        (0, 0, 0)
    );
    assert_eq!(
        store.view(ACCOUNT, deadline()).err(),
        Some(ports::Error::Busy)
    );
    assert_eq!(
        store.create_account(AccountId::from_bytes([33; 16]), deadline()),
        Err(CommitError::Rejected(ports::Error::Busy))
    );
    assert_eq!(store.checkpoint(deadline()), Err(ports::Error::Busy));
    let remove = Operation::delete(Table::Blobs, BODY.as_bytes()).unwrap();
    let request = CommitRequest {
        account: ACCOUNT,
        epoch: store.epoch(),
        expected: Sequence::from_u64(1),
        utc_ms: 0,
        deadline: deadline(),
    };
    assert_eq!(
        store.commit(&td_crypto::Provider, request, &[remove], &mut []),
        Err(CommitError::Rejected(ports::Error::Busy))
    );
    assert_eq!(old.logical_usage().unwrap(), original);
    drop(fence);
    assert_eq!(
        store.commit(&td_crypto::Provider, request, &[remove], &mut []),
        Ok(Sequence::from_u64(2))
    );
    assert_eq!(old.logical_usage().unwrap(), original);
    drop(old);
    let after = store.usage_fence(deadline()).unwrap();
    assert_eq!(after.usage().blob_count, 1);
    assert_eq!(after.usage().body_bytes, original.body_bytes);
}

#[test]
fn usage_fence_query_failure_rolls_back_and_cleanup_failure_stops_the_writer() {
    for refuse_cleanup in [false, true] {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let clock = Arc::new(Timer(AtomicU64::new(1)));
        let store = IndexStore::create(
            &mut root,
            StoreEpoch::from_bytes([25; 16]),
            clock.clone(),
            1,
            deadline(),
        )
        .unwrap();
        store.create_account(ACCOUNT, deadline()).unwrap();
        let events = Arc::new(AtomicU64::new(0));
        let hits = Arc::clone(&events);
        lock(&lock(&store.writer).unwrap().native.connection)
            .unwrap()
            .authorizer(Some(move |context: AuthContext<'_>| {
                if refuse_cleanup {
                    if matches!(context.action, AuthAction::Transaction { .. })
                        && hits.fetch_add(1, Ordering::Relaxed) == 1
                    {
                        return Authorization::Deny;
                    }
                } else if matches!(
                    context.action,
                    AuthAction::Read {
                        table_name: "blobs",
                        ..
                    }
                ) {
                    hits.fetch_add(1, Ordering::Relaxed);
                    return Authorization::Deny;
                }
                Authorization::Allow
            }))
            .unwrap();
        assert_eq!(
            store.usage_fence(deadline()).err(),
            Some(if refuse_cleanup {
                ports::Error::WriterStopped
            } else {
                ports::Error::Io {
                    kind: std::io::ErrorKind::Other,
                    os_code: None,
                }
            })
        );
        assert!(events.load(Ordering::Relaxed) > 0);
        {
            let writer = lock(&store.writer).unwrap();
            let db = lock(&writer.native.connection).unwrap();
            assert_eq!(writer.stopped, refuse_cleanup);
            assert_eq!(db.is_autocommit(), !refuse_cleanup);
            db.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
                .unwrap();
        }
        if refuse_cleanup {
            assert_eq!(
                store.usage_fence(deadline()).err(),
                Some(ports::Error::WriterStopped)
            );
        } else {
            assert_eq!(store.usage_fence(deadline()).unwrap().usage().accounts, 1);
            clock.0.store(100, Ordering::Relaxed);
            assert_eq!(
                store.usage_fence(deadline()).err(),
                Some(ports::Error::Deadline)
            );
        }
    }
}

#[test]
fn usage_fence_and_creation_share_the_full_account_ceiling() {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let store = IndexStore::create(
        &mut root,
        StoreEpoch::from_bytes([26; 16]),
        Arc::new(Timer(AtomicU64::new(1))),
        1,
        deadline(),
    )
    .unwrap();
    for ordinal in 0..MAX_ACCOUNTS {
        store
            .create_account(
                AccountId::from_bytes(u128::from(ordinal).to_be_bytes()),
                deadline(),
            )
            .unwrap();
    }
    assert_eq!(
        store.usage_fence(deadline()).unwrap().usage().accounts,
        MAX_ACCOUNTS
    );
    assert_eq!(
        store.create_account(
            AccountId::from_bytes(u128::from(MAX_ACCOUNTS).to_be_bytes()),
            deadline()
        ),
        Err(CommitError::Rejected(ports::Error::Capacity))
    );
    assert_eq!(
        store.usage_fence(deadline()).unwrap().usage().accounts,
        MAX_ACCOUNTS
    );
}
#[test]
fn dropping_old_views_under_the_fence_returns_or_retires_their_pool_slot() {
    for refuse_cleanup in [false, true] {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let store = IndexStore::create(
            &mut root,
            StoreEpoch::from_bytes([27; 16]),
            Arc::new(Timer(AtomicU64::new(1))),
            1,
            deadline(),
        )
        .unwrap();
        store.create_account(ACCOUNT, deadline()).unwrap();
        let view = store.view(ACCOUNT, deadline()).unwrap();
        if refuse_cleanup {
            lock(&view.native().unwrap().connection)
                .unwrap()
                .authorizer(Some(|context: AuthContext<'_>| {
                    if matches!(context.action, AuthAction::Transaction { .. }) {
                        Authorization::Deny
                    } else {
                        Authorization::Allow
                    }
                }))
                .unwrap();
        }
        let fence = store.usage_fence(deadline()).unwrap();
        drop(view);
        assert_eq!(
            matches!(
                lock(&store.readers).unwrap().first(),
                Some(ReaderSlot::Retired)
            ),
            refuse_cleanup
        );
        drop(fence);
        if refuse_cleanup {
            assert_eq!(
                store.view(ACCOUNT, deadline()).err(),
                Some(ports::Error::Busy)
            );
        } else {
            drop(store.view(ACCOUNT, deadline()).unwrap());
        }
    }
}
#[test]
fn whole_store_vm_allowance_is_not_refreshed_between_accounts() {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let store = IndexStore::create(
        &mut root,
        StoreEpoch::from_bytes([28; 16]),
        Arc::new(Timer(AtomicU64::new(1))),
        1,
        deadline(),
    )
    .unwrap();
    store.create_account(ACCOUNT, deadline()).unwrap();
    drop(store.usage_fence(deadline()).unwrap());
    let one_account = VM_STEPS
        - lock(&lock(&store.writer).unwrap().native.scope.budget)
            .unwrap()
            .remaining;
    store.create_account(OTHER, deadline()).unwrap();
    let budget = Arc::clone(&lock(&store.writer).unwrap().native.scope.budget);
    let events = Arc::new(AtomicU64::new(0));
    let hits = Arc::clone(&events);
    lock(&lock(&store.writer).unwrap().native.connection)
        .unwrap()
        .authorizer(Some(move |context: AuthContext<'_>| {
            if matches!(context.action, AuthAction::Transaction { .. })
                && hits.fetch_add(1, Ordering::Relaxed) == 0
            {
                lock(&budget).unwrap().remaining = one_account + 8;
            }
            Authorization::Allow
        }))
        .unwrap();
    assert_eq!(
        store.usage_fence(deadline()).err(),
        Some(ports::Error::Capacity)
    );
    let writer = lock(&store.writer).unwrap();
    assert!(!writer.stopped);
    let db = lock(&writer.native.connection).unwrap();
    assert!(db.is_autocommit());
    db.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
        .unwrap();
    drop(db);
    drop(writer);
    assert_eq!(events.load(Ordering::Relaxed), 2);
    assert_eq!(store.usage_fence(deadline()).unwrap().usage().accounts, 2);
}
#[test]
fn expiry_during_capture_or_cleanup_rolls_back_without_stopping_the_writer() {
    for during_cleanup in [false, true] {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let clock = Arc::new(Timer(AtomicU64::new(1)));
        let store = IndexStore::create(
            &mut root,
            StoreEpoch::from_bytes([29; 16]),
            clock.clone(),
            1,
            deadline(),
        )
        .unwrap();
        store.create_account(ACCOUNT, deadline()).unwrap();
        let timer = Arc::clone(&clock);
        let events = Arc::new(AtomicU64::new(0));
        let hits = Arc::clone(&events);
        lock(&lock(&store.writer).unwrap().native.connection)
            .unwrap()
            .authorizer(Some(move |context: AuthContext<'_>| {
                let expire = if during_cleanup {
                    matches!(context.action, AuthAction::Transaction { .. })
                        && hits.fetch_add(1, Ordering::Relaxed) == 1
                } else if matches!(
                    context.action,
                    AuthAction::Read {
                        table_name: "blobs",
                        ..
                    }
                ) {
                    hits.fetch_add(1, Ordering::Relaxed);
                    true
                } else {
                    false
                };
                if expire {
                    timer.0.store(100, Ordering::Relaxed);
                }
                Authorization::Allow
            }))
            .unwrap();
        assert_eq!(
            store.usage_fence(deadline()).err(),
            Some(ports::Error::Deadline)
        );
        assert!(events.load(Ordering::Relaxed) > 0);
        let writer = lock(&store.writer).unwrap();
        assert!(!writer.stopped);
        let db = lock(&writer.native.connection).unwrap();
        assert!(db.is_autocommit());
        db.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
            .unwrap();
        drop(db);
        drop(writer);
        clock.0.store(1, Ordering::Relaxed);
        assert_eq!(store.usage_fence(deadline()).unwrap().usage().accounts, 1);
    }
}

#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{
    format::row::{BlobKind, BlobRow, MailboxRow},
    ids::MailboxId,
    ports::{Crypto, Tick, Time},
    store_fs::tests::Fixture,
    sync::{DataState, DataType},
};
use std::sync::atomic::{AtomicU64, Ordering};

const ACCOUNT: AccountId = AccountId::from_bytes([1; 16]);
const OTHER: AccountId = AccountId::from_bytes([2; 16]);
const MAILBOX: MailboxId = MailboxId::from_bytes([3; 16]);
const BODY: BlobId = BlobId::from_bytes([4; 16]);
const DELETED: BlobId = BlobId::from_bytes([5; 16]);
const EPOCH: StoreEpoch = StoreEpoch::from_bytes([9; 16]);
const FRESH: StoreEpoch = StoreEpoch::from_bytes([10; 16]);
const RAW: &[u8] = b"Subject: restore\r\n\r\nretained body\r\n";

struct Timer(AtomicU64);
impl Clock for Timer {
    fn sample(&self) -> Result<Time, ports::Error> {
        Ok(Time {
            utc_ms: 0,
            monotonic: Tick(self.0.load(Ordering::Relaxed)),
        })
    }
}
fn clock() -> Arc<Timer> {
    Arc::new(Timer(AtomicU64::new(1)))
}
fn deadline() -> Deadline {
    Deadline::after(Tick(0), 100).unwrap()
}
struct Entropy {
    bytes: [u8; 16],
    calls: usize,
    fail: bool,
    clock: Option<(Arc<Timer>, u64)>,
}
impl Entropy {
    fn fresh() -> Self {
        Self {
            bytes: *FRESH.as_bytes(),
            calls: 0,
            fail: false,
            clock: None,
        }
    }
}
impl ports::Entropy for Entropy {
    fn fill(&mut self, output: &mut [u8]) -> Result<(), ports::CryptoError> {
        assert_eq!(output.len(), 16);
        self.calls += 1;
        if let Some((clock, tick)) = &self.clock {
            clock.0.store(*tick, Ordering::Relaxed);
        }
        if self.fail {
            output[..8].copy_from_slice(&self.bytes[..8]);
            return Err(ports::CryptoError::Entropy);
        }
        output.copy_from_slice(&self.bytes);
        Ok(())
    }
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

fn commit_form(
    store: &IndexStore<'_>,
    request: CommitRequest,
    operations: &[Operation<'_>],
    source: &mut std::io::Cursor<&[u8]>,
    encoded: bool,
) -> Result<Sequence, CommitError> {
    let mut sources = [BlobSource {
        id: BlobId::from_bytes([11; 16]),
        source,
    }];
    if !encoded {
        return store.commit(&td_crypto::Provider, request, operations, &mut sources);
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
    store.commit_batch(&td_crypto::Provider, request, &batch, &mut sources)
}

fn stale_epoch_commit(encoded: bool) {
    let source = Fixture::new();
    let destination = Fixture::new();
    let mut source_root = source.locked();
    let mut destination_root = destination.locked();
    let store = IndexStore::create(&mut source_root, EPOCH, clock(), 2, deadline()).unwrap();
    seed(&store);
    let captured = store.view(ACCOUNT, deadline()).unwrap().identity();
    let stale = CommitRequest {
        account: captured.account,
        epoch: captured.epoch,
        expected: captured.committed_sequence,
        utc_ms: 0,
        deadline: deadline(),
    };
    store
        .backup(&mut destination_root, deadline(), &mut [0; 65536])
        .unwrap();
    let restored = IndexStore::open(&mut destination_root, clock(), 2, deadline()).unwrap();
    let restored = restored
        .renew_epoch(&mut Entropy::fresh(), deadline())
        .unwrap();
    drop(restored);
    let restored = IndexStore::open(&mut destination_root, clock(), 2, deadline()).unwrap();
    inspect(&restored, FRESH);
    let current = restored.view(ACCOUNT, deadline()).unwrap().identity();
    assert_eq!(current.committed_sequence, stale.expected);
    assert_ne!(current.epoch, stale.epoch);

    let id = BlobId::from_bytes([11; 16]);
    let body = encode(Row::Blob(blob()));
    let value = encode(mailbox("current epoch commit"));
    let operations = [
        Operation::put(Table::Blobs, id.as_bytes(), &body).unwrap(),
        Operation::put(Table::Mailboxes, MAILBOX.as_bytes(), &value).unwrap(),
        Operation::change(
            ObjectType::Mailbox,
            ChangeAction::Updated,
            MAILBOX.as_bytes(),
        ),
    ];
    let mut input = std::io::Cursor::new(RAW);
    assert_eq!(
        commit_form(&restored, stale, &operations, &mut input, encoded),
        Err(CommitError::Rejected(ports::Error::Conflict))
    );
    assert_eq!(input.position(), 0);
    inspect(&restored, FRESH);
    let mut bytes = [0; 128];
    assert!(restored
        .view(ACCOUNT, deadline())
        .unwrap()
        .get(Key::Blob(id), &mut bytes)
        .unwrap()
        .is_none());

    let active = CommitRequest {
        epoch: current.epoch,
        ..stale
    };
    assert_eq!(
        commit_form(
            &restored,
            CommitRequest {
                expected: Sequence::from_u64(1),
                ..active
            },
            &operations,
            &mut input,
            encoded,
        ),
        Err(CommitError::Rejected(ports::Error::Conflict))
    );
    assert_eq!(input.position(), 0);
    assert_eq!(
        commit_form(&restored, active, &operations, &mut input, encoded),
        Ok(Sequence::from_u64(3))
    );
    assert_eq!(input.position(), RAW.len() as u64);
    restored.checkpoint(deadline()).unwrap();
    drop(restored);
    let restored = IndexStore::open(&mut destination_root, clock(), 2, deadline()).unwrap();
    restored.validate_integrity(deadline()).unwrap();
    let mut view = restored.view(ACCOUNT, deadline()).unwrap();
    assert_eq!(view.identity().epoch, FRESH);
    assert_eq!(view.identity().committed_sequence, Sequence::from_u64(3));
    assert_eq!(view.identity().history_floor, captured.history_floor);
    assert_eq!(
        view.get(Key::Mailbox(MAILBOX), &mut bytes).unwrap(),
        Some((mailbox("current epoch commit"), Sequence::from_u64(3)))
    );
    assert_eq!(
        view.next_change(
            ChangeCursor {
                sequence: stale.expected,
                operation: u32::MAX
            },
            ObjectType::Mailbox,
        )
        .unwrap(),
        ChangeStep::Record(ChangeRecord {
            cursor: ChangeCursor {
                sequence: Sequence::from_u64(3),
                operation: 2
            },
            change: Change {
                kind: ObjectType::Mailbox,
                action: ChangeAction::Updated,
                id: *MAILBOX.as_bytes(),
            },
        })
    );
    let mut body = view
        .open_blob_input(&td_crypto::Provider, id, RAW.len() as u64)
        .unwrap();
    assert_eq!(body.read(&mut bytes).unwrap(), RAW.len());
    assert_eq!(&bytes[..RAW.len()], RAW);
    body.finish().unwrap();
    let original = IndexStore::open(&mut source_root, clock(), 2, deadline()).unwrap();
    inspect(&original, EPOCH);
}

#[test]
fn typed_commit_refuses_a_pre_restore_epoch_before_reading_a_body() {
    stale_epoch_commit(false);
}

#[test]
fn encoded_commit_refuses_a_pre_restore_epoch_before_reading_a_body() {
    stale_epoch_commit(true);
}

#[test]
fn pruning_refuses_a_pre_restore_epoch_without_retiring_copied_history() {
    let source = Fixture::new();
    let destination = Fixture::new();
    let mut source_root = source.locked();
    let mut destination_root = destination.locked();
    let store = IndexStore::create(&mut source_root, EPOCH, clock(), 2, deadline()).unwrap();
    seed(&store);
    let captured = store.view(ACCOUNT, deadline()).unwrap().identity();
    let stale = HistoryPruneRequest {
        account: captured.account,
        epoch: captured.epoch,
        expected: captured.committed_sequence,
        through: captured.committed_sequence,
        max_rows: 1,
        deadline: deadline(),
    };
    store
        .backup(&mut destination_root, deadline(), &mut [0; 65536])
        .unwrap();
    let copied = IndexStore::open(&mut destination_root, clock(), 2, deadline()).unwrap();
    let copied = copied
        .renew_epoch(&mut Entropy::fresh(), deadline())
        .unwrap();
    drop(copied);
    let copied = IndexStore::open(&mut destination_root, clock(), 2, deadline()).unwrap();
    inspect(&copied, FRESH);
    let current = copied.view(ACCOUNT, deadline()).unwrap().identity();
    assert_eq!(current.committed_sequence, stale.expected);
    assert_ne!(current.epoch, stale.epoch);
    assert_eq!(
        copied.prune_history(stale),
        Err(CommitError::Rejected(ports::Error::Conflict))
    );
    inspect(&copied, FRESH);
    let active = HistoryPruneRequest {
        epoch: current.epoch,
        ..stale
    };
    assert_eq!(
        copied.prune_history(HistoryPruneRequest {
            expected: Sequence::from_u64(1),
            through: Sequence::from_u64(1),
            ..active
        }),
        Err(CommitError::Rejected(ports::Error::Conflict))
    );
    inspect(&copied, FRESH);
    let pruned = ViewIdentity {
        history_floor: stale.through,
        ..current
    };
    assert_eq!(
        copied.prune_history(active).unwrap(),
        HistoryPruned {
            identity: pruned,
            removed: 1,
            more: false
        }
    );
    assert_eq!(
        copied.prune_history(active).unwrap(),
        HistoryPruned {
            identity: pruned,
            removed: 0,
            more: false
        }
    );
    copied.checkpoint(deadline()).unwrap();
    drop(copied);
    let copied = IndexStore::open(&mut destination_root, clock(), 2, deadline()).unwrap();
    copied.validate_integrity(deadline()).unwrap();
    let mut view = copied.view(ACCOUNT, deadline()).unwrap();
    assert_eq!(view.identity(), pruned);
    let mut bytes = [0; 128];
    assert_eq!(
        view.get(Key::Mailbox(MAILBOX), &mut bytes).unwrap(),
        Some((mailbox("current"), captured.committed_sequence))
    );
    assert_eq!(
        view.next_change(
            ChangeCursor {
                sequence: captured.history_floor,
                operation: u32::MAX
            },
            ObjectType::Mailbox
        ),
        Err(ports::Error::HistoryLost)
    );
    assert_eq!(
        view.next_change(
            ChangeCursor {
                sequence: stale.through,
                operation: u32::MAX
            },
            ObjectType::Mailbox
        )
        .unwrap(),
        ChangeStep::Complete
    );
    let mut body = view
        .open_blob_input(&td_crypto::Provider, BODY, RAW.len() as u64)
        .unwrap();
    assert_eq!(body.read(&mut bytes).unwrap(), RAW.len());
    assert_eq!(&bytes[..RAW.len()], RAW);
    body.finish().unwrap();
    let original = IndexStore::open(&mut source_root, clock(), 2, deadline()).unwrap();
    inspect(&original, EPOCH);
}
fn mailbox(name: &str) -> Row<'_> {
    Row::Mailbox(MailboxRow {
        name,
        parent: None,
        role: None,
        sort_order: 0,
        subscribed: true,
    })
}
fn encode(row: Row<'_>) -> Vec<u8> {
    let mut bytes = vec![0; 128];
    let length = row.encode(&mut bytes).unwrap();
    bytes.truncate(length);
    bytes
}
fn blob() -> BlobRow {
    let mut digest = td_crypto::Provider.sha256().unwrap();
    digest.update(RAW).unwrap();
    BlobRow {
        kind: BlobKind::Message,
        length: RAW.len() as u64,
        digest: digest.finish().unwrap(),
        created_at: 0,
    }
}
fn seed(store: &IndexStore<'_>) {
    store.create_account(ACCOUNT, deadline()).unwrap();
    store.create_account(OTHER, deadline()).unwrap();
    let value = encode(mailbox("first"));
    let body = encode(Row::Blob(blob()));
    let ops = [
        Operation::put(Table::Mailboxes, MAILBOX.as_bytes(), &value).unwrap(),
        Operation::put(Table::Blobs, BODY.as_bytes(), &body).unwrap(),
        Operation::put(Table::Blobs, DELETED.as_bytes(), &body).unwrap(),
        Operation::change(
            ObjectType::Mailbox,
            ChangeAction::Created,
            MAILBOX.as_bytes(),
        ),
    ];
    store
        .commit(
            &td_crypto::Provider,
            request(0),
            &ops,
            &mut [
                BlobSource {
                    id: BODY,
                    source: &mut std::io::Cursor::new(RAW),
                },
                BlobSource {
                    id: DELETED,
                    source: &mut std::io::Cursor::new(RAW),
                },
            ],
        )
        .unwrap();
    let value = encode(mailbox("current"));
    let ops = [
        Operation::delete(Table::Blobs, DELETED.as_bytes()).unwrap(),
        Operation::put(Table::Mailboxes, MAILBOX.as_bytes(), &value).unwrap(),
        Operation::change(
            ObjectType::Mailbox,
            ChangeAction::Updated,
            MAILBOX.as_bytes(),
        ),
    ];
    store
        .commit(&td_crypto::Provider, request(1), &ops, &mut [])
        .unwrap();
    store
        .prune_history(HistoryPruneRequest {
            account: ACCOUNT,
            epoch: store.epoch(),
            expected: Sequence::from_u64(2),
            through: Sequence::from_u64(1),
            max_rows: 4096,
            deadline: deadline(),
        })
        .unwrap();
}
fn inspect(store: &IndexStore<'_>, epoch: StoreEpoch) {
    assert_eq!(store.epoch(), epoch);
    let mut view = store.view(ACCOUNT, deadline()).unwrap();
    assert_eq!(
        view.identity(),
        ViewIdentity {
            account: ACCOUNT,
            epoch,
            committed_sequence: Sequence::from_u64(2),
            history_floor: Sequence::from_u64(1),
        }
    );
    let mut bytes = [0; 128];
    assert_eq!(
        view.get(Key::Mailbox(MAILBOX), &mut bytes).unwrap(),
        Some((mailbox("current"), Sequence::from_u64(2)))
    );
    assert_eq!(
        view.get(Key::Blob(BODY), &mut bytes).unwrap(),
        Some((Row::Blob(blob()), Sequence::from_u64(1)))
    );
    assert!(view.get(Key::Blob(DELETED), &mut bytes).unwrap().is_none());
    let cursor = ChangeCursor {
        sequence: Sequence::from_u64(2),
        operation: 2,
    };
    assert_eq!(
        view.next_change(
            ChangeCursor {
                sequence: Sequence::from_u64(1),
                operation: u32::MAX
            },
            ObjectType::Mailbox
        )
        .unwrap(),
        ChangeStep::Record(ChangeRecord {
            cursor,
            change: Change {
                kind: ObjectType::Mailbox,
                action: ChangeAction::Updated,
                id: *MAILBOX.as_bytes()
            },
        })
    );
    assert_eq!(
        view.next_change(cursor, ObjectType::Mailbox).unwrap(),
        ChangeStep::Complete
    );
    let mut input = view
        .open_blob_input(&td_crypto::Provider, BODY, RAW.len() as u64)
        .unwrap();
    assert_eq!(input.read(&mut bytes).unwrap(), RAW.len());
    assert_eq!(&bytes[..RAW.len()], RAW);
    let mut body = input.finish().unwrap();
    assert_eq!(
        ports::BlobReader::read_at(&mut body, 0, &mut bytes).unwrap(),
        RAW.len()
    );
    assert_eq!(&bytes[..RAW.len()], RAW);
    drop(body);
    drop(view);
    let other = store.view(OTHER, deadline()).unwrap();
    assert_eq!(
        other.identity(),
        ViewIdentity {
            account: OTHER,
            epoch,
            committed_sequence: Sequence::default(),
            history_floor: Sequence::default()
        }
    );
    drop(other);
    let writer = lock(&store.writer).unwrap();
    assert!(lock(&writer.native.connection).unwrap().is_autocommit());
    writer.native.begin_work(deadline()).unwrap();
    const COUNTS: &str = concat!(
        "SELECT (SELECT count(*) FROM blob_ids), ",
        "(SELECT count(*) FROM changes), (SELECT count(*) FROM accounts)"
    );
    let counts: (i64, i64, i64) = writer
        .native
        .run(|db| {
            db.query_row(COUNTS, [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .map_err(sql)
        })
        .unwrap();
    assert_eq!(counts, (2, 1, 2));
}

#[test]
fn restored_snapshot_changes_only_epoch_and_reopens_with_new_identity() {
    let source = Fixture::new();
    let destination = Fixture::new();
    let mut source_root = source.locked();
    let mut destination_root = destination.locked();
    let timer = clock();
    let store = IndexStore::create(&mut source_root, EPOCH, timer.clone(), 2, deadline()).unwrap();
    seed(&store);
    inspect(&store, EPOCH);
    let original = store.view(ACCOUNT, deadline()).unwrap().identity();
    let old = DataState {
        account: original.account,
        epoch: original.epoch,
        kind: DataType::Mailbox,
        sequence: original.committed_sequence,
    };
    let receipt = store
        .backup(&mut destination_root, deadline(), &mut [0; 65536])
        .unwrap();
    assert_eq!(receipt.epoch, EPOCH);
    let restored = IndexStore::open(&mut destination_root, timer.clone(), 2, deadline()).unwrap();
    inspect(&restored, EPOCH);
    let mut entropy = Entropy::fresh();
    let restored = restored.renew_epoch(&mut entropy, deadline()).unwrap();
    assert_eq!(entropy.calls, 1);
    inspect(&restored, FRESH);
    let identity = restored.view(ACCOUNT, deadline()).unwrap().identity();
    let current = DataState {
        account: identity.account,
        epoch: identity.epoch,
        kind: DataType::Mailbox,
        sequence: identity.committed_sequence,
    };
    assert!(old.is_retained_for(old, original.history_floor));
    assert!(!old.is_retained_for(current, identity.history_floor));
    assert!(current.is_retained_for(current, identity.history_floor));
    drop(restored);
    let restored = IndexStore::open(&mut destination_root, timer.clone(), 2, deadline()).unwrap();
    inspect(&restored, FRESH);
    let value = encode(Row::Blob(blob()));
    assert_eq!(
        restored.commit(
            &td_crypto::Provider,
            CommitRequest {
                epoch: restored.epoch(),
                ..request(2)
            },
            &[Operation::put(Table::Blobs, DELETED.as_bytes(), &value).unwrap()],
            &mut [BlobSource {
                id: DELETED,
                source: &mut std::io::Cursor::new(RAW)
            }]
        ),
        Err(CommitError::Rejected(ports::Error::Conflict))
    );
    let value = encode(mailbox("after restore"));
    assert_eq!(
        restored.commit(
            &td_crypto::Provider,
            CommitRequest {
                epoch: restored.epoch(),
                ..request(2)
            },
            &[
                Operation::put(Table::Mailboxes, MAILBOX.as_bytes(), &value).unwrap(),
                Operation::change(
                    ObjectType::Mailbox,
                    ChangeAction::Updated,
                    MAILBOX.as_bytes()
                ),
            ],
            &mut []
        ),
        Ok(Sequence::from_u64(3))
    );
    assert_eq!(
        restored.view(ACCOUNT, deadline()).unwrap().identity().epoch,
        FRESH
    );
    let source = IndexStore::open(&mut source_root, timer, 1, deadline()).unwrap();
    inspect(&source, EPOCH);
}

#[test]
fn collision_and_partial_entropy_error_preserve_the_persisted_store() {
    for fail in [false, true] {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let timer = clock();
        let store = IndexStore::create(&mut root, EPOCH, timer.clone(), 1, deadline()).unwrap();
        seed(&store);
        let mut entropy = Entropy {
            fail,
            bytes: if fail {
                *FRESH.as_bytes()
            } else {
                *EPOCH.as_bytes()
            },
            ..Entropy::fresh()
        };
        assert_eq!(
            store.renew_epoch(&mut entropy, deadline()).map(|_| ()),
            Err(CommitError::Rejected(if fail {
                ports::Error::Entropy
            } else {
                ports::Error::Conflict
            }))
        );
        assert_eq!(entropy.calls, 1);
        let reopened = IndexStore::open(&mut root, timer, 1, deadline()).unwrap();
        inspect(&reopened, EPOCH);
    }
}

#[test]
fn exclusive_entry_refusals_do_not_request_entropy() {
    for reason in 0..3 {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let timer = clock();
        let store = IndexStore::create(&mut root, EPOCH, timer.clone(), 1, deadline()).unwrap();
        seed(&store);
        let expected = match reason {
            0 => {
                timer.0.store(100, Ordering::Relaxed);
                ports::Error::Deadline
            }
            1 => {
                lock(&store.writer).unwrap().stopped = true;
                ports::Error::WriterStopped
            }
            _ => {
                *lock(&store.readers).unwrap().first_mut().unwrap() = ReaderSlot::Borrowed;
                ports::Error::Busy
            }
        };
        let mut entropy = Entropy::fresh();
        assert_eq!(
            store.renew_epoch(&mut entropy, deadline()).map(|_| ()),
            Err(CommitError::Rejected(expected))
        );
        assert_eq!(entropy.calls, 0);
        timer.0.store(1, Ordering::Relaxed);
        let reopened = IndexStore::open(&mut root, timer, 1, deadline()).unwrap();
        inspect(&reopened, EPOCH);
    }
}

#[test]
fn entropy_handoff_retains_deadline_and_clock_even_when_fill_fails() {
    for tick in [0, 100] {
        for fail in [false, true] {
            let fixture = Fixture::new();
            let mut root = fixture.locked();
            let timer = clock();
            let store = IndexStore::create(&mut root, EPOCH, timer.clone(), 1, deadline()).unwrap();
            seed(&store);
            let mut entropy = Entropy {
                fail,
                clock: Some((timer.clone(), tick)),
                ..Entropy::fresh()
            };
            assert_eq!(
                store.renew_epoch(&mut entropy, deadline()).map(|_| ()),
                Err(CommitError::Rejected(if tick == 0 {
                    ports::Error::Invalid
                } else {
                    ports::Error::Deadline
                }))
            );
            assert_eq!(entropy.calls, 1);
            timer.0.store(1, Ordering::Relaxed);
            let reopened = IndexStore::open(&mut root, timer, 1, deadline()).unwrap();
            inspect(&reopened, EPOCH);
        }
    }
}

#[test]
fn persisted_epoch_mismatch_or_absence_is_not_overwritten() {
    for missing in [false, true] {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let timer = clock();
        let store = IndexStore::create(&mut root, EPOCH, timer.clone(), 1, deadline()).unwrap();
        seed(&store);
        {
            let writer = lock(&store.writer).unwrap();
            let db = lock(&writer.native.connection).unwrap();
            if missing {
                db.execute("DELETE FROM store", []).unwrap();
            } else {
                db.execute(
                    "UPDATE store SET epoch=?1",
                    [StoreEpoch::from_bytes([11; 16]).as_bytes().as_slice()],
                )
                .unwrap();
            }
        }
        let mut entropy = Entropy::fresh();
        assert_eq!(
            store.renew_epoch(&mut entropy, deadline()).map(|_| ()),
            Err(CommitError::Rejected(ports::Error::Corrupt))
        );
        assert_eq!(entropy.calls, 1);
        let reopened = IndexStore::open(&mut root, timer, 1, deadline());
        if missing {
            assert!(reopened.is_err());
        } else {
            inspect(&reopened.unwrap(), StoreEpoch::from_bytes([11; 16]));
        }
    }
}

#[test]
fn failed_update_and_unclassified_commit_leave_no_reusable_owner() {
    for commit in [false, true] {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let timer = clock();
        let store = IndexStore::create(&mut root, EPOCH, timer.clone(), 1, deadline()).unwrap();
        seed(&store);
        let hit = Arc::new(AtomicU64::new(0));
        let seen = hit.clone();
        lock(&lock(&store.writer).unwrap().native.connection)
            .unwrap()
            .authorizer(Some(move |context: AuthContext<'_>| {
                let target = if commit {
                    matches!(context.action, AuthAction::Transaction { operation }
                        if !matches!(operation,
                            TransactionOperation::Begin | TransactionOperation::Rollback))
                } else {
                    matches!(
                        context.action,
                        AuthAction::Update {
                            table_name: "store",
                            column_name: "epoch"
                        }
                    )
                };
                if target {
                    seen.fetch_add(1, Ordering::Relaxed);
                    Authorization::Deny
                } else {
                    Authorization::Allow
                }
            }))
            .unwrap();
        let mut entropy = Entropy::fresh();
        let result = store.renew_epoch(&mut entropy, deadline()).map(|_| ());
        assert!(hit.load(Ordering::Relaxed) > 0);
        if commit {
            assert!(matches!(result, Err(CommitError::Indeterminate(_))));
        } else {
            assert!(matches!(result, Err(CommitError::Rejected(_))));
        }
        assert_eq!(entropy.calls, 1);
        let reopened = IndexStore::open(&mut root, timer, 1, deadline()).unwrap();
        inspect(&reopened, EPOCH);
    }
}

#[test]
fn late_successful_commit_returns_the_durable_new_epoch() {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let timer = clock();
    let store = IndexStore::create(&mut root, EPOCH, timer.clone(), 1, deadline()).unwrap();
    seed(&store);
    let observed = timer.clone();
    lock(&lock(&store.writer).unwrap().native.connection)
        .unwrap()
        .commit_hook(Some(move || {
            observed.0.store(100, Ordering::Relaxed);
            false
        }))
        .unwrap();
    let mut entropy = Entropy::fresh();
    let store = store.renew_epoch(&mut entropy, deadline()).unwrap();
    assert_eq!(store.epoch(), FRESH);
    assert_eq!(timer.0.load(Ordering::Relaxed), 100);
    assert_eq!(entropy.calls, 1);
    drop(store);
    timer.0.store(1, Ordering::Relaxed);
    let reopened = IndexStore::open(&mut root, timer, 1, deadline()).unwrap();
    inspect(&reopened, FRESH);
}

#[test]
fn native_scope_failure_at_epoch_update_refuses_the_transaction() {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let timer = clock();
    let store = IndexStore::create(&mut root, EPOCH, timer.clone(), 1, deadline()).unwrap();
    seed(&store);
    let hit = Arc::new(AtomicU64::new(0));
    {
        let writer = lock(&store.writer).unwrap();
        let budget = writer.native.budget.clone();
        let seen = hit.clone();
        lock(&writer.native.connection)
            .unwrap()
            .authorizer(Some(move |context: AuthContext<'_>| {
                if matches!(
                    context.action,
                    AuthAction::Update {
                        table_name: "store",
                        column_name: "epoch"
                    }
                ) {
                    seen.fetch_add(1, Ordering::Relaxed);
                    lock(&budget).unwrap().failure = Some(ports::Error::Deadline);
                }
                Authorization::Allow
            }))
            .unwrap();
    }
    let mut entropy = Entropy::fresh();
    assert_eq!(
        store.renew_epoch(&mut entropy, deadline()).map(|_| ()),
        Err(CommitError::Rejected(ports::Error::Deadline))
    );
    assert!(hit.load(Ordering::Relaxed) > 0);
    assert_eq!(entropy.calls, 1);
    let reopened = IndexStore::open(&mut root, timer, 1, deadline()).unwrap();
    inspect(&reopened, EPOCH);
}

#[test]
fn distinct_epoch_candidates_include_zero_without_extra_domain_rules() {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let timer = clock();
    let mut store = IndexStore::create(&mut root, EPOCH, timer.clone(), 1, deadline()).unwrap();
    seed(&store);
    let mut entropy = Entropy::fresh();
    for epoch in [StoreEpoch::from_bytes([0; 16]), FRESH] {
        entropy.bytes = *epoch.as_bytes();
        store = store.renew_epoch(&mut entropy, deadline()).unwrap();
        inspect(&store, epoch);
    }
    assert_eq!(entropy.calls, 2);
    drop(store);
    let reopened = IndexStore::open(&mut root, timer, 1, deadline()).unwrap();
    inspect(&reopened, FRESH);
}

const EPOCH_CRASH_CHILD_CASE: &str = "store_fs::index::epoch::tests::restore_epoch_crash_child";

struct ParkingEntropy<'a> {
    root: &'a std::path::Path,
    pause: bool,
    inner: Entropy,
}
impl ports::Entropy for ParkingEntropy<'_> {
    fn fill(&mut self, output: &mut [u8]) -> Result<(), ports::CryptoError> {
        ports::Entropy::fill(&mut self.inner, output)?;
        assert_eq!(self.inner.calls, 1);
        if self.pause {
            // The candidate exists only in caller scratch; BEGIN has not run.
            super::super::crash_tests::acknowledge_and_wait(self.root, "before-epoch-sql");
        }
        Ok(())
    }
}

#[test]
#[ignore = "child endpoint invoked only by the restored-epoch abrupt-death parent oracle"]
fn restore_epoch_crash_child() {
    use super::super::crash_tests::{acknowledge_and_wait, PHASE_ENV, ROOT_ENV};
    let root_path = std::env::var_os(ROOT_ENV).unwrap();
    let root_path = std::path::Path::new(&root_path);
    let phase = std::env::var(PHASE_ENV).unwrap();
    assert!(matches!(
        phase.as_str(),
        "before-epoch-sql" | "before-epoch-commit" | "after-epoch-return"
    ));
    crate::store_fs::with_probe_root_path(root_path, |root| {
        let store = IndexStore::open(root, clock(), 2, deadline()).unwrap();
        inspect(&store, EPOCH);
        if phase == "before-epoch-commit" {
            let pause_root = root_path.to_path_buf();
            lock(&lock(&store.writer).unwrap().native.connection)
                .unwrap()
                .commit_hook(Some(move || {
                    acknowledge_and_wait(&pause_root, "before-epoch-commit");
                    false
                }))
                .unwrap();
        }
        let mut entropy = ParkingEntropy {
            root: root_path,
            pause: phase == "before-epoch-sql",
            inner: Entropy::fresh(),
        };
        let store = store.renew_epoch(&mut entropy, deadline()).unwrap();
        assert_eq!(phase, "after-epoch-return");
        assert_eq!(entropy.inner.calls, 1);
        inspect(&store, FRESH);
        assert!(
            fs::metadata(root_path.join("metadata.sqlite3-wal"))
                .unwrap()
                .len()
                > 32
        );
        acknowledge_and_wait(root_path, "after-epoch-return");
    });
}

#[test]
fn restored_epoch_process_death_cuts_preserve_history_and_expected_identity() {
    for phase in [
        "before-epoch-sql",
        "before-epoch-commit",
        "after-epoch-return",
    ] {
        let source = Fixture::new();
        let destination = Fixture::new();
        {
            let mut source_root = source.locked();
            let mut destination_root = destination.locked();
            let store =
                IndexStore::create(&mut source_root, EPOCH, clock(), 2, deadline()).unwrap();
            seed(&store);
            inspect(&store, EPOCH);
            let receipt = store
                .backup(&mut destination_root, deadline(), &mut [0; 65536])
                .unwrap();
            assert_eq!(receipt.epoch, EPOCH);
        }
        super::super::crash_tests::kill_child_at(
            &destination.path,
            phase,
            EPOCH_CRASH_CHILD_CASE,
            None,
        );
        let mut source_root = source.locked();
        let original = IndexStore::open(&mut source_root, clock(), 2, deadline()).unwrap();
        original.validate_integrity(deadline()).unwrap();
        inspect(&original, EPOCH);
        let mut destination_root = destination.locked();
        let restored = IndexStore::open(&mut destination_root, clock(), 2, deadline()).unwrap();
        restored.validate_integrity(deadline()).unwrap();
        let expected = if phase == "after-epoch-return" {
            FRESH
        } else {
            EPOCH
        };
        inspect(&restored, expected);
        let identity = restored.view(ACCOUNT, deadline()).unwrap().identity();
        let old = DataState {
            account: ACCOUNT,
            epoch: EPOCH,
            kind: DataType::Mailbox,
            sequence: Sequence::from_u64(2),
        };
        let current = DataState {
            account: identity.account,
            epoch: identity.epoch,
            kind: DataType::Mailbox,
            sequence: identity.committed_sequence,
        };
        assert_eq!(
            old.is_retained_for(current, identity.history_floor),
            phase != "after-epoch-return"
        );
        let value = encode(Row::Blob(blob()));
        assert_eq!(
            restored.commit(
                &td_crypto::Provider,
                CommitRequest {
                    epoch: restored.epoch(),
                    ..request(2)
                },
                &[Operation::put(Table::Blobs, DELETED.as_bytes(), &value).unwrap()],
                &mut [BlobSource {
                    id: DELETED,
                    source: &mut std::io::Cursor::new(RAW)
                }]
            ),
            Err(CommitError::Rejected(ports::Error::Conflict))
        );
        inspect(&restored, expected);
    }
}

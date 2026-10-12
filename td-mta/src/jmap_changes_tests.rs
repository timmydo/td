#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{
    format::{
        operation::Operation,
        row::{MailboxRow, Row},
        ObjectType, Table,
    },
    ids::{AccountId, MailboxId, StoreEpoch},
    ports::{Clock, Deadline, Tick, Time},
    store_fs::{tests::Fixture, CommitRequest, IndexStore},
    sync::DataType,
};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
const ACCOUNT: AccountId = AccountId::from_bytes([1; 16]);
struct Timer(AtomicU64);
impl Clock for Timer {
    fn sample(&self) -> Result<Time, ports::Error> {
        Ok(Time {
            monotonic: Tick(self.0.load(Ordering::Relaxed)),
            utc_ms: 0,
        })
    }
}
fn until() -> Deadline {
    Deadline::after(Tick(0), 100).unwrap()
}
fn state(sequence: u64) -> DataState {
    DataState {
        account: ACCOUNT,
        epoch: StoreEpoch::from_bytes([7; 16]),
        kind: DataType::Mailbox,
        sequence: Sequence::from_u64(sequence),
    }
}
fn run(test: impl FnOnce(&IndexStore<'_>, &Timer)) {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let clock = Arc::new(Timer(AtomicU64::new(1)));
    let store = IndexStore::create(
        &mut root,
        StoreEpoch::from_bytes([7; 16]),
        clock.clone(),
        3,
        until(),
    )
    .unwrap();
    store.create_account(ACCOUNT, until()).unwrap();
    test(&store, &clock);
}
fn commit(store: &IndexStore<'_>, expected: u64, changes: &[(u8, ChangeAction)]) {
    let values: Vec<_> = changes
        .iter()
        .map(|(n, action)| {
            let id = MailboxId::from_bytes([*n; 16]);
            let mut value = vec![0; 128];
            let length = Row::Mailbox(MailboxRow {
                name: if *action == ChangeAction::Created {
                    "First"
                } else {
                    "Changed"
                },
                parent: None,
                role: None,
                sort_order: 0,
                subscribed: true,
            })
            .encode(&mut value)
            .unwrap();
            value.truncate(length);
            (id, value, *action)
        })
        .collect();
    let mut ops = Vec::new();
    for (id, value, action) in &values {
        ops.push(if *action == ChangeAction::Destroyed {
            Operation::delete(Table::Mailboxes, id.as_bytes()).unwrap()
        } else {
            Operation::put(Table::Mailboxes, id.as_bytes(), value).unwrap()
        });
        ops.push(Operation::change(
            ObjectType::Mailbox,
            *action,
            id.as_bytes(),
        ));
    }
    store
        .commit(
            &td_crypto::Provider,
            CommitRequest {
                account: ACCOUNT,
                epoch: store.epoch(),
                expected: Sequence::from_u64(expected),
                utc_ms: 0,
                deadline: until(),
            },
            &ops,
            &mut [],
        )
        .unwrap();
}
fn collect(
    view: &mut IndexReadView<'_, '_>,
    since: DataState,
    maximum: usize,
) -> Result<Page, Error> {
    let mut cursor = Cursor::new(view, DataType::Mailbox, since, maximum)?;
    for _ in 0..100 {
        if cursor.poll()? {
            return cursor.finish();
        }
    }
    panic!("unbounded fixture page")
}
#[test]
fn changes_coalesce_and_pages_end_at_transaction_boundaries() {
    use ChangeAction::*;
    run(|store, _| {
        commit(store, 0, &[(2, Created), (3, Created)]);
        commit(store, 1, &[(2, Updated)]);
        commit(store, 2, &[(4, Created)]);
        commit(store, 3, &[(4, Destroyed)]);
        commit(store, 4, &[(3, Destroyed)]);
        let mut view = store.view(ACCOUNT, until()).unwrap();
        let full = collect(&mut view, state(0), 1024).unwrap();
        assert_eq!(full.new_state, state(5));
        assert!(!full.has_more);
        assert_eq!(
            full.changes,
            vec![Change {
                kind: ObjectType::Mailbox,
                id: [2; 16],
                action: Created
            }]
        );
        let later = collect(&mut view, state(1), 1024).unwrap();
        assert_eq!(
            later.changes,
            vec![
                Change {
                    kind: ObjectType::Mailbox,
                    id: [2; 16],
                    action: Updated
                },
                Change {
                    kind: ObjectType::Mailbox,
                    id: [3; 16],
                    action: Destroyed
                }
            ]
        );
        let first = collect(&mut view, state(0), 2).unwrap();
        assert_eq!(first.new_state, state(2));
        assert!(first.has_more);
        assert_eq!(first.changes.len(), 2);
        let last = collect(&mut view, first.new_state, 2).unwrap();
        assert_eq!(last.new_state, state(5));
        assert!(!last.has_more);
        assert_eq!(
            last.changes,
            vec![Change {
                kind: ObjectType::Mailbox,
                id: [3; 16],
                action: Destroyed
            }]
        );
        assert!(matches!(
            collect(&mut view, state(0), 1),
            Err(Error::CannotCalculateChanges)
        ));
        let empty = collect(&mut view, state(5), 1).unwrap();
        assert!(empty.changes.is_empty());
        assert_eq!(empty.new_state, state(5));
        assert!(!empty.has_more);
    });
}
#[test]
fn scope_limits_and_original_snapshot_are_preserved() {
    use ChangeAction::*;
    run(|store, _| {
        commit(store, 0, &[(2, Created)]);
        let mut old = store.view(ACCOUNT, until()).unwrap();
        let mut cursor = Cursor::new(&mut old, DataType::Mailbox, state(0), 1).unwrap();
        assert!(!cursor.poll().unwrap());
        commit(store, 1, &[(3, Created)]);
        while !cursor.poll().unwrap() {}
        let page = cursor.finish().unwrap();
        assert_eq!(page.new_state, state(1));
        assert!(!page.has_more);
        assert_eq!(page.identity.committed_sequence, Sequence::from_u64(1));
        for maximum in [0, 1025] {
            assert!(matches!(
                Cursor::new(&mut old, DataType::Mailbox, state(0), maximum),
                Err(Error::Invalid)
            ));
        }
        for wrong in [
            DataState {
                account: AccountId::from_bytes([9; 16]),
                ..state(0)
            },
            DataState {
                epoch: StoreEpoch::from_bytes([9; 16]),
                ..state(0)
            },
            state(2),
            DataState {
                kind: DataType::Email,
                ..state(0)
            },
        ] {
            assert!(matches!(
                Cursor::new(&mut old, DataType::Mailbox, wrong, 1),
                Err(Error::CannotCalculateChanges)
            ));
        }
        assert!(matches!(
            Cursor::new(&mut old, DataType::Mailbox, state(0), 1)
                .unwrap()
                .finish(),
            Err(Error::Incomplete)
        ));
    });
}
#[test]
fn expiry_after_a_complete_page_withholds_finish_and_sticks_in_poll() {
    run(|store, clock| {
        commit(store, 0, &[(2, ChangeAction::Created)]);
        let mut view = store.view(ACCOUNT, until()).unwrap();
        let mut cursor = Cursor::new(&mut view, DataType::Mailbox, state(0), 1).unwrap();
        while !cursor.poll().unwrap() {}
        clock.0.store(101, Ordering::Relaxed);
        assert_eq!(cursor.poll(), Err(Error::Store(ports::Error::Deadline)));
        clock.0.store(1, Ordering::Relaxed);
        assert_eq!(cursor.poll(), Err(Error::Store(ports::Error::Deadline)));
        assert!(matches!(
            cursor.finish(),
            Err(Error::Store(ports::Error::Deadline))
        ));
    });
}

#[test]
fn history_retirement_preserves_an_existing_cursor_but_rejects_old_tokens() {
    use ChangeAction::*;
    run(|store, _| {
        commit(store, 0, &[(2, Created), (3, Created)]);
        commit(store, 1, &[(2, Updated)]);
        let mut old = store.view(ACCOUNT, until()).unwrap();
        let mut cursor = Cursor::new(&mut old, DataType::Mailbox, state(0), 2).unwrap();
        assert!(!cursor.poll().unwrap());
        let pruned = store
            .prune_history(crate::store_fs::HistoryPruneRequest {
                account: ACCOUNT,
                epoch: store.epoch(),
                expected: Sequence::from_u64(2),
                through: Sequence::from_u64(1),
                max_rows: 1,
                deadline: until(),
            })
            .unwrap();
        assert!(pruned.more);
        while !cursor.poll().unwrap() {}
        let page = cursor.finish().unwrap();
        assert_eq!(page.changes.len(), 2);
        assert_eq!(page.new_state, state(2));
        let mut fresh = store.view(ACCOUNT, until()).unwrap();
        assert!(matches!(
            Cursor::new(&mut fresh, DataType::Mailbox, state(0), 2),
            Err(Error::CannotCalculateChanges)
        ));
        let page = collect(&mut fresh, state(1), 2).unwrap();
        assert_eq!(
            page.changes,
            vec![Change {
                kind: ObjectType::Mailbox,
                id: [2; 16],
                action: Updated
            }]
        );
    });
}

#[test]
fn finish_observes_expiry_without_an_intervening_poll() {
    run(|store, clock| {
        let mut view = store.view(ACCOUNT, until()).unwrap();
        let mut cursor = Cursor::new(&mut view, DataType::Mailbox, state(0), 1).unwrap();
        while !cursor.poll().unwrap() {}
        clock.0.store(101, Ordering::Relaxed);
        assert!(matches!(
            cursor.finish(),
            Err(Error::Store(ports::Error::Deadline))
        ));
    });
}

#[test]
fn canceled_ids_do_not_leave_an_unbounded_nonreuse_audit() {
    use ChangeAction::*;
    run(|store, _| {
        commit(store, 0, &[(2, Created)]);
        commit(store, 1, &[(2, Destroyed)]);
        commit(store, 2, &[(2, Created)]);
        let mut view = store.view(ACCOUNT, until()).unwrap();
        let page = collect(&mut view, state(0), 1).unwrap();
        assert_eq!(page.new_state, state(3));
        assert!(!page.has_more);
        assert_eq!(
            page.changes,
            vec![Change {
                kind: ObjectType::Mailbox,
                id: [2; 16],
                action: Created
            }]
        );
    });
}

#[test]
fn full_retirement_before_first_poll_preserves_exact_original_history() {
    use ChangeAction::*;
    run(|store, _| {
        commit(store, 0, &[(2, Created), (3, Created)]);
        commit(store, 1, &[(2, Updated)]);
        let mut old = store.view(ACCOUNT, until()).unwrap();
        let mut cursor = Cursor::new(&mut old, DataType::Mailbox, state(0), 2).unwrap();
        let result = store
            .prune_history(crate::store_fs::HistoryPruneRequest {
                account: ACCOUNT,
                epoch: store.epoch(),
                expected: Sequence::from_u64(2),
                through: Sequence::from_u64(1),
                max_rows: 100,
                deadline: until(),
            })
            .unwrap();
        assert!(!result.more);
        while !cursor.poll().unwrap() {}
        let page = cursor.finish().unwrap();
        assert_eq!(page.new_state, state(2));
        assert_eq!(
            page.changes,
            vec![
                Change {
                    kind: ObjectType::Mailbox,
                    id: [2; 16],
                    action: Created
                },
                Change {
                    kind: ObjectType::Mailbox,
                    id: [3; 16],
                    action: Created
                }
            ]
        );
    });
}

#[test]
fn native_commits_reject_duplicate_changes_and_cursor_coalesces_later_updates() {
    use ChangeAction::*;
    run(|store, _| {
        commit(store, 0, &[(2, Created), (3, Created)]);
        let id = MailboxId::from_bytes([2; 16]);
        let updated = Operation::change(ObjectType::Mailbox, Updated, id.as_bytes());
        assert_eq!(
            store.commit(
                &td_crypto::Provider,
                CommitRequest {
                    account: ACCOUNT,
                    epoch: store.epoch(),
                    expected: Sequence::from_u64(1),
                    utc_ms: 0,
                    deadline: until(),
                },
                &[updated, updated],
                &mut []
            ),
            Err(ports::CommitFailure::Rejected(ports::Error::Conflict))
        );
        commit(store, 1, &[(2, Updated), (3, Updated)]);
        commit(store, 2, &[(2, Updated), (3, Destroyed)]);
        let mut view = store.view(ACCOUNT, until()).unwrap();
        let page = collect(&mut view, state(1), 1024).unwrap();
        assert_eq!(
            page.changes,
            vec![
                Change {
                    kind: ObjectType::Mailbox,
                    id: [2; 16],
                    action: Updated
                },
                Change {
                    kind: ObjectType::Mailbox,
                    id: [3; 16],
                    action: Destroyed
                }
            ]
        );
        let page = collect(&mut view, state(0), 1024).unwrap();
        assert_eq!(
            page.changes,
            vec![Change {
                kind: ObjectType::Mailbox,
                id: [2; 16],
                action: Created
            }]
        );
    });
}

#[test]
fn destroyed_then_updated_is_rejected_by_the_writer_and_recreated_retained_id_is_corrupt() {
    use ChangeAction::*;
    run(|store, _| {
        commit(store, 0, &[(2, Created)]);
        let id = MailboxId::from_bytes([2; 16]);
        let operations = [
            Operation::delete(Table::Mailboxes, id.as_bytes()).unwrap(),
            Operation::change(ObjectType::Mailbox, Destroyed, id.as_bytes()),
            Operation::change(ObjectType::Mailbox, Updated, id.as_bytes()),
        ];
        assert_eq!(
            store.commit(
                &td_crypto::Provider,
                CommitRequest {
                    account: ACCOUNT,
                    epoch: store.epoch(),
                    expected: Sequence::from_u64(1),
                    utc_ms: 0,
                    deadline: until()
                },
                &operations,
                &mut []
            ),
            Err(ports::CommitFailure::Rejected(ports::Error::Conflict))
        );
        commit(store, 1, &[(2, Destroyed)]);
        commit(store, 2, &[(2, Created)]);
        let mut view = store.view(ACCOUNT, until()).unwrap();
        let mut cursor = Cursor::new(&mut view, DataType::Mailbox, state(1), 1).unwrap();
        assert!(!cursor.poll().unwrap());
        assert_eq!(cursor.poll(), Err(Error::Store(ports::Error::Corrupt)));
        assert_eq!(cursor.poll(), Err(Error::Store(ports::Error::Corrupt)));
        assert!(matches!(
            cursor.finish(),
            Err(Error::Store(ports::Error::Corrupt))
        ));
    });
}

#[test]
fn repeated_cursors_consume_one_original_view_work_budget() {
    run(|store, _| {
        let entries: Vec<_> = (1..=128).map(|n| (n, ChangeAction::Created)).collect();
        commit(store, 0, &entries);
        let mut view = store.view(ACCOUNT, until()).unwrap();
        let mut scratch = [0; 128];
        for _ in 0..256 {
            view.get(
                crate::format::key::Key::Mailbox(MailboxId::from_bytes([1; 16])),
                &mut scratch,
            )
            .unwrap();
        }
        let mut pages = 0;
        for _ in 0..10000 {
            let mut cursor = Cursor::new(&mut view, DataType::Mailbox, state(0), 128).unwrap();
            loop {
                match cursor.poll() {
                    Ok(false) => (),
                    Ok(true) => {
                        cursor.finish().unwrap();
                        pages += 1;
                        break;
                    }
                    Err(error) => {
                        assert_eq!(error, Error::Store(ports::Error::Capacity));
                        assert!(pages > 0);
                        assert_eq!(cursor.poll(), Err(error));
                        assert!(matches!(
                            cursor.finish(),
                            Err(Error::Store(ports::Error::Capacity))
                        ));
                        assert!(matches!(
                            Cursor::new(&mut view, DataType::Mailbox, state(0), 128),
                            Err(Error::Store(ports::Error::Capacity))
                        ));
                        return;
                    }
                }
            }
        }
        panic!("cursor renewed original storage work");
    });
}

#[test]
fn maximum_page_refuses_a_larger_atomic_transaction() {
    run(|store, _| {
        let mut value = [0; 128];
        let length = Row::Mailbox(MailboxRow {
            name: "Folder",
            parent: None,
            role: None,
            sort_order: 0,
            subscribed: true,
        })
        .encode(&mut value)
        .unwrap();
        let ids: Vec<_> = (0u16..1025)
            .map(|n| {
                let mut id = [0; 16];
                id[..2].copy_from_slice(&n.to_be_bytes());
                id
            })
            .collect();
        let mut ops = Vec::new();
        for id in &ids {
            ops.push(Operation::put(Table::Mailboxes, id, &value[..length]).unwrap());
            ops.push(Operation::change(
                ObjectType::Mailbox,
                ChangeAction::Created,
                id,
            ));
        }
        store
            .commit(
                &td_crypto::Provider,
                CommitRequest {
                    account: ACCOUNT,
                    epoch: store.epoch(),
                    expected: Sequence::default(),
                    utc_ms: 0,
                    deadline: until(),
                },
                &ops,
                &mut [],
            )
            .unwrap();
        let mut view = store.view(ACCOUNT, until()).unwrap();
        let mut cursor = Cursor::new(&mut view, DataType::Mailbox, state(0), 1024).unwrap();
        for _ in 0..1025 {
            assert!(!cursor.poll().unwrap());
        }
        assert_eq!(cursor.poll(), Err(Error::CannotCalculateChanges));
        assert_eq!(cursor.poll(), Err(Error::CannotCalculateChanges));
    });
}

#[test]
fn native_type_filter_skips_other_history_and_keeps_transaction_boundaries() {
    run(|store, _| {
        let mut thread = [0; 32];
        let thread_length = Row::Thread.encode(&mut thread).unwrap();
        let mut mailbox = [0; 128];
        let mailbox_length = Row::Mailbox(MailboxRow {
            name: "Folder",
            parent: None,
            role: None,
            sort_order: 0,
            subscribed: true,
        })
        .encode(&mut mailbox)
        .unwrap();
        let first = [2; 16];
        let second = [3; 16];
        for (sequence, operations) in [
            vec![
                Operation::put(Table::Threads, &first, &thread[..thread_length]).unwrap(),
                Operation::change(ObjectType::Thread, ChangeAction::Created, &first),
            ],
            vec![
                Operation::put(Table::Threads, &second, &thread[..thread_length]).unwrap(),
                Operation::change(ObjectType::Thread, ChangeAction::Created, &second),
                Operation::put(Table::Mailboxes, &first, &mailbox[..mailbox_length]).unwrap(),
                Operation::change(ObjectType::Mailbox, ChangeAction::Created, &first),
            ],
            vec![
                Operation::put(Table::Threads, &first, &thread[..thread_length]).unwrap(),
                Operation::change(ObjectType::Thread, ChangeAction::Updated, &first),
            ],
            vec![
                Operation::put(Table::Mailboxes, &second, &mailbox[..mailbox_length]).unwrap(),
                Operation::change(ObjectType::Mailbox, ChangeAction::Created, &second),
            ],
        ]
        .into_iter()
        .enumerate()
        {
            store
                .commit(
                    &td_crypto::Provider,
                    CommitRequest {
                        account: ACCOUNT,
                        epoch: store.epoch(),
                        expected: Sequence::from_u64(sequence as u64),
                        utc_ms: 0,
                        deadline: until(),
                    },
                    &operations,
                    &mut [],
                )
                .unwrap();
        }
        let mut view = store.view(ACCOUNT, until()).unwrap();
        let first = collect(&mut view, state(0), 1).unwrap();
        assert_eq!(first.new_state, state(2));
        assert!(first.has_more);
        assert_eq!(
            first.changes,
            vec![Change {
                kind: ObjectType::Mailbox,
                id: [2; 16],
                action: ChangeAction::Created
            }]
        );
        let last = collect(&mut view, first.new_state, 1).unwrap();
        assert_eq!(last.new_state, state(4));
        assert!(!last.has_more);
        assert_eq!(
            last.changes,
            vec![Change {
                kind: ObjectType::Mailbox,
                id: [3; 16],
                action: ChangeAction::Created
            }]
        );
        let mut cursor = Cursor::new(
            &mut view,
            DataType::Thread,
            DataState {
                kind: DataType::Thread,
                ..state(0)
            },
            2,
        )
        .unwrap();
        while !cursor.poll().unwrap() {}
        let page = cursor.finish().unwrap();
        assert_eq!(page.changes.len(), 2);
        assert!(page
            .changes
            .iter()
            .all(|change| change.kind == ObjectType::Thread
                && change.action == ChangeAction::Created));
        assert_eq!(page.new_state.sequence, Sequence::from_u64(4));
    });
}

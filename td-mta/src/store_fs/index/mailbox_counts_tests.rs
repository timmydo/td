#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{
    format::row::{BlobRow, EmailOrigin, EmailRow, MailboxRow},
    ids::EmailId,
    ports::{Crypto, Digest, Tick, Time},
    store_fs::tests::Fixture,
};
use std::sync::atomic::{AtomicU64, Ordering};
const ACCOUNT: AccountId = AccountId::from_bytes([1; 16]);
const OTHER: AccountId = AccountId::from_bytes([2; 16]);
const BODY: BlobId = BlobId::from_bytes([3; 16]);
struct Timer(AtomicU64, AtomicU64);
impl Clock for Timer {
    fn sample(&self) -> Result<Time, ports::Error> {
        Ok(Time {
            utc_ms: 0,
            monotonic: Tick(
                self.0
                    .fetch_add(self.1.load(Ordering::Relaxed), Ordering::Relaxed),
            ),
        })
    }
}
fn deadline() -> Deadline {
    Deadline::after(Tick(0), 100).unwrap()
}
fn open<'r>(root: &'r mut LockedRoot, clock: Arc<Timer>) -> IndexStore<'r> {
    IndexStore::create(root, StoreEpoch::from_bytes([9; 16]), clock, 3, deadline()).unwrap()
}
fn apply(
    store: &IndexStore<'_>,
    account: AccountId,
    expected: u64,
    records: &[(Key<'_>, Row<'_>)],
    deletes: &[Key<'_>],
    sources: &mut [BlobSource<'_>],
) {
    let rows: Vec<_> = records
        .iter()
        .map(|(key, row)| {
            let mut key_bytes = vec![0; 1024];
            let length = key.encode(&mut key_bytes).unwrap();
            key_bytes.truncate(length);
            let mut value = vec![0; format::MAX_VALUE_BYTES];
            let length = row.encode(&mut value).unwrap();
            value.truncate(length);
            (key.table(), key_bytes, value)
        })
        .collect();
    let removed: Vec<_> = deletes
        .iter()
        .map(|key| {
            let mut bytes = vec![0; 1024];
            let length = key.encode(&mut bytes).unwrap();
            bytes.truncate(length);
            (key.table(), bytes)
        })
        .collect();
    let mut operations: Vec<_> = rows
        .iter()
        .map(|(table, key, value)| Operation::put(*table, key, value).unwrap())
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
const BOX: MailboxId = MailboxId::from_bytes([4; 16]);
const EMPTY: MailboxId = MailboxId::from_bytes([5; 16]);
fn email(n: u8) -> EmailId {
    EmailId::from_bytes([n; 16])
}
fn thread(n: u8) -> ThreadId {
    ThreadId::from_bytes([n; 16])
}
fn seed(store: &IndexStore<'_>, account: AccountId) {
    store.create_account(account, deadline()).unwrap();
    let mailbox = Row::Mailbox(MailboxRow {
        name: "Inbox",
        parent: None,
        role: None,
        sort_order: 0,
        subscribed: true,
    });
    let mut rows = vec![
        (Key::Mailbox(BOX), mailbox),
        (
            Key::Mailbox(EMPTY),
            Row::Mailbox(MailboxRow {
                name: "Empty",
                parent: None,
                role: None,
                sort_order: 0,
                subscribed: true,
            }),
        ),
        (
            Key::Blob(BODY),
            Row::Blob(BlobRow {
                length: 0,
                digest: td_crypto::Provider.sha256().unwrap().finish().unwrap(),
                created_at: 0,
            }),
        ),
    ];
    for tid in 10..=13 {
        rows.push((Key::Thread(thread(tid)), Row::Thread));
    }
    // Thread order differs from ID order; two unread members count once.
    for (id, tid, member, keyword) in [
        (1, 12, true, None),
        (2, 10, true, Some("$seen")),
        (3, 12, true, None),
        (4, 11, true, Some("$draft")),
        (5, 10, false, None),
        (6, 13, true, Some("$draft")),
        (7, 10, true, None),
        (8, 12, true, Some("$seen")),
    ] {
        rows.extend([(
            Key::Email(email(id)),
            Row::Email(EmailRow {
                blob: BODY,
                thread: thread(tid),
                received_at: 0,
                origin: EmailOrigin::Jmap,
            }),
        )]);
        if member {
            rows.push((Key::Membership(email(id), BOX), Row::Membership));
        }
        if let Some(keyword) = keyword {
            rows.push((Key::Keyword(email(id), keyword), Row::Keyword));
        }
    }
    rows.push((Key::Keyword(email(6), "$seen"), Row::Keyword));
    apply(
        store,
        account,
        0,
        &rows,
        &[],
        &mut [BlobSource {
            id: BODY,
            source: &mut b"".as_slice(),
        }],
    );
}
fn totals(view: &mut IndexReadView<'_, '_>, mailbox: MailboxId) -> (u64, u64, u64, u64) {
    let counts = view
        .mailbox_counts(mailbox, &mut [0; format::MAX_VALUE_BYTES])
        .unwrap()
        .unwrap();
    assert_eq!(counts.identity, view.identity());
    assert_eq!(counts.mailbox, mailbox);
    (
        counts.total_emails,
        counts.unread_emails,
        counts.total_threads,
        counts.unread_threads,
    )
}

#[test]
fn exact_totals_are_account_scoped_and_snapshot_consistent() {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let store = open(
        &mut root,
        Arc::new(Timer(AtomicU64::new(1), AtomicU64::new(0))),
    );
    seed(&store, ACCOUNT);
    seed(&store, OTHER);
    let mut old = store.view(ACCOUNT, deadline()).unwrap();
    assert_eq!(totals(&mut old, BOX), (7, 3, 4, 2));
    assert_eq!(totals(&mut old, EMPTY), (0, 0, 0, 0));
    assert_eq!(
        old.mailbox_counts(MailboxId::from_bytes([99; 16]), &mut []),
        Ok(None)
    );
    assert_eq!(
        old.mailbox_counts(BOX, &mut []),
        Err(ports::Error::Capacity)
    );
    apply(
        &store,
        ACCOUNT,
        1,
        &[(Key::Keyword(email(2), "$draft"), Row::Keyword)],
        &[
            Key::Keyword(email(4), "$draft"),
            Key::Membership(email(1), BOX),
        ],
        &mut [],
    );
    let mut current = store.view(ACCOUNT, deadline()).unwrap();
    assert_eq!(totals(&mut current, BOX), (6, 3, 4, 3));
    assert_eq!(totals(&mut old, BOX), (7, 3, 4, 2));
    drop(current);
    let mut other = store.view(OTHER, deadline()).unwrap();
    assert_eq!(totals(&mut other, BOX), (7, 3, 4, 2));
}

#[test]
fn count_queries_do_not_sort_or_materialize_distinct_sets() {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let store = open(
        &mut root,
        Arc::new(Timer(AtomicU64::new(1), AtomicU64::new(0))),
    );
    seed(&store, ACCOUNT);
    let mut view = store.view(ACCOUNT, deadline()).unwrap();
    view.read_snapshot(|native| {
        native.run(|db| {
            for query in [MEMBERS, MEMBERSHIPS] {
                let mut statement = db
                    .prepare(&format!("EXPLAIN QUERY PLAN {query}"))
                    .map_err(sql)?;
                let details = if query == MEMBERS {
                    statement
                        .query_map(params![ACCOUNT.as_bytes().as_slice()], |row| {
                            row.get::<_, String>(3)
                        })
                        .map_err(sql)?
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(sql)?
                } else {
                    statement
                        .query_map(
                            params![ACCOUNT.as_bytes().as_slice(), BOX.as_bytes().as_slice()],
                            |row| row.get::<_, String>(3),
                        )
                        .map_err(sql)?
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(sql)?
                };
                assert!(
                    !details.iter().any(|s| [
                        "TEMP B-TREE",
                        "MATERIALIZE",
                        "AUTOMATIC",
                        "BLOOM",
                        "SCAN"
                    ]
                    .iter()
                    .any(|bad| s.contains(bad))),
                    "{details:?}"
                );
                if query == MEMBERS {
                    for required in [
                        "emails_thread (account=?)",
                        "SEARCH m USING PRIMARY KEY (account=? AND email_id=?)",
                        "SEARCH t USING PRIMARY KEY (account=? AND id=?)",
                        "SEARCH s USING PRIMARY KEY (account=? AND email_id=? AND keyword=?)",
                        "SEARCH d USING PRIMARY KEY (account=? AND email_id=? AND keyword=?)",
                    ] {
                        assert!(
                            details.iter().any(|s| s.contains(required)),
                            "missing {required}: {details:?}"
                        );
                    }
                } else {
                    assert!(
                        details
                            .iter()
                            .any(|s| s.contains("account=? AND mailbox_id=?")),
                        "{details:?}"
                    );
                }
            }
            Ok(())
        })
    })
    .unwrap();
}

#[test]
fn deadline_expiry_cannot_become_empty_counts() {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let clock = Arc::new(Timer(AtomicU64::new(1), AtomicU64::new(0)));
    let store = open(&mut root, clock.clone());
    seed(&store, ACCOUNT);
    let mut view = store.view(ACCOUNT, deadline()).unwrap();
    clock.0.store(101, Ordering::Relaxed);
    assert_eq!(
        view.mailbox_counts(BOX, &mut [0; format::MAX_VALUE_BYTES]),
        Err(ports::Error::Deadline)
    );
    clock.0.store(1, Ordering::Relaxed);
    assert_eq!(
        view.mailbox_counts(EMPTY, &mut [0; format::MAX_VALUE_BYTES]),
        Err(ports::Error::Deadline)
    );
}

#[test]
fn corrupt_references_and_future_sequences_refuse_all_totals() {
    for statement in [
        "DELETE FROM threads WHERE account=?1 AND id=x'0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c'",
        "DELETE FROM emails WHERE account=?1 AND id=x'01010101010101010101010101010101'",
        "UPDATE emails SET changed=x'0000000000000063' WHERE account=?1",
        "UPDATE threads SET changed=x'0000000000000063' WHERE account=?1",
        "UPDATE memberships SET changed=x'0000000000000063' WHERE account=?1",
        "UPDATE keywords SET changed=x'0000000000000063' WHERE account=?1 AND keyword='$seen'",
        "UPDATE keywords SET changed=x'0000000000000063' WHERE account=?1 AND keyword='$draft'",
        "UPDATE memberships SET changed=x'00' WHERE account=?1",
    ] {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let store = open(
            &mut root,
            Arc::new(Timer(AtomicU64::new(1), AtomicU64::new(0))),
        );
        seed(&store, ACCOUNT);
        lock(&store.writer)
            .unwrap()
            .native
            .run(|db| {
                db.execute_batch("PRAGMA foreign_keys=OFF; PRAGMA ignore_check_constraints=ON")
                    .map_err(sql)?;
                db.execute(statement, params![ACCOUNT.as_bytes().as_slice()])
                    .map_err(sql)?;
                db.execute_batch("PRAGMA foreign_keys=ON; PRAGMA ignore_check_constraints=OFF")
                    .map_err(sql)?;
                Ok(())
            })
            .unwrap();
        let mut view = store.view(ACCOUNT, deadline()).unwrap();
        assert_eq!(
            view.mailbox_counts(BOX, &mut [0; format::MAX_VALUE_BYTES]),
            Err(ports::Error::Corrupt),
            "{statement}"
        );
    }
}

#[test]
fn exhausted_work_and_lost_snapshot_never_report_empty_success() {
    for fuel in [true, false] {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let store = open(
            &mut root,
            Arc::new(Timer(AtomicU64::new(1), AtomicU64::new(0))),
        );
        seed(&store, ACCOUNT);
        let mut view = store.view(ACCOUNT, deadline()).unwrap();
        let expected = if fuel {
            lock(&view.native().unwrap().scope.budget)
                .unwrap()
                .remaining = 0;
            ports::Error::Capacity
        } else {
            view.native()
                .unwrap()
                .run(|db| db.execute_batch("ROLLBACK").map_err(sql))
                .unwrap();
            ports::Error::Corrupt
        };
        assert_eq!(
            view.mailbox_counts(BOX, &mut [0; format::MAX_VALUE_BYTES]),
            Err(expected)
        );
        assert_eq!(
            view.mailbox_counts(EMPTY, &mut [0; format::MAX_VALUE_BYTES]),
            Err(expected)
        );
    }
}

#[test]
fn batch_preserves_input_order_bounds_and_overlapping_memberships() {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let store = open(
        &mut root,
        Arc::new(Timer(AtomicU64::new(1), AtomicU64::new(0))),
    );
    seed(&store, ACCOUNT);
    apply(
        &store,
        ACCOUNT,
        1,
        &[
            (Key::Membership(email(1), EMPTY), Row::Membership),
            (Key::Membership(email(2), EMPTY), Row::Membership),
            (Key::Membership(email(7), EMPTY), Row::Membership),
        ],
        &[],
        &mut [],
    );
    let mut view = store.view(ACCOUNT, deadline()).unwrap();
    let mut scratch = vec![0; format::MAX_VALUE_BYTES];
    let missing = MailboxId::from_bytes([99; 16]);
    let counts = view
        .mailbox_counts_many(&[EMPTY, missing, BOX], &mut scratch)
        .unwrap();
    assert_eq!(counts.len(), 3);
    assert_eq!(counts[0].unwrap().mailbox, EMPTY);
    assert_eq!(
        (
            counts[0].unwrap().total_emails,
            counts[0].unwrap().unread_emails,
            counts[0].unwrap().total_threads,
            counts[0].unwrap().unread_threads
        ),
        (3, 2, 2, 2)
    );
    assert_eq!(counts[1], None);
    assert_eq!(counts[2].unwrap().mailbox, BOX);
    assert_eq!(
        (
            counts[2].unwrap().total_emails,
            counts[2].unwrap().unread_emails,
            counts[2].unwrap().total_threads,
            counts[2].unwrap().unread_threads
        ),
        (7, 3, 4, 2)
    );
    assert!(view.mailbox_counts_many(&[], &mut []).unwrap().is_empty());
    assert_eq!(
        view.mailbox_counts_many(&[BOX, BOX], &mut scratch),
        Err(ports::Error::Invalid)
    );
    assert_eq!(
        view.mailbox_counts_many(&vec![BOX; MAX_MAILBOX_COUNTS + 1], &mut scratch),
        Err(ports::Error::Capacity)
    );
    let ids: Vec<_> = (0..MAX_MAILBOX_COUNTS)
        .map(|i| MailboxId::from_bytes([i as u8; 16]))
        .collect();
    assert_eq!(
        view.mailbox_counts_many(&ids, &mut scratch)
            .unwrap()
            .iter()
            .flatten()
            .count(),
        2
    );
}

#[test]
fn every_work_and_clock_cutoff_before_complete_counts_refuses_partial_success() {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let clock = Arc::new(Timer(AtomicU64::new(1), AtomicU64::new(0)));
    let store = open(&mut root, clock.clone());
    seed(&store, ACCOUNT);
    let mut scratch = vec![0; format::MAX_VALUE_BYTES];
    for advancing_clock in [false, true] {
        let mut complete = false;
        let mut refused = 0;
        for cutoff in 1..5000 {
            let now = Tick(clock.0.load(Ordering::Relaxed));
            let until = Deadline::after(now, if advancing_clock { cutoff } else { 100 }).unwrap();
            let mut view = store.view(ACCOUNT, until).unwrap();
            if advancing_clock {
                clock.1.store(1, Ordering::Relaxed);
            } else {
                lock(&view.native().unwrap().scope.budget)
                    .unwrap()
                    .remaining = cutoff;
            }
            let result = view.mailbox_counts_many(&[BOX, EMPTY], &mut scratch);
            clock.1.store(0, Ordering::Relaxed);
            match result {
                Ok(counts) => {
                    let first = counts[0].unwrap();
                    assert_eq!(
                        (
                            first.total_emails,
                            first.unread_emails,
                            first.total_threads,
                            first.unread_threads
                        ),
                        (7, 3, 4, 2)
                    );
                    assert_eq!(counts[1].unwrap().total_emails, 0);
                    complete = true;
                    break;
                }
                Err(error) => {
                    assert_eq!(
                        error,
                        if advancing_clock {
                            ports::Error::Deadline
                        } else {
                            ports::Error::Capacity
                        }
                    );
                    refused += 1;
                }
            }
        }
        assert!(
            complete && refused > 100,
            "clock={advancing_clock}, refused={refused}"
        );
    }
}

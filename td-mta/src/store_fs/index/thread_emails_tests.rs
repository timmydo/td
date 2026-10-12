#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{
    format::row::{BlobRow, EmailOrigin, EmailRow},
    ports::{Crypto, Tick, Time},
    store_fs::tests::Fixture,
};
use std::sync::atomic::{AtomicU64, Ordering};
const ACCOUNT: AccountId = AccountId::from_bytes([1; 16]);
const OTHER: AccountId = AccountId::from_bytes([2; 16]);
const THREAD: ThreadId = ThreadId::from_bytes([3; 16]);
const EMPTY: ThreadId = ThreadId::from_bytes([4; 16]);
const BODY: BlobId = BlobId::from_bytes([5; 16]);
struct Timer(AtomicU64, AtomicU64);
impl Clock for Timer {
    fn sample(&self) -> Result<Time, ports::Error> {
        Ok(Time {
            monotonic: Tick(
                self.0
                    .fetch_add(self.1.load(Ordering::Relaxed), Ordering::Relaxed),
            ),
            utc_ms: 0,
        })
    }
}
fn deadline() -> Deadline {
    Deadline::after(Tick(0), 1_000_000).unwrap()
}
fn email(n: u16) -> EmailId {
    let mut bytes = [0; 16];
    bytes[..2].copy_from_slice(&n.to_be_bytes());
    EmailId::from_bytes(bytes)
}
fn encoded(row: Row<'_>) -> Vec<u8> {
    let mut bytes = vec![0; 256];
    let length = row.encode(&mut bytes).unwrap();
    bytes.truncate(length);
    bytes
}
fn seed(store: &IndexStore<'_>, account: AccountId, count: u16) {
    store.create_account(account, deadline()).unwrap();
    let blob = encoded(Row::Blob(BlobRow {
        length: 0,
        digest: td_crypto::Provider.sha256().unwrap().finish().unwrap(),
        created_at: 0,
    }));
    let thread = encoded(Row::Thread);
    let rows: Vec<_> = (0..count)
        .map(|n| {
            (
                email(n),
                encoded(Row::Email(EmailRow {
                    blob: BODY,
                    thread: THREAD,
                    received_at: date(n),
                    origin: EmailOrigin::Jmap,
                })),
            )
        })
        .collect();
    let mut ops = vec![
        Operation::put(Table::Blobs, BODY.as_bytes(), &blob).unwrap(),
        Operation::put(Table::Threads, THREAD.as_bytes(), &thread).unwrap(),
        Operation::put(Table::Threads, EMPTY.as_bytes(), &thread).unwrap(),
    ];
    for (id, row) in &rows {
        ops.push(Operation::put(Table::Emails, id.as_bytes(), row).unwrap());
    }
    store
        .commit(
            &td_crypto::Provider,
            CommitRequest {
                account,
                epoch: store.epoch(),
                expected: Sequence::default(),
                utc_ms: 0,
                deadline: deadline(),
            },
            &ops,
            &mut [BlobSource {
                id: BODY,
                source: &mut b"".as_slice(),
            }],
        )
        .unwrap();
}
fn date(n: u16) -> i64 {
    match n {
        0 => i64::MIN,
        1 => i64::MAX,
        _ => 100 - i64::from(n / 3),
    }
}
static FIXTURE_LOCK: Mutex<()> = Mutex::new(());
fn run(test: impl FnOnce(&IndexStore<'_>, &Timer)) {
    let _guard = FIXTURE_LOCK.lock().unwrap();
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let clock = Arc::new(Timer(AtomicU64::new(1), AtomicU64::new(0)));
    let store = IndexStore::create(
        &mut root,
        StoreEpoch::from_bytes([7; 16]),
        clock.clone(),
        3,
        deadline(),
    )
    .unwrap();
    seed(&store, ACCOUNT, 513);
    seed(&store, OTHER, 1);
    test(&store, &clock);
}
#[test]
fn ascending_windows_are_complete_tie_stable_and_account_scoped() {
    run(|store, _| {
        let mut view = store.view(ACCOUNT, deadline()).unwrap();
        let mut expected: Vec<_> = (0..513)
            .map(|n| ThreadEmail {
                received_at: date(n),
                email: email(n),
            })
            .collect();
        expected.sort_unstable_by_key(|entry| (entry.received_at, entry.email));
        for maximum in [1, 17, 256] {
            let mut view = store.view(ACCOUNT, deadline()).unwrap();
            let mut found = Vec::new();
            let mut after = None;
            loop {
                let page = view
                    .thread_emails(THREAD, after, maximum, &mut [0; 32])
                    .unwrap()
                    .unwrap();
                assert_eq!(page.identity, view.identity());
                assert_eq!(page.thread, THREAD);
                assert!(page.emails.len() <= maximum);
                assert!(page.emails.windows(2).all(|pair| pair[0] < pair[1]));
                after = page.emails.last().copied();
                found.extend_from_slice(&page.emails);
                if !page.has_more {
                    break;
                }
                assert_eq!(page.emails.len(), maximum);
            }
            assert_eq!(found, expected);
        }
        let last = expected.last().copied();
        let page = view
            .thread_emails(THREAD, last, 256, &mut [0; 32])
            .unwrap()
            .unwrap();
        assert!(page.emails.is_empty());
        assert!(!page.has_more);
        let mut other = store.view(OTHER, deadline()).unwrap();
        assert_eq!(
            other
                .thread_emails(THREAD, None, 256, &mut [0; 32])
                .unwrap()
                .unwrap()
                .emails
                .len(),
            1
        );
        assert!(view
            .thread_emails(ThreadId::from_bytes([9; 16]), None, 1, &mut [0; 32])
            .unwrap()
            .is_none());
        let empty = view
            .thread_emails(EMPTY, None, 1, &mut [])
            .unwrap()
            .unwrap();
        assert!(empty.emails.is_empty());
        assert!(!empty.has_more);
        assert!(matches!(
            view.thread_emails(THREAD, None, 0, &mut [0; 32]),
            Err(ports::Error::Invalid)
        ));
        assert!(matches!(
            view.thread_emails(THREAD, None, 257, &mut [0; 32]),
            Err(ports::Error::Capacity)
        ));
    });
}
#[test]
fn an_original_view_retains_deleted_members_and_fixed_thread_identity() {
    run(|store, _| {
        let mut old = store.view(ACCOUNT, deadline()).unwrap();
        let first = old
            .thread_emails(THREAD, None, 256, &mut [0; 32])
            .unwrap()
            .unwrap();
        store
            .commit(
                &td_crypto::Provider,
                CommitRequest {
                    account: ACCOUNT,
                    epoch: store.epoch(),
                    expected: Sequence::from_u64(1),
                    utc_ms: 0,
                    deadline: deadline(),
                },
                &[Operation::delete(Table::Emails, email(1).as_bytes()).unwrap()],
                &mut [],
            )
            .unwrap();
        let last = old
            .thread_emails(THREAD, first.emails.last().copied(), 256, &mut [0; 32])
            .unwrap()
            .unwrap();
        let final_page = old
            .thread_emails(THREAD, last.emails.last().copied(), 256, &mut [0; 32])
            .unwrap()
            .unwrap();
        assert_eq!(
            final_page.emails,
            vec![ThreadEmail {
                email: email(1),
                received_at: i64::MAX
            }]
        );
        assert!(!final_page.has_more);
        assert_eq!(final_page.identity, first.identity);
        let mut fresh = store.view(ACCOUNT, deadline()).unwrap();
        let current = fresh
            .thread_emails(THREAD, last.emails.last().copied(), 256, &mut [0; 32])
            .unwrap()
            .unwrap();
        assert!(current.emails.is_empty());
    });
}
#[test]
fn query_plan_uses_only_the_selected_account_thread_range_without_sql_sort() {
    run(|store, _| {
        let mut view = store.view(ACCOUNT, deadline()).unwrap();
        view.read_snapshot(|native| {
            native.run(|db| {
                let mut statement = db
                    .prepare(&format!("EXPLAIN QUERY PLAN {MEMBERS}"))
                    .map_err(sql)?;
                let details = statement
                    .query_map(
                        params![ACCOUNT.as_bytes().as_slice(), THREAD.as_bytes().as_slice()],
                        |row| row.get::<_, String>(3),
                    )
                    .map_err(sql)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(sql)?;
                assert!(
                    details
                        .iter()
                        .any(|s| s.contains("emails_thread (account=? AND thread_id=?)")),
                    "{details:?}"
                );
                assert!(
                    !details
                        .iter()
                        .any(|s| ["TEMP B-TREE", "SCAN", "AUTOMATIC", "MATERIALIZE"]
                            .iter()
                            .any(|bad| s.contains(bad))),
                    "{details:?}"
                );
                Ok(())
            })
        })
        .unwrap();
    });
}
#[test]
fn future_rows_and_original_snapshot_failures_never_escape_as_windows() {
    for fault in ["deadline", "fuel", "snapshot", "future"] {
        run(|store, clock| {
            if fault == "future" {
                lock(&store.writer)
                    .unwrap()
                    .native
                    .run(|db| {
                        db.execute(
                            "UPDATE emails SET changed=x'0000000000000063' WHERE account=?1",
                            params![ACCOUNT.as_bytes().as_slice()],
                        )
                        .map_err(sql)?;
                        Ok(())
                    })
                    .unwrap();
            }
            let mut view = store.view(ACCOUNT, deadline()).unwrap();
            let expected = match fault {
                "deadline" => {
                    clock.0.store(1_000_001, Ordering::Relaxed);
                    ports::Error::Deadline
                }
                "fuel" => {
                    lock(&view.native().unwrap().scope.budget)
                        .unwrap()
                        .remaining = 0;
                    ports::Error::Capacity
                }
                "snapshot" => {
                    view.native()
                        .unwrap()
                        .run(|db| db.execute_batch("ROLLBACK").map_err(sql))
                        .unwrap();
                    ports::Error::Corrupt
                }
                "future" => ports::Error::Corrupt,
                _ => panic!("unknown fault {fault}"),
            };
            assert_eq!(
                view.thread_emails(THREAD, None, 256, &mut [0; 32]).err(),
                Some(expected),
                "{fault}"
            );
        });
    }
}

#[test]
fn work_and_clock_cutoffs_withhold_late_results_without_renewing_a_view() {
    run(|store, clock| {
        let (used, expected) = {
            let mut view = store.view(ACCOUNT, deadline()).unwrap();
            let before = lock(&view.native().unwrap().scope.budget)
                .unwrap()
                .remaining;
            let page = view
                .thread_emails(THREAD, None, 256, &mut [0; 32])
                .unwrap()
                .unwrap();
            assert!(page.has_more);
            let remaining = lock(&view.native().unwrap().scope.budget)
                .unwrap()
                .remaining;
            (before - remaining, page.emails)
        };
        assert!(used > 1);
        let mut passed = false;
        let mut refused = false;
        for fuel in [0, 1, used / 2, used - 1, used, used + 1] {
            let mut view = store.view(ACCOUNT, deadline()).unwrap();
            lock(&view.native().unwrap().scope.budget)
                .unwrap()
                .remaining = fuel;
            match view.thread_emails(THREAD, None, 256, &mut [0; 32]) {
                Ok(Some(page)) => {
                    assert!(fuel >= used, "late fuel cutoff {fuel}/{used}");
                    passed = true;
                    assert_eq!(page.emails, expected);
                    assert!(page.has_more);
                }
                Err(ports::Error::Capacity) => {
                    assert!(fuel <= used, "passing fuel control {fuel}/{used}");
                    refused = true;
                    assert!(matches!(
                        view.thread_emails(EMPTY, None, 1, &mut [0; 32]),
                        Err(ports::Error::Capacity)
                    ));
                }
                other => panic!("unexpected fuel {fuel}: {other:?}"),
            }
        }
        assert!(passed && refused);
        clock.0.store(1, Ordering::Relaxed);
        let samples = {
            let mut view = store.view(ACCOUNT, deadline()).unwrap();
            clock.1.store(1, Ordering::Relaxed);
            view.thread_emails(THREAD, None, 256, &mut [0; 32])
                .unwrap()
                .unwrap();
            clock.1.store(0, Ordering::Relaxed);
            clock.0.load(Ordering::Relaxed) - 1
        };
        assert!((1..1_000_000).contains(&samples));
        for remaining in [1, samples - 1, samples + 1] {
            clock.0.store(1, Ordering::Relaxed);
            let mut view = store.view(ACCOUNT, deadline()).unwrap();
            clock.0.store(1_000_000 - remaining, Ordering::Relaxed);
            clock.1.store(1, Ordering::Relaxed);
            let result = view.thread_emails(THREAD, None, 256, &mut [0; 32]);
            if remaining > samples {
                assert_eq!(result.unwrap().unwrap().emails, expected);
            } else {
                assert_eq!(result.err(), Some(ports::Error::Deadline));
            }
            clock.1.store(0, Ordering::Relaxed);
        }
    });
}

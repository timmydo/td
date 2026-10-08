#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{
    format::row::{
        BlobKind, BlobRow, EmailOrigin, EmailRow, ReceiptRecipients, ReceiptTls, SmtpReceipt,
        MAX_RECEIPT_BYTES,
    },
    ports::{Crypto, Digest, Tick, Time},
    store_fs::tests::Fixture,
};
use std::sync::atomic::{AtomicU64, Ordering};
const ACCOUNT: AccountId = AccountId::from_bytes([1; 16]);
const OTHER: AccountId = AccountId::from_bytes([2; 16]);
const BODY: BlobId = BlobId::from_bytes([3; 16]);
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
            let mut value = vec![0; 65536];
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
fn seed(store: &IndexStore<'_>, account: AccountId, entries: &[(EmailId, ThreadId, &str)]) {
    store.create_account(account, deadline()).unwrap();
    let mut rows = vec![(
        Key::Blob(BODY),
        Row::Blob(BlobRow {
            kind: BlobKind::Message,
            length: 0,
            digest: td_crypto::Provider.sha256().unwrap().finish().unwrap(),
            created_at: 0,
        }),
    )];
    for &(email, thread, message) in entries {
        rows.extend([
            (Key::Thread(thread), Row::Thread),
            (
                Key::Email(email),
                Row::Email(EmailRow {
                    blob: BODY,
                    thread,
                    received_at: 0,
                    origin: EmailOrigin::Jmap,
                }),
            ),
            (Key::ThreadAnchor(message, email), Row::ThreadAnchor),
        ]);
    }
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
fn email(n: u8) -> EmailId {
    EmailId::from_bytes([n; 16])
}
fn thread(n: u8) -> ThreadId {
    ThreadId::from_bytes([n; 16])
}

#[test]
fn exact_lookup_uses_smallest_id_and_retains_account_snapshot() {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let store = open(&mut root, Arc::new(Timer(AtomicU64::new(1))));
    seed(
        &store,
        ACCOUNT,
        &[
            (email(255), thread(15), "same@example.test"),
            (email(2), thread(12), "same@example.test"),
            (email(0), thread(10), "same@example.test"),
            (email(4), thread(14), "Case@example.test"),
            (email(5), thread(15), "é@example.test"),
        ],
    );
    seed(
        &store,
        OTHER,
        &[(email(0), thread(20), "same@example.test")],
    );
    let mut old = store.view(ACCOUNT, deadline()).unwrap();
    let mut value = [0; 65536];
    assert_eq!(
        old.thread_anchor("same@example.test", &mut value),
        Ok(Some((email(0), thread(10))))
    );
    assert_eq!(
        old.thread_anchor("Case@example.test", &mut value),
        Ok(Some((email(4), thread(14))))
    );
    assert_eq!(old.thread_anchor("case@example.test", &mut value), Ok(None));
    assert_eq!(
        old.thread_anchor("é@example.test", &mut value),
        Ok(Some((email(5), thread(15))))
    );
    assert_eq!(
        old.thread_anchor("e\u{301}@example.test", &mut value),
        Ok(None)
    );
    let mut other = store.view(OTHER, deadline()).unwrap();
    assert_eq!(
        other.thread_anchor("same@example.test", &mut value),
        Ok(Some((email(0), thread(20))))
    );
    drop(other);
    old.read_snapshot(|native| {
        native.run(|db| {
            let mut query = db
                .prepare(&format!("EXPLAIN QUERY PLAN {FIRST_ANCHOR}"))
                .map_err(sql)?;
            let details = query
                .query_map(
                    params![ACCOUNT.as_bytes().as_slice(), "same@example.test"],
                    |row| row.get::<_, String>(3),
                )
                .map_err(sql)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql)?;
            assert!(
                details
                    .iter()
                    .any(|s| s.contains("SEARCH thread_anchors USING PRIMARY KEY")
                        && s.contains("account=? AND message_id=?")),
                "{details:?}"
            );
            assert!(
                !details.iter().any(|s| s.contains("TEMP B-TREE")),
                "{details:?}"
            );
            Ok(())
        })
    })
    .unwrap();
    apply(
        &store,
        ACCOUNT,
        1,
        &[],
        &[
            Key::Email(email(0)),
            Key::ThreadAnchor("same@example.test", email(0)),
        ],
        &mut [],
    );
    let mut current = store.view(ACCOUNT, deadline()).unwrap();
    assert_eq!(
        current.thread_anchor("same@example.test", &mut value),
        Ok(Some((email(2), thread(12))))
    );
    assert_eq!(
        old.thread_anchor("same@example.test", &mut value),
        Ok(Some((email(0), thread(10))))
    );
    apply(
        &store,
        ACCOUNT,
        2,
        &[],
        &[
            Key::ThreadAnchor("same@example.test", email(2)),
            Key::Email(email(2)),
            Key::ThreadAnchor("same@example.test", email(255)),
            Key::Email(email(255)),
        ],
        &mut [],
    );
    assert_eq!(
        current.thread_anchor("same@example.test", &mut value),
        Ok(Some((email(2), thread(12))))
    );
    drop(current);
    let mut current = store.view(ACCOUNT, deadline()).unwrap();
    assert_eq!(
        current.thread_anchor("same@example.test", &mut value),
        Ok(None)
    );
    assert_eq!(current.identity().committed_sequence, Sequence::from_u64(3));
}

#[test]
fn input_and_value_bounds_do_not_invent_an_absent_anchor() {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let store = open(&mut root, Arc::new(Timer(AtomicU64::new(1))));
    let maximum = "x".repeat(format::key::MAX_ANCHOR_BYTES);
    seed(&store, ACCOUNT, &[(email(0), thread(1), &maximum)]);
    let mut view = store.view(ACCOUNT, deadline()).unwrap();
    assert_eq!(
        view.thread_anchor("", &mut [0; 512]),
        Err(ports::Error::Invalid)
    );
    assert_eq!(
        view.thread_anchor(&(maximum.clone() + "x"), &mut [0; 512]),
        Err(ports::Error::Invalid)
    );
    assert_eq!(
        view.thread_anchor(&maximum, &mut []),
        Err(ports::Error::Capacity)
    );
    assert_eq!(
        view.thread_anchor(&maximum, &mut [0; 512]),
        Ok(Some((email(0), thread(1))))
    );
    let row = Row::Email(EmailRow {
        blob: BODY,
        thread: thread(1),
        received_at: 0,
        origin: EmailOrigin::Jmap,
    });
    let mut value = [0; 512];
    let length = row.encode(&mut value).unwrap();
    assert_eq!(
        view.thread_anchor(&maximum, &mut value[..length - 1]),
        Err(ports::Error::Capacity)
    );
    assert_eq!(
        view.thread_anchor(&maximum, &mut value[..length]),
        Ok(Some((email(0), thread(1))))
    );
    assert_eq!(view.thread_anchor("missing", &mut []), Ok(None));
}

#[test]
fn original_deadline_and_lost_snapshot_failures_are_sticky() {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let clock = Arc::new(Timer(AtomicU64::new(1)));
    let store = open(&mut root, clock.clone());
    seed(&store, ACCOUNT, &[(email(0), thread(1), "a@b")]);
    let mut view = store.view(ACCOUNT, deadline()).unwrap();
    clock.0.store(101, Ordering::Relaxed);
    assert_eq!(
        view.thread_anchor("a@b", &mut [0; 512]),
        Err(ports::Error::Deadline)
    );
    clock.0.store(1, Ordering::Relaxed);
    assert_eq!(
        view.thread_anchor("missing", &mut [0; 512]),
        Err(ports::Error::Deadline)
    );
    drop(view);
    let mut view = store.view(ACCOUNT, deadline()).unwrap();
    view.native()
        .unwrap()
        .run(|db| db.execute_batch("ROLLBACK").map_err(sql))
        .unwrap();
    assert_eq!(
        view.thread_anchor("a@b", &mut [0; 512]),
        Err(ports::Error::Corrupt)
    );
    view.native()
        .unwrap()
        .run(|db| db.execute_batch("BEGIN DEFERRED").map_err(sql))
        .unwrap();
    assert_eq!(
        view.thread_anchor("missing", &mut [0; 512]),
        Err(ports::Error::Corrupt)
    );
}

#[test]
fn corrupt_first_match_or_future_rows_do_not_fall_back_to_another_thread() {
    const ANCHOR_CHANGED: &str =
        "UPDATE thread_anchors SET changed=?1 WHERE account=?2 AND email_id=?3";
    const ANCHOR_ID: &str =
        "UPDATE thread_anchors SET email_id=?1 WHERE account=?2 AND email_id=?3";
    for mode in 0..9 {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let store = open(&mut root, Arc::new(Timer(AtomicU64::new(1))));
        seed(
            &store,
            ACCOUNT,
            &[(email(0), thread(1), "a@b"), (email(2), thread(3), "a@b")],
        );
        lock(&store.writer)
            .unwrap()
            .native
            .run(|db| {
                match mode {
                    0..=2 => {
                        let statement = match mode {
                            0 => ANCHOR_CHANGED,
                            1 => "UPDATE emails SET changed=?1 WHERE account=?2 AND id=?3",
                            _ => "UPDATE threads SET changed=?1 WHERE account=?2 AND id=?3",
                        };
                        let id = if mode == 2 {
                            *thread(1).as_bytes()
                        } else {
                            *email(0).as_bytes()
                        };
                        db.execute(
                            statement,
                            params![
                                99u64.to_be_bytes().as_slice(),
                                ACCOUNT.as_bytes().as_slice(),
                                id.as_slice()
                            ],
                        )
                        .map_err(sql)?;
                    }
                    3 | 4 => {
                        db.execute_batch("PRAGMA foreign_keys=OFF").map_err(sql)?;
                        let (statement, id) = if mode == 3 {
                            (
                                "DELETE FROM emails WHERE account=?1 AND id=?2",
                                *email(0).as_bytes(),
                            )
                        } else {
                            (
                                "DELETE FROM threads WHERE account=?1 AND id=?2",
                                *thread(1).as_bytes(),
                            )
                        };
                        db.execute(
                            statement,
                            params![ACCOUNT.as_bytes().as_slice(), id.as_slice()],
                        )
                        .map_err(sql)?;
                        db.execute_batch("PRAGMA foreign_keys=ON").map_err(sql)?;
                    }
                    _ => {
                        db.execute_batch(
                            "PRAGMA foreign_keys=OFF; PRAGMA ignore_check_constraints=ON",
                        )
                        .map_err(sql)?;
                        let (statement, length) = match mode {
                            5 => (ANCHOR_ID, 1),
                            6 => (ANCHOR_ID, 17),
                            7 => (ANCHOR_CHANGED, 1),
                            _ => (ANCHOR_CHANGED, 9),
                        };
                        db.execute(
                            statement,
                            params![
                                vec![0u8; length],
                                ACCOUNT.as_bytes().as_slice(),
                                email(0).as_bytes().as_slice()
                            ],
                        )
                        .map_err(sql)?;
                        db.execute_batch(
                            "PRAGMA foreign_keys=ON; PRAGMA ignore_check_constraints=OFF",
                        )
                        .map_err(sql)?;
                    }
                }
                Ok(())
            })
            .unwrap();
        let mut view = store.view(ACCOUNT, deadline()).unwrap();
        assert_eq!(
            view.thread_anchor("a@b", &mut [0; 512]),
            Err(ports::Error::Corrupt),
            "mode {mode}"
        );
    }
}

#[test]
fn native_failures_at_each_lookup_stage_remain_errors() {
    for stage in 0..=3 {
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
            let store = open(&mut root, Arc::new(Timer(AtomicU64::new(1))));
            seed(&store, ACCOUNT, &[(email(0), thread(1), "a@b")]);
            let mut view = store.view(ACCOUNT, deadline()).unwrap();
            let native = view.native().unwrap();
            let budget = Arc::clone(&native.budget);
            let seen = Arc::new(AtomicU64::new(0));
            let reads = Arc::clone(&seen);
            lock(&native.connection)
                .unwrap()
                .authorizer(Some(move |context: AuthContext<'_>| {
                    if matches!(context.action, AuthAction::Select)
                        && reads.fetch_add(1, Ordering::Relaxed) + 1 == stage
                    {
                        lock(&budget).unwrap().failure = Some(error);
                    }
                    Authorization::Allow
                }))
                .unwrap();
            if stage == 0 {
                assert_eq!(
                    view.thread_anchor("a@b", &mut [0; 512]),
                    Ok(Some((email(0), thread(1))))
                );
                assert_eq!(seen.load(Ordering::Relaxed), 3);
                continue;
            }
            assert_eq!(
                view.thread_anchor("a@b", &mut [0; 512]),
                Err(error),
                "stage {stage}"
            );
            assert_eq!(seen.load(Ordering::Relaxed), stage);
            assert_eq!(view.thread_anchor("missing", &mut [0; 512]), Err(error));
            assert_eq!(seen.load(Ordering::Relaxed), stage);
            lock(&view.native().unwrap().connection)
                .unwrap()
                .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
                .unwrap();
        }
    }
}

#[test]
fn smtp_lookup_bounds_receipt_work_and_scratch() {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let store = open(&mut root, Arc::new(Timer(AtomicU64::new(1))));
    seed(&store, ACCOUNT, &[(email(0), thread(1), "a@b")]);
    let short = "a".repeat(26) + "@b";
    let long = "a".repeat(27) + "@b";
    let addresses: Vec<_> = (0..1000)
        .map(|n| {
            if n < 764 {
                long.as_str()
            } else {
                short.as_str()
            }
        })
        .collect();
    let mut recipients = [0; MAX_RECEIPT_BYTES];
    let receipt = ReceiptRecipients::encode(&addresses, &mut recipients).unwrap();
    assert_eq!(receipt.encoded().len(), MAX_RECEIPT_BYTES);
    let row = Row::Email(EmailRow {
        blob: BODY,
        thread: thread(1),
        received_at: 0,
        origin: EmailOrigin::Smtp(SmtpReceipt {
            peer: "192.0.2.1".parse().unwrap(),
            gateway: None,
            tls: ReceiptTls::Tls13,
            ehlo: "host.test",
            reverse_path: "",
            recipients: receipt,
        }),
    });
    apply(
        &store,
        ACCOUNT,
        1,
        &[(Key::Email(email(0)), row)],
        &[],
        &mut [],
    );
    let mut value = [0; format::MAX_VALUE_BYTES];
    let length = row.encode(&mut value).unwrap();
    let mut view = store.view(ACCOUNT, deadline()).unwrap();
    assert_eq!(
        view.thread_anchor("a@b", &mut value[..length - 1]),
        Err(ports::Error::Capacity)
    );
    let seen = Arc::new(AtomicU64::new(0));
    let reads = Arc::clone(&seen);
    lock(&view.native().unwrap().connection)
        .unwrap()
        .authorizer(Some(move |context: AuthContext<'_>| {
            if matches!(context.action, AuthAction::Select) {
                reads.fetch_add(1, Ordering::Relaxed);
            }
            Authorization::Allow
        }))
        .unwrap();
    assert_eq!(
        view.thread_anchor("a@b", &mut value[..length]),
        Ok(Some((email(0), thread(1))))
    );
    assert_eq!(seen.load(Ordering::Relaxed), 4);
    lock(&view.native().unwrap().connection)
        .unwrap()
        .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
        .unwrap();
    assert_eq!(
        view.thread_anchor("a@b", &mut value),
        Ok(Some((email(0), thread(1))))
    );
}

#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{
    format::row::{BlobRow, EmailOrigin, EmailRow, MailboxRow},
    ids::ThreadId,
    ports::{Crypto, Tick, Time},
    store_fs::tests::Fixture,
};
use std::sync::atomic::{AtomicU64, Ordering};
const ACCOUNT: AccountId = AccountId::from_bytes([1; 16]);
const OTHER: AccountId = AccountId::from_bytes([2; 16]);
const EMAIL: EmailId = EmailId::from_bytes([3; 16]);
const BLOB: BlobId = BlobId::from_bytes([4; 16]);
const THREAD: ThreadId = ThreadId::from_bytes([5; 16]);
struct Timer(AtomicU64);
impl Clock for Timer {
    fn sample(&self) -> Result<crate::ports::Time, ports::Error> {
        Ok(Time {
            monotonic: Tick(self.0.load(Ordering::Relaxed)),
            utc_ms: 0,
        })
    }
}
fn deadline() -> Deadline {
    Deadline::after(Tick(0), 100).unwrap()
}
fn mailbox(n: u16) -> MailboxId {
    let mut b = [0; 16];
    b[..2].copy_from_slice(&n.to_be_bytes());
    MailboxId::from_bytes(b)
}
fn encoded(row: Row<'_>) -> Vec<u8> {
    let mut out = vec![0; 2048];
    let len = row.encode(&mut out).unwrap();
    out.truncate(len);
    out
}
fn seed(store: &IndexStore<'_>, account: AccountId, count: u16) {
    store.create_account(account, deadline()).unwrap();
    let body = b"Subject: stored metadata\r\n\r\nbody";
    let mut digest = td_crypto::Provider.sha256().unwrap();
    digest.update(body).unwrap();
    let blob = encoded(Row::Blob(BlobRow {
        length: body.len() as u64,
        digest: digest.finish().unwrap(),
        created_at: 0,
    }));
    let email = encoded(Row::Email(EmailRow {
        blob: BLOB,
        thread: THREAD,
        received_at: -1,
        origin: EmailOrigin::Jmap,
    }));
    let mb = encoded(Row::Mailbox(MailboxRow {
        name: "Folder",
        parent: None,
        role: None,
        sort_order: 0,
        subscribed: true,
    }));
    let boxes: Vec<_> = (0..count).map(mailbox).collect();
    let keywords: Vec<_> = (0..count).map(|n| format!("key{n:03}")).collect();
    let keys: Vec<_> = boxes
        .iter()
        .map(|m| {
            let mut b = vec![0; 32];
            Key::Membership(EMAIL, *m).encode(&mut b).unwrap();
            b
        })
        .chain(keywords.iter().map(|kw| {
            let mut b = vec![0; 300];
            let len = Key::Keyword(EMAIL, kw).encode(&mut b).unwrap();
            b.truncate(len);
            b
        }))
        .collect();
    let mut ops = vec![
        Operation::put(Table::Blobs, BLOB.as_bytes(), &blob).unwrap(),
        Operation::put(Table::Threads, THREAD.as_bytes(), &[]).unwrap(),
        Operation::put(Table::Emails, EMAIL.as_bytes(), &email).unwrap(),
    ];
    for id in &boxes {
        ops.push(Operation::put(Table::Mailboxes, id.as_bytes(), &mb).unwrap());
    }
    for (n, key) in keys.iter().enumerate() {
        ops.push(
            Operation::put(
                if n < usize::from(count) {
                    Table::Memberships
                } else {
                    Table::Keywords
                },
                key,
                &[],
            )
            .unwrap(),
        );
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
                id: BLOB,
                source: &mut body.as_slice(),
            }],
        )
        .unwrap();
}
static FIXTURE: Mutex<()> = Mutex::new(());
fn run(test: impl FnOnce(&IndexStore<'_>, &Timer)) {
    let _guard = FIXTURE.lock().unwrap();
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let timer = Arc::new(Timer(AtomicU64::new(1)));
    let store = IndexStore::create(
        &mut root,
        StoreEpoch::from_bytes([6; 16]),
        timer.clone(),
        3,
        deadline(),
    )
    .unwrap();
    seed(&store, ACCOUNT, 300);
    seed(&store, OTHER, 1);
    test(&store, &timer);
}
#[test]
fn metadata_and_every_set_window_are_complete_sorted_and_account_scoped() {
    run(|store, _| {
        let mut view = store.view(ACCOUNT, deadline()).unwrap();
        let value = view.email_metadata(EMAIL, &mut [0; 2048]).unwrap().unwrap();
        assert_eq!(
            (
                value.identity,
                value.email,
                value.blob,
                value.thread,
                value.received_at,
                value.size
            ),
            (view.identity(), EMAIL, BLOB, THREAD, -1, 32)
        );
        for maximum in [1, 17, 256] {
            let mut boxes = Vec::new();
            let mut after = None;
            for turn in 0..=300 {
                assert!(turn < 300, "mailbox pagination did not terminate");
                let page = view.email_mailboxes(EMAIL, after, maximum).unwrap();
                assert_eq!(page.identity, view.identity());
                assert_eq!(page.email, EMAIL);
                after = page.values.last().copied();
                boxes.extend(page.values);
                if !page.has_more {
                    break;
                }
            }
            assert_eq!(boxes, (0..300).map(mailbox).collect::<Vec<_>>());
            let mut words = Vec::new();
            let mut after = None::<String>;
            for turn in 0..=300 {
                assert!(turn < 300, "keyword pagination did not terminate");
                let page = view
                    .email_keywords(EMAIL, after.as_deref(), maximum)
                    .unwrap();
                assert_eq!(page.identity, view.identity());
                assert_eq!(page.email, EMAIL);
                after = page.values.last().cloned();
                words.extend(page.values);
                if !page.has_more {
                    break;
                }
            }
            assert_eq!(
                words,
                (0..300).map(|n| format!("key{n:03}")).collect::<Vec<_>>()
            );
        }
        let mut other = store.view(OTHER, deadline()).unwrap();
        assert_eq!(
            other.email_mailboxes(EMAIL, None, 256).unwrap().values,
            vec![mailbox(0)]
        );
        assert_eq!(
            other.email_keywords(EMAIL, None, 256).unwrap().values,
            vec!["key000"]
        );
        assert!(view
            .email_metadata(EmailId::from_bytes([9; 16]), &mut [0; 2048])
            .unwrap()
            .is_none());
        assert!(view
            .email_mailboxes(EmailId::from_bytes([9; 16]), None, 256)
            .unwrap()
            .values
            .is_empty());
        assert!(view
            .email_keywords(EmailId::from_bytes([9; 16]), None, 256)
            .unwrap()
            .values
            .is_empty());
        assert!(
            !view
                .email_mailboxes(EMAIL, Some(mailbox(299)), 256)
                .unwrap()
                .has_more
        );
        assert!(view
            .email_mailboxes(EMAIL, Some(mailbox(299)), 256)
            .unwrap()
            .values
            .is_empty());
        assert!(view
            .email_keywords(EMAIL, Some("key299"), 256)
            .unwrap()
            .values
            .is_empty());
    });
}
#[test]
fn old_snapshot_keeps_metadata_and_sets_after_new_transaction() {
    run(|store, _| {
        let mut old = store.view(ACCOUNT, deadline()).unwrap();
        let original = old.email_metadata(EMAIL, &mut [0; 2048]).unwrap().unwrap();
        let mut mb = [0; 32];
        Key::Membership(EMAIL, mailbox(0)).encode(&mut mb).unwrap();
        let mut kw = [0; 22];
        Key::Keyword(EMAIL, "key000").encode(&mut kw).unwrap();
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
                &[
                    Operation::delete(Table::Memberships, &mb).unwrap(),
                    Operation::delete(Table::Keywords, &kw).unwrap(),
                ],
                &mut [],
            )
            .unwrap();
        assert_eq!(
            old.email_metadata(EMAIL, &mut [0; 2048]).unwrap(),
            Some(original)
        );
        assert_eq!(
            old.email_mailboxes(EMAIL, None, 1).unwrap().values,
            vec![mailbox(0)]
        );
        assert_eq!(
            old.email_keywords(EMAIL, None, 1).unwrap().values,
            vec!["key000"]
        );
        let mut fresh = store.view(ACCOUNT, deadline()).unwrap();
        assert_eq!(
            fresh.email_mailboxes(EMAIL, None, 1).unwrap().values,
            vec![mailbox(1)]
        );
        assert_eq!(
            fresh.email_keywords(EMAIL, None, 1).unwrap().values,
            vec!["key001"]
        );
    });
}
#[test]
fn bounds_invalid_cursors_and_expired_views_never_return_partial_values() {
    run(|store, timer| {
        let mut view = store.view(ACCOUNT, deadline()).unwrap();
        assert!(matches!(
            view.email_mailboxes(EMAIL, None, 0),
            Err(ports::Error::Invalid)
        ));
        assert!(matches!(
            view.email_keywords(EMAIL, None, 257),
            Err(ports::Error::Capacity)
        ));
        for after in ["", "Bad", "a b", &"x".repeat(256)] {
            assert!(matches!(
                view.email_keywords(EMAIL, Some(after), 1),
                Err(ports::Error::Invalid)
            ));
        }
        assert!(matches!(
            view.email_metadata(EMAIL, &mut []),
            Err(ports::Error::Capacity)
        ));
        timer.0.store(100, Ordering::Relaxed);
        assert!(matches!(
            view.email_metadata(EMAIL, &mut [0; 2048]),
            Err(ports::Error::Deadline)
        ));
        assert!(matches!(
            view.email_mailboxes(EMAIL, None, 1),
            Err(ports::Error::Deadline)
        ));
        assert!(matches!(
            view.email_keywords(EMAIL, None, 1),
            Err(ports::Error::Deadline)
        ));
    });
}
#[test]
fn set_query_plans_seek_primary_key_ranges_without_sort_temporaries() {
    run(|store, _| {
        let mut view = store.view(ACCOUNT, deadline()).unwrap();
        view.read_snapshot(|native| {
            native.run(|db| {
                for (query, after, column) in [
                    (MAILBOX_FIRST, false, "mailbox_id"),
                    (MAILBOX_AFTER, true, "mailbox_id"),
                    (KEYWORD_FIRST, false, "keyword"),
                    (KEYWORD_AFTER, true, "keyword"),
                ] {
                    let mut stmt = db
                        .prepare(&format!("EXPLAIN QUERY PLAN {query}"))
                        .map_err(sql)?;
                    let mailbox_bound = mailbox(0);
                    let mut rows = if after && column == "mailbox_id" {
                        stmt.query(params![
                            ACCOUNT.as_bytes().as_slice(),
                            EMAIL.as_bytes().as_slice(),
                            mailbox_bound.as_bytes().as_slice(),
                            257
                        ])
                        .map_err(sql)?
                    } else if after {
                        stmt.query(params![
                            ACCOUNT.as_bytes().as_slice(),
                            EMAIL.as_bytes().as_slice(),
                            "key000",
                            257
                        ])
                        .map_err(sql)?
                    } else {
                        stmt.query(params![
                            ACCOUNT.as_bytes().as_slice(),
                            EMAIL.as_bytes().as_slice(),
                            257
                        ])
                        .map_err(sql)?
                    };
                    let mut details = Vec::new();
                    while let Some(row) = rows.next().map_err(sql)? {
                        details.push(row.get::<_, String>(3).map_err(sql)?);
                    }
                    assert!(
                        details.iter().any(|s| s.contains("SEARCH")
                            && s.contains("PRIMARY KEY")
                            && s.contains("account=?")
                            && s.contains("email_id=?")),
                        "{details:?}"
                    );
                    assert!(
                        !details.iter().any(|s| s.contains("TEMP B-TREE")),
                        "{details:?}"
                    );
                    if after {
                        assert!(
                            details.iter().any(|s| s.contains("SEARCH")
                                && s.contains("PRIMARY KEY")
                                && s.contains(&format!("{column}>?"))),
                            "{details:?}"
                        );
                    }
                }
                Ok(())
            })
        })
        .unwrap();
    });
}

#[test]
fn smtp_receipt_scratch_and_small_jmap_blob_scratch_are_explicit() {
    run(|store, _| {
        let mut view = store.view(ACCOUNT, deadline()).unwrap();
        let short = encoded(Row::Email(EmailRow {
            blob: BLOB,
            thread: THREAD,
            received_at: -1,
            origin: EmailOrigin::Jmap,
        }))
        .len();
        assert!(matches!(
            view.email_metadata(EMAIL, &mut vec![0; short]),
            Err(ports::Error::Capacity)
        ));
        drop(view);
        use crate::format::row::ReceiptTls;
        use crate::format::row::{ReceiptRecipients, SmtpReceipt, MAX_RECEIPT_BYTES};
        let addresses = vec!["recipient@example.test"; 1000];
        let mut receipt = vec![0; MAX_RECEIPT_BYTES];
        let recipients = ReceiptRecipients::encode(&addresses, &mut receipt).unwrap();
        let row = Row::Email(EmailRow {
            blob: BLOB,
            thread: THREAD,
            received_at: -1,
            origin: EmailOrigin::Smtp(SmtpReceipt {
                peer: "192.0.2.1".parse().unwrap(),
                gateway: None,
                tls: ReceiptTls::Tls13,
                ehlo: "host.test",
                reverse_path: "",
                recipients,
            }),
        });
        let mut value = vec![0; format::MAX_VALUE_BYTES];
        let length = row.encode(&mut value).unwrap();
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
                &[Operation::put(Table::Emails, EMAIL.as_bytes(), &value[..length]).unwrap()],
                &mut [],
            )
            .unwrap();
        let mut view = store.view(ACCOUNT, deadline()).unwrap();
        assert!(matches!(
            view.email_metadata(EMAIL, &mut value[..length - 1]),
            Err(ports::Error::Capacity)
        ));
        assert_eq!(
            view.email_metadata(EMAIL, &mut value)
                .unwrap()
                .unwrap()
                .size,
            32
        );
    });
}

#[test]
fn corrupt_metadata_and_extra_set_rows_withhold_whole_results() {
    for fault in [
        "email-sequence",
        "blob-sequence",
        "missing-blob",
        "mailbox-extra",
        "keyword-extra",
        "keyword-spelling",
    ] {
        run(|store, _| {
            lock(&store.writer).unwrap().native.run(|db| {
                let statement = match fault {
                    "email-sequence" => "UPDATE emails SET changed=x'0000000000000063' WHERE account=?1",
                    "blob-sequence" => "UPDATE blobs SET changed=x'0000000000000063' WHERE account=?1",
                    "missing-blob" => "UPDATE emails SET blob_id=x'99999999999999999999999999999999' WHERE account=?1",
                    "mailbox-extra" => "UPDATE memberships SET changed=x'0000000000000063' WHERE account=?1 AND mailbox_id=x'00010000000000000000000000000000'",
                    "keyword-extra" => "UPDATE keywords SET changed=x'0000000000000063' WHERE account=?1 AND keyword='key001'",
                    "keyword-spelling" => "UPDATE keywords SET keyword='key000A' WHERE account=?1 AND keyword='key001'",
                    _ => panic!("unknown fault"),
                };
                if fault == "missing-blob" { db.execute_batch("PRAGMA foreign_keys=OFF").map_err(sql)?; }
                db.execute(statement, params![ACCOUNT.as_bytes().as_slice()]).map_err(sql)?;
                if fault == "missing-blob" { db.execute_batch("PRAGMA foreign_keys=ON").map_err(sql)?; }
                Ok(())
            }).unwrap();
            let mut view = store.view(ACCOUNT, deadline()).unwrap();
            let error = match fault {
                "mailbox-extra" => view.email_mailboxes(EMAIL, None, 1).err(),
                "keyword-extra" | "keyword-spelling" => view.email_keywords(EMAIL, None, 1).err(),
                _ => view.email_metadata(EMAIL, &mut [0; 2048]).err(),
            };
            assert_eq!(error, Some(ports::Error::Corrupt), "{fault}");
        });
    }
}

#[test]
fn exhausted_native_fuel_and_lost_snapshot_refuse_all_three_projections() {
    for lost in [false, true] {
        for method in 0..3 {
            run(|store, _| {
                let mut view = store.view(ACCOUNT, deadline()).unwrap();
                let expected = if lost {
                    view.native()
                        .unwrap()
                        .run(|db| db.execute_batch("ROLLBACK").map_err(sql))
                        .unwrap();
                    ports::Error::Corrupt
                } else {
                    lock(&view.native().unwrap().scope.budget)
                        .unwrap()
                        .remaining = 0;
                    ports::Error::Capacity
                };
                let error = match method {
                    0 => view.email_metadata(EMAIL, &mut [0; 2048]).err(),
                    1 => view.email_mailboxes(EMAIL, None, 1).err(),
                    _ => view.email_keywords(EMAIL, None, 1).err(),
                };
                assert_eq!(error, Some(expected));
            });
        }
    }
}

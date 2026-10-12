#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::{write_stored, Stored, MAX_CHUNK_BYTES};
use crate::{
    email_property::Field,
    format::{
        key::Key,
        operation::Operation,
        row::{BlobRow, EmailOrigin, EmailRow, MailboxRow, Row},
        Sequence, Table,
    },
    ids::{AccountId, BlobId, EmailId, MailboxId, StoreEpoch, ThreadId},
    ports::{self, Clock, Crypto, Deadline, Error, Tick, Time},
    store_fs::{tests::Fixture, BlobSource, CommitRequest, IndexReadView, IndexStore},
    wire,
};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};
use td_crypto::Digest;
const ACCOUNT: AccountId = AccountId::from_bytes([1; 16]);
const OTHER: AccountId = AccountId::from_bytes([2; 16]);
const EMAIL: EmailId = EmailId::from_bytes([3; 16]);
const BLOB: BlobId = BlobId::from_bytes([4; 16]);
const THREAD: ThreadId = ThreadId::from_bytes([5; 16]);
struct Timer(AtomicU64);
impl Clock for Timer {
    fn sample(&self) -> Result<Time, ports::Error> {
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
            assert_eq!(Key::Membership(EMAIL, *m).encode(&mut b).unwrap(), 32);
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

fn output(view: &mut IndexReadView<'_, '_>, selected: Stored) -> td_json::Json {
    let mut bytes = Vec::new();
    assert!(
        write_stored(view, EMAIL, selected, &mut [0; 2048], |chunk| {
            assert!(chunk.len() <= MAX_CHUNK_BYTES);
            bytes.extend_from_slice(chunk);
            Ok(())
        })
        .unwrap()
    );
    td_json::parse(std::str::from_utf8(&bytes).unwrap()).unwrap()
}
#[test]
fn complete_stored_objects_page_all_sets_and_preserve_wire_kinds() {
    run(|store, _| {
        let mut view = store.view(ACCOUNT, deadline()).unwrap();
        let object = output(&mut view, Stored::all());
        assert_eq!(object.as_obj().unwrap().len(), 7);
        for (field, expected) in [
            (
                "id",
                wire::encode_id(EMAIL, &mut [0; wire::IdKind::WIRE_BYTES])
                    .unwrap()
                    .to_owned(),
            ),
            (
                "blobId",
                wire::encode_id(BLOB, &mut [0; wire::IdKind::WIRE_BYTES])
                    .unwrap()
                    .to_owned(),
            ),
            (
                "threadId",
                wire::encode_id(THREAD, &mut [0; wire::IdKind::WIRE_BYTES])
                    .unwrap()
                    .to_owned(),
            ),
        ] {
            assert_eq!(object[field].as_str(), Some(expected.as_str()));
        }
        assert_eq!(
            object["receivedAt"].as_str(),
            Some("1969-12-31T23:59:59.999Z")
        );
        assert_eq!(object["size"].as_u64(), Some(32));
        assert_eq!(object["mailboxIds"].as_obj().unwrap().len(), 300);
        assert_eq!(object["keywords"].as_obj().unwrap().len(), 300);
        for n in 0..300 {
            let mut id = [0; wire::IdKind::WIRE_BYTES];
            let id = wire::encode_id(mailbox(n), &mut id).unwrap();
            assert_eq!(object["mailboxIds"][id].as_bool(), Some(true));
            assert_eq!(
                object["keywords"][format!("key{n:03}").as_str()].as_bool(),
                Some(true)
            );
        }
        let mut other = store.view(OTHER, deadline()).unwrap();
        let object = output(&mut other, Stored::all());
        assert_eq!(object["mailboxIds"].as_obj().unwrap().len(), 1);
        assert_eq!(object["keywords"].as_obj().unwrap().len(), 1);
    });
}
#[test]
fn every_explicit_stored_projection_keeps_id_and_absence_emits_nothing() {
    run(|store, _| {
        let mut view = store.view(ACCOUNT, deadline()).unwrap();
        assert_eq!(
            output(&mut view, Stored::id_only()).as_obj().unwrap().len(),
            1
        );
        for (field, name) in [
            (Field::BlobId, "blobId"),
            (Field::ThreadId, "threadId"),
            (Field::MailboxIds, "mailboxIds"),
            (Field::Keywords, "keywords"),
            (Field::Size, "size"),
            (Field::ReceivedAt, "receivedAt"),
        ] {
            let mut selected = Stored::id_only();
            selected.include(Field::Id).unwrap();
            selected.include(field).unwrap();
            selected.include(field).unwrap();
            let object = output(&mut view, selected);
            assert_eq!(object.as_obj().unwrap().len(), 2);
            assert!(!object[name].is_null());
        }
        for field in [
            Field::Headers,
            Field::BodyStructure,
            Field::BodyValues,
            Field::TextBody,
            Field::HtmlBody,
            Field::Attachments,
            Field::HasAttachment,
            Field::Preview,
        ] {
            let mut selected = Stored::id_only();
            assert_eq!(selected.include(field), Err(Error::Invalid));
            assert_eq!(selected, Stored::id_only());
        }
        let mut calls = 0;
        assert!(!write_stored(
            &mut view,
            EmailId::from_bytes([9; 16]),
            Stored::all(),
            &mut [0; 2048],
            |_| {
                calls += 1;
                Ok(())
            }
        )
        .unwrap());
        assert_eq!(calls, 0);
    });
}
#[test]
fn selected_sink_failures_and_expiry_after_last_chunk_withhold_success() {
    run(|store, timer| {
        let calls = {
            let mut view = store.view(ACCOUNT, deadline()).unwrap();
            let mut calls = 0;
            assert!(
                write_stored(&mut view, EMAIL, Stored::all(), &mut [0; 2048], |_| {
                    calls += 1;
                    Ok(())
                })
                .unwrap()
            );
            calls
        };
        assert!(
            (3..50).contains(&calls),
            "unbatched or missing output: {calls}"
        );
        for cutoff in 1..=calls {
            let mut view = store.view(ACCOUNT, deadline()).unwrap();
            let mut seen = 0;
            assert_eq!(
                write_stored(&mut view, EMAIL, Stored::all(), &mut [0; 2048], |_| {
                    seen += 1;
                    if seen == cutoff {
                        Err(Error::Busy)
                    } else {
                        Ok(())
                    }
                }),
                Err(Error::Busy)
            );
            assert_eq!(seen, cutoff);
        }
        let mut view = store.view(ACCOUNT, deadline()).unwrap();
        let mut seen = 0;
        assert_eq!(
            write_stored(&mut view, EMAIL, Stored::all(), &mut [0; 2048], |_| {
                seen += 1;
                if seen == calls {
                    timer.0.store(100, Ordering::Relaxed);
                }
                Ok(())
            }),
            Err(Error::Deadline)
        );
        assert_eq!(seen, calls);
    });
}

#[test]
fn orphan_membership_refuses_success_while_old_snapshot_stays_complete() {
    run(|store, _| {
        let mut old = store.view(ACCOUNT, deadline()).unwrap();
        let keys: Vec<_> = (0..300)
            .map(|n| {
                let mut key = [0; 32];
                Key::Membership(EMAIL, mailbox(n)).encode(&mut key).unwrap();
                key
            })
            .collect();
        let operations: Vec<_> = keys
            .iter()
            .map(|key| Operation::delete(Table::Memberships, key).unwrap())
            .collect();
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
                &operations,
                &mut [],
            )
            .unwrap();
        assert_eq!(
            output(&mut old, Stored::all())["mailboxIds"]
                .as_obj()
                .unwrap()
                .len(),
            300
        );
        let mut fresh = store.view(ACCOUNT, deadline()).unwrap();
        let mut provisional = Vec::new();
        assert_eq!(
            write_stored(&mut fresh, EMAIL, Stored::all(), &mut [0; 2048], |chunk| {
                provisional.extend_from_slice(chunk);
                Ok(())
            }),
            Err(Error::Corrupt)
        );
        assert!(!provisional.is_empty());
        assert!(td_json::parse(std::str::from_utf8(&provisional).unwrap()).is_err());
        assert_eq!(
            output(&mut fresh, Stored::id_only())
                .as_obj()
                .unwrap()
                .len(),
            1
        );
    });
}

#[test]
fn selected_invalid_date_refuses_but_stored_id_only_remains_available() {
    run(|store, _| {
        let row = encoded(Row::Email(EmailRow {
            blob: BLOB,
            thread: THREAD,
            received_at: i64::MAX,
            origin: EmailOrigin::Jmap,
        }));
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
                &[Operation::put(Table::Emails, EMAIL.as_bytes(), &row).unwrap()],
                &mut [],
            )
            .unwrap();
        let mut view = store.view(ACCOUNT, deadline()).unwrap();
        let mut calls = 0;
        assert_eq!(
            write_stored(&mut view, EMAIL, Stored::all(), &mut [0; 2048], |_| {
                calls += 1;
                Ok(())
            }),
            Err(Error::Corrupt)
        );
        assert_eq!(calls, 0);
        assert_eq!(
            output(&mut view, Stored::id_only()).as_obj().unwrap().len(),
            1
        );
    });
}

#[test]
fn empty_and_maximum_keyword_maps_are_complete() {
    run(|store, _| {
        let mut key = [0; 22];
        Key::Keyword(EMAIL, "key000").encode(&mut key).unwrap();
        store
            .commit(
                &td_crypto::Provider,
                CommitRequest {
                    account: OTHER,
                    epoch: store.epoch(),
                    expected: Sequence::from_u64(1),
                    utc_ms: 0,
                    deadline: deadline(),
                },
                &[Operation::delete(Table::Keywords, &key).unwrap()],
                &mut [],
            )
            .unwrap();
        let mut view = store.view(OTHER, deadline()).unwrap();
        assert!(output(&mut view, Stored::all())["keywords"]
            .as_obj()
            .unwrap()
            .is_empty());
        drop(view);
        let keyword = "k".repeat(255);
        let mut key = vec![0; 271];
        Key::Keyword(EMAIL, &keyword).encode(&mut key).unwrap();
        store
            .commit(
                &td_crypto::Provider,
                CommitRequest {
                    account: OTHER,
                    epoch: store.epoch(),
                    expected: Sequence::from_u64(2),
                    utc_ms: 0,
                    deadline: deadline(),
                },
                &[Operation::put(Table::Keywords, &key, &[]).unwrap()],
                &mut [],
            )
            .unwrap();
        let mut view = store.view(OTHER, deadline()).unwrap();
        let object = output(&mut view, Stored::all());
        assert_eq!(object["keywords"].as_obj().unwrap().len(), 1);
        assert_eq!(object["keywords"][keyword.as_str()].as_bool(), Some(true));
    });
}

#[test]
fn expiry_in_early_and_middle_callbacks_stops_at_that_callback() {
    for cutoff in [1, 5] {
        run(|store, timer| {
            let mut view = store.view(ACCOUNT, deadline()).unwrap();
            let mut seen = 0;
            assert_eq!(
                write_stored(&mut view, EMAIL, Stored::all(), &mut [0; 2048], |_| {
                    seen += 1;
                    if seen == cutoff {
                        timer.0.store(100, Ordering::Relaxed);
                    }
                    Ok(())
                }),
                Err(Error::Deadline)
            );
            assert_eq!(seen, cutoff);
        });
    }
}

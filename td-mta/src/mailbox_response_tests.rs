#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{
    format::{self, operation::Operation, row::MailboxRow, Sequence},
    ids::{AccountId, StoreEpoch},
    ports::{Clock, Deadline, Tick, Time},
    store_fs::{tests::Fixture, CommitRequest, IndexStore},
};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
const ACCOUNT: AccountId = AccountId::from_bytes([1; 16]);
const ROOT: MailboxId = MailboxId::from_bytes([2; 16]);
const CHILD: MailboxId = MailboxId::from_bytes([3; 16]);
const RIGHTS: Rights = Rights {
    may_read_items: true,
    may_add_items: false,
    may_remove_items: true,
    may_set_seen: false,
    may_set_keywords: true,
    may_create_child: false,
    may_rename: true,
    may_delete: false,
    may_submit: true,
};
struct Timer(AtomicU64);
impl Clock for Timer {
    fn sample(&self) -> Result<Time, Error> {
        Ok(Time {
            monotonic: Tick(self.0.load(Ordering::Relaxed)),
            utc_ms: 0,
        })
    }
}
fn until() -> Deadline {
    Deadline::after(Tick(0), 100).unwrap()
}
fn put(store: &IndexStore<'_>, expected: u64, rows: &[(MailboxId, MailboxRow<'_>)]) {
    let encoded: Vec<_> = rows
        .iter()
        .map(|(id, row)| {
            let mut key = [0; 16];
            Key::Mailbox(*id).encode(&mut key).unwrap();
            let mut value = vec![0; format::MAX_VALUE_BYTES];
            let length = Row::Mailbox(*row).encode(&mut value).unwrap();
            value.truncate(length);
            (key, value)
        })
        .collect();
    let operations: Vec<_> = encoded
        .iter()
        .map(|(key, value)| Operation::put(format::Table::Mailboxes, key, value).unwrap())
        .collect();
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
            &operations,
            &mut [],
        )
        .unwrap();
}
fn seed(store: &IndexStore<'_>, name: &str) {
    store.create_account(ACCOUNT, until()).unwrap();
    put(
        store,
        0,
        &[
            (
                ROOT,
                MailboxRow {
                    name: "Inbox",
                    parent: None,
                    role: Some("inbox"),
                    sort_order: 0,
                    subscribed: true,
                },
            ),
            (
                CHILD,
                MailboxRow {
                    name,
                    parent: Some(ROOT),
                    role: None,
                    sort_order: 2147483647,
                    subscribed: false,
                },
            ),
        ],
    );
}
fn id(id: MailboxId) -> String {
    wire::encode_id(id, &mut [0; wire::IdKind::WIRE_BYTES])
        .unwrap()
        .to_owned()
}
fn json(
    view: &mut IndexReadView<'_, '_>,
    mailbox: MailboxId,
    projection: Projection,
    counts: Option<&MailboxCounts>,
) -> td_json::Json {
    let mut out = vec![0; MAX_OBJECT_BYTES];
    let length = encode(
        view,
        mailbox,
        projection,
        RIGHTS,
        counts,
        &mut vec![0; format::MAX_VALUE_BYTES],
        &mut out,
    )
    .unwrap()
    .unwrap();
    td_json::parse(std::str::from_utf8(&out[..length]).unwrap()).unwrap()
}

#[test]
fn full_objects_include_every_property_and_each_policy_right() {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let store = IndexStore::create(
        &mut root,
        StoreEpoch::from_bytes([4; 16]),
        Arc::new(Timer(AtomicU64::new(1))),
        3,
        until(),
    )
    .unwrap();
    let name = "Archive \"日\" \\";
    seed(&store, name);
    let mut view = store.view(ACCOUNT, until()).unwrap();
    let counts = view
        .mailbox_counts_many(&[ROOT, CHILD], &mut vec![0; format::MAX_VALUE_BYTES])
        .unwrap();
    let child = json(&mut view, CHILD, Projection::all(), counts[1].as_ref());
    assert_eq!(child.as_obj().unwrap().len(), 11);
    assert_eq!(child["id"].as_str(), Some(id(CHILD).as_str()));
    assert_eq!(child["name"].as_str(), Some(name));
    assert_eq!(child["parentId"].as_str(), Some(id(ROOT).as_str()));
    assert!(child["role"].is_null());
    assert_eq!(child["sortOrder"].as_u64(), Some(2147483647));
    assert_eq!(child["isSubscribed"].as_bool(), Some(false));
    for property in [
        "totalEmails",
        "unreadEmails",
        "totalThreads",
        "unreadThreads",
    ] {
        assert_eq!(child[property].as_u64(), Some(0));
    }
    let rights = &child["myRights"];
    assert_eq!(rights.as_obj().unwrap().len(), 9);
    for (name, value) in [
        ("mayReadItems", true),
        ("mayAddItems", false),
        ("mayRemoveItems", true),
        ("maySetSeen", false),
        ("maySetKeywords", true),
        ("mayCreateChild", false),
        ("mayRename", true),
        ("mayDelete", false),
        ("maySubmit", true),
    ] {
        assert_eq!(rights[name].as_bool(), Some(value));
    }
    let root = json(&mut view, ROOT, Projection::all(), counts[0].as_ref());
    assert!(root["parentId"].is_null());
    assert_eq!(root["role"].as_str(), Some("inbox"));
    assert_eq!(root["isSubscribed"].as_bool(), Some(true));
}

#[test]
fn projection_always_keeps_id_and_rejects_unknown_properties() {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let store = IndexStore::create(
        &mut root,
        StoreEpoch::from_bytes([4; 16]),
        Arc::new(Timer(AtomicU64::new(1))),
        3,
        until(),
    )
    .unwrap();
    seed(&store, "Child");
    let mut view = store.view(ACCOUNT, until()).unwrap();
    let counts = view
        .mailbox_counts(CHILD, &mut vec![0; format::MAX_VALUE_BYTES])
        .unwrap()
        .unwrap();
    for name in [
        "id",
        "name",
        "parentId",
        "role",
        "sortOrder",
        "totalEmails",
        "unreadEmails",
        "totalThreads",
        "unreadThreads",
        "myRights",
        "isSubscribed",
    ] {
        let mut projection = Projection::id_only();
        projection.include(name).unwrap();
        projection.include(name).unwrap();
        let object = json(
            &mut view,
            CHILD,
            projection,
            if projection.needs_counts() {
                Some(&counts)
            } else {
                None
            },
        );
        assert_eq!(
            object.as_obj().unwrap().len(),
            if name == "id" { 1 } else { 2 }
        );
        assert!(object.as_obj().unwrap().iter().any(|(key, _)| key == name));
    }
    let mut projection = Projection::id_only();
    for invalid in ["", "Name", "parent", "myRights/mayReadItems"] {
        assert_eq!(projection.include(invalid), Err(Error::Invalid));
    }
    assert_eq!(projection, Projection::id_only());
}

#[test]
fn counts_must_match_the_same_view_and_selected_mailbox() {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let clock = Arc::new(Timer(AtomicU64::new(1)));
    let store = IndexStore::create(
        &mut root,
        StoreEpoch::from_bytes([4; 16]),
        clock.clone(),
        3,
        until(),
    )
    .unwrap();
    seed(&store, "Before");
    let mut old = store.view(ACCOUNT, until()).unwrap();
    let counts = old
        .mailbox_counts(CHILD, &mut vec![0; format::MAX_VALUE_BYTES])
        .unwrap()
        .unwrap();
    put(
        &store,
        1,
        &[(
            CHILD,
            MailboxRow {
                name: "After",
                parent: Some(ROOT),
                role: None,
                sort_order: 1,
                subscribed: true,
            },
        )],
    );
    let mut new = store.view(ACCOUNT, until()).unwrap();
    let mut scratch = vec![0; format::MAX_VALUE_BYTES];
    let mut out = vec![0; MAX_OBJECT_BYTES];
    assert_eq!(
        encode(
            &mut new,
            CHILD,
            Projection::all(),
            RIGHTS,
            Some(&counts),
            &mut scratch,
            &mut out
        ),
        Err(Error::Invalid)
    );
    assert_eq!(
        encode(
            &mut old,
            ROOT,
            Projection::all(),
            RIGHTS,
            Some(&counts),
            &mut scratch,
            &mut out
        ),
        Err(Error::Invalid)
    );
    assert_eq!(
        encode(
            &mut old,
            CHILD,
            Projection::all(),
            RIGHTS,
            None,
            &mut scratch,
            &mut out
        ),
        Err(Error::Invalid)
    );
    assert_eq!(
        encode(
            &mut old,
            MailboxId::from_bytes([99; 16]),
            Projection::all(),
            RIGHTS,
            None,
            &mut scratch,
            &mut out
        ),
        Ok(None)
    );
    assert_eq!(
        json(&mut old, CHILD, Projection::all(), Some(&counts))["name"].as_str(),
        Some("Before")
    );
    assert_eq!(
        json(&mut new, CHILD, Projection::id_only(), None)["id"].as_str(),
        Some(id(CHILD).as_str())
    );
    clock.0.store(101, Ordering::Relaxed);
    assert_eq!(
        encode(
            &mut old,
            CHILD,
            Projection::all(),
            RIGHTS,
            Some(&counts),
            &mut scratch,
            &mut out
        ),
        Err(Error::Deadline)
    );
}

#[test]
fn exact_output_bounds_and_json_integer_bounds_are_enforced() {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let store = IndexStore::create(
        &mut root,
        StoreEpoch::from_bytes([4; 16]),
        Arc::new(Timer(AtomicU64::new(1))),
        3,
        until(),
    )
    .unwrap();
    seed(&store, &"\\\"".repeat(512));
    let mut view = store.view(ACCOUNT, until()).unwrap();
    let mut scratch = vec![0; format::MAX_VALUE_BYTES];
    let mut counts = view.mailbox_counts(CHILD, &mut scratch).unwrap().unwrap();
    let mut out = vec![0; MAX_OBJECT_BYTES];
    let length = encode(
        &mut view,
        CHILD,
        Projection::all(),
        RIGHTS,
        Some(&counts),
        &mut scratch,
        &mut out,
    )
    .unwrap()
    .unwrap();
    for size in 0..length {
        assert_eq!(
            encode(
                &mut view,
                CHILD,
                Projection::all(),
                RIGHTS,
                Some(&counts),
                &mut scratch,
                &mut out[..size]
            ),
            Err(Error::Capacity)
        );
    }
    assert_eq!(
        encode(
            &mut view,
            CHILD,
            Projection::all(),
            RIGHTS,
            Some(&counts),
            &mut scratch,
            &mut out[..length]
        ),
        Ok(Some(length))
    );
    for (property, field) in [
        ("totalEmails", 0),
        ("unreadEmails", 1),
        ("totalThreads", 2),
        ("unreadThreads", 3),
    ] {
        let mut projection = Projection::id_only();
        projection.include(property).unwrap();
        for value in [MAX_UNSIGNED_INT, MAX_UNSIGNED_INT + 1] {
            match field {
                0 => counts.total_emails = value,
                1 => counts.unread_emails = value,
                2 => counts.total_threads = value,
                _ => counts.unread_threads = value,
            }
            if value == MAX_UNSIGNED_INT {
                assert_eq!(
                    json(&mut view, CHILD, projection, Some(&counts))[property].as_u64(),
                    Some(value)
                );
            } else {
                assert_eq!(
                    encode(
                        &mut view,
                        CHILD,
                        projection,
                        RIGHTS,
                        Some(&counts),
                        &mut scratch,
                        &mut out
                    ),
                    Err(Error::Invalid)
                );
            }
        }
    }
}

#[test]
fn final_snapshot_failure_withholds_a_fully_formatted_object() {
    struct ClosingClock {
        samples: AtomicU64,
        fail_at: AtomicU64,
    }
    impl Clock for ClosingClock {
        fn sample(&self) -> Result<Time, Error> {
            let sample = self.samples.fetch_add(1, Ordering::Relaxed) + 1;
            if sample >= self.fail_at.load(Ordering::Relaxed) {
                return Err(Error::Deadline);
            }
            Ok(Time {
                monotonic: Tick(1),
                utc_ms: 0,
            })
        }
    }
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let clock = Arc::new(ClosingClock {
        samples: AtomicU64::new(0),
        fail_at: AtomicU64::new(u64::MAX),
    });
    let store = IndexStore::create(
        &mut root,
        StoreEpoch::from_bytes([4; 16]),
        clock.clone(),
        3,
        until(),
    )
    .unwrap();
    seed(&store, "Child");
    let mut view = store.view(ACCOUNT, until()).unwrap();
    let mut scratch = vec![0; format::MAX_VALUE_BYTES];
    let mut out = vec![0; MAX_OBJECT_BYTES];
    let mut projection = Projection::id_only();
    projection.include("name").unwrap();
    let before = clock.samples.load(Ordering::Relaxed);
    let length = encode(
        &mut view,
        CHILD,
        projection,
        RIGHTS,
        None,
        &mut scratch,
        &mut out,
    )
    .unwrap()
    .unwrap();
    let after = clock.samples.load(Ordering::Relaxed);
    let expected = out[..length].to_vec();
    out.fill(0);
    clock
        .fail_at
        .store(after + (after - before), Ordering::Relaxed);
    assert_eq!(
        encode(
            &mut view,
            CHILD,
            projection,
            RIGHTS,
            None,
            &mut scratch,
            &mut out
        ),
        Err(Error::Deadline)
    );
    assert_eq!(&out[..length], expected);
    clock.fail_at.store(u64::MAX, Ordering::Relaxed);
    assert_eq!(
        encode(
            &mut view,
            CHILD,
            projection,
            RIGHTS,
            None,
            &mut scratch,
            &mut out
        ),
        Err(Error::Deadline)
    );
}

#[test]
fn scalar_fields_keep_their_names_and_short_row_scratch_fails() {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let store = IndexStore::create(
        &mut root,
        StoreEpoch::from_bytes([4; 16]),
        Arc::new(Timer(AtomicU64::new(1))),
        3,
        until(),
    )
    .unwrap();
    seed(&store, "Child");
    let mut view = store.view(ACCOUNT, until()).unwrap();
    let mut scratch = vec![0; format::MAX_VALUE_BYTES];
    let mut counts = view.mailbox_counts(CHILD, &mut scratch).unwrap().unwrap();
    // Distinct synthetic values test the trusted scalar-to-wire boundary.
    counts.total_emails = 11;
    counts.unread_emails = 7;
    counts.total_threads = 5;
    counts.unread_threads = 3;
    let object = json(&mut view, CHILD, Projection::all(), Some(&counts));
    for (name, value) in [
        ("totalEmails", 11),
        ("unreadEmails", 7),
        ("totalThreads", 5),
        ("unreadThreads", 3),
    ] {
        assert_eq!(object[name].as_u64(), Some(value));
    }
    let names = [
        "mayReadItems",
        "mayAddItems",
        "mayRemoveItems",
        "maySetSeen",
        "maySetKeywords",
        "mayCreateChild",
        "mayRename",
        "mayDelete",
        "maySubmit",
    ];
    let mut projection = Projection::id_only();
    projection.include("myRights").unwrap();
    let mut out = vec![0; MAX_OBJECT_BYTES];
    for active in 0..names.len() {
        let rights = Rights {
            may_read_items: active == 0,
            may_add_items: active == 1,
            may_remove_items: active == 2,
            may_set_seen: active == 3,
            may_set_keywords: active == 4,
            may_create_child: active == 5,
            may_rename: active == 6,
            may_delete: active == 7,
            may_submit: active == 8,
        };
        let length = encode(
            &mut view,
            CHILD,
            projection,
            rights,
            None,
            &mut scratch,
            &mut out,
        )
        .unwrap()
        .unwrap();
        let object = td_json::parse(std::str::from_utf8(&out[..length]).unwrap()).unwrap();
        for (index, name) in names.iter().enumerate() {
            assert_eq!(object["myRights"][*name].as_bool(), Some(index == active));
        }
    }
    assert_eq!(
        encode(
            &mut view,
            CHILD,
            Projection::id_only(),
            RIGHTS,
            None,
            &mut [],
            &mut out
        ),
        Err(Error::Capacity)
    );
}
